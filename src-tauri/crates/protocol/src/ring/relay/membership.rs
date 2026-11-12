//! Membership-specific clients reuse the existing transport, IO thread and token verifier.
//! Departure is deliberately not a Connector: it cannot become online, route envelopes or
//! publish arbitrary changes, and never lets a Relay rebase signed bytes.
use super::*;
use crate::ring::{
    activity::ActivityEvidence,
    membership::{Enrollment, RingPolicy, RosterChange},
    RingId,
};

#[derive(Clone, Debug)]
pub struct PolicyPreview {
    pub head: ChainPin,
    pub policy: RingPolicy,
    pub affected: Vec<String>,
    pub token: String,
}
#[derive(Clone, Debug)]
pub struct ActivityReport {
    pub snapshot: String,
    pub terminal_evidence: Vec<String>,
}
impl RingClient {
    /// Discover once at the configured Relay before an authorized genesis/Upgrade pins it.
    /// RelayUrl retains the existing authenticated wss / loopback-only ws transport rules.
    pub fn activity_key(
        relay: &str,
        ring: &RingId,
        tls: Option<Arc<ClientConfig>>,
        timeout: Duration,
    ) -> Result<SignKey, RingError> {
        let (mut setup, _, _) = open_membership(relay, ring, tls, timeout)?;
        setup.send(ClientFrame::ActivityKey { id: 1 }.encode())?;
        match reply(&mut setup, 1)? {
            RelayFrame::ActivityKey { key, .. } => Ok(key),
            _ => Err(RingError::Protocol("expected activity.key".into())),
        }
    }
    pub(super) fn membership_request(
        &self,
        kind: Kind,
        frame: impl FnOnce(u64) -> ClientFrame,
    ) -> Result<RelayFrame, RingError> {
        let at_capacity = {
            let st = lock(&self.state);
            st.waiting.len() + st.replies.len() >= 256
        };
        if at_capacity {
            return Err(RingError::Backpressure);
        }
        let ticket = self.waited(kind, None, |id, _| frame(id))?;
        let id = ticket.id();
        let result = ticket.wait(self.timeouts.request);
        let response = lock(&self.state).replies.remove(&id);
        result?;
        response.ok_or_else(|| RingError::Protocol("missing membership response".into()))
    }
    /// The IO handler verifies and retains terminal evidence before later candidate frames.
    pub fn activity_evidence(&self) -> Result<ActivityReport, RingError> {
        match self
            .membership_request(Kind::ActivityEvidence, |id| ClientFrame::ActivityGet { id })?
        {
            RelayFrame::ActivityEvidence {
                snapshot, evidence, ..
            } => Ok(ActivityReport {
                snapshot,
                terminal_evidence: evidence,
            }),
            _ => Err(RingError::Protocol("expected activity evidence".into())),
        }
    }
    /// Nonterminal preview. The eventual signed SetPolicy binds this exact head, proposed
    /// policy and affected set. Worker rechecks eligibility at commit, even at the same head.
    pub fn preview_policy(&self, policy: RingPolicy) -> Result<PolicyPreview, RingError> {
        let frame =
            self.membership_request(Kind::PolicyPreview, |id| ClientFrame::ActivityPreview {
                id,
                policy: policy.clone(),
            })?;
        let RelayFrame::ActivityPreview {
            preview, affected, ..
        } = frame
        else {
            return Err(RingError::Protocol("expected policy preview".into()));
        };
        let chain = self.chain();
        let head = chain.head();
        let evidence =
            ActivityEvidence::parse(&preview, &head.roster().membership()?.policy.activity_key)?;
        // This is validation of the independently signed preview, not a Roster signature.
        if !evidence.binds(head, &policy)
            || evidence.units.len() != head.roster().membership()?.enrollments.len()
        {
            return Err(RingError::Invalid("stale/incomplete policy preview".into()));
        }
        let old = head.roster().membership()?;
        for u in &old.enrollments {
            let a = evidence
                .units
                .iter()
                .find(|a| a.enrollment_id == u.id)
                .ok_or_else(|| RingError::Invalid("missing preview unit".into()))?;
            if a.member_keys.len() != u.members.len()
                || u.members.iter().any(|k| !a.member_keys.contains(k))
                || a.last_active < u.enrolled_at
            {
                return Err(RingError::Invalid("invalid preview unit".into()));
            }
        }
        let eligible = old
            .enrollments
            .iter()
            .filter(|u| evidence.eligible(u, &policy, false))
            .map(|u| u.id.clone())
            .collect::<Vec<_>>();
        if affected.len() != eligible.len() || eligible.iter().any(|id| !affected.contains(id)) {
            return Err(RingError::Invalid("preview affected set".into()));
        }
        Ok(PolicyPreview {
            head: ChainPin {
                version: head.version(),
                hash: head.hash(),
            },
            policy,
            affected,
            token: preview,
        })
    }
}

pub(super) fn open_membership(
    relay: &str,
    ring: &RingId,
    tls: Option<Arc<ClientConfig>>,
    timeout: Duration,
) -> Result<(Setup, RelayUrl, String), RingError> {
    let url = RelayUrl::parse(relay).map_err(|e| RingError::Invalid(e.to_string()))?;
    let ws = transport::dial(
        &url,
        &url.ring_endpoint(ring),
        tls,
        Instant::now() + timeout,
        RingLimits::default().write_buffer_bytes,
    )?;
    let mut setup = Setup {
        ws,
        last_error: None,
    };
    let text = setup.recv()?;
    let nonce = match decode_relay(&text) {
        Ok(RelayFrame::Challenge { v, nonce, .. })
            if v == RELAY_PROTOCOL && b64::decode_array::<32>(&nonce).is_ok() =>
        {
            nonce
        }
        _ => return Err(RingError::Protocol("invalid membership challenge".into())),
    };
    Ok((setup, url, nonce))
}
fn reply(setup: &mut Setup, id: u64) -> Result<RelayFrame, RingError> {
    for _ in 0..MAX_EARLY_FRAMES {
        match decode_relay(&setup.recv()?).map_err(|e| RingError::Protocol(e.to_string()))? {
            RelayFrame::Error {
                code,
                detail,
                id: got,
                ..
            } if got.is_none() || got == Some(id) => return Err(RingError::Relay { code, detail }),
            f @ RelayFrame::ActivityKey { id: got, .. }
            | f @ RelayFrame::RosterChain { id: got, .. }
            | f @ RelayFrame::Ok { id: got }
            | f @ RelayFrame::ActivityPreview { id: got, .. }
            | f @ RelayFrame::ActivityEvidence { id: got, .. }
                if got == id =>
            {
                return Ok(f)
            }
            _ => {}
        }
    }
    Err(RingError::Protocol(
        "too much unrelated membership traffic".into(),
    ))
}
/// One bounded historical-key cleanup connection. The caller persists its trusted chain,
/// exact target and scoped signer independently of optional current membership (Slice5).
pub struct DepartureClient {
    setup: Setup,
    url: RelayUrl,
    nonce: String,
    signer: Arc<dyn Signer>,
    target: Enrollment,
    chain: RosterChain,
    next_id: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepartureStatus {
    Active,
    Absent(ChainPin),
}
impl DepartureClient {
    pub fn connect(
        chain: RosterChain,
        signer: Arc<dyn Signer>,
        target: Enrollment,
        tls: Option<Arc<ClientConfig>>,
        timeout: Duration,
    ) -> Result<Self, RingError> {
        if !target.members.contains(&signer.sign_key())
            || !chain.versions().iter().any(|h| {
                h.roster()
                    .membership()
                    .ok()
                    .and_then(|m| m.unit(&target.id).cloned())
                    .as_ref()
                    == Some(&target)
            })
        {
            return Err(RingError::Invalid(
                "cleanup target/signer not in verified history".into(),
            ));
        }
        let (setup, url, nonce) = open_membership(
            &chain.head().roster().relay_url,
            chain.ring_id(),
            tls,
            timeout,
        )?;
        Ok(Self {
            setup,
            url,
            nonce,
            signer,
            target,
            chain,
            next_id: 1,
        })
    }
    pub fn chain(&self) -> &RosterChain {
        &self.chain
    }
    pub fn sync(&mut self) -> Result<DepartureStatus, RingError> {
        loop {
            let head = self.chain.head();
            let id = self.next_id;
            self.next_id += 1;
            let msg = super::super::wire::leave_message(
                &self.url.origin(),
                self.chain.ring_id(),
                &self.nonce,
                &self.signer.sign_key(),
                &self.target,
                head.version(),
                &head.hash(),
                head.version(),
            );
            self.setup.send(
                ClientFrame::LeaveSync {
                    id,
                    since: head.version(),
                    pin_version: head.version(),
                    pin_hash: head.hash(),
                    target: self.target.clone(),
                    sign_key: self.signer.sign_key(),
                    sig: Signature::from_bytes(self.signer.sign(&msg)?),
                }
                .encode(),
            )?;
            let RelayFrame::RosterChain { rosters, more, .. } = reply(&mut self.setup, id)? else {
                return Err(RingError::Protocol("expected departure chain".into()));
            };
            if more && rosters.is_empty() {
                return Err(RingError::Protocol("empty departure page".into()));
            }
            let parsed = rosters
                .iter()
                .map(|t| SignedRoster::parse(t))
                .collect::<Result<Vec<_>, _>>()?;
            self.chain.accept(&parsed)?;
            if !more {
                break;
            }
        }
        let head = self.chain.head();
        let m = head.roster().membership()?;
        match m.unit(&self.target.id) {
            None if m.retired.contains(&self.target.id) => Ok(DepartureStatus::Absent(ChainPin {
                version: head.version(),
                hash: head.hash(),
            })),
            Some(u) if u == &self.target && head.member(&self.signer.sign_key()).is_some() => {
                Ok(DepartureStatus::Active)
            }
            _ => Err(RingError::Invalid("departure unit invariant broken".into())),
        }
    }
    /// Fetch/verify, sign the exact current head locally, submit, then verify accepted absence.
    /// A concurrent head discards the old candidate; no Worker can edit or rebase its bytes.
    pub fn complete(&mut self) -> Result<DepartureStatus, RingError> {
        for _ in 0..5 {
            if let status @ DepartureStatus::Absent(_) = self.sync()? {
                return Ok(status);
            }
            let head = self.chain.head();
            let mut r = head.roster().clone();
            r.version += 1;
            r.prev = Some(head.hash());
            r.signed_by = self.signer.sign_key();
            let mut m = r.membership()?;
            m.change = RosterChange::Remove {
                enrollment_id: self.target.id.clone(),
            };
            m.enrollments.retain(|e| e.id != self.target.id);
            m.retired.push(self.target.id.clone());
            r.members
                .retain(|m| !self.target.members.contains(&m.sign_key));
            r.set_membership(&m);
            let signed = r.sign(&*self.signer)?;
            self.chain.extended(std::slice::from_ref(&signed))?;
            let id = self.next_id;
            self.next_id += 1;
            self.setup.send(
                ClientFrame::LeavePut {
                    id,
                    roster: signed.token().into(),
                }
                .encode(),
            )?;
            match reply(&mut self.setup, id) {
                Ok(RelayFrame::Ok { .. }) => return self.sync(),
                Err(RingError::Relay {
                    code: ErrorCode::RosterConflict,
                    ..
                }) => continue,
                Err(e) => return Err(e),
                _ => {
                    return Err(RingError::Protocol(
                        "expected departure acknowledgment".into(),
                    ))
                }
            }
        }
        Err(RingError::Relay {
            code: ErrorCode::RosterConflict,
            detail: Some("departure retry limit".into()),
        })
    }
}

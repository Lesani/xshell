//! Blocking, read-only proposal transport. No Ring authentication or ordinary client IO.
use super::membership::open_membership;
use super::*;
use crate::ring::relay::proposal::ProposalHandle;
use crate::ring::{membership::verify_enroll_proposal, Roster};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct VerifiedProposal {
    pub chain: RosterChain,
    pub proposal: Roster,
    pub handle: ProposalHandle,
}
impl RingClient {
    pub fn create_proposal(
        &self,
        operation: String,
        guest: SignKey,
        final_noise_hash: [u8; 32],
        expires_at: u64,
        proposal: &Roster,
    ) -> Result<ProposalHandle, RingError> {
        let head = self.roster();
        verify_enroll_proposal(&head, proposal)?;
        match self.membership_request(Kind::ProposalCreated, |id| ClientFrame::ProposalCreate {
            id,
            operation,
            head: head.hash(),
            guest,
            context: b64::encode(&final_noise_hash),
            expires_at,
            proposal: serde_json::to_string(proposal).expect("Roster serializes"),
        })? {
            RelayFrame::ProposalCreated { handle, .. } if handle.validate() => Ok(handle),
            _ => Err(RingError::Protocol("expected proposal.created".into())),
        }
    }
    pub fn cancel_proposal(&self, handle: &ProposalHandle) -> Result<(), RingError> {
        self.request_start(None, |id| ClientFrame::ProposalCancel {
            id,
            grant: handle.grant.clone(),
        })?
        .wait(self.timeouts.request)
    }
}

/// One fresh challenge/possession-authenticated metadata socket. The operation owner can
/// drop it to cancel; it can never authenticate as a Ring member or route Session frames.
pub struct ProposalReader {
    setup: Setup,
    handle: ProposalHandle,
    pages: Vec<[u64; 2]>,
    cursor: u64,
    timeout: Duration,
    deadline: Option<Instant>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}
impl ProposalReader {
    pub fn connect(
        handle: &ProposalHandle,
        signer: &dyn Signer,
        tls: Option<Arc<ClientConfig>>,
        timeout: Duration,
    ) -> Result<Self, RingError> {
        if !handle.validate() || signer.sign_key() != handle.guest {
            return Err(RingError::Invalid("proposal handle/guest".into()));
        }
        let (mut setup, url, nonce) =
            open_membership(&handle.relay_url, &handle.ring_id, tls, timeout)?;
        let sig = Signature::from_bytes(signer.sign(&handle.read_message(&url.origin(), &nonce))?);
        setup.send(
            ClientFrame::ProposalRead {
                handle: handle.clone(),
                sig,
            }
            .encode(),
        )?;
        let (cursor, pages) = match frame(&mut setup)? {
            RelayFrame::ProposalReady { cursor, pages } => (cursor, pages),
            _ => return Err(RingError::Protocol("expected proposal.ready".into())),
        };
        if pages.is_empty()
            || pages.len() > 4096
            || cursor > pages.len() as u64
            || pages[0][0] != 0
            || pages.last().unwrap()[1] != handle.version
            || pages.iter().any(|p| p[0] >= p[1])
            || pages.windows(2).any(|p| p[0][1] != p[1][0])
        {
            return Err(RingError::Protocol("proposal page manifest".into()));
        }
        Ok(Self {
            setup,
            handle: handle.clone(),
            pages,
            cursor,
            timeout,
            deadline: None,
            cancel: None,
        })
    }
    pub fn with_cancel(mut self, cancel: Option<Arc<std::sync::atomic::AtomicBool>>) -> Self {
        self.cancel = cancel;
        self
    }
    fn response(&mut self) -> Result<RelayFrame, RingError> {
        let until = self.deadline.map_or(Instant::now() + self.timeout, |d| {
            d.min(Instant::now() + self.timeout)
        });
        loop {
            if self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(RingError::Invalid("proposal read cancelled".into()));
            }
            if Instant::now() >= until {
                return Err(RingError::Timeout);
            }
            self.setup
                .ws
                .get_mut()
                .set_deadline(until.min(Instant::now() + Duration::from_millis(100)))
                .map_err(|e| RingError::Connect(e.to_string()))?;
            match frame(&mut self.setup) {
                Err(RingError::Timeout) => continue,
                other => return other,
            }
        }
    }
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    /// The caller may retain already read exact pages in memory across a new challenge.
    /// The server admits only its current cursor or the immediately preceding page.
    pub fn page(&mut self, cursor: u64) -> Result<(Vec<String>, Option<String>), RingError> {
        self.setup
            .ws
            .get_mut()
            .set_deadline(self.deadline.map_or(Instant::now() + self.timeout, |d| {
                d.min(Instant::now() + self.timeout)
            }))
            .map_err(|e| RingError::Connect(e.to_string()))?;
        self.setup
            .send(ClientFrame::ProposalGet { cursor }.encode())?;
        match self.response()? {
            RelayFrame::ProposalPage {
                cursor: got,
                chain,
                proposal,
            } if got == cursor => {
                let cut = self
                    .pages
                    .get(cursor as usize)
                    .ok_or_else(|| RingError::Protocol("page cursor".into()))?;
                if chain.len() as u64 != cut[1] - cut[0]
                    || proposal.is_some() != (cursor as usize == self.pages.len() - 1)
                {
                    return Err(RingError::Protocol("proposal page shape".into()));
                }
                self.cursor = self.cursor.max(cursor + 1);
                Ok((chain, proposal))
            }
            _ => Err(RingError::Protocol("expected proposal.page".into())),
        }
    }
    /// Fetch all pages and independently verify the frozen digest, complete chain,
    /// approving Desktop and exact unsigned delta. Caller additionally checks its target,
    /// offer deadline and real Noise transcript before presenting consent.
    pub fn fetch_until(mut self, until: Instant) -> Result<VerifiedProposal, RingError> {
        self.deadline = Some(until);
        self.fetch()
    }
    pub fn fetch(mut self) -> Result<VerifiedProposal, RingError> {
        if self.cursor != 0 {
            return Err(RingError::Invalid("resume requires retained pages".into()));
        }
        let mut tokens = Vec::new();
        let mut unsigned = String::new();
        let mut total = 0;
        for i in 0..self.pages.len() {
            let (chain, proposal) = self.page(i as u64)?;
            for t in &chain {
                if t.len() > 65536 {
                    return Err(RingError::Invalid("proposal token limit".into()));
                }
                total += t.len();
            }
            total += proposal.as_ref().map_or(0, String::len);
            if total > super::super::wire::MAX_CANDIDATE_BYTES {
                return Err(RingError::Invalid("proposal candidate limit".into()));
            }
            tokens.extend(chain);
            if let Some(p) = proposal {
                unsigned = p;
            }
        }
        #[derive(Serialize)]
        struct Resource<'a> {
            chain: &'a [String],
            proposal: &'a str,
            pages: &'a [[u64; 2]],
        }
        let bytes = serde_json::to_vec(&Resource {
            chain: &tokens,
            proposal: &unsigned,
            pages: &self.pages,
        })
        .expect("resource serializes");
        if b64::encode(&Sha256::digest(&bytes)) != self.handle.resource {
            return Err(RingError::Invalid("proposal resource digest".into()));
        }
        let chain = RosterChain::from_tokens(&tokens)?;
        if chain.head().hash() != self.handle.head
            || chain.head().version() != self.handle.version
            || chain.head().ring_id() != &self.handle.ring_id
            || chain.head().roster().relay_url != self.handle.relay_url
        {
            return Err(RingError::Invalid("proposal predecessor".into()));
        }
        crate::ring::json::strict_object(unsigned.as_bytes()).map_err(RingError::Invalid)?;
        let proposal: Roster = serde_json::from_slice(unsigned.as_bytes())
            .map_err(|_| RingError::Invalid("unsigned proposal".into()))?;
        verify_enroll_proposal(chain.head(), &proposal)?;
        let hash = b64::decode_array::<32>(&self.handle.context)
            .map_err(|_| RingError::Invalid("proposal context".into()))?;
        let completed = crate::ring::relay::proposal::completed_token_bytes(
            &proposal,
            self.handle.expires_at,
            &hash,
        )?;
        let chain_bytes: usize = tokens.iter().map(String::len).sum();
        if completed > 65536 || chain_bytes + completed > super::super::wire::MAX_CANDIDATE_BYTES {
            return Err(RingError::Invalid(
                "completed proposal candidate limit".into(),
            ));
        }
        Ok(VerifiedProposal {
            chain,
            proposal,
            handle: self.handle,
        })
    }
}
fn frame(setup: &mut Setup) -> Result<RelayFrame, RingError> {
    let text = setup.recv()?;
    match decode_relay(&text).map_err(|e| RingError::Protocol(e.to_string()))? {
        RelayFrame::Error { code, detail, .. } => Err(RingError::Relay { code, detail }),
        f => Ok(f),
    }
}

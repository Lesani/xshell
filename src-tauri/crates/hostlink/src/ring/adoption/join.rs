//! Desktop-first Join and enrolled-Desktop approval. The only live words are in the
//! returned offer/driver; local confirmation is an in-process capability, never Host RPC.
use super::*;
use crate::ring::store::{OutgoingAdoption, OutgoingPhase, PendingJoin};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::pairing::adoption::{possession_message, AdoptionSigners};
use xshell_protocol::ring::pairing::{Joined, PairError};
use xshell_protocol::ring::relay::pair::{
    adopt_as_guest_with_ready, approve_adoption, AdoptionGuestRequest, AdoptionHostRequest,
    PairOptions, PairPipe,
};
use xshell_protocol::ring::relay::proposal::ProposalHandle;
use xshell_protocol::ring::relay::VerifiedProposal;
use xshell_protocol::ring::{b64, SignError};

/// Immutable, memory-only CLI/desktop offer. No target keys or consent authority.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JoinOfferInput {
    pub operation: String,
    #[serde(with = "offer_code")]
    pub code: DeviceCode,
    pub relay_url: String,
    pub expires_at: u64,
}
impl std::fmt::Debug for JoinOfferInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JoinOfferInput(..)")
    }
}
mod offer_code {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(code: &DeviceCode, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&code.format())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DeviceCode, D::Error> {
        let text = zeroize::Zeroizing::new(String::deserialize(d)?);
        DeviceCode::parse(&text).map_err(|_| serde::de::Error::custom("invalid device code"))
    }
}
impl JoinOfferInput {
    pub fn validate(&self) -> Result<(), String> {
        let operation =
            b64::decode_array::<32>(&self.operation).map_err(|_| "invalid Join wire operation")?;
        if b64::encode(&operation) != self.operation || self.relay_url.len() > 2048 {
            return Err("invalid Join offer".into());
        }
        xshell_protocol::ring::url::RelayUrl::parse(&self.relay_url)
            .map_err(|_| "invalid Join Relay")?;
        let left = self.expires_at.saturating_sub(now());
        if left == 0 || left > 600 {
            return Err("Join offer expired or exceeds ten minutes".into());
        }
        Ok(())
    }
    pub(in crate::ring) fn same_offer(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.expires_at == other.expires_at
            && self.relay_url == other.relay_url
            && self.code.format() == other.code.format()
    }
}

#[derive(Clone, Debug)]
pub enum JoinStatus {
    Waiting,
    AwaitingConfirmation(Box<ConsentPreview>),
    Completing,
    Installed { version: u64 },
    Declined,
    Failed { error: String },
    ReconciliationRequired { error: String },
}
struct JoinState {
    status: JoinStatus,
    lease: Option<CreateConfirmationLease>,
    decision: Option<CreateDecision>,
    ready: bool,
}
struct JoinShared {
    state: Mutex<JoinState>,
    changed: Condvar,
    cancel: Arc<AtomicBool>,
}
struct JoinDriver {
    keys: Arc<DeviceKeys>,
    profile: CompanionProfile,
    intent: AdoptionIntent,
    relay: String,
    code: DeviceCode,
    until: Instant,
}

/// Words are returned once to this in-process origin, not status/events or disk. Drop
/// cancels and joins the bounded driver; its human wait is our cancellable Condvar loop.
pub struct JoinOffer {
    pub operation: String,
    pub code: DeviceCode,
    pub expires_at: u64,
    shared: Arc<JoinShared>,
    worker: Option<JoinHandle<Result<AdoptionOutcome, String>>>,
}
impl JoinOffer {
    pub fn is_ready(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ready
    }
    /// Wait for the actual Relay slot acknowledgment before displaying Desktop words.
    pub fn await_ready(&self, timeout: Duration) -> Result<(), String> {
        let until = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.ready {
                return Ok(());
            }
            if let JoinStatus::Failed { error } | JoinStatus::ReconciliationRequired { error } =
                &state.status
            {
                return Err(error.clone());
            }
            if Instant::now() >= until {
                return Err("waiting for Join readiness timed out".into());
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, until.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    pub fn join_status(&self) -> JoinStatus {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }
    pub fn await_consent(
        &self,
        timeout: Duration,
    ) -> Result<(ConsentPreview, CreateConfirmationLease), String> {
        let until = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &state.status {
                JoinStatus::AwaitingConfirmation(preview) if state.decision.is_none() => {
                    return Ok((
                        (**preview).clone(),
                        state.lease.clone().ok_or("Join lease missing")?,
                    ))
                }
                JoinStatus::Failed { error } | JoinStatus::ReconciliationRequired { error } => {
                    return Err(error.clone())
                }
                JoinStatus::Installed { .. } | JoinStatus::Declined => {
                    return Err("Join no longer awaits confirmation".into())
                }
                _ => {}
            }
            if Instant::now() >= until {
                return Err("waiting for verified Join preview timed out".into());
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, until.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    pub fn confirm_join(&self, confirmation: CreateConfirmation) -> Result<(), String> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(state.status, JoinStatus::AwaitingConfirmation(_)) || state.decision.is_some()
        {
            return Err("Join is not awaiting confirmation".into());
        }
        state
            .lease
            .as_ref()
            .ok_or("Join lease missing")?
            .verify(&confirmation)?;
        state.decision = Some(CreateDecision::Confirm(confirmation));
        self.shared.changed.notify_all();
        Ok(())
    }
    pub fn decline_join(&self) -> Result<(), String> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(state.status, JoinStatus::AwaitingConfirmation(_)) || state.decision.is_some()
        {
            return Err("Join is not awaiting confirmation".into());
        }
        state.decision = Some(CreateDecision::Decline);
        self.shared.changed.notify_all();
        Ok(())
    }
    pub fn cancel_join(&self) {
        self.shared.cancel.store(true, Ordering::Release);
        self.shared.changed.notify_all();
    }
    pub fn wait(mut self) -> Result<AdoptionOutcome, String> {
        self.worker
            .take()
            .ok_or("Join driver missing")?
            .join()
            .map_err(|_| "Join driver panicked".to_string())?
    }
}
impl Drop for JoinOffer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.cancel_join();
            let _ = worker.join();
        }
    }
}

#[derive(Debug)]
pub struct ApprovalOutcome {
    pub operation: String,
    pub chain: RosterChain,
}
struct AllowedConsent {
    message: Vec<u8>,
    context: ConsentContext,
}
#[derive(Default)]
struct SigningState {
    h2: Option<[u8; 32]>,
    desktop_possession: bool,
    daemon_possession: bool,
    read: Option<ProposalHandle>,
    allowed: Option<AllowedConsent>,
    desktop_proof: Option<ConsentProof>,
    daemon_consent: bool,
}
struct LocalPrincipal<'a> {
    ring: &'a DesktopRing,
    keys: Arc<DeviceKeys>,
    intent: &'a AdoptionIntent,
    relay: &'a str,
    until: Instant,
    cancel: Arc<AtomicBool>,
    state: &'a Mutex<SigningState>,
}
struct LocalCounterpart<'a> {
    principal: &'a LocalPrincipal<'a>,
    client: &'a CompanionClient,
    key: SignKey,
}
impl LocalPrincipal<'_> {
    fn live(&self) -> Result<(), SignError> {
        if self.cancel.load(Ordering::Acquire)
            || Instant::now() >= self.until
            || now() >= self.intent.expires_at
            || !self.ring.pairing_ipc_owner()
        {
            return Err(SignError("inactive local Join signer".into()));
        }
        Ok(())
    }
    fn possession(&self, message: &[u8]) -> Option<[u8; 32]> {
        let text = std::str::from_utf8(message).ok()?;
        let fields: Vec<_> = text.split('\n').collect();
        if fields.len() != 3 || fields[0] != "xshell-adoption-possession-v2" {
            return None;
        }
        let h2 = b64::decode_array::<32>(fields[1]).ok()?;
        (possession_message(self.intent, &h2) == message).then_some(h2)
    }
    fn read_handle(&self, message: &[u8]) -> Option<ProposalHandle> {
        let fields: Vec<_> = std::str::from_utf8(message).ok()?.split('\n').collect();
        if fields.len() != 13
            || fields[0] != "xshell-proposal-read-v2"
            || b64::decode_array::<32>(fields[12]).is_err()
        {
            return None;
        }
        let handle = ProposalHandle {
            ring_id: RingId::parse(fields[2]).ok()?,
            relay_url: fields[3].into(),
            grant: fields[4].into(),
            resource: fields[5].into(),
            operation: fields[6].into(),
            head: fields[7].into(),
            version: fields[8].parse().ok()?,
            guest: SignKey::parse(fields[9]).ok()?,
            context: fields[10].into(),
            expires_at: fields[11].parse().ok()?,
        };
        let relay = xshell_protocol::ring::url::RelayUrl::parse(self.relay).ok()?;
        if !handle.validate()
            || handle.operation != self.intent.operation
            || handle.expires_at != self.intent.expires_at
            || handle.guest != self.keys.sign_key()
            || xshell_protocol::ring::url::RelayUrl::parse(&handle.relay_url).ok()? != relay
            || handle.read_message(&relay.origin(), fields[12]) != message
        {
            return None;
        }
        Some(handle)
    }
}
impl Signer for LocalPrincipal<'_> {
    fn sign_key(&self) -> SignKey {
        self.keys.sign_key()
    }
    fn sign(&self, message: &[u8]) -> Result<[u8; 64], SignError> {
        self.live()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h2) = self.possession(message) {
            if state.desktop_possession || state.allowed.is_some() {
                return Err(SignError("Join possession is one-shot".into()));
            }
            state.h2 = Some(h2);
            state.desktop_possession = true;
            return self.keys.sign(message);
        }
        // adopt_as_guest checks the encrypted handle against the actual final Noise
        // hash before this narrow read-possession request. Its verified callback must
        // later match this exact observed handle before any consent can be enabled.
        if let Some(handle) = self.read_handle(message) {
            if !state.desktop_possession
                || !state.daemon_possession
                || state.read.is_some()
                || state.allowed.is_some()
            {
                return Err(SignError("Join proposal read is not admitted".into()));
            }
            state.read = Some(handle);
            return self.keys.sign(message);
        }
        let allowed = state
            .allowed
            .as_ref()
            .filter(|allowed| allowed.message == message)
            .ok_or_else(|| SignError("arbitrary or unconfirmed local signing refused".into()))?;
        if state.desktop_proof.is_some() {
            return Err(SignError("Join consent is one-shot".into()));
        }
        let signature = self.keys.sign(message)?;
        state.desktop_proof = Some(ConsentProof {
            key: self.keys.sign_key(),
            expires_at: self.intent.expires_at,
            context: allowed.context.clone(),
            signature: Signature::from_bytes(signature),
        });
        Ok(signature)
    }
}
impl Signer for LocalCounterpart<'_> {
    fn sign_key(&self) -> SignKey {
        self.key
    }
    fn sign(&self, message: &[u8]) -> Result<[u8; 64], SignError> {
        self.principal.live()?;
        let mut state = self
            .principal
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(h2) = self.principal.possession(message) {
            if !state.desktop_possession
                || state.h2 != Some(h2)
                || state.daemon_possession
                || state.allowed.is_some()
            {
                return Err(SignError("counterpart possession is not admitted".into()));
            }
            state.daemon_possession = true;
            return match self
                .client
                .request(CompanionRequest::Possession {
                    operation: self.principal.intent.operation.clone(),
                    handshake_hash: b64::encode(&h2),
                })
                .map_err(SignError)?
            {
                CompanionResponse::Possession { signature } => Ok(*signature.as_bytes()),
                _ => Err(SignError("invalid counterpart possession reply".into())),
            };
        }
        let allowed = state
            .allowed
            .as_ref()
            .filter(|allowed| allowed.message == message)
            .ok_or_else(|| {
                SignError("arbitrary or unconfirmed counterpart signing refused".into())
            })?;
        let context = allowed.context.clone();
        let desktop = state
            .desktop_proof
            .as_ref()
            .filter(|proof| proof.context == context)
            .ok_or_else(|| SignError("exact principal consent missing".into()))?
            .clone();
        if state.daemon_consent {
            return Err(SignError("counterpart consent is one-shot".into()));
        }
        state.daemon_consent = true;
        match self
            .client
            .request(CompanionRequest::Consent {
                operation: self.principal.intent.operation.clone(),
                desktop_proof: desktop,
            })
            .map_err(SignError)?
        {
            CompanionResponse::Consent { proof }
                if proof.key == self.key
                    && proof.context == context
                    && proof.expires_at == self.principal.intent.expires_at
                    && xshell_protocol::ring::verify(&proof.key, message, &proof.signature) =>
            {
                Ok(*proof.signature.as_bytes())
            }
            _ => Err(SignError("invalid counterpart consent reply".into())),
        }
    }
}
impl DesktopRing {
    pub(super) fn record_driver_cancel(&self, cancel: &Arc<AtomicBool>) -> Result<(), String> {
        *self
            .adoption_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(cancel));
        if !self.pairing_ipc_owner() {
            cancel.store(true, Ordering::Release);
            return Err("adoption owner ended before driver start".into());
        }
        Ok(())
    }
    /// Starts the actual guest driver. The returned object is the only word-bearing offer
    /// and the local confirmation origin; statuses and durable reservations have no words.
    pub fn begin_join(self: &Arc<Self>, client: &CompanionClient) -> Result<JoinOffer, String> {
        let ttl = self.cfg.pair_ttl.min(Duration::from_secs(600));
        let offer = self.begin_join_with_offer(
            client,
            JoinOfferInput {
                operation: nonce()?,
                code: DeviceCode::generate().map_err(|e| e.to_string())?,
                relay_url: self.cfg.default_relay_url.clone(),
                expires_at: now() + ttl.as_secs(),
            },
            Instant::now() + ttl,
        )?;
        offer.await_ready(ttl)?;
        Ok(offer)
    }

    /// Inject one already-created origin offer; no replacement code, target or deadline.
    pub fn begin_join_with_offer(
        self: &Arc<Self>,
        client: &CompanionClient,
        offer: JoinOfferInput,
        original_until: Instant,
    ) -> Result<JoinOffer, String> {
        self.begin_join_with_offer_cancel(
            client,
            offer,
            original_until,
            Arc::new(AtomicBool::new(false)),
        )
    }
    pub(in crate::ring) fn begin_join_with_offer_cancel(
        self: &Arc<Self>,
        client: &CompanionClient,
        offer: JoinOfferInput,
        original_until: Instant,
        cancel: Arc<AtomicBool>,
    ) -> Result<JoinOffer, String> {
        offer.validate()?;
        if cancel.load(Ordering::Acquire) {
            return Err("Join cancelled before start".into());
        }
        let until = original_until
            .min(Instant::now() + Duration::from_secs(offer.expires_at.saturating_sub(now())));
        if Instant::now() >= until {
            return Err("Join offer expired".into());
        }
        let admission = self.reserve_adoption()?;
        self.begin_join_admitted(client, offer, until, cancel, admission)
    }

    /// Only the existing registered-origin dispatch waits for pure admission. Never retry
    /// registration, reservation, counterpart Start or any other constructor work.
    pub(in crate::ring) fn begin_registered_origin_join(
        self: &Arc<Self>,
        client: &CompanionClient,
        offer: JoinOfferInput,
        original_until: Instant,
        cancel: Arc<AtomicBool>,
    ) -> Result<JoinOffer, String> {
        offer.validate()?;
        let until = original_until
            .min(Instant::now() + Duration::from_secs(offer.expires_at.saturating_sub(now())));
        self.adoption_test_point(
            super::super::desktop::AdoptionTestPoint::OriginJoinBeforeAdmission,
        );
        let admission = loop {
            self.check_join_origin(&offer, until, &cancel)?;
            if let Some(admission) = self.try_reserve_adoption() {
                break admission;
            }
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::OriginJoinAdmissionBusy,
            );
            std::thread::sleep(
                Duration::from_millis(20).min(until.saturating_duration_since(Instant::now())),
            );
        };
        self.check_join_origin(&offer, until, &cancel)?;
        self.adoption_test_point(super::super::desktop::AdoptionTestPoint::OriginJoinAdmitted);
        self.begin_join_admitted(client, offer, until, cancel, admission)
    }

    fn check_join_origin(
        &self,
        offer: &JoinOfferInput,
        until: Instant,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Acquire) || Instant::now() >= until || now() >= offer.expires_at {
            return Err("Join origin expired/cancelled before reservation".into());
        }
        if !self.pairing_ipc_owner() {
            return Err("another process owns Join".into());
        }
        Ok(())
    }

    fn begin_join_admitted(
        self: &Arc<Self>,
        client: &CompanionClient,
        offer: JoinOfferInput,
        until: Instant,
        cancel: Arc<AtomicBool>,
        admission: super::AdoptionAdmission,
    ) -> Result<JoinOffer, String> {
        // The same guard continues into the worker; no release/reacquire gap. Re-read all
        // profile, identity and recovery fences only after admission has been acquired.
        self.check_join_origin(&offer, until, &cancel)?;
        if let Some(stored) = self.store.load_profile()?.0 {
            if stored.membership.is_some() {
                return Err("leave_required: Join needs an unpaired Desktop".into());
            }
            if stored.pending_join.is_some()
                || stored
                    .journal
                    .as_ref()
                    .is_some_and(|journal| !journal.phase.permits_new_operation())
            {
                return Err("reconciliation_required: an earlier Join is unresolved".into());
            }
        }
        let (keys, profile) = self.register_companion_inner(client)?;
        self.check_join_origin(&offer, until, &cancel)?;
        let intent = AdoptionIntent {
            operation: offer.operation,
            expires_at: offer.expires_at,
            target: profile.target(),
        };
        let code = offer.code;
        let relay = offer.relay_url;
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            if stored.membership.is_some() || stored.pending_join.is_some() {
                return Err("Join reservation changed".into());
            }
            stored.pending_join = Some(PendingJoin {
                retained_predecessor: vec![],
                intent: intent.clone(),
                relay_url: relay.clone(),
            });
            Ok((Some(stored), ()))
        })?;
        // No proof signer has started. Retain the reservation if counterpart admission
        // is uncertain; outage is not permission to manufacture a replacement offer.
        self.adoption_test_point(
            super::super::desktop::AdoptionTestPoint::JoinBeforeCounterpartStart,
        );
        client.request(CompanionRequest::Start {
            intent: intent.clone(),
        })?;
        self.record_driver_cancel(&cancel)?;
        let shared = Arc::new(JoinShared {
            state: Mutex::new(JoinState {
                status: JoinStatus::Waiting,
                lease: None,
                decision: None,
                ready: false,
            }),
            changed: Condvar::new(),
            cancel,
        });
        let (driver, ring, companion, offered, operation, expires_at) = (
            shared.clone(),
            self.clone(),
            client.clone(),
            code.clone(),
            intent.operation.clone(),
            intent.expires_at,
        );
        let worker = std::thread::Builder::new()
            .name("computer-join".into())
            .spawn(move || {
                let _admission = admission;
                ring.adoption_test_point(
                    super::super::desktop::AdoptionTestPoint::JoinWorkerStarted,
                );
                let result = ring.run_join(
                    &companion,
                    JoinDriver {
                        keys,
                        profile,
                        intent,
                        relay,
                        code: offered,
                        until,
                    },
                    &driver,
                );
                driver.cancel.store(true, Ordering::Release); // terminal driver ends its confirmation capability
                let mut state = driver.state.lock().unwrap_or_else(|e| e.into_inner());
                state.status = match &result {
                    Ok(AdoptionOutcome::Installed { chain, .. }) => JoinStatus::Installed {
                        version: chain.head().version(),
                    },
                    Ok(AdoptionOutcome::Declined { .. }) => JoinStatus::Declined,
                    Err(error) if error.starts_with("reconciliation_required:") => {
                        JoinStatus::ReconciliationRequired {
                            error: error.clone(),
                        }
                    }
                    Err(error) => JoinStatus::Failed {
                        error: error.clone(),
                    },
                };
                driver.changed.notify_all();
                result
            })
            .map_err(|e| e.to_string())?;
        Ok(JoinOffer {
            operation,
            code,
            expires_at,
            shared,
            worker: Some(worker),
        })
    }
    fn run_join(
        &self,
        client: &CompanionClient,
        driver: JoinDriver,
        shared: &JoinShared,
    ) -> Result<AdoptionOutcome, String> {
        let JoinDriver {
            keys,
            profile,
            intent,
            relay,
            code,
            until,
        } = driver;
        if !self.pairing_ipc_owner() {
            shared.cancel.store(true, Ordering::Release);
        }
        let signing = Mutex::new(SigningState::default());
        let principal = LocalPrincipal {
            ring: self,
            keys: keys.clone(),
            intent: &intent,
            relay: &relay,
            until,
            cancel: shared.cancel.clone(),
            state: &signing,
        };
        let counterpart = LocalCounterpart {
            principal: &principal,
            client,
            key: profile.daemon.sign_key,
        };
        let secret = code.secret();
        let mut declined = false;
        let result = adopt_as_guest_with_ready(
            &AdoptionGuestRequest {
                relay_url: &relay,
                secret: &secret,
                keys,
                signers: AdoptionSigners {
                    principal: &principal,
                    companion: Some(&counterpart),
                },
                intent: &intent,
                until,
                approver: None,
            },
            &PairOptions {
                step: self.cfg.timeouts.request.min(Duration::from_secs(15)),
                ring: self.cfg.timeouts,
                ..Default::default()
            },
            Some(&shared.cancel),
            || {
                if shared.cancel.load(Ordering::Acquire) || !self.pairing_ipc_owner() {
                    return Err(PairError::Cancelled);
                }
                self.adoption_test_point(super::super::desktop::AdoptionTestPoint::JoinSlotReady);
                shared.state.lock().unwrap_or_else(|e| e.into_inner()).ready = true;
                shared.changed.notify_all();
                Ok(())
            },
            |request| {
                let verified = request.verified;
                if signing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .read
                    .as_ref()
                    != Some(&verified.handle)
                {
                    return Err(PairError::Pin);
                }
                let hash = b64::decode_array::<32>(&verified.handle.context)
                    .map_err(|_| PairError::Crypto)?;
                let context = pairing_consent_context(&verified.proposal, &hash)
                    .map_err(|e| PairError::Payload(e.to_string()))?;
                let proposal_hash = consent_proposal_hash(&verified.proposal)
                    .map_err(|e| PairError::Payload(e.to_string()))?;
                let predecessor: Vec<String> = verified
                    .chain
                    .versions()
                    .iter()
                    .map(|roster| roster.token().into())
                    .collect();
                self.store
                    .transact_profile(|stored, _| {
                        let mut stored = stored.ok_or("retained Join profile missing")?;
                        if stored.pending_join.as_ref().is_none_or(|pending| {
                            pending.intent != intent || pending.relay_url != relay
                        }) || stored.membership.is_some()
                        {
                            return Err("Join reservation changed before preview".into());
                        }
                        stored.pending_join = None;
                        stored.journal = Some(AdoptionJournal {
                            intent: intent.clone(),
                            predecessor: predecessor.clone(),
                            proposal: verified.proposal.clone(),
                            context: context.clone(),
                            transcript: ConsentTranscript::Pairing {
                                final_noise_hash: verified.handle.context.clone(),
                            },
                            phase: AdoptionPhase::Prepared,
                            signed_candidate: None,
                            accepted: vec![],
                            desktop_installed: false,
                            daemon_installed: false,
                        });
                        Ok((Some(stored), ()))
                    })
                    .map_err(PairError::Relay)?;
                client
                    .request(CompanionRequest::Prepare {
                        proposal: Box::new(CompanionProposal {
                            operation: intent.operation.clone(),
                            predecessor,
                            proposal: verified.proposal.clone(),
                            transcript: ConsentTranscript::Pairing {
                                final_noise_hash: verified.handle.context.clone(),
                            },
                        }),
                    })
                    .map_err(PairError::Relay)?;
                let lease = CreateConfirmationLease {
                    operation: intent.operation.clone(),
                    proposal_hash: proposal_hash.clone(),
                    until: request.until,
                    expires_at: intent.expires_at,
                    cancelled: shared.cancel.clone(),
                };
                let preview = ConsentPreview {
                    operation: intent.operation.clone(),
                    proposal_hash,
                    ring_id: verified.proposal.ring_id.clone(),
                    relay_url: verified.proposal.relay_url.clone(),
                    profile: profile.clone(),
                    context: context.clone(),
                    proposal: verified.proposal.clone(),
                    expires_at: intent.expires_at,
                };
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                state.status = JoinStatus::AwaitingConfirmation(Box::new(preview));
                state.lease = Some(lease.clone());
                shared.changed.notify_all();
                let confirmation = loop {
                    self.create_live(&lease).map_err(PairError::Relay)?;
                    if let Some(decision) = state.decision.take() {
                        break decision;
                    }
                    state = shared
                        .changed
                        .wait_timeout(state, Duration::from_millis(50))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                };
                if matches!(confirmation, CreateDecision::Decline) {
                    declined = true;
                    return Ok(false);
                }
                let CreateDecision::Confirm(proof) = confirmation else {
                    unreachable!()
                };
                lease.verify(&proof).map_err(PairError::Relay)?;
                self.create_live(&lease).map_err(PairError::Relay)?;
                state.status = JoinStatus::Completing;
                drop(state);
                self.journal_update(&intent.operation, |journal| {
                    journal.phase = AdoptionPhase::ConsentMayHaveLeft
                })
                .map_err(PairError::Relay)?;
                signing.lock().unwrap_or_else(|e| e.into_inner()).allowed = Some(AllowedConsent {
                    message: consent_message(&verified.proposal, intent.expires_at, &context)
                        .map_err(|e| PairError::Payload(e.to_string()))?,
                    context,
                });
                Ok(true)
            },
        );
        match result {
            Ok((accepted, _)) => {
                self.install_joined_computer(client, &intent.operation, &profile, &accepted)
                    .map_err(|error| format!("reconciliation_required: {error}"))?;
                Ok(AdoptionOutcome::Installed {
                    operation: intent.operation,
                    chain: accepted,
                })
            }
            Err(error) => {
                let stored = self
                    .store
                    .load_profile()?
                    .0
                    .ok_or("retained Join profile missing")?;
                let uncertain = stored.journal.as_ref().is_some_and(|journal| {
                    journal.intent.operation == intent.operation
                        && !matches!(
                            journal.phase,
                            AdoptionPhase::Prepared | AdoptionPhase::Cancelled
                        )
                });
                if uncertain {
                    return Err(format!("reconciliation_required: {error}"));
                }
                client
                    .request(CompanionRequest::Cancel {
                        operation: intent.operation.clone(),
                    })
                    .map_err(|failure| format!("reconciliation_required: {failure}"))?;
                self.store.transact_profile(|stored, _| {
                    let mut stored = stored.ok_or("retained Join profile missing")?;
                    if let Some(journal) = stored
                        .journal
                        .as_mut()
                        .filter(|journal| journal.intent.operation == intent.operation)
                    {
                        journal.phase = if declined {
                            AdoptionPhase::DeclinedBeforeProof
                        } else if now() >= journal.intent.expires_at {
                            AdoptionPhase::ExpiredBeforeProof
                        } else {
                            AdoptionPhase::Cancelled
                        };
                    }
                    if stored
                        .pending_join
                        .as_ref()
                        .is_some_and(|pending| pending.intent.operation == intent.operation)
                    {
                        stored.pending_join = None;
                    }
                    Ok((Some(stored), ()))
                })?;
                if declined {
                    Ok(AdoptionOutcome::Declined {
                        operation: intent.operation,
                    })
                } else {
                    Err(error.to_string())
                }
            }
        }
    }

    pub(super) fn outgoing_update(
        &self,
        operation: &str,
        update: impl FnOnce(&mut OutgoingAdoption),
    ) -> Result<(), String> {
        self.store
            .transact_profile(|stored, _| {
                let mut stored = stored.ok_or("retained approver profile missing")?;
                update(
                    stored
                        .outgoing
                        .as_mut()
                        .filter(|journal| journal.intent.operation == operation)
                        .ok_or("outgoing adoption operation changed")?,
                );
                Ok((Some(stored), ()))
            })
            .map(|_| ())
    }
    /// Entered words authorize only this real exchange/proposal. Publication is a Relay
    /// outcome, never a claim that the guest's two private installations have completed.
    pub fn approve_device_code(&self, code: &DeviceCode) -> Result<ApprovalOutcome, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns approval".into());
        }
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("no retained approver profile")?;
        if stored.pending_join.is_some()
            || stored.outgoing.as_ref().is_some_and(|journal| {
                !matches!(
                    journal.phase,
                    OutgoingPhase::Published | OutgoingPhase::Cancelled
                )
            })
        {
            return Err("reconciliation_required: an earlier proposal is unresolved".into());
        }
        let member = stored
            .membership
            .ok_or("approval requires enrolled membership")?;
        if member.chain.head().roster().v != 2
            || member
                .chain
                .head()
                .roster()
                .membership()
                .map_err(|e| e.to_string())?
                .unit_for(&stored.keys.sign_key())
                .is_none_or(|unit| unit.kind != EnrollmentKind::Computer)
            || !member
                .chain
                .authorized_member(&stored.keys.sign_key())
                .is_some_and(|member| member.role == xshell_protocol::ring::Role::Desktop)
        {
            return Err("approval requires an authorized v2 Computer Desktop".into());
        }
        let until = Instant::now() + self.cfg.pair_ttl.min(Duration::from_secs(600));
        let cancel = Arc::new(AtomicBool::new(false));
        self.record_driver_cancel(&cancel)?;
        let relay = member.chain.head().roster().relay_url.clone();
        let ring_id = member.chain.ring_id().clone();
        let secret = code.secret();
        let options = PairOptions {
            step: self.cfg.timeouts.request.min(Duration::from_secs(15)),
            ring: self.cfg.timeouts,
            ..Default::default()
        };
        let pipe = PairPipe::open(&relay, &secret.slot(), &options).map_err(|e| e.to_string())?;
        // Readiness-gated origins already occupy their slot. An absent peer is an
        // unknown/expired/wrong-Relay code, not a new approver-owned offer to wait on.
        if !pipe.has_peer() {
            pipe.close();
            return Err(PairError::NotFound.to_string());
        }

        let lane =
            Mutex::new(None::<Arc<xshell_protocol::ring::relay::connector::ProposalConnection>>);
        let operation = Mutex::new(None::<String>);
        let published = Mutex::new(None::<RosterChain>);
        let result = approve_adoption(
            pipe,
            &AdoptionHostRequest {
                keys: &stored.keys,
                secret: &secret,
                ring_id: &ring_id,
                until,
            },
            Some(&cancel),
            |intent, hash| {
                let ready_until = until.min(Instant::now() + self.cfg.pair_publish_wait);
                let (chain, captured) = loop {
                    if cancel.load(Ordering::Acquire)
                        || Instant::now() >= ready_until
                        || !self.pairing_ipc_owner()
                    {
                        return Err(PairError::Cancelled);
                    }
                    if let Ok(snapshot) = self.proposal_snapshot() {
                        if self
                            .store
                            .load()
                            .map_err(PairError::Relay)?
                            .0
                            .as_ref()
                            .is_some_and(|member| member.chain.head() == snapshot.0.head())
                        {
                            break snapshot;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let proposal = xshell_protocol::ring::pairing::adoption::enroll_proposal(
                    chain.head(),
                    stored.keys.sign_key(),
                    intent,
                    now(),
                )?;
                self.store
                    .transact_profile(|profile, _| {
                        let mut profile = profile.ok_or("retained approver missing")?;
                        if profile
                            .membership
                            .as_ref()
                            .is_none_or(|member| member.chain.head() != chain.head())
                        {
                            return Err(
                                "accepted predecessor changed before proposal preparation".into()
                            );
                        }
                        profile.outgoing = Some(OutgoingAdoption {
                            intent: intent.clone(),
                            predecessor: chain
                                .versions()
                                .iter()
                                .map(|r| r.token().into())
                                .collect(),
                            proposal: proposal.clone(),
                            final_noise_hash: Some(b64::encode(&hash)),
                            phase: OutgoingPhase::Prepared,
                            signed_candidate: None,
                            accepted: vec![],
                        });
                        Ok((Some(profile), ()))
                    })
                    .map_err(PairError::Relay)?;
                *operation.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(intent.operation.clone());
                let captured = Arc::new(captured);
                *lane.lock().unwrap_or_else(|e| e.into_inner()) = Some(captured.clone());
                let handle = captured
                    .create(intent, hash, &proposal)
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                Ok(VerifiedProposal {
                    chain,
                    proposal,
                    handle,
                })
            },
            |signed| {
                self.adoption_test_point(
                    super::super::desktop::AdoptionTestPoint::ApprovalProofsReceived,
                );
                let op = operation
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or(PairError::Cancelled)?;
                self.outgoing_update(&op, |journal| {
                    journal.signed_candidate = Some(signed.token().into())
                })
                .map_err(PairError::Relay)?;
                let original_expiry = self
                    .store
                    .load_profile()
                    .map_err(PairError::Relay)?
                    .0
                    .and_then(|profile| profile.outgoing)
                    .filter(|journal| journal.intent.operation == op)
                    .ok_or(PairError::Cancelled)?
                    .intent
                    .expires_at;
                if cancel.load(Ordering::Acquire)
                    || Instant::now() >= until
                    || now() >= original_expiry
                    || !self.pairing_ipc_owner()
                    || self
                        .store
                        .load()
                        .map_err(PairError::Relay)?
                        .0
                        .as_ref()
                        .is_none_or(|member| {
                            member.chain.head().hash()
                                != signed.roster().prev.clone().unwrap_or_default()
                        })
                {
                    return Err(PairError::Cancelled);
                }
                self.outgoing_update(&op, |journal| {
                    journal.phase = OutgoingPhase::PublicationUncertain
                })
                .map_err(PairError::Relay)?;
                let captured = lane
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or(PairError::Cancelled)?;
                self.adoption_test_point(
                    super::super::desktop::AdoptionTestPoint::ApprovalBeforePublish,
                );
                if cancel.load(Ordering::Acquire)
                    || now() >= original_expiry
                    || !self.pairing_ipc_owner()
                {
                    return Err(PairError::Expired);
                }
                captured
                    .publish_once(signed)
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                let accepted = captured
                    .read_accepted(signed)
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                self.adoption_test_point(
                    super::super::desktop::AdoptionTestPoint::AcceptedBeforeJournal,
                );
                self.store
                    .transact_profile(|profile, _| {
                        let mut profile = profile.ok_or("retained approver missing")?;
                        let journal = profile
                            .outgoing
                            .as_mut()
                            .filter(|journal| journal.intent.operation == op)
                            .ok_or("outgoing proposal changed")?;
                        journal.accepted = accepted
                            .versions()
                            .iter()
                            .map(|roster| roster.token().into())
                            .collect();
                        journal.phase = OutgoingPhase::Published;
                        let member = profile
                            .membership
                            .as_mut()
                            .ok_or("approver membership disappeared")?;
                        member.chain = member
                            .chain
                            .extended(accepted.versions())
                            .map_err(|e| e.to_string())?
                            .0;
                        member.retarget_farewells();
                        Ok((Some(profile), ()))
                    })
                    .map_err(PairError::Relay)?;
                self.reload();
                *published.lock().unwrap_or_else(|e| e.into_inner()) = Some(accepted);
                Ok(Joined {
                    ring_id: signed.ring_id().clone(),
                    relay_url: signed.roster().relay_url.clone(),
                    version: signed.version(),
                    hash: signed.hash(),
                    signed_by: signed.roster().signed_by,
                })
            },
            |handle| {
                if let Some(lane) = lane.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    let _ = lane.cancel(handle);
                }
            },
        );
        let op = operation.into_inner().unwrap_or_else(|e| e.into_inner());
        if let Some(chain) = published.into_inner().unwrap_or_else(|e| e.into_inner()) {
            // Even a lost guest ACK after this point does not undo known Relay publication.
            return Ok(ApprovalOutcome {
                operation: op.ok_or("published operation missing")?,
                chain,
            });
        }
        if let Some(op) = &op {
            let phase = self
                .store
                .load_profile()?
                .0
                .and_then(|profile| profile.outgoing)
                .map(|journal| journal.phase);
            if phase == Some(OutgoingPhase::Prepared) {
                self.outgoing_update(op, |journal| journal.phase = OutgoingPhase::Cancelled)?;
            }
            if phase == Some(OutgoingPhase::PublicationUncertain) {
                return Err(format!(
                    "reconciliation_required: {}",
                    result
                        .err()
                        .map_or_else(|| "publication unresolved".into(), |e| e.to_string())
                ));
            }
        }
        Err(result.err().map_or_else(
            || "approval ended without publication".into(),
            |e| e.to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Observer;
    impl crate::ring::RingObserver for Observer {
        fn changed(&self, _: &crate::ring::RingView) {}
    }
    #[test]
    fn canonical_join_signers_refuse_arbitrary_domains_cross_operation_and_unconfirmed_consent() {
        // Synthetic statement-unit fixture only. Real final Noise context is exercised by
        // the public workerd controllers, never claimed from this fixed hash value.
        let dir = tempfile::tempdir().unwrap();
        let ring = DesktopRing::open(
            crate::ring::DesktopRingConfig::new(dir.path().join("desktop"), "computer".into()),
            Arc::new(Observer),
        );
        assert!(ring.pairing_ipc_owner());
        let keys = ring.store.retained_keys().unwrap();
        let daemon = DeviceKeys::generate().unwrap();
        let identity = |keys: &DeviceKeys| PublicIdentity {
            name: "computer".into(),
            sign_key: keys.sign_key(),
            noise_key: keys.noise_key(),
        };
        let intent = AdoptionIntent {
            operation: nonce().unwrap(),
            expires_at: now() + 60,
            target: xshell_protocol::ring::pairing::adoption::JoinTarget::Computer {
                desktop: identity(&keys),
                daemon: identity(&daemon),
            },
        };
        let state = Mutex::new(SigningState::default());
        let principal = LocalPrincipal {
            ring: &ring,
            keys: keys.clone(),
            intent: &intent,
            relay: "ws://127.0.0.1:9",
            until: Instant::now() + Duration::from_secs(60),
            cancel: Arc::new(AtomicBool::new(false)),
            state: &state,
        };
        crate::ring::store::ensure_private_dir(dir.path()).unwrap();
        let client = CompanionClient::for_store(dir.path()).unwrap();
        let counterpart = LocalCounterpart {
            principal: &principal,
            client: &client,
            key: daemon.sign_key(),
        };
        for message in [
            b"arbitrary bytes".as_slice(),
            b"xshell-computer-consent-v2\nunconfirmed".as_slice(),
        ] {
            assert!(principal.sign(message).is_err());
            assert!(counterpart
                .sign(message)
                .unwrap_err()
                .to_string()
                .contains("unconfirmed"));
        }
        let mut other = intent.clone();
        other.operation = nonce().unwrap();
        let wrong = possession_message(&other, &[7; 32]);
        assert!(principal.sign(&wrong).is_err());
        assert!(counterpart.sign(&wrong).is_err());
        let message = possession_message(&intent, &[7; 32]);
        let proof = principal.sign(&message).unwrap();
        assert!(xshell_protocol::ring::verify(
            &keys.sign_key(),
            &message,
            &Signature::from_bytes(proof)
        ));
        assert!(principal.sign(&message).is_err());
        state.lock().unwrap().daemon_possession = true; // unit-only read gate setup
        let handle = ProposalHandle {
            ring_id: RingId::derive(&keys.sign_key()),
            relay_url: principal.relay.into(),
            operation: intent.operation.clone(),
            head: nonce().unwrap(),
            version: 1,
            guest: keys.sign_key(),
            context: nonce().unwrap(),
            expires_at: intent.expires_at,
            grant: nonce().unwrap(),
            resource: nonce().unwrap(),
        };
        let challenge = nonce().unwrap();
        let origin = xshell_protocol::ring::url::RelayUrl::parse(principal.relay)
            .unwrap()
            .origin();
        assert!(principal
            .sign(&handle.read_message("http://wrong-origin", &challenge))
            .is_err());
        principal
            .sign(&handle.read_message(&origin, &challenge))
            .unwrap();
        assert!(principal
            .sign(&handle.read_message(&origin, &nonce().unwrap()))
            .is_err());
        assert!(state.lock().unwrap().desktop_proof.is_none());
        ring.quit();
        assert!(principal.sign(b"arbitrary bytes").is_err());
    }
}

//! Pairing over the Relay's pairing pipe (blocking): [`PairPipe`] is one socket on
//! `/v1/pair/{slot}` (section 16 of the protocol), [`pair_as_guest`] runs a joining device's
//! whole side (handshake, then [`RingClient::fetch_chain`]), and [`host_pairing`] a
//! Desktop's. The pipe is not a security boundary: the pre-shared key, the Desktop's
//! single-use and expiry bookkeeping ([`PairingHost`]) and the pinned chain are.

use super::super::chain::RosterChain;
use super::super::pairing::{
    valid_slot, Guest, Host, JoinRequest, Joined, PairError, PairRefusal, PairSecret,
    MAX_PAIR_MESSAGE,
};
use super::super::url::RelayUrl;
use super::super::{b64, DeviceKeys, NoiseKey, RingError, RingId, Role, SignKey, Signer};
use super::client::{ChainPin, RingClient, RingTimeouts};
use super::transport::{self, Conn};
use super::wire::{
    decode_pair_relay, ErrorCode, PairClientFrame, PairRelayFrame, MAX_PAIR_FRAME, PING,
};
use rustls::ClientConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

#[derive(Clone)]
pub struct PairOptions {
    /// TLS for `wss://`; `None`: the Mozilla roots.
    pub tls: Option<Arc<ClientConfig>>,
    /// Dialing and each handshake step once both sides are on the pipe. A guest waits this
    /// long for the Desktop's answer to its join, which the Desktop sends only once the
    /// Relay holds the new Roster version: the Desktop's wait for that
    /// (`DesktopRingConfig::pair_publish_wait`, 10 s) must stay below the guest's step.
    pub step: Duration,
    /// How often a waiting socket pings the Relay.
    pub ping_interval: Duration,
    /// The Ring client's timeouts, for fetching the chain afterwards.
    pub ring: RingTimeouts,
}

impl Default for PairOptions {
    fn default() -> Self {
        PairOptions {
            tls: None,
            step: Duration::from_secs(15),
            ping_interval: Duration::from_secs(30),
            ring: RingTimeouts::default(),
        }
    }
}

/// Polled while blocking, so a pairing can be cancelled from another thread.
pub type Cancel = Arc<AtomicBool>;

/// How often a blocked read looks at the cancel flag.
const POLL: Duration = Duration::from_millis(100);

fn relay_err(e: RingError) -> PairError {
    PairError::Relay(e.to_string())
}

/// One socket on a pairing slot.
pub struct PairPipe {
    ws: WebSocket<Conn>,
    peer: bool,
    opts: PairOptions,
    next_ping: Instant,
    operation_until: Option<Instant>,
}

impl PairPipe {
    /// Opens `slot` on `relay_url` and reads the Relay's first word: `pair.wait` (this socket
    /// is first) or `pair.peer` (the other side is here).
    pub fn open(relay_url: &str, slot: &str, opts: &PairOptions) -> Result<PairPipe, PairError> {
        if !valid_slot(slot) {
            return Err(PairError::Invalid("bad slot".into()));
        }
        let url = RelayUrl::parse(relay_url).map_err(|e| PairError::Invalid(e.to_string()))?;
        let deadline = Instant::now() + opts.step;
        let ws = transport::dial(
            &url,
            &url.pair_endpoint(slot),
            opts.tls.clone(),
            deadline,
            MAX_PAIR_FRAME * 4,
        )
        .map_err(|e| match e {
            RingError::Timeout => PairError::Timeout,
            e => relay_err(e),
        })?;
        let mut p = PairPipe {
            ws,
            peer: false,
            opts: opts.clone(),
            next_ping: Instant::now() + opts.ping_interval,
            operation_until: None,
        };
        match p.frame(deadline, None)? {
            PairRelayFrame::Wait { .. } => {}
            PairRelayFrame::Peer => p.peer = true,
            other => return Err(PairError::Relay(format!("unexpected {other:?}"))),
        }
        Ok(p)
    }

    /// Cap all later I/O by the original offer deadline; subsequent calls cannot extend it.
    pub fn limit_until(&mut self, until: Instant) {
        self.operation_until = Some(self.operation_until.map_or(until, |d| d.min(until)));
    }

    /// Whether the other side is on the pipe.
    pub fn has_peer(&self) -> bool {
        self.peer
    }

    /// Waits for the other side until `until` ([`PairError::Expired`] after).
    pub fn wait_peer(&mut self, until: Instant, cancel: Option<&Cancel>) -> Result<(), PairError> {
        while !self.peer {
            match self.frame(until, cancel) {
                Ok(PairRelayFrame::Peer) => self.peer = true,
                Ok(other) => return Err(PairError::Relay(format!("unexpected {other:?}"))),
                Err(PairError::Timeout) => return Err(PairError::Expired),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn send(&mut self, msg: &[u8]) -> Result<(), PairError> {
        if msg.len() > MAX_PAIR_MESSAGE {
            return Err(PairError::Payload("message too long".into()));
        }
        let text = PairClientFrame::Msg {
            payload: b64::encode(msg),
        }
        .encode();
        self.write(text)
    }

    /// The other side's next message, within one step.
    pub fn recv(&mut self, cancel: Option<&Cancel>) -> Result<Vec<u8>, PairError> {
        self.recv_until(Instant::now() + self.opts.step, cancel)
    }

    /// Human consent waits use the original absolute offer deadline. Only transport pings
    /// are sent while waiting; no encrypted wait frames consume the eight-message pipe.
    pub fn recv_until(
        &mut self,
        until: Instant,
        cancel: Option<&Cancel>,
    ) -> Result<Vec<u8>, PairError> {
        match self.frame(until, cancel)? {
            PairRelayFrame::Msg { payload } => {
                match b64::decoded_len(payload.len()) {
                    Some(n) if n <= MAX_PAIR_MESSAGE => {}
                    _ => return Err(PairError::Payload("message too long".into())),
                }
                b64::decode(&payload).map_err(|_| PairError::Payload("encoding".into()))
            }
            other => Err(PairError::Relay(format!("unexpected {other:?}"))),
        }
    }

    pub fn close(mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }

    fn write(&mut self, text: String) -> Result<(), PairError> {
        let until = Instant::now() + self.opts.step;
        let until = self.operation_until.map_or(until, |d| d.min(until));
        let _ = self.ws.get_mut().set_deadline(until);
        let mut r = self.ws.send(Message::text(text));
        loop {
            match r {
                Ok(()) => return Ok(()),
                Err(tungstenite::Error::Io(e)) if transport::is_would_block(&e) => {
                    if Instant::now() >= until {
                        return Err(
                            if self.operation_until.is_some_and(|d| Instant::now() >= d) {
                                PairError::Expired
                            } else {
                                PairError::Timeout
                            },
                        );
                    }
                    r = self.ws.flush();
                }
                Err(e) => return Err(relay_err(transport::map_ws_error(e))),
            }
        }
    }

    /// The next frame that is not `pong` or unknown, pinging while it waits. A Relay `error`
    /// ends the pipe.
    fn frame(
        &mut self,
        until: Instant,
        cancel: Option<&Cancel>,
    ) -> Result<PairRelayFrame, PairError> {
        let until = self.operation_until.map_or(until, |d| d.min(until));
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                return Err(PairError::Cancelled);
            }
            let now = Instant::now();
            if now >= until {
                return Err(PairError::Timeout);
            }
            if now >= self.next_ping {
                self.next_ping = now + self.opts.ping_interval;
                self.write(PING.to_string())?;
            }
            let wake = until.min(self.next_ping).min(now + POLL);
            let _ = self.ws.get_mut().set_deadline(wake);
            match self.ws.read() {
                Ok(Message::Text(t)) => match decode_pair_relay(t.as_str()) {
                    Ok(PairRelayFrame::Pong) | Ok(PairRelayFrame::Unknown { .. }) => continue,
                    Ok(PairRelayFrame::Error { code, .. }) => return Err(pipe_error(&code)),
                    Ok(f) => return Ok(f),
                    Err(e) => return Err(PairError::Relay(e.to_string())),
                },
                Ok(Message::Close(_)) => return Err(PairError::Relay("the pipe closed".into())),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e)) if transport::is_would_block(&e) => continue,
                Err(e) => return Err(relay_err(transport::map_ws_error(e))),
            }
        }
    }
}

fn pipe_error(code: &ErrorCode) -> PairError {
    match code {
        ErrorCode::PairExpired => PairError::Expired,
        // Someone else is on the slot, or it was used.
        ErrorCode::PairBusy => PairError::Refused(PairRefusal::Used),
        c => PairError::Relay(format!("relay refused: {c}")),
    }
}

/// What a joining device asks for.
pub struct GuestRequest<'a> {
    pub relay_url: &'a str,
    pub secret: &'a PairSecret,
    /// The Desktop's Noise key from a QR payload; `None` for a code.
    pub pin: Option<NoiseKey>,
    /// The Ring a QR payload names; the Desktop's answers must name it too.
    pub ring_id: Option<&'a RingId>,
    pub keys: Arc<DeviceKeys>,
    pub role: Role,
    pub name: &'a str,
    /// How long to wait for the other side when this socket is first: a QR's Desktop is
    /// already there (`ZERO` fails at once with [`PairError::NotFound`]); `xshelld pair`
    /// waits for the code to be typed.
    pub wait: Duration,
}

/// A joining device's side, from the slot to the verified chain that lists it.
pub fn pair_as_guest(
    req: &GuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
) -> Result<RosterChain, PairError> {
    pair_as_guest_pinned(req, opts, cancel, None)
}

/// QR/enrollment callers pin the approving signer in addition to the Noise key. Scope
/// checks precede any authentication or chain fetch at a message-4 Relay address.
pub fn pair_as_guest_pinned(
    req: &GuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    approver: Option<SignKey>,
) -> Result<RosterChain, PairError> {
    pair_as_guest_pinned_with_joined(req, opts, cancel, approver).map(|(chain, _)| chain)
}

/// Retains the authenticated message-4 head for enrollment evidence. A later fetched
/// chain head is Mobile persistence evidence, not the approving Desktop's commit head.
pub fn pair_as_guest_pinned_with_joined(
    req: &GuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    approver: Option<SignKey>,
) -> Result<(RosterChain, Joined), PairError> {
    let expected_relay = RelayUrl::parse(req.relay_url).map_err(|_| PairError::Pin)?;
    let mut pipe = PairPipe::open(req.relay_url, &req.secret.slot(), opts)?;
    if !pipe.has_peer() {
        if req.wait.is_zero() {
            pipe.close();
            return Err(PairError::NotFound);
        }
        pipe.wait_peer(Instant::now() + req.wait, cancel)?;
    }
    let (mut g, m1) = Guest::start(&req.keys, req.secret, req.pin)?;
    pipe.send(&m1)?;
    let hello = g.read_hello(&pipe.recv(cancel)?)?;
    if req.ring_id.is_some_and(|r| r != &hello.ring_id) {
        return Err(PairError::Payload("another ring".into()));
    }
    let m3 = g.join(&req.keys, req.role, req.name)?;
    pipe.send(&m3)?;
    let joined = g.finish(&pipe.recv(cancel)?)?;
    pipe.close();
    if joined.ring_id != hello.ring_id
        || req.ring_id.is_some_and(|ring| ring != &joined.ring_id)
        || approver.is_some_and(|key| key != joined.signed_by)
        || RelayUrl::parse(&joined.relay_url).map_err(|_| PairError::Pin)? != expected_relay
    {
        return Err(PairError::Pin);
    }
    let signer: Arc<dyn Signer> = req.keys.clone();
    let chain = RingClient::fetch_chain(
        req.relay_url,
        &joined.ring_id,
        signer,
        &ChainPin {
            version: joined.version,
            hash: joined.hash.clone(),
        },
        opts.tls.clone(),
        opts.ring,
    )
    .map_err(relay_err)?;
    // Fetch verified signatures and the exact message-4 head first; now bind that
    // head's signer/member relationship to the authenticated Desktop and recipient.
    let joined_head = chain.get(joined.version).ok_or(PairError::Pin)?;
    let head = chain.head();
    if joined_head.roster().signed_by != joined.signed_by
        || !joined_head
            .member(&joined.signed_by)
            .is_some_and(|m| m.role == Role::Desktop && m.noise_key == hello.noise_key)
        || !head
            .member(&joined.signed_by)
            .is_some_and(|m| m.role == Role::Desktop && m.noise_key == hello.noise_key)
        || !head
            .member(&req.keys.sign_key())
            .is_some_and(|m| m.role == req.role && m.noise_key == req.keys.noise_key())
        || RelayUrl::parse(&head.roster().relay_url).map_err(|_| PairError::Pin)? != expected_relay
    {
        return Err(PairError::Pin);
    }
    Ok((chain, joined))
}

/// The Desktop's bookkeeping, called by [`host_pairing`] once message 3 arrived.
pub trait PairingHost: Send + Sync {
    /// Uses the secret up: `Err(Used)` the second time, `Err(Expired)` after its time.
    /// Called first, before anything in the message is looked at.
    fn consume(&self) -> Result<(), PairRefusal>;
    /// Adds the device: a new Roster version, committed and published (acknowledged by the
    /// Relay) before this returns.
    fn add(&self, req: &JoinRequest) -> Result<Joined, PairRefusal>;
}

/// What the Desktop's side of a pairing is about.
pub struct HostRequest<'a> {
    pub keys: &'a DeviceKeys,
    pub secret: &'a PairSecret,
    pub ring_id: &'a RingId,
    /// This Desktop's name, in message 2.
    pub name: &'a str,
    /// The role this flow adds: `mobile` for a QR, `daemon` for a code.
    pub role: Role,
}

/// The Desktop's side over `pipe`, which already has the peer (see
/// [`PairPipe::wait_peer`]). Returns what was added, or why not.
pub fn host_pairing(
    mut pipe: PairPipe,
    req: &HostRequest<'_>,
    host: &dyn PairingHost,
    cancel: Option<&Cancel>,
) -> Result<JoinRequest, PairError> {
    if !pipe.has_peer() {
        return Err(PairError::NotFound);
    }
    let mut h = Host::start(req.keys, req.secret)?;
    h.read_start(&pipe.recv(cancel)?)?;
    let m2 = h.hello(req.ring_id, req.name)?;
    pipe.send(&m2)?;
    let m3 = pipe.recv(cancel)?;
    // The secret is spent by any third message, before it is even decrypted: a replay, or a
    // second try, finds it gone.
    let consumed = host.consume();
    let join = h.read_join(&m3)?;
    let result = consumed
        .and_then(|()| {
            if join.role == req.role {
                Ok(())
            } else {
                Err(PairRefusal::Role)
            }
        })
        .and_then(|()| host.add(&join));
    let m4 = h.answer(&result)?;
    let sent = pipe.send(&m4);
    pipe.close();
    sent?;
    match result {
        Ok(_) => Ok(join),
        Err(r) => Err(PairError::Refused(r)),
    }
}

use super::super::membership::{
    consent_message, consent_proposal_hash, pairing_consent_context, ConsentProof,
};
use super::super::pairing::adoption::{
    apply_consent, AdoptionConsent, AdoptionDecision, AdoptionIntent, AdoptionSigners,
};
use super::super::SignedRoster;
use super::{ProposalReader, VerifiedProposal};

pub struct AdoptionGuestRequest<'a> {
    pub relay_url: &'a str,
    pub secret: &'a PairSecret,
    pub keys: Arc<DeviceKeys>,
    pub signers: AdoptionSigners<'a>,
    pub intent: &'a AdoptionIntent,
    /// Fixed when the code/operation was created; never reset by a network step.
    pub until: Instant,
    pub approver: Option<SignKey>,
}
/// Actual local-consent boundary for the next controller/IPC vertical. The callback must
/// present this verified Ring/approver/target and wait cancellably for explicit consent.
/// Neither code entry nor counterpart possession calls it on the user's behalf.
pub struct AdoptionConsentRequest<'a> {
    pub verified: &'a VerifiedProposal,
    pub intent: &'a AdoptionIntent,
    pub until: Instant,
    pub cancel: Option<&'a Cancel>,
}
fn live(until: Instant, cancel: Option<&Cancel>) -> Result<(), PairError> {
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        return Err(PairError::Cancelled);
    }
    if Instant::now() >= until {
        return Err(PairError::Expired);
    }
    Ok(())
}
fn offer_deadline(expires_at: u64, until: Instant) -> Result<Instant, PairError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| PairError::Expired)?;
    let remaining = Duration::from_secs(expires_at)
        .checked_sub(now)
        .filter(|d| !d.is_zero() && *d <= Duration::from_secs(600))
        .ok_or(PairError::Expired)?;
    Ok(until.min(Instant::now() + remaining))
}
fn check_proposal(
    verified: &VerifiedProposal,
    intent: &AdoptionIntent,
    hash: &[u8; 32],
    approver: Option<SignKey>,
) -> Result<(), PairError> {
    let h = &verified.handle;
    if !h.validate()
        || h.operation != intent.operation
        || h.context != b64::encode(hash)
        || h.expires_at != intent.expires_at
        || h.guest != intent.target.principal().sign_key
        || approver.is_some_and(|key| key != verified.proposal.signed_by)
        || h.head != verified.chain.head().hash()
        || h.version != verified.chain.head().version()
        || h.ring_id != *verified.chain.head().ring_id()
        || h.relay_url != verified.chain.head().roster().relay_url
    {
        return Err(PairError::Pin);
    }
    super::super::membership::verify_enroll_proposal(verified.chain.head(), &verified.proposal)
        .map_err(|e| PairError::Payload(e.to_string()))?;
    intent.target.matches_proposal(&verified.proposal)
}

/// Real exchange: message 1, message 3, encrypted consent (three guest sends), then verify
/// the accepted signed head. Membership installation belongs to the later controller.
pub fn adopt_as_guest(
    req: &AdoptionGuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    confirm: impl FnOnce(AdoptionConsentRequest<'_>) -> Result<bool, PairError>,
) -> Result<(RosterChain, Joined), PairError> {
    let until = offer_deadline(req.intent.expires_at, req.until)?;
    live(until, cancel)?;
    let relay = RelayUrl::parse(req.relay_url).map_err(|_| PairError::Pin)?;
    let mut bounded_opts = opts.clone();
    bounded_opts.step = bounded_opts
        .step
        .min(until.saturating_duration_since(Instant::now()));
    let mut pipe = PairPipe::open(req.relay_url, &req.secret.slot(), &bounded_opts)?;
    pipe.limit_until(until);
    pipe.wait_peer(until, cancel)?;
    let (mut guest, m1) = Guest::start(&req.keys, req.secret, None)?;
    pipe.send(&m1)?;
    let hello = guest.read_hello(&pipe.recv(cancel)?)?;
    pipe.send(&guest.join_adoption(&req.keys, req.intent, &req.signers)?)?;
    let hash = guest.final_handshake_hash()?;
    let message = guest.decrypt(&pipe.recv(cancel)?)?;
    let handle: super::proposal::ProposalHandle = serde_json::from_slice(&message)
        .map_err(|_| PairError::Payload("proposal handle".into()))?;
    if handle.operation != req.intent.operation
        || handle.context != b64::encode(&hash)
        || handle.guest != req.keys.sign_key()
        || handle.expires_at != req.intent.expires_at
        || handle.ring_id != hello.ring_id
        || RelayUrl::parse(&handle.relay_url).map_err(|_| PairError::Pin)? != relay
    {
        return Err(PairError::Pin);
    }
    live(until, cancel)?;
    let reader = ProposalReader::connect(
        &handle,
        req.signers.principal,
        opts.tls.clone(),
        opts.step
            .min(until.saturating_duration_since(Instant::now())),
    )
    .map_err(relay_err)?;
    let verified = reader
        .with_cancel(cancel.cloned())
        .fetch_until(until)
        .map_err(relay_err)?;
    check_proposal(&verified, req.intent, &hash, req.approver)?;
    // The authenticated final Noise responder must be the approving Desktop in the chain.
    if verified
        .chain
        .head()
        .member(&verified.proposal.signed_by)
        .map(|m| m.noise_key)
        != Some(hello.noise_key)
    {
        return Err(PairError::Pin);
    }
    live(until, cancel)?;
    let approved = confirm(AdoptionConsentRequest {
        verified: &verified,
        intent: req.intent,
        until,
        cancel,
    })?;
    live(until, cancel)?;
    if !approved {
        let decision = AdoptionDecision::Cancel {
            operation: handle.operation.clone(),
            proposal_hash: consent_proposal_hash(&verified.proposal)
                .map_err(|e| PairError::Payload(e.to_string()))?,
        };
        pipe.send(&guest.encrypt(&serde_json::to_vec(&decision).expect("cancel serializes"))?)?;
        pipe.close();
        return Err(PairError::Cancelled);
    }
    live(until, cancel)?;
    let context = pairing_consent_context(&verified.proposal, &hash)
        .map_err(|e| PairError::Payload(e.to_string()))?;
    let message = consent_message(&verified.proposal, handle.expires_at, &context)
        .map_err(|e| PairError::Payload(e.to_string()))?;
    let proofs = req
        .signers
        .sign(&req.intent.target, &message)?
        .into_iter()
        .map(|(key, signature)| ConsentProof {
            key,
            signature,
            expires_at: handle.expires_at,
            context: context.clone(),
        })
        .collect();
    let consent = AdoptionConsent {
        operation: handle.operation.clone(),
        proposal_hash: consent_proposal_hash(&verified.proposal)
            .map_err(|e| PairError::Payload(e.to_string()))?,
        proofs,
    };
    let completed = apply_consent(&verified.proposal, req.intent, &hash, consent.clone())?;
    pipe.send(&guest.encrypt(
        &serde_json::to_vec(&AdoptionDecision::Consent { consent }).expect("consent serializes"),
    )?)?;
    let joined = guest.finish(&pipe.recv(cancel)?)?;
    pipe.close();
    if joined.ring_id != handle.ring_id
        || joined.relay_url != handle.relay_url
        || joined.signed_by != verified.proposal.signed_by
        || joined.version != verified.proposal.version
    {
        return Err(PairError::Pin);
    }
    // Authenticate only AFTER commit acknowledgment. The grant has no membership authority.
    let signer = req.keys.clone();
    let chain = RingClient::fetch_chain(
        &handle.relay_url,
        &handle.ring_id,
        signer,
        &ChainPin {
            version: joined.version,
            hash: joined.hash.clone(),
        },
        opts.tls.clone(),
        opts.ring,
    )
    .map_err(relay_err)?;
    if chain
        .versions()
        .iter()
        .find(|r| r.version() == joined.version)
        .map(|r| r.roster())
        != Some(&completed)
    {
        return Err(PairError::Pin);
    }
    Ok((chain, joined))
}

pub struct AdoptionHostRequest<'a> {
    pub keys: &'a DeviceKeys,
    pub secret: &'a PairSecret,
    pub ring_id: &'a RingId,
    pub until: Instant,
}

/// Approver's three sends: message 2, encrypted frozen handle, commit acknowledgment.
/// `prepare` may only freeze metadata. `publish` sees the exact, genuinely consented token.
pub fn approve_adoption(
    mut pipe: PairPipe,
    req: &AdoptionHostRequest<'_>,
    cancel: Option<&Cancel>,
    prepare: impl FnOnce(&AdoptionIntent, [u8; 32]) -> Result<VerifiedProposal, PairError>,
    publish: impl FnOnce(&SignedRoster) -> Result<Joined, PairError>,
    cancel_read: impl FnOnce(&super::proposal::ProposalHandle),
) -> Result<(), PairError> {
    let AdoptionHostRequest {
        keys,
        secret,
        ring_id: ring,
        until,
    } = *req;
    pipe.limit_until(until);
    pipe.wait_peer(until, cancel)?;
    live(until, cancel)?;
    let mut host = Host::start(keys, secret)?;
    host.read_start(&pipe.recv(cancel)?)?;
    pipe.send(&host.hello(ring, "computer")?)?;
    let intent = host.read_adoption(&pipe.recv(cancel)?)?;
    let until = offer_deadline(intent.expires_at, until)?;
    pipe.limit_until(until);
    let hash = host.final_handshake_hash()?;
    let verified = prepare(&intent, hash)?;
    let grant = verified.handle.clone();
    let result = (|| {
        check_proposal(&verified, &intent, &hash, Some(keys.sign_key()))?;
        live(until, cancel)?;
        pipe.send(
            &host.encrypt(&serde_json::to_vec(&verified.handle).expect("handle serializes"))?,
        )?;
        let bytes = host.decrypt(&pipe.recv_until(until, cancel).map_err(|e| {
            if e == PairError::Timeout {
                PairError::Expired
            } else {
                e
            }
        })?)?;
        let decision: AdoptionDecision = serde_json::from_slice(&bytes)
            .map_err(|_| PairError::Payload("consent payload".into()))?;
        let consent = match decision {
            AdoptionDecision::Consent { consent } => consent,
            AdoptionDecision::Cancel {
                operation,
                proposal_hash,
            } if operation == intent.operation
                && proposal_hash
                    == consent_proposal_hash(&verified.proposal)
                        .map_err(|e| PairError::Payload(e.to_string()))? =>
            {
                return Err(PairError::Cancelled)
            }
            _ => return Err(PairError::Crypto),
        };
        live(until, cancel)?;
        let completed = apply_consent(&verified.proposal, &intent, &hash, consent)?;
        let signed = completed
            .sign(keys)
            .map_err(|e| PairError::Payload(e.to_string()))?;
        let mut chain = verified.chain;
        chain
            .accept(std::slice::from_ref(&signed))
            .map_err(|e| PairError::Payload(e.to_string()))?;
        live(until, cancel)?;
        let joined = publish(&signed)?;
        if joined.hash != signed.hash()
            || joined.version != signed.version()
            || joined.signed_by != keys.sign_key()
            || joined.ring_id != *ring
        {
            return Err(PairError::Pin);
        }
        pipe.send(&host.answer(&Ok(joined))?)?;
        pipe.close();
        Ok(())
    })();
    cancel_read(&grant);
    result
}

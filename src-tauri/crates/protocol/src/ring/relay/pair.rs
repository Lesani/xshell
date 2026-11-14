//! Pairing over the Relay's pairing pipe (blocking): [`PairPipe`] is one socket on
//! `/v1/pair/{slot}` (section 16 of the protocol), [`pair_as_guest`] runs a joining device's
//! whole side (handshake, then [`RingClient::fetch_chain`]), and [`host_pairing`] a
//! Desktop's. The pipe is not a security boundary: the pre-shared key, the Desktop's
//! single-use and expiry bookkeeping ([`PairingHost`]) and the pinned chain are.

use super::super::chain::RosterChain;
use super::super::pairing::{
    is_answer, is_enrolled_answer, parse_answer, valid_slot, Guest, Host, JoinRequest, Joined,
    PairError, PairRefusal, PairSecret, PairingOffer, MAX_PAIR_MESSAGE,
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

/// The bound on a guest's authenticated decline after its consent callback.
const DECLINE_SEND: Duration = Duration::from_secs(2);
/// How long a v2 phone looks for the approver's authenticated typed refusal after a step
/// failed (a revoked proposal read, a closed pipe): a refusal already sent wins.
const REFUSAL_GRACE: Duration = Duration::from_secs(2);

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

    /// [`Self::send`] with at most `max` for this one write (still within the offer deadline).
    pub fn send_within(&mut self, msg: &[u8], max: Duration) -> Result<(), PairError> {
        let step = self.opts.step;
        self.opts.step = step.min(max);
        let r = self.send(msg);
        self.opts.step = step;
        r
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

/// Calls `ready` only after the Relay has registered this guest's slot. A caller that
/// advertises its code earlier can make a fast approver arrive first and reject it.
pub fn pair_as_guest_with_ready(
    req: &GuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    ready: impl FnOnce(),
) -> Result<RosterChain, PairError> {
    pair_as_guest_pinned_with_joined_ready(req, opts, cancel, None, ready).map(|(chain, _)| chain)
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
    pair_as_guest_pinned_with_joined_ready(req, opts, cancel, approver, || {})
}

fn pair_as_guest_pinned_with_joined_ready(
    req: &GuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    approver: Option<SignKey>,
    ready: impl FnOnce(),
) -> Result<(RosterChain, Joined), PairError> {
    let expected_relay = RelayUrl::parse(req.relay_url).map_err(|_| PairError::Pin)?;
    let mut pipe = PairPipe::open(req.relay_url, &req.secret.slot(), opts)?;
    ready();
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
/// Points in a guest's adoption exchange where a test can stop it (`test-relay` only).
#[cfg(feature = "test-relay")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestTestPoint {
    /// The proposal handle is received and checked, before the proposal reader connects.
    HandleBeforeReader,
}
/// A test's callback at each [`GuestTestPoint`].
#[cfg(feature = "test-relay")]
pub type GuestTestHook = Box<dyn Fn(GuestTestPoint)>;
#[cfg(feature = "test-relay")]
thread_local! {
    static GUEST_TEST_HOOK: std::cell::RefCell<Option<GuestTestHook>> =
        const { std::cell::RefCell::new(None) };
}
/// Runs `hook` at every [`GuestTestPoint`] of guest exchanges on this thread (`test-relay`).
#[cfg(feature = "test-relay")]
pub fn set_guest_test_hook(hook: Option<GuestTestHook>) {
    GUEST_TEST_HOOK.with(|h| *h.borrow_mut() = hook);
}
#[cfg(feature = "test-relay")]
fn guest_test_point(point: GuestTestPoint) {
    GUEST_TEST_HOOK.with(|h| {
        if let Some(hook) = h.borrow().as_ref() {
            hook(point);
        }
    });
}
/// `e`, unless the approver's authenticated typed refusal is (or arrives within
/// [`REFUSAL_GRACE`]) on the pipe: then that refusal.
fn refusal_or(pipe: &mut PairPipe, guest: &mut Guest, e: PairError) -> PairError {
    let Ok(m) = pipe.recv_until(Instant::now() + REFUSAL_GRACE, None) else {
        return e;
    };
    match guest.decrypt(&m) {
        Ok(buf) if is_answer(&buf) => match parse_answer(&buf) {
            Err(refused @ PairError::Refused(_)) => refused,
            _ => e,
        },
        _ => e,
    }
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
    adopt_as_guest_with_ready(req, opts, cancel, || Ok(()), confirm)
}

/// Readiness is the Relay's pair.wait/pair.peer acknowledgment, not socket creation.
/// The one-shot callback can refuse output (for example a closed invoking stdout).
pub fn adopt_as_guest_with_ready(
    req: &AdoptionGuestRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    ready: impl FnOnce() -> Result<(), PairError>,
    confirm: impl FnOnce(AdoptionConsentRequest<'_>) -> Result<bool, PairError>,
) -> Result<(RosterChain, Joined), PairError> {
    match adopt_guest(req, None, opts, cancel, ready, confirm)? {
        GuestOutcome::Adopted(chain, joined) => Ok((chain, joined)),
        // Only a v2 phone offer pins an anchor that makes this completion meaningful.
        GuestOutcome::Enrolled(..) => Err(PairError::Payload("unexpected enrolled reply".into())),
    }
}

enum GuestOutcome {
    Adopted(RosterChain, Joined),
    Enrolled(RosterChain, Joined),
}

/// The QR anchors of a v2 phone offer (BINDING AMENDMENT 1): the Ring, the Relay, the
/// approving Desktop's signing and Noise keys and the accepted head the offer was issued at
/// must all equal the verified proposal's. Called before any consent is requested or signed.
pub fn check_offer_anchor(
    offer: &PairingOffer,
    verified: &VerifiedProposal,
) -> Result<(), PairError> {
    let head = verified.chain.head();
    let anchor = offer.head.as_ref().ok_or(PairError::Pin)?;
    if head.version() != anchor.version
        || head.hash() != anchor.hash
        || verified.chain.ring_id() != &offer.ring_id
        || verified.proposal.ring_id != offer.ring_id
        || head.roster().relay_url != offer.relay_url
        || verified.proposal.relay_url != offer.relay_url
        || verified.handle.relay_url != offer.relay_url
        || verified.proposal.signed_by != offer.sign_key
        || !head
            .member(&offer.sign_key)
            .is_some_and(|m| m.role == Role::Desktop && m.noise_key == offer.noise_key)
    {
        return Err(PairError::Pin);
    }
    Ok(())
}

/// An "already enrolled" completion is accepted only for the exact offer anchor, from the
/// pinned Desktop, and only if the authenticated chain lists this phone as its own Mobile
/// unit. It never carries a new version.
fn verify_enrolled(
    offer: &PairingOffer,
    hello: &super::super::pairing::HostHello,
    joined: &Joined,
    keys: &Arc<DeviceKeys>,
    opts: &PairOptions,
) -> Result<RosterChain, PairError> {
    let anchor = offer.head.as_ref().ok_or(PairError::Pin)?;
    if joined.ring_id != offer.ring_id
        || hello.ring_id != offer.ring_id
        || hello.noise_key != offer.noise_key
        || joined.relay_url != offer.relay_url
        || joined.version != anchor.version
        || joined.hash != anchor.hash
    {
        return Err(PairError::Pin);
    }
    let signer: Arc<dyn Signer> = keys.clone();
    let chain = RingClient::fetch_chain(
        &offer.relay_url,
        &offer.ring_id,
        signer,
        &ChainPin {
            version: joined.version,
            hash: joined.hash.clone(),
        },
        opts.tls.clone(),
        opts.ring,
    )
    .map_err(relay_err)?;
    let at = chain.get(joined.version).ok_or(PairError::Pin)?;
    let lists_phone = |r: &SignedRoster| {
        r.member(&keys.sign_key())
            .is_some_and(|m| m.role == Role::Mobile && m.noise_key == keys.noise_key())
            && r.roster().membership().is_ok_and(|m| {
                m.unit_for(&keys.sign_key()).is_some_and(|u| {
                    u.kind == super::super::membership::EnrollmentKind::Mobile
                        && u.members == [keys.sign_key()]
                })
            })
    };
    if at.hash() != joined.hash
        || at.roster().signed_by != joined.signed_by
        || at.roster().v != 2
        || !lists_phone(at)
        || !lists_phone(chain.head())
        || !at
            .member(&offer.sign_key)
            .is_some_and(|m| m.role == Role::Desktop && m.noise_key == offer.noise_key)
    {
        return Err(PairError::Pin);
    }
    Ok(chain)
}

fn adopt_guest(
    req: &AdoptionGuestRequest<'_>,
    offer: Option<&PairingOffer>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    ready: impl FnOnce() -> Result<(), PairError>,
    confirm: impl FnOnce(AdoptionConsentRequest<'_>) -> Result<bool, PairError>,
) -> Result<GuestOutcome, PairError> {
    let until = offer_deadline(req.intent.expires_at, req.until)?;
    live(until, cancel)?;
    let relay = RelayUrl::parse(req.relay_url).map_err(|_| PairError::Pin)?;
    let mut bounded_opts = opts.clone();
    bounded_opts.step = bounded_opts
        .step
        .min(until.saturating_duration_since(Instant::now()));
    let mut pipe = PairPipe::open(req.relay_url, &req.secret.slot(), &bounded_opts)?;
    pipe.limit_until(until);
    // A QR offer's Desktop holds its slot from before the code was shown (and, after a
    // cancellation, until the offer expires, to refuse a late phone). Nobody there: the
    // Desktop is gone; fail now (as v1's `wait: ZERO`) instead of waiting out the offer.
    if offer.is_some() && !pipe.has_peer() {
        pipe.close();
        return Err(PairError::NotFound);
    }
    live(until, cancel)?;
    ready()?;
    live(until, cancel)?;
    pipe.wait_peer(until, cancel)?;
    let (mut guest, m1) = Guest::start(&req.keys, req.secret, offer.map(|o| o.noise_key))?;
    pipe.send(&m1)?;
    let hello = guest.read_hello(&pipe.recv(cancel)?)?;
    if offer.is_some_and(|o| o.ring_id != hello.ring_id) {
        return Err(PairError::Pin);
    }
    pipe.send(&guest.join_adoption(&req.keys, req.intent, &req.signers)?)?;
    let hash = guest.final_handshake_hash()?;
    let message = guest.decrypt(&pipe.recv(cancel)?)?;
    if is_answer(&message) {
        // Instead of a proposal: a typed refusal, or (v2 phone) "already enrolled".
        pipe.close();
        let joined = parse_answer(&message)?;
        let offer = offer
            .filter(|_| is_enrolled_answer(&message))
            .ok_or_else(|| PairError::Payload("unexpected answer".into()))?;
        let chain = verify_enrolled(offer, &hello, &joined, &req.keys, opts)?;
        return Ok(GuestOutcome::Enrolled(chain, joined));
    }
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
    #[cfg(feature = "test-relay")]
    guest_test_point(GuestTestPoint::HandleBeforeReader);
    // The approver may have refused meanwhile (a Desktop cancel revokes the proposal and
    // sends a typed refusal on the pipe): that refusal is the answer, not the connect or
    // read error.
    let reader = match ProposalReader::connect(
        &handle,
        req.signers.principal,
        opts.tls.clone(),
        opts.step
            .min(until.saturating_duration_since(Instant::now())),
    )
    .map_err(relay_err)
    {
        Ok(r) => r,
        Err(e) => return Err(refusal_or(&mut pipe, &mut guest, e)),
    };
    let verified = match reader
        .with_cancel(cancel.cloned())
        .fetch_until(until)
        .map_err(relay_err)
    {
        Ok(v) => v,
        Err(e) => return Err(refusal_or(&mut pipe, &mut guest, e)),
    };
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
    if let Some(offer) = offer {
        check_offer_anchor(offer, &verified)?;
    }
    live(until, cancel)?;
    let approved = confirm(AdoptionConsentRequest {
        verified: &verified,
        intent: req.intent,
        until,
        cancel,
    })?;
    let cancelled = cancel.is_some_and(|c| c.load(Ordering::Acquire));
    if !approved || cancelled {
        // A decline (or a cancellation during consent) is told to the approver,
        // authenticated, with a short deadline of its own and regardless of the cancel flag.
        // No consent is ever signed after it.
        if Instant::now() < until {
            let decision = AdoptionDecision::Cancel {
                operation: handle.operation.clone(),
                proposal_hash: consent_proposal_hash(&verified.proposal)
                    .map_err(|e| PairError::Payload(e.to_string()))?,
            };
            let message =
                guest.encrypt(&serde_json::to_vec(&decision).expect("cancel serializes"))?;
            let _ = pipe.send_within(&message, DECLINE_SEND);
        }
        pipe.close();
        live(until, None)?;
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
    let sealed = guest.encrypt(
        &serde_json::to_vec(&AdoptionDecision::Consent { consent }).expect("consent serializes"),
    )?;
    if let Err(e) = pipe.send(&sealed) {
        return Err(refusal_or(&mut pipe, &mut guest, e));
    }
    let answer = match pipe.recv(cancel) {
        Ok(m) => m,
        Err(e) => return Err(refusal_or(&mut pipe, &mut guest, e)),
    };
    let joined = guest.finish(&answer)?;
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
    Ok(GuestOutcome::Adopted(chain, joined))
}

/// What a phone needs to join a v2 Ring from a scanned v2 [`PairingOffer`].
pub struct MobileAdoptionRequest<'a> {
    /// The parsed QR payload; must be v2 (carry a head anchor).
    pub offer: &'a PairingOffer,
    /// The phone's retained identity. A retry after a lost acknowledgment or failed local
    /// persistence MUST reuse the same keys: that is what makes "already enrolled" recovery
    /// possible.
    pub keys: Arc<DeviceKeys>,
    /// The phone's display name (cleaned up with `member_name(.., "phone")`).
    pub name: &'a str,
    /// A local cap; the offer's `expiresAt` (at most 600 s away) also bounds the exchange.
    pub until: Instant,
}

/// A completed phone enrollment into a v2 Ring.
#[derive(Debug)]
pub struct MobileEnrollment {
    /// Verified from genesis, pinned at `joined`, and listing this phone as a Mobile unit.
    pub chain: RosterChain,
    /// The accepted head that lists the phone: the new Enroll version, or (when
    /// `already_enrolled`) the offer's anchor head that already listed it.
    pub joined: Joined,
    /// `true`: this phone was already a member (an earlier attempt's acknowledgment or local
    /// persistence was lost); no new version was created. Persist `chain` as usual.
    pub already_enrolled: bool,
    /// The operation (enrollment unit ID for a new enrollment).
    pub operation: String,
}

/// The phone's intent for `offer`: a fresh operation, the offer's expiry, the phone's
/// public identity. Exposed for callers that want to show or log the operation.
pub fn mobile_intent(
    offer: &PairingOffer,
    keys: &DeviceKeys,
    name: &str,
) -> Result<AdoptionIntent, PairError> {
    use super::super::pairing::adoption::{JoinTarget, PublicIdentity};
    let intent = AdoptionIntent {
        operation: super::wire::new_nonce()
            .map_err(|e| PairError::Relay(format!("no randomness: {e}")))?,
        expires_at: offer.expires_at,
        target: JoinTarget::Mobile {
            phone: PublicIdentity {
                sign_key: keys.sign_key(),
                noise_key: keys.noise_key(),
                name: super::super::member_name(name, "phone"),
            },
        },
    };
    intent.validate()?;
    Ok(intent)
}

/// The phone's whole side of a v2 pairing (Mobile FFI entry point).
///
/// Sequence: open the offer's slot (`ready` runs once the Relay registered this socket),
/// XXpsk3 with the Desktop's Noise key pinned from the QR, message 3 carries a
/// [`JoinTarget::Mobile`](super::super::pairing::adoption::JoinTarget) intent with the phone's
/// possession proof. The Desktop then answers with one of:
/// - a proposal handle: the phone reads the frozen proposal through the Relay proposal
///   reader, verifies it structurally and against every QR anchor ([`check_offer_anchor`]),
///   then calls `confirm`. Scanning the QR is the user's consent (plan decision 3), so the
///   app's `confirm` returns `Ok(true)` after its own checks; `Ok(false)` sends an
///   authenticated decline and returns [`PairError::Cancelled`] (no proof is ever signed).
///   On approval the phone signs its Pairing consent proof, the Desktop signs and publishes,
///   and the phone fetches and verifies the exact published version;
/// - "already enrolled" (`already_enrolled = true`): verified against the offer anchor and
///   the authenticated chain, no consent asked, nothing signed;
/// - a typed refusal: [`PairError::Refused`] with `Duplicate`, `Full`, `Stale`,
///   `Cancelled`, `Expired`, `NotCommitted` (nothing committed: retry with a new offer) or
///   `PublicationUncertain` (retry with a new offer and the same keys; recovery returns
///   `already_enrolled` if it landed).
///
/// A v1 offer is refused with [`PairError::Invalid`]; use `pair_as_guest_pinned_with_joined`.
pub fn adopt_mobile_as_guest_with_ready(
    req: &MobileAdoptionRequest<'_>,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
    ready: impl FnOnce() -> Result<(), PairError>,
    confirm: impl FnOnce(AdoptionConsentRequest<'_>) -> Result<bool, PairError>,
) -> Result<MobileEnrollment, PairError> {
    if req.offer.head.is_none() {
        return Err(PairError::Invalid("not a v2 offer".into()));
    }
    let intent = mobile_intent(req.offer, &req.keys, req.name)?;
    let guest = AdoptionGuestRequest {
        relay_url: &req.offer.relay_url,
        secret: &req.offer.secret,
        keys: req.keys.clone(),
        signers: AdoptionSigners {
            principal: &*req.keys,
            companion: None,
        },
        intent: &intent,
        until: req.until,
        approver: Some(req.offer.sign_key),
    };
    let (chain, joined, already_enrolled) =
        match adopt_guest(&guest, Some(req.offer), opts, cancel, ready, confirm)? {
            GuestOutcome::Adopted(chain, joined) => (chain, joined, false),
            GuestOutcome::Enrolled(chain, joined) => (chain, joined, true),
        };
    Ok(MobileEnrollment {
        chain,
        joined,
        already_enrolled,
        operation: intent.operation,
    })
}

pub struct AdoptionHostRequest<'a> {
    pub keys: &'a DeviceKeys,
    pub secret: &'a PairSecret,
    pub ring_id: &'a RingId,
    pub until: Instant,
}

/// What an approver's `prepare` decided for a verified, possession-proven intent.
pub enum Prepared {
    /// A frozen proposal to send as the handle.
    Proposal(Box<VerifiedProposal>),
    /// v2 phone recovery: the target is already a member of this accepted head; answer
    /// "already enrolled" with it instead of creating any new version.
    Enrolled(Joined),
}

/// Approver's three sends: message 2, encrypted frozen handle, commit acknowledgment.
/// `prepare` may only freeze metadata. `publish` sees the exact, genuinely consented token.
/// A phone ([`JoinTarget::Mobile`](super::super::pairing::adoption::JoinTarget)) intent is
/// refused here: phones join through [`approve_mobile_adoption`].
pub fn approve_adoption(
    pipe: PairPipe,
    req: &AdoptionHostRequest<'_>,
    cancel: Option<&Cancel>,
    prepare: impl FnOnce(&AdoptionIntent, [u8; 32]) -> Result<VerifiedProposal, PairError>,
    publish: impl FnOnce(&SignedRoster) -> Result<Joined, PairError>,
    cancel_read: impl FnOnce(&super::proposal::ProposalHandle),
) -> Result<(), PairError> {
    approve(
        pipe,
        req,
        "computer",
        None,
        cancel,
        |intent, hash| {
            if matches!(
                intent.target,
                super::super::pairing::adoption::JoinTarget::Mobile { .. }
            ) {
                return Err(PairError::Payload(
                    "a phone joins through a pairing offer".into(),
                ));
            }
            prepare(intent, hash).map(|v| Prepared::Proposal(Box::new(v)))
        },
        publish,
        cancel_read,
    )
}

/// The Desktop's side of a v2 phone pairing over its offer's pipe (which it opened first).
pub struct MobileHostRequest<'a> {
    pub keys: &'a DeviceKeys,
    pub secret: &'a PairSecret,
    pub ring_id: &'a RingId,
    /// This Desktop's name, in message 2.
    pub name: &'a str,
    /// The offer's local deadline.
    pub until: Instant,
    /// The offer's `expiresAt`; a phone intent may not outlive it.
    pub offer_expires_at: u64,
    /// Stops the exchange outright (the app quits). A `cancel` instead is deferred until
    /// the phone's intent is read, so the phone always learns `cancelled_by_desktop`
    /// (authenticated): before that, nothing could tell it why.
    pub abort: Option<&'a Cancel>,
}

/// Like [`approve_adoption`] for a phone: only a `Mobile` intent within the offer's expiry
/// is passed to `prepare`, which may also answer [`Prepared::Enrolled`]. Failures are sent
/// to the phone as authenticated typed refusals: a `prepare`/`publish` error
/// `PairError::Refused(r)` sends `r`; a local cancellation or expiry sends `Cancelled` /
/// `Expired`. A `cancel` raised before the phone's intent arrived (no phone yet, or the
/// handshake still running) keeps the exchange going until that intent, bounded by
/// `until`, and then refuses it with `Cancelled`; only `req.abort` stops it silently.
pub fn approve_mobile_adoption(
    pipe: PairPipe,
    req: &MobileHostRequest<'_>,
    cancel: Option<&Cancel>,
    prepare: impl FnOnce(&AdoptionIntent, [u8; 32]) -> Result<Prepared, PairError>,
    publish: impl FnOnce(&SignedRoster) -> Result<Joined, PairError>,
    cancel_read: impl FnOnce(&super::proposal::ProposalHandle),
) -> Result<(), PairError> {
    let offer_expires_at = req.offer_expires_at;
    approve(
        pipe,
        &AdoptionHostRequest {
            keys: req.keys,
            secret: req.secret,
            ring_id: req.ring_id,
            until: req.until,
        },
        req.name,
        Some(req.abort),
        cancel,
        |intent, hash| {
            if !matches!(
                intent.target,
                super::super::pairing::adoption::JoinTarget::Mobile { .. }
            ) {
                return Err(PairError::Refused(PairRefusal::Role));
            }
            if intent.expires_at > offer_expires_at {
                return Err(PairError::Refused(PairRefusal::Expired));
            }
            prepare(intent, hash)
        },
        publish,
        cancel_read,
    )
}

/// The authenticated typed refusal a v2 phone receives for `e`, if any.
fn typed_refusal(e: &PairError) -> Option<PairRefusal> {
    match e {
        PairError::Refused(r) => Some(*r),
        PairError::Cancelled => Some(PairRefusal::Cancelled),
        PairError::Expired => Some(PairRefusal::Expired),
        _ => None,
    }
}

/// `typed`: a v2 phone (`Some(abort)`): failures are sent as typed refusals, and a
/// `cancel` before the intent is deferred to a `Cancelled` refusal of that intent.
#[allow(clippy::too_many_arguments)]
fn approve(
    mut pipe: PairPipe,
    req: &AdoptionHostRequest<'_>,
    name: &str,
    typed: Option<Option<&Cancel>>,
    cancel: Option<&Cancel>,
    prepare: impl FnOnce(&AdoptionIntent, [u8; 32]) -> Result<Prepared, PairError>,
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
    // Until the phone's intent is read only an abort stops a phone exchange: a cancel then
    // still owes the phone its authenticated refusal.
    let early = typed.unwrap_or(cancel);
    pipe.wait_peer(until, early)?;
    live(until, early)?;
    let mut host = Host::start(keys, secret)?;
    host.read_start(&pipe.recv(early)?)?;
    pipe.send(&host.hello(ring, name)?)?;
    let intent = host.read_adoption(&pipe.recv(early)?)?;
    let typed = typed.is_some();
    // Best effort: the phone learns why, authenticated by the finished handshake.
    let refuse = |mut pipe: PairPipe, host: &mut Host, e: &PairError| {
        if let Some(r) = typed.then(|| typed_refusal(e)).flatten() {
            if let Ok(m) = host.answer(&Err(r)) {
                let _ = pipe.send(&m);
            }
        }
        pipe.close();
    };
    let until = match offer_deadline(intent.expires_at, until) {
        Ok(until) => until,
        Err(e) => {
            refuse(pipe, &mut host, &e);
            return Err(e);
        }
    };
    pipe.limit_until(until);
    if typed && cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        let e = PairError::Cancelled;
        refuse(pipe, &mut host, &e);
        return Err(e);
    }
    let hash = host.final_handshake_hash()?;
    let verified = match prepare(&intent, hash) {
        Ok(Prepared::Proposal(verified)) => *verified,
        Ok(Prepared::Enrolled(joined)) => {
            if joined.ring_id != *ring {
                pipe.close();
                return Err(PairError::Pin);
            }
            let sent = host.answer_enrolled(&joined).and_then(|m| pipe.send(&m));
            pipe.close();
            return sent;
        }
        Err(e) => {
            refuse(pipe, &mut host, &e);
            return Err(e);
        }
    };
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
        Ok(())
    })();
    match &result {
        Ok(()) => pipe.close(),
        Err(e) => refuse(pipe, &mut host, e),
    }
    cancel_read(&grant);
    result
}

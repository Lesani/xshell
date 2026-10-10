//! The Relay contract: scenarios any Relay must pass, run here against the [`TestRelay`] and
//! in `xshell-remote` against the real Worker on workerd (ADR-0009), so the two cannot drift.
//! Each scenario panics on failure and uses fresh random keys, hence a fresh Ring: no reset
//! between scenarios is needed.
//!
//! [`entitlement_slot_round_trips`] assumes a Relay without the Push Gateway's key (it then
//! stores any well-formed token). [`HOSTED_SCENARIOS`] need a Hosted Relay and its gateway
//! key in the target, [`QUOTA_SCENARIOS`] a Relay with a small daily quota, named in the
//! target. The pairing scenarios open more slots from one machine than the default pairing
//! rate limit allows, so a runner raises it, except for [`PAIR_RATE_SCENARIOS`].
//! [`PUSH_SCENARIOS`] need a Relay that forwards pushes to the target's
//! [`FakeGateway`] (run them one at a time: they script its replies),
//! [`PUSH_UNAVAILABLE_SCENARIOS`] one without a gateway, and [`FOREGROUND_LEASE_SCENARIOS`]
//! one with a short foreground lease, named in the target.
//!
//! [`TestRelay`]: super::test_relay::TestRelay

use super::super::chain::RosterChain;
use super::super::entitlement::{sign_entitlement, Tier};
use super::super::pairing::PairSecret;
use super::super::roster::{Member, Role, Roster, RosterError, SignedRoster};
use super::super::url::RelayUrl;
use super::super::{b64, DeviceKeys, RingError, RingId, SignError, SignKey, Signature, Signer};
use super::client::{PushRequest, RingClient, RingClientConfig, RingEvents, RingTimeouts};
pub use super::fake_gateway::{FakeGateway, Reply};
use super::transport::{self, Conn};
use super::wire::{
    auth_message, close, decode_relay, ByeReason, ClientFrame, CloseReason, ErrorCode,
    MemberPresence, RelayFrame, CAP_FOREGROUND, CAP_PUSH, FOREGROUND_LEASE, MAX_ENVELOPE_PAYLOAD,
    PING, PONG, QUOTA_REFUSALS_BEFORE_CLOSE,
};
use super::wire::{
    decode_pair_relay, PairClientFrame, PairRelayFrame, MAX_PAIR_MSGS, PAIR_MAX_SLOTS_PER_PREFIX,
};
use rustls::ClientConfig;
use serde_json::json;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::{Message, WebSocket};

/// How long a scenario waits for something that should happen.
pub const WAIT: Duration = Duration::from_secs(5);
/// How long a scenario waits to be sure something does not happen.
pub const QUIET: Duration = Duration::from_millis(300);

/// A Relay under test.
#[derive(Clone)]
pub struct RelayTarget {
    /// The `relayUrl` Rosters name, e.g. `ws://127.0.0.1:8787`.
    pub url: String,
    /// TLS for a `wss://` target with a private CA.
    pub tls: Option<Arc<ClientConfig>>,
    /// The Relay's authentication deadline.
    pub auth_timeout: Duration,
    /// For a Hosted Relay: a signer holding the Push Gateway key the Relay trusts, so the
    /// Hosted scenarios can mint entitlement tokens.
    pub gateway: Option<Arc<dyn Signer>>,
    /// For a Relay with a daily frame quota (section 14): the quota per Ring. The
    /// [`QUOTA_SCENARIOS`] need a small one (at most 1000).
    pub quota_frames_per_day: Option<u64>,
    /// For a Relay with a pairing-pipe rate limit (section 16): slot opens per client
    /// address per minute. The [`PAIR_RATE_SCENARIOS`] need one (at most 50).
    pub pair_opens_per_minute: Option<u32>,
    /// For a Relay with a short pairing slot lifetime: [`PAIR_TTL_SCENARIOS`] need one of at
    /// most 5 s. `None`: the protocol's 600 s.
    pub pair_ttl: Option<Duration>,
    /// For a Relay that takes the client address from a trusted proxy header (section 16):
    /// the header's name. The pairing scenarios then send client addresses of their own in
    /// it; without one they open from the runner's address.
    pub client_ip_header: Option<String>,
    /// For a Relay that forwards pushes: the fake Push Gateway it forwards to.
    /// [`PUSH_SCENARIOS`] need one.
    pub fake_gateway: Option<Arc<FakeGateway>>,
    /// For a Relay with a short foreground lease: [`FOREGROUND_LEASE_SCENARIOS`] need one of
    /// at most 5 s. `None`: the protocol's 75 s.
    pub foreground_lease: Option<Duration>,
}

impl RelayTarget {
    pub fn origin(&self) -> String {
        RelayUrl::parse(&self.url)
            .expect("target url is a relay url")
            .origin()
    }
}

/// What a [`Recorder`] saw.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Envelope {
        from: SignKey,
        payload: Vec<u8>,
    },
    Presence {
        key: SignKey,
        presence: MemberPresence,
    },
    Roster {
        version: u64,
    },
    RosterRejected(RosterError),
    Error {
        code: ErrorCode,
        to: Option<SignKey>,
    },
    Entitlement(Option<String>),
    Closed(CloseReason),
}

/// [`RingEvents`] that records everything, for tests.
#[derive(Default)]
pub struct Recorder {
    events: Mutex<Vec<Event>>,
    cv: Condvar,
}

impl Recorder {
    pub fn new() -> Arc<Recorder> {
        Arc::new(Recorder::default())
    }

    fn push(&self, e: Event) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(e);
        self.cv.notify_all();
    }

    pub fn events(&self) -> Vec<Event> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The first event (seen or arriving within `timeout`) matching `pred`.
    pub fn wait_for(&self, timeout: Duration, pred: impl Fn(&Event) -> bool) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        let mut ev = self.events.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(e) = ev.iter().find(|e| pred(e)) {
                return Some(e.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            ev = self
                .cv
                .wait_timeout(ev, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn closed(&self, timeout: Duration) -> Option<CloseReason> {
        match self.wait_for(timeout, |e| matches!(e, Event::Closed(_))) {
            Some(Event::Closed(c)) => Some(c),
            _ => None,
        }
    }

    pub fn envelopes(&self) -> Vec<(SignKey, Vec<u8>)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Envelope { from, payload } => Some((from, payload)),
                _ => None,
            })
            .collect()
    }

    /// The latest presence reported for `key`.
    pub fn presence_of(&self, key: &SignKey) -> Option<MemberPresence> {
        self.events().into_iter().rev().find_map(|e| match e {
            Event::Presence { key: k, presence } if &k == key => Some(presence),
            _ => None,
        })
    }

    pub fn wait_presence(&self, key: &SignKey, pred: impl Fn(&MemberPresence) -> bool) -> bool {
        self.wait_for(
            WAIT,
            |e| matches!(e, Event::Presence { key: k, presence } if k == key && pred(presence)),
        )
        .is_some()
    }
}

impl RingEvents for Recorder {
    fn envelope(&self, from: SignKey, payload: Vec<u8>) {
        self.push(Event::Envelope { from, payload });
    }
    fn presence(&self, key: SignKey, presence: MemberPresence) {
        self.push(Event::Presence { key, presence });
    }
    fn roster(&self, roster: &SignedRoster) {
        self.push(Event::Roster {
            version: roster.version(),
        });
    }
    fn roster_rejected(&self, error: RosterError) {
        self.push(Event::RosterRejected(error));
    }
    fn error(&self, code: ErrorCode, to: Option<SignKey>, _detail: Option<String>) {
        self.push(Event::Error { code, to });
    }
    fn entitlement(&self, token: Option<&str>) {
        self.push(Event::Entitlement(token.map(str::to_string)));
    }
    fn closed(&self, why: CloseReason) {
        self.push(Event::Closed(why));
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A Ring for tests: v1 by `desktop`; v2 adds `desktop2`, `daemon` and `mobile`.
pub struct TestRing {
    pub desktop: Arc<DeviceKeys>,
    pub desktop2: Arc<DeviceKeys>,
    pub daemon: Arc<DeviceKeys>,
    pub mobile: Arc<DeviceKeys>,
    pub chain: RosterChain,
}

pub fn keys() -> Arc<DeviceKeys> {
    Arc::new(DeviceKeys::generate().expect("randomness"))
}

pub fn member(k: &DeviceKeys, name: &str, role: Role) -> Member {
    Member::new(name, role, k.sign_key(), k.noise_key(), now())
}

impl TestRing {
    pub fn new(relay_url: &str) -> TestRing {
        let (desktop, desktop2, daemon, mobile) = (keys(), keys(), keys(), keys());
        let v1 = SignedRoster::genesis(&*desktop, desktop.noise_key(), "desktop", relay_url, now())
            .expect("genesis");
        let v2 = v1
            .next(&*desktop, now(), |r| {
                r.add(member(&desktop2, "desktop-2", Role::Desktop));
                r.add(member(&daemon, "daemon", Role::Daemon));
                r.add(member(&mobile, "mobile", Role::Mobile));
            })
            .expect("v2");
        TestRing {
            desktop,
            desktop2,
            daemon,
            mobile,
            chain: RosterChain::from_chain(vec![v1, v2]).expect("chain"),
        }
    }

    pub fn ring_id(&self) -> RingId {
        self.chain.ring_id().clone()
    }

    /// The chain up to `version`.
    pub fn up_to(&self, version: u64) -> RosterChain {
        let v: Vec<SignedRoster> = self.chain.versions()[..version as usize].to_vec();
        RosterChain::from_chain(v).expect("prefix")
    }

    /// Appends a version signed by `desktop`.
    pub fn next(&mut self, edit: impl FnOnce(&mut super::super::RosterDraft)) -> SignedRoster {
        let n = self
            .chain
            .head()
            .next(&*self.desktop, now(), edit)
            .expect("next version");
        self.chain.accept(std::slice::from_ref(&n)).expect("accept");
        n
    }
}

pub fn config(t: &RelayTarget, chain: &RosterChain, signer: Arc<dyn Signer>) -> RingClientConfig {
    let mut cfg = RingClientConfig::new(chain.clone(), signer);
    cfg.tls = t.tls.clone();
    cfg.timeouts = RingTimeouts {
        connect: Duration::from_secs(10),
        request: WAIT,
        ..RingTimeouts::default()
    };
    cfg
}

pub fn try_connect(
    t: &RelayTarget,
    chain: &RosterChain,
    signer: Arc<dyn Signer>,
) -> Result<(RingClient, Arc<Recorder>), RingError> {
    let rec = Recorder::new();
    let c = RingClient::connect(config(t, chain, signer), rec.clone())?;
    Ok((c, rec))
}

pub fn connect(
    t: &RelayTarget,
    chain: &RosterChain,
    signer: Arc<dyn Signer>,
) -> (RingClient, Arc<Recorder>) {
    try_connect(t, chain, signer).expect("connect")
}

/// What a raw connection read.
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    Text(String),
    Binary,
    Closed(Option<u16>),
    Timeout,
}

/// A hand-driven Relay connection, for frames the Ring client would never send.
pub struct RawConn {
    ws: WebSocket<Conn>,
}

impl RawConn {
    pub fn open(t: &RelayTarget, ring: &RingId) -> Result<RawConn, RingError> {
        let url = RelayUrl::parse(&t.url).map_err(|e| RingError::Invalid(e.to_string()))?;
        let ws = transport::dial(
            &url,
            &url.ring_endpoint(ring),
            t.tls.clone(),
            Instant::now() + Duration::from_secs(10),
            4 * 1024 * 1024,
        )?;
        Ok(RawConn { ws })
    }

    fn deadline(&mut self, d: Duration) {
        let _ = self.ws.get_mut().set_deadline(Instant::now() + d);
    }

    pub fn send(&mut self, text: &str) -> Result<(), RingError> {
        self.deadline(WAIT);
        self.ws
            .send(Message::text(text))
            .map_err(transport::map_ws_error)
    }

    pub fn send_binary(&mut self, bytes: &[u8]) -> Result<(), RingError> {
        self.deadline(WAIT);
        self.ws
            .send(Message::binary(bytes.to_vec()))
            .map_err(transport::map_ws_error)
    }

    pub fn recv(&mut self, timeout: Duration) -> Raw {
        self.deadline(timeout);
        loop {
            match self.ws.read() {
                Ok(Message::Text(t)) => return Raw::Text(t.as_str().to_string()),
                Ok(Message::Binary(_)) => return Raw::Binary,
                Ok(Message::Close(f)) => {
                    let _ = self.ws.flush();
                    return Raw::Closed(f.map(|f| u16::from(f.code)));
                }
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue
                }
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Raw::Timeout
                }
                Err(_) => return Raw::Closed(None),
            }
        }
    }

    /// The next decodable frame other than `pong`, or `None` on close or timeout.
    pub fn frame(&mut self, timeout: Duration) -> Option<RelayFrame> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.recv(left) {
                Raw::Text(t) => match decode_relay(&t) {
                    Ok(RelayFrame::Pong) | Err(_) => continue,
                    Ok(f) => return Some(f),
                },
                Raw::Binary => continue,
                Raw::Closed(_) | Raw::Timeout => return None,
            }
        }
    }

    /// Opens a pairing pipe socket on `slot`.
    pub fn open_pair(t: &RelayTarget, slot: &str) -> Result<RawConn, RingError> {
        RawConn::open_pair_from(t, slot, None)
    }

    /// Opens a pairing pipe socket on `slot`, naming client address `from` in the target's
    /// [`RelayTarget::client_ip_header`] (ignored when it has none).
    pub fn open_pair_from(
        t: &RelayTarget,
        slot: &str,
        from: Option<&str>,
    ) -> Result<RawConn, RingError> {
        let url = RelayUrl::parse(&t.url).map_err(|e| RingError::Invalid(e.to_string()))?;
        let headers: Vec<(&str, &str)> = match (&t.client_ip_header, from) {
            (Some(name), Some(addr)) => vec![(name.as_str(), addr)],
            _ => Vec::new(),
        };
        let ws = transport::dial_with_headers(
            &url,
            &url.pair_endpoint(slot),
            &headers,
            t.tls.clone(),
            Instant::now() + Duration::from_secs(10),
            4 * 1024 * 1024,
        )?;
        Ok(RawConn { ws })
    }

    /// The next pairing pipe frame other than `pong`, or `None` on close or timeout.
    pub fn pair_frame(&mut self, timeout: Duration) -> Option<PairRelayFrame> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.recv(left) {
                Raw::Text(t) => match decode_pair_relay(&t) {
                    Ok(PairRelayFrame::Pong) | Err(_) => continue,
                    Ok(f) => return Some(f),
                },
                Raw::Binary => continue,
                Raw::Closed(_) | Raw::Timeout => return None,
            }
        }
    }

    pub fn pair_msg(&mut self, bytes: &[u8]) {
        self.send(
            &PairClientFrame::Msg {
                payload: b64::encode(bytes),
            }
            .encode(),
        )
        .expect("send pair.msg");
    }

    /// The next `error` frame's code (skipping other frames).
    pub fn error(&mut self, timeout: Duration) -> Option<(ErrorCode, Option<u64>, Option<String>)> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frame(left)? {
                RelayFrame::Error {
                    code, id, detail, ..
                } => return Some((code, id, detail)),
                _ => continue,
            }
        }
    }

    /// Reads until the Relay closes; its close code.
    pub fn close_code(&mut self, timeout: Duration) -> Option<u16> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.recv(left) {
                Raw::Closed(c) => return c,
                Raw::Timeout => return None,
                _ => continue,
            }
        }
    }

    /// The challenge: its nonce and the Relay's head version.
    pub fn challenge(&mut self) -> (String, u64) {
        match self.frame(WAIT) {
            Some(RelayFrame::Challenge {
                nonce,
                roster_version,
                ..
            }) => (nonce, roster_version),
            other => panic!("expected challenge, got {other:?}"),
        }
    }

    pub fn auth_chain(&mut self, rosters: &[&SignedRoster]) {
        let rosters = rosters.iter().map(|r| r.token().to_string()).collect();
        self.send(&ClientFrame::AuthChain { rosters }.encode())
            .expect("send auth.chain");
    }

    pub fn auth(&mut self, signer: &dyn Signer, origin: &str, ring: &RingId, nonce: &str) {
        let key = signer.sign_key();
        let sig = signer
            .sign(&auth_message(origin, ring, nonce, &key))
            .expect("sign");
        self.send(
            &ClientFrame::Auth {
                sign_key: key,
                sig: Signature::from_bytes(sig),
                caps: Vec::new(),
            }
            .encode(),
        )
        .expect("send auth");
    }

    /// Opens, uploads what the Relay lacks of `chain`, authenticates as `signer`, and returns
    /// the connection after `welcome`.
    pub fn login(t: &RelayTarget, chain: &RosterChain, signer: &dyn Signer) -> RawConn {
        let ring = chain.ring_id().clone();
        let mut c = RawConn::open(t, &ring).expect("open");
        let (nonce, v) = c.challenge();
        if v < chain.head().version() {
            let missing: Vec<&SignedRoster> = chain.since(v).iter().collect();
            c.auth_chain(&missing);
        }
        c.auth(signer, &t.origin(), &ring, &nonce);
        match c.frame(WAIT) {
            Some(RelayFrame::Welcome { .. }) => c,
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    pub fn env(&mut self, to: &SignKey, payload: &[u8]) {
        self.send(
            &ClientFrame::Env {
                to: *to,
                payload: b64::encode(payload),
            }
            .encode(),
        )
        .expect("send env");
    }
}

/// The Relay's head version for `ring` as its challenge reports it.
pub fn relay_version(t: &RelayTarget, ring: &RingId) -> u64 {
    RawConn::open(t, ring).expect("open").challenge().1
}

/// A signer that claims one key and signs with another.
pub struct ForgingSigner {
    pub claims: SignKey,
    pub signs_with: Arc<DeviceKeys>,
}

impl Signer for ForgingSigner {
    fn sign_key(&self) -> SignKey {
        self.claims
    }
    fn sign(&self, msg: &[u8]) -> Result<[u8; 64], SignError> {
        Signer::sign(&*self.signs_with, msg)
    }
}

/// A signer whose keystore refuses.
pub struct FailingSigner(pub SignKey);

impl Signer for FailingSigner {
    fn sign_key(&self) -> SignKey {
        self.0
    }
    fn sign(&self, _: &[u8]) -> Result<[u8; 64], SignError> {
        Err(SignError("keystore locked".into()))
    }
}

fn signer(k: &Arc<DeviceKeys>) -> Arc<dyn Signer> {
    k.clone()
}

/// A version after `prev` signed by `by` with no client-side checks.
pub fn raw_next(
    prev: &SignedRoster,
    by: &dyn Signer,
    edit: impl FnOnce(&mut Roster),
) -> SignedRoster {
    let mut r = prev.roster().clone();
    r.version += 1;
    r.prev = Some(prev.hash());
    r.signed_by = by.sign_key();
    r.issued_at += 1;
    edit(&mut r);
    r.sign(by).expect("sign raw version")
}

/// A well-formed entitlement token with a meaningless signature.
pub fn fake_entitlement(ring: &RingId) -> String {
    let claims = json!({
        "v": 1, "kid": "test", "ringId": ring.as_str(), "tier": "push",
        "purchaseRef": "test", "issuedAt": now(), "expiresAt": now() + 3600,
    });
    format!(
        "xet1.{}.{}",
        b64::encode(claims.to_string().as_bytes()),
        b64::encode(&[1u8; 64])
    )
}

// ---- Scenarios ------------------------------------------------------------------------------

pub fn member_auth_succeeds(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, _) = connect(t, &r.chain, signer(&r.desktop));
    assert_eq!(a.roster().version(), 2);
    let me = a
        .members()
        .into_iter()
        .find(|m| m.member.sign_key == r.desktop.sign_key());
    assert!(matches!(
        me.map(|m| m.presence),
        Some(MemberPresence::Online { .. })
    ));
    let (d, _) = connect(t, &r.chain, signer(&r.daemon));
    assert!(!d.is_closed());
}

pub fn non_member_auth_fails(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    let stranger = keys();
    // The client refuses locally …
    assert!(matches!(
        try_connect(t, &r.chain, signer(&stranger)),
        Err(RingError::Invalid(_))
    ));
    // … and the Relay refuses a hand-made attempt.
    let mut c = RawConn::open(t, &r.ring_id()).expect("open");
    let (nonce, _) = c.challenge();
    c.auth(&*stranger, &t.origin(), &r.ring_id(), &nonce);
    assert_eq!(c.error(WAIT).map(|e| e.0), Some(ErrorCode::NotMember));
    assert_eq!(c.close_code(WAIT), Some(close::REFUSED));
}

pub fn forged_auth_signature_fails(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    let forger = Arc::new(ForgingSigner {
        claims: r.daemon.sign_key(),
        signs_with: keys(),
    });
    match try_connect(t, &r.chain, forger) {
        Err(RingError::Relay { code, .. }) => assert_eq!(code, ErrorCode::BadSignature),
        Err(e) => panic!("expected bad_signature, got {e}"),
        Ok(_) => panic!("a forged signature authenticated"),
    }
}

pub fn failing_signer_fails_auth(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    let broken = Arc::new(FailingSigner(r.daemon.sign_key()));
    assert!(matches!(
        try_connect(t, &r.chain, broken),
        Err(RingError::Sign(_))
    ));
}

pub fn signature_for_other_origin_fails(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    let mut c = RawConn::open(t, &r.ring_id()).expect("open");
    let (nonce, _) = c.challenge();
    c.auth(
        &*r.daemon,
        "wss://other-relay.example",
        &r.ring_id(),
        &nonce,
    );
    assert_eq!(c.error(WAIT).map(|e| e.0), Some(ErrorCode::BadSignature));
    assert_eq!(c.close_code(WAIT), Some(close::BAD_SIGNATURE));
}

pub fn genesis_upload_creates_ring(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    assert_eq!(relay_version(t, &r.ring_id()), 0);
    let (_a, _) = connect(t, &r.up_to(1), signer(&r.desktop));
    assert_eq!(relay_version(t, &r.ring_id()), 1);
    // Without a Roster on the Relay, auth without a candidate fails.
    let other = TestRing::new(&t.url);
    let mut c = RawConn::open(t, &other.ring_id()).expect("open");
    let (nonce, v) = c.challenge();
    assert_eq!(v, 0);
    c.auth(&*other.desktop, &t.origin(), &other.ring_id(), &nonce);
    assert_eq!(c.error(WAIT).map(|e| e.0), Some(ErrorCode::NoRoster));
}

pub fn genesis_from_underived_key_is_refused(t: &RelayTarget) {
    let (a, b) = (keys(), keys());
    // A genesis signed by `a` that claims the Ring id derived from `b`.
    let ring = RingId::derive(&b.sign_key());
    let g = Roster {
        v: 1,
        ring_id: ring.clone(),
        version: 1,
        prev: None,
        relay_url: t.url.clone(),
        signed_by: a.sign_key(),
        issued_at: now(),
        members: vec![
            member(&a, "a", Role::Desktop),
            member(&b, "b", Role::Desktop),
        ],
        extra: Default::default(),
    }
    .sign(&*a)
    .expect("sign");
    let mut c = RawConn::open(t, &ring).expect("open");
    let (nonce, _) = c.challenge();
    c.auth_chain(&[&g]);
    c.auth(&*a, &t.origin(), &ring, &nonce);
    let e = c.error(WAIT).expect("an error");
    assert_eq!(
        (e.0, e.2.as_deref()),
        (ErrorCode::RosterInvalid, Some("ring_mismatch"))
    );
    assert_eq!(c.close_code(WAIT), Some(close::REFUSED));
    assert_eq!(relay_version(t, &ring), 0);
}

pub fn non_member_cannot_seed_ring(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let stranger = keys();
    let mut c = RawConn::open(t, &r.ring_id()).expect("open");
    let (nonce, _) = c.challenge();
    c.auth_chain(&[r.chain.genesis()]);
    c.auth(&*stranger, &t.origin(), &r.ring_id(), &nonce);
    assert_eq!(c.error(WAIT).map(|e| e.0), Some(ErrorCode::NotMember));
    // Nothing was committed.
    assert_eq!(relay_version(t, &r.ring_id()), 0);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    assert_eq!(relay_version(t, &r.ring_id()), 2);
}

pub fn member_added_later_supplies_extension(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    // The Relay holds v1 only; the Daemon was added in v2, which it never saw published.
    let (_a, rec_a) = connect(t, &r.up_to(1), signer(&r.desktop));
    assert_eq!(relay_version(t, &r.ring_id()), 1);
    let (d, _) = connect(t, &r.chain, signer(&r.daemon));
    assert_eq!(d.roster().version(), 2);
    assert_eq!(relay_version(t, &r.ring_id()), 2);
    // The other connected member hears of the new version.
    assert!(rec_a
        .wait_for(WAIT, |e| e == &Event::Roster { version: 2 })
        .is_some());
}

pub fn envelope_reaches_only_addressee_in_same_ring(t: &RelayTarget) {
    let r1 = TestRing::new(&t.url);
    let r2 = TestRing::new(&t.url);
    let (a1, rec_a1) = connect(t, &r1.chain, signer(&r1.desktop));
    let (_d1, rec_d1) = connect(t, &r1.chain, signer(&r1.daemon));
    let (_m1, rec_m1) = connect(t, &r1.chain, signer(&r1.mobile));
    let (_a2, rec_a2) = connect(t, &r2.chain, signer(&r2.desktop));
    a1.send(&r1.daemon.sign_key(), b"hello daemon")
        .expect("send");
    assert!(rec_d1
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"hello daemon")
        )
        .is_some());
    // Another Ring's member is not a recipient, even on the same Relay.
    a1.send(&r2.desktop.sign_key(), b"across rings")
        .expect("send");
    let refused = rec_a1.wait_for(WAIT, |e| {
        matches!(
            e,
            Event::Error {
                code: ErrorCode::UnknownRecipient,
                ..
            }
        )
    });
    assert_eq!(
        refused,
        Some(Event::Error {
            code: ErrorCode::UnknownRecipient,
            to: Some(r2.desktop.sign_key())
        })
    );
    std::thread::sleep(QUIET);
    assert!(rec_m1.envelopes().is_empty());
    assert!(rec_a2.envelopes().is_empty());
}

pub fn envelope_from_is_stamped(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    // A client-supplied `from` is ignored.
    let frame = json!({
        "t": "env",
        "to": r.desktop.sign_key().to_b64(),
        "payload": b64::encode(b"who am i"),
        "from": r.mobile.sign_key().to_b64(),
    });
    d.send(&frame.to_string()).expect("send");
    let got = rec_a.wait_for(WAIT, |e| matches!(e, Event::Envelope { .. }));
    assert_eq!(
        got,
        Some(Event::Envelope {
            from: r.daemon.sign_key(),
            payload: b"who am i".to_vec()
        })
    );
}

pub fn oversize_envelope_is_refused(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    // 64 KiB exactly is delivered …
    d.env(&r.desktop.sign_key(), &vec![7u8; MAX_ENVELOPE_PAYLOAD]);
    assert!(rec_a
        .wait_for(WAIT, |e| matches!(e, Event::Envelope { payload, .. } if payload.len() == MAX_ENVELOPE_PAYLOAD))
        .is_some());
    // … one byte more is refused, and the socket stays open.
    d.env(&r.desktop.sign_key(), &vec![7u8; MAX_ENVELOPE_PAYLOAD + 1]);
    assert_eq!(d.error(WAIT).map(|e| e.0), Some(ErrorCode::TooLarge));
    d.env(&r.desktop.sign_key(), b"still here");
    assert!(rec_a
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"still here")
        )
        .is_some());
    // The client refuses it before sending.
    assert!(matches!(
        a.send(&r.daemon.sign_key(), &vec![0u8; MAX_ENVELOPE_PAYLOAD + 1]),
        Err(RingError::Invalid(_))
    ));
}

pub fn oversize_or_malformed_control_frame_closes(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    let big = json!({"t": "bye", "reason": "quit", "pad": "x".repeat(9000)});
    d.send(&big.to_string()).expect("send");
    assert_eq!(d.error(WAIT).map(|e| e.0), Some(ErrorCode::TooLarge));
    assert_eq!(d.close_code(WAIT), Some(close::TOO_LARGE));
    let mut m = RawConn::login(t, &r.chain, &*r.mobile);
    m.send("{not json").expect("send");
    assert_eq!(m.error(WAIT).map(|e| e.0), Some(ErrorCode::BadRequest));
    assert_eq!(m.close_code(WAIT), Some(close::BAD_REQUEST));
}

pub fn envelope_to_offline_member_reports_offline(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    a.send(&r.daemon.sign_key(), b"anyone?").expect("send");
    assert_eq!(
        rec_a.wait_for(WAIT, |e| matches!(e, Event::Error { .. })),
        Some(Event::Error {
            code: ErrorCode::Offline,
            to: Some(r.daemon.sign_key())
        })
    );
}

pub fn presence_online(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let (_d, _) = connect(t, &r.chain, signer(&r.daemon));
    assert!(rec_a.wait_presence(&r.daemon.sign_key(), |p| matches!(
        p,
        MemberPresence::Online { .. }
    )));
    let d = a
        .members()
        .into_iter()
        .find(|m| m.member.sign_key == r.daemon.sign_key())
        .expect("daemon listed");
    assert!(matches!(
        d.presence,
        MemberPresence::Online { since: Some(_), .. }
    ));
}

pub fn bye_reports_xshell_closed(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let (d, rec_d) = connect(t, &r.chain, signer(&r.daemon));
    assert!(rec_a.wait_presence(&r.daemon.sign_key(), |p| matches!(
        p,
        MemberPresence::Online { .. }
    )));
    d.bye(ByeReason::quit()).expect("bye");
    assert_eq!(rec_d.closed(WAIT), Some(CloseReason::Bye));
    assert!(rec_a.wait_presence(&r.daemon.sign_key(), |p| {
        matches!(p, MemberPresence::Closed { reason, at: Some(_) } if reason == "quit")
    }));
    // A device connecting later reads the same from `welcome`.
    let (m, _) = connect(t, &r.chain, signer(&r.mobile));
    let d = m
        .members()
        .into_iter()
        .find(|s| s.member.sign_key == r.daemon.sign_key())
        .expect("daemon listed");
    assert!(matches!(d.presence, MemberPresence::Closed { ref reason, .. } if reason == "quit"));
}

pub fn drop_without_bye_reports_unreachable(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let (d, _) = connect(t, &r.chain, signer(&r.daemon));
    assert!(rec_a.wait_presence(&r.daemon.sign_key(), |p| matches!(
        p,
        MemberPresence::Online { .. }
    )));
    drop(d);
    assert!(rec_a.wait_presence(&r.daemon.sign_key(), |p| {
        matches!(p, MemberPresence::Unreachable { at: Some(_) })
    }));
}

pub fn unseen_member_is_never_connected(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, _) = connect(t, &r.chain, signer(&r.desktop));
    let m = a
        .members()
        .into_iter()
        .find(|s| s.member.sign_key == r.mobile.sign_key())
        .expect("mobile listed");
    assert_eq!(m.presence, MemberPresence::NeverConnected);
}

pub fn client_lists_online_members(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let (_d, _) = connect(t, &r.chain, signer(&r.daemon));
    let (_m, _) = connect(t, &r.chain, signer(&r.mobile));
    for k in [r.daemon.sign_key(), r.mobile.sign_key()] {
        assert!(rec_a.wait_presence(&k, |p| matches!(p, MemberPresence::Online { .. })));
    }
    let mut online: Vec<SignKey> = a
        .online_members()
        .into_iter()
        .map(|m| m.member.sign_key)
        .collect();
    online.sort();
    let mut want = vec![r.daemon.sign_key(), r.mobile.sign_key()];
    want.sort();
    assert_eq!(online, want);
}

pub fn roster_put_is_broadcast_and_accepted(t: &RelayTarget) {
    let mut r = TestRing::new(&t.url);
    let (a, _) = connect(t, &r.chain, signer(&r.desktop));
    let (d, rec_d) = connect(t, &r.chain, signer(&r.daemon));
    let phone2 = keys();
    let v3 = r.next(|d| d.add(member(&phone2, "phone-2", Role::Mobile)));
    a.publish_roster(&v3).expect("publish");
    assert_eq!(a.roster().version(), 3);
    assert!(rec_d
        .wait_for(WAIT, |e| e == &Event::Roster { version: 3 })
        .is_some());
    assert_eq!(d.roster(), v3);
    let (p2, _) = connect(t, &r.chain, signer(&phone2));
    assert_eq!(p2.roster().version(), 3);
}

pub fn relay_refuses_stale_or_mobile_signed_put(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    let mut m = RawConn::login(t, &r.chain, &*r.mobile);
    let put = |m: &mut RawConn, id: u64, roster: &SignedRoster| {
        m.send(
            &ClientFrame::RosterPut {
                id,
                roster: roster.token().to_string(),
            }
            .encode(),
        )
        .expect("send");
        m.error(WAIT).expect("an error")
    };
    let mobile_signed = raw_next(r.chain.head(), &*r.mobile, |_| {});
    let e = put(&mut m, 1, &mobile_signed);
    assert_eq!(
        (e.0, e.1, e.2.as_deref()),
        (
            ErrorCode::RosterInvalid,
            Some(1),
            Some("signer_not_desktop")
        )
    );
    let daemon_signed = raw_next(r.chain.head(), &*r.daemon, |_| {});
    assert_eq!(
        put(&mut m, 2, &daemon_signed).2.as_deref(),
        Some("signer_not_desktop")
    );
    assert_eq!(put(&mut m, 3, r.chain.genesis()).0, ErrorCode::RosterStale);
    let fork = raw_next(r.chain.genesis(), &*r.desktop, |x| {
        x.issued_at += 99;
        x.members.push(member(&r.mobile, "fork", Role::Mobile));
    });
    assert_eq!(put(&mut m, 4, &fork).0, ErrorCode::RosterConflict);
    let gap = raw_next(r.chain.head(), &*r.desktop, |x| x.version += 1);
    assert_eq!(put(&mut m, 5, &gap).2.as_deref(), Some("gap"));
    assert_eq!(relay_version(t, &r.ring_id()), 2);
}

pub fn removed_member_is_disconnected(t: &RelayTarget) {
    let mut r = TestRing::new(&t.url);
    let (a, _) = connect(t, &r.chain, signer(&r.desktop));
    let (_m, rec_m) = connect(t, &r.chain, signer(&r.mobile));
    let gone = r.mobile.sign_key();
    let v3 = r.next(|d| {
        d.remove(&gone);
    });
    a.publish_roster(&v3).expect("publish");
    assert_eq!(
        rec_m.closed(WAIT),
        Some(CloseReason::Relay {
            close_code: Some(close::REMOVED),
            error: Some(ErrorCode::Removed)
        })
    );
    // It cannot come back on the new head.
    let mut c = RawConn::open(t, &r.ring_id()).expect("open");
    let (nonce, _) = c.challenge();
    c.auth(&*r.mobile, &t.origin(), &r.ring_id(), &nonce);
    assert_eq!(c.error(WAIT).map(|e| e.0), Some(ErrorCode::NotMember));
}

/// After a removal the Relay stops routing for the removed member: an envelope addressed to
/// it is `unknown_recipient` (the socket stays open), and its presence is forgotten.
pub fn removed_member_is_unknown_recipient(t: &RelayTarget) {
    let mut r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let (_m, rec_m) = connect(t, &r.chain, signer(&r.mobile));
    let gone = r.mobile.sign_key();
    let v3 = r.next(|d| {
        d.remove(&gone);
    });
    a.publish_roster(&v3).expect("publish");
    assert!(rec_m.closed(WAIT).is_some(), "the removed member is cut");
    let mut d = RawConn::login(t, &r.chain, &*r.desktop2);
    d.env(&gone, b"still there?");
    let deadline = Instant::now() + WAIT;
    let refused = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match d.frame(left) {
            Some(RelayFrame::Error { code, to, .. }) => break Some((code, to)),
            Some(_) => continue,
            None => break None,
        }
    };
    assert_eq!(refused, Some((ErrorCode::UnknownRecipient, Some(gone))));
    // The socket stays open.
    d.env(&r.desktop.sign_key(), b"still here");
    assert!(rec_a
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"still here")
        )
        .is_some());
    // A device logging in now hears nothing of the removed member.
    let mut c = RawConn::open(t, &r.ring_id()).expect("open");
    let (nonce, v) = c.challenge();
    assert_eq!(v, 3);
    c.auth(&*r.daemon, &t.origin(), &r.ring_id(), &nonce);
    match c.frame(WAIT) {
        Some(RelayFrame::Welcome { presence, .. }) => {
            assert!(presence.iter().all(|p| p.sign_key != gone), "{presence:?}");
            assert!(presence.iter().any(|p| p.sign_key == r.daemon.sign_key()));
        }
        other => panic!("expected welcome, got {other:?}"),
    }
}

/// A Mobile-signed extension staged through `auth.chain` is refused before anything is
/// committed: only a Desktop may sign a new version.
pub fn auth_chain_signed_by_mobile_is_refused(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    assert_eq!(relay_version(t, &r.ring_id()), 2);
    let intruder = keys();
    let forged = raw_next(r.chain.head(), &*r.mobile, |x| {
        x.members.push(member(&intruder, "intruder", Role::Desktop));
    });
    let mut c = RawConn::open(t, &r.ring_id()).expect("open");
    let (nonce, v) = c.challenge();
    assert_eq!(v, 2);
    c.auth_chain(&[&forged]);
    c.auth(&*r.mobile, &t.origin(), &r.ring_id(), &nonce);
    let e = c.error(WAIT).expect("an error");
    assert_eq!(
        (e.0, e.2.as_deref()),
        (ErrorCode::RosterInvalid, Some("signer_not_desktop"))
    );
    assert_eq!(c.close_code(WAIT), Some(close::REFUSED));
    assert_eq!(relay_version(t, &r.ring_id()), 2);
    // The intruder the forged version names is no member.
    let mut i = RawConn::open(t, &r.ring_id()).expect("open");
    let (nonce, _) = i.challenge();
    i.auth(&*intruder, &t.origin(), &r.ring_id(), &nonce);
    assert_eq!(i.error(WAIT).map(|e| e.0), Some(ErrorCode::NotMember));
}

pub fn client_syncs_newer_roster_on_welcome(t: &RelayTarget) {
    let mut r = TestRing::new(&t.url);
    let old = r.chain.clone();
    let (a, _) = connect(t, &r.chain, signer(&r.desktop));
    let phone2 = keys();
    let v3 = r.next(|d| d.add(member(&phone2, "phone-2", Role::Mobile)));
    a.publish_roster(&v3).expect("publish");
    let (d, rec_d) = connect(t, &old, signer(&r.daemon));
    assert_eq!(d.roster(), v3);
    assert!(rec_d.events().contains(&Event::Roster { version: 3 }));
}

/// A Ring moves from `old` to `new` with a chain bigger than one frame: the first device on
/// the new Relay stages it over several `auth.chain` frames, and a device that missed the
/// move pages it down through `roster.get`.
pub fn ring_moves_to_new_relay_with_full_chain(old: &RelayTarget, new: &RelayTarget) {
    let mut r = TestRing::new(&old.url);
    let (_a, _) = connect(old, &r.chain, signer(&r.desktop));
    let new_url = new.url.clone();
    r.next(|d| d.relay_url = new_url);
    let lagging = r.chain.clone();
    // Pad the following versions so the whole chain is over 1 MiB.
    let pad = "p".repeat(44 * 1024);
    r.next(|d| {
        d.extra.insert("pad".into(), json!(pad));
    });
    for _ in 0..18 {
        r.next(|_| {});
    }
    let total: usize = r.chain.versions().iter().map(|v| v.token().len()).sum();
    assert!(total > super::wire::MAX_FRAME, "chain is {total} bytes");
    let (a2, _) = connect(new, &r.chain, signer(&r.desktop));
    assert_eq!(a2.roster().version(), r.chain.head().version());
    assert_eq!(relay_version(new, &r.ring_id()), r.chain.head().version());
    let (d, _) = connect(new, &lagging, signer(&r.daemon));
    assert_eq!(d.chain(), r.chain);
}

pub fn entitlement_slot_round_trips(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, _) = connect(t, &r.chain, signer(&r.desktop));
    let (_d, rec_d) = connect(t, &r.chain, signer(&r.daemon));
    let token = fake_entitlement(&r.ring_id());
    a.put_entitlement(&token).expect("put");
    assert!(rec_d
        .wait_for(WAIT, |e| e == &Event::Entitlement(Some(token.clone())))
        .is_some());
    let (m, _) = connect(t, &r.chain, signer(&r.mobile));
    assert_eq!(m.entitlement().as_deref(), Some(token.as_str()));
    let mut raw = RawConn::login(t, &r.chain, &*r.desktop2);
    raw.send(
        &ClientFrame::EntitlementPut {
            id: 9,
            token: "xet1.nope".into(),
        }
        .encode(),
    )
    .expect("send");
    assert_eq!(
        raw.error(WAIT).map(|e| (e.0, e.1)),
        Some((ErrorCode::EntitlementInvalid, Some(9)))
    );
}

pub fn auth_timeout_closes(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let mut c = RawConn::open(t, &r.ring_id()).expect("open");
    let _ = c.challenge();
    let started = Instant::now();
    let limit = t.auth_timeout + Duration::from_secs(3);
    assert_eq!(c.error(limit).map(|e| e.0), Some(ErrorCode::AuthTimeout));
    assert_eq!(c.close_code(WAIT), Some(close::AUTH_TIMEOUT));
    assert!(started.elapsed() < limit);
}

pub fn replaced_socket_close_keeps_new_online(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mut old = RawConn::login(t, &r.chain, &*r.daemon);
    let _new = RawConn::login(t, &r.chain, &*r.daemon);
    assert_eq!(old.error(WAIT).map(|e| e.0), Some(ErrorCode::Replaced));
    // The replaced socket can no longer send for its key.
    let _ = old.send(
        &ClientFrame::Env {
            to: r.desktop.sign_key(),
            payload: b64::encode(b"from the old socket"),
        }
        .encode(),
    );
    assert_eq!(old.close_code(WAIT), Some(close::REPLACED));
    drop(old);
    std::thread::sleep(QUIET);
    assert!(rec_a.envelopes().is_empty());
    assert!(matches!(
        rec_a.presence_of(&r.daemon.sign_key()),
        Some(MemberPresence::Online { .. })
    ));
    let d = a
        .members()
        .into_iter()
        .find(|m| m.member.sign_key == r.daemon.sign_key())
        .expect("listed");
    assert!(matches!(d.presence, MemberPresence::Online { .. }));
}

pub fn stale_bye_after_replacement_is_ignored(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mut old = RawConn::login(t, &r.chain, &*r.daemon);
    let _new = RawConn::login(t, &r.chain, &*r.daemon);
    // The replaced socket says goodbye late.
    let _ = old.send(
        &ClientFrame::Bye {
            reason: ByeReason::quit(),
        }
        .encode(),
    );
    let _ = old.close_code(WAIT);
    std::thread::sleep(QUIET);
    assert!(matches!(
        rec_a.presence_of(&r.daemon.sign_key()),
        Some(MemberPresence::Online { .. })
    ));
}

pub fn unknown_client_type_is_answered_and_tolerated(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    d.send(r#"{"t":"future.thing","foreground":true}"#)
        .expect("send");
    assert_eq!(d.error(WAIT).map(|e| e.0), Some(ErrorCode::UnknownType));
    d.env(&r.desktop.sign_key(), b"after unknown");
    assert!(rec_a
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"after unknown")
        )
        .is_some());
}

pub fn binary_frame_is_unsupported(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, _) = connect(t, &r.chain, signer(&r.desktop));
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    d.send_binary(b"\x00\x01").expect("send");
    assert_eq!(d.error(WAIT).map(|e| e.0), Some(ErrorCode::Unsupported));
    assert_eq!(d.close_code(WAIT), Some(close::BAD_REQUEST));
}

pub fn readded_member_keeps_new_socket_online(t: &RelayTarget) {
    let mut r = TestRing::new(&t.url);
    let mobile = r.mobile.clone();
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mut old = RawConn::login(t, &r.chain, &*mobile);
    assert!(rec_a.wait_presence(&mobile.sign_key(), |p| matches!(
        p,
        MemberPresence::Online { .. }
    )));
    // Removed, while its old socket stops answering (so its close arrives late) …
    let v3 = r.next(|d| {
        d.remove(&mobile.sign_key());
    });
    a.publish_roster(&v3).expect("remove");
    // … then added back, and connected again on a new socket.
    let v4 = r.next(|d| d.add(member(&mobile, "mobile-again", Role::Mobile)));
    a.publish_roster(&v4).expect("re-add");
    let _new = RawConn::login(t, &r.chain, &*mobile);
    assert!(rec_a
        .wait_for(WAIT, |e| e == &Event::Roster { version: 4 })
        .is_some());
    std::thread::sleep(QUIET);
    // The old socket's late goodbye and close do not touch the new socket.
    let _ = old.send(
        &ClientFrame::Bye {
            reason: ByeReason::quit(),
        }
        .encode(),
    );
    drop(old);
    std::thread::sleep(Duration::from_millis(1500));
    let m = a
        .members()
        .into_iter()
        .find(|s| s.member.sign_key == mobile.sign_key())
        .expect("listed");
    assert!(
        matches!(m.presence, MemberPresence::Online { .. }),
        "{:?}",
        m.presence
    );
}

fn gateway(t: &RelayTarget) -> &Arc<dyn Signer> {
    t.gateway
        .as_ref()
        .expect("a Hosted target carries the gateway's signing key")
}

/// A Hosted entitlement for `ring`, valid until `expires_at`.
pub fn hosted_token(t: &RelayTarget, ring: &RingId, expires_at: u64) -> String {
    sign_entitlement(
        &**gateway(t),
        ring,
        Tier::Hosted,
        "test-purchase",
        now(),
        expires_at,
    )
    .expect("sign entitlement")
}

fn refused_for_entitlement(rec: &Recorder) -> bool {
    rec.wait_for(WAIT, |e| {
        matches!(
            e,
            Event::Error {
                code: ErrorCode::EntitlementRequired,
                ..
            }
        )
    })
    .is_some()
}

/// Hosted: a new Ring authenticates into a limited session that does not route, installs
/// its first token from there, and then routes.
pub fn hosted_first_token_enables_routing(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let (d, rec_d) = connect(t, &r.chain, signer(&r.daemon));
    assert!(a.limited() && d.limited());
    assert_eq!(a.entitlement(), None);
    a.send(&r.daemon.sign_key(), b"before").expect("send");
    assert!(refused_for_entitlement(&rec_a));
    assert!(!a.is_closed(), "a limited session stays open");
    // A Push-tier token is stored but does not lift the limit.
    let push = sign_entitlement(
        &**gateway(t),
        &r.ring_id(),
        Tier::Push,
        "p",
        now(),
        now() + 3600,
    )
    .expect("sign");
    a.put_entitlement(&push).expect("put push token");
    a.send(&r.daemon.sign_key(), b"push tier").expect("send");
    let token = hosted_token(t, &r.ring_id(), now() + 3600);
    a.put_entitlement(&token).expect("put hosted token");
    assert!(rec_d
        .wait_for(WAIT, |e| e == &Event::Entitlement(Some(token.clone())))
        .is_some());
    assert!(!d.limited());
    a.send(&r.daemon.sign_key(), b"after").expect("send");
    assert!(rec_d
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"after")
        )
        .is_some());
    assert!(!rec_d
        .envelopes()
        .iter()
        .any(|(_, p)| p == b"before" || p == b"push tier"));
    // A token for another Ring, or a forged one, is refused.
    let other = TestRing::new(&t.url);
    let foreign = hosted_token(t, &other.ring_id(), now() + 3600);
    assert!(matches!(
        a.put_entitlement(&foreign),
        Err(RingError::Relay {
            code: ErrorCode::EntitlementInvalid,
            ..
        })
    ));
    assert!(matches!(
        a.put_entitlement(&fake_entitlement(&r.ring_id())),
        Err(RingError::Relay {
            code: ErrorCode::EntitlementInvalid,
            ..
        })
    ));
}

/// Hosted: when the last token expires, existing and new connections stop routing; a
/// refreshed token lifts the limit for all of them. The routing-level cutoff.
pub fn hosted_routing_ends_when_the_last_token_expires(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let (d, rec_d) = connect(t, &r.chain, signer(&r.daemon));
    let expires = now() + 2;
    let token = hosted_token(t, &r.ring_id(), expires);
    a.put_entitlement(&token).expect("put");
    assert!(rec_d
        .wait_for(WAIT, |e| matches!(e, Event::Entitlement(Some(_))))
        .is_some());
    a.send(&r.daemon.sign_key(), b"in time").expect("send");
    assert!(rec_d
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"in time")
        )
        .is_some());
    let deadline = Instant::now() + Duration::from_secs(5);
    while now() < expires && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // The existing connection is refused …
    a.send(&r.daemon.sign_key(), b"too late").expect("send");
    assert!(refused_for_entitlement(&rec_a));
    assert!(a.limited());
    // … and so is a new one, which also cannot install an expired token.
    let (m, rec_m) = connect(t, &r.chain, signer(&r.mobile));
    assert!(m.limited());
    assert_eq!(m.entitlement(), None);
    m.send(&r.desktop.sign_key(), b"new but late")
        .expect("send");
    assert!(refused_for_entitlement(&rec_m));
    let expired = hosted_token(t, &r.ring_id(), now() - 1);
    assert!(matches!(
        m.put_entitlement(&expired),
        Err(RingError::Relay {
            code: ErrorCode::EntitlementInvalid,
            ..
        })
    ));
    // A refreshed token lifts it for everyone.
    let fresh = hosted_token(t, &r.ring_id(), now() + 3600);
    d.put_entitlement(&fresh).expect("refresh");
    assert!(rec_m
        .wait_for(WAIT, |e| e == &Event::Entitlement(Some(fresh.clone())))
        .is_some());
    assert!(!m.limited());
    m.send(&r.desktop.sign_key(), b"refreshed").expect("send");
    assert!(rec_a
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"refreshed")
        )
        .is_some());
    std::thread::sleep(QUIET);
    let late: Vec<_> = rec_d
        .envelopes()
        .into_iter()
        .filter(|(_, p)| p == b"too late")
        .collect();
    assert!(late.is_empty());
}

/// A Relay with a daily quota of client frames per Ring (`t.quota_frames_per_day`, at most
/// 1000): exact pings are not counted, and neither are the frames before `auth`; once the
/// Ring's quota for the day is used up, every frame but `bye` is refused with `quota`
/// (echoing the frame's `to` or `id`), the 32nd refusal in a row closes the socket with 4029,
/// and another socket of the same Ring is refused too.
pub fn quota_refuses_then_closes(t: &RelayTarget) {
    let limit = t
        .quota_frames_per_day
        .expect("the target names its daily quota");
    assert!(
        (1..=1000).contains(&limit),
        "the quota scenarios need a quota of at most 1000 frames"
    );
    let r = TestRing::new(&t.url);
    let mut a = RawConn::login(t, &r.chain, &*r.desktop);
    let head = r.chain.head().version();
    for _ in 0..limit + 8 {
        a.send(PING).expect("send ping");
    }
    for _ in 0..limit + 8 {
        assert_eq!(
            a.recv(WAIT),
            Raw::Text(PONG.into()),
            "exact pings are answered"
        );
    }
    let get = |id: u64| ClientFrame::RosterGet { id, since: head }.encode();
    for id in 0..limit {
        a.send(&get(id)).expect("send roster.get");
        match a.frame(WAIT) {
            Some(RelayFrame::RosterChain { id: got, .. }) => assert_eq!(got, id),
            other => panic!("frame {id} of {limit}: expected roster.chain, got {other:?}"),
        }
    }
    // Over the quota: refused, with `to` or `id` echoed.
    a.env(&r.daemon.sign_key(), b"over quota");
    match a.frame(WAIT) {
        Some(RelayFrame::Error { code, to, .. }) => {
            assert_eq!(code, ErrorCode::Quota);
            assert_eq!(to, Some(r.daemon.sign_key()));
        }
        other => panic!("expected a quota error, got {other:?}"),
    }
    for id in 1..QUOTA_REFUSALS_BEFORE_CLOSE as u64 {
        a.send(&get(1000 + id)).expect("send roster.get");
        match a.frame(WAIT) {
            Some(RelayFrame::Error { code, id: got, .. }) => {
                assert_eq!(code, ErrorCode::Quota);
                assert_eq!(got, Some(1000 + id));
            }
            other => panic!("refusal {id}: expected a quota error, got {other:?}"),
        }
    }
    assert_eq!(a.close_code(WAIT), Some(close::QUOTA));
    // The quota is the Ring's: another member logs in (auth is not counted) but is refused,
    // and its goodbye still works.
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    d.send(&get(1)).expect("send roster.get");
    assert_eq!(d.error(WAIT).map(|e| e.0), Some(ErrorCode::Quota));
    d.send(
        &ClientFrame::Bye {
            reason: ByeReason::quit(),
        }
        .encode(),
    )
    .expect("send bye");
    assert_eq!(d.close_code(WAIT), Some(close::NORMAL));
}

// ---- Foreground and push (sections 17 and 18) -------------------------------------------------

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn state(c: &mut RawConn, foreground: bool) {
    c.send(&ClientFrame::State { foreground }.encode())
        .expect("send state");
}

fn foreground(p: &MemberPresence) -> bool {
    p.is_foreground(now_ms())
}

fn not_foreground_online(p: &MemberPresence) -> bool {
    matches!(p, MemberPresence::Online { .. }) && !p.is_foreground(now_ms())
}

/// Presence events about `key` the recorder holds.
fn presence_count(rec: &Recorder, key: &SignKey) -> usize {
    rec.events()
        .iter()
        .filter(|e| matches!(e, Event::Presence { key: k, .. } if k == key))
        .count()
}

/// A Mobile's `state` reaches the others' presence, with a lease deadline; a repeat changes
/// nothing; `state false` clears it; a device connecting later reads it from `welcome`.
pub fn state_foreground_reaches_presence(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    assert!(a.relay_caps().iter().any(|c| c == CAP_FOREGROUND));
    let mut m = RawConn::login(t, &r.chain, &*r.mobile);
    let mk = r.mobile.sign_key();
    assert!(rec_a.wait_presence(&mk, |p| matches!(p, MemberPresence::Online { .. })));
    let before = now_ms();
    state(&mut m, true);
    assert!(rec_a.wait_presence(&mk, foreground));
    let lease = t.foreground_lease.unwrap_or(FOREGROUND_LEASE).as_millis() as u64;
    match rec_a.presence_of(&mk) {
        Some(MemberPresence::Online {
            foreground_until: Some(u),
            ..
        }) => assert!(
            u > before && u <= now_ms() + lease + 1000,
            "foregroundUntil {u} outside the lease"
        ),
        other => panic!("expected a foreground lease, got {other:?}"),
    }
    let n = presence_count(&rec_a, &mk);
    state(&mut m, true);
    std::thread::sleep(QUIET);
    assert_eq!(
        presence_count(&rec_a, &mk),
        n,
        "a repeated state is broadcast"
    );
    state(&mut m, false);
    assert!(rec_a.wait_presence(&mk, not_foreground_online));
    state(&mut m, true);
    assert!(rec_a.wait_presence(&mk, foreground));
    let (d, _) = connect(t, &r.chain, signer(&r.daemon));
    let seen = d
        .members()
        .into_iter()
        .find(|s| s.member.sign_key == mk)
        .expect("mobile listed");
    assert!(seen.presence.is_foreground(now_ms()), "{:?}", seen.presence);
}

/// Foreground ends with the socket: on `bye`, on a drop, and a new socket starts without it.
pub fn state_foreground_clears_on_bye_and_drop(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mk = r.mobile.sign_key();
    let mut m = RawConn::login(t, &r.chain, &*r.mobile);
    state(&mut m, true);
    assert!(rec_a.wait_presence(&mk, foreground));
    m.send(
        &ClientFrame::Bye {
            reason: ByeReason::quit(),
        }
        .encode(),
    )
    .expect("send bye");
    assert!(rec_a.wait_presence(&mk, |p| matches!(p, MemberPresence::Closed { .. })));
    assert!(!rec_a.presence_of(&mk).unwrap().is_foreground(0));
    let mut m = RawConn::login(t, &r.chain, &*r.mobile);
    assert!(rec_a.wait_presence(&mk, not_foreground_online));
    state(&mut m, true);
    assert!(rec_a.wait_presence(&mk, foreground));
    drop(m);
    assert!(rec_a.wait_presence(&mk, |p| matches!(p, MemberPresence::Unreachable { .. })));
    let _m = RawConn::login(t, &r.chain, &*r.mobile);
    std::thread::sleep(QUIET);
    let p = rec_a.presence_of(&mk).expect("presence");
    assert!(not_foreground_online(&p), "{p:?}");
}

/// Only a Mobile may send `state`: anyone else gets `bad_request` and stays connected.
pub fn state_refused_from_non_mobile(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    state(&mut d, true);
    assert_eq!(d.error(WAIT).map(|e| e.0), Some(ErrorCode::BadRequest));
    d.env(&r.desktop.sign_key(), b"still here");
    assert!(rec_a
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"still here")
        )
        .is_some());
    let mut d2 = RawConn::login(t, &r.chain, &*r.desktop2);
    state(&mut d2, true);
    assert_eq!(d2.error(WAIT).map(|e| e.0), Some(ErrorCode::BadRequest));
    std::thread::sleep(QUIET);
    for k in [r.daemon.sign_key(), r.desktop2.sign_key()] {
        assert!(!rec_a.presence_of(&k).is_some_and(|p| p.is_foreground(0)));
    }
}

/// A lease runs out without frames or pings, even with the socket open: the flag clears and
/// the change is broadcast. Pings renew it.
pub fn state_foreground_lease_expires(t: &RelayTarget) {
    let lease = t
        .foreground_lease
        .expect("a target with a short foreground lease");
    assert!(lease <= Duration::from_secs(5));
    let r = TestRing::new(&t.url);
    let (_a, rec_a) = connect(t, &r.chain, signer(&r.desktop));
    let mk = r.mobile.sign_key();
    // Pinging: renewed past the first lease.
    let mut m = RawConn::login(t, &r.chain, &*r.mobile);
    state(&mut m, true);
    assert!(rec_a.wait_presence(&mk, foreground));
    let first = match rec_a.presence_of(&mk) {
        Some(MemberPresence::Online {
            foreground_until: Some(u),
            ..
        }) => u,
        other => panic!("expected a lease, got {other:?}"),
    };
    let end = Instant::now() + lease * 2;
    while Instant::now() < end {
        m.send(PING).expect("ping");
        let _ = m.recv(lease / 8);
    }
    let p = rec_a.presence_of(&mk).expect("presence");
    match p {
        MemberPresence::Online {
            foreground_until: Some(u),
            ..
        } => assert!(u > first && p.is_foreground(now_ms()), "not renewed: {u}"),
        other => panic!("a pinging Mobile lost foreground: {other:?}"),
    }
    // Silent (a half-open socket): cleared once the lease runs out, still online.
    let silent = Instant::now();
    assert!(rec_a
        .wait_for(lease * 2 + WAIT, |e| matches!(
            e,
            Event::Presence { key, presence } if key == &mk && silent.elapsed() > lease / 2
                && not_foreground_online(presence)
        ))
        .is_some());
}

fn fake_gateway(t: &RelayTarget) -> &Arc<FakeGateway> {
    t.fake_gateway
        .as_ref()
        .expect("a target with a fake Push Gateway")
}

/// A unique, well-formed push request.
pub fn push_request() -> PushRequest {
    let mut n = [0u8; 16];
    getrandom::getrandom(&mut n).expect("randomness");
    PushRequest {
        blob: format!("xpb1.test.{}", b64::encode(&n)),
        sealed_payload: b64::encode(&[7u8; 609]),
        collapse_id: format!("c-{}", &b64::encode(&n)[..8]),
    }
}

/// The gateway's requests for `req`'s blob.
fn forwarded(gw: &FakeGateway, req: &PushRequest) -> Vec<serde_json::Value> {
    gw.requests()
        .into_iter()
        .filter(|v| v["blob"] == req.blob.as_str())
        .collect()
}

fn push_frame(id: u64, req: &PushRequest) -> String {
    ClientFrame::Push {
        id,
        blob: req.blob.clone(),
        sealed_payload: req.sealed_payload.clone(),
        collapse_id: req.collapse_id.clone(),
    }
    .encode()
}

fn push_failed(r: Result<(), RingError>) -> Option<String> {
    match r {
        Err(RingError::Relay {
            code: ErrorCode::PushFailed,
            detail,
        }) => detail,
        other => panic!("expected push_failed, got {other:?}"),
    }
}

/// A Daemon's push reaches the gateway exactly once, as exactly `{blob, sealedPayload,
/// collapseId}`, and is answered `ok`.
pub fn push_forwards_exactly_once_verbatim(t: &RelayTarget) {
    let gw = fake_gateway(t);
    let r = TestRing::new(&t.url);
    let (d, _) = connect(t, &r.chain, signer(&r.daemon));
    assert!(d.relay_caps().iter().any(|c| c == CAP_PUSH));
    let req = push_request();
    d.push(&req, WAIT).expect("push");
    std::thread::sleep(QUIET);
    let got = forwarded(gw, &req);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        got[0],
        json!({
            "blob": req.blob,
            "sealedPayload": req.sealed_payload,
            "collapseId": req.collapse_id,
        })
    );
}

/// While any Mobile of the Ring is in the foreground the Relay refuses a push with
/// `foreground` and calls no gateway; once it leaves, pushes go through.
pub fn push_refused_while_a_mobile_is_foreground(t: &RelayTarget) {
    let gw = fake_gateway(t);
    let r = TestRing::new(&t.url);
    let (d, rec_d) = connect(t, &r.chain, signer(&r.daemon));
    let mk = r.mobile.sign_key();
    let mut m = RawConn::login(t, &r.chain, &*r.mobile);
    state(&mut m, true);
    assert!(rec_d.wait_presence(&mk, foreground));
    let req = push_request();
    assert_eq!(
        push_failed(d.push(&req, WAIT)).as_deref(),
        Some("foreground")
    );
    state(&mut m, false);
    assert!(rec_d.wait_presence(&mk, not_foreground_online));
    std::thread::sleep(QUIET);
    assert!(forwarded(gw, &req).is_empty(), "forwarded while foreground");
    d.push(&req, WAIT).expect("push after the foreground");
    assert_eq!(forwarded(gw, &req).len(), 1);
}

/// Only a Daemon may push: a Desktop or a Mobile gets `push_failed` with detail `role`.
pub fn push_refused_from_non_daemon(t: &RelayTarget) {
    let gw = fake_gateway(t);
    let r = TestRing::new(&t.url);
    let req = push_request();
    for who in [&r.desktop, &r.mobile] {
        let mut c = RawConn::login(t, &r.chain, &**who);
        c.send(&push_frame(5, &req)).expect("send push");
        assert_eq!(
            c.error(WAIT),
            Some((ErrorCode::PushFailed, Some(5), Some("role".into())))
        );
    }
    std::thread::sleep(QUIET);
    assert!(forwarded(gw, &req).is_empty());
}

/// A gateway refusal comes back as its `code` when that is `^[a-z_]{1,32}$`, else as
/// `gateway`.
pub fn push_relays_gateway_refusal_detail(t: &RelayTarget) {
    let gw = fake_gateway(t);
    let r = TestRing::new(&t.url);
    let (d, _) = connect(t, &r.chain, signer(&r.daemon));
    for (reply, detail) in [
        (Reply::refusal(410, "device_gone"), "device_gone"),
        (Reply::refusal(429, "quota_exceeded"), "quota_exceeded"),
        (Reply::refusal(400, "Not-A-Code"), "gateway"),
        (
            Reply {
                status: 500,
                body: "oops".into(),
            },
            "gateway",
        ),
    ] {
        gw.respond(reply);
        let req = push_request();
        assert_eq!(push_failed(d.push(&req, WAIT)).as_deref(), Some(detail));
        assert_eq!(forwarded(gw, &req).len(), 1);
    }
}

/// Malformed push fields are `bad_request` with the frame's `id`, and nothing is forwarded.
pub fn push_bad_fields_refused(t: &RelayTarget) {
    let gw = fake_gateway(t);
    let r = TestRing::new(&t.url);
    let mut d = RawConn::login(t, &r.chain, &*r.daemon);
    let ok = push_request();
    let cases = [
        PushRequest {
            blob: "xpb2.nope.AAAA".into(),
            ..ok.clone()
        },
        PushRequest {
            blob: format!("xpb1.{}", "a".repeat(4092)),
            ..ok.clone()
        },
        PushRequest {
            blob: "xpb1.a b".into(),
            ..ok.clone()
        },
        PushRequest {
            sealed_payload: "AA==".into(),
            ..ok.clone()
        },
        PushRequest {
            sealed_payload: "A".repeat(3073),
            ..ok.clone()
        },
        PushRequest {
            sealed_payload: String::new(),
            ..ok.clone()
        },
        PushRequest {
            collapse_id: String::new(),
            ..ok.clone()
        },
        PushRequest {
            collapse_id: "c".repeat(65),
            ..ok.clone()
        },
    ];
    for (i, c) in cases.iter().enumerate() {
        let id = 100 + i as u64;
        d.send(&push_frame(id, c)).expect("send push");
        assert_eq!(
            d.error(WAIT).map(|e| (e.0, e.1)),
            Some((ErrorCode::BadRequest, Some(id))),
            "case {i}"
        );
    }
    std::thread::sleep(QUIET);
    // Every case keeps the blob or the collapse id of `ok`.
    assert!(gw
        .requests()
        .iter()
        .all(|v| v["blob"] != ok.blob.as_str() && v["collapseId"] != ok.collapse_id.as_str()));
}

/// A Relay without a gateway lists no `push` cap and refuses a push with `unavailable`; the
/// client does not even send one.
pub fn push_unavailable_without_gateway(t: &RelayTarget) {
    let r = TestRing::new(&t.url);
    let (d, _) = connect(t, &r.chain, signer(&r.daemon));
    assert!(!d.relay_caps().iter().any(|c| c == CAP_PUSH));
    assert!(d.relay_caps().iter().any(|c| c == CAP_FOREGROUND));
    assert!(matches!(
        d.push(&push_request(), WAIT),
        Err(RingError::Relay {
            code: ErrorCode::Unsupported,
            ..
        })
    ));
    let mut raw = RawConn::login(t, &r.chain, &*r.daemon);
    raw.send(&push_frame(3, &push_request()))
        .expect("send push");
    assert_eq!(
        raw.error(WAIT),
        Some((ErrorCode::PushFailed, Some(3), Some("unavailable".into())))
    );
}

// ---- The pairing pipe (section 16) ------------------------------------------------------------

/// A fresh random slot.
pub fn fresh_slot() -> String {
    PairSecret::generate().expect("randomness").slot()
}

fn expect_pair(c: &mut RawConn, what: &str, pred: impl Fn(&PairRelayFrame) -> bool) {
    match c.pair_frame(WAIT) {
        Some(f) if pred(&f) => {}
        other => panic!("expected {what}, got {other:?}"),
    }
}

fn expect_pair_error(c: &mut RawConn, code: ErrorCode, close_code: u16) {
    expect_pair(
        c,
        code.as_str(),
        |f| matches!(f, PairRelayFrame::Error { code: c, .. } if *c == code),
    );
    assert_eq!(c.close_code(WAIT), Some(close_code));
}

/// The first socket on a slot hears `pair.wait`, the second makes both hear `pair.peer`, and
/// `pair.msg` goes to the other side unchanged, both ways.
pub fn pair_pipe_joins_two(t: &RelayTarget) {
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(t, &slot).expect("open a");
    expect_pair(&mut a, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { v: 1 })
    });
    let mut b = RawConn::open_pair(t, &slot).expect("open b");
    expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
    expect_pair(&mut a, "pair.peer", |f| *f == PairRelayFrame::Peer);
    a.pair_msg(b"from a");
    expect_pair(&mut b, "a's message", |f| {
        *f == PairRelayFrame::Msg {
            payload: b64::encode(b"from a"),
        }
    });
    b.pair_msg(&[7u8; 8192]);
    expect_pair(&mut a, "b's message", |f| {
        *f == PairRelayFrame::Msg {
            payload: b64::encode(&[7u8; 8192]),
        }
    });
    a.send(PING).expect("ping");
    match a.recv(WAIT) {
        Raw::Text(t) => assert_eq!(t, PONG),
        other => panic!("expected pong, got {other:?}"),
    }
}

/// A third socket gets `pair_busy` and close 4010, and so does any socket on a slot whose
/// two parties met, after they left.
pub fn pair_pipe_refuses_third(t: &RelayTarget) {
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(t, &slot).expect("open a");
    expect_pair(&mut a, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    let mut b = RawConn::open_pair(t, &slot).expect("open b");
    expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
    let mut c = RawConn::open_pair(t, &slot).expect("open c");
    expect_pair_error(&mut c, ErrorCode::PairBusy, close::PAIR_BUSY);
    drop(a);
    drop(b);
    std::thread::sleep(QUIET);
    let mut d = RawConn::open_pair(t, &slot).expect("open d");
    expect_pair_error(&mut d, ErrorCode::PairBusy, close::PAIR_BUSY);
}

/// More than eight `pair.msg` from one socket: `too_many`, close 4000. A message over
/// 8192 bytes, a message before the other side came, or any other type: `bad_request`,
/// close 4000. A frame over 16 KiB: `too_large`, close 1009.
pub fn pair_pipe_caps_messages(t: &RelayTarget) {
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(t, &slot).expect("open a");
    expect_pair(&mut a, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    let mut b = RawConn::open_pair(t, &slot).expect("open b");
    expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
    expect_pair(&mut a, "pair.peer", |f| *f == PairRelayFrame::Peer);
    for i in 0..MAX_PAIR_MSGS {
        a.pair_msg(&[i as u8]);
    }
    for _ in 0..MAX_PAIR_MSGS {
        expect_pair(&mut b, "a message", |f| {
            matches!(f, PairRelayFrame::Msg { .. })
        });
    }
    a.pair_msg(b"one too many");
    expect_pair_error(&mut a, ErrorCode::TooMany, close::BAD_REQUEST);

    let cases: Vec<(String, ErrorCode, u16)> = vec![
        (
            PairClientFrame::Msg {
                payload: b64::encode(&[0u8; 8193]),
            }
            .encode(),
            ErrorCode::BadRequest,
            close::BAD_REQUEST,
        ),
        (
            json!({"t": "env", "to": "x", "payload": ""}).to_string(),
            ErrorCode::BadRequest,
            close::BAD_REQUEST,
        ),
        (
            json!({"t": "ping", "pad": "x".repeat(17 * 1024)}).to_string(),
            ErrorCode::TooLarge,
            close::TOO_LARGE,
        ),
    ];
    for (frame, code, close_code) in cases {
        let slot = fresh_slot();
        let mut a = RawConn::open_pair(t, &slot).expect("open");
        expect_pair(&mut a, "pair.wait", |f| {
            matches!(f, PairRelayFrame::Wait { .. })
        });
        let mut b = RawConn::open_pair(t, &slot).expect("open b");
        expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
        expect_pair(&mut a, "pair.peer", |f| *f == PairRelayFrame::Peer);
        a.send(&frame).expect("send");
        expect_pair_error(&mut a, code, close_code);
    }
    // Nobody to forward to.
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(t, &slot).expect("open");
    expect_pair(&mut a, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    a.pair_msg(b"alone");
    expect_pair_error(&mut a, ErrorCode::BadRequest, close::BAD_REQUEST);
}

/// When one side goes, the Relay closes the other with 1000.
pub fn pair_pipe_closes_peer(t: &RelayTarget) {
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(t, &slot).expect("open a");
    expect_pair(&mut a, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    let mut b = RawConn::open_pair(t, &slot).expect("open b");
    expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
    expect_pair(&mut a, "pair.peer", |f| *f == PairRelayFrame::Peer);
    drop(a);
    assert_eq!(b.close_code(WAIT), Some(close::NORMAL));
}

/// A slot expires its lifetime after its first open: a socket still waiting gets
/// `pair_expired` and close 4008, and so does a later open. The target names a lifetime of
/// at most 5 s.
pub fn pair_pipe_expires(t: &RelayTarget) {
    let ttl = t
        .pair_ttl
        .expect("a target with a short pairing slot lifetime");
    assert!(ttl <= Duration::from_secs(5));
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(t, &slot).expect("open a");
    expect_pair(&mut a, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    match a.pair_frame(ttl + WAIT) {
        Some(PairRelayFrame::Error {
            code: ErrorCode::PairExpired,
            ..
        }) => {}
        other => panic!("expected pair_expired, got {other:?}"),
    }
    assert_eq!(a.close_code(WAIT), Some(close::PAIR_EXPIRED));
    let mut b = RawConn::open_pair(t, &slot).expect("open b");
    expect_pair_error(&mut b, ErrorCode::PairExpired, close::PAIR_EXPIRED);
}

/// A slot used before its expiry answers `pair_busy`, not `pair_expired`, after it: a used-up
/// slot's tombstone is checked before the lifetime. The target names a lifetime of at most
/// 5 s.
pub fn pair_pipe_used_slot_stays_busy(t: &RelayTarget) {
    let ttl = t
        .pair_ttl
        .expect("a target with a short pairing slot lifetime");
    assert!(ttl <= Duration::from_secs(5));
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(t, &slot).expect("open a");
    expect_pair(&mut a, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    let mut b = RawConn::open_pair(t, &slot).expect("open b");
    expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
    drop((a, b));
    std::thread::sleep(ttl + QUIET);
    let mut c = RawConn::open_pair(t, &slot).expect("open c");
    expect_pair_error(&mut c, ErrorCode::PairBusy, close::PAIR_BUSY);
}

/// Opens past the per-address limit are refused before the upgrade, with HTTP 429. The
/// target names a limit of at most 20, so the opens stay within the per-prefix slot cap.
pub fn pair_pipe_rate_limited(t: &RelayTarget) {
    let limit = t
        .pair_opens_per_minute
        .expect("a target with a pairing rate limit");
    assert!(limit as usize <= PAIR_MAX_SLOTS_PER_PREFIX);
    for _ in 0..limit {
        let mut c = RawConn::open_pair(t, &fresh_slot()).expect("open within the limit");
        expect_pair(&mut c, "pair.wait", |f| {
            matches!(f, PairRelayFrame::Wait { .. })
        });
    }
    match RawConn::open_pair(t, &fresh_slot()) {
        Err(RingError::Connect(m)) => assert!(m.contains("429"), "{m}"),
        Ok(_) => panic!("an open over the limit was upgraded"),
        Err(e) => panic!("expected HTTP 429, got {e}"),
    }
}

/// Client addresses in one address prefix for the slot cap scenarios: a fresh random IPv4 /24
/// (`10.x.y.0/24`) or IPv6 /64 (`fd…::/64`) when the target trusts a client address header,
/// so scenarios sharing a Relay do not meet in a prefix; else `None`, the runner's address.
struct Prefix {
    v6: bool,
    net: [u8; 6],
    header: bool,
}

impl Prefix {
    fn fresh(t: &RelayTarget, v6: bool) -> Prefix {
        let mut net = [0u8; 6];
        getrandom::getrandom(&mut net).expect("randomness");
        Prefix {
            v6,
            net,
            header: t.client_ip_header.is_some(),
        }
    }

    /// Host `i` in the prefix.
    fn addr(&self, i: u16) -> Option<String> {
        if !self.header {
            return None;
        }
        let n = &self.net;
        Some(if self.v6 {
            format!(
                "fd{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}00:{i:x}:{:x}::1",
                n[0],
                n[1],
                n[2],
                n[3],
                n[4],
                n[5],
                i.wrapping_mul(7919)
            )
        } else {
            format!("10.{}.{}.{}", n[0], n[1], 1 + i % 254)
        })
    }
}

fn expect_503(r: Result<RawConn, RingError>) {
    match r {
        Err(RingError::Connect(m)) => assert!(m.contains("503"), "{m}"),
        Ok(_) => panic!("an open over the slot cap was upgraded"),
        Err(e) => panic!("expected HTTP 503, got {e}"),
    }
}

/// Opens `n` new slots from `p` (host `i` opening the `i`-th), each waiting.
fn open_waiting(t: &RelayTarget, p: &Prefix, n: usize) -> Vec<(String, RawConn)> {
    (0..n)
        .map(|i| {
            let slot = fresh_slot();
            let mut c = RawConn::open_pair_from(t, &slot, p.addr(i as u16).as_deref())
                .unwrap_or_else(|e| panic!("open {} within the cap: {e}", i + 1));
            expect_pair(&mut c, "pair.wait", |f| {
                matches!(f, PairRelayFrame::Wait { .. })
            });
            (slot, c)
        })
        .collect()
}

/// At most 20 slots are outstanding per client address prefix: the 21st new slot from an IPv4
/// /24 (or one IPv6 /64) is refused before the upgrade with HTTP 503, while a slot already
/// outstanding still takes its second socket. With a client address header, the scenario
/// also checks that an IPv6 /64 is one prefix and that another /24 still opens. Without one,
/// it needs a Relay where nothing else from the runner's prefix is outstanding.
pub fn pair_prefix_cap_refuses_21st(t: &RelayTarget) {
    let cap = PAIR_MAX_SLOTS_PER_PREFIX;
    let p = Prefix::fresh(t, false);
    let held = open_waiting(t, &p, cap);
    let host = p.addr(cap as u16);
    expect_503(RawConn::open_pair_from(t, &fresh_slot(), host.as_deref()));
    // An outstanding slot is not new: its second socket joins.
    let mut b =
        RawConn::open_pair_from(t, &held[0].0, host.as_deref()).expect("open the second side");
    expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
    if t.client_ip_header.is_none() {
        return;
    }
    // Another /24 is another prefix.
    let other = Prefix::fresh(t, false);
    let _o = open_waiting(t, &other, 1);
    // One IPv6 /64 is one prefix, whatever the interface ids.
    let p6 = Prefix::fresh(t, true);
    let _held6 = open_waiting(t, &p6, cap);
    expect_503(RawConn::open_pair_from(
        t,
        &fresh_slot(),
        p6.addr(cap as u16).as_deref(),
    ));
}

/// A slot's reservation is released as soon as its two sides meet: with the prefix at the cap,
/// a meeting frees a place for a new slot at once, and a later open of the used-up slot still
/// gets `pair_busy` (its tombstone), not 503. Without a client address header, it needs a
/// Relay where nothing else from the runner's prefix is outstanding.
pub fn pair_reservation_released_on_meeting(t: &RelayTarget) {
    let cap = PAIR_MAX_SLOTS_PER_PREFIX;
    let p = Prefix::fresh(t, false);
    let mut held = open_waiting(t, &p, cap);
    let host = p.addr(cap as u16);
    expect_503(RawConn::open_pair_from(t, &fresh_slot(), host.as_deref()));
    let (slot, mut a) = held.remove(0);
    let mut b = RawConn::open_pair_from(t, &slot, host.as_deref()).expect("open the second side");
    expect_pair(&mut b, "pair.peer", |f| *f == PairRelayFrame::Peer);
    expect_pair(&mut a, "pair.peer", |f| *f == PairRelayFrame::Peer);
    // The meeting gave the reservation back: a new slot opens although the sockets stay.
    let mut c = RawConn::open_pair_from(t, &fresh_slot(), host.as_deref())
        .expect("a new slot after the meeting");
    expect_pair(&mut c, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    // The prefix is at the cap again, but the used-up slot is not new.
    drop((a, b));
    std::thread::sleep(QUIET);
    let mut d = RawConn::open_pair_from(t, &slot, host.as_deref()).expect("open the used slot");
    expect_pair_error(&mut d, ErrorCode::PairBusy, close::PAIR_BUSY);
}

/// A socket refused for a protocol violation burns its slot: with the prefix at the cap, a
/// `pair.msg` before the other side came (`bad_request`, close 4000) frees a place for a new
/// slot at once, and the burnt slot answers `pair_busy`. Without a client address header, it
/// needs a Relay where nothing else from the runner's prefix is outstanding.
pub fn pair_refusal_burns_slot(t: &RelayTarget) {
    let cap = PAIR_MAX_SLOTS_PER_PREFIX;
    let p = Prefix::fresh(t, false);
    let mut held = open_waiting(t, &p, cap);
    let host = p.addr(cap as u16);
    expect_503(RawConn::open_pair_from(t, &fresh_slot(), host.as_deref()));
    let (slot, mut a) = held.remove(0);
    a.pair_msg(b"alone");
    expect_pair_error(&mut a, ErrorCode::BadRequest, close::BAD_REQUEST);
    let mut c = RawConn::open_pair_from(t, &fresh_slot(), host.as_deref())
        .expect("a new slot after the refusal");
    expect_pair(&mut c, "pair.wait", |f| {
        matches!(f, PairRelayFrame::Wait { .. })
    });
    let mut d = RawConn::open_pair_from(t, &slot, host.as_deref()).expect("open the burnt slot");
    expect_pair_error(&mut d, ErrorCode::PairBusy, close::PAIR_BUSY);
}

/// Scenarios for a Relay with the default per-prefix slot cap and a raised rate limit (at
/// least 100 opens a minute). Without a client address header in the target, each needs a
/// Relay of its own.
pub const PAIR_CAP_SCENARIOS: &[(&str, Scenario)] = &[
    ("pair_prefix_cap_refuses_21st", pair_prefix_cap_refuses_21st),
    (
        "pair_reservation_released_on_meeting",
        pair_reservation_released_on_meeting,
    ),
    ("pair_refusal_burns_slot", pair_refusal_burns_slot),
];

/// Scenarios for a Relay with a short pairing slot lifetime; the target names it.
pub const PAIR_TTL_SCENARIOS: &[(&str, Scenario)] = &[
    ("pair_pipe_expires", pair_pipe_expires),
    (
        "pair_pipe_used_slot_stays_busy",
        pair_pipe_used_slot_stays_busy,
    ),
];

/// Scenarios for a Relay with a small pairing rate limit; the target names it.
pub const PAIR_RATE_SCENARIOS: &[(&str, Scenario)] =
    &[("pair_pipe_rate_limited", pair_pipe_rate_limited)];

/// Scenarios for a Relay with a small daily quota; the target must name it.
pub const QUOTA_SCENARIOS: &[(&str, Scenario)] =
    &[("quota_refuses_then_closes", quota_refuses_then_closes)];

/// Scenarios for a Relay that forwards pushes to the target's fake gateway. Run them one at a
/// time: they script the gateway's replies.
pub const PUSH_SCENARIOS: &[(&str, Scenario)] = &[
    (
        "push_forwards_exactly_once_verbatim",
        push_forwards_exactly_once_verbatim,
    ),
    (
        "push_refused_while_a_mobile_is_foreground",
        push_refused_while_a_mobile_is_foreground,
    ),
    ("push_refused_from_non_daemon", push_refused_from_non_daemon),
    (
        "push_relays_gateway_refusal_detail",
        push_relays_gateway_refusal_detail,
    ),
    ("push_bad_fields_refused", push_bad_fields_refused),
];

/// Scenarios for a Relay without a Push Gateway.
pub const PUSH_UNAVAILABLE_SCENARIOS: &[(&str, Scenario)] = &[(
    "push_unavailable_without_gateway",
    push_unavailable_without_gateway,
)];

/// Scenarios for a Relay with a short foreground lease; the target names it.
pub const FOREGROUND_LEASE_SCENARIOS: &[(&str, Scenario)] = &[(
    "state_foreground_lease_expires",
    state_foreground_lease_expires,
)];

/// Scenarios for a Hosted Relay; the target must carry the gateway key.
pub const HOSTED_SCENARIOS: &[(&str, Scenario)] = &[
    (
        "hosted_first_token_enables_routing",
        hosted_first_token_enables_routing,
    ),
    (
        "hosted_routing_ends_when_the_last_token_expires",
        hosted_routing_ends_when_the_last_token_expires,
    ),
];

/// A scenario against one Relay.
pub type Scenario = fn(&RelayTarget);

/// Every single-Relay scenario, by name.
pub const SCENARIOS: &[(&str, Scenario)] = &[
    ("member_auth_succeeds", member_auth_succeeds),
    ("non_member_auth_fails", non_member_auth_fails),
    ("forged_auth_signature_fails", forged_auth_signature_fails),
    ("failing_signer_fails_auth", failing_signer_fails_auth),
    (
        "signature_for_other_origin_fails",
        signature_for_other_origin_fails,
    ),
    ("genesis_upload_creates_ring", genesis_upload_creates_ring),
    (
        "genesis_from_underived_key_is_refused",
        genesis_from_underived_key_is_refused,
    ),
    ("non_member_cannot_seed_ring", non_member_cannot_seed_ring),
    (
        "member_added_later_supplies_extension",
        member_added_later_supplies_extension,
    ),
    (
        "envelope_reaches_only_addressee_in_same_ring",
        envelope_reaches_only_addressee_in_same_ring,
    ),
    ("envelope_from_is_stamped", envelope_from_is_stamped),
    ("oversize_envelope_is_refused", oversize_envelope_is_refused),
    (
        "oversize_or_malformed_control_frame_closes",
        oversize_or_malformed_control_frame_closes,
    ),
    (
        "envelope_to_offline_member_reports_offline",
        envelope_to_offline_member_reports_offline,
    ),
    ("presence_online", presence_online),
    ("bye_reports_xshell_closed", bye_reports_xshell_closed),
    (
        "drop_without_bye_reports_unreachable",
        drop_without_bye_reports_unreachable,
    ),
    (
        "unseen_member_is_never_connected",
        unseen_member_is_never_connected,
    ),
    ("client_lists_online_members", client_lists_online_members),
    (
        "roster_put_is_broadcast_and_accepted",
        roster_put_is_broadcast_and_accepted,
    ),
    (
        "relay_refuses_stale_or_mobile_signed_put",
        relay_refuses_stale_or_mobile_signed_put,
    ),
    (
        "removed_member_is_disconnected",
        removed_member_is_disconnected,
    ),
    (
        "removed_member_is_unknown_recipient",
        removed_member_is_unknown_recipient,
    ),
    (
        "auth_chain_signed_by_mobile_is_refused",
        auth_chain_signed_by_mobile_is_refused,
    ),
    (
        "client_syncs_newer_roster_on_welcome",
        client_syncs_newer_roster_on_welcome,
    ),
    ("entitlement_slot_round_trips", entitlement_slot_round_trips),
    ("auth_timeout_closes", auth_timeout_closes),
    (
        "replaced_socket_close_keeps_new_online",
        replaced_socket_close_keeps_new_online,
    ),
    (
        "stale_bye_after_replacement_is_ignored",
        stale_bye_after_replacement_is_ignored,
    ),
    (
        "unknown_client_type_is_answered_and_tolerated",
        unknown_client_type_is_answered_and_tolerated,
    ),
    ("binary_frame_is_unsupported", binary_frame_is_unsupported),
    (
        "readded_member_keeps_new_socket_online",
        readded_member_keeps_new_socket_online,
    ),
    ("pair_pipe_joins_two", pair_pipe_joins_two),
    ("pair_pipe_refuses_third", pair_pipe_refuses_third),
    ("pair_pipe_caps_messages", pair_pipe_caps_messages),
    ("pair_pipe_closes_peer", pair_pipe_closes_peer),
    (
        "state_foreground_reaches_presence",
        state_foreground_reaches_presence,
    ),
    (
        "state_foreground_clears_on_bye_and_drop",
        state_foreground_clears_on_bye_and_drop,
    ),
    (
        "state_refused_from_non_mobile",
        state_refused_from_non_mobile,
    ),
];

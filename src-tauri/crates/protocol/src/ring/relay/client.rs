//! The Ring client: one authenticated connection to the Ring's Relay. [`RingClient::connect`]
//! blocks through dialing, the challenge, authentication and Roster sync; after that one IO
//! thread owns the socket and delivers [`RingEvents`]. There is no automatic reconnect: the
//! caller's supervisor decides when to dial again.

use super::super::chain::RosterChain;
use super::super::url::RelayUrl;
use super::super::{b64, Member, RingError, RosterError, SignKey, Signature, SignedRoster, Signer};
use super::io::{self, Handler, IoConfig, Outbox};
use super::transport::{self, Conn};
use super::wire::{
    auth_message, check_payload, decode_relay, entitlement_well_formed, ByeReason, ClientFrame,
    CloseReason, ErrorCode, MemberPresence, Presence, RelayFrame, MAX_ENVELOPE_PAYLOAD, MAX_FRAME,
    RELAY_PROTOCOL,
};
use rustls::ClientConfig;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

#[derive(Debug, Clone, Copy)]
pub struct RingTimeouts {
    /// Dialing, TLS, the upgrade, authentication and the initial Roster sync, in all.
    pub connect: Duration,
    /// How often the client sends `{"t":"ping"}`.
    pub ping_interval: Duration,
    /// No complete frame from the Relay for this long means the connection is dead.
    pub dead_after: Duration,
    /// A `roster.put` or `entitlement.put` waits this long for its answer.
    pub request: Duration,
    /// `bye` waits this long for the Relay to close.
    pub bye: Duration,
}

impl Default for RingTimeouts {
    fn default() -> Self {
        RingTimeouts {
            connect: Duration::from_secs(15),
            ping_interval: Duration::from_secs(30),
            // Two missed pongs, plus slack for a slow network.
            dead_after: Duration::from_secs(75),
            request: Duration::from_secs(15),
            bye: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RingLimits {
    /// Bytes queued for the IO thread; `send` fails with `Backpressure` beyond.
    pub queue_bytes: usize,
    /// Bytes tungstenite may hold that the socket has not taken yet.
    pub write_buffer_bytes: usize,
}

impl Default for RingLimits {
    fn default() -> Self {
        RingLimits {
            queue_bytes: 4 * 1024 * 1024,
            write_buffer_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub struct RingClientConfig {
    /// The chain this device trusts; its head names the Relay.
    pub chain: RosterChain,
    pub signer: Arc<dyn Signer>,
    /// TLS for `wss://`; `None` means rustls with the Mozilla roots.
    pub tls: Option<Arc<ClientConfig>>,
    pub timeouts: RingTimeouts,
    pub limits: RingLimits,
}

impl RingClientConfig {
    pub fn new(chain: RosterChain, signer: Arc<dyn Signer>) -> Self {
        RingClientConfig {
            chain,
            signer,
            tls: None,
            timeouts: RingTimeouts::default(),
            limits: RingLimits::default(),
        }
    }
}

/// What the client delivers. Called on the IO thread in wire order (events of the initial
/// Roster sync on the connecting thread, before `connect` returns), never under a client
/// lock. Envelope senders and presence are Relay assertions until the Noise sessions (#9)
/// authenticate the peer.
pub trait RingEvents: Send + Sync {
    /// An envelope from a member of the trusted Roster.
    fn envelope(&self, from: SignKey, payload: Vec<u8>);
    fn presence(&self, key: SignKey, presence: MemberPresence);
    /// A new verified head.
    fn roster(&self, roster: &SignedRoster);
    /// The whole verified chain that `roster` just made the head of, called right after
    /// it: what a device persists, so a restart resumes from everything it verified.
    fn chain(&self, _chain: &RosterChain) {}
    /// The Relay sent a Roster version that failed verification; the trusted head stands.
    fn roster_rejected(&self, error: RosterError);
    /// An `error` the Relay sent that answers no request (`offline`, `unknown_recipient`, …).
    fn error(&self, _code: ErrorCode, _to: Option<SignKey>, _detail: Option<String>) {}
    fn entitlement(&self, _token: Option<&str>) {}
    /// Exactly once, last.
    fn closed(&self, why: CloseReason);
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberStatus {
    pub member: Member,
    pub presence: MemberPresence,
}

type Waiter = mpsc::SyncSender<Result<(), RingError>>;

/// A request awaiting its `ok` or `error`.
struct Pending {
    waiter: Waiter,
    /// For `roster.put`: the token to accept on `ok`, through the same path as broadcasts.
    accept: Option<String>,
}

struct State {
    chain: RosterChain,
    presence: HashMap<SignKey, Presence>,
    entitlement: Option<String>,
    limited: bool,
    waiting: HashMap<u64, Pending>,
    last_error: Option<ErrorCode>,
    closed: bool,
}

/// A connected Ring client. Dropping it drops the connection without a goodbye (the Relay
/// then reports this device unreachable); [`RingClient::bye`] says goodbye first.
pub struct RingClient {
    me: SignKey,
    state: Arc<Mutex<State>>,
    outbox: Outbox,
    next_id: Arc<AtomicU64>,
    timeouts: RingTimeouts,
}

fn lock(s: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// Blocking frame IO for the connect phase; the socket's deadline bounds every call.
struct Setup {
    ws: WebSocket<Conn>,
    last_error: Option<(ErrorCode, Option<String>)>,
}

impl Setup {
    fn send(&mut self, text: String) -> Result<(), RingError> {
        let mut r = self.ws.send(Message::text(text));
        loop {
            match r {
                Ok(()) => return Ok(()),
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    r = self.ws.flush();
                }
                Err(e) => return Err(self.fail(e)),
            }
        }
    }

    fn fail(&self, e: tungstenite::Error) -> RingError {
        match e {
            tungstenite::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => {
                RingError::Timeout
            }
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                self.closed(None)
            }
            other => RingError::Connect(other.to_string()),
        }
    }

    fn closed(&self, code: Option<u16>) -> RingError {
        match &self.last_error {
            Some((code, detail)) => RingError::Relay {
                code: code.clone(),
                detail: detail.clone(),
            },
            None => RingError::Closed(CloseReason::Relay {
                close_code: code,
                error: None,
            }),
        }
    }

    /// The next text frame.
    fn recv(&mut self) -> Result<String, RingError> {
        loop {
            match self.ws.read() {
                Ok(Message::Text(t)) => return Ok(t.as_str().to_string()),
                Ok(Message::Close(f)) => return Err(self.closed(f.map(|f| u16::from(f.code)))),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue
                }
                Err(e) => return Err(self.fail(e)),
            }
        }
    }

    /// The next decodable known frame of the current phase; unknown and undecodable frames
    /// are skipped, an `error` without an id ends the connect, and a frame that cannot
    /// occur in this phase is a protocol error. After `welcome`, session traffic (other
    /// members' presence, envelopes, Roster and entitlement broadcasts) is kept, bounded,
    /// for the IO thread.
    fn recv_frame(&mut self, early: &mut Early) -> Result<RelayFrame, RingError> {
        loop {
            let text = self.recv()?;
            let frame = match decode_relay(&text) {
                Ok(RelayFrame::Pong) | Ok(RelayFrame::Unknown { .. }) | Err(_) => continue,
                Ok(f) => f,
            };
            match frame {
                RelayFrame::Error {
                    code,
                    id: None,
                    detail,
                    to: None,
                } => {
                    self.last_error = Some((code.clone(), detail.clone()));
                    return Err(RingError::Relay { code, detail });
                }
                f @ (RelayFrame::Challenge { .. } | RelayFrame::Welcome { .. })
                    if !early.welcomed =>
                {
                    return Ok(f)
                }
                f @ (RelayFrame::RosterChain { .. } | RelayFrame::Error { .. })
                    if early.welcomed =>
                {
                    return Ok(f)
                }
                RelayFrame::Env { .. }
                | RelayFrame::Presence(_)
                | RelayFrame::Roster { .. }
                | RelayFrame::Entitlement { .. }
                    if early.welcomed =>
                {
                    early.push(text)?
                }
                other => {
                    return Err(RingError::Protocol(format!(
                        "unexpected {} during connect",
                        frame_type(&other)
                    )))
                }
            }
        }
    }
}

/// Session frames received while the connect finishes, replayed on the IO thread.
#[derive(Default)]
struct Early {
    welcomed: bool,
    frames: Vec<String>,
    bytes: usize,
}

/// Bounds on [`Early`]: what a Relay may send between `welcome` and the end of the sync.
const MAX_EARLY_FRAMES: usize = 256;
const MAX_EARLY_BYTES: usize = 1024 * 1024;

impl Early {
    fn push(&mut self, text: String) -> Result<(), RingError> {
        if self.frames.len() >= MAX_EARLY_FRAMES || self.bytes + text.len() > MAX_EARLY_BYTES {
            return Err(RingError::Protocol(
                "too much session traffic before the connect finished".into(),
            ));
        }
        self.bytes += text.len();
        self.frames.push(text);
        Ok(())
    }
}

fn frame_type(f: &RelayFrame) -> String {
    let json = f.encode();
    serde_json::from_str::<serde_json::Value>(&json)
        .ok()
        .and_then(|v| v.get("t").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// Packs tokens into frames of at most `MAX_FRAME` bytes each.
fn pack_chain(tokens: &[&str], wrap: impl Fn(Vec<String>) -> String) -> Vec<String> {
    let mut frames = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut size = 64;
    for t in tokens {
        let n = t.len() + 3;
        if !cur.is_empty() && size + n > MAX_FRAME {
            frames.push(wrap(std::mem::take(&mut cur)));
            size = 64;
        }
        cur.push(t.to_string());
        size += n;
    }
    if !cur.is_empty() {
        frames.push(wrap(cur));
    }
    frames
}

/// The Roster version a device was told it was added in (by the Desktop, through the
/// pairing handshake): the chain it fetches must hold exactly this version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainPin {
    pub version: u64,
    /// `b64u(SHA-256(token))`.
    pub hash: String,
}

/// Dials `url`'s Ring endpoint for `ring` and answers the challenge as `signer`, staging
/// what `stage` returns for the Relay's head version first. Returns the connection after
/// `welcome`.
#[allow(clippy::too_many_arguments)] // one private call site per dial mode
fn authenticate(
    url: &RelayUrl,
    ring: &super::super::RingId,
    signer: &dyn Signer,
    stage: &dyn Fn(u64) -> Vec<String>,
    tls: Option<Arc<ClientConfig>>,
    deadline: Instant,
    write_buffer: usize,
    early: &mut Early,
) -> Result<(Setup, Welcomed), RingError> {
    let me = signer.sign_key();
    let ws = transport::dial(url, &url.ring_endpoint(ring), tls, deadline, write_buffer)?;
    let mut s = Setup {
        ws,
        last_error: None,
    };
    let (nonce, relay_version) = match s.recv_frame(early)? {
        RelayFrame::Challenge {
            v,
            nonce,
            roster_version,
            ..
        } => {
            if v != RELAY_PROTOCOL {
                return Err(RingError::Protocol(format!("relay protocol {v}")));
            }
            if b64::decode_array::<32>(&nonce).is_err() {
                return Err(RingError::Protocol("bad challenge nonce".into()));
            }
            (nonce, roster_version)
        }
        other => {
            return Err(RingError::Protocol(format!(
                "expected challenge, got {other:?}"
            )))
        }
    };
    // Stage what the Relay lacks, then answer the challenge.
    let staged = stage(relay_version);
    if !staged.is_empty() {
        let tokens: Vec<&str> = staged.iter().map(String::as_str).collect();
        for f in pack_chain(&tokens, |rosters| {
            ClientFrame::AuthChain { rosters }.encode()
        }) {
            s.send(f)?;
        }
    }
    let msg = auth_message(&url.origin(), ring, &nonce, &me);
    let sig = Signature::from_bytes(signer.sign(&msg)?);
    s.send(
        ClientFrame::Auth {
            sign_key: me,
            sig,
            caps: Vec::new(),
        }
        .encode(),
    )?;
    let w = match s.recv_frame(early)? {
        RelayFrame::Welcome {
            you,
            roster_version,
            presence,
            entitlement,
            limited,
            ..
        } => {
            if you != me {
                return Err(RingError::Protocol("welcome for another key".into()));
            }
            Welcomed {
                presence,
                entitlement,
                limited,
                relay_head: roster_version,
            }
        }
        RelayFrame::Error { code, detail, .. } => return Err(RingError::Relay { code, detail }),
        other => {
            return Err(RingError::Protocol(format!(
                "expected welcome, got {other:?}"
            )))
        }
    };
    early.welcomed = true;
    Ok((s, w))
}

struct Welcomed {
    presence: Vec<Presence>,
    entitlement: Option<String>,
    limited: bool,
    relay_head: u64,
}

/// One `roster.get` round: the versions after `since` and whether more remain.
fn roster_get(
    s: &mut Setup,
    early: &mut Early,
    id: u64,
    since: u64,
) -> Result<(Vec<String>, bool), RingError> {
    s.send(ClientFrame::RosterGet { id, since }.encode())?;
    loop {
        match s.recv_frame(early)? {
            RelayFrame::RosterChain {
                id: got,
                rosters,
                more,
            } if got == id => return Ok((rosters, more)),
            RelayFrame::Error {
                id: Some(got),
                code,
                detail,
                ..
            } if got == id => return Err(RingError::Relay { code, detail }),
            _ => continue,
        }
    }
}

impl RingClient {
    /// A device that was just paired has no chain yet: it authenticates as a member of the
    /// Relay's head (without staging anything), reads the whole chain, and trusts it only if
    /// it verifies from genesis for `ring`, holds exactly the pinned version (so a Relay
    /// that hides or forks it fails), and lists this device in its head. The connection is
    /// closed afterwards; [`RingClient::connect`] keeps its own check that a device is in its
    /// chain.
    pub fn fetch_chain(
        relay_url: &str,
        ring: &super::super::RingId,
        signer: Arc<dyn Signer>,
        pin: &ChainPin,
        tls: Option<Arc<ClientConfig>>,
        timeouts: RingTimeouts,
    ) -> Result<RosterChain, RingError> {
        use super::super::chain::MAX_CHAIN_LEN;
        let deadline = Instant::now() + timeouts.connect;
        let url = RelayUrl::parse(relay_url).map_err(|e| RingError::Invalid(e.to_string()))?;
        let mut early = Early::default();
        let (mut s, _) = authenticate(
            &url,
            ring,
            &*signer,
            &|_| Vec::new(),
            tls,
            deadline,
            RingLimits::default().write_buffer_bytes,
            &mut early,
        )?;
        let mut versions: Vec<SignedRoster> = Vec::new();
        let mut id = 0;
        loop {
            id += 1;
            let since = versions.last().map(|r| r.version()).unwrap_or(0);
            let (rosters, more) = roster_get(&mut s, &mut early, id, since)?;
            if rosters.is_empty() {
                break;
            }
            for t in &rosters {
                versions.push(SignedRoster::parse(t)?);
            }
            if versions.len() > MAX_CHAIN_LEN {
                return Err(RingError::Roster(RosterError::TooLarge));
            }
            // Early session traffic is of no use here; keep the buffer bounded.
            early.frames.clear();
            early.bytes = 0;
            if !more {
                break;
            }
        }
        let _ = s.ws.close(None);
        let _ = s.ws.flush();
        let chain = RosterChain::from_chain(versions)?;
        if chain.ring_id() != ring {
            return Err(RingError::Roster(RosterError::RingMismatch));
        }
        match chain.get(pin.version) {
            Some(r) if r.hash() == pin.hash => {}
            _ => {
                return Err(RingError::Invalid(
                    "the relay's roster does not hold the version this device was added in".into(),
                ))
            }
        }
        if chain.head().member(&signer.sign_key()).is_none() {
            return Err(RingError::Invalid(
                "this device is not in the relay's roster".into(),
            ));
        }
        Ok(chain)
    }

    /// Dials the Relay named by the trusted head, authenticates, and syncs the Roster: a
    /// Relay behind gets the missing versions staged before `auth`, a Relay ahead is asked
    /// for its newer versions, each verified before it is trusted.
    pub fn connect(
        cfg: RingClientConfig,
        events: Arc<dyn RingEvents>,
    ) -> Result<RingClient, RingError> {
        let deadline = Instant::now() + cfg.timeouts.connect;
        let mut chain = cfg.chain.clone();
        let me = cfg.signer.sign_key();
        if chain.head().member(&me).is_none() {
            return Err(RingError::Invalid(
                "this device is not in its Roster".into(),
            ));
        }
        let url = RelayUrl::parse(&chain.head().roster().relay_url)
            .map_err(|e| RingError::Invalid(e.to_string()))?;
        let ring = chain.ring_id().clone();
        let mut early = Early::default();
        let stage = |relay_version: u64| -> Vec<String> {
            if relay_version < chain.head().version() {
                chain
                    .since(relay_version)
                    .iter()
                    .map(|r| r.token().to_string())
                    .collect()
            } else {
                Vec::new()
            }
        };
        let (mut s, w) = authenticate(
            &url,
            &ring,
            &*cfg.signer,
            &stage,
            cfg.tls.clone(),
            deadline,
            cfg.limits.write_buffer_bytes,
            &mut early,
        )?;
        let Welcomed {
            presence,
            entitlement,
            limited,
            relay_head,
        } = w;

        // Catch up with a Relay that is ahead, verifying every version.
        let mut id = 0u64;
        while chain.head().version() < relay_head {
            id += 1;
            let since = chain.head().version();
            let (rosters, more) = roster_get(&mut s, &mut early, id, since)?;
            let parsed: Result<Vec<SignedRoster>, RosterError> =
                rosters.iter().map(|t| SignedRoster::parse(t)).collect();
            match parsed.and_then(|p| chain.accept(&p)) {
                Ok(a) if a.added > 0 => {}
                Ok(_) => break,
                Err(e) => {
                    events.roster_rejected(e);
                    break;
                }
            }
            if !more {
                break;
            }
        }
        if chain.head().version() > cfg.chain.head().version() {
            events.roster(chain.head());
            events.chain(&chain);
        }

        let presence = presence
            .into_iter()
            .filter(|p| chain.head().member(&p.sign_key).is_some())
            .map(|p| (p.sign_key, p))
            .collect();
        let state = Arc::new(Mutex::new(State {
            chain,
            presence,
            entitlement,
            limited,
            waiting: HashMap::new(),
            last_error: None,
            closed: false,
        }));
        let next_id = Arc::new(AtomicU64::new(id + 1));
        s.ws.get_mut()
            .set_nonblocking()
            .map_err(|e| RingError::Connect(e.to_string()))?;
        let handler_state = state.clone();
        let handler_id = next_id.clone();
        let io_cfg = IoConfig {
            ping_interval: cfg.timeouts.ping_interval,
            dead_after: cfg.timeouts.dead_after,
            bye_timeout: cfg.timeouts.bye,
            queue_bytes: cfg.limits.queue_bytes,
        };
        let outbox = io::spawn(s.ws, io_cfg, move |outbox| {
            Box::new(ClientHandler {
                me,
                state: handler_state,
                events,
                outbox,
                next_id: handler_id,
                early: early.frames,
            })
        })
        .map_err(|e| RingError::Connect(e.to_string()))?;
        Ok(RingClient {
            me,
            state,
            outbox,
            next_id,
            timeouts: cfg.timeouts,
        })
    }

    pub fn me(&self) -> SignKey {
        self.me
    }

    /// Every member of the trusted head, with what the Relay says about it.
    pub fn members(&self) -> Vec<MemberStatus> {
        let st = lock(&self.state);
        st.chain
            .head()
            .roster()
            .members
            .iter()
            .map(|m| MemberStatus {
                member: m.clone(),
                presence: if m.sign_key == self.me && !st.closed {
                    MemberPresence::Online { since: None }
                } else {
                    st.presence
                        .get(&m.sign_key)
                        .map(MemberPresence::from)
                        .unwrap_or(MemberPresence::NeverConnected)
                },
            })
            .collect()
    }

    /// The other members the Relay reports online.
    pub fn online_members(&self) -> Vec<MemberStatus> {
        self.members()
            .into_iter()
            .filter(|m| m.member.sign_key != self.me)
            .filter(|m| matches!(m.presence, MemberPresence::Online { .. }))
            .collect()
    }

    pub fn roster(&self) -> SignedRoster {
        lock(&self.state).chain.head().clone()
    }

    pub fn chain(&self) -> RosterChain {
        lock(&self.state).chain.clone()
    }

    pub fn entitlement(&self) -> Option<String> {
        lock(&self.state).entitlement.clone()
    }

    /// On a Hosted Relay: whether this Ring's session is limited (no envelope routing)
    /// because the Relay holds no valid Hosted entitlement. Set from `welcome`, updated by
    /// entitlement broadcasts and by `entitlement_required` refusals.
    pub fn limited(&self) -> bool {
        lock(&self.state).limited
    }

    pub fn is_closed(&self) -> bool {
        self.outbox.is_closed()
    }

    /// Sends one envelope (at most 64 KiB) to `to`. Delivery failures come back as
    /// [`RingEvents::error`].
    pub fn send(&self, to: &SignKey, payload: &[u8]) -> Result<(), RingError> {
        if payload.len() > MAX_ENVELOPE_PAYLOAD {
            return Err(RingError::Invalid("envelope payload over 64 KiB".into()));
        }
        if to == &self.me {
            return Err(RingError::Invalid("an envelope to oneself".into()));
        }
        self.outbox.send_text(
            ClientFrame::Env {
                to: *to,
                payload: b64::encode(payload),
            }
            .encode(),
        )
    }

    fn request(
        &self,
        accept: Option<String>,
        frame: impl FnOnce(u64) -> ClientFrame,
    ) -> Result<(), RingError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut st = lock(&self.state);
            if st.closed {
                return Err(RingError::Closed(CloseReason::Local));
            }
            st.waiting.insert(id, Pending { waiter: tx, accept });
        }
        if let Err(e) = self.outbox.send_text(frame(id).encode()) {
            lock(&self.state).waiting.remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(self.timeouts.request) {
            Ok(r) => r,
            Err(_) => {
                lock(&self.state).waiting.remove(&id);
                Err(RingError::Timeout)
            }
        }
    }

    /// Uploads the next Roster version and waits for the Relay to accept it. It must extend
    /// this client's trusted head.
    pub fn publish_roster(&self, next: &SignedRoster) -> Result<(), RingError> {
        lock(&self.state)
            .chain
            .extended(std::slice::from_ref(next))?;
        // On `ok` the IO thread accepts the version through the same path as the Relay's
        // broadcast, so `RingEvents::roster` fires exactly once whichever arrives first.
        self.request(Some(next.token().to_string()), |id| {
            ClientFrame::RosterPut {
                id,
                roster: next.token().to_string(),
            }
        })
    }

    /// Stores a Push Gateway entitlement token in the Ring's slot on the Relay.
    pub fn put_entitlement(&self, token: &str) -> Result<(), RingError> {
        if !entitlement_well_formed(token) {
            return Err(RingError::Invalid("not an entitlement token".into()));
        }
        self.request(None, |id| ClientFrame::EntitlementPut {
            id,
            token: token.to_string(),
        })
    }

    /// Says goodbye with `reason` and waits (at most the `bye` timeout) for the Relay to
    /// close, so it reports this device "xshell closed" rather than unreachable.
    pub fn bye(self, reason: ByeReason) -> Result<(), RingError> {
        self.goodbye(reason)
    }

    /// [`RingClient::bye`] for a shared client: the connection ends either way, and a second
    /// goodbye is refused.
    pub fn goodbye(&self, reason: ByeReason) -> Result<(), RingError> {
        let rx = self.outbox.bye(reason)?;
        // The IO thread enforces the deadline; the margin covers scheduling.
        match rx.recv_timeout(self.timeouts.bye + Duration::from_millis(500)) {
            Ok(r) => r,
            Err(_) => Err(RingError::Timeout),
        }
    }
}

impl Drop for RingClient {
    fn drop(&mut self) {
        if !self.outbox.is_closed() {
            self.outbox.shutdown();
        }
    }
}

struct ClientHandler {
    me: SignKey,
    state: Arc<Mutex<State>>,
    events: Arc<dyn RingEvents>,
    outbox: Outbox,
    next_id: Arc<AtomicU64>,
    early: Vec<String>,
}

enum Acceptance {
    /// Appended; `roster` fired.
    New,
    /// Already held byte for byte; nothing fired.
    Known,
    /// Skips versions; the caller asks for them.
    Gap,
    /// Refused; `roster_rejected` fired.
    Rejected(RosterError),
}

impl ClientHandler {
    /// The one place Roster versions from the Relay become trusted.
    fn accept(&mut self, rosters: &[String]) -> Acceptance {
        let parsed: Result<Vec<SignedRoster>, RosterError> =
            rosters.iter().map(|t| SignedRoster::parse(t)).collect();
        let result = {
            let mut st = lock(&self.state);
            match parsed {
                Err(e) => Err(e),
                Ok(p) => {
                    let known = p.iter().all(|r| {
                        st.chain
                            .get(r.version())
                            .is_some_and(|k| k.token() == r.token())
                    });
                    if known {
                        return Acceptance::Known;
                    }
                    st.chain
                        .accept(&p)
                        .map(|_| (st.chain.head().clone(), st.chain.clone()))
                }
            }
        };
        match result {
            Ok((head, chain)) => {
                self.events.roster(&head);
                self.events.chain(&chain);
                Acceptance::New
            }
            Err(RosterError::Gap) => Acceptance::Gap,
            Err(e) => {
                self.events.roster_rejected(e.clone());
                Acceptance::Rejected(e)
            }
        }
    }

    fn roster_get(&self, since: u64) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .outbox
            .send_text(ClientFrame::RosterGet { id, since }.encode());
    }
}

impl Handler for ClientHandler {
    fn start(&mut self) {
        for t in std::mem::take(&mut self.early) {
            self.text(&t);
        }
    }

    fn text(&mut self, text: &str) {
        let Ok(frame) = decode_relay(text) else {
            return;
        };
        match frame {
            RelayFrame::Env { from, payload } => {
                if check_payload(&payload).is_err() {
                    return;
                }
                let member = lock(&self.state).chain.head().member(&from).is_some();
                if !member || from == self.me {
                    return;
                }
                if let Ok(bytes) = b64::decode(&payload) {
                    self.events.envelope(from, bytes);
                }
            }
            RelayFrame::Presence(p) => {
                let key = p.sign_key;
                if key == self.me {
                    return;
                }
                {
                    let mut st = lock(&self.state);
                    if st.chain.head().member(&key).is_none() {
                        return;
                    }
                    st.presence.insert(key, p.clone());
                }
                self.events.presence(key, MemberPresence::from(&p));
            }
            RelayFrame::Roster { roster } => {
                if let Acceptance::Gap = self.accept(&[roster]) {
                    let since = lock(&self.state).chain.head().version();
                    self.roster_get(since);
                }
            }
            RelayFrame::RosterChain { rosters, more, .. } => {
                let gap = matches!(self.accept(&rosters), Acceptance::Gap);
                if more || gap {
                    let since = lock(&self.state).chain.head().version();
                    self.roster_get(since);
                }
            }
            RelayFrame::Ok { id } => {
                let Some(p) = lock(&self.state).waiting.remove(&id) else {
                    return;
                };
                let r = match p.accept {
                    Some(token) => match self.accept(&[token]) {
                        Acceptance::New | Acceptance::Known => Ok(()),
                        Acceptance::Gap => Err(RingError::Roster(RosterError::Gap)),
                        Acceptance::Rejected(e) => Err(RingError::Roster(e)),
                    },
                    None => Ok(()),
                };
                let _ = p.waiter.send(r);
            }
            RelayFrame::Error {
                code,
                id,
                to,
                detail,
            } => {
                let waiter = id.and_then(|id| lock(&self.state).waiting.remove(&id));
                match waiter {
                    Some(p) => {
                        let _ = p.waiter.send(Err(RingError::Relay { code, detail }));
                    }
                    None => {
                        let mut st = lock(&self.state);
                        if code == ErrorCode::EntitlementRequired {
                            st.limited = true;
                        }
                        st.last_error = Some(code.clone());
                        drop(st);
                        self.events.error(code, to, detail);
                    }
                }
            }
            RelayFrame::Entitlement { token, limited } => {
                if token
                    .as_deref()
                    .is_some_and(|t| !entitlement_well_formed(t))
                {
                    return;
                }
                {
                    let mut st = lock(&self.state);
                    st.entitlement = token.clone();
                    st.limited = limited;
                }
                self.events.entitlement(token.as_deref());
            }
            RelayFrame::Challenge { .. }
            | RelayFrame::Welcome { .. }
            | RelayFrame::Pong
            | RelayFrame::Unknown { .. } => {}
        }
    }

    fn closed(&mut self, why: CloseReason) {
        let why = match why {
            CloseReason::Relay {
                close_code,
                error: None,
            } => CloseReason::Relay {
                close_code,
                error: lock(&self.state).last_error.clone(),
            },
            w => w,
        };
        let waiting: Vec<Waiter> = {
            let mut st = lock(&self.state);
            st.closed = true;
            st.waiting.drain().map(|(_, p)| p.waiter).collect()
        };
        for w in waiting {
            let _ = w.send(Err(RingError::Closed(why.clone())));
        }
        self.events.closed(why);
    }
}

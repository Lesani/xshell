//! An in-process test Relay (test support only). It implements the protocol in the module
//! docs with this crate's own chain checks, deep enough to test the Ring client without
//! workerd: one thread per connection, polling a short read timeout and an outbox. It also
//! plays a hostile Relay on request: injected frames, a peer that stops reading, endless
//! WebSocket fragments, and (through [`TrickleProxy`]) TLS records that arrive a few bytes at
//! a time.

use super::super::chain::{RosterChain, MAX_CHAIN_LEN};
use super::super::entitlement::{verify_entitlement, GatewayKeys};
use super::super::pairing::valid_slot;
use super::super::push::{blob_well_formed, collapse_id_well_formed, sealed_well_formed};
use super::super::roster::{Role, RosterError, SignedRoster};
use super::super::url::RelayUrl;
use super::super::{verify, RingId, SignKey, Signature};
use super::contract::RelayTarget;
use super::wire::{
    auth_message, check_payload, close, decode_client, entitlement_well_formed, new_nonce,
    ClientFrame, ErrorCode, Presence, RelayFrame, WireError, DROPPED, HOSTED_QUOTA_FRAMES_PER_DAY,
    MAX_CANDIDATE_BYTES, MAX_FRAME, MAX_STAGE_BYTES, MAX_STAGE_CHUNK, MAX_STAGE_CHUNKS, PING, PONG,
    QUOTA_REFUSALS_BEFORE_CLOSE, RELAY_PROTOCOL,
};
use super::wire::{
    decode_pair_client, PairClientFrame, PairRelayFrame, MAX_PAIR_FRAME, MAX_PAIR_MSGS,
    PAIR_MAX_SLOTS, PAIR_MAX_SLOTS_PER_PREFIX, PAIR_SLOT_TTL, PAIR_TOMBSTONE,
};
use super::wire::{
    CAP_FOREGROUND, CAP_PUSH, FOREGROUND_LEASE, FOREGROUND_RENEW_BEFORE, PUSH_GATEWAY_TIMEOUT,
    PUSH_MAX_IN_FLIGHT,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::collections::VecDeque;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::frame::coding::{CloseCode, Data, OpCode};
use tungstenite::protocol::frame::{CloseFrame, Frame};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

/// A certificate and its PKCS#8 key, both DER, for a TLS test Relay.
#[derive(Clone)]
pub struct TestTls {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

#[derive(Clone)]
pub struct TestRelayOptions {
    /// The origin the Relay expects in auth messages, instead of its own.
    pub origin_override: Option<String>,
    pub auth_timeout: Duration,
    /// Serve `wss://` with this certificate.
    pub tls: Option<TestTls>,
    /// Act as the Hosted Relay: route only for Rings holding a valid Hosted entitlement
    /// signed by one of these gateway keys; others get limited sessions.
    pub hosted: Option<GatewayKeys>,
    /// Deliver the `roster` broadcast to the uploader before its `ok` (default: after).
    pub broadcast_before_ok: bool,
    /// Caps on a staged candidate's physical chunks and serialized bytes (defaults: the
    /// protocol's).
    pub max_stage_chunks: usize,
    pub max_stage_bytes: usize,
    /// A daily quota of authenticated client frames per Ring (section 14 of the protocol).
    /// `None`: the protocol's default, `HOSTED_QUOTA_FRAMES_PER_DAY` on a Hosted Relay and no
    /// quota on any other.
    pub quota_frames_per_day: Option<u64>,
    /// Pairing pipe: ignore single use and expiry (a slot stays usable for any number of
    /// meetings), to prove the endpoints enforce them themselves.
    pub lax_pairing: bool,
    /// Pairing pipe: a slot's lifetime after its first open (default 600 s).
    pub pair_ttl: Duration,
    /// Pairing pipe: slot opens per client address per minute before HTTP 429 (`None`: no
    /// limit; a Worker applies `wire::PAIR_OPENS_PER_MINUTE`).
    pub pair_opens_per_minute: Option<u32>,
    /// Pairing pipe: slots outstanding at once before HTTP 503.
    pub pair_max_slots: usize,
    /// Pairing pipe: slots outstanding per client address prefix (an IPv4 /24, an IPv6 /64)
    /// before HTTP 503.
    pub pair_max_slots_per_prefix: usize,
    /// Pairing pipe: how long a used-up or expired slot keeps its tombstone.
    pub pair_tombstone: Duration,
    /// Pairing pipe: take the client address from this request header, as set by a trusted
    /// reverse proxy (its last list entry), instead of the socket's peer address. An open
    /// without a usable value counts against one shared bucket (section 16).
    pub client_ip_header: Option<String>,
    /// Forward `push` frames to this Push Gateway (`http://host:port`, plain HTTP only) and
    /// list the `push` cap. `None`: no cap, and `push` is refused as `unavailable`.
    pub push_gateway: Option<String>,
    /// The deadline for the gateway's answer (default 15 s).
    pub push_timeout: Duration,
    /// How long a Mobile counts as foreground after its last frame or ping (default 75 s).
    pub foreground_lease: Duration,
}

impl Default for TestRelayOptions {
    fn default() -> Self {
        TestRelayOptions {
            origin_override: None,
            auth_timeout: Duration::from_secs(10),
            tls: None,
            hosted: None,
            broadcast_before_ok: false,
            max_stage_chunks: MAX_STAGE_CHUNKS,
            max_stage_bytes: MAX_STAGE_BYTES,
            quota_frames_per_day: None,
            lax_pairing: false,
            pair_ttl: PAIR_SLOT_TTL,
            pair_opens_per_minute: None,
            pair_max_slots: PAIR_MAX_SLOTS,
            pair_max_slots_per_prefix: PAIR_MAX_SLOTS_PER_PREFIX,
            pair_tombstone: PAIR_TOMBSTONE,
            client_ip_header: None,
            push_gateway: None,
            push_timeout: PUSH_GATEWAY_TIMEOUT,
            foreground_lease: FOREGROUND_LEASE,
        }
    }
}

/// What a [`Tamper`] does with one envelope.
pub enum Verdict {
    /// Deliver these payloads instead (none: drop it; several: duplicate or inject).
    Deliver(Vec<Vec<u8>>),
    /// Refuse it to the sender with this code (as `error{code, to}`), delivering nothing.
    Refuse(ErrorCode),
}

/// Rewrites envelopes to one member: called with the sender and the decoded payload.
pub type Tamper = Box<dyn FnMut(&SignKey, Vec<u8>) -> Verdict + Send>;

/// A hostile behaviour for one connection.
#[derive(Clone, Copy, Debug)]
pub enum Fault {
    /// Stop reading from and writing to the socket.
    StopReading,
    /// Start a text message and never finish it: one continuation fragment of `chunk` bytes
    /// every `every`. Reading goes on.
    EndlessFragments { chunk: usize, every: Duration },
    /// Drop broadcasts and envelopes for this socket (a Relay withholding them); direct
    /// answers and injected frames still go out.
    Mute,
    /// Start a text message and send empty continuation fragments as fast as the socket
    /// takes them, without pause. Reading goes on.
    ZeroFragmentFlood,
    /// Stop reading, but keep sending `pong` every 50 ms, so the peer never looks dead.
    PongsOnly,
    /// TLS only: take the connection over and send valid TLS records without pause: 32 empty
    /// application-data records (rustls refuses a 33rd in a row), then one carrying a single
    /// byte of an endless run of empty WebSocket continuation frames, and again. Afterwards
    /// it only counts what the client sends (`raw_rx_bytes`).
    EmptyTlsRecords,
}

enum Cmd {
    Text(String),
    Inject(String),
    Close(u16),
    Fault(Fault),
}

struct ConnRef {
    gen: u64,
    tx: Sender<Cmd>,
}

struct Rec {
    online: bool,
    last_seen: Option<u64>,
    last_reason: Option<String>,
    /// The generation of the socket this record describes.
    gen: u64,
    /// A Mobile's `state`, and the lease the last broadcast announced (unix ms).
    foreground: bool,
    foreground_until: Option<u64>,
}

struct RingState {
    chain: RosterChain,
    presence: HashMap<SignKey, Rec>,
    conns: HashMap<SignKey, ConnRef>,
    entitlement: Option<String>,
    pings: HashMap<SignKey, u64>,
    /// The last generation handed out per key. Never reset, not even when a member is
    /// removed, so a re-added member's new socket cannot share an old socket's generation.
    gens: HashMap<SignKey, u64>,
    /// The quota counter: the UTC day (days since the epoch) and the frames counted on it.
    quota: (u64, u64),
    /// When each key's current socket last sent a frame or a ping (the Worker reads
    /// `getWebSocketAutoResponseTimestamp` for pings).
    activity: HashMap<SignKey, Instant>,
    /// Pushes being forwarded to the gateway now.
    pushes_in_flight: usize,
}

impl RingState {
    fn presence_of(&self, key: &SignKey) -> Presence {
        match self.presence.get(key) {
            Some(r) => Presence {
                sign_key: *key,
                online: r.online,
                last_seen: r.last_seen,
                last_reason: r.last_reason.clone(),
                foreground: r.online && r.foreground,
                foreground_until: if r.online && r.foreground {
                    r.foreground_until
                } else {
                    None
                },
            },
            None => Presence {
                sign_key: *key,
                online: false,
                last_seen: None,
                last_reason: None,
                foreground: false,
                foreground_until: None,
            },
        }
    }

    /// Unix ms when `key`'s foreground lease runs out, counted from its socket's last frame
    /// or ping; `None` once it has.
    fn lease_until(&self, key: &SignKey, lease: Duration) -> Option<u64> {
        let since = self.activity.get(key)?.elapsed();
        let left = lease.checked_sub(since).filter(|d| !d.is_zero())?;
        Some(now_ms() + left.as_millis() as u64)
    }

    /// Whether any Mobile is in the foreground now: its `state` says so and its lease runs.
    fn any_foreground(&self, lease: Duration) -> bool {
        self.presence
            .iter()
            .any(|(k, r)| r.online && r.foreground && self.lease_until(k, lease).is_some())
    }

    /// Renews or ends the foreground leases (the Worker's alarm): a lease close to running
    /// out is extended to the socket's latest activity, and one that ran out clears the
    /// flag. Either is broadcast like any presence change.
    fn sweep_foreground(&mut self, lease: Duration) {
        let renew = FOREGROUND_RENEW_BEFORE.min(lease / 3);
        let now = now_ms();
        let mut changed = Vec::new();
        let keys: Vec<SignKey> = self.presence.keys().copied().collect();
        for k in keys {
            let until = self.lease_until(&k, lease);
            let Some(r) = self.presence.get_mut(&k) else {
                continue;
            };
            if !(r.online && r.foreground) {
                continue;
            }
            match until {
                None => {
                    r.foreground = false;
                    r.foreground_until = None;
                    changed.push(k);
                }
                Some(u) => {
                    let old = r.foreground_until.unwrap_or(0);
                    if u > old && old.saturating_sub(now) < renew.as_millis() as u64 {
                        r.foreground_until = Some(u);
                        changed.push(k);
                    }
                }
            }
        }
        for k in changed {
            let p = RelayFrame::Presence(self.presence_of(&k)).encode();
            self.broadcast(Some(&k), &p);
        }
    }

    fn send(&self, key: &SignKey, cmd: Cmd) {
        if let Some(c) = self.conns.get(key) {
            let _ = c.tx.send(cmd);
        }
    }

    fn broadcast(&self, except: Option<&SignKey>, text: &str) {
        for (k, c) in &self.conns {
            if Some(k) != except {
                let _ = c.tx.send(Cmd::Text(text.to_string()));
            }
        }
    }

    /// A terminal transition for `key`'s socket of generation `gen`; ignored for an old
    /// generation or a socket already offline.
    fn offline(&mut self, key: &SignKey, gen: u64, reason: &str) {
        let Some(r) = self.presence.get_mut(key) else {
            return;
        };
        if r.gen != gen || !r.online {
            return;
        }
        r.online = false;
        r.last_seen = Some(now());
        r.last_reason = Some(reason.to_string());
        r.foreground = false;
        r.foreground_until = None;
        let p = RelayFrame::Presence(self.presence_of(key)).encode();
        self.broadcast(Some(key), &p);
    }

    /// Whether `key`'s socket of generation `gen` may still act: its key is in the head and
    /// it is the key's current socket.
    fn current(&self, key: &SignKey, gen: u64) -> Result<(), ErrorCode> {
        if self.chain.head().member(key).is_none() {
            return Err(ErrorCode::Removed);
        }
        if self.conns.get(key).map(|c| c.gen) != Some(gen) {
            return Err(ErrorCode::Replaced);
        }
        Ok(())
    }

    /// After the chain grew: tell everyone, and disconnect members the head dropped.
    fn announce(&mut self, added: &[SignedRoster], removed: &[SignKey], except: Option<&SignKey>) {
        for r in added {
            self.broadcast(
                except,
                &RelayFrame::Roster {
                    roster: r.token().to_string(),
                }
                .encode(),
            );
        }
        for k in removed {
            self.send(k, Cmd::Text(RelayFrame::error(ErrorCode::Removed).encode()));
            self.send(k, Cmd::Close(close::REMOVED));
            self.conns.remove(k);
            self.presence.remove(k);
        }
    }
}

struct Shared {
    rings: Mutex<HashMap<RingId, RingState>>,
    /// Staged `auth.chain` frames, as a Worker keeps them in Durable Object storage:
    /// `stage/<ringId>/<candidateId>/<n>` → (stored at, JSON array of tokens).
    stage: Mutex<BTreeMap<String, (Instant, String)>>,
    stop: AtomicBool,
    /// Bytes read from sockets taken over by Fault::EmptyTlsRecords.
    raw_rx: std::sync::atomic::AtomicUsize,
    /// Set once Fault::EmptyTlsRecords starts writing.
    flooding: AtomicBool,
    /// Answer every `roster.put` with `error{code:"internal"}` (see `refuse_roster_puts`).
    refuse_puts: AtomicBool,
    /// Drop every `roster.put` without an answer (see `ignore_roster_puts`).
    ignore_puts: AtomicBool,
    /// `roster.put`s answered `roster_stale` (see `stale_roster_puts`).
    stale_puts: std::sync::atomic::AtomicUsize,
    /// Envelope rewriters by (Ring, addressee).
    tampers: Mutex<HashMap<(RingId, SignKey), Tamper>>,
    /// Every envelope payload routed, decoded, while recording is on.
    recording: AtomicBool,
    recorded: Mutex<Vec<Vec<u8>>>,
    /// Every `env` a client sent while recording is on, before any check (refused ones
    /// included): (sender, addressee, decoded payload).
    attempts: Mutex<Vec<(SignKey, SignKey, Vec<u8>)>>,
    /// The pairing pipe's slots and the per-address open log.
    pair: Mutex<PairState>,
    /// `push` frames received (any outcome).
    push_frames: std::sync::atomic::AtomicUsize,
    opts: TestRelayOptions,
    origin: Mutex<String>,
    tls: Option<Arc<ServerConfig>>,
}

impl Shared {
    /// Routes one envelope to `tx`, through the addressee's tamper hook if there is one.
    /// `Some(code)`: refused to the sender.
    fn route_env(
        &self,
        ring: &RingId,
        from: &SignKey,
        to: &SignKey,
        payload: String,
        tx: &Sender<Cmd>,
    ) -> Option<ErrorCode> {
        let bytes = || super::super::b64::decode(&payload).unwrap_or_default();
        if self.recording.load(Ordering::Acquire) {
            self.recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(bytes());
        }
        let mut tampers = self.tampers.lock().unwrap_or_else(|e| e.into_inner());
        let out = match tampers.get_mut(&(ring.clone(), *to)) {
            None => vec![payload],
            Some(f) => match f(from, bytes()) {
                Verdict::Refuse(code) => return Some(code),
                Verdict::Deliver(v) => v.iter().map(|p| super::super::b64::encode(p)).collect(),
            },
        };
        drop(tampers);
        for payload in out {
            let env = RelayFrame::Env {
                from: *from,
                payload,
            }
            .encode();
            let _ = tx.send(Cmd::Text(env));
        }
        None
    }

    /// Staged chunks live at most this long: the auth deadline plus a margin (the Worker's
    /// alarm does the same).
    fn stage_ttl(&self) -> Duration {
        self.opts.auth_timeout + Duration::from_secs(1)
    }

    fn stage_sweep(&self) {
        let ttl = self.stage_ttl();
        self.stage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (at, _)| at.elapsed() < ttl);
    }

    /// The quota in effect: the configured one, else the Hosted default, else none.
    fn quota(&self) -> Option<u64> {
        self.opts.quota_frames_per_day.or(self
            .opts
            .hosted
            .as_ref()
            .map(|_| HOSTED_QUOTA_FRAMES_PER_DAY))
    }

    /// Whether `ring` may route envelopes: always, unless this is a Hosted Relay without a
    /// valid Hosted entitlement for it.
    fn entitled(&self, ring: &RingId, s: &RingState) -> bool {
        match &self.opts.hosted {
            None => true,
            Some(keys) => s
                .entitlement
                .as_deref()
                .is_some_and(|t| verify_entitlement(t, ring, now(), keys, true).is_ok()),
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Shared {
    /// What `challenge.caps` and `welcome.caps` list.
    fn caps(&self) -> Vec<String> {
        let mut c = vec![CAP_FOREGROUND.to_string()];
        if self.opts.push_gateway.is_some() {
            c.push(CAP_PUSH.to_string());
        }
        c
    }
}

/// `POST {gateway}/v1/push` with `body`, waiting at most `timeout`: `Ok` on 200, else the
/// `detail` the Relay reports (section 18).
fn forward_push(gateway: &str, body: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let unreachable = || "unreachable".to_string();
    let rest = gateway.strip_prefix("http://").ok_or_else(unreachable)?;
    let (host, base) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{}", p.trim_end_matches('/'))),
        None => (rest, String::new()),
    };
    let addr: SocketAddr = std::net::ToSocketAddrs::to_socket_addrs(host)
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(unreachable)?;
    let left = || {
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1))
    };
    let mut tcp = TcpStream::connect_timeout(&addr, left()).map_err(|_| unreachable())?;
    let _ = tcp.set_write_timeout(Some(left()));
    write!(
        tcp,
        "POST {base}/v1/push HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|_| unreachable())?;
    let mut resp = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(unreachable());
        }
        let _ = tcp.set_read_timeout(Some(left()));
        match tcp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(unreachable()),
        }
        if resp.len() > 64 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&resp);
    let status: u16 = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(unreachable)?;
    if status == 200 {
        return Ok(());
    }
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_string))
        .filter(|c| {
            (1..=32).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        });
    Err(code.unwrap_or_else(|| "gateway".into()))
}

pub struct TestRelay {
    shared: Arc<Shared>,
    addr: SocketAddr,
}

impl TestRelay {
    pub fn start() -> TestRelay {
        Self::start_with(TestRelayOptions::default())
    }

    pub fn start_with(opts: TestRelayOptions) -> TestRelay {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test relay");
        let addr = listener.local_addr().expect("test relay address");
        let scheme = if opts.tls.is_some() { "wss" } else { "ws" };
        let own = format!("{scheme}://127.0.0.1:{}", addr.port());
        let origin = opts.origin_override.clone().unwrap_or_else(|| {
            RelayUrl::parse(&own)
                .map(|u| u.origin())
                .unwrap_or_else(|_| own.clone())
        });
        let tls = opts.tls.as_ref().map(|t| {
            // 256-bit TLS 1.3 suites only: Fault::EmptyTlsRecords rebuilds the extracted key.
            let mut provider = rustls::crypto::ring::default_provider();
            provider.cipher_suites = vec![
                rustls::crypto::ring::cipher_suite::TLS13_AES_256_GCM_SHA384,
                rustls::crypto::ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
            ];
            let cfg = ServerConfig::builder_with_provider(Arc::new(provider))
                .with_safe_default_protocol_versions()
                .expect("tls versions")
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(t.cert_der.clone())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(t.key_der.clone())),
                )
                .expect("test certificate");
            let mut cfg = cfg;
            // For Fault::EmptyTlsRecords, which writes records itself.
            cfg.enable_secret_extraction = true;
            Arc::new(cfg)
        });
        let shared = Arc::new(Shared {
            rings: Mutex::new(HashMap::new()),
            stage: Mutex::new(BTreeMap::new()),
            stop: AtomicBool::new(false),
            raw_rx: std::sync::atomic::AtomicUsize::new(0),
            flooding: AtomicBool::new(false),
            refuse_puts: AtomicBool::new(false),
            ignore_puts: AtomicBool::new(false),
            stale_puts: std::sync::atomic::AtomicUsize::new(0),
            tampers: Mutex::new(HashMap::new()),
            recording: AtomicBool::new(false),
            recorded: Mutex::new(Vec::new()),
            attempts: Mutex::new(Vec::new()),
            pair: Mutex::new(PairState::default()),
            push_frames: std::sync::atomic::AtomicUsize::new(0),
            opts,
            origin: Mutex::new(origin),
            tls,
        });
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let s = shared.clone();
        thread::spawn(move || {
            // The foreground lease sweep (a Worker's alarm).
            let lease = s.opts.foreground_lease;
            let tick = (lease / 50).clamp(Duration::from_millis(5), Duration::from_millis(500));
            while !s.stop.load(Ordering::Acquire) {
                thread::sleep(tick);
                for r in s
                    .rings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values_mut()
                {
                    r.sweep_foreground(lease);
                }
            }
        });
        let s = shared.clone();
        thread::spawn(move || {
            while !s.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((tcp, _)) => {
                        let s = s.clone();
                        thread::spawn(move || {
                            let _ = serve(s, tcp);
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        TestRelay { shared, addr }
    }

    pub fn url(&self) -> String {
        let scheme = if self.shared.tls.is_some() {
            "wss"
        } else {
            "ws"
        };
        format!("{scheme}://127.0.0.1:{}", self.addr.port())
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The origin this Relay expects devices to sign.
    pub fn origin(&self) -> String {
        self.shared
            .origin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Changes the origin this Relay expects, e.g. to a proxy's in front of it.
    pub fn set_origin(&self, origin: &str) {
        *self.shared.origin.lock().unwrap_or_else(|e| e.into_inner()) = origin.to_string();
    }

    pub fn target(&self) -> RelayTarget {
        RelayTarget {
            url: self.url(),
            tls: None,
            auth_timeout: self.shared.opts.auth_timeout,
            gateway: None,
            fake_gateway: None,
            foreground_lease: (self.shared.opts.foreground_lease != FOREGROUND_LEASE)
                .then_some(self.shared.opts.foreground_lease),
            quota_frames_per_day: self.shared.quota(),
            pair_opens_per_minute: self.shared.opts.pair_opens_per_minute,
            pair_ttl: (self.shared.opts.pair_ttl != PAIR_SLOT_TTL)
                .then_some(self.shared.opts.pair_ttl),
            client_ip_header: self.shared.opts.client_ip_header.clone(),
        }
    }

    /// Rewrites every envelope routed to `to` in `ring` (see [`Tamper`]); `None` stops.
    pub fn tamper(&self, ring: &RingId, to: &SignKey, f: Option<Tamper>) {
        let mut t = self
            .shared
            .tampers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match f {
            Some(f) => {
                t.insert((ring.clone(), *to), f);
            }
            None => {
                t.remove(&(ring.clone(), *to));
            }
        }
    }

    /// Starts (or stops) recording every envelope payload routed.
    pub fn record_payloads(&self, on: bool) {
        self.shared.recording.store(on, Ordering::Release);
    }

    /// The payloads recorded so far, decoded.
    pub fn recorded_payloads(&self) -> Vec<Vec<u8>> {
        self.shared
            .recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Every `env` sent to this Relay while recording was on, before any check, refused
    /// ones included (an envelope to a non-member, say): (sender, addressee, decoded payload).
    pub fn recorded_attempts(&self) -> Vec<(SignKey, SignKey, Vec<u8>)> {
        self.shared
            .attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// How many pairing slots are outstanding (hold a reservation against the caps).
    pub fn pair_slots(&self) -> usize {
        let mut p = self.shared.pair.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.pair_sweep(&mut p);
        p.slots
            .values()
            .filter(|s| self.shared.pair_outstanding(s))
            .count()
    }

    /// How many pairing slots the Relay holds any state for: outstanding slots, slots with a
    /// socket or an open in progress, and tombstones.
    pub fn pair_slots_held(&self) -> usize {
        let mut p = self.shared.pair.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.pair_sweep(&mut p);
        p.slots.len()
    }

    fn rings(&self) -> std::sync::MutexGuard<'_, HashMap<RingId, RingState>> {
        self.shared.rings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends `raw` as a text frame to `key`'s socket; false when it has none.
    pub fn inject(&self, ring: &RingId, key: &SignKey, raw: &str) -> bool {
        self.command(ring, key, Cmd::Inject(raw.to_string()))
    }

    /// Makes `key`'s socket misbehave.
    pub fn fault(&self, ring: &RingId, key: &SignKey, fault: Fault) -> bool {
        self.command(ring, key, Cmd::Fault(fault))
    }

    /// Closes `key`'s socket with `code` and no error frame, as a Relay that lost the
    /// socket would; false when it has none.
    pub fn kick(&self, ring: &RingId, key: &SignKey, code: u16) -> bool {
        self.command(ring, key, Cmd::Close(code))
    }

    /// While on, every `roster.put` fails with `error{code:"internal"}` and stores nothing.
    pub fn refuse_roster_puts(&self, on: bool) {
        self.shared.refuse_puts.store(on, Ordering::Release);
    }

    /// While on, every `roster.put` is dropped unanswered (a Relay that withholds its
    /// acknowledgements); envelopes are still routed.
    pub fn ignore_roster_puts(&self, on: bool) {
        self.shared.ignore_puts.store(on, Ordering::Release);
    }

    /// How many `roster.put`s were answered `roster_stale` so far.
    pub fn stale_roster_puts(&self) -> usize {
        self.shared.stale_puts.load(Ordering::Acquire)
    }

    fn command(&self, ring: &RingId, key: &SignKey, cmd: Cmd) -> bool {
        let rings = self.rings();
        let Some(c) = rings.get(ring).and_then(|r| r.conns.get(key)) else {
            return false;
        };
        c.tx.send(cmd).is_ok()
    }

    /// The stored head version of `ring`, if the Relay knows it.
    pub fn head_version(&self, ring: &RingId) -> Option<u64> {
        self.rings().get(ring).map(|r| r.chain.head().version())
    }

    /// How many pings `key` sent.
    pub fn pings(&self, ring: &RingId, key: &SignKey) -> u64 {
        self.rings()
            .get(ring)
            .and_then(|r| r.pings.get(key).copied())
            .unwrap_or(0)
    }

    pub fn presence(&self, ring: &RingId, key: &SignKey) -> Option<Presence> {
        self.rings().get(ring).map(|r| r.presence_of(key))
    }

    /// How many `push` frames the Relay received, whatever became of them.
    pub fn push_frames(&self) -> usize {
        self.shared.push_frames.load(Ordering::Acquire)
    }

    /// Bytes the client sent into connections taken over by `Fault::EmptyTlsRecords`.
    pub fn raw_rx_bytes(&self) -> usize {
        self.shared.raw_rx.load(Ordering::Relaxed)
    }

    /// Whether a connection taken over by `Fault::EmptyTlsRecords` is flooding yet.
    pub fn flooding(&self) -> bool {
        self.shared.flooding.load(Ordering::Acquire)
    }

    /// Loses one staged chunk (the `n`th in key order), leaving a gap in the indices.
    pub fn drop_staged_chunk(&self, n: usize) {
        let mut stage = self.shared.stage.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = stage.keys().nth(n).cloned() {
            stage.remove(&k);
        }
    }

    /// Loses every staged chunk, as storage that expired or was evicted would.
    pub fn drop_staged(&self) {
        self.shared
            .stage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// How many staged `auth.chain` chunks the Relay holds, committed or not yet swept.
    pub fn staged_chunks(&self) -> usize {
        self.shared.stage_sweep();
        self.shared
            .stage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

/// The server side of a connection, with or without TLS.
enum Io {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

/// The server side of a connection, with or without TLS. `replay` holds bytes already read
/// (the HTTP request head, read before the WebSocket handshake) and is read first.
struct SConn {
    io: Io,
    replay: Vec<u8>,
}

impl SConn {
    fn tcp(&self) -> &TcpStream {
        match &self.io {
            Io::Plain(s) => s,
            Io::Tls(t) => t.get_ref(),
        }
    }
}

impl Read for SConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.replay.is_empty() {
            let n = buf.len().min(self.replay.len());
            buf[..n].copy_from_slice(&self.replay[..n]);
            self.replay.drain(..n);
            return Ok(n);
        }
        match &mut self.io {
            Io::Plain(s) => s.read(buf),
            Io::Tls(t) => t.read(buf),
        }
    }
}

impl Write for SConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.io {
            Io::Plain(s) => s.write(buf),
            Io::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.io {
            Io::Plain(s) => s.flush(),
            Io::Tls(t) => t.flush(),
        }
    }
}

/// What a request head asks for, before any WebSocket handshake (section 6).
#[derive(Debug, PartialEq)]
enum Route {
    Health,
    Ring,
    Pair(String),
    UpgradeRequired,
    NotFound,
}

fn route(head: &str) -> Route {
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    let upgrade = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("upgrade") && v.trim().eq_ignore_ascii_case("websocket")
        })
    });
    if method == "GET" && path == "/healthz" {
        Route::Health
    } else if let Some(slot) = slot_of_path(path) {
        if upgrade {
            Route::Pair(slot)
        } else {
            Route::UpgradeRequired
        }
    } else if ring_of_path(path).is_none() {
        Route::NotFound
    } else if upgrade {
        Route::Ring
    } else {
        Route::UpgradeRequired
    }
}

/// Reads the request head (up to the blank line), or `None` if it does not come in time.
fn read_head(stream: &mut SConn, deadline: Instant) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if Instant::now() >= deadline || head.len() > 64 * 1024 {
            return None;
        }
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => head.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
    Some(head)
}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

enum Phase {
    /// What a Worker keeps in the socket's attachment before `auth`; the staged tokens
    /// themselves are in the stage store under `candidate_id`.
    Pre {
        nonce: String,
        candidate_id: String,
        manifest: Manifest,
        deadline: Instant,
    },
    Authed {
        key: SignKey,
        gen: u64,
        bye_seen: bool,
    },
    Closing {
        until: Instant,
    },
}

struct Conn {
    ws: WebSocket<SConn>,
    shared: Arc<Shared>,
    ring: RingId,
    phase: Phase,
    /// The authenticated key and generation, kept through `Closing`.
    authed: Option<(SignKey, u64, bool)>,
    tx: Sender<Cmd>,
    rx: Receiver<Cmd>,
    fault: Option<Fault>,
    muted: bool,
    next_fragment: Instant,
    fragments_started: bool,
    /// Fault::EmptyTlsRecords: hand the connection to `empty_record_flood` after `run`.
    take_over: bool,
    /// Quota refusals in a row on this socket (a Worker keeps it in the attachment).
    quota_refusals: usize,
}

/// A staged candidate's manifest, kept in the attachment: what reconstruction must find.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Manifest {
    /// `auth.chain` frames received.
    frames: usize,
    /// Physical chunks stored, indexed 0..chunks.
    chunks: usize,
    /// Tokens, and their bytes, in all.
    versions: usize,
    bytes: usize,
    /// Serialized chunk bytes in all.
    stored: usize,
}

/// Splits one frame's tokens into serialized chunks of at most `MAX_STAGE_CHUNK` bytes.
fn chunk_tokens(tokens: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut size = 2;
    for t in tokens {
        let n = t.len() + 3;
        if !cur.is_empty() && size + n > MAX_STAGE_CHUNK {
            out.push(serde_json::to_string(&cur).unwrap_or_default());
            cur.clear();
            size = 2;
        }
        cur.push(t);
        size += n;
    }
    if !cur.is_empty() {
        out.push(serde_json::to_string(&cur).unwrap_or_default());
    }
    out
}

/// The Ring a request path names: `…/v1/ring/{ringId}` as a path suffix, so a Relay URL may
/// carry a path (section 6).
fn ring_of_path(path: &str) -> Option<RingId> {
    let (_, id) = path.rsplit_once("/v1/ring/")?;
    RingId::parse(id).ok()
}

/// The slot a request path names: `…/v1/pair/{slot}` as a path suffix (section 16).
fn slot_of_path(path: &str) -> Option<String> {
    let (_, slot) = path.rsplit_once("/v1/pair/")?;
    valid_slot(slot).then(|| slot.to_string())
}

fn stage_prefix(ring: &RingId, candidate: &str) -> String {
    format!("stage/{ring}/{candidate}/")
}

fn serve(shared: Arc<Shared>, tcp: TcpStream) -> Result<(), ()> {
    tcp.set_nonblocking(false).map_err(|_| ())?;
    tcp.set_read_timeout(Some(Duration::from_millis(5)))
        .map_err(|_| ())?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| ())?;
    let _ = tcp.set_nodelay(true);
    let io = match &shared.tls {
        Some(cfg) => {
            let c = ServerConnection::new(cfg.clone()).map_err(|_| ())?;
            Io::Tls(Box::new(StreamOwned::new(c, tcp)))
        }
        None => Io::Plain(tcp),
    };
    let mut stream = SConn {
        io,
        replay: Vec::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let head = read_head(&mut stream, deadline).ok_or(())?;
    let peer_ip = stream.tcp().peer_addr().map(|a| a.ip()).ok();
    let head_text = String::from_utf8_lossy(&head);
    let route = route(&head_text);
    let status = match &route {
        Route::Ring => None,
        Route::Pair(slot) => {
            let ip = match &shared.opts.client_ip_header {
                Some(name) => header_address(&head_text, name),
                None => peer_ip,
            };
            shared.pair_admit(ClientAddr::of(ip), slot)
        }
        Route::Health => Some("200 OK"),
        Route::UpgradeRequired => Some("426 Upgrade Required"),
        Route::NotFound => Some("404 Not Found"),
    };
    if let Some(status) = status {
        let body = if status.starts_with("200") { "ok" } else { "" };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.flush();
        return Ok(());
    }
    // An admitted pairing open holds a slot reservation until its socket takes it over.
    let mut reservation = match &route {
        Route::Pair(slot) => Some(Reservation(Some((shared.clone(), slot.clone())))),
        _ => None,
    };
    stream.replay = head;
    let path = Arc::new(Mutex::new(String::new()));
    let p = path.clone();
    #[allow(clippy::result_large_err)] // the shape tungstenite's callback requires
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let path = req.uri().path();
        if ring_of_path(path).is_none() && slot_of_path(path).is_none() {
            let mut not_found = ErrorResponse::new(None);
            *not_found.status_mut() = tungstenite::http::StatusCode::NOT_FOUND;
            return Err(not_found);
        }
        *p.lock().unwrap_or_else(|e| e.into_inner()) = path.to_string();
        Ok(resp)
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(2 * MAX_FRAME))
        .max_frame_size(Some(2 * MAX_FRAME));
    let mut r = tungstenite::accept_hdr_with_config(stream, callback, Some(config));
    let ws = loop {
        match r {
            Ok(ws) => break ws,
            Err(HandshakeError::Interrupted(mid)) if Instant::now() < deadline => {
                r = mid.handshake()
            }
            Err(_) => {
                // Give the reservation back before the client sees the connection close.
                drop(reservation.take());
                return Err(());
            }
        }
    };
    let path = path.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(mut r) = reservation.take() {
        // The socket takes the reservation over in `pair_serve`.
        r.0 = None;
        let slot = slot_of_path(&path).ok_or(())?;
        pair_serve(shared, ws, slot);
        return Ok(());
    }
    let ring = ring_of_path(&path).ok_or(())?;
    let (tx, rx) = mpsc::channel();
    shared.stage_sweep();
    let mut conn = Conn {
        ws,
        phase: Phase::Pre {
            nonce: new_nonce().map_err(|_| ())?,
            candidate_id: new_nonce().map_err(|_| ())?[..22].to_string(),
            manifest: Manifest::default(),
            deadline: Instant::now() + shared.opts.auth_timeout,
        },
        shared,
        ring,
        authed: None,
        tx,
        rx,
        fault: None,
        muted: false,
        next_fragment: Instant::now(),
        fragments_started: false,
        take_over: false,
        quota_refusals: 0,
    };
    conn.run();
    conn.ended();
    if conn.take_over {
        let shared = conn.shared.clone();
        empty_record_flood(conn.ws, &shared);
    }
    Ok(())
}

/// Fault::EmptyTlsRecords: writes TLS 1.3 records with the extracted traffic keys.
fn empty_record_flood(ws: WebSocket<SConn>, shared: &Shared) {
    use rustls::crypto::cipher::{OutboundChunks, OutboundPlainMessage};
    use rustls::{ConnectionTrafficSecrets, ContentType, ProtocolVersion, SupportedCipherSuite};
    let Io::Tls(tls) = ws.into_inner().io else {
        return;
    };
    let StreamOwned { conn, sock } = *tls;
    let Some(SupportedCipherSuite::Tls13(suite)) = conn.negotiated_cipher_suite() else {
        return;
    };
    let Ok(secrets) = conn.dangerous_extract_secrets() else {
        return;
    };
    let (seq, keys) = secrets.tx;
    let (key, iv) = match keys {
        ConnectionTrafficSecrets::Aes128Gcm { key, iv }
        | ConnectionTrafficSecrets::Aes256Gcm { key, iv }
        | ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } => (key, iv),
        _ => return,
    };
    // Record i carries one zero byte when i % 33 == 32, else nothing; record 0 starts the
    // endless message.
    let encode = |enc: &mut Box<dyn rustls::crypto::cipher::MessageEncrypter>, i: u64| {
        let payload: &[u8] = if i == 0 {
            &[0x01, 0x00]
        } else if i % 33 == 32 {
            &[0x00]
        } else {
            &[]
        };
        let msg = OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version: ProtocolVersion::TLSv1_2,
            payload: OutboundChunks::Single(payload),
        };
        enc.encrypt(msg, seq + i)
            .map(|m| m.encode())
            .unwrap_or_default()
    };
    // The test Relay offers only 256-bit suites, so the key copies into an `AeadKey`.
    let Ok(key_bytes) = <[u8; 32]>::try_from(key.as_ref()) else {
        return;
    };
    // Encrypt a long run up front, in parallel, so the flood outpaces the reader's
    // decryption: writing it is then only copying.
    const AHEAD: u64 = 4_000_000;
    const THREADS: u64 = 4;
    let parts: Vec<Vec<u8>> = thread::scope(|sc| {
        let handles: Vec<_> = (0..THREADS)
            .map(|p| {
                let key = rustls::crypto::cipher::AeadKey::from(key_bytes);
                let iv = rustls::crypto::cipher::Iv::copy(iv.as_ref());
                let encode = &encode;
                sc.spawn(move || {
                    let mut enc = suite.aead_alg.encrypter(key, iv);
                    let per = AHEAD / THREADS;
                    let mut out = Vec::with_capacity(per as usize * 22);
                    for i in p * per..(p + 1) * per {
                        out.extend(encode(&mut enc, i));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    let mut enc = suite.aead_alg.encrypter(key, iv);
    let mut next = AHEAD;
    let _ = sock.set_read_timeout(Some(Duration::from_micros(50)));
    let mut sock = sock;
    let mut buf = vec![0u8; 64 * 1024];
    let mut drain = |sock: &mut TcpStream| -> bool {
        while let Ok(n) = sock.read(&mut buf) {
            if n == 0 {
                return false;
            }
            shared.raw_rx.fetch_add(n, Ordering::Relaxed);
        }
        true
    };
    shared.flooding.store(true, Ordering::Release);
    for part in &parts {
        for slice in part.chunks(1024 * 1024) {
            if shared.stop.load(Ordering::Acquire)
                || sock.write_all(slice).is_err()
                || !drain(&mut sock)
            {
                return;
            }
        }
    }
    drop(parts);
    while !shared.stop.load(Ordering::Acquire) {
        let mut batch = Vec::with_capacity(64 * 1024);
        for _ in 0..2048 {
            batch.extend(encode(&mut enc, next));
            next += 1;
        }
        if sock.write_all(&batch).is_err() || !drain(&mut sock) {
            return;
        }
    }
}

impl Conn {
    fn rings(&self) -> std::sync::MutexGuard<'_, HashMap<RingId, RingState>> {
        self.shared.rings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&mut self, text: &str) -> bool {
        // A faulty socket writes nothing whole (a text frame would end the endless message).
        if matches!(self.phase, Phase::Closing { .. }) || self.fault.is_some() {
            return true;
        }
        match self.ws.send(Message::text(text)) {
            Ok(()) => true,
            Err(e) if would_block(&e) => self.ws.flush().is_ok(),
            Err(_) => false,
        }
    }

    fn error(
        &mut self,
        code: ErrorCode,
        id: Option<u64>,
        to: Option<SignKey>,
        detail: Option<&str>,
    ) -> bool {
        let f = RelayFrame::Error {
            code,
            id,
            to,
            detail: detail.map(str::to_string),
        };
        self.write(&f.encode())
    }

    fn close(&mut self, code: u16) {
        if matches!(self.phase, Phase::Closing { .. }) {
            return;
        }
        if let Phase::Authed { key, gen, bye_seen } = self.phase {
            self.authed = Some((key, gen, bye_seen));
        }
        let _ = self.ws.close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        }));
        let _ = self.ws.flush();
        self.phase = Phase::Closing {
            until: Instant::now() + Duration::from_secs(1),
        };
    }

    fn run(&mut self) {
        let challenge_version = self
            .rings()
            .get(&self.ring)
            .map(|r| r.chain.head().version())
            .unwrap_or(0);
        let Phase::Pre { nonce, .. } = &self.phase else {
            return;
        };
        let challenge = RelayFrame::Challenge {
            v: RELAY_PROTOCOL,
            nonce: nonce.clone(),
            roster_version: challenge_version,
            caps: self.shared.caps(),
        }
        .encode();
        if !self.write(&challenge) {
            return;
        }
        loop {
            if self.shared.stop.load(Ordering::Acquire) {
                return;
            }
            while let Ok(cmd) = self.rx.try_recv() {
                match cmd {
                    Cmd::Text(t) if self.muted => drop(t),
                    Cmd::Text(t) | Cmd::Inject(t) => {
                        if !self.write(&t) {
                            return;
                        }
                    }
                    Cmd::Close(code) => self.close(code),
                    Cmd::Fault(Fault::Mute) => self.muted = true,
                    Cmd::Fault(f) => {
                        if let Fault::ZeroFragmentFlood = f {
                            let _ = self
                                .ws
                                .get_ref()
                                .tcp()
                                .set_read_timeout(Some(Duration::from_micros(50)));
                        }
                        if let Fault::EmptyTlsRecords = f {
                            self.take_over = true;
                            let _ = self.ws.flush();
                            return;
                        }
                        self.fault = Some(f)
                    }
                }
            }
            match self.fault {
                Some(Fault::StopReading) => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Some(Fault::PongsOnly) => {
                    thread::sleep(Duration::from_millis(50));
                    if self.ws.send(Message::text(PONG)).is_err() {
                        return;
                    }
                    continue;
                }
                Some(Fault::ZeroFragmentFlood) => {
                    // Raw frame bytes, so the flood outpaces any parser: a non-final empty text
                    // frame once, then empty non-final continuation frames (opcode 0, no
                    // payload, unmasked as a server's are), 64 KiB per write.
                    let mut buf = Vec::with_capacity(64 * 1024 + 2);
                    if !self.fragments_started {
                        self.fragments_started = true;
                        buf.extend_from_slice(&[0x01, 0x00]);
                    }
                    buf.resize(buf.len() + 64 * 1024, 0);
                    if self.ws.get_mut().write_all(&buf).is_err()
                        || self.ws.get_mut().flush().is_err()
                    {
                        return;
                    }
                }
                Some(Fault::EndlessFragments { chunk, every })
                    if Instant::now() >= self.next_fragment =>
                {
                    self.next_fragment = Instant::now() + every;
                    let op = if self.fragments_started {
                        Data::Continue
                    } else {
                        Data::Text
                    };
                    self.fragments_started = true;
                    let f = Frame::message(vec![b'x'; chunk], OpCode::Data(op), false);
                    if self.ws.send(Message::Frame(f)).is_err() {
                        return;
                    }
                }
                _ => {}
            }
            match &self.phase {
                Phase::Pre { deadline, .. } if Instant::now() >= *deadline => {
                    self.error(ErrorCode::AuthTimeout, None, None, None);
                    self.close(close::AUTH_TIMEOUT);
                }
                Phase::Closing { until } if Instant::now() >= *until => return,
                _ => {}
            }
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    if !matches!(self.phase, Phase::Closing { .. }) || self.authed.is_some() {
                        self.handle(t.as_str());
                    }
                }
                Ok(Message::Binary(_)) => {
                    self.error(ErrorCode::Unsupported, None, None, None);
                    self.close(close::BAD_REQUEST);
                }
                Ok(Message::Close(_)) => {
                    let _ = self.ws.flush();
                    return;
                }
                Ok(_) => {}
                Err(e) if would_block(&e) => {}
                Err(_) => return,
            }
        }
    }

    /// The socket is gone: a drop, unless it said goodbye or was replaced.
    fn ended(&mut self) {
        let authed = match self.phase {
            Phase::Authed { key, gen, bye_seen } => Some((key, gen, bye_seen)),
            _ => self.authed,
        };
        let Some((key, gen, bye_seen)) = authed else {
            return;
        };
        let ring = self.ring.clone();
        let mut rings = self.rings();
        let Some(state) = rings.get_mut(&ring) else {
            return;
        };
        if !bye_seen {
            state.offline(&key, gen, DROPPED);
        }
        if state.conns.get(&key).is_some_and(|c| c.gen == gen) {
            state.conns.remove(&key);
        }
    }

    fn handle(&mut self, text: &str) {
        let pre = matches!(self.phase, Phase::Pre { .. });
        let decoded = decode_client(text);
        if let Phase::Authed { key, gen, .. } = self.phase {
            let ring = self.ring.clone();
            if let Some(s) = self.rings().get_mut(&ring) {
                if s.conns.get(&key).is_some_and(|c| c.gen == gen) {
                    s.activity.insert(key, Instant::now());
                }
            }
        }
        // The quota counts authenticated frames past the size cap and the parse, except the
        // exact ping (auto-answered on a Worker, so it never reaches the Relay's code) and
        // `bye` (always honoured).
        if matches!(self.phase, Phase::Authed { .. })
            && text != PING
            && !matches!(
                decoded,
                Ok(ClientFrame::Bye { .. }) | Err(WireError::TooLarge | WireError::Malformed(_))
            )
            && !self.quota_allows(&decoded)
        {
            return;
        }
        let frame = match decoded {
            Ok(f) => f,
            Err(WireError::TooLarge) => {
                self.error(ErrorCode::TooLarge, None, None, None);
                self.close(close::TOO_LARGE);
                return;
            }
            Err(WireError::Malformed(_)) => {
                self.error(ErrorCode::BadRequest, None, None, None);
                self.close(close::BAD_REQUEST);
                return;
            }
            Err(WireError::Invalid { .. }) => {
                self.error(ErrorCode::BadRequest, None, None, None);
                if pre {
                    self.close(close::BAD_REQUEST);
                }
                return;
            }
        };
        if let ClientFrame::Ping = frame {
            if let Phase::Authed { key, .. } = self.phase {
                let ring = self.ring.clone();
                if let Some(s) = self.rings().get_mut(&ring) {
                    *s.pings.entry(key).or_insert(0) += 1;
                }
            }
            self.write(PONG);
            return;
        }
        match self.phase {
            Phase::Pre { .. } => self.handle_pre(frame),
            Phase::Authed { key, gen, .. } => self.handle_authed(key, gen, frame),
            Phase::Closing { .. } => {
                // Late frames from a socket being closed (a replaced device's goodbye):
                // only presence bookkeeping, guarded by the generation.
                if let (ClientFrame::Bye { reason }, Some((key, gen, _))) = (frame, self.authed) {
                    let ring = self.ring.clone();
                    if let Some(s) = self.rings().get_mut(&ring) {
                        s.offline(&key, gen, reason.as_str());
                    }
                    self.authed = Some((key, gen, true));
                }
            }
        }
    }

    /// Counts one frame against the Ring's daily quota. Over it, refuses the frame (echoing
    /// its `id` or `to`), and closes after `QUOTA_REFUSALS_BEFORE_CLOSE` refusals in a row.
    fn quota_allows(&mut self, decoded: &Result<ClientFrame, WireError>) -> bool {
        let Some(limit) = self.shared.quota() else {
            return true;
        };
        let day = now() / 86_400;
        let ring = self.ring.clone();
        let allowed = match self.rings().get_mut(&ring) {
            Some(s) => {
                if s.quota.0 != day {
                    s.quota = (day, 0);
                }
                let ok = s.quota.1 < limit;
                if ok {
                    s.quota.1 += 1;
                }
                ok
            }
            None => true,
        };
        if allowed {
            self.quota_refusals = 0;
            return true;
        }
        let (id, to) = match decoded {
            Ok(ClientFrame::Env { to, .. }) => (None, Some(*to)),
            Ok(
                ClientFrame::RosterPut { id, .. }
                | ClientFrame::RosterGet { id, .. }
                | ClientFrame::EntitlementPut { id, .. }
                | ClientFrame::Push { id, .. },
            ) => (Some(*id), None),
            _ => (None, None),
        };
        self.error(ErrorCode::Quota, id, to, None);
        self.quota_refusals += 1;
        if self.quota_refusals >= QUOTA_REFUSALS_BEFORE_CLOSE {
            self.close(close::QUOTA);
        }
        false
    }

    fn handle_pre(&mut self, frame: ClientFrame) {
        match frame {
            ClientFrame::AuthChain { rosters } if rosters.is_empty() => {
                self.error(ErrorCode::BadRequest, None, None, None);
                self.close(close::BAD_REQUEST);
            }
            ClientFrame::AuthChain { rosters } => {
                // Check each token now, so a bad candidate fails early; store the exact
                // tokens, chunk by chunk, outside the committed Roster state.
                for t in &rosters {
                    if let Err(e) = SignedRoster::parse(t) {
                        return self.refuse_roster(e);
                    }
                }
                let ring = self.ring.clone();
                let (max_chunks, max_stored) = (
                    self.shared.opts.max_stage_chunks,
                    self.shared.opts.max_stage_bytes,
                );
                let Phase::Pre {
                    candidate_id,
                    manifest: m,
                    ..
                } = &mut self.phase
                else {
                    return;
                };
                let chunks = chunk_tokens(&rosters);
                m.frames += 1;
                m.versions += rosters.len();
                m.bytes += rosters.iter().map(String::len).sum::<usize>();
                m.stored += chunks.iter().map(String::len).sum::<usize>();
                let first = m.chunks;
                m.chunks += chunks.len();
                if m.versions > MAX_CHAIN_LEN
                    || m.bytes > MAX_CANDIDATE_BYTES
                    || m.chunks > max_chunks
                    || m.stored > max_stored
                {
                    return self.refuse_roster(RosterError::TooLarge);
                }
                let prefix = stage_prefix(&ring, candidate_id);
                let mut stage = self.shared.stage.lock().unwrap_or_else(|e| e.into_inner());
                for (i, c) in chunks.into_iter().enumerate() {
                    stage.insert(format!("{prefix}{:08}", first + i), (Instant::now(), c));
                }
            }
            ClientFrame::Auth { sign_key, sig, .. } => self.auth(sign_key, sig),
            _ => {
                self.error(ErrorCode::BadRequest, None, None, None);
                self.close(close::BAD_REQUEST);
            }
        }
    }

    fn refuse_roster(&mut self, e: RosterError) {
        self.error(ErrorCode::RosterInvalid, None, None, Some(e.as_code()));
        self.close(close::REFUSED);
    }

    /// Rebuilds the staged candidate from the stage store and deletes it. `None` when a
    /// chunk is missing (expired or lost): the candidate cannot be trusted to be whole.
    fn take_candidate(&self, candidate_id: &str, m: Manifest) -> Option<Vec<SignedRoster>> {
        let prefix = stage_prefix(&self.ring, candidate_id);
        let entries: Vec<(String, String)> = {
            let mut stage = self.shared.stage.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<String> = stage
                .range(prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&prefix))
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter()
                .filter_map(|k| stage.remove(&k).map(|(_, v)| (k, v)))
                .collect()
        };
        // Exactly the chunks 0..m.chunks, each within the chunk limit, adding up to the
        // manifest's totals.
        if entries.len() != m.chunks {
            return None;
        }
        let (mut versions, mut bytes, mut stored) = (0, 0, 0);
        let mut out = Vec::new();
        for (i, (k, v)) in entries.iter().enumerate() {
            if k[prefix.len()..] != format!("{i:08}") || v.len() > MAX_STAGE_CHUNK {
                return None;
            }
            stored += v.len();
            let tokens: Vec<String> = serde_json::from_str(v).ok()?;
            if tokens.is_empty() {
                return None;
            }
            for t in tokens {
                versions += 1;
                bytes += t.len();
                out.push(SignedRoster::parse(&t).ok()?);
            }
        }
        if (versions, bytes, stored) != (m.versions, m.bytes, m.stored) {
            return None;
        }
        Some(out)
    }

    fn auth(&mut self, key: SignKey, sig: Signature) {
        let (nonce, candidate_id, manifest) = match &self.phase {
            Phase::Pre {
                nonce,
                candidate_id,
                manifest,
                ..
            } => (nonce.clone(), candidate_id.clone(), *manifest),
            _ => return,
        };
        let Some(candidate) = self.take_candidate(&candidate_id, manifest) else {
            self.error(ErrorCode::RosterInvalid, None, None, Some("invalid"));
            return self.close(close::REFUSED);
        };
        let ring = self.ring.clone();
        let origin = self
            .shared
            .origin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut rings = self.shared.rings.lock().unwrap_or_else(|e| e.into_inner());
        // The candidate: a full chain for a new Ring, or an extension of the stored head.
        let result = match rings.get(&ring) {
            None if candidate.is_empty() => Err((ErrorCode::NoRoster, None)),
            None => RosterChain::from_chain(candidate.clone())
                .and_then(|c| {
                    if c.ring_id() == &ring {
                        Ok(c)
                    } else {
                        Err(RosterError::RingMismatch)
                    }
                })
                .map(|c| {
                    let added = c.versions().to_vec();
                    (c, added, Vec::new())
                })
                .map_err(|e| (ErrorCode::RosterInvalid, Some(e.as_code()))),
            Some(s) => s
                .chain
                .extended(&candidate)
                .map(|(c, a)| {
                    let added = c.since(s.chain.head().version()).to_vec();
                    (c, added, a.removed)
                })
                .map_err(|e| (ErrorCode::RosterInvalid, Some(e.as_code()))),
        };
        let (chain, added, removed) = match result {
            Ok(r) => r,
            Err((code, detail)) => {
                drop(rings);
                self.error(code, None, None, detail);
                return self.close(close::REFUSED);
            }
        };
        if chain.head().member(&key).is_none() {
            drop(rings);
            self.error(ErrorCode::NotMember, None, None, None);
            return self.close(close::REFUSED);
        }
        if !verify(&key, &auth_message(&origin, &ring, &nonce, &key), &sig) {
            drop(rings);
            self.error(ErrorCode::BadSignature, None, None, None);
            return self.close(close::BAD_SIGNATURE);
        }
        // Authenticated: commit the candidate.
        let existed = rings.contains_key(&ring);
        let state = rings.entry(ring.clone()).or_insert_with(|| RingState {
            chain: chain.clone(),
            presence: HashMap::new(),
            conns: HashMap::new(),
            entitlement: None,
            pings: HashMap::new(),
            gens: HashMap::new(),
            quota: (0, 0),
            activity: HashMap::new(),
            pushes_in_flight: 0,
        });
        state.chain = chain;
        if existed {
            state.announce(&added, &removed, Some(&key));
        }
        let gen = {
            let g = state.gens.entry(key).or_insert(0);
            *g += 1;
            *g
        };
        if let Some(old) = state.conns.insert(
            key,
            ConnRef {
                gen,
                tx: self.tx.clone(),
            },
        ) {
            let _ = old
                .tx
                .send(Cmd::Text(RelayFrame::error(ErrorCode::Replaced).encode()));
            let _ = old.tx.send(Cmd::Close(close::REPLACED));
        }
        state.presence.insert(
            key,
            Rec {
                online: true,
                last_seen: Some(now()),
                last_reason: None,
                gen,
                foreground: false,
                foreground_until: None,
            },
        );
        state.activity.insert(key, Instant::now());
        let me = RelayFrame::Presence(state.presence_of(&key)).encode();
        state.broadcast(Some(&key), &me);
        let limited = !self.shared.entitled(&ring, state);
        let entitlement = match &self.shared.opts.hosted {
            None => state.entitlement.clone(),
            Some(keys) => state
                .entitlement
                .clone()
                .filter(|t| verify_entitlement(t, &ring, now(), keys, false).is_ok()),
        };
        let welcome = RelayFrame::Welcome {
            you: key,
            roster_version: state.chain.head().version(),
            presence: state
                .chain
                .head()
                .roster()
                .members
                .iter()
                .map(|m| state.presence_of(&m.sign_key))
                .collect(),
            entitlement,
            limited,
            caps: self.shared.caps(),
        }
        .encode();
        drop(rings);
        self.phase = Phase::Authed {
            key,
            gen,
            bye_seen: false,
        };
        self.write(&welcome);
    }

    /// Re-checks, under the Ring's lock, that this socket still speaks for a member; if not,
    /// refuses and closes before any side effect.
    fn still_current(&mut self, me: &SignKey, gen: u64) -> bool {
        let ring = self.ring.clone();
        let verdict = match self.rings().get(&ring) {
            Some(s) => s.current(me, gen),
            None => Err(ErrorCode::Removed),
        };
        match verdict {
            Ok(()) => true,
            Err(code) => {
                let close_code = if code == ErrorCode::Removed {
                    close::REMOVED
                } else {
                    close::REPLACED
                };
                self.error(code, None, None, None);
                self.close(close_code);
                false
            }
        }
    }

    fn handle_authed(&mut self, me: SignKey, gen: u64, frame: ClientFrame) {
        let ring = self.ring.clone();
        if matches!(
            frame,
            ClientFrame::Env { .. }
                | ClientFrame::RosterPut { .. }
                | ClientFrame::EntitlementPut { .. }
                | ClientFrame::State { .. }
                | ClientFrame::Push { .. }
        ) && !self.still_current(&me, gen)
        {
            return;
        }
        match frame {
            ClientFrame::Env { to, payload } => {
                if self.shared.recording.load(Ordering::Acquire) {
                    let bytes = super::super::b64::decode(&payload).unwrap_or_default();
                    self.shared
                        .attempts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((me, to, bytes));
                }
                let refusal = if let Err(code) = check_payload(&payload) {
                    Some(code)
                } else if to == me {
                    Some(ErrorCode::BadRequest)
                } else {
                    let rings = self.rings();
                    match rings.get(&ring) {
                        // Membership and generation were checked above; recheck under this
                        // lock, the one the side effect happens under.
                        Some(s) if s.current(&me, gen).is_err() => Some(ErrorCode::Removed),
                        Some(s) if !self.shared.entitled(&ring, s) => {
                            Some(ErrorCode::EntitlementRequired)
                        }
                        Some(s) if s.chain.head().member(&to).is_none() => {
                            Some(ErrorCode::UnknownRecipient)
                        }
                        Some(s) => match s.conns.get(&to) {
                            None => Some(ErrorCode::Offline),
                            Some(c) => self.shared.route_env(&ring, &me, &to, payload, &c.tx),
                        },
                        None => Some(ErrorCode::Internal),
                    }
                };
                if let Some(code) = refusal {
                    self.error(code, None, Some(to), None);
                }
            }
            ClientFrame::Bye { reason } => {
                if let Some(s) = self.rings().get_mut(&ring) {
                    s.offline(&me, gen, reason.as_str());
                }
                self.phase = Phase::Authed {
                    key: me,
                    gen,
                    bye_seen: true,
                };
                self.close(close::NORMAL);
            }
            ClientFrame::RosterPut { id, roster } => {
                if self.shared.ignore_puts.load(Ordering::Acquire) {
                    return;
                }
                if self.shared.refuse_puts.load(Ordering::Acquire) {
                    self.error(ErrorCode::Internal, Some(id), None, None);
                    return;
                }
                let r = match SignedRoster::parse(&roster) {
                    Ok(r) => r,
                    Err(e) => {
                        self.error(ErrorCode::RosterInvalid, Some(id), None, Some(e.as_code()));
                        return;
                    }
                };
                let mut rings = self.shared.rings.lock().unwrap_or_else(|e| e.into_inner());
                let Some(s) = rings.get_mut(&ring) else {
                    return;
                };
                if s.current(&me, gen).is_err() {
                    return;
                }
                if s.chain.head().token() == r.token() {
                    drop(rings);
                    self.write(&RelayFrame::Ok { id }.encode());
                    return;
                }
                match s.chain.accept(std::slice::from_ref(&r)) {
                    Ok(a) => {
                        let first = self.shared.opts.broadcast_before_ok;
                        s.announce(
                            std::slice::from_ref(&r),
                            &a.removed,
                            if first { Some(&me) } else { None },
                        );
                        drop(rings);
                        if first {
                            self.write(
                                &RelayFrame::Roster {
                                    roster: r.token().to_string(),
                                }
                                .encode(),
                            );
                        }
                        self.write(&RelayFrame::Ok { id }.encode());
                    }
                    Err(e) => {
                        drop(rings);
                        let code = match e {
                            RosterError::Stale => {
                                self.shared.stale_puts.fetch_add(1, Ordering::AcqRel);
                                ErrorCode::RosterStale
                            }
                            RosterError::PrevMismatch => ErrorCode::RosterConflict,
                            _ => ErrorCode::RosterInvalid,
                        };
                        self.error(code, Some(id), None, Some(e.as_code()));
                    }
                }
            }
            ClientFrame::RosterGet { id, since } => {
                let tokens: Vec<String> = match self.rings().get(&ring) {
                    Some(s) => s
                        .chain
                        .since(since)
                        .iter()
                        .map(|r| r.token().to_string())
                        .collect(),
                    None => Vec::new(),
                };
                let mut out = Vec::new();
                let mut size = 64;
                for t in &tokens {
                    if !out.is_empty() && size + t.len() + 3 > MAX_FRAME {
                        break;
                    }
                    size += t.len() + 3;
                    out.push(t.clone());
                }
                let more = out.len() < tokens.len();
                self.write(
                    &RelayFrame::RosterChain {
                        id,
                        rosters: out,
                        more,
                    }
                    .encode(),
                );
            }
            ClientFrame::EntitlementPut { id, token } => {
                // A Hosted Relay verifies with the gateway's keys; any other checks the shape.
                let refused = match &self.shared.opts.hosted {
                    Some(keys) => verify_entitlement(&token, &ring, now(), keys, false)
                        .err()
                        .map(|e| Some(e.as_code())),
                    None => (!entitlement_well_formed(&token)).then_some(None),
                };
                if let Some(detail) = refused {
                    self.error(ErrorCode::EntitlementInvalid, Some(id), None, detail);
                    return;
                }
                let shared = self.shared.clone();
                let mut rings = self.rings();
                let Some(s) = rings.get_mut(&ring) else {
                    return;
                };
                if s.current(&me, gen).is_err() {
                    return;
                }
                s.entitlement = Some(token.clone());
                // Every socket of the Ring routes (or not) by the stored token from now on.
                let limited = !shared.entitled(&ring, s);
                s.broadcast(
                    None,
                    &RelayFrame::Entitlement {
                        token: Some(token),
                        limited,
                    }
                    .encode(),
                );
                drop(rings);
                self.write(&RelayFrame::Ok { id }.encode());
            }
            ClientFrame::State { foreground } => {
                let mut rings = self.rings();
                let Some(s) = rings.get_mut(&ring) else {
                    return;
                };
                let mobile = s.chain.head().member(&me).map(|m| m.role) == Some(Role::Mobile);
                if !mobile || s.current(&me, gen).is_err() {
                    drop(rings);
                    self.error(ErrorCode::BadRequest, None, None, Some("state"));
                    return;
                }
                let until = s.lease_until(&me, self.shared.opts.foreground_lease);
                let Some(r) = s.presence.get_mut(&me) else {
                    return;
                };
                if r.foreground == foreground {
                    return;
                }
                r.foreground = foreground;
                r.foreground_until = if foreground { until } else { None };
                let p = RelayFrame::Presence(s.presence_of(&me)).encode();
                s.broadcast(Some(&me), &p);
            }
            ClientFrame::Push {
                id,
                blob,
                sealed_payload,
                collapse_id,
            } => self.push(me, gen, id, blob, sealed_payload, collapse_id),
            ClientFrame::Unknown { .. } => {
                self.error(ErrorCode::UnknownType, None, None, None);
            }
            ClientFrame::AuthChain { .. } | ClientFrame::Auth { .. } => {
                self.error(ErrorCode::BadRequest, None, None, None);
            }
            ClientFrame::Ping => {}
        }
    }
}

impl Conn {
    /// A `push` frame (section 18): checked in order (role, fields, foreground, a gateway),
    /// then forwarded on a thread of its own, which answers on this socket.
    fn push(
        &mut self,
        me: SignKey,
        gen: u64,
        id: u64,
        blob: String,
        sealed: String,
        collapse: String,
    ) {
        self.shared.push_frames.fetch_add(1, Ordering::AcqRel);
        let ring = self.ring.clone();
        let failed = |detail: &str| (ErrorCode::PushFailed, Some(detail.to_string()));
        let verdict: Result<(), (ErrorCode, Option<String>)> = {
            let mut rings = self.rings();
            match rings.get_mut(&ring) {
                None => Err((ErrorCode::Internal, None)),
                Some(s) if s.current(&me, gen).is_err() => Err((ErrorCode::Removed, None)),
                Some(s) if s.chain.head().member(&me).map(|m| m.role) != Some(Role::Daemon) => {
                    Err(failed("role"))
                }
                Some(_)
                    if !blob_well_formed(&blob)
                        || !sealed_well_formed(&sealed)
                        || !collapse_id_well_formed(&collapse) =>
                {
                    Err((ErrorCode::BadRequest, None))
                }
                Some(s) if s.any_foreground(self.shared.opts.foreground_lease) => {
                    Err(failed("foreground"))
                }
                Some(_) if self.shared.opts.push_gateway.is_none() => Err(failed("unavailable")),
                Some(s) if s.pushes_in_flight >= PUSH_MAX_IN_FLIGHT => Err(failed("busy")),
                Some(s) => {
                    s.pushes_in_flight += 1;
                    Ok(())
                }
            }
        };
        if let Err((code, detail)) = verdict {
            self.error(code, Some(id), None, detail.as_deref());
            return;
        }
        let body = serde_json::json!({
            "blob": blob,
            "sealedPayload": sealed,
            "collapseId": collapse,
        })
        .to_string();
        let gateway = self.shared.opts.push_gateway.clone().unwrap_or_default();
        let timeout = self.shared.opts.push_timeout;
        let (shared, tx) = (self.shared.clone(), self.tx.clone());
        thread::spawn(move || {
            let r = forward_push(&gateway, &body, timeout);
            if let Some(s) = shared
                .rings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&ring)
            {
                s.pushes_in_flight = s.pushes_in_flight.saturating_sub(1);
            }
            let answer = match r {
                Ok(()) => RelayFrame::Ok { id },
                Err(detail) => RelayFrame::Error {
                    code: ErrorCode::PushFailed,
                    id: Some(id),
                    to: None,
                    detail: Some(detail),
                },
            };
            // A direct answer: it goes out even on a muted socket.
            let _ = tx.send(Cmd::Inject(answer.encode()));
        });
    }
}

// ---- The pairing pipe (section 16) ------------------------------------------------------

/// An admission's slot reservation, given back when dropped before a socket took it over
/// (a failed upgrade).
struct Reservation(Option<(Arc<Shared>, String)>);

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some((shared, slot)) = self.0.take() {
            shared.pair_release(&slot);
        }
    }
}

/// A client address as the pairing pipe's limits count it (section 16): an IPv4 address, an
/// IPv6 address's /64, or none (one shared bucket). [`ClientAddr::prefix`] is the bucket of the
/// per-prefix slot cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ClientAddr {
    V4([u8; 4]),
    V6([u8; 8]),
    Unknown,
}

impl ClientAddr {
    fn of(ip: Option<IpAddr>) -> ClientAddr {
        match ip {
            Some(IpAddr::V4(a)) => ClientAddr::V4(a.octets()),
            Some(IpAddr::V6(a)) => match a.to_ipv4_mapped() {
                Some(v4) => ClientAddr::V4(v4.octets()),
                None => {
                    let mut p = [0u8; 8];
                    p.copy_from_slice(&a.octets()[..8]);
                    ClientAddr::V6(p)
                }
            },
            None => ClientAddr::Unknown,
        }
    }

    /// The address prefix the slot cap counts by: an IPv4 /24, an IPv6 /64.
    fn prefix(self) -> ClientAddr {
        match self {
            ClientAddr::V4([a, b, c, _]) => ClientAddr::V4([a, b, c, 0]),
            other => other,
        }
    }
}

/// The client address a trusted proxy names in header `name` of a request head: the last list
/// entry of its last occurrence (as in `X-Forwarded-For`), an IPv6 address possibly in
/// brackets. `None` without a usable value.
fn header_address(head: &str, name: &str) -> Option<IpAddr> {
    let value = head
        .split("\r\n")
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .last()?
        .1;
    let last = value.rsplit(',').next()?.trim();
    let bare = last
        .strip_prefix('[')
        .and_then(|l| l.strip_suffix(']'))
        .unwrap_or(last);
    bare.parse().ok()
}

#[derive(Default)]
struct PairState {
    slots: HashMap<String, Slot>,
    /// Slot opens per client address, for the rate limit.
    opens: HashMap<ClientAddr, VecDeque<Instant>>,
}

struct Slot {
    first_open: Instant,
    /// The address prefix of the open that created the slot: its reservation counts there.
    prefix: ClientAddr,
    /// The sockets on it, at most two: (socket id, its commands).
    socks: Vec<(u64, Sender<PairCmd>)>,
    /// Two parties met: the slot is used up.
    met: bool,
    /// When a used-up slot stopped being needed: the meeting, or its last socket leaving,
    /// whichever came later. Its tombstone runs from here.
    ended: Instant,
    next_id: u64,
    /// Opens admitted whose upgrade has not finished yet.
    reserved: usize,
}

impl Slot {
    fn new(prefix: ClientAddr) -> Slot {
        let now = Instant::now();
        Slot {
            first_open: now,
            prefix,
            socks: Vec::new(),
            met: false,
            ended: now,
            next_id: 0,
            reserved: 0,
        }
    }
}

enum PairCmd {
    Text(String),
    Close(u16),
}

impl Shared {
    /// Whether `s` is outstanding: it holds a reservation against the slot caps from its first
    /// open until it is used up or expires (section 16).
    fn pair_outstanding(&self, s: &Slot) -> bool {
        !s.met && (self.opts.lax_pairing || s.first_open.elapsed() < self.opts.pair_ttl)
    }

    /// Forgets slots nobody holds whose tombstone has run out.
    fn pair_sweep(&self, p: &mut PairState) {
        let (ttl, tomb) = (self.opts.pair_ttl, self.opts.pair_tombstone);
        p.slots.retain(|_, s| {
            let ended = if s.met { s.ended } else { s.first_open + ttl };
            !s.socks.is_empty() || s.reserved > 0 || Instant::now() < ended + tomb
        });
    }

    /// Before the upgrade: an HTTP status that refuses the open (rate limit, slot caps), or
    /// `None` to go on.
    fn pair_admit(&self, addr: ClientAddr, slot: &str) -> Option<&'static str> {
        let mut p = self.pair.lock().unwrap_or_else(|e| e.into_inner());
        self.pair_sweep(&mut p);
        if let Some(limit) = self.opts.pair_opens_per_minute {
            let log = p.opens.entry(addr).or_default();
            while log
                .front()
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(60))
            {
                log.pop_front();
            }
            if log.len() >= limit as usize {
                return Some("429 Too Many Requests");
            }
            log.push_back(Instant::now());
        }
        // A slot the Relay holds state for (outstanding, or a tombstone) is not new: the caps
        // do not apply, and the slot itself answers the open.
        if let Some(s) = p.slots.get_mut(slot) {
            s.reserved += 1;
            return None;
        }
        // A new slot is reserved here, at admission, so concurrent opens cannot overshoot the
        // caps; a failed upgrade gives the reservation back (`pair_release`).
        let prefix = addr.prefix();
        let (mut all, mut mine) = (0, 0);
        for s in p.slots.values().filter(|s| self.pair_outstanding(s)) {
            all += 1;
            mine += usize::from(s.prefix == prefix);
        }
        if all >= self.opts.pair_max_slots || mine >= self.opts.pair_max_slots_per_prefix {
            return Some("503 Service Unavailable");
        }
        let mut s = Slot::new(prefix);
        s.reserved = 1;
        p.slots.insert(slot.to_string(), s);
        None
    }

    /// Gives back an admission whose upgrade failed; a slot it created, which nobody has
    /// joined, is forgotten (its open was refused).
    fn pair_release(&self, slot: &str) {
        let mut p = self.pair.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = p.slots.get_mut(slot) {
            s.reserved = s.reserved.saturating_sub(1);
            if s.reserved == 0 && s.socks.is_empty() && !s.met && s.next_id == 0 {
                p.slots.remove(slot);
            }
        }
    }
}

fn pair_write(ws: &mut WebSocket<SConn>, text: &str) -> bool {
    match ws.send(Message::text(text)) {
        Ok(()) => true,
        Err(e) if would_block(&e) => ws.flush().is_ok(),
        Err(_) => false,
    }
}

fn pair_close(ws: &mut WebSocket<SConn>, code: u16) {
    let _ = ws.close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    }));
    let _ = ws.flush();
    // Let the close handshake finish.
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        match ws.read() {
            Err(e) if would_block(&e) => {}
            Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn pair_refuse(ws: &mut WebSocket<SConn>, code: ErrorCode, close_code: u16) {
    pair_write(ws, &PairRelayFrame::Error { code, detail: None }.encode());
    pair_close(ws, close_code);
}

/// One socket on a pairing slot.
fn pair_serve(shared: Arc<Shared>, mut ws: WebSocket<SConn>, slot: String) {
    let lax = shared.opts.lax_pairing;
    let ttl = shared.opts.pair_ttl;
    let (tx, rx) = mpsc::channel();
    // Join the slot.
    let joined = {
        let mut p = shared.pair.lock().unwrap_or_else(|e| e.into_inner());
        let s = p.slots.entry(slot.clone()).or_insert_with(|| Slot {
            reserved: 1,
            ..Slot::new(ClientAddr::Unknown)
        });
        // This socket's admission reservation becomes the socket itself.
        s.reserved = s.reserved.saturating_sub(1);
        if lax && s.socks.is_empty() {
            s.met = false;
            s.first_open = Instant::now();
        }
        // A used-up (or burnt) slot answers `pair_busy` before its expiry is looked at.
        if s.socks.len() >= 2 || (s.met && !lax) {
            Err((ErrorCode::PairBusy, close::PAIR_BUSY))
        } else if !lax && s.first_open.elapsed() >= ttl {
            Err((ErrorCode::PairExpired, close::PAIR_EXPIRED))
        } else {
            let id = s.next_id;
            s.next_id += 1;
            s.socks.push((id, tx));
            if s.socks.len() == 2 {
                // Used up: the reservation is released, a tombstone remains.
                s.met = true;
                s.ended = Instant::now();
                for (_, t) in &s.socks {
                    let _ = t.send(PairCmd::Text(PairRelayFrame::Peer.encode()));
                }
            } else {
                let _ = s.socks[0]
                    .1
                    .send(PairCmd::Text(PairRelayFrame::Wait { v: 1 }.encode()));
            }
            Ok((id, s.first_open))
        }
    };
    let (me, first_open) = match joined {
        Ok(x) => x,
        Err((code, c)) => return pair_refuse(&mut ws, code, c),
    };
    // Leaves the slot; `burn`: the socket was refused for a protocol violation, which uses the
    // slot up (its reservation is released, a tombstone remains).
    let leave = |shared: &Shared, burn: bool| {
        let mut p = shared.pair.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = p.slots.get_mut(&slot) {
            s.socks.retain(|(id, _)| *id != me);
            if burn {
                s.met = true;
                s.ended = Instant::now();
            }
            if s.met && s.socks.is_empty() {
                s.ended = Instant::now();
            }
            // The Relay closes the other side when one side goes.
            for (_, t) in &s.socks {
                let _ = t.send(PairCmd::Close(close::NORMAL));
            }
        }
    };
    let mut sent = 0usize;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                PairCmd::Text(t) => {
                    if !pair_write(&mut ws, &t) {
                        leave(&shared, false);
                        return;
                    }
                }
                PairCmd::Close(c) => {
                    leave(&shared, false);
                    return pair_close(&mut ws, c);
                }
            }
        }
        if !lax && first_open.elapsed() >= ttl {
            leave(&shared, false);
            return pair_refuse(&mut ws, ErrorCode::PairExpired, close::PAIR_EXPIRED);
        }
        let text = match ws.read() {
            Ok(Message::Text(t)) => t.as_str().to_string(),
            Ok(Message::Binary(_)) => {
                leave(&shared, true);
                return pair_refuse(&mut ws, ErrorCode::Unsupported, close::BAD_REQUEST);
            }
            Ok(Message::Close(_)) => {
                let _ = ws.flush();
                break;
            }
            Ok(_) => continue,
            Err(e) if would_block(&e) => continue,
            Err(_) => break,
        };
        if text.len() > MAX_PAIR_FRAME {
            leave(&shared, true);
            return pair_refuse(&mut ws, ErrorCode::TooLarge, close::TOO_LARGE);
        }
        match decode_pair_client(&text) {
            Ok(PairClientFrame::Ping) => {
                pair_write(&mut ws, PONG);
            }
            Ok(PairClientFrame::Msg { payload }) => {
                let ok_len = super::super::b64::decoded_len(payload.len())
                    .is_some_and(|n| n <= super::super::pairing::MAX_PAIR_MESSAGE);
                if !ok_len || super::super::b64::decode(&payload).is_err() {
                    leave(&shared, true);
                    return pair_refuse(&mut ws, ErrorCode::BadRequest, close::BAD_REQUEST);
                }
                sent += 1;
                if sent > MAX_PAIR_MSGS {
                    leave(&shared, true);
                    return pair_refuse(&mut ws, ErrorCode::TooMany, close::BAD_REQUEST);
                }
                let p = shared.pair.lock().unwrap_or_else(|e| e.into_inner());
                let other = p
                    .slots
                    .get(&slot)
                    .and_then(|s| s.socks.iter().find(|(id, _)| *id != me))
                    .map(|(_, t)| t.clone());
                drop(p);
                match other {
                    Some(t) => {
                        let _ = t.send(PairCmd::Text(PairRelayFrame::Msg { payload }.encode()));
                    }
                    // Nobody to forward to yet.
                    None => {
                        leave(&shared, true);
                        return pair_refuse(&mut ws, ErrorCode::BadRequest, close::BAD_REQUEST);
                    }
                }
            }
            Err(_) => {
                leave(&shared, true);
                return pair_refuse(&mut ws, ErrorCode::BadRequest, close::BAD_REQUEST);
            }
        }
    }
    leave(&shared, false);
}

/// A TCP proxy that forwards the client's bytes at once and the server's a few bytes at a
/// time, so TLS records and WebSocket frames arrive in pieces.
pub struct TrickleProxy {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl TrickleProxy {
    pub fn start(upstream: SocketAddr, chunk: usize, every: Duration) -> TrickleProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let addr = listener.local_addr().expect("proxy address");
        listener.set_nonblocking(true).expect("nonblocking proxy");
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                let Ok((client, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = client.set_nonblocking(false);
                let Ok(server) = TcpStream::connect(upstream) else {
                    continue;
                };
                // Each trickled piece goes out at once. With Nagle, a small write waits for
                // the ACK of the one before, and macOS delays loopback ACKs: the trickle
                // then crawled far past the test's connect timeout.
                let _ = client.set_nodelay(true);
                let _ = server.set_nodelay(true);
                let (Ok(c2), Ok(s2)) = (client.try_clone(), server.try_clone()) else {
                    continue;
                };
                thread::spawn(move || pipe(client, server, usize::MAX, Duration::ZERO));
                thread::spawn(move || pipe(s2, c2, chunk.max(1), every));
            }
        });
        TrickleProxy { addr, stop }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for TrickleProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Forwards `from` to `to`, `chunk` bytes per `every`. The pace is kept against a schedule,
/// not by sleeping `every` after each piece: a sleep can last several times longer than asked
/// (macOS runners under load stretch a 15 ms sleep to 65 ms), and per-piece sleeps would add
/// that up. Behind schedule, pieces go out back to back until it is caught up.
fn pipe(mut from: TcpStream, mut to: TcpStream, chunk: usize, every: Duration) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut due = Instant::now();
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        // An idle gap does not bank credit for a burst later.
        due = due.max(Instant::now());
        for piece in buf[..n].chunks(chunk) {
            if to.write_all(piece).is_err() {
                return;
            }
            if !every.is_zero() {
                due += every;
                let now = Instant::now();
                if due > now {
                    thread::sleep(due - now);
                }
            }
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Both);
    let _ = from.shutdown(std::net::Shutdown::Both);
}

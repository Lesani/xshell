//! Pairwise Noise sessions over a [`Connector`] (blocking): [`Sessions`] opens sessions to
//! other members ([`Sessions::open`]) and, with an [`Acceptor`], answers them, and turns each
//! into a [`SessionStream`], an ordered byte stream (`Read + Write`) that the xshell
//! protocol's frame codec runs on unchanged.
//!
//! The owner forwards its [`ConnectorEvents`] here ([`Sessions::envelope`], `error`,
//! `state`, `presence`, and [`Sessions::sweep`] for every head it adopts), or uses
//! [`SessionEvents`], which does it. A session dies, and its stream fails, when:
//! - a head is adopted that no longer lists the peer with the same keys and role;
//! - the Relay reports the peer offline, or refuses an envelope to it (any `error` with `to`);
//! - the connection leaves `Connected`, or becomes limited (envelopes may have been lost);
//! - a message fails to decrypt, or is not the next one;
//! - a newer handshake from the same peer replaces it;
//! - either side closes it.
//!
//! One session per peer. A responder accepts a first message only if its timestamp is newer
//! than any it accepted from that peer ([`Freshness`]), so a replayed one never replaces a
//! live session. An initiator retries with a new session id every `hs_timeout` until
//! `open_timeout`, which covers a responder that has not seen the head listing it yet.

use super::super::chain::RosterChain;
use super::super::noise::{
    inner, read_hello, session_role, Freshness, Header, Initiator, Kind, NoiseError, Opener,
    Sealer, Sid,
};
use super::super::{DeviceKeys, Member, RingError, Role, SignKey, SignedRoster};
use super::connector::{Connector, ConnectorEvents, LinkState, MoveState};
use super::wire::{ErrorCode, MemberPresence};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct SessionsConfig {
    pub keys: Arc<DeviceKeys>,
    /// An initiator waits this long for an answer before it tries again with a new id.
    pub hs_timeout: Duration,
    /// [`Sessions::open`] gives up after this long.
    pub open_timeout: Duration,
    /// Received messages a stream may hold unread; more kills the session.
    pub queue_messages: usize,
    /// … and bytes.
    pub queue_bytes: usize,
    /// A write waits this long for the Relay connection's queue to drain before it kills
    /// the session.
    pub write_stall: Duration,
}

impl SessionsConfig {
    pub fn new(keys: Arc<DeviceKeys>) -> Self {
        SessionsConfig {
            keys,
            hs_timeout: Duration::from_secs(5),
            open_timeout: Duration::from_secs(15),
            queue_messages: 64,
            queue_bytes: 4 * 1024 * 1024,
            write_stall: Duration::from_secs(60),
        }
    }
}

/// A session a peer opened, accepted.
pub struct Incoming {
    pub stream: SessionStream,
    /// The peer as the head listed it.
    pub member: Member,
    /// `Desktop` or `Mobile`.
    pub role: Role,
}

/// Takes accepted sessions. Called on the Relay's IO thread: it must not block.
pub type Acceptor = Arc<dyn Fn(Incoming) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Not attached to a Connector, or it stopped.
    Detached,
    /// The peer is not a member of the trusted head.
    NotMember,
    /// The responder refused (`forbidden`).
    Refused(String),
    /// No answer before the open deadline.
    Timeout,
    /// The handshake failed.
    Noise(NoiseError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Detached => f.write_str("not connected to the relay"),
            SessionError::NotMember => f.write_str("not a member of the ring"),
            SessionError::Refused(e) => write!(f, "refused: {e}"),
            SessionError::Timeout => f.write_str("no answer in time"),
            SessionError::Noise(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// How a stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum End {
    /// Closed by either side: reads return 0.
    Clean(String),
    /// Killed: reads and writes fail.
    Killed(String),
}

struct Rx {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    end: Option<End>,
}

struct Shared {
    member: Member,
    role: Role,
    sid: Sid,
    /// Taken for sealing and queueing each message, so wire order is nonce order.
    sealer: Mutex<Sealer>,
    /// Set (under `sealer`) once a close is being sent: no write follows it.
    closing: std::sync::atomic::AtomicBool,
    opener: Mutex<Opener>,
    rx: Mutex<Rx>,
    cv: Condvar,
}

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn new(member: Member, role: Role, session: super::super::noise::Session) -> Arc<Shared> {
        let sid = session.sid();
        let (sealer, opener) = session.split();
        Arc::new(Shared {
            member,
            role,
            sid,
            sealer: Mutex::new(sealer),
            closing: std::sync::atomic::AtomicBool::new(false),
            opener: Mutex::new(opener),
            rx: Mutex::new(Rx {
                queue: VecDeque::new(),
                bytes: 0,
                end: None,
            }),
            cv: Condvar::new(),
        })
    }

    /// Ends the stream. A kill discards what was received and not yet read: nothing from
    /// a session that failed reaches the reader. Only an orderly close lets it drain.
    fn end(&self, end: End) {
        let mut rx = lk(&self.rx);
        if rx.end.is_none() {
            if matches!(end, End::Killed(_)) {
                rx.queue.clear();
                rx.bytes = 0;
            }
            rx.end = Some(end);
        }
        self.cv.notify_all();
    }

    fn killed(&self) -> Option<String> {
        match &lk(&self.rx).end {
            Some(End::Killed(why)) => Some(why.clone()),
            _ => None,
        }
    }

    fn ended(&self) -> bool {
        lk(&self.rx).end.is_some()
    }
}

struct Pending {
    init: Initiator,
    member: Member,
    /// The session, live as soon as the answer arrived (DATA may follow it at once).
    done: Option<Result<Arc<Shared>, SessionError>>,
}

#[derive(Default)]
struct St {
    live: HashMap<SignKey, Arc<Shared>>,
    pending: HashMap<SignKey, Pending>,
    fresh_in: Freshness,
    fresh_out: Freshness,
    detached: bool,
    /// The newest head [`Sessions::sweep`] saw: a session is installed only if this head
    /// (and the Connector's) still lists its peer as the handshake found it.
    swept: Option<SignedRoster>,
}

/// Whether `head` lists `m` with the same keys and role.
fn listed(head: &SignedRoster, m: &Member) -> bool {
    head.member(&m.sign_key)
        .is_some_and(|x| x.noise_key == m.noise_key && x.role == m.role)
}

struct Inner {
    cfg: SessionsConfig,
    accept: Option<Acceptor>,
    transport: Mutex<Weak<Connector>>,
    st: Mutex<St>,
    cv: Condvar,
}

/// The sessions of one device over one [`Connector`]; see the module docs.
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Inner>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Inner {
    fn connector(&self) -> Option<Arc<Connector>> {
        lk(&self.transport).upgrade()
    }

    /// Under the state lock: whether a session with `m` may be installed now. The heads it
    /// is checked against are updated before every sweep takes this lock, so an install
    /// either sees the newer head or is swept after it.
    fn may_install(&self, st: &St, m: &Member) -> bool {
        let connector_ok = self
            .connector()
            .is_some_and(|c| listed(c.chain().head(), m));
        let swept_ok = st.swept.as_ref().is_none_or(|h| listed(h, m));
        connector_ok && swept_ok
    }

    /// Sends one envelope, waiting out a full queue until `until`.
    fn send(&self, to: &SignKey, env: &[u8], until: Instant) -> Result<(), RingError> {
        let c = self
            .connector()
            .ok_or(RingError::Closed(super::wire::CloseReason::Local))?;
        loop {
            match c.send(to, env) {
                Err(RingError::Backpressure) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                r => return r,
            }
        }
    }

    /// Ends `s` and forgets it, if it is still the peer's session.
    fn kill(&self, s: &Arc<Shared>, why: &str) {
        self.forget(s);
        s.end(End::Killed(why.to_string()));
    }

    /// Removes `s` from the live sessions, if it is still the peer's.
    fn forget(&self, s: &Arc<Shared>) {
        let mut st = lk(&self.st);
        if st
            .live
            .get(&s.member.sign_key)
            .is_some_and(|cur| Arc::ptr_eq(cur, s))
        {
            st.live.remove(&s.member.sign_key);
        }
    }

    fn kill_peer(&self, peer: &SignKey, why: &str) {
        let (s, pending) = {
            let mut st = lk(&self.st);
            let p = st.pending.get_mut(peer).map(|p| {
                if p.done.is_none() {
                    p.done = Some(Err(SessionError::Timeout));
                }
            });
            (st.live.remove(peer), p.is_some())
        };
        if pending {
            self.cv.notify_all();
        }
        if let Some(s) = s {
            s.end(End::Killed(why.to_string()));
        }
    }

    fn kill_all(&self, why: &str) {
        let all: Vec<Arc<Shared>> = {
            let mut st = lk(&self.st);
            for p in st.pending.values_mut() {
                if p.done.is_none() {
                    p.done = Some(Err(SessionError::Timeout));
                }
            }
            st.live.drain().map(|(_, s)| s).collect()
        };
        self.cv.notify_all();
        for s in all {
            s.end(End::Killed(why.to_string()));
        }
    }

    fn stream(self: &Arc<Self>, s: Arc<Shared>) -> SessionStream {
        SessionStream {
            s,
            hub: Arc::downgrade(self),
            buf: Vec::new(),
            pos: 0,
        }
    }

    fn on_hello(self: &Arc<Self>, from: SignKey, env: &[u8]) {
        let Some(accept) = self.accept.clone() else {
            return;
        };
        let Some(c) = self.connector() else {
            return;
        };
        if !c.routing() {
            return;
        }
        let chain = c.chain();
        // Unknown keys, a `from` the handshake does not bind, junk: dropped without an
        // answer, so a stranger learns nothing.
        let Ok(hello) = read_hello(&self.cfg.keys, chain.ring_id(), &from, chain.head(), env)
        else {
            return;
        };
        let Some(role) = session_role(hello.member().role) else {
            if let Ok(reply) = hello.refuse("forbidden") {
                let _ = self.send(&from, &reply, Instant::now());
            }
            return;
        };
        let member = hello.member().clone();
        let (old, s, reply) = {
            let mut st = lk(&self.st);
            if st.detached || !st.fresh_in.fresh(&from, hello.ts()) {
                return;
            }
            if !self.may_install(&st, &member) {
                return;
            }
            let ts = hello.ts();
            let Ok((session, reply)) = hello.accept() else {
                return;
            };
            st.fresh_in.accept(from, ts);
            let s = Shared::new(member.clone(), role, session);
            (st.live.insert(from, s.clone()), s, reply)
        };
        if let Some(old) = old {
            old.end(End::Killed("replaced by a newer session".into()));
        }
        if self.send(&from, &reply, Instant::now()).is_err() {
            self.kill(&s, "could not answer");
            return;
        }
        let stream = self.stream(s);
        accept(Incoming {
            stream,
            member,
            role,
        });
    }

    fn on_answer(&self, from: SignKey, env: &[u8]) {
        let mut st = lk(&self.st);
        let Some(p) = st.pending.get_mut(&from) else {
            return;
        };
        if p.done.is_some() {
            return;
        }
        let member = p.member.clone();
        let r = match p.init.finish(env) {
            Err(NoiseError::WrongSession) | Err(NoiseError::Header(_)) => return,
            Ok(_) if !self.may_install(&st, &member) => {
                Err(SessionError::Noise(NoiseError::UnknownPeer))
            }
            Ok(session) => {
                let role = member.role;
                Ok(Shared::new(member, role, session))
            }
            Err(NoiseError::Refused(e)) => Err(SessionError::Refused(e)),
            Err(e) => Err(SessionError::Noise(e)),
        };
        // Live at once, on this (the IO) thread: the responder's first DATA may be next.
        let old = match &r {
            Ok(s) => st.live.insert(from, s.clone()),
            Err(_) => None,
        };
        if let Some(p) = st.pending.get_mut(&from) {
            p.done = Some(r);
        }
        drop(st);
        if let Some(old) = old {
            old.end(End::Killed("replaced by a newer session".into()));
        }
        self.cv.notify_all();
    }

    fn on_data(&self, from: SignKey, env: &[u8]) {
        let Some(s) = lk(&self.st).live.get(&from).cloned() else {
            return;
        };
        let r = lk(&s.opener).open(env);
        match r {
            Ok((inner::STREAM, body)) => {
                if body.is_empty() {
                    return;
                }
                let mut rx = lk(&s.rx);
                if rx.end.is_some() {
                    return;
                }
                if rx.queue.len() >= self.cfg.queue_messages
                    || rx.bytes + body.len() > self.cfg.queue_bytes
                {
                    drop(rx);
                    self.kill(&s, "the reader fell behind");
                    return;
                }
                rx.bytes += body.len();
                rx.queue.push_back(body);
                s.cv.notify_all();
            }
            Ok((inner::CLOSE, reason)) => {
                self.forget(&s);
                s.end(End::Clean(String::from_utf8_lossy(&reason).into_owned()));
            }
            // Reserved types (ping, rekey): authenticated, ignored in v1.
            Ok(_) => {}
            Err(NoiseError::WrongSession) | Err(NoiseError::Header(_)) => {}
            Err(e) => self.kill(&s, &e.to_string()),
        }
    }
}

impl Sessions {
    /// `accept`: take sessions peers open (a Daemon); `None`: only open them.
    pub fn new(cfg: SessionsConfig, accept: Option<Acceptor>) -> Sessions {
        Sessions {
            inner: Arc::new(Inner {
                cfg,
                accept,
                transport: Mutex::new(Weak::new()),
                st: Mutex::new(St::default()),
                cv: Condvar::new(),
            }),
        }
    }

    /// The Connector whose envelopes these are. Kept weakly.
    pub fn attach(&self, c: &Arc<Connector>) {
        *lk(&self.inner.transport) = Arc::downgrade(c);
    }

    /// Ends every session for good; later handshakes are ignored.
    pub fn stop(&self) {
        lk(&self.inner.st).detached = true;
        self.inner.kill_all("stopped");
    }

    /// The peers with a live session.
    pub fn peers(&self) -> Vec<SignKey> {
        lk(&self.inner.st).live.keys().copied().collect()
    }

    /// An envelope from the Connector.
    pub fn envelope(&self, from: SignKey, payload: &[u8]) {
        let Ok((h, _)) = Header::parse(payload) else {
            return;
        };
        match h.kind {
            Kind::Hs1 => self.inner.on_hello(from, payload),
            Kind::Hs2 => self.inner.on_answer(from, payload),
            Kind::Data => self.inner.on_data(from, payload),
        }
    }

    /// An `error` from the Relay: a refused envelope (any code, with `to`) kills that
    /// peer's session, since one of its messages may be lost; `entitlement_required` kills
    /// all of them.
    pub fn error(&self, code: &ErrorCode, to: Option<SignKey>) {
        if *code == ErrorCode::EntitlementRequired {
            self.inner
                .kill_all("the relay stopped routing (limited session)");
        } else if let Some(to) = to {
            self.inner
                .kill_peer(&to, &format!("the relay refused a message ({code})"));
        }
    }

    /// The Connector's state: anything but an unlimited `Connected` kills every session.
    pub fn state(&self, s: &LinkState) {
        if !matches!(s, LinkState::Connected { limited: false }) {
            self.inner.kill_all("the relay connection changed");
        }
    }

    pub fn presence(&self, key: SignKey, p: &MemberPresence) {
        if !matches!(p, MemberPresence::Online { .. }) {
            self.inner.kill_peer(&key, "the peer went offline");
        }
    }

    /// A head this device adopted: sessions with peers it no longer lists (with the same
    /// keys and role) die.
    pub fn sweep(&self, head: &SignedRoster) {
        let dead: Vec<Arc<Shared>> = {
            let mut st = lk(&self.inner.st);
            if st
                .swept
                .as_ref()
                .is_none_or(|h| h.ring_id() != head.ring_id() || h.version() <= head.version())
            {
                st.swept = Some(head.clone());
            }
            // Handshakes in flight with a peer the head no longer lists as they began: an
            // answer that arrives later never becomes a session.
            for p in st.pending.values_mut() {
                if p.done.is_none() && !listed(head, &p.member) {
                    p.done = Some(Err(SessionError::NotMember));
                }
            }
            st.live
                .values()
                .filter(|s| !listed(head, &s.member))
                .cloned()
                .collect()
        };
        self.inner.cv.notify_all();
        for s in dead {
            self.inner.kill(&s, "removed from the ring");
        }
    }

    /// Opens a session to `peer`.
    pub fn open(&self, peer: &SignKey) -> Result<SessionStream, SessionError> {
        let inner = &self.inner;
        let deadline = Instant::now() + inner.cfg.open_timeout;
        loop {
            if Instant::now() >= deadline {
                return Err(SessionError::Timeout);
            }
            if lk(&inner.st).detached {
                return Err(SessionError::Detached);
            }
            let Some(c) = inner.connector() else {
                return Err(SessionError::Detached);
            };
            if !c.routing() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let chain: RosterChain = c.chain();
            let member = chain
                .head()
                .member(peer)
                .cloned()
                .ok_or(SessionError::NotMember)?;
            let ts = lk(&inner.st).fresh_out.next(*peer, now_ms());
            let (init, hs1) = Initiator::start(&inner.cfg.keys, chain.ring_id(), &member, ts)
                .map_err(SessionError::Noise)?;
            lk(&inner.st).pending.insert(
                *peer,
                Pending {
                    init,
                    member: member.clone(),
                    done: None,
                },
            );
            let attempt_end = (Instant::now() + inner.cfg.hs_timeout).min(deadline);
            if inner.send(peer, &hs1, attempt_end).is_err() {
                lk(&inner.st).pending.remove(peer);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let done = {
                let mut st = lk(&inner.st);
                loop {
                    if let Some(d) = st.pending.get_mut(peer).and_then(|p| p.done.take()) {
                        st.pending.remove(peer);
                        break Some(d);
                    }
                    let left = attempt_end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        st.pending.remove(peer);
                        break None;
                    }
                    st = inner
                        .cv
                        .wait_timeout(st, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            };
            match done {
                Some(Ok(s)) => return Ok(inner.stream(s)),
                Some(Err(e @ SessionError::Refused(_))) => return Err(e),
                Some(Err(e @ SessionError::Noise(_))) => return Err(e),
                Some(Err(e @ SessionError::NotMember)) => return Err(e),
                // Refused envelope, connection change: try again shortly, with a new id.
                Some(Err(_)) => std::thread::sleep(Duration::from_millis(100)),
                None => {}
            }
        }
    }
}

/// One session's byte stream. Clones share the session; [`SessionStream::close`] ends it for
/// all of them.
pub struct SessionStream {
    s: Arc<Shared>,
    hub: Weak<Inner>,
    buf: Vec<u8>,
    pos: usize,
}

impl SessionStream {
    /// Another handle on the same session (a reader and a writer thread each own one).
    pub fn try_clone(&self) -> SessionStream {
        SessionStream {
            s: self.s.clone(),
            hub: self.hub.clone(),
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// The peer as the head listed it when the session opened.
    pub fn member(&self) -> &Member {
        &self.s.member
    }

    pub fn peer(&self) -> SignKey {
        self.s.member.sign_key
    }

    /// The peer's role: as the responder maps it for a session it accepted, the Roster's
    /// for one this side opened.
    pub fn role(&self) -> Role {
        self.s.role
    }

    pub fn sid(&self) -> Sid {
        self.s.sid
    }

    pub fn is_open(&self) -> bool {
        !self.s.ended()
    }

    /// Whether the session was killed (not closed in order): what was received and not
    /// yet read is gone.
    pub fn is_killed(&self) -> bool {
        self.s.killed().is_some()
    }

    /// Why the session ended, if it did.
    pub fn ended(&self) -> Option<String> {
        lk(&self.s.rx).end.as_ref().map(|e| match e {
            End::Clean(r) | End::Killed(r) => r.clone(),
        })
    }

    /// Waits up to `timeout` for the session to end; whether it did.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut rx = lk(&self.s.rx);
        while rx.end.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            rx = self
                .s
                .cv
                .wait_timeout(rx, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Ends the session: the peer is told (inside the encryption, so the Relay cannot forge
    /// it), and reads on this side return end of stream.
    pub fn close(&self, reason: &str) {
        if self.s.ended() {
            return;
        }
        if let Some(hub) = self.hub.upgrade() {
            {
                // Sealed and queued under the sealer's lock, like every write, and no write
                // after it: the close is the last message, in nonce order.
                let mut sealer = lk(&self.s.sealer);
                if self
                    .s
                    .closing
                    .swap(true, std::sync::atomic::Ordering::AcqRel)
                {
                    return;
                }
                if let Ok(env) = sealer.seal_close(reason) {
                    let until = Instant::now() + Duration::from_secs(1);
                    let _ = hub.send(&self.s.member.sign_key, &env, until);
                }
            }
            hub.forget(&self.s);
            self.s.end(End::Clean(reason.to_string()));
        } else {
            self.s.end(End::Clean(reason.to_string()));
        }
    }
}

fn killed(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, why.to_string())
}

impl io::Read for SessionStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // A killed session delivers nothing more, not even a chunk already taken.
        if let Some(why) = self.s.killed() {
            self.buf.clear();
            self.pos = 0;
            return Err(killed(&why));
        }
        if self.pos >= self.buf.len() {
            let mut rx = lk(&self.s.rx);
            loop {
                if let Some(next) = rx.queue.pop_front() {
                    rx.bytes -= next.len();
                    self.buf = next;
                    self.pos = 0;
                    break;
                }
                match &rx.end {
                    Some(End::Clean(_)) => return Ok(0),
                    Some(End::Killed(why)) => return Err(killed(why)),
                    None => {}
                }
                rx = self.s.cv.wait(rx).unwrap_or_else(|e| e.into_inner());
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl io::Write for SessionStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| killed("the relay connection is gone"))?;
        let take = data.len().min(super::super::noise::MAX_STREAM_CHUNK);
        // Sealed and queued under the sealer's lock, so wire order is nonce order.
        let mut sealer = lk(&self.s.sealer);
        if self.s.closing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "session closing"));
        }
        if let Some(e) = lk(&self.s.rx).end.clone() {
            return Err(match e {
                End::Clean(_) => io::Error::new(io::ErrorKind::BrokenPipe, "session closed"),
                End::Killed(why) => killed(&why),
            });
        }
        let env = match sealer.seal(inner::STREAM, &data[..take]) {
            Ok(e) => e,
            Err(e) => {
                drop(sealer);
                hub.kill(&self.s, &e.to_string());
                return Err(killed(&e.to_string()));
            }
        };
        let until = Instant::now() + hub.cfg.write_stall;
        if let Err(e) = hub.send(&self.s.member.sign_key, &env, until) {
            drop(sealer);
            let why = format!("cannot send: {e}");
            hub.kill(&self.s, &why);
            return Err(killed(&why));
        }
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// [`ConnectorEvents`] that feed a [`Sessions`] and then pass everything on to `next`.
pub struct SessionEvents {
    pub sessions: Sessions,
    pub next: Option<Arc<dyn ConnectorEvents>>,
}

impl ConnectorEvents for SessionEvents {
    fn state(&self, s: &LinkState) {
        self.sessions.state(s);
        if let Some(n) = &self.next {
            n.state(s);
        }
    }

    fn roster(&self, chain: &RosterChain) {
        self.sessions.sweep(chain.head());
        if let Some(n) = &self.next {
            n.roster(chain);
        }
    }

    fn presence(&self, key: SignKey, p: &MemberPresence) {
        self.sessions.presence(key, p);
        if let Some(n) = &self.next {
            n.presence(key, p);
        }
    }

    fn moved(&self, m: &MoveState) {
        if let Some(n) = &self.next {
            n.moved(m);
        }
    }

    fn envelope(&self, from: SignKey, payload: Vec<u8>) {
        self.sessions.envelope(from, &payload);
        if let Some(n) = &self.next {
            n.envelope(from, payload);
        }
    }

    fn error(&self, code: &ErrorCode, to: Option<SignKey>) {
        self.sessions.error(code, to);
        if let Some(n) = &self.next {
            n.error(code, to);
        }
    }
    fn entitlement(&self, token: Option<&str>) {
        if let Some(n) = &self.next {
            n.entitlement(token);
        }
    }
}

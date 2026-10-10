//! Conversation streams of agent Terminals for the Chat View (capability `session.stream`).
//!
//! A connection subscribes to a Terminal, never to a file: the session file is derived from
//! the Terminal's launch spec, which the Daemon already vets, and opened confined to the
//! agent's session storage on every read. One worker thread does every read, off the
//! connection and PTY threads, so a subscription's `res` is always queued before any of its
//! `session.append`s, and an unsubscribe's `res` after the last.
//!
//! The worker polls each subscribed file every [`Config::session_poll`](super::Config) (hooks
//! fire only at turn boundaries, so polling is what shows a turn as it happens) and reads at
//! once when [`SessionStreams::wake`] names the Terminal (an agent report, a status change, a
//! relink).
//!
//! Every result is checked once more under the registry lock right before it is queued: the
//! connection is still there, the Terminal read is still the one listed and still visible to
//! the connection's role, and its session, agent and working directory are unchanged.
//! Otherwise it is dropped and the next read starts over, so nothing of a session the
//! Terminal has left reaches a connection after the change.
//!
//! Each subscription has a generation, renewed on every reset (another session, the file
//! replaced, truncated or appearing). Pages and appends carry it, entry ids include it, and a
//! `session.page` for an older generation is refused.
//!
//! Bounds: at most [`Config::max_session_subs`](super::Config) subscriptions and
//! [`Config::max_session_requests`](super::Config) queued requests per connection and
//! [`Config::max_session_queue`](super::Config) over all; wakes
//! coalesce per Terminal; every message is checked against
//! [`CHAT_PAGE_MAX_BYTES`] before it is queued; appends wait while the connection has more than
//! [`APPEND_QUEUE_GATE`] queued (they are control frames, which are never dropped); a Codex
//! rollout is looked for with a bounded walk and a backoff.

use super::conn::reply;
use super::outbox::Outbox;
use super::registry::{frame, Daemon, Registry};
use super::role::{self, Role};
use super::terminal::Terminal;
use super::{ConnId, TestPoint};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::chat::{self, ChatAgent, SessionFile};
use xshell_core::launch::LaunchSpec;
use xshell_protocol::msg::{
    encode_res, ChatEntry, ServerMsg, SessionPage, CHAT_PAGE_DEFAULT, CHAT_PAGE_MAX,
    CHAT_PAGE_MAX_BYTES, NOT_SUBSCRIBED, NO_SESSION_STREAM, SESSION_CHANGED,
};

/// Appends to a connection wait while it has more than this queued.
pub(crate) const APPEND_QUEUE_GATE: usize = 1024 * 1024;
/// The most directory entries one look for a Codex rollout visits.
const CODEX_WALK_MAX: usize = 100_000;
/// The longest wait before looking for a missing Codex rollout again.
const RESOLVE_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Attempts of a subscribe whose Terminal changes while it is read.
const SUBSCRIBE_ATTEMPTS: usize = 3;

pub(crate) const TOO_MANY_SUBS: &str = "too many session subscriptions";
pub(crate) const TOO_MANY_REQUESTS: &str = "too many session requests";

enum Kind {
    Subscribe {
        limit: Option<u32>,
    },
    Page {
        gen: u64,
        before: u64,
        limit: Option<u32>,
    },
    Unsubscribe,
}

struct Req {
    conn: ConnId,
    role: Role,
    ob: Weak<Outbox>,
    id: Option<u64>,
    terminal: Uuid,
    kind: Kind,
}

#[derive(Default)]
struct Queue {
    reqs: VecDeque<Req>,
    /// Requests queued or in progress, per connection.
    pending: HashMap<ConnId, usize>,
    /// Terminals to read at once, coalesced.
    wakes: HashSet<Uuid>,
    /// Connections gone, whose subscriptions end (at most one entry per connection, and
    /// never refused: cleanup does not depend on admission).
    dropped: HashSet<ConnId>,
    stop: bool,
}

struct Inner {
    poll: Duration,
    max_subs: usize,
    max_requests: usize,
    /// Requests queued over all connections.
    max_queue: usize,
    queue: Mutex<Queue>,
    cv: Condvar,
    daemon: OnceLock<Weak<Daemon>>,
    subs: AtomicUsize,
    next_gen: AtomicU64,
}

pub(crate) struct SessionStreams {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

fn lock(m: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl SessionStreams {
    pub fn new(
        poll: Duration,
        max_subs: usize,
        max_requests: usize,
        max_queue: usize,
    ) -> SessionStreams {
        SessionStreams {
            inner: Arc::new(Inner {
                poll,
                max_subs,
                max_requests,
                max_queue,
                queue: Mutex::new(Queue::default()),
                cv: Condvar::new(),
                daemon: OnceLock::new(),
                subs: AtomicUsize::new(0),
                next_gen: AtomicU64::new(1),
            }),
            worker: Mutex::new(None),
        }
    }

    /// Serves `d` from now on: starts the worker.
    pub fn bind(&self, d: Weak<Daemon>) {
        if self.inner.daemon.set(d).is_err() {
            return;
        }
        let inner = self.inner.clone();
        match std::thread::Builder::new()
            .name("session-stream".into())
            .spawn(move || Worker::new(inner).run())
        {
            Ok(h) => *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(h),
            Err(e) => crate::log!("ERROR", "cannot start the session-stream worker: {e}"),
        }
    }

    /// Stops the worker and waits for it: a read in progress finishes first.
    pub fn stop(&self) {
        {
            let mut q = lock(&self.inner.queue);
            q.stop = true;
            q.reqs.clear();
            q.wakes.clear();
        }
        self.inner.cv.notify_all();
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = worker {
            let _ = h.join();
        }
    }

    fn enqueue(&self, ob: &Arc<Outbox>, req: Req) {
        let id = req.id;
        let refused = {
            let mut q = lock(&self.inner.queue);
            let n = q.pending.get(&req.conn).copied().unwrap_or(0);
            if q.stop {
                Some("xshelld is exiting")
            } else if n >= self.inner.max_requests || q.reqs.len() >= self.inner.max_queue {
                Some(TOO_MANY_REQUESTS)
            } else {
                q.pending.insert(req.conn, n + 1);
                q.reqs.push_back(req);
                None
            }
        };
        match refused {
            Some(e) => reply(ob, id, Err(e.to_string())),
            None => self.inner.cv.notify_all(),
        }
    }

    /// `session.subscribe`; answered from the worker.
    pub fn subscribe(
        &self,
        conn: ConnId,
        role: Role,
        ob: &Arc<Outbox>,
        id: Option<u64>,
        terminal: Uuid,
        limit: Option<u32>,
    ) {
        self.enqueue(
            ob,
            Req::new(conn, role, ob, id, terminal, Kind::Subscribe { limit }),
        );
    }

    /// `session.page`; answered from the worker.
    #[allow(clippy::too_many_arguments)]
    pub fn page(
        &self,
        conn: ConnId,
        role: Role,
        ob: &Arc<Outbox>,
        id: Option<u64>,
        terminal: Uuid,
        gen: u64,
        before: u64,
        limit: Option<u32>,
    ) {
        let kind = Kind::Page { gen, before, limit };
        self.enqueue(ob, Req::new(conn, role, ob, id, terminal, kind));
    }

    /// `session.unsubscribe`; answered from the worker.
    pub fn unsubscribe(
        &self,
        conn: ConnId,
        role: Role,
        ob: &Arc<Outbox>,
        id: Option<u64>,
        terminal: Uuid,
    ) {
        self.enqueue(
            ob,
            Req::new(conn, role, ob, id, terminal, Kind::Unsubscribe),
        );
    }

    /// Read `terminal`'s subscribed session now. Never blocks on I/O or another lock: safe on
    /// any thread, also under the registry lock.
    pub fn wake(&self, terminal: Uuid) {
        let mut q = lock(&self.inner.queue);
        if q.stop || self.inner.subs.load(Ordering::SeqCst) == 0 {
            return;
        }
        q.wakes.insert(terminal);
        drop(q);
        self.inner.cv.notify_all();
    }

    /// `conn` is gone (call after it left the registry): its queued requests are dropped
    /// and its subscriptions end. Never blocks behind a read.
    pub fn drop_conn(&self, conn: ConnId) {
        let mut q = lock(&self.inner.queue);
        q.reqs.retain(|r| r.conn != conn);
        q.pending.remove(&conn);
        if !q.stop {
            q.dropped.insert(conn);
        }
        drop(q);
        self.inner.cv.notify_all();
    }

    /// Test hook: the subscriptions held.
    pub fn count(&self) -> usize {
        self.inner.subs.load(Ordering::SeqCst)
    }
}

impl Req {
    fn new(
        conn: ConnId,
        role: Role,
        ob: &Arc<Outbox>,
        id: Option<u64>,
        terminal: Uuid,
        kind: Kind,
    ) -> Req {
        Req {
            conn,
            role,
            ob: Arc::downgrade(ob),
            id,
            terminal,
            kind,
        }
    }
}

/// What a subscription's content depends on in the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    agent: Option<ChatAgent>,
    session: Option<String>,
    cwd: String,
}

impl Key {
    fn of(spec: &LaunchSpec) -> Key {
        Key {
            agent: chat::chat_agent(spec),
            session: chat::stream_session(spec).map(str::to_string),
            cwd: spec.cwd.clone(),
        }
    }
}

/// The session file as opened for a subscription.
struct Opened {
    id: (u64, u64),
    /// Where the next forward read starts (the end of what was read).
    cursor: u64,
    /// The bytes before `cursor` when it was set, to notice the file rewritten in place.
    mark: Vec<u8>,
}

struct Sub {
    role: Role,
    ob: Weak<Outbox>,
    gen: u64,
    key: Key,
    file: Option<SessionFile>,
    /// When to look for a missing Codex rollout again, and the wait after that.
    resolve_at: Instant,
    backoff: Duration,
    opened: Option<Opened>,
}

/// A subscription's state read fresh: its newest page and where reading goes on.
struct Snapshot {
    gen: u64,
    key: Key,
    file: Option<SessionFile>,
    opened: Option<Opened>,
    items: Vec<ChatEntry>,
    before: Option<u64>,
    resolve_at: Instant,
    backoff: Duration,
}

/// Whether a result read for a connection may still be published.
enum Check {
    Ok,
    /// The connection is gone: nothing to answer.
    Gone,
    /// The Terminal is no longer listed or visible: the refusal `listed` gives.
    Refused(String),
    /// The Terminal changed (another instance, session, agent or directory): read again.
    Changed,
}

struct Worker {
    inner: Arc<Inner>,
    subs: HashMap<(ConnId, Uuid), Sub>,
    next_tick: Instant,
}

fn clamp_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(CHAT_PAGE_DEFAULT).clamp(1, CHAT_PAGE_MAX) as usize
}

/// Under the registry lock: may a result read from `t` for connection `conn` be queued?
fn check(
    reg: &Registry,
    d: &Daemon,
    conn: ConnId,
    ob: &Arc<Outbox>,
    role: Role,
    t: &Arc<Terminal>,
    key: &Key,
) -> Check {
    if !reg.conns.get(&conn).is_some_and(|p| Arc::ptr_eq(&p.ob, ob)) {
        return Check::Gone;
    }
    match role::listed(reg, role, &t.id) {
        Err(e) => Check::Refused(e),
        Ok(c) if !Arc::ptr_eq(c, t) || !d.is_current(reg, t) => Check::Changed,
        Ok(_) if Key::of(&t.spec()) != *key => Check::Changed,
        Ok(_) => Check::Ok,
    }
}

fn append_frame(
    terminal: Uuid,
    gen: u64,
    reset: bool,
    session: Option<String>,
    items: Vec<ChatEntry>,
    before: Option<u64>,
) -> Option<Arc<[u8]>> {
    frame(&ServerMsg::SessionAppend {
        terminal,
        gen,
        reset,
        session,
        items,
        before,
    })
}

/// `f` if it is within the per-message budget. Entries are collected so that it always is;
/// one over it is a bug, logged and not sent.
fn within_budget(f: Arc<[u8]>) -> Option<Arc<[u8]>> {
    if f.len() <= CHAT_PAGE_MAX_BYTES {
        Some(f)
    } else {
        crate::log!(
            "ERROR",
            "session message of {} bytes over its budget; not sent",
            f.len()
        );
        None
    }
}

/// Answer a subscribe or page request with `page`, within the per-message budget.
fn reply_page(ob: &Outbox, id: Option<u64>, page: SessionPage) {
    let Some(id) = id else {
        return;
    };
    let v = serde_json::to_value(page).unwrap_or(Value::Null);
    let f = within_budget(Arc::from(encode_res(id, Ok(v))))
        .unwrap_or_else(|| Arc::from(encode_res(id, Err("response too large".into()))));
    ob.push_control(f);
}

/// Whether the file still holds what the subscription read up to `cursor`: at least that
/// long, with the same bytes just before it.
fn intact(f: &mut std::fs::File, cursor: u64, mark: &[u8]) -> bool {
    f.metadata().is_ok_and(|m| m.len() >= cursor)
        && chat::tail_mark(f, cursor).as_deref() == Some(mark)
}

impl Worker {
    fn new(inner: Arc<Inner>) -> Worker {
        Worker {
            inner,
            subs: HashMap::new(),
            next_tick: Instant::now(),
        }
    }

    fn daemon(&self) -> Option<Arc<Daemon>> {
        self.inner.daemon.get().and_then(Weak::upgrade)
    }

    fn set_count(&self) {
        self.inner.subs.store(self.subs.len(), Ordering::SeqCst);
    }

    /// Each round: end the subscriptions of gone connections, read what is due (woken
    /// Terminals, or everything once per poll), then answer one request. Reads and requests
    /// alternate, so one connection's requests never hold up another's appends for long.
    fn run(mut self) {
        let mut woken: HashSet<Uuid> = HashSet::new();
        loop {
            let (dropped, req, tick) = {
                let mut q = lock(&self.inner.queue);
                loop {
                    if q.stop {
                        return;
                    }
                    woken.extend(q.wakes.drain());
                    let now = Instant::now();
                    let tick = !self.subs.is_empty() && now >= self.next_tick;
                    if !q.dropped.is_empty() || !q.reqs.is_empty() || tick || !woken.is_empty() {
                        break (std::mem::take(&mut q.dropped), q.reqs.pop_front(), tick);
                    }
                    q = if self.subs.is_empty() {
                        self.inner.cv.wait(q).unwrap_or_else(|e| e.into_inner())
                    } else {
                        let left = self.next_tick.saturating_duration_since(now);
                        self.inner
                            .cv
                            .wait_timeout(q, left)
                            .unwrap_or_else(|e| e.into_inner())
                            .0
                    };
                }
            };
            if !dropped.is_empty() {
                self.subs.retain(|(c, _), _| !dropped.contains(c));
                self.set_count();
            }
            let Some(d) = self.daemon() else {
                return;
            };
            let due: Vec<(ConnId, Uuid)> = if tick {
                self.next_tick = Instant::now() + self.inner.poll;
                woken.clear();
                self.subs.keys().copied().collect()
            } else {
                let w = std::mem::take(&mut woken);
                self.subs
                    .keys()
                    .filter(|(_, t)| w.contains(t))
                    .copied()
                    .collect()
            };
            for k in due {
                if self.tick(&d, k) {
                    woken.insert(k.1);
                }
            }
            if let Some(r) = req {
                let conn = r.conn;
                self.handle(&d, r);
                let mut q = lock(&self.inner.queue);
                if let Some(n) = q.pending.get_mut(&conn) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        q.pending.remove(&conn);
                    }
                }
            }
            self.set_count();
        }
    }

    fn handle(&mut self, d: &Arc<Daemon>, r: Req) {
        let Some(ob) = r.ob.upgrade() else {
            return;
        };
        match r.kind {
            Kind::Subscribe { limit } => self.subscribe(d, &r, &ob, clamp_limit(limit)),
            Kind::Page { gen, before, limit } => {
                let res = self.older(d, &r, &ob, gen, before, clamp_limit(limit));
                if let Some(res) = res {
                    reply(&ob, r.id, res);
                }
            }
            Kind::Unsubscribe => {
                self.subs.remove(&(r.conn, r.terminal));
                self.set_count();
                reply(&ob, r.id, Ok(Value::Null));
            }
        }
    }

    /// The Terminal `role` may act on under `id`, if `conn` is still there.
    fn lookup(
        d: &Daemon,
        conn: ConnId,
        ob: &Arc<Outbox>,
        role: Role,
        id: &Uuid,
    ) -> Option<Result<Arc<Terminal>, String>> {
        let reg = d.reg.lock().unwrap();
        if !reg.conns.get(&conn).is_some_and(|p| Arc::ptr_eq(&p.ob, ob)) {
            return None;
        }
        Some(role::listed(&reg, role, id).cloned())
    }

    fn subscribe(&mut self, d: &Arc<Daemon>, r: &Req, ob: &Arc<Outbox>, limit: usize) {
        let key = (r.conn, r.terminal);
        for _ in 0..SUBSCRIBE_ATTEMPTS {
            let t = match Self::lookup(d, r.conn, ob, r.role, &r.terminal) {
                None => return,
                Some(Err(e)) => return reply(ob, r.id, Err(e)),
                Some(Ok(t)) => t,
            };
            let spec = t.spec();
            if chat::chat_agent(&spec).is_none() {
                return reply(ob, r.id, Err(NO_SESSION_STREAM.into()));
            }
            let held = self.subs.keys().filter(|(c, _)| *c == r.conn).count();
            if !self.subs.contains_key(&key) && held >= self.inner.max_subs {
                return reply(ob, r.id, Err(TOO_MANY_SUBS.into()));
            }
            let snap = self.snapshot(d, &spec, None, limit);
            d.test_point(r.terminal, TestPoint::SessionRead);
            let reg = d.reg.lock().unwrap();
            match check(&reg, d, r.conn, ob, r.role, &t, &snap.key) {
                Check::Gone => return,
                Check::Refused(e) => {
                    drop(reg);
                    return reply(ob, r.id, Err(e));
                }
                Check::Changed => continue,
                Check::Ok => {}
            }
            let page = SessionPage {
                gen: snap.gen,
                session: snap.key.session.clone(),
                items: snap.items,
                before: snap.before,
            };
            // Just read: the first poll of the first subscription is one interval away (the
            // tick time is stale while there were none).
            if self.subs.is_empty() {
                self.next_tick = Instant::now() + self.inner.poll;
            }
            // Held (and counted, so wakes are taken) before the answer goes out.
            self.subs.insert(
                key,
                Sub {
                    role: r.role,
                    ob: Arc::downgrade(ob),
                    gen: snap.gen,
                    key: snap.key,
                    file: snap.file,
                    resolve_at: snap.resolve_at,
                    backoff: snap.backoff,
                    opened: snap.opened,
                },
            );
            self.set_count();
            reply_page(ob, r.id, page);
            drop(reg);
            return;
        }
        reply(ob, r.id, Err(SESSION_CHANGED.into()));
    }

    /// `session.page`: `None` when the connection is gone.
    fn older(
        &mut self,
        d: &Arc<Daemon>,
        r: &Req,
        ob: &Arc<Outbox>,
        gen: u64,
        before: u64,
        limit: usize,
    ) -> Option<Result<Value, String>> {
        let changed = || Some(Err(SESSION_CHANGED.to_string()));
        let Some(sub) = self.subs.get(&(r.conn, r.terminal)) else {
            return Some(Err(NOT_SUBSCRIBED.into()));
        };
        if sub.gen != gen {
            return changed();
        }
        let (Some(sf), Some(op)) = (sub.file.clone(), sub.opened.as_ref()) else {
            return changed();
        };
        if before > op.cursor {
            return Some(Err("invalid cursor".into()));
        }
        let (want_id, key, cursor, mark) = (op.id, sub.key.clone(), op.cursor, op.mark.clone());
        let t = match Self::lookup(d, r.conn, ob, r.role, &r.terminal)? {
            Err(e) => return Some(Err(e)),
            Ok(t) => t,
        };
        if Key::of(&t.spec()) != key {
            self.wake_self(r.terminal);
            return changed();
        }
        // The same file, still holding what this generation read: a file rewritten in place
        // must not mix into it (the reset it needs is scheduled).
        let Some(mut f) = chat::open(&sf).filter(|f| chat::identity(f) == Some(want_id)) else {
            self.wake_self(r.terminal);
            return changed();
        };
        if !intact(&mut f, cursor, &mark) {
            self.wake_self(r.terminal);
            return changed();
        }
        let (items, before) = chat::page(&mut f, sf.agent, gen, before, limit);
        d.test_point(r.terminal, TestPoint::SessionRead);
        if !intact(&mut f, cursor, &mark) {
            self.wake_self(r.terminal);
            return changed();
        }
        let reg = d.reg.lock().unwrap();
        match check(&reg, d, r.conn, ob, r.role, &t, &key) {
            Check::Gone => None,
            Check::Refused(e) => Some(Err(e)),
            Check::Changed => changed(),
            Check::Ok => {
                let page = SessionPage {
                    gen,
                    session: key.session,
                    items,
                    before,
                };
                // Answered under the lock, like every publish.
                reply_page(ob, r.id, page);
                None
            }
        }
    }

    fn wake_self(&self, terminal: Uuid) {
        lock(&self.inner.queue).wakes.insert(terminal);
    }

    fn new_gen(&self) -> u64 {
        self.inner.next_gen.fetch_add(1, Ordering::SeqCst)
    }

    /// The session file of `spec`, looked for now (a Codex rollout with a bounded walk).
    fn resolve(d: &Daemon, spec: &LaunchSpec) -> Option<SessionFile> {
        chat::session_file_within(&d.ctx, spec, CODEX_WALK_MAX)
    }

    /// A fresh read of `spec`'s session under a new generation: its newest page, and the
    /// end of its complete lines as the forward cursor (one boundary for both).
    fn snapshot(
        &self,
        d: &Daemon,
        spec: &LaunchSpec,
        cached: Option<SessionFile>,
        limit: usize,
    ) -> Snapshot {
        let gen = self.new_gen();
        let key = Key::of(spec);
        let now = Instant::now();
        let file = cached.or_else(|| key.session.as_ref().and_then(|_| Self::resolve(d, spec)));
        let mut snap = Snapshot {
            gen,
            key,
            file: None,
            opened: None,
            items: Vec::new(),
            before: None,
            resolve_at: now,
            backoff: self.inner.poll,
        };
        let Some(sf) = file else {
            snap.resolve_at = now + snap.backoff;
            snap.backoff = (snap.backoff * 2).min(RESOLVE_BACKOFF_MAX);
            return snap;
        };
        if let Some((items, before, opened)) = Self::read_page(&sf, gen, limit) {
            snap.items = items;
            snap.before = before;
            snap.opened = Some(opened);
        }
        snap.file = Some(sf);
        snap
    }

    fn read_page(
        sf: &SessionFile,
        gen: u64,
        limit: usize,
    ) -> Option<(Vec<ChatEntry>, Option<u64>, Opened)> {
        let mut f = chat::open(sf)?;
        let id = chat::identity(&f)?;
        let len = f.metadata().ok()?.len();
        let end = chat::complete_end(&mut f, len);
        let (items, before) = chat::page(&mut f, sf.agent, gen, end, limit);
        let mark = chat::tail_mark(&mut f, end)?;
        Some((
            items,
            before,
            Opened {
                id,
                cursor: end,
                mark,
            },
        ))
    }

    /// Read one subscription: a reset when its session changed, its file appeared or was
    /// replaced or truncated, else what was appended. `true`: there is more to read now.
    fn tick(&mut self, d: &Arc<Daemon>, k: (ConnId, Uuid)) -> bool {
        let Some(sub) = self.subs.get(&k) else {
            return false;
        };
        let Some(ob) = sub.ob.upgrade() else {
            self.subs.remove(&k);
            return false;
        };
        let t = match Self::lookup(d, k.0, &ob, sub.role, &k.1) {
            Some(Ok(t)) => t,
            // Gone or no longer visible: the connection learns it from `terminals`.
            _ => {
                self.subs.remove(&k);
                return false;
            }
        };
        let spec = t.spec();
        let key = Key::of(&spec);
        if key.agent.is_none() {
            self.subs.remove(&k);
            return false;
        }
        if key != sub.key {
            return self.reset(d, k, &ob, &t, &spec, None);
        }
        let now = Instant::now();
        let sub = self.subs.get_mut(&k).expect("present");
        if sub.file.is_none() {
            if key.session.is_none() || now < sub.resolve_at {
                return false;
            }
            match Self::resolve(d, &spec) {
                Some(sf) => sub.file = Some(sf),
                None => {
                    sub.resolve_at = now + sub.backoff;
                    sub.backoff = (sub.backoff * 2).min(RESOLVE_BACKOFF_MAX);
                    return false;
                }
            }
        }
        let sf = sub.file.clone().expect("resolved");
        let Some(mut f) = chat::open(&sf) else {
            // Gone or refused for now; a Codex rollout is looked for again (with backoff).
            if sf.agent == ChatAgent::Codex {
                sub.file = None;
                sub.resolve_at = now + sub.backoff;
                sub.backoff = (sub.backoff * 2).min(RESOLVE_BACKOFF_MAX);
            }
            sub.opened = None;
            return false;
        };
        let id = chat::identity(&f);
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let Some(op) = sub.opened.as_ref() else {
            // The file appeared (or came back).
            return self.reset(d, k, &ob, &t, &spec, Some(sf));
        };
        if Some(op.id) != id
            || len < op.cursor
            || chat::tail_mark(&mut f, op.cursor).as_deref() != Some(&op.mark[..])
        {
            return self.reset(d, k, &ob, &t, &spec, Some(sf));
        }
        if len == op.cursor || ob.queued_bytes() > APPEND_QUEUE_GATE {
            return false;
        }
        let (gen, cursor, file_id) = (sub.gen, op.cursor, op.id);
        let fwd = chat::read_forward(&mut f, sf.agent, gen, cursor, len, CHAT_PAGE_MAX as usize);
        let mark = chat::tail_mark(&mut f, fwd.next);
        d.test_point(k.1, TestPoint::SessionRead);
        let reg = d.reg.lock().unwrap();
        match check(&reg, d, k.0, &ob, sub.role, &t, &key) {
            Check::Ok => {}
            Check::Gone | Check::Refused(_) => {
                drop(reg);
                self.subs.remove(&k);
                return false;
            }
            // Read again next time, from the same cursor.
            Check::Changed => return false,
        }
        if !fwd.items.is_empty() {
            if let Some(fr) = append_frame(k.1, gen, false, key.session.clone(), fwd.items, None)
                .and_then(within_budget)
            {
                ob.push_control(fr);
            }
        }
        drop(reg);
        let Some(mark) = mark else {
            sub.opened = None;
            return false;
        };
        sub.opened = Some(Opened {
            id: file_id,
            cursor: fwd.next,
            mark,
        });
        fwd.more
    }

    /// Move subscription `k` to a new generation: its newest page, sent as a reset.
    /// `cached`: the session file, when the session itself did not change.
    fn reset(
        &mut self,
        d: &Arc<Daemon>,
        k: (ConnId, Uuid),
        ob: &Arc<Outbox>,
        t: &Arc<Terminal>,
        spec: &LaunchSpec,
        cached: Option<SessionFile>,
    ) -> bool {
        if ob.queued_bytes() > APPEND_QUEUE_GATE {
            return false;
        }
        let role = self.subs[&k].role;
        let snap = self.snapshot(d, spec, cached, CHAT_PAGE_DEFAULT as usize);
        d.test_point(k.1, TestPoint::SessionRead);
        let reg = d.reg.lock().unwrap();
        match check(&reg, d, k.0, ob, role, t, &snap.key) {
            Check::Ok => {}
            Check::Gone | Check::Refused(_) => {
                drop(reg);
                self.subs.remove(&k);
                return false;
            }
            Check::Changed => return false,
        }
        if let Some(fr) = append_frame(
            k.1,
            snap.gen,
            true,
            snap.key.session.clone(),
            snap.items,
            snap.before,
        )
        .and_then(within_budget)
        {
            ob.push_control(fr);
        }
        drop(reg);
        if let Some(sub) = self.subs.get_mut(&k) {
            sub.gen = snap.gen;
            sub.key = snap.key;
            sub.file = snap.file;
            sub.resolve_at = snap.resolve_at;
            sub.backoff = snap.backoff;
            sub.opened = snap.opened;
        }
        false
    }
}

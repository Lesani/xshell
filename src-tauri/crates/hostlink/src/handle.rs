//! One Host as the Desktop sees it: status, the last `terminals` list, the current link, and
//! the attachment table (Terminal → output sink). Attachments outlive links: after a
//! reconnect, or a replaced configuration, the supervisor attaches them again.
//!
//! Locking: `State` is one mutex. Link methods may be called under it (they only enqueue);
//! `Link::close` and user callbacks never are. Sinks and the observer are called under it,
//! which keeps their events in order; they must not call back into the handle.
//!
//! The sync paths the UI thread uses (input, resize, status snapshots, kick) never take
//! `State`: they read small separately locked copies (`ep`, `sizes`, `snap`) that are only
//! ever held for a copy or an enqueue, so a sink blocked inside a delivery cannot stall them.
//! Stopping never takes `State` either (see `stop_begin`).

use crate::cancel::CancelToken;
use crate::config::HostConfig;
use crate::errors::{HostError, HostErrorCode};
use crate::link::{Link, LinkEvents, Waiter};
use crate::manager::ManagerConfig;
use crate::status::{HostSnapshot, HostStatus, Phase, StatusKind};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_protocol::msg::{ClientMsg, OpenSpec, TerminalInfo};

/// Where one Tab's output goes.
pub trait TermSink: Send + Sync {
    /// `false`: the receiver is gone; the handle retires this sink.
    fn data(&self, bytes: &[u8]) -> bool;
    /// The Terminal ended. `bytes` is everything this sink received through `data`, so the
    /// receiver can apply the exit only after that much output (the exit watermark).
    fn exit(&self, code: i32, bytes: u64);
}

const SAVE_FILE_TIMEOUT: Duration = Duration::from_secs(120);
/// The hello capability of Daemons that serve `term.relaunch`.
const RELAUNCH_CAPABILITY: &str = "term.relaunch";
/// The hello capability of Daemons that run a [`LaunchSpec`]'s `launchPrefix`.
///
/// [`LaunchSpec`]: xshell_core::launch::LaunchSpec
const LAUNCH_PREFIX_CAPABILITY: &str = "launch.prefix";

pub(crate) struct SinkSlot {
    sink: Arc<dyn TermSink>,
    bytes: u64,
}

impl SinkSlot {
    fn new(sink: Arc<dyn TermSink>) -> Self {
        Self { sink, bytes: 0 }
    }
}

pub(crate) struct Attachment {
    /// Receives output now. `None`: its receiver went away while a replacement is pending.
    sink: Option<SinkSlot>,
    /// A replacement installed before its `term.attach` was sent; it takes over when that
    /// request's reply arrives (output before the reply belongs to the previous attach).
    pending: Option<(u64, SinkSlot)>,
    /// The link generation of the latest attach that was actually sent.
    pub(crate) attached_gen: u64,
    /// The epoch of the latest attach request.
    epoch: u64,
    /// Attach requests (by epoch) awaiting a reply. Lists never prune an attachment with
    /// one in flight.
    inflight: HashSet<u64>,
}

pub(crate) struct State {
    pub cfg: HostConfig,
    pub status: HostStatus,
    published: Option<HostStatus>,
    pub terminals: Option<Vec<TerminalInfo>>,
    /// The generation of the newest link (events of older links are ignored).
    pub gen: u64,
    /// The usable link; always of generation `gen`. Change it only through `set_link`.
    link: Option<Arc<Link>>,
    pub link_closed: bool,
    pub close_reason: Option<String>,
    /// The generation of the newest `terminals` list.
    pub list_gen: u64,
    pub atts: HashMap<Uuid, Attachment>,
    epoch: u64,
    /// An upgrade is in progress: phase `upgrading` until connected again.
    pub upgrading: bool,
    /// The Daemon acknowledged `daemon.upgrade`; it must close the link by then.
    pub upgrade_deadline: Option<Instant>,
    /// After an upgrade, the next Daemon must report the Desktop's version.
    pub upgrade_expect: bool,
}

impl State {
    fn next_epoch(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    pub(crate) fn link(&self) -> Option<&Arc<Link>> {
        self.link.as_ref()
    }
}

/// What the sync paths need: the link to enqueue on, or why there is none.
struct Endpoint {
    link: Option<Arc<Link>>,
    down: HostError,
}

pub(crate) struct Shared {
    pub id: String,
    pub mc: Arc<ManagerConfig>,
    pub st: Mutex<State>,
    pub cv: Condvar,
    /// Set by `kick`; the supervisor polls it (no `State` lock needed to kick).
    pub kick: AtomicBool,
    /// The current transport process (0: none), for tests.
    child_pid: AtomicU32,
    ep: Mutex<Endpoint>,
    /// The last size this Desktop set per Terminal, re-sent after a re-attach.
    sizes: Mutex<HashMap<Uuid, (u16, u16)>>,
    /// The latest status and list, for `snapshot`.
    snap: Mutex<HostSnapshot>,
}

type Callback<T> = Box<dyn FnOnce(T) + Send>;
type Reply = Result<Value, HostError>;

/// Joins the replies of `term.open` and its `term.attach`: (how many arrived, the first).
type Joined = Arc<Mutex<(u8, Option<Reply>)>>;

/// Calls the wrapped callback at most once, whichever path gets there first.
struct Once<T>(Mutex<Option<Callback<T>>>);

impl<T> Once<T> {
    fn new(f: Callback<T>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Some(f))))
    }
    fn call(&self, v: T) {
        let f = self.0.lock().unwrap().take();
        if let Some(f) = f {
            f(v);
        }
    }
}

fn unusable(st: &State) -> HostError {
    match st.status.status {
        StatusKind::Incompatible => HostError::new(
            HostErrorCode::Incompatible,
            st.status
                .last_error
                .clone()
                .unwrap_or_else(|| "the host's xshelld is incompatible".into()),
        ),
        k => HostError::offline(format!(
            "{} is {}",
            st.cfg.name,
            serde_json::to_value(k)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default()
        )),
    }
}

/// Take a briefly held lock without ever waiting long: the holders only copy or enqueue.
/// Contention past a few retries reports `busy` instead of blocking the caller.
fn quick<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, HostError> {
    for i in 0..64 {
        match m.try_lock() {
            Ok(g) => return Ok(g),
            Err(TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(TryLockError::WouldBlock) => {
                if i < 32 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }
    Err(HostError::busy())
}

/// What happened to an attach request.
enum Done<'a> {
    Ok,
    Err(&'a HostError),
}

impl Shared {
    pub(crate) fn new(cfg: HostConfig, mc: Arc<ManagerConfig>, config_generation: u64) -> Self {
        let status = HostStatus::initial(&cfg.id, &mc.desktop_version, config_generation);
        let down = HostError::offline(format!("{} is reconnecting", cfg.name));
        Self {
            id: cfg.id.clone(),
            mc,
            snap: Mutex::new(HostSnapshot {
                status: status.clone(),
                terminals: None,
            }),
            st: Mutex::new(State {
                cfg,
                status,
                published: None,
                terminals: None,
                gen: 0,
                link: None,
                link_closed: false,
                close_reason: None,
                list_gen: 0,
                atts: HashMap::new(),
                epoch: 0,
                upgrading: false,
                upgrade_deadline: None,
                upgrade_expect: false,
            }),
            cv: Condvar::new(),
            kick: AtomicBool::new(false),
            child_pid: AtomicU32::new(0),
            ep: Mutex::new(Endpoint { link: None, down }),
            sizes: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    fn sync_ep(&self, st: &State) {
        let mut ep = self.ep.lock().unwrap();
        ep.link = st.link.clone();
        ep.down = unusable(st);
    }

    /// The only way the usable link changes, so the sync paths see it too.
    pub(crate) fn set_link(&self, st: &mut State, link: Option<Arc<Link>>) {
        st.link = link;
        self.sync_ep(st);
    }

    /// Emit the status if it changed since the last emit.
    pub(crate) fn publish(&self, st: &mut State) {
        if st.published.as_ref() != Some(&st.status) {
            st.published = Some(st.status.clone());
            self.snap.lock().unwrap().status = st.status.clone();
            self.sync_ep(st);
            self.mc.observer.status(&st.status);
        }
    }

    pub(crate) fn snapshot(&self) -> HostSnapshot {
        self.snap.lock().unwrap().clone()
    }

    /// Start a new link generation: everything from older links is ignored from now on.
    pub(crate) fn begin_link(&self, child_pid: Option<u32>) -> u64 {
        let mut st = self.lock();
        st.gen += 1;
        self.set_link(&mut st, None);
        st.link_closed = false;
        st.close_reason = None;
        self.child_pid
            .store(child_pid.unwrap_or(0), Ordering::SeqCst);
        st.gen
    }

    pub(crate) fn events(self: &Arc<Self>, gen: u64) -> Arc<dyn LinkEvents> {
        Arc::new(LinkObs {
            sh: Arc::downgrade(self),
            gen,
        })
    }

    pub(crate) fn wait_first_list(&self, gen: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        loop {
            if st.list_gen == gen {
                return true;
            }
            if st.gen != gen || st.link_closed {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.cv.wait_timeout(st, left).unwrap().0;
        }
    }

    /// Make `link` (generation `gen`) the usable one: re-attach every attachment the list
    /// still has, then let `set` publish the status, all in one critical section. Returns
    /// how many attachments could not be sent yet (queue full), or `None` when a newer
    /// generation exists or the link already closed.
    pub(crate) fn adopt(
        self: &Arc<Self>,
        gen: u64,
        link: &Arc<Link>,
        set: impl FnOnce(&mut State),
    ) -> Option<usize> {
        let mut st = self.lock();
        if st.gen != gen || st.link_closed {
            return None;
        }
        self.set_link(&mut st, Some(link.clone()));
        let listed: HashSet<Uuid> = st
            .terminals
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|i| i.terminal)
            .collect();
        st.atts.retain(|t, _| listed.contains(t));
        let left = self.reattach_locked(&mut st, gen, link);
        set(&mut st);
        self.publish(&mut st);
        self.cv.notify_all();
        Some(left)
    }

    /// Send `term.attach` (and the remembered size) for every attachment not yet attached
    /// on `gen`. An attachment counts as attached only once its request is enqueued; the
    /// ones refused (queue full) are left for a retry. Returns how many are left.
    pub(crate) fn reattach_locked(
        self: &Arc<Self>,
        st: &mut State,
        gen: u64,
        link: &Arc<Link>,
    ) -> usize {
        if st.gen != gen {
            return 0;
        }
        let mut left = 0;
        let ids: Vec<Uuid> = st.atts.keys().copied().collect();
        for t in ids {
            match st.atts.get_mut(&t) {
                // An explicit attach on this link already replayed into the newest sink.
                None => continue,
                Some(a) if a.attached_gen == gen => continue,
                // Requests of older links can no longer complete (they are fenced).
                Some(a) => a.inflight.clear(),
            }
            let epoch = st.next_epoch();
            let sh = Arc::downgrade(self);
            let w: Waiter = Box::new(move |r| {
                if let Some(sh) = sh.upgrade() {
                    sh.attach_done(t, epoch, gen, done_of(&r));
                }
            });
            match link.request(
                ClientMsg::TermAttach { terminal: t },
                self.mc.term_timeout,
                w,
            ) {
                Ok(()) => {
                    let a = st.atts.get_mut(&t).expect("listed");
                    // The superseded attach intent of an old link takes over now: nothing
                    // reaches this Terminal on the new link before this request's reply.
                    if let Some((_, p)) = a.pending.take() {
                        a.sink = Some(p);
                    }
                    a.attached_gen = gen;
                    a.epoch = epoch;
                    a.inflight.insert(epoch);
                    let size = self.sizes.lock().unwrap().get(&t).copied();
                    if let Some((cols, rows)) = size {
                        let _ = link.notify(ClientMsg::TermResize {
                            terminal: t,
                            cols,
                            rows,
                        });
                    }
                }
                Err(_) => left += 1,
            }
        }
        left
    }

    /// An attach request finished. Only the request it belongs to, on the link generation
    /// it was sent on, may change anything. A reply promotes its pending sink; a refusal
    /// from the Daemon rolls it back; a timeout on a live link is ambiguous (the late reply
    /// would be dropped), so the link is closed and the reconnect re-attaches everything.
    fn attach_done(&self, t: Uuid, epoch: u64, gen: u64, done: Done<'_>) {
        let closing = {
            let mut st = self.lock();
            if st.gen != gen {
                return;
            }
            let Some(a) = st.atts.get_mut(&t) else {
                return;
            };
            if !a.inflight.remove(&epoch) {
                return;
            }
            let pending_is_ours = a.pending.as_ref().map(|p| p.0) == Some(epoch);
            match done {
                Done::Ok => {
                    if pending_is_ours {
                        a.sink = a.pending.take().map(|p| p.1);
                    }
                    None
                }
                Done::Err(e) if e.code == HostErrorCode::Remote => {
                    if pending_is_ours {
                        a.pending = None;
                    }
                    let orphan = a.sink.is_none() && a.pending.is_none();
                    if orphan || (!pending_is_ours && a.epoch == epoch) {
                        Self::detach_locked(&mut st, t, &self.sizes);
                    }
                    None
                }
                Done::Err(e) if e.code == HostErrorCode::Timeout => {
                    let link = st.link.take();
                    self.sync_ep(&st);
                    st.link_closed = true;
                    st.close_reason = Some("term.attach timed out; reconnecting".into());
                    self.cv.notify_all();
                    link
                }
                Done::Err(_) => None,
            }
        };
        if let Some(l) = closing {
            l.close();
        }
    }

    fn usable_link(st: &State) -> Result<(Arc<Link>, u64), HostError> {
        match &st.link {
            Some(l) if !l.is_closed() => Ok((l.clone(), st.gen)),
            _ => Err(unusable(st)),
        }
    }

    /// Install `sink` for `t` and send `term.attach`, under the caller's lock.
    fn begin_attach(
        self: &Arc<Self>,
        st: &mut State,
        (link, gen): (&Arc<Link>, u64),
        (t, sink): (Uuid, Arc<dyn TermSink>),
        done: Callback<Reply>,
    ) -> Result<(), HostError> {
        let epoch = st.next_epoch();
        let fresh = !st.atts.contains_key(&t);
        if fresh {
            st.atts.insert(
                t,
                Attachment {
                    sink: Some(SinkSlot::new(sink)),
                    pending: None,
                    attached_gen: gen,
                    epoch,
                    inflight: HashSet::from([epoch]),
                },
            );
        } else {
            let a = st.atts.get_mut(&t).expect("exists");
            a.pending = Some((epoch, SinkSlot::new(sink)));
            a.attached_gen = gen;
            a.epoch = epoch;
            a.inflight.insert(epoch);
        }
        let sh = Arc::downgrade(self);
        let w: Waiter = Box::new(move |r| {
            if let Some(sh) = sh.upgrade() {
                sh.attach_done(t, epoch, gen, done_of(&r));
            }
            done(r);
        });
        let sent = link.request(
            ClientMsg::TermAttach { terminal: t },
            self.mc.term_timeout,
            w,
        );
        if sent.is_err() {
            // Not sent: undo exactly what this call installed.
            if fresh {
                st.atts.remove(&t);
            } else if let Some(a) = st.atts.get_mut(&t) {
                a.inflight.remove(&epoch);
                if a.pending.as_ref().map(|p| p.0) == Some(epoch) {
                    a.pending = None;
                }
            }
        }
        sent
    }

    fn detach_locked(st: &mut State, t: Uuid, sizes: &Mutex<HashMap<Uuid, (u16, u16)>>) {
        if st.atts.remove(&t).is_some() {
            sizes.lock().unwrap().remove(&t);
            if let Some(l) = &st.link {
                let _ = l.notify(ClientMsg::TermDetach { terminal: t });
            }
        }
    }
}

fn done_of(r: &Result<Value, HostError>) -> Done<'_> {
    match r {
        Ok(_) => Done::Ok,
        Err(e) => Done::Err(e),
    }
}

/// Link events for one generation; anything from a link that is no longer current is
/// dropped here (generation fencing).
struct LinkObs {
    sh: Weak<Shared>,
    gen: u64,
}

impl LinkEvents for LinkObs {
    fn output(&self, t: Uuid, data: &[u8]) {
        let Some(sh) = self.sh.upgrade() else { return };
        let mut st = sh.lock();
        if st.gen != self.gen {
            return;
        }
        let Some(a) = st.atts.get_mut(&t) else {
            return;
        };
        let Some(slot) = a.sink.as_mut() else {
            // The previous sink is retired and the replacement's reply has not arrived:
            // this output belongs to the previous attach.
            return;
        };
        if slot.sink.data(data) {
            slot.bytes += data.len() as u64;
        } else if a.pending.is_some() {
            // Retire only the gone receiver; the pending replacement stays and takes over
            // when its reply arrives (the reader handles both, in wire order).
            a.sink = None;
        } else {
            Shared::detach_locked(&mut st, t, &sh.sizes);
        }
    }

    fn terminals(&self, list: Vec<TerminalInfo>) {
        let Some(sh) = self.sh.upgrade() else { return };
        let mut st = sh.lock();
        if st.gen != self.gen {
            return;
        }
        let listed: HashSet<Uuid> = list.iter().map(|i| i.terminal).collect();
        st.atts
            .retain(|t, a| listed.contains(t) || !a.inflight.is_empty());
        sh.mc.observer.terminals(&sh.id, &list);
        sh.snap.lock().unwrap().terminals = Some(list.clone());
        st.terminals = Some(list);
        st.list_gen = self.gen;
        sh.cv.notify_all();
    }

    fn term_exit(&self, t: Uuid, code: i32) {
        let Some(sh) = self.sh.upgrade() else { return };
        let st = sh.lock();
        if st.gen != self.gen {
            return;
        }
        // The Terminal stays listed, so the attachment stays too.
        if let Some(Some(s)) = st.atts.get(&t).map(|a| a.sink.as_ref()) {
            s.sink.exit(code, s.bytes);
        }
    }

    fn closed(&self, why: String) {
        let Some(sh) = self.sh.upgrade() else { return };
        let mut st = sh.lock();
        if st.gen != self.gen {
            return;
        }
        st.link_closed = true;
        st.close_reason = Some(why);
        sh.set_link(&mut st, None);
        sh.cv.notify_all();
    }
}

struct Running {
    cancel: CancelToken,
    join: JoinHandle<()>,
}

/// A configured Host. Lives as long as its id stays configured; a replaced configuration
/// restarts the supervisor but keeps the handle and its attachments.
pub struct HostHandle {
    pub(crate) sh: Arc<Shared>,
    run: Mutex<Option<Running>>,
    /// Shut down for good: `start` does nothing any more.
    retired: AtomicBool,
}

fn opt_u32(v: &Value, k: &str) -> Option<u32> {
    v.get(k).and_then(Value::as_u64).map(|n| n as u32)
}

impl HostHandle {
    pub(crate) fn new(cfg: HostConfig, mc: Arc<ManagerConfig>, config_generation: u64) -> Self {
        Self {
            sh: Arc::new(Shared::new(cfg, mc, config_generation)),
            run: Mutex::new(None),
            retired: AtomicBool::new(false),
        }
    }

    /// Start the supervisor. Takes no `State` lock (a blocked sink cannot delay it).
    pub(crate) fn start(&self) {
        let mut run = self.run.lock().unwrap();
        if self.retired.load(Ordering::SeqCst) || run.is_some() {
            return;
        }
        let cancel = CancelToken::new();
        let sh = self.sh.clone();
        let c = cancel.clone();
        let join = std::thread::Builder::new()
            .name(format!("host-{}", self.sh.id))
            .spawn(move || crate::supervisor::run(sh, c))
            .expect("spawn supervisor");
        *run = Some(Running { cancel, join });
    }

    /// Never start again (manager shutdown).
    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
    }

    /// Cancel the supervisor (killing its children); the caller joins. Needs no lock a
    /// delivery callback could hold: the supervisor polls the token.
    pub(crate) fn stop_begin(&self) -> Option<JoinHandle<()>> {
        let r = self.run.lock().unwrap().take()?;
        r.cancel.cancel();
        if let Ok(_st) = self.sh.st.try_lock() {
            self.sh.cv.notify_all();
        }
        Some(r.join)
    }

    /// The running supervisor's token, for helper work such as the upgrade kill script.
    fn cancel_token(&self) -> CancelToken {
        self.run
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.cancel.clone())
            .unwrap_or_default()
    }

    /// After a stop: take the new configuration, fence the old links, keep attachments.
    pub(crate) fn reset(&self, cfg: HostConfig, config_generation: u64) {
        let closing = {
            let mut st = self.sh.lock();
            st.gen += 1;
            let link = st.link.take();
            st.cfg = cfg;
            st.terminals = None;
            st.list_gen = 0;
            st.link_closed = false;
            st.close_reason = None;
            self.sh.child_pid.store(0, Ordering::SeqCst);
            st.upgrading = false;
            st.upgrade_deadline = None;
            st.upgrade_expect = false;
            self.sh.kick.store(false, Ordering::SeqCst);
            for a in st.atts.values_mut() {
                a.attached_gen = 0;
                a.inflight.clear();
            }
            st.status =
                HostStatus::initial(&self.sh.id, &self.sh.mc.desktop_version, config_generation);
            self.sh.snap.lock().unwrap().terminals = None;
            self.sh.publish(&mut st);
            link
        };
        if let Some(l) = closing {
            l.close();
        }
    }

    pub fn id(&self) -> &str {
        &self.sh.id
    }

    pub fn config(&self) -> HostConfig {
        self.sh.lock().cfg.clone()
    }

    pub(crate) fn set_display(&self, cfg: HostConfig) {
        self.sh.lock().cfg = cfg;
    }

    pub fn status(&self) -> HostStatus {
        self.sh.snapshot().status
    }

    pub fn terminals(&self) -> Option<Vec<TerminalInfo>> {
        self.sh.snapshot().terminals
    }

    /// The latest status and list. Never waits on deliveries.
    pub fn snapshot(&self) -> HostSnapshot {
        self.sh.snapshot()
    }

    /// The pid of the current transport process (ssh), for tests.
    #[doc(hidden)]
    pub fn child_pid(&self) -> Option<u32> {
        Some(self.sh.child_pid.load(Ordering::SeqCst)).filter(|p| *p != 0)
    }

    fn request(&self, msg: ClientMsg, timeout: Duration, w: Waiter) {
        let once = Once::new(w);
        let sent = {
            let st = self.sh.lock();
            Shared::usable_link(&st).and_then(|(l, _)| {
                let o = once.clone();
                l.request(msg, timeout, Box::new(move |r| o.call(r)))
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    pub fn call(&self, method: String, params: Value, w: Waiter) {
        let timeout = if method == "save_dropped_file" {
            SAVE_FILE_TIMEOUT
        } else {
            self.sh.mc.call_timeout
        };
        self.request(ClientMsg::Call { method, params }, timeout, w);
    }

    /// `term.open`, then `term.attach` with `sink` installed first. Answers the pid.
    pub fn term_open(
        &self,
        spec: OpenSpec,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<Option<u32>, HostError>) + Send>,
    ) {
        let once = Once::new(w);
        let t = spec.terminal;
        let size = (spec.cols, spec.rows);
        // Both replies are joined; the second one to finish answers.
        let joined: Joined = Arc::new(Mutex::new((0, None)));
        let finish = {
            let once = once.clone();
            move |open: Option<Reply>, attach: Reply| {
                let r = match (open, attach) {
                    (Some(Err(e)), _) | (_, Err(e)) => Err(e),
                    (Some(Ok(v)), Ok(_)) => Ok(opt_u32(&v, "pid")),
                    (None, Ok(_)) => Ok(None),
                };
                once.call(r);
            }
        };
        let finish = Arc::new(Mutex::new(Some(finish)));
        let timeout = self.sh.mc.term_timeout;
        let mut spec = spec;
        let sent = {
            let mut st = self.sh.lock();
            Shared::usable_link(&st).and_then(|(link, gen)| {
                // The Host's configuration decides the prefix, never the request. A Daemon
                // that predates prefixes would drop it and run the agent bare, so it is
                // refused instead.
                spec.launch.launch_prefix = st.cfg.launch_prefix(&spec.launch);
                if spec.launch.launch_prefix.is_some()
                    && !st
                        .status
                        .daemon_capabilities
                        .iter()
                        .any(|c| c == LAUNCH_PREFIX_CAPABILITY)
                {
                    return Err(HostError::invalid(
                        "this Host's xshelld cannot run launch prefixes; upgrade it first",
                    ));
                }
                let (j1, f1) = (joined.clone(), finish.clone());
                let w_open: Waiter = Box::new(move |r| {
                    let mut j = j1.lock().unwrap();
                    j.0 += 1;
                    if j.0 == 2 {
                        let attach = j.1.take().unwrap_or(Ok(Value::Null));
                        drop(j);
                        if let Some(f) = f1.lock().unwrap().take() {
                            f(Some(r), attach);
                        }
                    } else {
                        j.1 = Some(r);
                    }
                });
                link.request(ClientMsg::TermOpen { spec }, timeout, w_open)?;
                self.sh.sizes.lock().unwrap().insert(t, size);
                let (j2, f2) = (joined.clone(), finish.clone());
                let done: Callback<Reply> = Box::new(move |r| {
                    let mut j = j2.lock().unwrap();
                    j.0 += 1;
                    if j.0 == 2 {
                        let open = j.1.take();
                        drop(j);
                        if let Some(f) = f2.lock().unwrap().take() {
                            f(open, r);
                        }
                    } else {
                        j.1 = Some(r);
                    }
                });
                // If this fails, the open still runs; the Terminal shows up in the list.
                self.sh.begin_attach(&mut st, (&link, gen), (t, sink), done)
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    /// Attach `sink` to an existing Terminal (replacing any earlier sink once the reply
    /// arrives). Answers the Terminal's exit code, if it already ended.
    pub fn term_attach(
        &self,
        t: Uuid,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<Option<i32>, HostError>) + Send>,
    ) {
        let once = Once::new(w);
        let o = once.clone();
        let done: Callback<Reply> = Box::new(move |r| {
            o.call(r.map(|v| v.get("exitCode").and_then(Value::as_i64).map(|c| c as i32)))
        });
        let sent = {
            let mut st = self.sh.lock();
            Shared::usable_link(&st).and_then(|(link, gen)| {
                self.sh.begin_attach(&mut st, (&link, gen), (t, sink), done)
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    /// Forget the sink. Never fails: offline, there is nothing to tell the Daemon.
    pub fn term_detach(&self, t: Uuid) -> Result<(), HostError> {
        let mut st = self.sh.lock();
        Shared::detach_locked(&mut st, t, &self.sh.sizes);
        Ok(())
    }

    /// The current link for the sync paths, or why there is none. Never waits on `State`.
    fn endpoint(&self) -> Result<Arc<Link>, HostError> {
        let ep = quick(&self.sh.ep)?;
        match &ep.link {
            Some(l) if !l.is_closed() => Ok(l.clone()),
            _ => Err(ep.down.clone()),
        }
    }

    /// Sync and non-blocking: enqueues or fails with `offline`, `incompatible` or `busy`.
    pub fn term_input(&self, t: Uuid, data: String) -> Result<(), HostError> {
        self.endpoint()?
            .notify(ClientMsg::TermInput { terminal: t, data })
    }

    /// Sync and non-blocking; the size is remembered for re-attach even when offline.
    pub fn term_resize(&self, t: Uuid, cols: u16, rows: u16) -> Result<(), HostError> {
        quick(&self.sh.sizes)?.insert(t, (cols, rows));
        self.endpoint()?.notify(ClientMsg::TermResize {
            terminal: t,
            cols,
            rows,
        })
    }

    pub fn term_close(&self, t: Uuid, w: Waiter) {
        self.request(
            ClientMsg::TermClose { terminal: t },
            self.sh.mc.term_timeout,
            w,
        );
    }

    pub fn term_update(
        &self,
        t: Uuid,
        session_id: Option<String>,
        meta: Option<Map<String, Value>>,
        w: Waiter,
    ) {
        self.request(
            ClientMsg::TermUpdate {
                terminal: t,
                session_id,
                meta,
            },
            self.sh.mc.term_timeout,
            w,
        );
    }

    /// `term.relaunch`: restart the Terminal with `skipPermissions` set to `skip`. Answers
    /// the pid now running it. Refused without sending anything when the Daemon of the
    /// current link does not advertise the `term.relaunch` capability.
    pub fn term_relaunch(
        &self,
        t: Uuid,
        skip: bool,
        w: Box<dyn FnOnce(Result<Option<u32>, HostError>) + Send>,
    ) {
        let once = Once::new(w);
        let sent = {
            // The capability and the link are checked and used under one lock: `adopt`
            // replaces both together.
            let st = self.sh.lock();
            Shared::usable_link(&st).and_then(|(l, _)| {
                if !st
                    .status
                    .daemon_capabilities
                    .iter()
                    .any(|c| c == RELAUNCH_CAPABILITY)
                {
                    return Err(HostError::invalid(
                        "this Host's xshelld cannot restart terminals; upgrade it first",
                    ));
                }
                let o = once.clone();
                l.request(
                    ClientMsg::TermRelaunch {
                        terminal: t,
                        skip_permissions: skip,
                    },
                    self.sh.mc.term_timeout,
                    Box::new(move |r: Reply| o.call(r.map(|v| opt_u32(&v, "pid")))),
                )
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    /// Connected: `daemon.upgrade`, then the supervisor waits for the old Daemon to close
    /// the link before reconnecting. Incompatible: SIGTERM the Daemon through its pidfile,
    /// then reconnect.
    pub fn upgrade(&self, w: Waiter) {
        let once = Once::new(w);
        enum Plan {
            Request(Arc<Link>),
            Kill,
            Refuse(HostError),
        }
        let (plan, cfg) = {
            let mut st = self.sh.lock();
            let cfg = st.cfg.clone();
            let plan = match Shared::usable_link(&st) {
                Ok((l, _)) => Plan::Request(l),
                Err(_) if st.status.status == StatusKind::Incompatible => Plan::Kill,
                Err(e) => Plan::Refuse(e),
            };
            if !matches!(plan, Plan::Refuse(_)) {
                st.upgrading = true;
                st.upgrade_expect = true;
                st.status.phase = Some(Phase::Upgrading);
                self.sh.publish(&mut st);
            }
            (plan, cfg)
        };
        match plan {
            Plan::Refuse(e) => once.call(Err(e)),
            Plan::Request(link) => {
                let sh = Arc::downgrade(&self.sh);
                let o = once.clone();
                let w: Waiter = Box::new(move |r| {
                    if let Some(sh) = sh.upgrade() {
                        let mut st = sh.lock();
                        match &r {
                            Ok(_) => {
                                st.upgrade_deadline =
                                    Some(Instant::now() + sh.mc.upgrade_close_timeout);
                            }
                            Err(_) => {
                                st.upgrading = false;
                                st.upgrade_expect = false;
                                st.status.phase = None;
                                sh.publish(&mut st);
                            }
                        }
                        sh.cv.notify_all();
                    }
                    o.call(r);
                });
                let timeout = self.sh.mc.term_timeout;
                if let Err(e) = link.request(ClientMsg::DaemonUpgrade, timeout, w) {
                    let mut st = self.sh.lock();
                    st.upgrading = false;
                    st.upgrade_expect = false;
                    st.status.phase = None;
                    self.sh.publish(&mut st);
                    drop(st);
                    once.call(Err(e));
                }
            }
            // The configuration the plan was made under decides; a direct Host (a local
            // socket) has no transport to run the script through either.
            Plan::Kill
                if cfg.daemon_override().is_some()
                    || self.sh.mc.transports.direct(&cfg).is_some() =>
            {
                // A Daemon command means the user manages the binary, and such hosts often
                // allow only `<cmd> connect` / `<cmd> --version` over ssh: send no script.
                once.call(Err(HostError::new(
                    crate::errors::HostErrorCode::Incompatible,
                    "this host runs xshelld through a Daemon command: replace that binary with a compatible xshelld and restart its Daemon on the host",
                )));
            }
            Plan::Kill => {
                let sh = self.sh.clone();
                let o = once.clone();
                let cancel = self.cancel_token();
                let spawned = std::thread::Builder::new()
                    .name("host-upgrade".into())
                    .spawn(move || {
                        let t = sh.mc.transports.for_host(&cfg);
                        let r = crate::supervisor::kill_daemon(&*t, &cancel);
                        sh.kick.store(true, Ordering::SeqCst);
                        o.call(r.map(|_| Value::Null));
                    });
                if spawned.is_err() {
                    once.call(Err(HostError::offline("cannot start the upgrade")));
                }
            }
        }
    }

    /// Retry now instead of waiting out the backoff. Never waits on `State`.
    pub fn kick(&self) {
        self.sh.kick.store(true, Ordering::SeqCst);
        if let Ok(_st) = self.sh.st.try_lock() {
            self.sh.cv.notify_all();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::link::testpeer::*;
    use crate::manager::tests::{test_config, NullObserver};
    use serde_json::json;
    use std::sync::mpsc;
    use xshell_core::launch::LaunchSpec;
    use xshell_protocol::msg::{Hello, ProtocolRange, ServerMsg};

    #[derive(Default)]
    pub struct VecSink {
        pub data: Mutex<Vec<u8>>,
        pub exits: Mutex<Vec<(i32, u64)>>,
        pub gone: std::sync::atomic::AtomicBool,
    }

    impl TermSink for VecSink {
        fn data(&self, b: &[u8]) -> bool {
            if self.gone.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            self.data.lock().unwrap().extend_from_slice(b);
            true
        }
        fn exit(&self, code: i32, bytes: u64) {
            self.exits.lock().unwrap().push((code, bytes));
        }
    }

    impl VecSink {
        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.data.lock().unwrap()).into_owned()
        }
        pub fn wait_text(&self, needle: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let t = self.text();
                if t.contains(needle) {
                    return t;
                }
                assert!(Instant::now() < deadline, "no {needle:?} in {t:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    pub fn info(t: Uuid) -> TerminalInfo {
        TerminalInfo {
            terminal: t,
            spec: LaunchSpec::default(),
            meta: Default::default(),
            created_at_ms: 1,
            pid: Some(2),
            exit_code: None,
        }
    }

    fn shared() -> Arc<Shared> {
        shared_with(|_| {})
    }

    fn shared_with(tweak: impl FnOnce(&mut ManagerConfig)) -> Arc<Shared> {
        let mut mc = test_config(Arc::new(NullObserver));
        tweak(&mut mc);
        let mc = Arc::new(mc);
        Arc::new(Shared::new(
            crate::config::test_host("h_ab12cd34", "x"),
            mc,
            1,
        ))
    }

    /// What the supervisor does for one connection, over a scripted peer whose first list
    /// is `list`.
    fn connect(sh: &Arc<Shared>, list: Vec<TerminalInfo>) -> (Peer, Arc<Link>, u64) {
        let (p, l, g, left) = connect_with(sh, list, crate::link::LinkLimits::default());
        assert_eq!(left, 0);
        (p, l, g)
    }

    fn connect_with(
        sh: &Arc<Shared>,
        list: Vec<TerminalInfo>,
        limits: crate::link::LinkLimits,
    ) -> (Peer, Arc<Link>, u64, usize) {
        connect_full(sh, list, limits, &[])
    }

    /// [`connect`] to a Daemon advertising `caps`, adopted the way the supervisor does.
    fn connect_capable(sh: &Arc<Shared>, list: Vec<TerminalInfo>, caps: &[&str]) -> Peer {
        connect_full(sh, list, crate::link::LinkLimits::default(), caps).0
    }

    fn connect_full(
        sh: &Arc<Shared>,
        list: Vec<TerminalInfo>,
        limits: crate::link::LinkLimits,
        caps: &[&str],
    ) -> (Peer, Arc<Link>, u64, usize) {
        let (io, mut peer) = pair();
        let caps: Vec<String> = caps.iter().map(|c| c.to_string()).collect();
        let mut b = msg_frame(&ServerMsg::Hello(Hello {
            protocol: ProtocolRange { min: 1, max: 1 },
            version: "1.5.0".into(),
            capabilities: caps.clone(),
        }));
        b.extend(terminals_frame(list));
        peer.write(&b);
        let gen = sh.begin_link(None);
        let (link, _, _) = Link::establish_with(
            io,
            ProtocolRange { min: 1, max: 1 },
            "1.5.0",
            sh.events(gen),
            Duration::from_secs(5),
            limits,
        )
        .unwrap();
        assert!(sh.wait_first_list(gen, Duration::from_secs(5)));
        let left = sh
            .adopt(gen, &link, |st| {
                st.status.set_kind(StatusKind::Connected);
                st.status.daemon_capabilities = caps;
            })
            .expect("adopted");
        peer.expect("hello");
        (peer, link, gen, left)
    }

    fn handle(sh: &Arc<Shared>) -> HostHandle {
        HostHandle {
            sh: sh.clone(),
            run: Mutex::new(None),
            retired: AtomicBool::new(false),
        }
    }

    fn res_slot<T: Send + 'static>() -> (Box<dyn FnOnce(T) + Send>, mpsc::Receiver<T>) {
        let (tx, rx) = mpsc::channel();
        (
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
            rx,
        )
    }

    const T5: Duration = Duration::from_secs(5);

    #[test]
    fn sink_before_attach() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut peer, _link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let sink = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_attach(t, sink.clone(), w);
        let id = peer.expect("term.attach")["id"].as_u64().unwrap();
        // Reply, replay and live output in one write.
        let mut b = res_frame(id, Ok(json!({"exitCode": null})));
        b.extend(out_frame(t, b"\x1bcREPLAY"));
        b.extend(out_frame(t, b"LIVE"));
        peer.write(&b);
        assert_eq!(rx.recv_timeout(T5).unwrap(), Ok(None));
        assert_eq!(sink.wait_text("LIVE"), "\x1bcREPLAYLIVE");
    }

    #[test]
    fn attach_error_rolls_back() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut peer, _link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let (w, rx) = res_slot();
        h.term_attach(t, Arc::new(VecSink::default()), w);
        let id = peer.expect("term.attach")["id"].as_u64().unwrap();
        peer.reply(id, Err("unknown terminal".into()));
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Remote
        );
        assert!(sh.lock().atts.is_empty());
    }

    #[test]
    fn exit_watermark_counts_delivered_bytes() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut peer, _link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let sink = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_attach(t, sink.clone(), w);
        let id = peer.expect("term.attach")["id"].as_u64().unwrap();
        let mut b = res_frame(id, Ok(json!({"exitCode": null})));
        b.extend(out_frame(t, b"12345"));
        b.extend(out_frame(t, b"678"));
        b.extend(msg_frame(&ServerMsg::TermExit {
            terminal: t,
            code: 4,
        }));
        peer.write(&b);
        rx.recv_timeout(T5).unwrap().unwrap();
        let deadline = Instant::now() + T5;
        while sink.exits.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sink.exits.lock().unwrap().as_slice(), &[(4, 8)]);
        // The attachment survives the exit: the Terminal is still listed.
        assert!(sh.lock().atts.contains_key(&t));
    }

    #[test]
    fn old_link_events_are_fenced() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut p1, _l1, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let sink = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_attach(t, sink.clone(), w);
        let id = p1.expect("term.attach")["id"].as_u64().unwrap();
        p1.reply(id, Ok(json!({"exitCode": null})));
        rx.recv_timeout(T5).unwrap().unwrap();
        // A second link takes over (the first one never closed: a stalled ssh).
        let (mut p2, _l2, _) = connect(&sh, vec![info(t)]);
        let re = p2.expect("term.attach")["id"].as_u64().unwrap();
        let mut b = res_frame(re, Ok(json!({"exitCode": null})));
        b.extend(out_frame(t, b"NEW"));
        p2.write(&b);
        sink.wait_text("NEW");
        // Late output, exit and close from the old link reach nothing.
        let mut b = out_frame(t, b"OLD");
        b.extend(msg_frame(&ServerMsg::TermExit {
            terminal: t,
            code: 9,
        }));
        b.extend(terminals_frame(vec![]));
        p1.write(&b);
        drop(p1);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!sink.text().contains("OLD"));
        assert!(sink.exits.lock().unwrap().is_empty());
        let st = sh.lock();
        assert!(st.link.is_some() && !st.link_closed);
        assert!(st.atts.contains_key(&t));
        assert_eq!(st.terminals.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn explicit_attach_racing_reconnect_gets_one_replay() {
        let sh = shared();
        let t = Uuid::new_v4();
        let a = Arc::new(VecSink::default());
        // Attached on link 1.
        let (mut p1, _l1, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let (w, rx) = res_slot();
        h.term_attach(t, a.clone(), w);
        let id = p1.expect("term.attach")["id"].as_u64().unwrap();
        p1.reply(id, Ok(json!({"exitCode": null})));
        rx.recv_timeout(T5).unwrap().unwrap();
        // Reconnect: the supervisor re-attaches sink A; the frontend attaches sink B before
        // the re-attach's reply arrives.
        let (mut p2, _l2, _) = connect(&sh, vec![info(t)]);
        let r1 = p2.expect("term.attach")["id"].as_u64().unwrap();
        let b = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_attach(t, b.clone(), w);
        let r2 = p2.expect("term.attach")["id"].as_u64().unwrap();
        let mut bytes = res_frame(r1, Ok(json!({"exitCode": null})));
        bytes.extend(out_frame(t, b"\x1bcR1"));
        bytes.extend(res_frame(r2, Ok(json!({"exitCode": null}))));
        bytes.extend(out_frame(t, b"\x1bcR2"));
        bytes.extend(out_frame(t, b"live"));
        p2.write(&bytes);
        rx.recv_timeout(T5).unwrap().unwrap();
        assert_eq!(b.wait_text("live"), "\x1bcR2live");
        assert_eq!(a.text().matches("\x1bc").count(), 1, "{:?}", a.text());
        // And when the explicit attach comes first, the supervisor skips its re-attach.
        let c = Arc::new(VecSink::default());
        let gen = sh.begin_link(None);
        let (io, mut p3) = pair();
        let mut hb = hello_frame(1, 1, "1.5.0");
        hb.extend(terminals_frame(vec![info(t)]));
        p3.write(&hb);
        let (l3, _, _) = Link::establish(
            io,
            ProtocolRange { min: 1, max: 1 },
            "1.5.0",
            sh.events(gen),
            T5,
        )
        .unwrap();
        {
            let mut st = sh.lock();
            sh.set_link(&mut st, Some(l3.clone()));
        }
        let (w, _rx) = res_slot();
        h.term_attach(t, c.clone(), w);
        assert_eq!(sh.adopt(gen, &l3, |_| {}), Some(0));
        p3.expect("hello");
        p3.expect("term.attach");
        // No second attach was sent: the next message is our probe.
        h.term_input(t, "probe".into()).unwrap();
        assert_eq!(p3.read_json()["t"], "term.input");
    }

    #[test]
    fn dead_sink_detaches() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut peer, _link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let sink = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_attach(t, sink.clone(), w);
        let id = peer.expect("term.attach")["id"].as_u64().unwrap();
        peer.reply(id, Ok(json!({"exitCode": null})));
        rx.recv_timeout(T5).unwrap().unwrap();
        sink.gone.store(true, std::sync::atomic::Ordering::SeqCst);
        peer.write(&out_frame(t, b"x"));
        assert_eq!(peer.expect("term.detach")["terminal"], json!(t));
        assert!(sh.lock().atts.is_empty());
    }

    #[test]
    fn calls_fail_fast_when_not_usable() {
        let sh = shared();
        let h = handle(&sh);
        let (w, rx) = slot();
        h.call("get_home_dir".into(), json!({}), w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        assert_eq!(
            h.term_input(Uuid::nil(), "x".into()).unwrap_err().code,
            HostErrorCode::Offline
        );
        {
            let mut st = sh.lock();
            st.status.set_kind(StatusKind::Incompatible);
            sh.publish(&mut st);
        }
        assert_eq!(
            h.term_resize(Uuid::nil(), 1, 1).unwrap_err().code,
            HostErrorCode::Incompatible
        );
        assert!(h.term_detach(Uuid::nil()).is_ok());
    }

    #[test]
    fn open_installs_sink_and_returns_pid() {
        let sh = shared();
        let (mut peer, _link, _) = connect(&sh, vec![]);
        let h = handle(&sh);
        let t = Uuid::new_v4();
        let sink = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_open(
            OpenSpec {
                terminal: t,
                launch: LaunchSpec::default(),
                cols: 90,
                rows: 30,
                meta: Map::new(),
            },
            sink.clone(),
            w,
        );
        let open = peer.expect("term.open");
        assert_eq!(open["spec"]["cols"], 90);
        let attach = peer.expect("term.attach");
        let mut b = terminals_frame(vec![info(t)]);
        b.extend(res_frame(
            open["id"].as_u64().unwrap(),
            Ok(json!({"pid": 77})),
        ));
        b.extend(res_frame(
            attach["id"].as_u64().unwrap(),
            Ok(json!({"exitCode": null})),
        ));
        b.extend(out_frame(t, b"hi"));
        peer.write(&b);
        assert_eq!(rx.recv_timeout(T5).unwrap(), Ok(Some(77)));
        sink.wait_text("hi");
        assert_eq!(sh.sizes.lock().unwrap().get(&t), Some(&(90, 30)));
        // A failed open reports the open's error and removes the attachment.
        let t2 = Uuid::new_v4();
        let (w, rx) = res_slot();
        h.term_open(
            OpenSpec {
                terminal: t2,
                launch: LaunchSpec::default(),
                cols: 80,
                rows: 24,
                meta: Map::new(),
            },
            Arc::new(VecSink::default()),
            w,
        );
        let open = peer.expect("term.open");
        let attach = peer.expect("term.attach");
        let mut b = res_frame(open["id"].as_u64().unwrap(), Err("no such cwd".into()));
        b.extend(res_frame(
            attach["id"].as_u64().unwrap(),
            Err("unknown terminal".into()),
        ));
        peer.write(&b);
        assert_eq!(
            rx.recv_timeout(T5).unwrap(),
            Err(HostError::remote("no such cwd"))
        );
        assert!(!sh.lock().atts.contains_key(&t2));
    }

    /// Attach `sink` and complete it with a reply (no output).
    fn attached(h: &HostHandle, peer: &mut Peer, t: Uuid, sink: Arc<VecSink>) {
        let (w, rx) = res_slot();
        h.term_attach(t, sink, w);
        let id = peer.expect("term.attach")["id"].as_u64().unwrap();
        peer.reply(id, Ok(json!({"exitCode": null})));
        rx.recv_timeout(T5).unwrap().unwrap();
    }

    #[test]
    fn retiring_the_old_sink_keeps_the_pending_replacement() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut peer, _link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let a = Arc::new(VecSink::default());
        attached(&h, &mut peer, t, a.clone());
        // Remount: B's attach is pending while A's receiver goes away.
        let b = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_attach(t, b.clone(), w);
        let id = peer.expect("term.attach")["id"].as_u64().unwrap();
        a.gone.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut bytes = out_frame(t, b"for A");
        bytes.extend(res_frame(id, Ok(json!({"exitCode": null}))));
        bytes.extend(out_frame(t, b"\x1bcREPLAY"));
        peer.write(&bytes);
        rx.recv_timeout(T5).unwrap().unwrap();
        assert_eq!(b.wait_text("REPLAY"), "\x1bcREPLAY");
        assert!(sh.lock().atts.contains_key(&t));
        // No detach went out: the next message is our probe.
        h.term_input(t, "probe".into()).unwrap();
        assert_eq!(peer.read_json()["t"], "term.input");
    }

    #[test]
    fn stale_attach_completion_is_fenced() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut p1, _l1, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let (w, _rx) = res_slot();
        h.term_attach(t, Arc::new(VecSink::default()), w);
        let old = p1.expect("term.attach")["id"].as_u64().unwrap();
        // A new link takes over and re-attaches; then the old link's reply arrives late.
        let (mut p2, _l2, _) = connect(&sh, vec![info(t)]);
        p2.expect("term.attach");
        let inflight = sh.lock().atts[&t].inflight.clone();
        assert_eq!(inflight.len(), 1);
        p1.reply(old, Ok(json!({"exitCode": null})));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(sh.lock().atts[&t].inflight, inflight);
        // So a list without the Terminal cannot prune it while its re-attach is in flight.
        p2.write(&terminals_frame(vec![]));
        std::thread::sleep(Duration::from_millis(100));
        assert!(sh.lock().atts.contains_key(&t));
    }

    #[test]
    fn attach_timeout_reconnects_and_restores() {
        let sh = shared_with(|c| c.term_timeout = Duration::from_millis(150));
        let t = Uuid::new_v4();
        let (mut p1, _l1, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        attached(&h, &mut p1, t, Arc::new(VecSink::default()));
        let b = Arc::new(VecSink::default());
        let (w, rx) = res_slot();
        h.term_attach(t, b.clone(), w);
        p1.expect("term.attach"); // never answered
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Timeout);
        {
            let st = sh.lock();
            assert!(
                st.link_closed && st.link().is_none(),
                "the link is given up"
            );
            assert!(st.atts[&t].pending.is_some(), "the replacement is kept");
        }
        // The reconnect re-attaches into the replacement.
        let (mut p2, _l2, _) = connect(&sh, vec![info(t)]);
        let id = p2.expect("term.attach")["id"].as_u64().unwrap();
        let mut bytes = res_frame(id, Ok(json!({"exitCode": null})));
        bytes.extend(out_frame(t, b"\x1bcBACK"));
        p2.write(&bytes);
        assert_eq!(b.wait_text("BACK"), "\x1bcBACK");
    }

    #[test]
    fn adoption_retries_attachments_refused_by_a_full_queue() {
        let sh = shared();
        let ts: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        let list: Vec<TerminalInfo> = ts.iter().map(|t| info(*t)).collect();
        let (mut p1, _l1, _) = connect(&sh, list.clone());
        let h = handle(&sh);
        for t in &ts {
            attached(&h, &mut p1, *t, Arc::new(VecSink::default()));
        }
        // The new link takes one request at a time.
        let limits = crate::link::LinkLimits {
            max_pending: 1,
            ..Default::default()
        };
        let (mut p2, l2, gen, left) = connect_with(&sh, list, limits);
        assert_eq!(left, 2);
        let sent_on = |sh: &Shared| {
            sh.lock()
                .atts
                .values()
                .filter(|a| a.attached_gen == gen)
                .count()
        };
        assert_eq!(sent_on(&sh), 1, "only enqueued attaches count as attached");
        for expect_left in [1, 0] {
            let id = p2.expect("term.attach")["id"].as_u64().unwrap();
            // Still full until the reply frees the slot.
            assert_eq!(
                sh.reattach_locked(&mut sh.lock(), gen, &l2),
                expect_left + 1
            );
            p2.reply(id, Ok(json!({"exitCode": null})));
            let deadline = Instant::now() + T5;
            loop {
                let n = sh.reattach_locked(&mut sh.lock(), gen, &l2);
                if n == expect_left {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        p2.expect("term.attach");
        assert_eq!(sent_on(&sh), 3);
    }

    /// Sends `term.close` and checks it is the next message on the wire, so nothing was
    /// queued before it.
    fn assert_nothing_sent(h: &HostHandle, peer: &mut Peer, t: Uuid) {
        let (w, _rx) = res_slot();
        h.term_close(t, w);
        let next = peer.read_json();
        assert_eq!(next["t"], "term.close", "{next}");
    }

    fn open_spec(t: Uuid, launch: LaunchSpec) -> OpenSpec {
        OpenSpec {
            terminal: t,
            launch,
            cols: 80,
            rows: 24,
            meta: Map::new(),
        }
    }

    fn prefixed_shared() -> Arc<Shared> {
        let sh = shared();
        sh.lock()
            .cfg
            .launch_prefixes
            .insert("claude".into(), "wrap --x".into());
        sh
    }

    #[test]
    fn open_sends_configured_prefix() {
        let sh = prefixed_shared();
        let mut peer = connect_capable(&sh, vec![], &["term", "launch.prefix"]);
        let h = handle(&sh);
        let (w, _rx) = res_slot();
        // A prefix in the request is replaced by the Host's.
        let launch = LaunchSpec {
            launch_prefix: Some(vec!["evil".into()]),
            ..LaunchSpec::default()
        };
        h.term_open(
            open_spec(Uuid::new_v4(), launch),
            Arc::new(VecSink::default()),
            w,
        );
        let open = peer.expect("term.open");
        assert_eq!(open["spec"]["launchPrefix"], json!(["wrap", "--x"]));

        // Agents without a prefix, and raw shells, are sent without one.
        for launch in [
            LaunchSpec {
                agent: Some("codex".into()),
                launch_prefix: Some(vec!["evil".into()]),
                ..LaunchSpec::default()
            },
            LaunchSpec {
                shell_mode: Some("raw".into()),
                ..LaunchSpec::default()
            },
        ] {
            let (w, _rx) = res_slot();
            h.term_open(
                open_spec(Uuid::new_v4(), launch),
                Arc::new(VecSink::default()),
                w,
            );
            let open = peer.expect("term.open");
            assert!(open["spec"].get("launchPrefix").is_none(), "{open}");
        }
    }

    #[test]
    fn open_with_prefix_refused_without_capability() {
        let sh = prefixed_shared();
        let (mut peer, _link, _) = connect(&sh, vec![]);
        let h = handle(&sh);
        let t = Uuid::new_v4();
        let (w, rx) = res_slot();
        h.term_open(
            open_spec(t, LaunchSpec::default()),
            Arc::new(VecSink::default()),
            w,
        );
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Invalid);
        assert_nothing_sent(&h, &mut peer, t);

        // Without a prefix to run, the same Daemon opens the Terminal as before.
        let (w, _rx) = res_slot();
        let launch = LaunchSpec {
            agent: Some("codex".into()),
            ..LaunchSpec::default()
        };
        h.term_open(open_spec(t, launch), Arc::new(VecSink::default()), w);
        peer.expect("term.open");
    }

    #[test]
    fn relaunch_sent_with_capability() {
        let sh = shared();
        let t = Uuid::new_v4();
        let mut peer = connect_capable(&sh, vec![info(t)], &["term", "term.relaunch"]);
        let h = handle(&sh);
        let (w, rx) = res_slot();
        h.term_relaunch(t, true, w);
        let m = peer.expect("term.relaunch");
        assert_eq!(m["terminal"], json!(t));
        assert_eq!(m["skipPermissions"], json!(true));
        peer.reply(
            m["id"].as_u64().unwrap(),
            Ok(json!({"pid": 7, "relaunched": true})),
        );
        assert_eq!(rx.recv_timeout(T5).unwrap(), Ok(Some(7)));
    }

    #[test]
    fn relaunch_refused_without_capability() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut peer, _link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let (w, rx) = res_slot();
        h.term_relaunch(t, true, w);
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Invalid);
        assert_nothing_sent(&h, &mut peer, t);
    }

    /// The capability belongs to the link it came with: after a reconnect to a Daemon
    /// without it, the request is refused even though the previous Daemon had it.
    #[test]
    fn relaunch_refused_after_reconnect_to_incapable_daemon() {
        let sh = shared();
        let t = Uuid::new_v4();
        let _old = connect_capable(&sh, vec![info(t)], &["term.relaunch"]);
        let (mut peer, _link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let (w, rx) = res_slot();
        h.term_relaunch(t, false, w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Invalid
        );
        assert_nothing_sent(&h, &mut peer, t);
    }
}

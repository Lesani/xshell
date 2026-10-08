//! One Host as the Desktop sees it: status, the last `terminals` list, the current link, and
//! the attachment table (Terminal → output sink). Attachments outlive links: after a
//! reconnect, or a replaced configuration, the supervisor attaches them again.
//!
//! Locking: `State` is one mutex. Link methods may be called under it (they only enqueue);
//! `Link::close` and user callbacks never are. Sinks and the observer are called under it,
//! which keeps their events in order; they must not call back into the handle.

use crate::cancel::CancelToken;
use crate::config::HostConfig;
use crate::errors::{HostError, HostErrorCode};
use crate::link::{Link, LinkEvents, Waiter};
use crate::manager::ManagerConfig;
use crate::status::{HostSnapshot, HostStatus, Phase, StatusKind};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::protocol::msg::{ClientMsg, OpenSpec, TerminalInfo};

/// Where one Tab's output goes.
pub trait TermSink: Send + Sync {
    /// `false`: the receiver is gone; the handle detaches.
    fn data(&self, bytes: &[u8]) -> bool;
    /// The Terminal ended. `bytes` is everything this sink received through `data`, so the
    /// receiver can apply the exit only after that much output (the exit watermark).
    fn exit(&self, code: i32, bytes: u64);
}

pub(crate) const TERM_TIMEOUT: Duration = Duration::from_secs(30);
const SAVE_FILE_TIMEOUT: Duration = Duration::from_secs(120);

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
    /// Receives output now.
    sink: SinkSlot,
    /// A replacement installed before its `term.attach` was sent; it takes over when that
    /// request's reply arrives (output before the reply belongs to the previous attach).
    pending: Option<(u64, SinkSlot)>,
    /// The last size this Desktop set, re-sent after a re-attach.
    pub(crate) size: Option<(u16, u16)>,
    /// The link generation of the latest attach.
    pub(crate) attached_gen: u64,
    /// The epoch of the latest attach request.
    epoch: u64,
    /// Attach requests awaiting a reply. Lists never prune an attachment with one in flight.
    inflight: u32,
}

pub(crate) struct State {
    pub cfg: HostConfig,
    pub status: HostStatus,
    published: Option<HostStatus>,
    pub terminals: Option<Vec<TerminalInfo>>,
    /// The generation of the newest link (events of older links are ignored).
    pub gen: u64,
    /// The usable link; always of generation `gen`.
    pub link: Option<Arc<Link>>,
    pub link_closed: bool,
    pub close_reason: Option<String>,
    /// The generation of the newest `terminals` list.
    pub list_gen: u64,
    pub atts: HashMap<Uuid, Attachment>,
    epoch: u64,
    pub kick: bool,
    pub child_pid: Option<u32>,
    /// An upgrade is in progress: phase `upgrading` until connected again.
    pub upgrading: bool,
    /// The Daemon acknowledged `daemon.upgrade`; it must close the link by then.
    pub upgrade_deadline: Option<Instant>,
    /// After an upgrade, the next Daemon must report the Desktop's version.
    pub upgrade_expect: bool,
    /// The running supervisor's token (for helper work such as the upgrade kill script).
    pub cancel: CancelToken,
}

impl State {
    fn next_epoch(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }
}

pub(crate) struct Shared {
    pub id: String,
    pub mc: Arc<ManagerConfig>,
    pub st: Mutex<State>,
    pub cv: Condvar,
}

/// Calls the wrapped callback at most once, whichever path gets there first.
type Callback<T> = Box<dyn FnOnce(T) + Send>;
type Reply = Result<Value, HostError>;

/// Joins the replies of `term.open` and its `term.attach`: (how many arrived, the first).
type Joined = Arc<Mutex<(u8, Option<Reply>)>>;

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

impl Shared {
    pub(crate) fn new(cfg: HostConfig, mc: Arc<ManagerConfig>, config_generation: u64) -> Self {
        let status = HostStatus::initial(&cfg.id, &mc.desktop_version, config_generation);
        Self {
            id: cfg.id.clone(),
            mc,
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
                kick: false,
                child_pid: None,
                upgrading: false,
                upgrade_deadline: None,
                upgrade_expect: false,
                cancel: CancelToken::new(),
            }),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    /// Emit the status if it changed since the last emit.
    pub(crate) fn publish(&self, st: &mut State) {
        if st.published.as_ref() != Some(&st.status) {
            st.published = Some(st.status.clone());
            self.mc.observer.status(&st.status);
        }
    }

    /// Start a new link generation: everything from older links is ignored from now on.
    pub(crate) fn begin_link(&self, child_pid: Option<u32>) -> u64 {
        let mut st = self.lock();
        st.gen += 1;
        st.link = None;
        st.link_closed = false;
        st.close_reason = None;
        st.child_pid = child_pid;
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
    /// still has, then let `set` publish the status, all in one critical section. `false`
    /// when a newer generation exists or the link already closed.
    pub(crate) fn adopt(
        self: &Arc<Self>,
        gen: u64,
        link: &Arc<Link>,
        set: impl FnOnce(&mut State),
    ) -> bool {
        let mut st = self.lock();
        if st.gen != gen || st.link_closed {
            return false;
        }
        st.link = Some(link.clone());
        let listed: HashSet<Uuid> = st
            .terminals
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|i| i.terminal)
            .collect();
        st.atts.retain(|t, _| listed.contains(t));
        let ids: Vec<Uuid> = st.atts.keys().copied().collect();
        for t in ids {
            let epoch = st.next_epoch();
            let a = st.atts.get_mut(&t).expect("listed");
            // An explicit attach on this link already replayed into the newest sink.
            if a.attached_gen == gen {
                continue;
            }
            if let Some((_, p)) = a.pending.take() {
                a.sink = p;
            }
            a.attached_gen = gen;
            a.epoch = epoch;
            a.inflight += 1;
            let size = a.size;
            let sh = Arc::downgrade(self);
            let w: Waiter = Box::new(move |r| {
                if let Some(sh) = sh.upgrade() {
                    sh.attach_done(t, epoch, gen, false, r.as_ref().err());
                }
            });
            if link
                .request(ClientMsg::TermAttach { terminal: t }, TERM_TIMEOUT, w)
                .is_err()
            {
                if let Some(a) = st.atts.get_mut(&t) {
                    a.inflight = a.inflight.saturating_sub(1);
                }
            }
            if let Some((cols, rows)) = size {
                let _ = link.notify(ClientMsg::TermResize {
                    terminal: t,
                    cols,
                    rows,
                });
            }
        }
        set(&mut st);
        self.publish(&mut st);
        self.cv.notify_all();
        true
    }

    /// An attach request finished. A reply promotes its pending sink; a refusal from the
    /// Daemon rolls the sink back (and drops a fresh attachment). A lost connection keeps
    /// everything: the next link re-attaches.
    fn attach_done(&self, t: Uuid, epoch: u64, gen: u64, fresh: bool, err: Option<&HostError>) {
        let mut st = self.lock();
        let Some(a) = st.atts.get_mut(&t) else {
            return;
        };
        a.inflight = a.inflight.saturating_sub(1);
        let pending_is_ours = a.pending.as_ref().map(|p| p.0) == Some(epoch);
        match err {
            None => {
                if pending_is_ours {
                    a.sink = a.pending.take().expect("pending").1;
                }
            }
            Some(e) if e.code == HostErrorCode::Remote => {
                if pending_is_ours {
                    a.pending = None;
                } else if a.epoch == epoch && a.attached_gen == gen {
                    let _ = fresh;
                    st.atts.remove(&t);
                }
            }
            Some(_) => {}
        }
    }

    fn usable_link(st: &State) -> Result<(Arc<Link>, u64), HostError> {
        match &st.link {
            Some(l) if !l.is_closed() => Ok((l.clone(), st.gen)),
            _ => Err(unusable(st)),
        }
    }

    /// Install `sink` for `t` and send `term.attach`, under the caller's lock. Returns the
    /// request's epoch and whether the attachment is new.
    fn begin_attach(
        self: &Arc<Self>,
        st: &mut State,
        (link, gen): (&Arc<Link>, u64),
        (t, sink, size): (Uuid, Arc<dyn TermSink>, Option<(u16, u16)>),
        done: Callback<Reply>,
    ) -> Result<(), HostError> {
        let epoch = st.next_epoch();
        let fresh = !st.atts.contains_key(&t);
        if fresh {
            st.atts.insert(
                t,
                Attachment {
                    sink: SinkSlot::new(sink),
                    pending: None,
                    size,
                    attached_gen: gen,
                    epoch,
                    inflight: 1,
                },
            );
        } else {
            let a = st.atts.get_mut(&t).expect("exists");
            a.pending = Some((epoch, SinkSlot::new(sink)));
            a.attached_gen = gen;
            a.epoch = epoch;
            a.inflight += 1;
            if size.is_some() {
                a.size = size;
            }
        }
        let sh = Arc::downgrade(self);
        let w: Waiter = Box::new(move |r| {
            if let Some(sh) = sh.upgrade() {
                sh.attach_done(t, epoch, gen, fresh, r.as_ref().err());
            }
            done(r);
        });
        let sent = link.request(ClientMsg::TermAttach { terminal: t }, TERM_TIMEOUT, w);
        if sent.is_err() {
            // Not sent: undo exactly what this call installed.
            if fresh {
                st.atts.remove(&t);
            } else if let Some(a) = st.atts.get_mut(&t) {
                a.inflight = a.inflight.saturating_sub(1);
                if a.pending.as_ref().map(|p| p.0) == Some(epoch) {
                    a.pending = None;
                }
            }
        }
        sent
    }

    fn detach_locked(st: &mut State, t: Uuid) {
        if st.atts.remove(&t).is_some() {
            if let Some(l) = &st.link {
                let _ = l.notify(ClientMsg::TermDetach { terminal: t });
            }
        }
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
        let alive = match st.atts.get_mut(&t) {
            Some(a) => {
                let ok = a.sink.sink.data(data);
                if ok {
                    a.sink.bytes += data.len() as u64;
                }
                ok
            }
            None => true,
        };
        if !alive {
            Shared::detach_locked(&mut st, t);
        }
    }

    fn terminals(&self, list: Vec<TerminalInfo>) {
        let Some(sh) = self.sh.upgrade() else { return };
        let mut st = sh.lock();
        if st.gen != self.gen {
            return;
        }
        let listed: HashSet<Uuid> = list.iter().map(|i| i.terminal).collect();
        st.atts.retain(|t, a| listed.contains(t) || a.inflight > 0);
        sh.mc.observer.terminals(&sh.id, &list);
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
        if let Some(a) = st.atts.get(&t) {
            a.sink.sink.exit(code, a.sink.bytes);
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
        st.link = None;
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
}

fn opt_u32(v: &Value, k: &str) -> Option<u32> {
    v.get(k).and_then(Value::as_u64).map(|n| n as u32)
}

impl HostHandle {
    pub(crate) fn new(cfg: HostConfig, mc: Arc<ManagerConfig>, config_generation: u64) -> Self {
        Self {
            sh: Arc::new(Shared::new(cfg, mc, config_generation)),
            run: Mutex::new(None),
        }
    }

    pub(crate) fn start(&self) {
        let cancel = CancelToken::new();
        {
            let mut st = self.sh.lock();
            st.cancel = cancel.clone();
            self.sh.publish(&mut st);
        }
        let sh = self.sh.clone();
        let c = cancel.clone();
        let join = std::thread::Builder::new()
            .name(format!("host-{}", self.sh.id))
            .spawn(move || crate::supervisor::run(sh, c))
            .expect("spawn supervisor");
        *self.run.lock().unwrap() = Some(Running { cancel, join });
    }

    /// Cancel the supervisor (killing its children); the caller joins.
    pub(crate) fn stop_begin(&self) -> Option<JoinHandle<()>> {
        let r = self.run.lock().unwrap().take()?;
        r.cancel.cancel();
        {
            let _st = self.sh.lock();
            self.sh.cv.notify_all();
        }
        Some(r.join)
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
            st.child_pid = None;
            st.upgrading = false;
            st.upgrade_deadline = None;
            st.upgrade_expect = false;
            st.kick = false;
            for a in st.atts.values_mut() {
                a.attached_gen = 0;
                a.inflight = 0;
            }
            st.status =
                HostStatus::initial(&self.sh.id, &self.sh.mc.desktop_version, config_generation);
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
        self.sh.lock().status.clone()
    }

    pub fn terminals(&self) -> Option<Vec<TerminalInfo>> {
        self.sh.lock().terminals.clone()
    }

    pub fn snapshot(&self) -> HostSnapshot {
        let st = self.sh.lock();
        HostSnapshot {
            status: st.status.clone(),
            terminals: st.terminals.clone(),
        }
    }

    /// The pid of the current transport process (ssh), for tests.
    #[doc(hidden)]
    pub fn child_pid(&self) -> Option<u32> {
        self.sh.lock().child_pid
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
        let size = Some((spec.cols, spec.rows));
        // Both replies are joined; the second one to finish answers.
        let joined: Joined = Arc::new(Mutex::new((0, None)));
        let finish = {
            let once = once.clone();
            move |open: Option<Result<Value, HostError>>, attach: Result<Value, HostError>| {
                let r = match (open, attach) {
                    (Some(Err(e)), _) | (_, Err(e)) => Err(e),
                    (Some(Ok(v)), Ok(_)) => Ok(opt_u32(&v, "pid")),
                    (None, Ok(_)) => Ok(None),
                };
                once.call(r);
            }
        };
        let finish = Arc::new(Mutex::new(Some(finish)));
        let sent = {
            let mut st = self.sh.lock();
            Shared::usable_link(&st).and_then(|(link, gen)| {
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
                link.request(ClientMsg::TermOpen { spec }, TERM_TIMEOUT, w_open)?;
                let (j2, f2) = (joined.clone(), finish.clone());
                let done: Box<dyn FnOnce(Result<Value, HostError>) + Send> = Box::new(move |r| {
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
                self.sh
                    .begin_attach(&mut st, (&link, gen), (t, sink, size), done)
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
        let done: Box<dyn FnOnce(Result<Value, HostError>) + Send> = Box::new(move |r| {
            o.call(r.map(|v| v.get("exitCode").and_then(Value::as_i64).map(|c| c as i32)))
        });
        let sent = {
            let mut st = self.sh.lock();
            Shared::usable_link(&st).and_then(|(link, gen)| {
                self.sh
                    .begin_attach(&mut st, (&link, gen), (t, sink, None), done)
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    /// Forget the sink. Never fails: offline, there is nothing to tell the Daemon.
    pub fn term_detach(&self, t: Uuid) -> Result<(), HostError> {
        let mut st = self.sh.lock();
        Shared::detach_locked(&mut st, t);
        Ok(())
    }

    pub fn term_input(&self, t: Uuid, data: String) -> Result<(), HostError> {
        let st = self.sh.lock();
        let (l, _) = Shared::usable_link(&st)?;
        l.notify(ClientMsg::TermInput { terminal: t, data })
    }

    pub fn term_resize(&self, t: Uuid, cols: u16, rows: u16) -> Result<(), HostError> {
        let mut st = self.sh.lock();
        if let Some(a) = st.atts.get_mut(&t) {
            a.size = Some((cols, rows));
        }
        let (l, _) = Shared::usable_link(&st)?;
        l.notify(ClientMsg::TermResize {
            terminal: t,
            cols,
            rows,
        })
    }

    pub fn term_close(&self, t: Uuid, w: Waiter) {
        self.request(ClientMsg::TermClose { terminal: t }, TERM_TIMEOUT, w);
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
            TERM_TIMEOUT,
            w,
        );
    }

    /// Connected: `daemon.upgrade`, then the supervisor waits for the old Daemon to close
    /// the link before reconnecting. Incompatible: SIGTERM the Daemon through its pidfile,
    /// then reconnect.
    pub fn upgrade(&self, w: Waiter) {
        let once = Once::new(w);
        enum Plan {
            Request(Arc<Link>),
            Kill(CancelToken),
            Refuse(HostError),
        }
        let plan = {
            let mut st = self.sh.lock();
            let plan = match Shared::usable_link(&st) {
                Ok((l, _)) => Plan::Request(l),
                Err(_) if st.status.status == StatusKind::Incompatible => {
                    Plan::Kill(st.cancel.clone())
                }
                Err(e) => Plan::Refuse(e),
            };
            if !matches!(plan, Plan::Refuse(_)) {
                st.upgrading = true;
                st.upgrade_expect = true;
                st.status.phase = Some(Phase::Upgrading);
                self.sh.publish(&mut st);
            }
            plan
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
                if let Err(e) = link.request(ClientMsg::DaemonUpgrade, TERM_TIMEOUT, w) {
                    let mut st = self.sh.lock();
                    st.upgrading = false;
                    st.upgrade_expect = false;
                    st.status.phase = None;
                    self.sh.publish(&mut st);
                    drop(st);
                    once.call(Err(e));
                }
            }
            Plan::Kill(cancel) => {
                let sh = self.sh.clone();
                let o = once.clone();
                let cfg = self.config();
                let spawned = std::thread::Builder::new()
                    .name("host-upgrade".into())
                    .spawn(move || {
                        let t = sh.mc.transports.for_host(&cfg);
                        let r = crate::supervisor::kill_daemon(&*t, &cancel);
                        {
                            let mut st = sh.lock();
                            st.kick = true;
                            sh.cv.notify_all();
                        }
                        o.call(r.map(|_| Value::Null));
                    });
                if spawned.is_err() {
                    once.call(Err(HostError::offline("cannot start the upgrade")));
                }
            }
        }
    }

    /// Retry now instead of waiting out the backoff.
    pub fn kick(&self) {
        let mut st = self.sh.lock();
        st.kick = true;
        self.sh.cv.notify_all();
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
    use xshell_core::protocol::msg::ProtocolRange;
    use xshell_core::protocol::msg::ServerMsg;

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
        let mc = Arc::new(test_config(Arc::new(NullObserver)));
        Arc::new(Shared::new(
            crate::config::test_host("h_ab12cd34", "x"),
            mc,
            1,
        ))
    }

    /// What the supervisor does for one connection, over a scripted peer whose first list
    /// is `list`.
    fn connect(sh: &Arc<Shared>, list: Vec<TerminalInfo>) -> (Peer, Arc<Link>, u64) {
        let (io, mut peer) = pair();
        let mut b = hello_frame(1, 1, "1.5.0");
        b.extend(terminals_frame(list));
        peer.write(&b);
        let gen = sh.begin_link(None);
        let (link, _, _) = Link::establish(
            io,
            ProtocolRange { min: 1, max: 1 },
            "1.5.0",
            sh.events(gen),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(sh.wait_first_list(gen, Duration::from_secs(5)));
        assert!(sh.adopt(gen, &link, |st| st.status.set_kind(StatusKind::Connected)));
        peer.expect("hello");
        (peer, link, gen)
    }

    fn handle(sh: &Arc<Shared>) -> HostHandle {
        HostHandle {
            sh: sh.clone(),
            run: Mutex::new(None),
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
            st.link = Some(l3.clone());
        }
        let (w, _rx) = res_slot();
        h.term_attach(t, c.clone(), w);
        assert!(sh.adopt(gen, &l3, |_| {}));
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
        sh.lock().status.set_kind(StatusKind::Incompatible);
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
        assert_eq!(sh.lock().atts[&t].size, Some((90, 30)));
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
}

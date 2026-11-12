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
use crate::config::{HostConfig, HostDefinition, HostRoute};
use crate::errors::{HostError, HostErrorCode};
use crate::link::{Link, LinkEvents, Waiter};
use crate::manager::ManagerConfig;
use crate::status::{HostSnapshot, HostStatus, Phase, StatusKind, UpdateAction};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::transfer::UPLOAD_OUTCOME_UNKNOWN;
use xshell_protocol::msg::{ClientMsg, JoinExpect, OpenReply, OpenSpec, TerminalInfo};

/// Where one Tab's output goes.
pub trait TermSink: Send + Sync {
    /// `false`: the receiver is gone; the handle retires this sink.
    fn data(&self, bytes: &[u8]) -> bool;
    /// The Terminal ended. `bytes` is everything this sink received through `data`, so the
    /// receiver can apply the exit only after that much output (the exit watermark).
    fn exit(&self, code: i32, bytes: u64);
    /// Before replay from a replacement Link; byte watermarks remain cumulative.
    fn generation_reset(&self) {}
    /// This viewer exceeded its output budget and must explicitly reattach.
    fn resync_required(&self) {
        self.generation_reset();
    }
    /// A shared physical sink may reattach only while at least one viewer still proves
    /// its identity/configuration. Plain low-level sinks retain their existing behavior.
    fn can_reattach(&self) -> bool {
        true
    }
    fn reset_for_generation(&self, _generation: u64) {
        self.generation_reset();
    }
    /// Supply a replay cut to a Direct viewer joining an already attached broker sink.
    fn replay_snapshot(&self) -> Option<Vec<u8>> {
        None
    }
    /// `None` uses the low-level remembered size; `Some(None)` deliberately sends no
    /// resize because no currently authorized shared viewer has actually interacted.
    fn reattach_size(&self) -> Option<Option<(u16, u16)>> {
        None
    }
}

const SAVE_FILE_TIMEOUT: Duration = Duration::from_secs(120);
/// The hello capability of Daemons that serve `term.relaunch`.
const RELAUNCH_CAPABILITY: &str = xshell_protocol::cap::TERM_RELAUNCH;
/// The hello capability of Daemons that serve `ring.identity` and `ring.join`.
pub const RING_CAPABILITY: &str = "ring";
/// `ring.join` takes `expect`: the Daemon refuses the join unless its membership is still
/// the expected one.
pub const RING_CJOIN_CAPABILITY: &str = "ring.cjoin";
const RING_REFUSED: &str = "this Host's xshelld cannot join a ring; upgrade it first";
/// The hello capability of Daemons that run a [`LaunchSpec`]'s `launchPrefix`.
///
/// [`LaunchSpec`]: xshell_core::launch::LaunchSpec
const LAUNCH_PREFIX_CAPABILITY: &str = "launch.prefix";
/// The hello capability of Daemons that resume an agent session at most once: a `term.open`
/// may set `adoptExisting` and is answered with the Terminal that already runs it.
const OPEN_EXISTING_CAPABILITY: &str = xshell_protocol::cap::TERM_OPEN_EXISTING;
/// The `call` methods a Daemon serves only with capability `file.transfer`.
const FILE_TRANSFER_METHODS: &[&str] = &[
    "file_stat",
    "file_read",
    "make_dir",
    "upload_begin",
    "upload_chunk",
    "upload_commit",
    "upload_abort",
];

/// A Ring alias is access to an identity on this configured route, not unrestricted
/// access to every later Daemon reached by that route.
#[derive(Clone, Copy)]
struct IdentityTarget {
    daemon_key: xshell_protocol::ring::SignKey,
    config_generation: u64,
}
impl IdentityTarget {
    fn matches(
        self,
        config_generation: u64,
        identity: Option<xshell_protocol::ring::SignKey>,
    ) -> bool {
        self.config_generation == config_generation && identity == Some(self.daemon_key)
    }
    fn check(
        self,
        config_generation: u64,
        identity: Option<xshell_protocol::ring::SignKey>,
    ) -> Result<(), HostError> {
        self.matches(config_generation, identity)
            .then_some(())
            .ok_or_else(|| {
                HostError::offline("this Ring alias is not verified on the current Host connection")
            })
    }
}

pub(crate) struct SinkSlot {
    sink: Arc<dyn TermSink>,
    bytes: u64,
    identity: Option<IdentityTarget>,
}

impl SinkSlot {
    fn new(sink: Arc<dyn TermSink>, identity: Option<IdentityTarget>) -> Self {
        Self {
            sink,
            bytes: 0,
            identity,
        }
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

/// Identity of the connection shown when the user confirmed. Never retarget a queued action.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTarget {
    pub config_generation: u64,
    pub link_generation: u64,
    pub daemon_version: Option<String>,
    pub daemon_build: Option<xshell_protocol::build::BuildId>,
}

/// The connection observed at a Desktop input action; checked before enqueue, never replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputTarget {
    pub config_generation: u64,
    pub link_generation: u64,
}

pub(crate) struct State {
    pub cfg: HostDefinition,
    pub status: HostStatus,
    published: Option<HostStatus>,
    pub terminals: Option<Vec<TerminalInfo>>,
    /// The generation of the newest link (events of older links are ignored).
    pub gen: u64,
    /// The usable link; always of generation `gen`. Change it only through `set_link`.
    link: Option<Arc<Link>>,
    /// Proof for exactly `gen`, cleared before a different Link can be selected.
    identity: Option<xshell_protocol::ring::SignKey>,
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
    /// After an accepted stop, the next usable connection must report the expected version/build.
    pub upgrade_expect: bool,
    pub upgrade_operation: Option<u64>,
    pub upgrade_complete_deadline: Option<Instant>,
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
    input_target: InputTarget,
    identity: Option<xshell_protocol::ring::SignKey>,
    completion_attention: bool,
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
    relay: Option<crate::ring::dial::RelayDialer>,
    /// Last identity accepted on a current link for this configured target. A contradiction
    /// survives reconnects until new current proof; retarget/reset clears it.
    observed_identity: Mutex<Option<xshell_protocol::ring::SignKey>>,
    /// The last size this Desktop set per Terminal, re-sent after a re-attach.
    sizes: Mutex<HashMap<Uuid, (u16, u16)>>,
    /// The latest status and list, for `snapshot`.
    snap: Mutex<HostSnapshot>,
    hubs: Mutex<HashMap<Uuid, Arc<crate::attachments::Hub>>>,
    direct_viewer: Uuid,
    facade_lock: Mutex<()>,
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
        Self::new_definition(cfg.into(), mc, config_generation)
    }

    fn new_definition(cfg: HostDefinition, mc: Arc<ManagerConfig>, config_generation: u64) -> Self {
        mc.action_targets.register(&cfg.id, config_generation);
        let mut status = HostStatus::initial(
            &cfg.id,
            &mc.desktop_version,
            mc.desktop_build.clone(),
            config_generation,
        );
        if matches!(cfg.route, HostRoute::Relay(_)) {
            status.route_update_unavailable =
                Some(crate::status::RouteUpdateUnavailable::RelayOnly);
        }
        let down = HostError::offline(format!("{} is reconnecting", cfg.name));
        Self {
            observed_identity: Mutex::new(None),
            relay: match &cfg.route {
                HostRoute::Relay(d) => Some(d.clone()),
                _ => None,
            },
            id: cfg.id.clone(),
            mc,
            hubs: Mutex::new(HashMap::new()),
            direct_viewer: Uuid::new_v4(),
            facade_lock: Mutex::new(()),
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
                identity: None,
                link: None,
                link_closed: false,
                close_reason: None,
                list_gen: 0,
                atts: HashMap::new(),
                epoch: 0,
                upgrading: false,
                upgrade_deadline: None,
                upgrade_expect: false,
                upgrade_operation: None,
                upgrade_complete_deadline: None,
            }),
            cv: Condvar::new(),
            kick: AtomicBool::new(false),
            child_pid: AtomicU32::new(0),
            ep: Mutex::new(Endpoint {
                identity: None,
                link: None,
                down,
                completion_attention: false,
                input_target: InputTarget {
                    config_generation,
                    link_generation: 0,
                },
            }),
            sizes: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    fn sync_ep(&self, st: &State) {
        let mut ep = self.ep.lock().unwrap();
        ep.link = st.link.clone();
        ep.identity = st.identity;
        ep.input_target = InputTarget {
            config_generation: st.status.config_generation,
            // Adoption exposes this link before publishing its status. The internal
            // generation already identifies it; status may still name its predecessor.
            link_generation: st.gen,
        };
        ep.completion_attention = st
            .status
            .daemon_capabilities
            .iter()
            .any(|c| c == xshell_protocol::cap::TERM_COMPLETION_ATTENTION);
        ep.down = unusable(st);
    }

    /// The only way the usable link changes, so the sync paths see it too.
    pub(crate) fn set_link(&self, st: &mut State, link: Option<Arc<Link>>) {
        st.link = link;
        self.sync_ep(st);
    }

    /// Emit the status if it changed since the last emit.
    pub(crate) fn publish(&self, st: &mut State) {
        st.status.update_action = st
            .status
            .daemon_version
            .as_deref()
            .filter(|v| {
                crate::version::update_eligible(
                    &self.mc.desktop_version,
                    self.mc.desktop_build.as_ref(),
                    v,
                    st.status.daemon_build.as_ref(),
                )
            })
            .map(|v| {
                if semver::Version::parse(v)
                    .ok()
                    .zip(semver::Version::parse(&self.mc.desktop_version).ok())
                    .is_some_and(|(d, us)| d.cmp_precedence(&us).is_lt())
                {
                    UpdateAction::Update
                } else {
                    UpdateAction::Replace
                }
            });
        if st.status.route_update_unavailable.is_some() {
            st.status.update_action = None;
        }
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
        st.identity = None;
        self.set_link(&mut st, None);
        for hub in self.hubs.lock().unwrap().values() {
            hub.reset_for_generation(st.gen);
        }
        st.link_closed = false;
        st.close_reason = None;
        self.child_pid
            .store(child_pid.unwrap_or(0), Ordering::SeqCst);
        st.gen
    }

    fn observe_identity(self: &Arc<Self>, gen: u64, key: xshell_protocol::ring::SignKey) -> bool {
        let mut st = self.lock();
        if st.gen != gen || st.link_closed || st.link().is_none_or(|link| link.is_closed()) {
            return false;
        }
        *self.observed_identity.lock().unwrap() = Some(key);
        if st.identity.is_some_and(|previous| previous != key) {
            for hub in self.hubs.lock().unwrap().values() {
                hub.generation_reset();
            }
        }
        st.identity = Some(key);
        self.sync_ep(&st);
        for hub in self.hubs.lock().unwrap().values() {
            hub.refresh_proof();
        }
        if let Some(link) = st.link().cloned() {
            if self.reattach_locked(&mut st, gen, &link) > 0 {
                // Adoption may have skipped these while identity was unknown. Reuse the
                // supervisor's bounded retry loop if proof arrives under queue pressure.
                self.kick.store(true, Ordering::SeqCst);
                self.cv.notify_all();
            }
        }
        true
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
            if st.gen != gen || st.link_closed || !st.cfg.authorized() {
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
        if st.gen != gen || st.link_closed || !st.cfg.authorized() {
            return None;
        }
        self.set_link(&mut st, Some(link.clone()));
        for hub in self.hubs.lock().unwrap().values() {
            hub.refresh_proof();
        }
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
        st.status.link_generation = gen;
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
            let desired = st
                .atts
                .get(&t)
                .and_then(|a| a.pending.as_ref().map(|p| &p.1).or(a.sink.as_ref()));
            if desired.is_none_or(|slot| {
                !slot.sink.can_reattach()
                    || slot.identity.is_some_and(|target| {
                        !target.matches(st.status.config_generation, st.identity)
                    })
            }) {
                continue;
            }
            match st.atts.get_mut(&t) {
                // An explicit attach on this link already replayed into the newest sink.
                None => continue,
                Some(a) if a.attached_gen == gen => continue,
                // Requests of older links can no longer complete (they are fenced).
                Some(a) => a.inflight.clear(),
            }
            if let Some(slot) = st
                .atts
                .get(&t)
                .and_then(|a| a.pending.as_ref().map(|p| &p.1).or(a.sink.as_ref()))
            {
                slot.sink.reset_for_generation(gen);
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
                    let size = a
                        .sink
                        .as_ref()
                        .and_then(|slot| slot.sink.reattach_size())
                        .unwrap_or_else(|| self.sizes.lock().unwrap().get(&t).copied());
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
            let desired = st
                .atts
                .get(&t)
                .and_then(|a| a.pending.as_ref().map(|p| &p.1).or(a.sink.as_ref()));
            if desired.is_some_and(|slot| {
                slot.identity
                    .is_some_and(|target| !target.matches(st.status.config_generation, st.identity))
            }) {
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
        if !st.cfg.authorized() {
            return Err(HostError::offline("the Ring Host authorization changed"));
        }
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
        identity: Option<IdentityTarget>,
        done: Callback<Reply>,
    ) -> Result<(), HostError> {
        let epoch = st.next_epoch();
        let fresh = !st.atts.contains_key(&t);
        if fresh {
            st.atts.insert(
                t,
                Attachment {
                    sink: Some(SinkSlot::new(sink, identity)),
                    pending: None,
                    attached_gen: gen,
                    epoch,
                    inflight: HashSet::from([epoch]),
                },
            );
        } else {
            let a = st.atts.get_mut(&t).expect("exists");
            a.pending = Some((epoch, SinkSlot::new(sink, identity)));
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
        let config_generation = st.status.config_generation;
        let identity = st.identity;
        let Some(a) = st.atts.get_mut(&t) else {
            return;
        };
        let Some(slot) = a.sink.as_mut() else {
            // The previous sink is retired and the replacement's reply has not arrived:
            // this output belongs to the previous attach.
            return;
        };
        if slot
            .identity
            .is_some_and(|target| !target.matches(config_generation, identity))
        {
            return;
        }
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
            if s.identity
                .is_none_or(|target| target.matches(st.status.config_generation, st.identity))
            {
                s.sink.exit(code, s.bytes);
            }
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

struct IdentityAccess {
    owner: Arc<HostHandle>,
    target: IdentityTarget,
}

/// Host access. The canonical instance owns its supervisor; an identity-bound alias view
/// shares its State and owner lifetime, but carries its own immutable admission target.
/// Replacing a configuration restarts the canonical supervisor, not its alias views.
pub struct HostHandle {
    pub(crate) sh: Arc<Shared>,
    identity_access: Option<IdentityAccess>,
    generation_access: Option<InputTarget>,
    run: Mutex<Option<Running>>,
    /// Shut down for good: `start` does nothing any more.
    retired: AtomicBool,
}

/// A `term.open` reply; an older Daemon's `{pid}` (or anything unexpected) reads as a
/// started Terminal with that pid.
fn open_reply(v: &Value) -> OpenReply {
    serde_json::from_value(v.clone()).unwrap_or_else(|_| OpenReply {
        pid: opt_u32(v, "pid"),
        ..OpenReply::default()
    })
}

fn opt_u32(v: &Value, k: &str) -> Option<u32> {
    v.get(k).and_then(Value::as_u64).map(|n| n as u32)
}

impl HostHandle {
    pub(crate) fn new(cfg: HostConfig, mc: Arc<ManagerConfig>, config_generation: u64) -> Self {
        Self {
            sh: Arc::new(Shared::new(cfg, mc, config_generation)),
            identity_access: None,
            generation_access: None,
            run: Mutex::new(None),
            retired: AtomicBool::new(false),
        }
    }

    /// An identity-bound view shares the owner Link/attachments, never another supervisor.
    pub(crate) fn for_identity(
        owner: Arc<Self>,
        daemon_key: xshell_protocol::ring::SignKey,
    ) -> Arc<Self> {
        let config_generation = owner.sh.ep.lock().unwrap().input_target.config_generation;
        Arc::new(Self {
            sh: owner.sh.clone(),
            identity_access: Some(IdentityAccess {
                owner,
                target: IdentityTarget {
                    daemon_key,
                    config_generation,
                },
            }),
            generation_access: None,
            run: Mutex::new(None),
            retired: AtomicBool::new(true),
        })
    }

    /// A dispatch view checks the expected connection under the actual admission lock.
    pub(crate) fn for_generation(owner: Arc<Self>, target: InputTarget) -> Arc<Self> {
        Arc::new(Self {
            sh: owner.sh.clone(),
            identity_access: owner.identity_access.as_ref().map(|access| IdentityAccess {
                owner: access.owner.clone(),
                target: access.target,
            }),
            generation_access: Some(target),
            run: Mutex::new(None),
            retired: AtomicBool::new(true),
        })
    }

    fn identity_target(&self) -> Option<IdentityTarget> {
        self.identity_access.as_ref().map(|access| access.target)
    }

    fn usable_link(&self, st: &State) -> Result<(Arc<Link>, u64), HostError> {
        if let Some(target) = self.identity_target() {
            target.check(st.status.config_generation, st.identity)?;
        }
        if self.generation_access.is_some_and(|target| {
            target.config_generation != st.status.config_generation
                || target.link_generation != st.gen
        }) {
            return Err(HostError::offline("the broker connection changed"));
        }
        Shared::usable_link(st)
    }

    fn usable_endpoint(&self, ep: &Endpoint) -> Result<Arc<Link>, HostError> {
        if self
            .generation_access
            .is_some_and(|target| target != ep.input_target)
        {
            return Err(HostError::offline("the broker connection changed"));
        }
        if let Some(target) = self.identity_target() {
            target.check(ep.input_target.config_generation, ep.identity)?;
        }
        if self
            .sh
            .relay
            .as_ref()
            .is_some_and(|dialer| !dialer.authorized())
        {
            return Err(HostError::offline("the Ring Host authorization changed"));
        }
        match &ep.link {
            Some(link) if !link.is_closed() => Ok(link.clone()),
            _ => Err(ep.down.clone()),
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
        if let Some(access) = &self.identity_access {
            return access.owner.cancel_token();
        }
        self.run
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.cancel.clone())
            .unwrap_or_default()
    }

    /// After a stop: take the new configuration, fence the old links, keep attachments.
    pub(crate) fn reset(&self, cfg: HostConfig, config_generation: u64) {
        self.sh
            .mc
            .action_targets
            .register(&self.sh.id, config_generation);
        let closing = {
            let mut st = self.sh.lock();
            st.gen += 1;
            let link = st.link.take();
            st.cfg = cfg.into();
            st.identity = None;
            *self.sh.observed_identity.lock().unwrap() = None;
            st.terminals = None;
            st.list_gen = 0;
            st.link_closed = false;
            st.close_reason = None;
            self.sh.child_pid.store(0, Ordering::SeqCst);
            st.upgrading = false;
            st.upgrade_deadline = None;
            st.upgrade_expect = false;
            st.upgrade_operation = None;
            st.upgrade_complete_deadline = None;
            self.sh.kick.store(false, Ordering::SeqCst);
            for a in st.atts.values_mut() {
                a.attached_gen = 0;
                a.inflight.clear();
            }
            st.status = HostStatus::initial(
                &self.sh.id,
                &self.sh.mc.desktop_version,
                self.sh.mc.desktop_build.clone(),
                config_generation,
            );
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

    /// Saved/configured route only; discovered Relay Hosts have no SSH configuration.
    pub fn saved_config(&self) -> Option<HostConfig> {
        self.sh.lock().cfg.configured().cloned()
    }

    pub fn display_name(&self) -> String {
        self.sh.lock().cfg.name.clone()
    }

    pub(crate) fn allows_ring_identity(&self, key: &xshell_protocol::ring::SignKey) -> bool {
        self.sh
            .observed_identity
            .lock()
            .unwrap()
            .is_none_or(|observed| observed == *key)
    }

    pub(crate) fn relay_route(&self) -> Option<crate::ring::dial::RelayDialer> {
        self.sh.relay.clone()
    }

    pub(crate) fn new_relay(
        dialer: crate::ring::dial::RelayDialer,
        mc: Arc<ManagerConfig>,
        generation: u64,
    ) -> Self {
        let sh = Shared::new_definition(
            HostDefinition {
                id: dialer.host.id(),
                name: dialer.host.name.clone(),
                route: HostRoute::Relay(dialer),
            },
            mc,
            generation,
        );
        Self {
            sh: Arc::new(sh),
            identity_access: None,
            generation_access: None,
            run: Mutex::new(None),
            retired: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_display(&self, cfg: HostConfig) {
        self.sh.lock().cfg = cfg.into();
    }

    pub fn status(&self) -> HostStatus {
        self.snapshot().status
    }

    pub fn terminals(&self) -> Option<Vec<TerminalInfo>> {
        self.snapshot().terminals
    }

    /// The latest status and list. Never waits on deliveries.
    pub fn snapshot(&self) -> HostSnapshot {
        let mut snapshot = self.sh.snapshot();
        if self.identity_access.is_some() {
            snapshot.status.route_update_unavailable =
                Some(crate::status::RouteUpdateUnavailable::IdentityBoundAlias);
            snapshot.status.update_action = None;
            let admitted = quick(&self.sh.ep).and_then(|ep| self.usable_endpoint(&ep));
            if let Err(error) = admitted {
                snapshot.status.set_kind(StatusKind::Offline);
                snapshot.status.last_error = Some(error.message);
                snapshot.terminals = None;
            }
        }
        snapshot
    }

    pub(crate) fn access_snapshot(&self) -> HostSnapshot {
        let mut snapshot = self.snapshot();
        if let Some(status) = self.sh.mc.action_targets.direct_status(&snapshot.status) {
            snapshot.status = status;
        } else {
            snapshot.status.set_kind(StatusKind::Offline);
            snapshot.status.config_generation = 0;
            snapshot.status.link_generation = 0;
        }
        snapshot
    }
    /// The pid of the current transport process (ssh), for tests.
    #[doc(hidden)]
    pub fn child_pid(&self) -> Option<u32> {
        Some(self.sh.child_pid.load(Ordering::SeqCst)).filter(|p| *p != 0)
    }

    fn request(&self, msg: ClientMsg, timeout: Duration, w: Waiter) {
        self.request_mapped(msg, timeout, w, |e| e)
    }

    /// [`Self::request`], with `in_flight` applied to the errors of a request that was sent
    /// (its timeout, a lost connection); one that never left keeps its error.
    fn request_mapped(
        &self,
        msg: ClientMsg,
        timeout: Duration,
        w: Waiter,
        in_flight: fn(HostError) -> HostError,
    ) {
        let once = Once::new(w);
        let broker_mutation = self.generation_access.is_some();
        let sent = {
            let st = self.sh.lock();
            self.usable_link(&st).and_then(|(l, _)| {
                let o = once.clone();
                l.request(
                    msg,
                    timeout,
                    Box::new(move |r| {
                        o.call(r.map_err(|e| {
                            let e = in_flight(e);
                            if broker_mutation {
                                e.mutation_outcome()
                            } else {
                                e
                            }
                        }))
                    }),
                )
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    fn configured_lifecycle_refusal(&self) -> Option<HostError> {
        self.identity_access.as_ref().map(|_| HostError::invalid(
            "Identity-bound alias access cannot manage the Daemon or Ring; use the explicitly configured Host"))
    }

    pub fn call(&self, method: String, params: Value, w: Waiter) {
        if method.starts_with("daemon.") || method.starts_with("ring.") {
            if let Some(error) = self.configured_lifecycle_refusal() {
                w(Err(error));
                return;
            }
        }
        if self.sh.relay.is_some() && (method.starts_with("daemon.") || method.starts_with("ring."))
        {
            w(Err(HostError::invalid(
                "Relay-only access cannot change Daemon lifecycle or Ring membership",
            )));
            return;
        }
        // An older Daemon would answer "unknown method": refuse with the remedy instead.
        if FILE_TRANSFER_METHODS.contains(&method.as_str()) {
            let lacking = {
                let st = self.sh.lock();
                self.usable_link(&st).is_ok()
                    && !st
                        .status
                        .daemon_capabilities
                        .iter()
                        .any(|c| c == xshell_protocol::cap::FILE_TRANSFER)
            };
            if lacking {
                w(Err(HostError::invalid(
                    "this Host's xshelld cannot transfer files; upgrade it first",
                )));
                return;
            }
        }
        let timeout = match method.as_str() {
            "save_dropped_file" => SAVE_FILE_TIMEOUT,
            "upload_commit" | "file_stat" => self.sh.mc.long_call_timeout,
            _ => self.sh.mc.call_timeout,
        };
        // A commit sent without an answer (timed out, connection lost) may have published
        // the file or not: say so, so the client checks the target instead of retrying.
        let in_flight: fn(HostError) -> HostError = if method == "upload_commit" {
            |e| match e.code {
                HostErrorCode::Timeout | HostErrorCode::Offline => HostError::new(
                    HostErrorCode::Indeterminate,
                    format!("{UPLOAD_OUTCOME_UNKNOWN}: {}", e.message),
                ),
                _ => e,
            }
        } else {
            |e| e
        };
        self.request_mapped(ClientMsg::Call { method, params }, timeout, w, in_flight);
    }

    pub(crate) fn direct_viewer(&self) -> Uuid {
        self.sh.direct_viewer
    }

    fn register_viewer(
        &self,
        terminal: Uuid,
        key: Uuid,
        sink: Arc<dyn TermSink>,
        opening: bool,
    ) -> Result<
        (
            Arc<crate::attachments::Hub>,
            crate::attachments::Registration,
        ),
        HostError,
    > {
        let mut hubs = self.sh.hubs.lock().unwrap();
        hubs.retain(|_, hub| !hub.empty());
        if !hubs.contains_key(&terminal) && hubs.len() >= 64 {
            return Err(HostError::busy());
        }
        let hub = hubs
            .entry(terminal)
            .or_insert_with(|| crate::attachments::Hub::new(self.sh.mc.replay_budget.clone()))
            .clone();
        let direct = key == self.sh.direct_viewer;
        let sink = if direct {
            crate::ring::broker::queued_sink(sink, self.sh.mc.output_bytes.clone())
        } else {
            sink
        };
        let config = self.sh.ep.lock().unwrap().input_target.config_generation;
        let sh = Arc::downgrade(&self.sh);
        let identity = self.identity_target();
        let resize: crate::attachments::ScopedResize = Box::new(move |cols, rows| {
            let sh = sh
                .upgrade()
                .ok_or_else(|| HostError::offline("Host retired"))?;
            let ep = quick(&sh.ep)?;
            if ep.input_target.config_generation != config
                || identity.is_some_and(|target| {
                    !target.matches(ep.input_target.config_generation, ep.identity)
                })
                || sh.relay.as_ref().is_some_and(|route| !route.authorized())
            {
                return Err(HostError::offline("attachment connection changed"));
            }
            ep.link
                .as_ref()
                .filter(|link| !link.is_closed())
                .ok_or_else(|| HostError::offline("Host offline"))?
                .notify(ClientMsg::TermResize {
                    terminal,
                    cols,
                    rows,
                })?;
            quick(&sh.sizes)?.insert(terminal, (cols, rows));
            Ok(())
        });
        let first = hub.register(
            key,
            sink,
            crate::attachments::ViewerAccess {
                fence: self.attachment_fence(),
                config,
                resize,
                direct,
            },
            opening,
        )?;
        Ok((hub, first))
    }
    fn attachment_fence(&self) -> crate::attachments::Fence {
        let sh = Arc::downgrade(&self.sh);
        let identity = self.identity_target();
        let config = self.sh.ep.lock().unwrap().input_target.config_generation;
        Box::new(move || {
            let Some(sh) = sh.upgrade() else {
                return false;
            };
            let ep = sh.ep.lock().unwrap();
            ep.input_target.config_generation == config
                && ep.link.as_ref().is_some_and(|l| !l.is_closed())
                && identity.is_none_or(|target| {
                    target.matches(ep.input_target.config_generation, ep.identity)
                })
                && sh.relay.as_ref().is_none_or(|route| route.authorized())
        })
    }
    pub(crate) fn access_term_open(
        &self,
        key: Uuid,
        spec: OpenSpec,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<OpenReply, HostError>) + Send>,
    ) {
        let admission = self.sh.facade_lock.lock().unwrap();
        let (hub, registration) = match self.register_viewer(spec.terminal, key, sink, true) {
            Ok(hub) => hub,
            Err(error) => {
                drop(admission);
                w(Err(error));
                return;
            }
        };
        let _ = hub.interaction(key, Some((spec.cols, spec.rows)));
        let terminal = spec.terminal;
        let done = hub.clone();
        self.term_open_slot(
            spec,
            hub,
            Box::new(move |result| {
                done.complete(
                    registration.epoch,
                    result.as_ref().map(|_| None).map_err(Clone::clone),
                );
                if result
                    .as_ref()
                    .is_err_and(|e| e.code != HostErrorCode::Indeterminate)
                    || result
                        .as_ref()
                        .is_ok_and(|r| r.existed && r.terminal.is_some_and(|id| id != terminal))
                {
                    done.remove_current(registration.epoch, key);
                }
                w(result);
            }),
            true,
            Some(admission),
        );
    }
    pub(crate) fn access_term_attach(
        &self,
        key: Uuid,
        terminal: Uuid,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<Option<i32>, HostError>) + Send>,
    ) {
        let admission = self.sh.facade_lock.lock().unwrap();
        let (hub, registration) = match self.register_viewer(terminal, key, sink, false) {
            Ok(result) => result,
            Err(e) => {
                drop(admission);
                w(Err(e));
                return;
            }
        };
        if registration.physical {
            let done = hub.clone();
            self.term_attach_slot(
                terminal,
                hub,
                Box::new(move |result| {
                    // Joining error callbacks may retry before this callback returns.
                    if result.is_err() {
                        done.remove_current(registration.epoch, key);
                    }
                    done.complete(registration.epoch, result.clone());
                    w(result);
                }),
                true,
                Some(admission),
            );
        } else {
            drop(admission);
            let done = hub.clone();
            hub.wait(
                registration.epoch,
                Box::new(move |result| {
                    if result.is_err() {
                        done.remove_current(registration.epoch, key);
                    }
                    w(result);
                }),
            );
        }
    }
    pub(crate) fn access_term_detach(&self, key: Uuid, terminal: Uuid) -> Result<(), HostError> {
        let _admission = self.sh.facade_lock.lock().unwrap();
        let hub = self.sh.hubs.lock().unwrap().get(&terminal).cloned();
        if let Some(hub) = hub {
            let (empty, _) = hub.remove(key);
            if empty {
                self.sh.hubs.lock().unwrap().remove(&terminal);
                let mut st = self.sh.lock();
                // Forgetting an invalid alias's own subscription is local, not permission
                // to notify a different Daemon. Independent Direct access remains valid.
                if self.usable_link(&st).is_ok() {
                    Shared::detach_locked(&mut st, terminal, &self.sh.sizes);
                } else {
                    st.atts.remove(&terminal);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn restore_viewer_size(
        &self,
        key: Uuid,
        terminal: Uuid,
        size: (u16, u16),
        sequence: u64,
    ) {
        if let Some(hub) = self.sh.hubs.lock().unwrap().get(&terminal).cloned() {
            hub.restore_viewer_size(key, size, sequence);
        }
    }
    pub(crate) fn access_term_input(
        &self,
        key: Uuid,
        terminal: Uuid,
        data: String,
        observed: Option<Uuid>,
        target: Option<InputTarget>,
    ) -> Result<u64, HostError> {
        let hub = self.sh.hubs.lock().unwrap().get(&terminal).cloned();
        let dispatch = |size: Option<(u16, u16)>, config: Option<u64>| {
            let ep = quick(&self.sh.ep)?;
            let target = target
                .map(|target| {
                    if key == self.sh.direct_viewer {
                        self.sh
                            .mc
                            .action_targets
                            .direct(&self.sh.id, target, ep.input_target)
                    } else {
                        Ok(target)
                    }
                })
                .transpose()?;
            if target.is_some_and(|target| target != ep.input_target)
                || config.is_some_and(|config| config != ep.input_target.config_generation)
            {
                return Err(HostError::offline("the input connection changed"));
            }
            let link = self.usable_endpoint(&ep)?;
            if let Some((cols, rows)) = size {
                link.notify(ClientMsg::TermResize {
                    terminal,
                    cols,
                    rows,
                })?;
                quick(&self.sh.sizes)?.insert(terminal, (cols, rows));
            }
            link.notify(ClientMsg::TermInput {
                terminal,
                data,
                handled_completion_id: observed.filter(|_| ep.completion_attention),
            })
        };
        if let Some(hub) = hub {
            hub.perform(key, None, dispatch)
        } else {
            dispatch(None, None).map(|_| crate::attachments::interaction())
        }
    }
    pub(crate) fn access_term_resize(
        &self,
        key: Uuid,
        terminal: Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<u64, HostError> {
        let hub = self.sh.hubs.lock().unwrap().get(&terminal).cloned();
        let dispatch = |_: Option<(u16, u16)>, config: Option<u64>| {
            let ep = quick(&self.sh.ep)?;
            if config.is_some_and(|config| config != ep.input_target.config_generation) {
                return Err(HostError::offline("the resize connection changed"));
            }
            self.usable_endpoint(&ep)?.notify(ClientMsg::TermResize {
                terminal,
                cols,
                rows,
            })?;
            quick(&self.sh.sizes)?.insert(terminal, (cols, rows));
            Ok(())
        };
        if let Some(hub) = hub {
            hub.perform(key, Some((cols, rows)), dispatch)
        } else {
            dispatch(None, None).map(|_| crate::attachments::interaction())
        }
    }

    /// `term.open`, then `term.attach` with `sink` installed first. Answers the open's reply.
    ///
    /// `spec.adopt_existing` is the caller's wish; it is sent only to a Daemon with capability
    /// `term.open-existing` (an older one answers just the pid). When the Daemon answers that
    /// another Terminal already runs the session (`existed`), the attach to the requested UUID
    /// fails by design: its sink and size are dropped and the owner's id is answered.
    pub fn term_open(
        &self,
        spec: OpenSpec,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<OpenReply, HostError>) + Send>,
    ) {
        self.term_open_slot(spec, sink, w, false, None);
    }

    fn term_open_slot(
        &self,
        spec: OpenSpec,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<OpenReply, HostError>) + Send>,
        shared: bool,
        admission: Option<MutexGuard<'_, ()>>,
    ) {
        let once = Once::new(w);
        let t = spec.terminal;
        let size = (spec.cols, spec.rows);
        // Both replies are joined; the second one to finish answers.
        let joined: Joined = Arc::new(Mutex::new((0, None)));
        let broker_mutation = self.generation_access.is_some();
        let finish = {
            let once = once.clone();
            let sh = Arc::downgrade(&self.sh);
            let identity = self.identity_target();
            move |open: Option<Reply>, attach: Reply| {
                let open = open.map(|r| {
                    r.map_err(|e| {
                        if broker_mutation {
                            e.mutation_outcome()
                        } else {
                            e
                        }
                    })
                    .map(|v| open_reply(&v))
                });
                let attach = attach.map_err(|e| {
                    if broker_mutation {
                        e.mutation_outcome()
                    } else {
                        e
                    }
                });
                let r = match (open, attach) {
                    // Another Terminal runs the session: nothing listens on `t`.
                    (Some(Ok(o)), _) if o.existed && o.terminal.is_some_and(|o| o != t) => {
                        if let Some(sh) = sh.upgrade() {
                            let mut st = sh.lock();
                            if identity.is_none_or(|target| {
                                target.matches(st.status.config_generation, st.identity)
                            }) {
                                Shared::detach_locked(&mut st, t, &sh.sizes);
                                sh.sizes.lock().unwrap().remove(&t);
                            }
                        }
                        Ok(o)
                    }
                    (Some(Err(e)), _) | (_, Err(e)) => Err(e),
                    (Some(Ok(o)), Ok(_)) => Ok(o),
                    (None, Ok(_)) => Ok(OpenReply::default()),
                };
                once.call(r);
            }
        };
        let finish = Arc::new(Mutex::new(Some(finish)));
        let on_attach = {
            let (j2, f2) = (joined.clone(), finish.clone());
            Arc::new(move |r: Reply| {
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
            })
        };
        let timeout = self.sh.mc.term_timeout;
        let mut spec = spec;
        let sent = {
            let mut st = self.sh.lock();
            self.usable_link(&st).and_then(|(link, gen)| {
                // The Host's configuration decides the prefix, never the request. A Daemon
                // that predates prefixes would drop it and run the agent bare, so it is
                // refused instead.
                spec.launch.launch_prefix = st.cfg.launch_prefix(&spec.launch);
                let caps = &st.status.daemon_capabilities;
                if spec.launch.launch_prefix.is_some()
                    && !caps.iter().any(|c| c == LAUNCH_PREFIX_CAPABILITY)
                {
                    return Err(HostError::invalid(
                        "this Host's xshelld cannot run launch prefixes; upgrade it first",
                    ));
                }
                spec.adopt_existing &= caps.iter().any(|c| c == OPEN_EXISTING_CAPABILITY);
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
                let a = on_attach.clone();
                let done: Callback<Reply> = Box::new(move |r| a(r));
                // If this fails, the open still runs: its answer decides (an adopted owner,
                // or the Terminal shows up in the list), so the failure joins it below,
                // outside the lock.
                Ok(self
                    .sh
                    .begin_attach(
                        &mut st,
                        (&link, gen),
                        (t, sink),
                        if shared { None } else { self.identity_target() },
                        done,
                    )
                    .err())
            })
        };
        drop(admission);
        match sent {
            Err(e) => once.call(Err(e)),
            Ok(Some(e)) => on_attach(Err(e)),
            Ok(None) => {}
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
        self.term_attach_slot(t, sink, w, false, None);
    }

    fn term_attach_slot(
        &self,
        t: Uuid,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<Option<i32>, HostError>) + Send>,
        shared: bool,
        admission: Option<MutexGuard<'_, ()>>,
    ) {
        let once = Once::new(w);
        let o = once.clone();
        let done: Callback<Reply> = Box::new(move |r| {
            o.call(r.map(|v| v.get("exitCode").and_then(Value::as_i64).map(|c| c as i32)))
        });
        let sent = {
            let mut st = self.sh.lock();
            self.usable_link(&st).and_then(|(link, gen)| {
                self.sh.begin_attach(
                    &mut st,
                    (&link, gen),
                    (t, sink),
                    if shared { None } else { self.identity_target() },
                    done,
                )
            })
        };
        drop(admission);
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    /// Forget the sink. Independent configured access can detach offline; an alias must
    /// still prove its current identity before changing shared attachments or notifying.
    pub fn term_detach(&self, t: Uuid) -> Result<(), HostError> {
        let mut st = self.sh.lock();
        if self.identity_access.is_some() {
            self.usable_link(&st)?;
        }
        Shared::detach_locked(&mut st, t, &self.sh.sizes);
        Ok(())
    }

    /// Sync paths check identity and enqueue under the same Endpoint lock that Link
    /// changes/proof observations update. They never wait for a sink's Host State lock.
    pub fn term_input(
        &self,
        t: Uuid,
        data: String,
        observed: Option<Uuid>,
        target: Option<InputTarget>,
    ) -> Result<(), HostError> {
        let ep = quick(&self.sh.ep)?;
        if target.is_some_and(|target| target != ep.input_target) {
            return Err(HostError::offline("the input connection changed"));
        }
        self.usable_endpoint(&ep)?.notify(ClientMsg::TermInput {
            terminal: t,
            data,
            handled_completion_id: observed.filter(|_| ep.completion_attention),
        })
    }

    /// The size is remembered offline for independent configured access, never from a
    /// refused identity-bound alias that could otherwise resize a different Daemon later.
    pub fn term_resize(&self, t: Uuid, cols: u16, rows: u16) -> Result<(), HostError> {
        if self.identity_access.is_none() {
            quick(&self.sh.sizes)?.insert(t, (cols, rows));
        }
        let ep = quick(&self.sh.ep)?;
        let link = self.usable_endpoint(&ep)?;
        if self.identity_access.is_some() {
            quick(&self.sh.sizes)?.insert(t, (cols, rows));
        }
        link.notify(ClientMsg::TermResize {
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
        let custom = meta
            .as_ref()
            .is_some_and(|m| m.contains_key(xshell_protocol::terminal_meta::CUSTOM_TITLE_KEY));
        let msg = ClientMsg::TermUpdate {
            terminal: t,
            session_id,
            meta,
        };
        if custom {
            self.gated(
                msg,
                &[xshell_protocol::cap::TERM_CUSTOM_TITLE],
                "this Daemon does not support custom Tab names; update it first",
                None,
                w,
            );
        } else {
            self.request(msg, self.sh.mc.term_timeout, w);
        }
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
        let broker_mutation = self.generation_access.is_some();
        let sent = {
            // The capability and the link are checked and used under one lock: `adopt`
            // replaces both together.
            let st = self.sh.lock();
            self.usable_link(&st).and_then(|(l, _)| {
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
                    Box::new(move |r: Reply| {
                        o.call(
                            r.map_err(|e| {
                                if broker_mutation {
                                    e.mutation_outcome()
                                } else {
                                    e
                                }
                            })
                            .map(|v| opt_u32(&v, "pid")),
                        )
                    }),
                )
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    /// A request sent only when the Daemon of the current link advertises every one of
    /// `capabilities`; refused with `refusal` without sending anything otherwise. With
    /// `link_gen`, only on that link generation (the connection the caller saw): after a
    /// reconnect it is refused as offline, unsent.
    fn gated(
        &self,
        msg: ClientMsg,
        capabilities: &[&str],
        refusal: &str,
        link_gen: Option<u64>,
        w: Waiter,
    ) {
        let once = Once::new(w);
        let broker_mutation = self.generation_access.is_some();
        let sent = {
            // The capability, the generation and the link are checked and used under one
            // lock: `adopt` replaces them together.
            let st = self.sh.lock();
            self.usable_link(&st).and_then(|(l, gen)| {
                if link_gen.is_some_and(|g| g != gen) {
                    return Err(HostError::offline(format!("{} reconnected", st.cfg.name)));
                }
                let has = |c: &&str| st.status.daemon_capabilities.iter().any(|x| x == c);
                if !capabilities.iter().all(has) {
                    return Err(HostError::invalid(refusal));
                }
                let o = once.clone();
                l.request(
                    msg,
                    self.sh.mc.term_timeout,
                    Box::new(move |r| {
                        o.call(r.map_err(|e| {
                            if broker_mutation {
                                e.mutation_outcome()
                            } else {
                                e
                            }
                        }))
                    }),
                )
            })
        };
        if let Err(e) = sent {
            once.call(Err(e));
        }
    }

    /// Typed completion administration on this authorized Host generation. No request
    /// here can register a counterpart, confirm its GUI, or sign arbitrary bytes.
    pub fn companion_pending(
        &self,
        request: xshell_protocol::ring::companion::CompanionAdminRequest,
        w: Waiter,
    ) {
        self.gated(
            ClientMsg::RingCompanion { request },
            &[RING_CAPABILITY],
            RING_REFUSED,
            Some(self.link_generation()),
            w,
        );
    }

    /// `ring.identity`: the Host's Ring keys, name and membership, on link generation
    /// `link_gen` only ([`HostStatus::link_generation`]). Refused without sending anything
    /// when the Daemon does not advertise the `ring` capability.
    pub fn ring_identity(&self, link_gen: u64, w: Waiter) {
        self.gated(
            ClientMsg::RingIdentity,
            &[RING_CAPABILITY],
            RING_REFUSED,
            Some(link_gen),
            Box::new({
                let sh = self.sh.clone();
                move |reply| {
                    // Identity contradictions are routing evidence, not enrollment results.
                    // Record before a caller can reject a foreign Ring or a refused join.
                    if let Ok(value) = &reply {
                        if let Ok(identity) = crate::ring::LocalIdentity::from_json(value) {
                            sh.observe_identity(link_gen, identity.sign_key);
                        }
                    }
                    w(reply);
                }
            }),
        );
    }

    /// `ring.join` with the whole Roster chain, on link generation `link_gen` only. Refused
    /// like [`HostHandle::ring_identity`]; with `expect` (a conditional join) also when the
    /// Daemon does not advertise `ring.cjoin`, since an older one would ignore the condition.
    pub fn ring_join(
        &self,
        rosters: Vec<String>,
        expect: Option<JoinExpect>,
        link_gen: u64,
        w: Waiter,
    ) {
        if let Some(error) = self.configured_lifecycle_refusal() {
            w(Err(error));
            return;
        }
        if self.sh.relay.is_some() {
            w(Err(HostError::invalid(
                "discovered Relay Hosts cannot be automatically enrolled",
            )));
            return;
        }

        let caps: &[&str] = if expect.is_some() {
            &[RING_CAPABILITY, RING_CJOIN_CAPABILITY]
        } else {
            &[RING_CAPABILITY]
        };
        self.gated(
            ClientMsg::RingJoin { rosters, expect },
            caps,
            RING_REFUSED,
            Some(link_gen),
            w,
        );
    }

    /// "Upgrade now" and "Replace with this Desktop's build" (ADR-0003, #49). First, while
    /// the old Daemon still runs, this Desktop's build is installed (its identity checked)
    /// and recorded as the Host's `selected` build, so a refusal touches no Terminal. Then:
    /// connected, `daemon.upgrade`, and the supervisor waits for the old Daemon to close the
    /// link before reconnecting; incompatible, SIGTERM the Daemon through its pidfile (a
    /// direct Host: its dialer's own stop), then reconnect. The supervisor's check after the
    /// switch decides whether it worked.
    pub fn upgrade(&self, w: Waiter) {
        self.upgrade_target(None, w);
    }

    pub fn upgrade_target(&self, target: Option<UpdateTarget>, w: Waiter) {
        self.upgrade_inner(target, false, false, w);
    }

    /// Explicit local mode switches may restart the same exact build; not a remote update.
    pub(crate) fn restart_for_mode(&self, w: Waiter) {
        self.upgrade_inner(None, true, false, w);
    }

    pub(crate) fn access_upgrade_target(&self, target: Option<UpdateTarget>, w: Waiter) {
        self.upgrade_inner(target, false, true, w);
    }
    fn upgrade_inner(
        &self,
        target: Option<UpdateTarget>,
        mode_switch: bool,
        public: bool,
        w: Waiter,
    ) {
        if let Some(error) = self.configured_lifecycle_refusal() {
            w(Err(error));
            return;
        }
        let once = Once::new(w);
        let reservation = (|| {
            let mut st = self.sh.lock();
            if st.status.route_update_unavailable.is_some() {
                return Err(HostError::invalid("Relay-only access cannot update or stop the Daemon; use its owning Desktop or an independent SSH route"));
            }
            if st.upgrading {
                return Err(HostError::new(
                    crate::HostErrorCode::Busy,
                    "a Host update is already in progress",
                ));
            }
            let mut target = target;
            if public {
                if let Some(t) = &mut target {
                    let raw = self
                        .sh
                        .mc
                        .action_targets
                        .direct(
                            &self.sh.id,
                            InputTarget {
                                config_generation: t.config_generation,
                                link_generation: t.link_generation,
                            },
                            InputTarget {
                                config_generation: st.status.config_generation,
                                link_generation: st.status.link_generation,
                            },
                        )
                        .map_err(|_| {
                            HostError::invalid("the Host update target changed; review it again")
                        })?;
                    t.config_generation = raw.config_generation;
                    t.link_generation = raw.link_generation;
                }
            }
            if let Some(t) = &target {
                if t.config_generation != st.status.config_generation
                    || t.link_generation != st.status.link_generation
                    || t.daemon_version != st.status.daemon_version
                    || t.daemon_build != st.status.daemon_build
                {
                    return Err(HostError::new(
                        crate::HostErrorCode::Invalid,
                        "the Host update target changed; review it again",
                    ));
                }
            }
            if !(mode_switch && st.cfg.id == crate::LOCAL_HOST_ID) {
                if let Some(reason) = st.status.update_unavailable {
                    return Err(
                        if reason == crate::status::UpdateUnavailable::DesktopOwned {
                            HostError::invalid(reason.message())
                        } else {
                            HostError::offline(reason.message())
                        },
                    );
                }
            }
            if !mode_switch
                && self
                    .sh
                    .mc
                    .transports
                    .direct(st.cfg.configured().expect("configured update route"))
                    .is_some_and(|d| !d.upgradable())
            {
                return Err(HostError::new(
                    crate::HostErrorCode::Incompatible,
                    "this Daemon cannot be upgraded here",
                ));
            }
            if !(mode_switch && st.cfg.id == crate::LOCAL_HOST_ID)
                && !crate::version::update_eligible(
                    &self.sh.mc.desktop_version,
                    self.sh.mc.desktop_build.as_ref(),
                    st.status.daemon_version.as_deref().unwrap_or(""),
                    st.status.daemon_build.as_ref(),
                )
            {
                return Err(HostError::new(
                    crate::HostErrorCode::Invalid,
                    "this Host does not need this Desktop's build",
                ));
            }
            let windows = st.status.os.as_deref() == Some("windows");
            let stop = match self.usable_link(&st) {
                Ok((link, _)) => {
                    if windows
                        && !st
                            .status
                            .daemon_capabilities
                            .iter()
                            .any(|c| c == xshell_protocol::cap::DAEMON_UPGRADE_SELECTED)
                    {
                        return Err(HostError::offline("update and reopen xshell on that Windows Host first; its owning Desktop must support Daemon updates"));
                    }
                    Stop::Request(link)
                }
                Err(_)
                    if st.status.status == StatusKind::Incompatible
                        && (matches!(st.status.os.as_deref(), Some("linux" | "macos"))
                            || (cfg!(unix)
                                && self
                                    .sh
                                    .mc
                                    .transports
                                    .direct(st.cfg.configured().expect("configured update route"))
                                    .is_some_and(|d| d.upgradable()))) =>
                {
                    Stop::Script
                }
                Err(e) => return Err(e),
            };
            let cfg = st
                .cfg
                .configured()
                .expect("configured update route")
                .clone();
            let gen = st.gen;
            let op = st.next_epoch();
            st.upgrade_operation = Some(op);
            st.upgrading = true;
            st.upgrade_expect = false;
            st.status.phase = Some(Phase::Preparing);
            st.status.last_error = None;
            st.status.error_hint = None;
            self.sh.publish(&mut st);
            Ok((cfg, gen, op, windows, stop))
        })();
        let (cfg, gen, op, windows, mut stop) = match reservation {
            Ok(r) => r,
            Err(e) => return once.call(Err(e)),
        };
        let direct = self.sh.mc.transports.direct(&cfg);
        if matches!(stop, Stop::Script) {
            if let Some(d) = &direct {
                if !d.upgradable() {
                    let e = HostError::new(
                        crate::HostErrorCode::Incompatible,
                        "this Daemon cannot be upgraded here",
                    );
                    finish_upgrade(&self.sh, op, Some(&e));
                    return once.call(Err(e));
                }
                stop = Stop::Direct;
            }
        }
        let sh = self.sh.clone();
        let o = once.clone();
        let cancel = self.cancel_token();
        let spawned = std::thread::Builder::new()
            .name("host-upgrade".into())
            .spawn(move || {
                let result = (|| {
                    check_upgrade(&sh, op, gen, &cancel)?;
                    if windows {
                        let cmd = cfg.daemon_override().ok_or_else(|| {
                            HostError::offline(
                                "Windows updates require a configured Daemon command",
                            )
                        })?;
                        crate::install::stage_windows(
                            &*sh.mc.transports.for_host(&cfg),
                            &*sh.mc.binaries,
                            cmd,
                            &sh.mc.desktop_version,
                            sh.mc.desktop_build.as_ref(),
                            &cancel,
                        )
                        .map_err(|e| HostError::offline(e.message))?;
                    } else {
                        prepare_switch(&sh, &cfg, direct.as_deref(), &cancel, op, gen)?;
                    }
                    check_upgrade(&sh, op, gen, &cancel)?;
                    match stop {
                        Stop::Request(link) => {
                            request_upgrade(&sh, &link, op, gen, windows, o.clone());
                            return Ok(());
                        }
                        Stop::Script => crate::supervisor::kill_daemon(
                            &*sh.mc.transports.for_host(&cfg),
                            &cancel,
                        )?,
                        Stop::Direct => {
                            match direct.as_deref().and_then(|d| d.stop_incompatible(&cancel)) {
                                Some(r) => r?,
                                None => {
                                    return Err(HostError::offline(
                                        "this Daemon cannot be upgraded here",
                                    ))
                                }
                            }
                        }
                    }
                    accept_upgrade(&sh, op, gen, false);
                    sh.kick.store(true, Ordering::SeqCst);
                    o.call(Ok(Value::Null));
                    Ok(())
                })();
                if let Err(e) = result {
                    finish_upgrade(&sh, op, Some(&e));
                    o.call(Err(e));
                } else {
                    wait_upgrade_completion(&sh, op, &cancel);
                }
            });
        if spawned.is_err() {
            let e = HostError::offline("cannot start the upgrade");
            finish_upgrade(&self.sh, op, Some(&e));
            once.call(Err(e));
        }
    }

    /// The generation of the newest link: it grows with each connection attempt.
    pub fn link_generation(&self) -> u64 {
        self.sh.lock().gen
    }

    /// Retry now instead of waiting out the backoff. Never waits on `State`.
    pub fn kick(&self) {
        self.sh.kick.store(true, Ordering::SeqCst);
        if let Ok(_st) = self.sh.st.try_lock() {
            self.sh.cv.notify_all();
        }
    }
}

/// How [`HostHandle::upgrade`] ends the old Daemon once the new build is selected.
enum Stop {
    /// `daemon.upgrade` over the usable link.
    Request(Arc<Link>),
    /// The pidfile SIGTERM script over the Host's transport.
    Script,
    /// The direct dialer's own stop.
    Direct,
}

/// Install this Desktop's build where the Daemon runs, check it, and record it as the Host's
/// `selected` build (#49, C1). A Daemon command Host too (#55): its connect command prefers
/// the selected build. A direct dialer that cannot select (a GUI-bound Local Daemon)
/// selects nothing.
fn prepare_switch(
    sh: &Shared,
    cfg: &HostConfig,
    direct: Option<&dyn crate::dial::Dialer>,
    cancel: &CancelToken,
    op: u64,
    gen: u64,
) -> Result<(), HostError> {
    let mc = &sh.mc;
    let dir = mc.install_dir();
    if let Some(d) = direct {
        return match d.select_build(&dir) {
            None | Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(HostError::offline(e)),
        };
    }
    let t = mc.transports.for_host(cfg);
    let failed = |e: crate::install::InstallError| {
        if e.cancelled {
            HostError::offline("the upgrade was cancelled")
        } else {
            HostError::offline(e.message)
        }
    };
    crate::install::ensure_installed(
        &*t,
        &*mc.binaries,
        &mc.desktop_version,
        mc.desktop_build.as_ref(),
        mc.ours,
        cancel,
        &|| {},
    )
    .map_err(failed)?;
    check_upgrade(sh, op, gen, cancel)?;
    crate::install::select_remote(&*t, &dir, cancel).map_err(failed)
}

fn check_upgrade(sh: &Shared, op: u64, gen: u64, cancel: &CancelToken) -> Result<(), HostError> {
    let st = sh.lock();
    if cancel.is_cancelled() || st.upgrade_operation != Some(op) || st.gen != gen {
        Err(HostError::offline(
            "the Host update was cancelled or its connection changed",
        ))
    } else {
        Ok(())
    }
}

fn finish_upgrade(sh: &Shared, op: u64, error: Option<&HostError>) {
    let mut st = sh.lock();
    if st.upgrade_operation != Some(op) {
        return;
    }
    st.upgrade_operation = None;
    st.upgrading = false;
    st.upgrade_expect = false;
    st.upgrade_deadline = None;
    st.upgrade_complete_deadline = None;
    st.status.phase = None;
    if let Some(e) = error {
        st.status.last_error = Some(e.message.clone());
        st.status.error_hint = Some(crate::HostErrorHint::UpdateFailed);
    }
    sh.publish(&mut st);
}

/// The preparation worker stays alive until this operation finishes. Its deadline is
/// independent of an SSH probe/dial that may currently block the supervisor.
fn wait_upgrade_completion(sh: &Shared, op: u64, cancel: &CancelToken) {
    let mut st = sh.lock();
    loop {
        if st.upgrade_operation != Some(op) {
            return;
        }
        if cancel.is_cancelled() {
            drop(st);
            finish_upgrade(
                sh,
                op,
                Some(&HostError::offline("the Host update was cancelled")),
            );
            return;
        }
        expire_upgrade(sh, &mut st);
        if st.upgrade_operation != Some(op) {
            return;
        }
        st = sh.cv.wait_timeout(st, Duration::from_millis(50)).unwrap().0;
    }
}

pub(crate) fn expire_upgrade(sh: &Shared, st: &mut State) {
    if st
        .upgrade_complete_deadline
        .is_some_and(|d| d <= Instant::now())
    {
        st.upgrade_operation = None;
        st.upgrading = false;
        st.upgrade_expect = false;
        st.upgrade_deadline = None;
        st.upgrade_complete_deadline = None;
        st.status.phase = None;
        st.status.last_error = Some("This Host did not reconnect with this Desktop’s build. Check that xshell is still open there, then reconnect.".into());
        st.status.error_hint = Some(crate::HostErrorHint::UpdateFailed);
        sh.publish(st);
    }
}

pub(crate) fn accept_upgrade(sh: &Shared, op: u64, original_gen: u64, rpc: bool) {
    let mut st = sh.lock();
    if st.upgrade_operation != Some(op) {
        return;
    }
    // A successful blocking stop can return after the supervisor has already adopted
    // its successor. Only a newer usable exact connection can complete this operation;
    // the original connection (notably a same-build mode switch) never counts.
    if !rpc
        && st.gen > original_gen
        && st.status.link_generation == st.gen
        && !st.link_closed
        && st.link().is_some()
        && st.status.usable()
        && st.status.daemon_version.as_deref() == Some(sh.mc.desktop_version.as_str())
        && sh
            .mc
            .desktop_build
            .as_ref()
            .is_none_or(|build| crate::version::same_build(st.status.daemon_build.as_ref(), build))
    {
        st.upgrade_operation = None;
        st.upgrading = false;
        st.upgrade_expect = false;
        st.upgrade_deadline = None;
        st.upgrade_complete_deadline = None;
        st.status.phase = None;
        st.status.last_error = None;
        st.status.error_hint = None;
        sh.publish(&mut st);
        sh.cv.notify_all();
        return;
    }
    st.upgrade_expect = true;
    st.status.phase = Some(Phase::Upgrading);
    sh.publish(&mut st);
    st.upgrade_complete_deadline = Some(Instant::now() + Duration::from_secs(120));
    if rpc {
        st.upgrade_deadline = Some(Instant::now() + sh.mc.upgrade_close_timeout);
    }
    sh.cv.notify_all();
}

fn request_upgrade(
    sh: &Arc<Shared>,
    link: &Arc<Link>,
    op: u64,
    original_gen: u64,
    windows: bool,
    once: Arc<Once<Reply>>,
) {
    let msg = if windows {
        let Some(build) = sh.mc.desktop_build.clone() else {
            let e = HostError::offline("this Desktop's exact build is unknown");
            finish_upgrade(sh, op, Some(&e));
            return once.call(Err(e));
        };
        ClientMsg::DaemonUpgradeSelected {
            version: sh.mc.desktop_version.clone(),
            build,
        }
    } else {
        ClientMsg::DaemonUpgrade
    };
    let weak = Arc::downgrade(sh);
    let o = once.clone();
    let w: Waiter = Box::new(move |r| {
        if let Some(sh) = weak.upgrade() {
            match &r {
                Ok(_) => accept_upgrade(&sh, op, original_gen, true),
                Err(e) => {
                    if e.code == crate::HostErrorCode::Remote
                        && e.message == xshell_protocol::DESKTOP_OWNED_UPGRADE_REFUSED
                    {
                        let mut st = sh.lock();
                        if st.gen == original_gen && st.upgrade_operation == Some(op) {
                            st.status.update_unavailable =
                                Some(crate::status::UpdateUnavailable::DesktopOwned);
                        }
                    }
                    finish_upgrade(&sh, op, Some(e));
                }
            }
        }
        o.call(r);
    });
    if let Err(e) = link.request(msg, sh.mc.term_timeout, w) {
        finish_upgrade(sh, op, Some(&e));
        once.call(Err(e));
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
            completion_attention: None,
            terminal: t,
            spec: LaunchSpec::default(),
            meta: Default::default(),
            created_at_ms: 1,
            pid: Some(2),
            exit_code: None,
            agent_status: None,
            status_at_ms: None,
            last_line: None,
            permission_prompt: None,
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
    /// [`connect`] to a Daemon that serves the file-transfer methods.
    fn connect_files(sh: &Arc<Shared>) -> (Peer, Arc<Link>, u64) {
        let (p, l, g, _) = connect_full(
            sh,
            vec![],
            crate::link::LinkLimits::default(),
            &[xshell_protocol::cap::FILE_TRANSFER],
        );
        (p, l, g)
    }

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
            build: None,
        }));
        b.extend(terminals_frame(list));
        peer.write(&b);
        let gen = sh.begin_link(None);
        let (link, _, _) = Link::establish_with(
            io,
            ProtocolRange { min: 1, max: 1 },
            "1.5.0",
            None,
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
            identity_access: None,
            generation_access: None,
            run: Mutex::new(None),
            retired: AtomicBool::new(false),
        }
    }

    #[test]
    fn identity_observations_are_link_bound_and_stale_results_cannot_restore_aliases() {
        let sh = shared();
        let (_old_peer, _old_link, old_gen) = connect(&sh, vec![]);
        let original = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let changed = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let h = handle(&sh);
        assert!(sh.observe_identity(old_gen, original));
        let (_new_peer, _new_link, current_gen) = connect(&sh, vec![]);
        assert!(sh.observe_identity(current_gen, changed));
        assert!(!sh.observe_identity(old_gen, original));
        assert!(
            !h.allows_ring_identity(&original),
            "stale original proof restored a contradictory alias"
        );
        assert!(sh.observe_identity(current_gen, original));
        assert!(!sh.observe_identity(old_gen, changed));
        assert!(
            h.allows_ring_identity(&original),
            "stale contradiction invalidated current proof"
        );
        h.reset(crate::config::test_host("h_ab12cd34", "new-target"), 2);
        let (_retarget_peer, _retarget_link, retarget_gen) = connect(&sh, vec![]);
        assert!(sh.observe_identity(retarget_gen, original));
        assert!(!sh.observe_identity(current_gen, changed));
        assert!(
            h.allows_ring_identity(&original),
            "pre-retarget proof changed the new target's association"
        );
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

    fn assert_identity_access_refused(access: &HostHandle, terminal: Uuid) {
        let (waiter, receiver) = slot();
        access.call("list_dir".into(), json!({"path":"/"}), waiter);
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        let (waiter, receiver) = res_slot();
        access.term_open(
            OpenSpec {
                terminal,
                launch: LaunchSpec::default(),
                cols: 80,
                rows: 24,
                meta: Default::default(),
                first_message: None,
                adopt_existing: false,
            },
            Arc::new(VecSink::default()),
            waiter,
        );
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        let (waiter, receiver) = res_slot();
        access.term_attach(terminal, Arc::new(VecSink::default()), waiter);
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        let (waiter, receiver) = res_slot();
        access.term_relaunch(terminal, true, waiter);
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        let (waiter, receiver) = slot();
        access.term_close(terminal, waiter);
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        let (waiter, receiver) = slot();
        access.term_update(terminal, Some("session".into()), None, waiter);
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        assert_eq!(
            access
                .term_input(terminal, "must not send".into(), None, None)
                .unwrap_err()
                .code,
            HostErrorCode::Offline
        );
        assert_eq!(
            access.term_resize(terminal, 1, 1).unwrap_err().code,
            HostErrorCode::Offline
        );
        assert_eq!(
            access.term_detach(terminal).unwrap_err().code,
            HostErrorCode::Offline
        );
    }

    #[test]
    fn identity_bound_access_admits_only_current_proven_links_and_keeps_direct_access() {
        let sh = shared();
        let terminal = Uuid::new_v4();
        let (mut first, _first_link, first_gen) = connect(&sh, vec![info(terminal)]);
        let owner = Arc::new(handle(&sh));
        let original = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let changed = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let access = HostHandle::for_identity(owner.clone(), original);
        assert!(Arc::ptr_eq(&access.sh, &owner.sh));
        assert!(Arc::ptr_eq(
            &access.identity_access.as_ref().unwrap().owner,
            &owner
        ));
        assert!(
            access.run.lock().unwrap().is_none(),
            "an alias view owns no second supervisor"
        );
        assert_identity_access_refused(&access, terminal);
        assert!(!access.status().usable());
        owner
            .term_input(terminal, "direct unknown".into(), None, None)
            .unwrap();
        assert_eq!(
            first.read_json()["data"],
            "direct unknown",
            "refused alias work escaped to the peer"
        );
        assert!(sh.observe_identity(first_gen, original));
        let (waiter, receiver) = slot();
        access.call("get_home_dir".into(), json!({}), waiter);
        let request = first.read_json();
        assert_eq!(request["t"], "call");
        first.reply(request["id"].as_u64().unwrap(), Ok(json!("original home")));
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap(),
            json!("original home")
        );
        {
            let _blocked_delivery = sh.lock();
            let bounded_view = HostHandle::for_identity(owner.clone(), original);
            bounded_view
                .term_input(terminal, "identity proven".into(), None, None)
                .unwrap();
            bounded_view.term_resize(terminal, 100, 40).unwrap();
        }
        assert_eq!(first.read_json()["data"], "identity proven");
        assert_eq!(first.read_json()["cols"], 100);
        let (mut replacement, _replacement_link, replacement_gen) =
            connect(&sh, vec![info(terminal)]);
        assert!(
            !sh.observe_identity(first_gen, original),
            "old positive proof was accepted"
        );
        assert_identity_access_refused(&access, terminal);
        assert_eq!(
            sh.sizes.lock().unwrap().get(&terminal),
            Some(&(100, 40)),
            "refused Resize changed later replay state"
        );
        assert!(sh.observe_identity(replacement_gen, changed));
        assert_identity_access_refused(&access, terminal);
        owner
            .term_input(terminal, "independent changed identity".into(), None, None)
            .unwrap();
        assert_eq!(
            replacement.read_json()["data"],
            "independent changed identity",
            "alias request/notification reached replacement"
        );
        assert!(owner.status().usable());
        assert!(!access.status().usable());
        owner.reset(crate::config::test_host("h_ab12cd34", "retargeted"), 2);
        let (mut retargeted, _retargeted_link, retargeted_gen) = connect(&sh, vec![info(terminal)]);
        assert!(sh.observe_identity(retargeted_gen, original));
        assert_identity_access_refused(&access, terminal);
        owner
            .term_input(terminal, "independent retarget".into(), None, None)
            .unwrap();
        assert_eq!(retargeted.read_json()["data"], "independent retarget");
    }

    #[test]
    fn identity_bound_attachments_wait_for_current_proof_without_replaying_to_another_daemon() {
        let sh = shared();
        let terminal = Uuid::new_v4();
        let (mut first, _first_link, first_gen) = connect(&sh, vec![info(terminal)]);
        let owner = Arc::new(handle(&sh));
        let original = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let changed = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        assert!(sh.observe_identity(first_gen, original));
        let access = HostHandle::for_identity(owner.clone(), original);
        let sink = Arc::new(VecSink::default());
        let (waiter, receiver) = res_slot();
        access.term_attach(terminal, sink.clone(), waiter);
        let request = first.read_json();
        assert_eq!(request["t"], "term.attach");
        first.reply(
            request["id"].as_u64().unwrap(),
            Ok(json!({"exitCode":null})),
        );
        receiver.recv_timeout(T5).unwrap().unwrap();
        access.term_resize(terminal, 100, 40).unwrap();
        assert_eq!(first.read_json()["t"], "term.resize");
        let (mut replacement, _replacement_link, replacement_gen) =
            connect(&sh, vec![info(terminal)]);
        replacement.write(&out_frame(terminal, b"UNPROVEN OUTPUT"));
        assert!(sh.observe_identity(replacement_gen, changed));
        replacement.write(&out_frame(terminal, b"FOREIGN OUTPUT"));
        let (waiter, receiver) = slot();
        owner.call("get_home_dir".into(), json!({}), waiter);
        let request = replacement.read_json();
        assert_eq!(
            request["t"], "call",
            "automatic Attach/Resize escaped before proof or to K2"
        );
        replacement.reply(
            request["id"].as_u64().unwrap(),
            Ok(json!("replacement home")),
        );
        receiver.recv_timeout(T5).unwrap().unwrap();
        assert_eq!(
            sink.text(),
            "",
            "a retained identity-bound sink received another Daemon's output"
        );
        let (mut returning, _returning_link, returning_gen) = connect(&sh, vec![info(terminal)]);
        owner
            .term_input(terminal, "unknown again".into(), None, None)
            .unwrap();
        assert_eq!(returning.read_json()["t"], "term.input");
        assert!(sh.observe_identity(returning_gen, original));
        let attach = returning.read_json();
        assert_eq!(attach["t"], "term.attach");
        assert_eq!(returning.read_json()["cols"], 100);
        returning.reply(attach["id"].as_u64().unwrap(), Ok(json!({"exitCode":null})));
        returning.write(&out_frame(terminal, b"ORIGINAL OUTPUT"));
        sink.wait_text("ORIGINAL OUTPUT");
    }

    #[test]
    fn identity_proof_under_backpressure_requests_bounded_attachment_retry() {
        let sh = shared();
        let terminals = [Uuid::new_v4(), Uuid::new_v4()];
        let (mut first, _first_link, first_gen) =
            connect(&sh, terminals.iter().copied().map(info).collect());
        let owner = Arc::new(handle(&sh));
        let key = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        assert!(sh.observe_identity(first_gen, key));
        let access = HostHandle::for_identity(owner.clone(), key);
        for terminal in terminals {
            let (waiter, receiver) = res_slot();
            access.term_attach(terminal, Arc::new(VecSink::default()), waiter);
            let request = first.read_json();
            first.reply(
                request["id"].as_u64().unwrap(),
                Ok(json!({"exitCode":null})),
            );
            receiver.recv_timeout(T5).unwrap().unwrap();
        }
        let (mut returning, _returning_link, returning_gen, left) = connect_with(
            &sh,
            terminals.iter().copied().map(info).collect(),
            crate::link::LinkLimits {
                max_pending: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            left, 0,
            "unknown identity does not start an attachment retry/reconnect loop"
        );
        assert!(sh.observe_identity(returning_gen, key));
        assert!(
            sh.kick.load(Ordering::SeqCst),
            "proof-time backpressure did not wake bounded retries"
        );
        let request = returning.read_json();
        assert_eq!(request["t"], "term.attach");
        returning.reply(
            request["id"].as_u64().unwrap(),
            Ok(json!({"exitCode":null})),
        );
        let until = Instant::now() + T5;
        loop {
            let mut state = sh.lock();
            let link = state.link().unwrap().clone();
            if sh.reattach_locked(&mut state, returning_gen, &link) == 0 {
                break;
            }
            drop(state);
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(returning.read_json()["t"], "term.attach");
    }

    #[test]
    fn identity_bound_alias_lifecycle_limitation_is_typed_and_has_no_transport_effects() {
        use std::sync::atomic::AtomicUsize;
        struct NoLifecycle(Arc<AtomicUsize>);
        impl crate::TransportFactory for NoLifecycle {
            fn for_host(&self, _: &HostConfig) -> Box<dyn crate::Transport> {
                panic!("alias must not probe/stage/select/stop via SSH")
            }
            fn direct(&self, _: &HostConfig) -> Option<Box<dyn crate::Dialer>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                struct NotUpgradable;
                impl crate::Dialer for NotUpgradable {
                    fn dial(&self, _: &CancelToken) -> Result<crate::Dialed, crate::DialError> {
                        panic!("no extra supervisor")
                    }
                    fn describe(&self) -> String {
                        "independent configured control".into()
                    }
                    fn select_build(&self, _: &str) -> Option<Result<(), String>> {
                        panic!("alias must not select")
                    }
                    fn stop_incompatible(&self, _: &CancelToken) -> Option<Result<(), HostError>> {
                        panic!("alias must not stop")
                    }
                }
                Some(Box::new(NotUpgradable))
            }
        }
        impl crate::BinarySource for NoLifecycle {
            fn fetch(&self, _: &crate::Wanted, _: &CancelToken) -> Result<crate::Fetched, String> {
                panic!("alias must not fetch or stage a Daemon");
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let sh = shared_with(|config| {
            config.transports = Arc::new(NoLifecycle(calls.clone()));
            config.binaries = Arc::new(NoLifecycle(calls.clone()));
        });
        let (_peer, _link, gen) = connect(&sh, vec![]);
        let owner = Arc::new(handle(&sh));
        let key = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        assert!(sh.observe_identity(gen, key));
        {
            let mut state = sh.lock();
            state.status.daemon_version = Some("1.4.0".into());
            state.status.update_unavailable = Some(crate::status::UpdateUnavailable::DesktopOwned);
            sh.publish(&mut state);
        }
        let access = HostHandle::for_identity(owner.clone(), key);
        let status = access.status();
        assert_eq!(
            status.route_update_unavailable,
            Some(crate::status::RouteUpdateUnavailable::IdentityBoundAlias)
        );
        assert_eq!(
            serde_json::to_value(status.route_update_unavailable).unwrap(),
            json!("identity-bound-alias")
        );
        assert_eq!(
            status.update_unavailable,
            owner.status().update_unavailable,
            "ownership diagnosis is independent"
        );
        assert_eq!(status.update_action, None);
        for reason in [
            crate::status::UpdateUnavailable::DesktopOwned,
            crate::status::UpdateUnavailable::WindowsOwner,
        ] {
            let mut state = sh.lock();
            state.status.update_unavailable = Some(reason);
            sh.publish(&mut state);
            drop(state);
            assert_eq!(access.status().update_unavailable, Some(reason));
            assert_eq!(
                access.status().route_update_unavailable,
                Some(crate::status::RouteUpdateUnavailable::IdentityBoundAlias)
            );
        }
        assert_eq!(owner.status().route_update_unavailable, None);
        assert!(owner.status().update_action.is_some());
        for mode in [false, true] {
            let (waiter, receiver) = slot();
            if mode {
                access.restart_for_mode(waiter);
            } else {
                access.upgrade(waiter);
            }
            assert_eq!(
                receiver.recv_timeout(T5).unwrap().unwrap_err().code,
                HostErrorCode::Invalid
            );
        }
        for method in ["daemon.upgrade", "daemon.upgrade-selected", "ring.join"] {
            let (waiter, receiver) = slot();
            access.call(method.into(), json!({}), waiter);
            assert_eq!(
                receiver.recv_timeout(T5).unwrap().unwrap_err().code,
                HostErrorCode::Invalid
            );
        }
        let (waiter, receiver) = slot();
        access.ring_join(vec![], None, gen, waiter);
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Invalid
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        {
            let mut state = sh.lock();
            state.status.update_unavailable = None;
            sh.publish(&mut state);
        }
        let (waiter, receiver) = slot();
        owner.upgrade(waiter);
        assert_eq!(
            receiver.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Incompatible
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "independent configured lifecycle policy was globally disabled"
        );
    }

    #[test]
    fn completion_attention_input_captures_token_and_gates_on_exact_link() {
        let sh = shared();
        let t = Uuid::new_v4();
        let f = Uuid::new_v4();
        let mut p = connect_capable(
            &sh,
            vec![info(t)],
            &[xshell_protocol::cap::TERM_COMPLETION_ATTENTION],
        );
        let h = handle(&sh);
        let original = sh.ep.lock().unwrap().input_target;
        // A newer cached completion never replaces the caller's observation.
        sh.lock().terminals.as_mut().unwrap()[0].completion_attention =
            Some(xshell_protocol::msg::CompletionAttention {
                completion_id: Uuid::new_v4(),
                input_handled: false,
            });
        h.term_input(t, "ö".into(), Some(f), Some(original))
            .unwrap();
        let raw = p.expect("term.input");
        assert_eq!(raw["handledCompletionId"], f.to_string());
        assert_eq!(raw["data"], "ö");
        let mut old = connect_capable(&sh, vec![info(t)], &[]);
        assert!(h
            .term_input(t, "old link".into(), Some(f), Some(original))
            .is_err());
        let current = sh.ep.lock().unwrap().input_target;
        assert!(h
            .term_input(
                t,
                "old config".into(),
                Some(f),
                Some(InputTarget {
                    config_generation: current.config_generation + 1,
                    ..current
                })
            )
            .is_err());
        h.term_input(t, "x".into(), Some(f), None).unwrap();
        assert!(old
            .expect("term.input")
            .get("handledCompletionId")
            .is_none());
        sh.set_link(&mut sh.lock(), None);
        assert!(h
            .term_input(t, "not replayed".into(), Some(f), None)
            .is_err());
        let mut back = connect_capable(
            &sh,
            vec![info(t)],
            &[xshell_protocol::cap::TERM_COMPLETION_ATTENTION],
        );
        h.term_input(t, "next action".into(), Some(f), None)
            .unwrap();
        assert_eq!(back.expect("term.input")["data"], "next action");
    }

    #[test]
    fn completion_attention_input_refuses_predecessor_during_successor_adoption() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (_old_peer, _old_link, _) = connect(&sh, vec![info(t)]);
        let h = handle(&sh);
        let predecessor = sh.ep.lock().unwrap().input_target;
        let gen = sh.begin_link(None);
        let (io, mut successor) = pair();
        let mut greeting = hello_frame(1, 1, "1.5.0");
        greeting.extend(terminals_frame(vec![info(t)]));
        successor.write(&greeting);
        let (link, _, _) = Link::establish(
            io,
            ProtocolRange { min: 1, max: 1 },
            "1.5.0",
            sh.events(gen),
            T5,
        )
        .unwrap();
        assert!(sh.wait_first_list(gen, T5));
        successor.expect("hello");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let adopting = sh.clone();
        let adoption = std::thread::spawn(move || {
            adopting.adopt(gen, &link, |st| {
                // The successor endpoint is installed, State is held, and status has not yet
                // been published. Input intentionally uses the separate nonblocking endpoint.
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(T5).unwrap();
                st.status.set_kind(StatusKind::Connected);
            })
        });
        entered_rx.recv_timeout(T5).unwrap();
        assert!(sh.st.try_lock().is_err());
        assert_eq!(h.status().link_generation, predecessor.link_generation);
        let during = h.term_input(t, "obsolete action".into(), None, Some(predecessor));
        release_tx.send(()).unwrap();
        assert_eq!(adoption.join().unwrap(), Some(0));
        assert_eq!(during.unwrap_err().code, HostErrorCode::Offline);
        let current = InputTarget {
            config_generation: h.status().config_generation,
            link_generation: gen,
        };
        h.term_input(t, "current action".into(), None, Some(current))
            .unwrap();
        // No predecessor bytes may have reached the successor ahead of this action.
        let next = successor.read_json();
        assert_eq!(next["t"], "term.input");
        assert_eq!(next["data"], "current action");
    }

    #[test]
    fn update_reservation_stale_target_and_late_callback_guards() {
        let sh = shared();
        let _peer = connect(&sh, vec![]);
        let h = handle(&sh);
        {
            let mut st = sh.lock();
            st.status.daemon_version = Some("1.4.0".into());
            st.upgrading = true;
            st.upgrade_operation = Some(11);
        }
        let (w, rx) = res_slot();
        h.upgrade(w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            crate::HostErrorCode::Busy
        );
        let old = HostError::offline("late failure");
        finish_upgrade(&sh, 10, Some(&old));
        assert_eq!(sh.lock().upgrade_operation, Some(11));
        accept_upgrade(&sh, 10, 0, true);
        assert!(sh.lock().upgrade_complete_deadline.is_none());
        finish_upgrade(&sh, 11, None);
        let (w, rx) = res_slot();
        h.upgrade_target(
            Some(UpdateTarget {
                config_generation: 0,
                link_generation: 0,
                daemon_version: Some("1.4.0".into()),
                daemon_build: None,
            }),
            w,
        );
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            crate::HostErrorCode::Invalid
        );
        assert!(!sh.lock().upgrading);
    }

    #[test]
    fn completion_deadline_runs_without_a_supervisor_and_eligibility_is_rechecked() {
        let sh = shared();
        let _peer = connect(&sh, vec![]);
        let h = handle(&sh);
        for version in ["1.6.0", "dev", "1.5.0"] {
            sh.lock().status.daemon_version = Some(version.into());
            let (w, rx) = res_slot();
            h.upgrade(w);
            assert_eq!(
                rx.recv_timeout(T5).unwrap().unwrap_err().code,
                crate::HostErrorCode::Invalid
            );
            assert!(!sh.lock().upgrading);
        }
        {
            let mut st = sh.lock();
            st.status.daemon_version = Some("1.4.0".into());
            st.status.os = Some("windows".into());
        }
        let (w, rx) = res_slot();
        h.upgrade(w);
        assert!(rx
            .recv_timeout(T5)
            .unwrap()
            .unwrap_err()
            .message
            .contains("owning Desktop"));
        assert!(!sh.lock().upgrading);
        {
            let mut st = sh.lock();
            st.upgrade_operation = Some(3);
            st.upgrading = true;
            st.upgrade_complete_deadline = Some(Instant::now() + Duration::from_millis(10));
        }
        wait_upgrade_completion(&sh, 3, &CancelToken::new());
        let st = sh.lock();
        assert!(!st.upgrading);
        assert_eq!(
            st.status.error_hint,
            Some(crate::HostErrorHint::UpdateFailed)
        );
    }

    #[test]
    fn update_acknowledgement_is_not_completion() {
        let sh = shared();
        let (mut peer, link, _) = connect(&sh, vec![]);
        {
            let mut st = sh.lock();
            st.upgrading = true;
            st.upgrade_operation = Some(7);
        }
        let (w, rx) = res_slot();
        request_upgrade(&sh, &link, 7, 0, false, Once::new(w));
        let id = peer.expect("daemon.upgrade")["id"].as_u64().unwrap();
        peer.write(&res_frame(id, Ok(Value::Null)));
        rx.recv_timeout(T5).unwrap().unwrap();
        let st = sh.lock();
        assert!(st.upgrading && st.upgrade_expect);
        assert_eq!(st.status.phase, Some(Phase::Upgrading));
        assert!(st.upgrade_complete_deadline.unwrap() > Instant::now() + Duration::from_secs(119));
    }

    #[test]
    fn legacy_ownership_refusal_is_exact_and_preserves_the_remote_error() {
        for message in [
            xshell_protocol::DESKTOP_OWNED_UPGRADE_REFUSED.to_string(),
            format!(
                "{} (different error)",
                xshell_protocol::DESKTOP_OWNED_UPGRADE_REFUSED
            ),
            "cannot save the Terminal list".into(),
        ] {
            let sh = shared();
            let (mut peer, link, generation) = connect(&sh, vec![]);
            {
                let mut st = sh.lock();
                st.upgrading = true;
                st.upgrade_operation = Some(7);
            }
            let (waiter, reply) = res_slot();
            request_upgrade(&sh, &link, 7, generation, false, Once::new(waiter));
            let id = peer.expect("daemon.upgrade")["id"].as_u64().unwrap();
            peer.write(&res_frame(id, Err(message.clone())));
            let error = reply.recv_timeout(T5).unwrap().unwrap_err();
            assert_eq!(error, HostError::remote(message.clone()));
            let st = sh.lock();
            assert_eq!(st.status.last_error.as_deref(), Some(message.as_str()));
            assert_eq!(
                st.status.error_hint,
                Some(crate::HostErrorHint::UpdateFailed)
            );
            assert_eq!(
                st.status.update_unavailable,
                (message == xshell_protocol::DESKTOP_OWNED_UPGRADE_REFUSED)
                    .then_some(crate::status::UpdateUnavailable::DesktopOwned)
            );
            assert!(!st.upgrading);
        }
    }

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
        h.term_input(t, "probe".into(), None, None).unwrap();
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
    fn upload_commit_timeout_is_outcome_unknown() {
        let sh = shared_with(|mc| mc.long_call_timeout = Duration::from_millis(100));
        let (mut peer, _link, _) = connect_files(&sh);
        let h = handle(&sh);
        let (w, rx) = slot();
        h.call("upload_commit".into(), json!({"uploadId": "u"}), w);
        let id = peer.expect("call")["id"].as_u64().unwrap();
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Indeterminate);
        assert!(e.message.starts_with(UPLOAD_OUTCOME_UNKNOWN), "{e:?}");
        // The commit's late answer reaches nobody; the link still serves calls.
        peer.reply(
            id,
            Ok(json!({"path": "/p", "size": 1, "version": "sha256:00"})),
        );
        let (w, rx) = slot();
        h.call("file_stat".into(), json!({"path": "/p"}), w);
        let id = peer.expect("call")["id"].as_u64().unwrap();
        peer.reply(id, Ok(json!({"size": 1})));
        assert_eq!(rx.recv_timeout(T5).unwrap(), Ok(json!({"size": 1})));
        // Another call's timeout stays a timeout.
        let sh = shared_with(|mc| mc.call_timeout = Duration::from_millis(100));
        let (mut peer, _link, _) = connect_files(&sh);
        let h = handle(&sh);
        let (w, rx) = slot();
        h.call("upload_chunk".into(), json!({}), w);
        peer.expect("call");
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Timeout);
    }

    #[test]
    fn upload_commit_lost_connection_is_outcome_unknown() {
        // Never sent (no link): the ordinary error.
        let sh = shared();
        let h = handle(&sh);
        let (w, rx) = slot();
        h.call("upload_commit".into(), json!({"uploadId": "u"}), w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        // Sent, the Daemon published, then the connection dropped before the answer.
        let sh = shared();
        let (mut peer, _link, _) = connect_files(&sh);
        let h = handle(&sh);
        let (w, rx) = slot();
        h.call("upload_commit".into(), json!({"uploadId": "u"}), w);
        peer.expect("call");
        drop(peer);
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Indeterminate);
        assert!(e.message.starts_with(UPLOAD_OUTCOME_UNKNOWN), "{e:?}");
        // Other calls keep a plain offline.
        let sh = shared();
        let (mut peer, _link, _) = connect_files(&sh);
        let h = handle(&sh);
        let (w, rx) = slot();
        h.call("upload_chunk".into(), json!({}), w);
        peer.expect("call");
        drop(peer);
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Offline);
        // The Daemon's own refusal is not ambiguous.
        let sh = shared();
        let (mut peer, _link, _) = connect_files(&sh);
        let h = handle(&sh);
        let (w, rx) = slot();
        h.call("upload_commit".into(), json!({"uploadId": "u"}), w);
        let id = peer.expect("call")["id"].as_u64().unwrap();
        peer.reply(id, Err("file exists: /p".into()));
        let e = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            (HostErrorCode::Remote, "file exists: /p")
        );
    }

    #[test]
    fn transfer_calls_to_a_daemon_without_the_capability_are_refused_unsent() {
        let sh = shared();
        let (mut peer, _link, _) = connect(&sh, vec![]);
        let h = handle(&sh);
        for m in FILE_TRANSFER_METHODS {
            let (w, rx) = slot();
            h.call(m.to_string(), json!({}), w);
            let e = rx.recv_timeout(T5).unwrap().unwrap_err();
            assert_eq!(e.code, HostErrorCode::Invalid, "{m}");
            assert!(e.message.contains("upgrade it first"), "{e:?}");
        }
        // Other calls still go out.
        let (w, rx) = slot();
        h.call("get_home_dir".into(), json!({}), w);
        let id = peer.expect("call")["id"].as_u64().unwrap();
        peer.reply(id, Ok(json!("/home/x")));
        assert_eq!(rx.recv_timeout(T5).unwrap(), Ok(json!("/home/x")));
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
            h.term_input(Uuid::nil(), "x".into(), None, None)
                .unwrap_err()
                .code,
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
                first_message: None,
                adopt_existing: false,
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
        assert_eq!(rx.recv_timeout(T5).unwrap().map(|r| r.pid), Ok(Some(77)));
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
                first_message: None,
                adopt_existing: false,
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

    fn adopting_spec(t: Uuid) -> OpenSpec {
        OpenSpec {
            adopt_existing: true,
            ..open_spec(
                t,
                LaunchSpec {
                    agent: Some("claude".into()),
                    session_id: Some("s1".into()),
                    ..LaunchSpec::default()
                },
            )
        }
    }

    #[test]
    fn term_open_sends_adopt_only_with_capability() {
        // An older Daemon would answer a Terminal the caller did not ask for: never sent.
        let sh = shared();
        let mut peer = connect_capable(&sh, vec![], &["term"]);
        let h = handle(&sh);
        let (w, _rx) = res_slot();
        h.term_open(
            adopting_spec(Uuid::new_v4()),
            Arc::new(VecSink::default()),
            w,
        );
        let open = peer.expect("term.open");
        assert!(open["spec"].get("adoptExisting").is_none(), "{open}");

        let sh = shared();
        let mut peer = connect_capable(&sh, vec![], &["term", "term.open-existing"]);
        let h = handle(&sh);
        let (w, _rx) = res_slot();
        h.term_open(
            adopting_spec(Uuid::new_v4()),
            Arc::new(VecSink::default()),
            w,
        );
        let open = peer.expect("term.open");
        assert_eq!(open["spec"]["adoptExisting"], json!(true));
        // The caller's "no" stays no (local migration).
        let (w, _rx) = res_slot();
        h.term_open(
            open_spec(Uuid::new_v4(), LaunchSpec::default()),
            Arc::new(VecSink::default()),
            w,
        );
        let open = peer.expect("term.open");
        assert!(open["spec"].get("adoptExisting").is_none(), "{open}");
    }

    #[test]
    fn term_open_existed_returns_owner_and_drops_requested_sink() {
        let sh = shared();
        let owner = Uuid::new_v4();
        let mut peer = connect_capable(&sh, vec![info(owner)], &["term", "term.open-existing"]);
        let h = handle(&sh);
        let t = Uuid::new_v4();
        let (w, rx) = res_slot();
        h.term_open(adopting_spec(t), Arc::new(VecSink::default()), w);
        let open = peer.expect("term.open");
        let attach = peer.expect("term.attach");
        assert_eq!(attach["terminal"], json!(t));
        let mut b = res_frame(
            open["id"].as_u64().unwrap(),
            Ok(json!({"pid": 77, "terminal": owner, "existed": true})),
        );
        // The Daemon has no Terminal under the requested UUID.
        b.extend(res_frame(
            attach["id"].as_u64().unwrap(),
            Err(format!("unknown terminal {t}")),
        ));
        peer.write(&b);
        assert_eq!(
            rx.recv_timeout(T5).unwrap(),
            Ok(OpenReply {
                pid: Some(77),
                terminal: Some(owner),
                existed: true
            })
        );
        assert!(!sh.lock().atts.contains_key(&t));
        assert!(!sh.sizes.lock().unwrap().contains_key(&t));
        // An older Daemon's `{pid}` reads as the requested Terminal started.
        let t2 = Uuid::new_v4();
        let (w, rx) = res_slot();
        h.term_open(
            open_spec(t2, LaunchSpec::default()),
            Arc::new(VecSink::default()),
            w,
        );
        let open = peer.expect("term.open");
        let attach = peer.expect("term.attach");
        let mut b = res_frame(open["id"].as_u64().unwrap(), Ok(json!({"pid": 5})));
        b.extend(res_frame(
            attach["id"].as_u64().unwrap(),
            Ok(json!({"exitCode": null})),
        ));
        peer.write(&b);
        assert_eq!(
            rx.recv_timeout(T5).unwrap(),
            Ok(OpenReply {
                pid: Some(5),
                terminal: None,
                existed: false
            })
        );
        assert!(sh.lock().atts.contains_key(&t2));
    }

    /// A2: the attach could not even be queued (no request slot left). The open's answer
    /// still decides: an adoption wins, anything else reports the attach's failure.
    #[test]
    fn term_open_attach_enqueue_failure_waits_for_the_open() {
        let sh = shared();
        let owner = Uuid::new_v4();
        let limits = crate::link::LinkLimits {
            max_pending: 1,
            ..Default::default()
        };
        let (mut peer, _l, _, _) = connect_full(
            &sh,
            vec![info(owner)],
            limits,
            &["term", "term.open-existing"],
        );
        let h = handle(&sh);
        let t = Uuid::new_v4();
        let (w, rx) = res_slot();
        h.term_open(adopting_spec(t), Arc::new(VecSink::default()), w);
        let open = peer.expect("term.open");
        // Not answered before the open is.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        peer.reply(
            open["id"].as_u64().unwrap(),
            Ok(json!({"pid": 9, "terminal": owner, "existed": true})),
        );
        assert_eq!(
            rx.recv_timeout(T5).unwrap(),
            Ok(OpenReply {
                pid: Some(9),
                terminal: Some(owner),
                existed: true
            })
        );
        assert!(!sh.lock().atts.contains_key(&t));
        assert!(!sh.sizes.lock().unwrap().contains_key(&t));

        // A started Terminal whose attach was never sent: the attach's failure is answered.
        let t2 = Uuid::new_v4();
        let (w, rx) = res_slot();
        h.term_open(adopting_spec(t2), Arc::new(VecSink::default()), w);
        let open = peer.expect("term.open");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        peer.reply(
            open["id"].as_u64().unwrap(),
            Ok(json!({"pid": 10, "terminal": t2, "existed": false})),
        );
        let err = rx.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(err.code, HostErrorCode::Busy, "{err:?}");
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
        h.term_input(t, "probe".into(), None, None).unwrap();
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
            first_message: None,
            adopt_existing: false,
        }
    }

    fn prefixed_shared() -> Arc<Shared> {
        let sh = shared();
        if let HostRoute::Configured(cfg) = &mut sh.lock().cfg.route {
            cfg.launch_prefixes
                .insert("claude".into(), "wrap --x".into());
        }
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
    fn custom_title_update_gate_tracks_current_link_and_classifies_reply() {
        let sh = shared();
        let t = Uuid::new_v4();
        let h = handle(&sh);
        let patch = || {
            Some(
                json!({"customTitle": "Review"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
        };
        let mut peer = connect_capable(&sh, vec![info(t)], &["term"]);
        for value in [
            patch(),
            Some(json!({"customTitle": null}).as_object().unwrap().clone()),
        ] {
            let (w, rx) = res_slot();
            h.term_update(t, None, value, w);
            assert_eq!(
                rx.recv_timeout(T5).unwrap().unwrap_err().code,
                HostErrorCode::Invalid
            );
            assert_nothing_sent(&h, &mut peer, t);
        }
        // Ordinary title updates remain ungated on the same old Daemon.
        let (w, _rx) = res_slot();
        h.term_update(
            t,
            None,
            Some(json!({"title": "automatic"}).as_object().unwrap().clone()),
            w,
        );
        assert_eq!(peer.expect("term.update")["meta"]["title"], "automatic");

        let mut peer = connect_capable(
            &sh,
            vec![info(t)],
            &[xshell_protocol::cap::TERM_CUSTOM_TITLE],
        );
        let (w, rx) = res_slot();
        h.term_update(t, None, patch(), w);
        let sent = peer.expect("term.update");
        assert_eq!(sent["meta"]["customTitle"], "Review");
        peer.reply(
            sent["id"].as_u64().unwrap(),
            Err(format!(
                "{} sync failure",
                xshell_protocol::CUSTOM_TITLE_NOT_DURABLE
            )),
        );
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Indeterminate
        );

        // The old capability must not survive reconnect, even for a clear.
        let mut peer = connect_capable(&sh, vec![info(t)], &["term"]);
        let (w, rx) = res_slot();
        h.term_update(t, None, patch(), w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Invalid
        );
        assert_nothing_sent(&h, &mut peer, t);
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

    #[test]
    fn ring_join_sent_with_capability() {
        let sh = shared();
        let mut peer = connect_capable(&sh, vec![], &["term", "ring"]);
        let h = handle(&sh);
        let gen = h.status().link_generation;
        assert_eq!(gen, h.link_generation());
        let (w, rx) = res_slot();
        h.ring_join(vec!["xro1.a.b".into()], None, gen, w);
        let m = peer.expect("ring.join");
        assert_eq!(m["rosters"], json!(["xro1.a.b"]));
        assert!(m.get("expect").is_none());
        peer.reply(m["id"].as_u64().unwrap(), Ok(json!({"version": 1})));
        assert_eq!(rx.recv_timeout(T5).unwrap(), Ok(json!({"version": 1})));
        let (w, rx) = res_slot();
        h.ring_identity(gen, w);
        let m = peer.expect("ring.identity");
        peer.reply(m["id"].as_u64().unwrap(), Ok(json!({"name": "x"})));
        assert_eq!(rx.recv_timeout(T5).unwrap(), Ok(json!({"name": "x"})));
    }

    /// A conditional join needs `ring.cjoin`: an older Daemon would ignore the condition.
    #[test]
    fn conditional_ring_join_needs_cjoin() {
        let sh = shared();
        let t = Uuid::new_v4();
        let expect = || {
            Some(JoinExpect {
                ring_id: None,
                version: None,
            })
        };
        let mut peer = connect_capable(&sh, vec![], &["term", "ring"]);
        let h = handle(&sh);
        let (w, rx) = res_slot();
        h.ring_join(vec![], expect(), h.status().link_generation, w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Invalid
        );
        assert_nothing_sent(&h, &mut peer, t);
        let mut peer = connect_capable(&sh, vec![], &["term", "ring", "ring.cjoin"]);
        let (w, rx) = res_slot();
        h.ring_join(vec![], expect(), h.status().link_generation, w);
        let m = peer.expect("ring.join");
        assert_eq!(m["expect"], json!({"ringId": null}));
        peer.reply(m["id"].as_u64().unwrap(), Ok(json!({"version": 1})));
        assert!(rx.recv_timeout(T5).unwrap().is_ok());
    }

    /// A request for the connection the caller saw is never sent on a newer one.
    #[test]
    fn ring_requests_are_fenced_to_their_link_generation() {
        let sh = shared();
        let t = Uuid::new_v4();
        let _old = connect_capable(&sh, vec![], &["term", "ring"]);
        let h = handle(&sh);
        let seen = h.status().link_generation;
        // A reconnect between the caller's check and its dispatch.
        let mut peer = connect_capable(&sh, vec![], &["term", "ring"]);
        assert!(h.status().link_generation > seen);
        let (w, rx) = res_slot();
        h.ring_identity(seen, w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        let (w, rx) = res_slot();
        h.ring_join(vec![], None, seen, w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Offline
        );
        assert_nothing_sent(&h, &mut peer, t);
    }

    #[test]
    fn ring_join_refused_without_capability() {
        let sh = shared();
        let t = Uuid::new_v4();
        let (mut peer, _link, _) = connect(&sh, vec![]);
        let h = handle(&sh);
        let gen = h.status().link_generation;
        let (w, rx) = res_slot();
        h.ring_join(vec![], None, gen, w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Invalid
        );
        let (w, rx) = res_slot();
        h.ring_identity(gen, w);
        assert_eq!(
            rx.recv_timeout(T5).unwrap().unwrap_err().code,
            HostErrorCode::Invalid
        );
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

//! Push notifications, the Daemon side (#27, ADR-0006, ADR-0011). A Mobile registers over its
//! session (`push.register`: the Push Gateway's blob, its seal key, its triggers); on an Agent
//! Status change that an agent reported itself (a hook, or Codex's OSC 9 needs you) the
//! Daemon decides, seals the payload to that Mobile's seal key and asks the Relay to forward
//! it to the Push Gateway.
//!
//! - **Decide.** The trigger is on, the registration is neither dormant nor paused, the Relay
//!   connection is up and lists `push`, and no Mobile of the Ring is in the foreground (its
//!   lease running, `MemberPresence::is_foreground`). The Relay checks foreground again.
//! - **Debounce** per (Host, Mobile), leading plus trailing over [`Config::push_window`]
//!   ([`Debounce`]): the first change in a quiet window goes out at once, later ones wait
//!   for the window's end and only the newest still-current one goes out.
//! - **An attempt** is reserved on the worker (the slot is then owned by that attempt's id,
//!   and only its own completion frees it), then runs on a sender thread: it writes its `seq`
//!   to disk (a failed write drops it), seals, and at the **submission boundary** checks
//!   everything again in one ordered step, holding the registry lock and then this module's
//!   lock while it queues the frame without blocking: the Terminal still has that run and
//!   status, the registration has that generation and its trigger on, the trusted head is the
//!   one adopted at the reservation (its adoption epoch), no Mobile is in the foreground, and
//!   the Daemon is not stopping. Anything else drops it, never sent late. The answer is
//!   awaited outside every lock.
//! - **Sends never block** the pipeline: at most one per Mobile and [`MAX_OUTSTANDING`] in
//!   all; events meanwhile coalesce in the Mobile's pending slot. Answers are fenced by the
//!   registration generation: one for a replaced registration changes nothing.
//! - Nothing is queued while the Daemon is offline, and a timeout (unknown outcome) is never
//!   retried. Only `reconcile_pending` is retried, once, after [`Config::push_retry`].
//! - **Stopping** refuses new work, then joins the worker and the senders (bounded) and
//!   closes the store, so nothing is written once the Daemon's lock is released.
//!
//! The registrations live in `ring_dir/push.json` (0600, like the Ring's other files):
//! `{"v":1,"ringId":"…","mobiles":{"<signKey>":{"noiseKey","blob","sealKey","triggers",
//! "registeredAt","dormant","gen","seq"}}}`. An entry is kept only while the trusted head
//! lists that sign key with the same Noise key as a `mobile`; another Ring wipes the file.
//! Heads arrive with the Ring's adoption epoch; a stale one (a lower epoch) is ignored, and
//! a registration is checked against the head adopted here, never a snapshot of its own.
//!
//! Lock order: the registry lock and the Ring's locks come before this module's state lock;
//! under it only the Connector's and the Relay client's (queueing a frame) are taken.
//!
//! [`Config::push_window`]: super::Config::push_window
//! [`Config::push_retry`]: super::Config::push_retry

use super::registry::{now_ms, Daemon, Registry};
use super::ring::{read_private, write_private};
use super::{PushHooks, PushPoint};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::{hook_agent, HookAgent};
use xshell_protocol::msg::{AgentStatus, PushTriggers};
use xshell_protocol::ring::push::{
    blob_well_formed, collapse_id, seal, PushAgent, PushPayload, PushStatus,
};
use xshell_protocol::ring::relay::wire::CAP_PUSH;
use xshell_protocol::ring::relay::{Connector, ErrorCode, LinkState, PushRequest};
use xshell_protocol::ring::{
    DeviceKeys, Member, NoiseKey, RingError, RingId, Role, RosterChain, SignKey,
};

/// Sends in flight at once, per Daemon.
pub(crate) const MAX_OUTSTANDING: usize = 4;
/// Events waiting for the worker; the oldest is dropped beyond.
const QUEUE_CAP: usize = 64;
/// Events one Mobile's pending slot keeps (one per Terminal, newest last).
const PENDING_CAP: usize = 16;
/// Gateway codes that make a registration dormant until the Mobile registers again.
const DORMANT: &[&str] = &[
    "device_gone",
    "blob_expired",
    "blob_invalid",
    "binding_moved",
    "subscription_inactive",
];
/// Gateway codes that pause every Mobile until the next UTC midnight.
const PAUSE: &[&str] = &["quota_exceeded", "attempts_exceeded"];
/// The one code retried (the gateway refunded that delivery).
const RETRY: &str = "reconcile_pending";
/// The refusal for anyone but a Mobile.
pub(crate) const MOBILE_ONLY: &str = "push.register is for a Mobile";

#[derive(Debug, Clone)]
pub(crate) struct PushConfig {
    pub window: Duration,
    pub timeout: Duration,
    pub retry: Duration,
    pub hooks: PushHooks,
}

/// An Agent Status change an agent reported itself.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentEvent {
    pub terminal: Uuid,
    pub run: u64,
    pub status: AgentStatus,
    /// Unix ms of the change.
    pub at: u64,
}

// ---- Debounce ---------------------------------------------------------------------------

/// Leading plus trailing debounce for one (Host, Mobile), pure: the caller passes the clock
/// and names each attempt. An event offered in a quiet window goes out at once; later ones
/// are kept (one per key, newest last) until the window since the last send ends, and then
/// handed back, newest first, for the caller to send the newest that is still current.
/// While an attempt owns the slot everything is kept, and only that attempt frees it.
#[derive(Debug)]
pub(crate) struct Debounce<E> {
    window: Duration,
    last: Option<Instant>,
    owner: Option<u64>,
    pending: Vec<E>,
}

impl<E> Debounce<E> {
    pub fn new(window: Duration) -> Self {
        Debounce {
            window,
            last: None,
            owner: None,
            pending: Vec::new(),
        }
    }

    /// An eligible event at `now`: `Some` to attempt it now as attempt `id` (which then owns
    /// the slot until [`Debounce::done`]), `None` when it was kept. `same` says which kept
    /// event it replaces.
    pub fn offer(
        &mut self,
        now: Instant,
        e: E,
        same: impl Fn(&E, &E) -> bool,
        id: u64,
    ) -> Option<E> {
        let quiet = self.last.is_none_or(|l| now >= l + self.window);
        if self.owner.is_none() && self.pending.is_empty() && quiet {
            self.owner = Some(id);
            return Some(e);
        }
        self.keep(e, same);
        None
    }

    fn keep(&mut self, e: E, same: impl Fn(&E, &E) -> bool) {
        self.pending.retain(|p| !same(p, &e));
        if self.pending.len() >= PENDING_CAP {
            self.pending.remove(0);
        }
        self.pending.push(e);
    }

    /// When the kept events are due; `None` while owned or with nothing kept.
    pub fn due(&self, now: Instant) -> Option<Instant> {
        if self.owner.is_some() || self.pending.is_empty() {
            return None;
        }
        Some(self.last.map_or(now, |l| l + self.window))
    }

    /// The kept events, newest first, once due; attempt `id` then owns the slot.
    pub fn take(&mut self, now: Instant, id: u64) -> Vec<E> {
        match self.due(now) {
            Some(at) if at <= now => {
                self.owner = Some(id);
                let mut v = std::mem::take(&mut self.pending);
                v.reverse();
                v
            }
            _ => Vec::new(),
        }
    }

    /// An attempt went out at `now`: the window starts again.
    pub fn sent(&mut self, now: Instant) {
        self.last = Some(now);
    }

    /// Attempt `id` is over (answered, or dropped before sending). `false`: it does not own
    /// the slot, which stays as it is.
    pub fn done(&mut self, id: u64) -> bool {
        if self.owner != Some(id) {
            return false;
        }
        self.owner = None;
        true
    }

    pub fn owned_by(&self, id: u64) -> bool {
        self.owner == Some(id)
    }

    pub fn is_owned(&self) -> bool {
        self.owner.is_some()
    }

    /// Forgets what is kept (a new registration).
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

// ---- The store ----------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct Reg {
    noise_key: NoiseKey,
    blob: String,
    seal_key: NoiseKey,
    triggers: PushTriggers,
    registered_at: u64,
    /// Why the gateway stopped taking pushes for it; cleared by registering again.
    dormant: Option<String>,
    /// Bumped by every registration; fences answers of older attempts.
    gen: u64,
    /// The last `seq` sealed to this Mobile.
    seq: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct PushFile {
    v: u32,
    ring_id: Option<RingId>,
    mobiles: BTreeMap<SignKey, Reg>,
}

impl Default for PushFile {
    fn default() -> Self {
        PushFile {
            v: 1,
            ring_id: None,
            mobiles: BTreeMap::new(),
        }
    }
}

fn wants(t: &PushTriggers, s: AgentStatus) -> bool {
    match s {
        AgentStatus::NeedsYou => t.needs_you,
        AgentStatus::Finished => t.finished,
        AgentStatus::Working | AgentStatus::Ended => false,
    }
}

/// Whether `head` lists `key` as a Mobile with Noise key `noise`.
fn listed_mobile(chain: &RosterChain, key: &SignKey, noise: &NoiseKey) -> bool {
    chain
        .head()
        .member(key)
        .is_some_and(|m| m.role == Role::Mobile && &m.noise_key == noise)
}

fn next_utc_midnight(now_ms: u64) -> u64 {
    (now_ms / 86_400_000 + 1) * 86_400_000
}

// ---- The pipeline --------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Cand {
    ev: AgentEvent,
    /// The registration generation it was taken under.
    gen: u64,
}

struct Slot {
    deb: Debounce<Cand>,
    /// `reconcile_pending`: the one retry, due at that instant, run by the attempt that
    /// still owns the slot.
    retry: Option<(Instant, Cand, u64)>,
}

struct St {
    file: PushFile,
    /// The trusted chain adopted last, and its adoption epoch.
    chain: Option<RosterChain>,
    epoch: u64,
    slots: HashMap<SignKey, Slot>,
    queue: VecDeque<AgentEvent>,
    outstanding: usize,
    /// Every Mobile is paused until then (unix ms): the gateway's daily caps.
    paused_until: Option<u64>,
    next_gen: u64,
    next_attempt: u64,
    /// Snapshots taken for the file, so an older one is never written over a newer one.
    snapshots: u64,
    /// The Relay connection a "no push" was last logged for.
    logged: Option<u64>,
    stop: bool,
}

impl St {
    fn paused(&self, now_ms: u64) -> bool {
        self.paused_until.is_some_and(|u| now_ms < u)
    }

    fn attempt_id(&mut self) -> u64 {
        self.next_attempt += 1;
        self.next_attempt
    }

    /// The registration of `key` if attempt `cand` may still go out under it: the same
    /// generation, its trigger on, not dormant or paused, and still listed in the adopted
    /// head of the Ring the file is for.
    fn valid(&self, key: &SignKey, cand: &Cand) -> Option<Reg> {
        let chain = self.chain.as_ref()?;
        if self.stop || self.paused(now_ms()) || self.file.ring_id.as_ref() != Some(chain.ring_id())
        {
            return None;
        }
        self.file
            .mobiles
            .get(key)
            .filter(|r| {
                r.gen == cand.gen
                    && r.dormant.is_none()
                    && wants(&r.triggers, cand.ev.status)
                    && listed_mobile(chain, key, &r.noise_key)
            })
            .cloned()
    }

    /// Drops the slots of `key`s no longer registered, except one an attempt still owns.
    fn prune_slots(&mut self) {
        let St { file, slots, .. } = self;
        slots.retain(|k, s| {
            if file.mobiles.contains_key(k) {
                return true;
            }
            s.deb.clear();
            s.retry = None;
            s.deb.is_owned()
        });
    }
}

/// The written store: the newest snapshot on disk, and whether it is closed for good.
#[derive(Default)]
struct Written {
    n: u64,
    closed: bool,
}

struct Inner {
    path: PathBuf,
    cfg: PushConfig,
    st: Mutex<St>,
    cv: Condvar,
    written: Mutex<Written>,
    daemon: OnceLock<Weak<Daemon>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    senders: Mutex<Vec<JoinHandle<()>>>,
}

/// See the module docs.
pub(crate) struct Push {
    inner: Arc<Inner>,
}

/// What the Relay connection looks like for one decision.
struct Link {
    connector: Arc<Connector>,
    keys: Arc<DeviceKeys>,
    /// Connected, with `push` listed.
    usable: bool,
    foreground: bool,
    connection: Option<u64>,
}

/// What the payload says about the Terminal, read when the event was found current.
struct Snap {
    agent: PushAgent,
    project: String,
    title: Option<String>,
    needs_you: u32,
}

/// One reserved attempt, handed to its sender thread.
struct Reserved {
    key: SignKey,
    id: u64,
    cand: Cand,
    retry: bool,
    reg: Reg,
    seq: u64,
    epoch: u64,
    ring: RingId,
    snap: Snap,
    keys: Arc<DeviceKeys>,
}

enum Work {
    Event(AgentEvent),
    Fire {
        key: SignKey,
        cands: Vec<Cand>,
        id: u64,
        retry: bool,
    },
}

fn lock(inner: &Inner) -> MutexGuard<'_, St> {
    inner.st.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits until `h` finished, at most until `deadline`; joins it if it did.
fn join_by(h: JoinHandle<()>, deadline: Instant) -> bool {
    while !h.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if h.is_finished() {
        let _ = h.join();
        true
    } else {
        false
    }
}

impl Push {
    /// Loads `ring_dir/push.json`. An unreadable file is moved aside and logged.
    pub fn new(ring_dir: PathBuf, cfg: PushConfig) -> Push {
        let path = ring_dir.join("push.json");
        let file = match read_private(&path) {
            Ok(None) => PushFile::default(),
            Ok(Some(bytes)) => match serde_json::from_slice::<PushFile>(&bytes) {
                Ok(f) if f.v == 1 => f,
                r => {
                    let why = r.err().map_or("unknown version".into(), |e| e.to_string());
                    let aside = path.with_extension(format!("json.bad-{}", now_ms()));
                    let _ = std::fs::rename(&path, &aside);
                    crate::log!(
                        "ERROR",
                        "{} is invalid ({why}); moved to {}",
                        path.display(),
                        aside.display()
                    );
                    PushFile::default()
                }
            },
            Err(e) => {
                crate::log!("ERROR", "cannot read {}: {e}", path.display());
                PushFile::default()
            }
        };
        let next_gen = file.mobiles.values().map(|r| r.gen).max().unwrap_or(0) + 1;
        Push {
            inner: Arc::new(Inner {
                path,
                cfg,
                st: Mutex::new(St {
                    file,
                    chain: None,
                    epoch: 0,
                    slots: HashMap::new(),
                    queue: VecDeque::new(),
                    outstanding: 0,
                    paused_until: None,
                    next_gen,
                    next_attempt: 0,
                    snapshots: 0,
                    logged: None,
                    stop: false,
                }),
                cv: Condvar::new(),
                written: Mutex::new(Written::default()),
                daemon: OnceLock::new(),
                worker: Mutex::new(None),
                senders: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Serves `d` from now on: starts the worker.
    pub fn bind(&self, d: Weak<Daemon>) {
        if self.inner.daemon.set(d).is_err() {
            return;
        }
        let inner = self.inner.clone();
        match std::thread::Builder::new()
            .name("push".into())
            .spawn(move || inner.run())
        {
            Ok(h) => *self.inner.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(h),
            Err(e) => crate::log!("ERROR", "push: cannot start the worker: {e}"),
        }
    }

    /// Refuses new work from now on: no event, registration or attempt is taken.
    pub fn stop(&self) {
        lock(&self.inner).stop = true;
        self.inner.cv.notify_all();
    }

    /// Stops, joins the worker and the senders (waiting at most until `deadline`), and
    /// closes the store: once this returns nothing more is written, whatever is still
    /// running.
    pub fn shutdown(&self, deadline: Instant) {
        self.stop();
        let worker = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let mut late = 0;
        if let Some(h) = worker {
            late += usize::from(!join_by(h, deadline));
        }
        let senders =
            std::mem::take(&mut *self.inner.senders.lock().unwrap_or_else(|e| e.into_inner()));
        for h in senders {
            late += usize::from(!join_by(h, deadline));
        }
        // Waits for a write in progress; none starts afterwards.
        self.inner
            .written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
        if late > 0 {
            crate::log!("WARN", "push: {late} push threads still running at exit");
        }
    }

    /// An Agent Status change an agent reported itself. Never blocks (callers hold the
    /// registry lock): a newer event for the same Terminal replaces a queued one, and the
    /// oldest is dropped past the queue's cap.
    pub fn notify(&self, terminal: Uuid, run: u64, status: AgentStatus) {
        if !matches!(status, AgentStatus::NeedsYou | AgentStatus::Finished) {
            return;
        }
        let mut st = lock(&self.inner);
        if st.stop || st.file.mobiles.is_empty() {
            return;
        }
        st.queue.retain(|e| e.terminal != terminal);
        if st.queue.len() >= QUEUE_CAP {
            st.queue.pop_front();
        }
        st.queue.push_back(AgentEvent {
            terminal,
            run,
            status,
            at: now_ms(),
        });
        drop(st);
        self.inner.cv.notify_all();
    }

    /// The trusted chain the Ring adopted at `epoch`: entries the head no longer lists as
    /// that Mobile go, and another Ring wipes them all. A lower epoch than one seen (a
    /// callback that lost a race) changes nothing.
    pub fn on_head(&self, epoch: u64, chain: &RosterChain) {
        let changed = {
            let mut st = lock(&self.inner);
            if st.stop || epoch <= st.epoch {
                return;
            }
            st.epoch = epoch;
            st.chain = Some(chain.clone());
            let ring = chain.ring_id().clone();
            let mut changed = false;
            if st.file.ring_id.as_ref() != Some(&ring) {
                changed = st.file.ring_id.is_some() || !st.file.mobiles.is_empty();
                st.file.mobiles.clear();
                st.file.ring_id = Some(ring);
            }
            let before = st.file.mobiles.len();
            st.file
                .mobiles
                .retain(|k, r| listed_mobile(chain, k, &r.noise_key));
            changed |= st.file.mobiles.len() != before;
            st.prune_slots();
            changed
        };
        if changed {
            if let Err(e) = self.inner.save() {
                crate::log!("ERROR", "push: cannot save the registrations: {e}");
            }
        }
    }

    /// `push.register` from `peer` (the session's member; `None` for a local connection),
    /// checked against the head adopted here.
    pub fn register(
        &self,
        peer: Option<&Member>,
        blob: &str,
        seal_key: &str,
        triggers: PushTriggers,
    ) -> Result<Value, String> {
        let peer = peer.filter(|m| m.role == Role::Mobile).ok_or(MOBILE_ONLY)?;
        if !blob_well_formed(blob) {
            return Err("push.register: not a push blob".into());
        }
        let seal_key =
            NoiseKey::parse(seal_key).map_err(|e| format!("push.register: sealKey: {e}"))?;
        if seal_key == peer.noise_key {
            return Err("push.register: sealKey must not be the session key".into());
        }
        {
            let mut st = lock(&self.inner);
            if st.stop {
                return Err("xshelld is exiting".into());
            }
            let chain = st.chain.as_ref().ok_or("this Host is in no Ring")?;
            if st.file.ring_id.as_ref() != Some(chain.ring_id())
                || !listed_mobile(chain, &peer.sign_key, &peer.noise_key)
            {
                return Err("push.register: not a Mobile of this Ring".into());
            }
            let gen = st.next_gen;
            st.next_gen += 1;
            let seq = st.file.mobiles.get(&peer.sign_key).map_or(0, |r| r.seq);
            st.file.mobiles.insert(
                peer.sign_key,
                Reg {
                    noise_key: peer.noise_key,
                    blob: blob.to_string(),
                    seal_key,
                    triggers,
                    registered_at: now_ms(),
                    dormant: None,
                    gen,
                    seq,
                },
            );
            st.paused_until = None;
            if let Some(s) = st.slots.get_mut(&peer.sign_key) {
                s.deb.clear();
            }
        }
        crate::log!(
            "INFO",
            "push: {} registered (needs you: {}, finished: {})",
            peer.name,
            triggers.needs_you,
            triggers.finished
        );
        self.inner
            .save()
            .map_err(|e| format!("cannot save the push registration: {e}"))?;
        Ok(Value::Null)
    }

    /// `push.unregister` from `peer`.
    pub fn unregister(&self, peer: Option<&Member>) -> Result<Value, String> {
        let peer = peer
            .filter(|m| m.role == Role::Mobile)
            .ok_or("push.unregister is for a Mobile")?;
        let removed = {
            let mut st = lock(&self.inner);
            let removed = st.file.mobiles.remove(&peer.sign_key).is_some();
            st.prune_slots();
            removed
        };
        if removed {
            self.inner
                .save()
                .map_err(|e| format!("cannot save the push registration: {e}"))?;
        }
        Ok(Value::Null)
    }
}

impl Inner {
    fn hook(&self, p: PushPoint) {
        if let Some(h) = &self.cfg.hooks.at {
            h(p);
        }
    }

    /// The clock `seq` is drawn from (unix ms).
    fn clock(&self) -> u64 {
        self.cfg.hooks.clock.map_or_else(now_ms, |f| f())
    }

    /// Writes the registrations; a snapshot older than one already written is skipped, and
    /// nothing is written once the store is closed.
    fn save(&self) -> Result<(), String> {
        self.hook(PushPoint::Save);
        if self
            .cfg
            .hooks
            .fail_saves
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
        {
            return Err("refused by a test hook".into());
        }
        let (n, bytes) = {
            let mut st = lock(self);
            st.snapshots += 1;
            let bytes = serde_json::to_vec(&st.file).map_err(|e| e.to_string())?;
            (st.snapshots, bytes)
        };
        let mut w = self.written.lock().unwrap_or_else(|e| e.into_inner());
        if w.closed {
            return Err("xshelld is exiting".into());
        }
        if w.n > n {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            super::ring::Store::new(dir.to_path_buf())
                .ensure_dir()
                .map_err(|e| e.to_string())?;
        }
        write_private(&self.path, &bytes).map_err(|e| e.to_string())?;
        w.n = n;
        Ok(())
    }

    fn daemon(&self) -> Option<Arc<Daemon>> {
        self.daemon.get().and_then(Weak::upgrade)
    }

    fn run(self: Arc<Self>) {
        loop {
            let work = {
                let mut st = lock(&self);
                loop {
                    if st.stop {
                        return;
                    }
                    if let Some(ev) = st.queue.pop_front() {
                        break Work::Event(ev);
                    }
                    let now = Instant::now();
                    let mut next: Option<Instant> = None;
                    let mut fire = None;
                    if st.outstanding < MAX_OUTSTANDING {
                        for (k, s) in st.slots.iter() {
                            let due = s
                                .retry
                                .as_ref()
                                .map(|r| (r.0, true))
                                .or_else(|| s.deb.due(now).map(|at| (at, false)));
                            match due {
                                Some((at, retry)) if at <= now => {
                                    fire = Some((*k, retry));
                                    break;
                                }
                                Some((at, _)) => next = Some(next.map_or(at, |n| n.min(at))),
                                None => {}
                            }
                        }
                    }
                    if let Some((key, retry)) = fire {
                        let id = st.attempt_id();
                        let Some(s) = st.slots.get_mut(&key) else {
                            continue;
                        };
                        let (cands, id) = if retry {
                            match s.retry.take() {
                                Some((_, c, owner)) => (vec![c], owner),
                                None => (Vec::new(), id),
                            }
                        } else {
                            (s.deb.take(now, id), id)
                        };
                        break Work::Fire {
                            key,
                            cands,
                            id,
                            retry,
                        };
                    }
                    st = match next {
                        None => self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
                        Some(at) => {
                            self.cv
                                .wait_timeout(st, at.saturating_duration_since(now))
                                .unwrap_or_else(|e| e.into_inner())
                                .0
                        }
                    };
                }
            };
            match work {
                Work::Event(ev) => self.offer(ev),
                Work::Fire {
                    key,
                    cands,
                    id,
                    retry,
                } => self.reserve(key, cands, id, retry),
            }
        }
    }

    /// The Relay connection as a decision needs it; `None` outside a Ring.
    fn link(&self) -> Option<Link> {
        let d = self.daemon()?;
        let (connector, keys, _) = d.ring.push_link()?;
        let connected = matches!(connector.state(), LinkState::Connected { .. });
        let caps = connector.relay_caps();
        let members = connector.members();
        let now = now_ms();
        let foreground = members.as_ref().is_some_and(|ms| {
            ms.iter()
                .any(|m| m.member.role == Role::Mobile && m.presence.is_foreground(now))
        });
        let usable =
            connected && members.is_some() && caps.is_some_and(|c| c.iter().any(|c| c == CAP_PUSH));
        Some(Link {
            connection: connector.connection(),
            connector,
            keys,
            usable,
            foreground,
        })
    }

    /// Logs a Relay without push once per connection.
    fn log_no_push(&self, link: &Link, why: &str) {
        let first = {
            let mut st = lock(self);
            let first = st.logged != link.connection;
            st.logged = link.connection;
            first
        };
        if first {
            crate::log!("INFO", "push: not sent: {why}");
        }
    }

    /// Whether the decision holds now: `None` drops the event.
    fn decide(&self) -> Option<Link> {
        let link = self.link()?;
        if !link.usable {
            if link.connection.is_some() {
                self.log_no_push(&link, "the relay does not forward pushes");
            }
            return None;
        }
        if link.foreground {
            return None;
        }
        Some(link)
    }

    /// A new event: offered to every Mobile whose trigger is on; the ones whose window is
    /// quiet attempt it at once.
    fn offer(self: &Arc<Self>, ev: AgentEvent) {
        if self.decide().is_none() {
            return;
        }
        let now = Instant::now();
        let window = self.cfg.window;
        let go: Vec<(SignKey, Cand, u64)> = {
            let mut st = lock(self);
            if st.stop || st.paused(now_ms()) {
                return;
            }
            let regs: Vec<(SignKey, u64)> = st
                .file
                .mobiles
                .iter()
                .filter(|(_, r)| r.dormant.is_none() && wants(&r.triggers, ev.status))
                .map(|(k, r)| (*k, r.gen))
                .collect();
            let mut go = Vec::new();
            for (k, gen) in regs {
                let id = st.attempt_id();
                let slot = st.slots.entry(k).or_insert_with(|| Slot {
                    deb: Debounce::new(window),
                    retry: None,
                });
                let c = Cand {
                    ev: ev.clone(),
                    gen,
                };
                if let Some(c) = slot
                    .deb
                    .offer(now, c, |a, b| a.ev.terminal == b.ev.terminal, id)
                {
                    go.push((k, c, id));
                }
            }
            go
        };
        for (k, c, id) in go {
            self.reserve(k, vec![c], id, false);
        }
    }

    /// The Terminal `ev` is about, if it is still listed with that run and status.
    fn current_in(reg: &Registry, ev: &AgentEvent) -> Option<Snap> {
        if reg.frozen {
            return None;
        }
        let t = reg.terminals.get(&ev.terminal)?;
        if t.run != ev.run || t.agent_status() != Some(ev.status) {
            return None;
        }
        let info = t.info();
        // Only what the Mobile's Inbox lists: a direct agent (ADR-0004). A wrapped agent's
        // cwd and title stay on the Host.
        if !info.spec.is_direct_agent() {
            return None;
        }
        let agent = match hook_agent(&info.spec)? {
            HookAgent::Claude => PushAgent::Claude,
            HookAgent::Codex => PushAgent::Codex,
        };
        let needs_you = reg
            .terminals
            .values()
            .filter(|t| {
                t.agent_status() == Some(AgentStatus::NeedsYou) && t.spec().is_direct_agent()
            })
            .count();
        Some(Snap {
            agent,
            project: info.spec.cwd.clone(),
            title: info
                .meta
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            needs_you: needs_you.min(u32::MAX as usize) as u32,
        })
    }

    fn current(&self, ev: &AgentEvent) -> Option<Snap> {
        let d = self.daemon()?;
        let reg = d.reg.lock().unwrap_or_else(|e| e.into_inner());
        Self::current_in(&reg, ev)
    }

    /// Attempt `id` is over without a send (or before one was queued).
    fn release(&self, key: &SignKey, id: u64) {
        if let Some(s) = lock(self).slots.get_mut(key) {
            s.deb.done(id);
        }
        self.cv.notify_all();
    }

    /// Reserves attempt `id` for `key` (which owns the slot): the newest of `cands` still
    /// current, if everything else holds. Its `seq` is taken here; the rest runs on a
    /// sender thread.
    fn reserve(self: &Arc<Self>, key: SignKey, cands: Vec<Cand>, id: u64, retry: bool) {
        let Some(link) = self.decide() else {
            return self.release(&key, id);
        };
        let Some((cand, snap)) = cands
            .into_iter()
            .find_map(|c| self.current(&c.ev).map(|s| (c, s)))
        else {
            return self.release(&key, id);
        };
        let now = Instant::now();
        let seq_floor = self.clock();
        let r = {
            let mut st = lock(self);
            let valid = st.valid(&key, &cand);
            let at_capacity = st.outstanding >= MAX_OUTSTANDING;
            let epoch = st.epoch;
            let ring = st.chain.as_ref().map(|c| c.ring_id().clone());
            let Some(slot) = st.slots.get_mut(&key) else {
                return;
            };
            if !slot.deb.owned_by(id) {
                return;
            }
            match (valid, ring) {
                (Some(reg), Some(ring)) if !at_capacity => {
                    if !retry {
                        slot.deb.sent(now);
                    }
                    st.outstanding += 1;
                    let seq = (reg.seq + 1).max(seq_floor);
                    if let Some(r) = st.file.mobiles.get_mut(&key) {
                        r.seq = seq;
                    }
                    Some(Reserved {
                        key,
                        id,
                        cand,
                        retry,
                        reg,
                        seq,
                        epoch,
                        ring,
                        snap,
                        keys: link.keys.clone(),
                    })
                }
                (Some(_), Some(_)) => {
                    // At capacity: back into the pending slot, out when a send finishes.
                    slot.deb.done(id);
                    slot.deb.keep(cand, |a, b| a.ev.terminal == b.ev.terminal);
                    None
                }
                _ => {
                    slot.deb.done(id);
                    None
                }
            }
        };
        let Some(r) = r else {
            self.cv.notify_all();
            return;
        };
        let inner = self.clone();
        let spawned = std::thread::Builder::new()
            .name("push-send".into())
            .spawn(move || inner.send(r));
        let mut senders = self.senders.lock().unwrap_or_else(|e| e.into_inner());
        senders.retain(|h| !h.is_finished());
        match spawned {
            Ok(h) => senders.push(h),
            Err(e) => {
                drop(senders);
                crate::log!("WARN", "push: cannot start a sender: {e}");
                self.dropped(&key, id);
            }
        }
    }

    /// A reserved attempt that never reached the Relay.
    fn dropped(&self, key: &SignKey, id: u64) {
        {
            let mut st = lock(self);
            st.outstanding = st.outstanding.saturating_sub(1);
            if let Some(s) = st.slots.get_mut(key) {
                s.deb.done(id);
            }
        }
        self.cv.notify_all();
    }

    /// One reserved attempt, on its own thread: persist its `seq`, seal, check everything
    /// again and queue it in one ordered step, then wait for the answer outside every lock.
    fn send(self: Arc<Self>, r: Reserved) {
        // The seq is on disk before anything is sent, so it never repeats; if it cannot be
        // written, nothing is sent (and nothing retried).
        if let Err(e) = self.save() {
            crate::log!(
                "ERROR",
                "push: not sent: cannot save the push sequence: {e}"
            );
            return self.dropped(&r.key, r.id);
        }
        let host = r.keys.sign_key();
        let payload = PushPayload {
            host,
            terminal: r.cand.ev.terminal,
            status: match r.cand.ev.status {
                AgentStatus::NeedsYou => PushStatus::NeedsYou,
                _ => PushStatus::Finished,
            },
            agent: r.snap.agent,
            project: r.snap.project.clone(),
            title: r.snap.title.clone(),
            at: r.cand.ev.at,
            seq: r.seq,
            needs_you: r.snap.needs_you,
        };
        let sealed = match seal(&r.keys, &r.ring, &r.reg.seal_key, &payload) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("ERROR", "push: cannot seal: {e}");
                return self.dropped(&r.key, r.id);
            }
        };
        let req = PushRequest {
            blob: r.reg.blob.clone(),
            sealed_payload: sealed,
            collapse_id: collapse_id(&host),
        };
        self.hook(PushPoint::Submit);
        // The submission boundary.
        let Some(link) = self.decide() else {
            return self.dropped(&r.key, r.id);
        };
        let Some(d) = self.daemon() else {
            return self.dropped(&r.key, r.id);
        };
        let ticket = {
            let reg = d.reg.lock().unwrap_or_else(|e| e.into_inner());
            if Self::current_in(&reg, &r.cand.ev).is_none() {
                drop(reg);
                return self.dropped(&r.key, r.id);
            }
            let st = lock(&self);
            let owned = st.slots.get(&r.key).is_some_and(|s| s.deb.owned_by(r.id));
            if !owned
                || st.epoch != r.epoch
                || st.chain.as_ref().map(|c| c.ring_id()) != Some(&r.ring)
                || st.valid(&r.key, &r.cand).is_none()
            {
                drop(st);
                drop(reg);
                return self.dropped(&r.key, r.id);
            }
            link.connector.push_start(&req)
        };
        let result = ticket.and_then(|t| t.wait(self.cfg.timeout));
        self.complete(r.key, r.id, r.cand, r.retry, result);
    }

    /// The answer to attempt `id`, applied only to the registration generation it was sent
    /// under; only this attempt frees the slot it owns.
    fn complete(&self, key: SignKey, id: u64, cand: Cand, retry: bool, r: Result<(), RingError>) {
        let mut log: Option<(&'static str, String)> = None;
        let mut save = false;
        {
            let mut st = lock(self);
            st.outstanding = st.outstanding.saturating_sub(1);
            let current = st.file.mobiles.get(&key).is_some_and(|r| r.gen == cand.gen);
            let mut retrying = false;
            match &r {
                Ok(()) => {}
                Err(RingError::Relay {
                    code: ErrorCode::PushFailed,
                    detail,
                }) => {
                    let d = detail.as_deref().unwrap_or("");
                    if DORMANT.contains(&d) {
                        if current {
                            if let Some(reg) = st.file.mobiles.get_mut(&key) {
                                reg.dormant = Some(d.to_string());
                                save = true;
                            }
                            log = Some(("INFO", format!("push: registration dormant ({d})")));
                        }
                    } else if PAUSE.contains(&d) {
                        if current {
                            st.paused_until = Some(next_utc_midnight(now_ms()));
                            log = Some(("WARN", format!("push: paused until midnight UTC ({d})")));
                        }
                    } else if d == RETRY {
                        let stop = st.stop;
                        if current && !retry && !stop {
                            if let Some(s) = st.slots.get_mut(&key).filter(|s| s.deb.owned_by(id)) {
                                s.retry = Some((Instant::now() + self.cfg.retry, cand.clone(), id));
                                retrying = true;
                            }
                        }
                    } else if d == "foreground" {
                    } else if d == "bad_request" || d == "payload_too_large" {
                        log = Some(("ERROR", format!("push: the gateway refused it ({d})")));
                    } else if d == "unavailable" {
                        if st.logged.is_none() {
                            log = Some(("INFO", "push: the relay has no push gateway".into()));
                        }
                    } else {
                        log = Some(("WARN", format!("push: not delivered ({d})")));
                    }
                }
                Err(RingError::Timeout) => {
                    log = Some(("WARN", "push: no answer in time; not retried".into()));
                }
                Err(e) => log = Some(("WARN", format!("push: not sent: {e}"))),
            }
            if !retrying {
                if let Some(s) = st.slots.get_mut(&key) {
                    s.deb.done(id);
                }
            }
        }
        self.cv.notify_all();
        if let Some((level, m)) = log {
            match level {
                "ERROR" => crate::log!("ERROR", "{m}"),
                "WARN" => crate::log!("WARN", "{m}"),
                _ => crate::log!("INFO", "{m}"),
            }
        }
        if save {
            if let Err(e) = self.save() {
                crate::log!("ERROR", "push: cannot save the registrations: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::role::tests::daemon;
    use crate::server::terminal::{self, Terminal};
    use serde_json::Map;
    use xshell_core::launch::LaunchSpec;
    use xshell_core::terminal::state::PersistedTerminal;

    const W: Duration = Duration::from_secs(10);

    fn same(a: &(u8, u8), b: &(u8, u8)) -> bool {
        a.0 == b.0
    }

    #[test]
    fn debounce_leading_then_trailing() {
        let t0 = Instant::now();
        let mut d = Debounce::new(W);
        assert_eq!(d.offer(t0, (1, 1), same, 1), Some((1, 1)));
        d.sent(t0);
        // Owned: kept.
        assert_eq!(d.offer(t0, (2, 1), same, 2), None);
        assert_eq!(d.due(t0), None);
        assert!(d.done(1));
        // Inside the window: kept, due at its end; the newest per key wins.
        assert_eq!(d.offer(t0 + W / 2, (1, 2), same, 3), None);
        assert_eq!(d.offer(t0 + W / 2, (2, 2), same, 4), None);
        assert_eq!(d.due(t0), Some(t0 + W));
        assert!(d.take(t0 + W / 2, 5).is_empty());
        assert_eq!(d.take(t0 + W, 6), vec![(2, 2), (1, 2)]);
        // Owned again until its own attempt is done.
        assert_eq!(d.offer(t0 + W, (3, 1), same, 7), None);
        d.sent(t0 + W);
        assert!(d.done(6));
        assert_eq!(d.due(t0 + W), Some(t0 + 2 * W));
    }

    #[test]
    fn debounce_only_the_owner_frees_the_slot() {
        let t0 = Instant::now();
        let mut d = Debounce::new(W);
        assert!(d.offer(t0, (1, 1), same, 1).is_some());
        // Another attempt's completion changes nothing.
        assert!(!d.done(9));
        assert!(d.owned_by(1));
        assert_eq!(d.offer(t0 + W, (1, 2), same, 2), None);
        assert!(d.done(1));
        assert_eq!(d.take(t0 + W, 3), vec![(1, 2)]);
        assert!(!d.done(1));
        assert!(d.done(3));
    }

    #[test]
    fn debounce_window_resets_after_quiet() {
        let t0 = Instant::now();
        let mut d = Debounce::new(W);
        assert!(d.offer(t0, (1, 1), same, 1).is_some());
        d.sent(t0);
        d.done(1);
        // A full quiet window later: leading again.
        assert_eq!(d.offer(t0 + W, (1, 2), same, 2), Some((1, 2)));
        d.sent(t0 + W);
        d.done(2);
        assert_eq!(d.offer(t0 + W + W / 10, (1, 3), same, 3), None);
    }

    #[test]
    fn debounce_dropped_attempt_keeps_the_window() {
        let t0 = Instant::now();
        let mut d = Debounce::new(W);
        assert!(d.offer(t0, (1, 1), same, 1).is_some());
        d.sent(t0);
        d.done(1);
        assert_eq!(d.offer(t0 + W / 2, (1, 2), same, 2), None);
        // The trailing one is no longer current: nothing sent, and the window still
        // counts from the leading send.
        assert_eq!(d.take(t0 + W, 3), vec![(1, 2)]);
        d.done(3);
        assert_eq!(d.due(t0 + W), None);
        assert!(d.offer(t0 + W, (1, 3), same, 4).is_some());
    }

    #[test]
    fn debounce_caps_what_it_keeps() {
        let t0 = Instant::now();
        let mut d = Debounce::new(W);
        assert!(d.offer(t0, (0, 0), same, 1).is_some());
        d.sent(t0);
        d.done(1);
        for k in 1..=40u8 {
            d.offer(t0, (k, 0), same, 100 + k as u64);
        }
        let kept = d.take(t0 + W, 2);
        assert_eq!(kept.len(), PENDING_CAP);
        assert_eq!(kept[0], (40, 0));
    }

    fn mobile(n: u8) -> (SignKey, NoiseKey) {
        let k = DeviceKeys::from_seeds(&[n; 32], &[n + 1; 32]);
        (k.sign_key(), k.noise_key())
    }

    fn chain(mobiles: &[(SignKey, NoiseKey)], creator: u8) -> RosterChain {
        use xshell_protocol::ring::SignedRoster;
        let desk = DeviceKeys::from_seeds(&[creator; 32], &[creator + 1; 32]);
        let g = SignedRoster::genesis(&desk, desk.noise_key(), "d", "wss://r.example", 1).unwrap();
        let v2 = g
            .next(&desk, 2, |d| {
                for (i, (s, n)) in mobiles.iter().enumerate() {
                    d.add(Member::new(&format!("m{i}"), Role::Mobile, *s, *n, 2));
                }
            })
            .unwrap();
        RosterChain::from_chain(vec![g, v2]).unwrap()
    }

    fn reg(noise: NoiseKey, gen: u64) -> Reg {
        Reg {
            noise_key: noise,
            blob: "xpb1.k.AAAA".into(),
            seal_key: mobile(200).1,
            triggers: PushTriggers {
                needs_you: true,
                finished: true,
            },
            registered_at: 1,
            dormant: None,
            gen,
            seq: 5,
        }
    }

    fn cfg() -> PushConfig {
        PushConfig {
            window: W,
            timeout: W,
            retry: W,
            hooks: PushHooks::default(),
        }
    }

    #[test]
    fn store_round_trips_and_prunes() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (mobile(10), mobile(20));
        let c = chain(&[a, b], 1);
        let p = Push::new(t.path().join("ring"), cfg());
        p.on_head(1, &c);
        {
            let mut st = lock(&p.inner);
            st.file.mobiles.insert(a.0, reg(a.1, 1));
            st.file.mobiles.insert(b.0, reg(b.1, 2));
        }
        p.inner.save().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(t.path().join("ring").join("push.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = Push::new(t.path().join("ring"), cfg());
        assert_eq!(lock(&again.inner).file, lock(&p.inner).file);
        assert_eq!(lock(&again.inner).next_gen, 3);
        // `b` leaves the Roster; `a`'s Noise key changes: both entries go.
        let a2 = (a.0, mobile(30).1);
        let c3 = chain(&[a2], 1);
        again.on_head(5, &c3);
        assert!(lock(&again.inner).file.mobiles.is_empty());
        // A head adopted earlier than one seen (a callback that lost a race) changes nothing.
        lock(&again.inner).file.mobiles.insert(a2.0, reg(a2.1, 3));
        again.on_head(4, &c);
        assert_eq!(lock(&again.inner).file.mobiles.len(), 1);
        assert_eq!(lock(&again.inner).chain.as_ref(), Some(&c3));
    }

    #[test]
    fn another_ring_wipes_the_store() {
        let t = tempfile::tempdir().unwrap();
        let a = mobile(10);
        let c = chain(&[a], 1);
        let p = Push::new(t.path().join("ring"), cfg());
        p.on_head(1, &c);
        lock(&p.inner).file.mobiles.insert(a.0, reg(a.1, 1));
        p.inner.save().unwrap();
        // The same Mobile in another Ring.
        let other = chain(&[a], 50);
        p.on_head(2, &other);
        // A late callback of the old Ring never switches back.
        p.on_head(1, &c);
        assert_eq!(lock(&p.inner).file.ring_id.as_ref(), Some(other.ring_id()));
        let again = Push::new(t.path().join("ring"), cfg());
        let st = lock(&again.inner);
        assert!(st.file.mobiles.is_empty());
        assert_eq!(st.file.ring_id.as_ref(), Some(other.ring_id()));
    }

    #[test]
    fn registration_is_checked_against_the_adopted_head() {
        let t = tempfile::tempdir().unwrap();
        let mk = DeviceKeys::from_seeds(&[10; 32], &[11; 32]);
        let m = Member::new("m", Role::Mobile, mk.sign_key(), mk.noise_key(), 2);
        let c = chain(&[(m.sign_key, m.noise_key)], 1);
        let p = Push::new(t.path().join("ring"), cfg());
        let sk = mobile(200).1.to_b64();
        let tr = PushTriggers {
            needs_you: true,
            finished: true,
        };
        // No head adopted yet: refused, nothing stored.
        assert!(p.register(Some(&m), "xpb1.k.AAAA", &sk, tr).is_err());
        assert!(lock(&p.inner).file.ring_id.is_none());
        p.on_head(3, &c);
        assert!(p.register(Some(&m), "xpb1.k.AAAA", &sk, tr).is_ok());
        // Another Ring adopted, then the old Ring's late callback: the store follows the
        // newer adoption and a registration for the old Ring is refused.
        let other = chain(&[], 50);
        p.on_head(4, &other);
        p.on_head(3, &c);
        assert!(p.register(Some(&m), "xpb1.k.AAAA", &sk, tr).is_err());
        assert!(lock(&p.inner).file.mobiles.is_empty());
        assert_eq!(lock(&p.inner).file.ring_id.as_ref(), Some(other.ring_id()));
    }

    #[test]
    fn unreadable_store_is_moved_aside() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        super::super::ring::Store::new(dir.clone())
            .ensure_dir()
            .unwrap();
        write_private(&dir.join("push.json"), b"{nope").unwrap();
        let p = Push::new(t.path().join("ring"), cfg());
        assert!(lock(&p.inner).file.mobiles.is_empty());
        let names: Vec<String> = std::fs::read_dir(t.path().join("ring"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("push.json.bad-")),
            "{names:?}"
        );
    }

    #[test]
    fn outcome_classes() {
        for d in DORMANT {
            assert!(!PAUSE.contains(d) && *d != RETRY);
        }
        assert_eq!(next_utc_midnight(86_400_000 + 5), 2 * 86_400_000);
        assert_eq!(next_utc_midnight(0), 86_400_000);
        let t = PushTriggers {
            needs_you: true,
            finished: false,
        };
        assert!(wants(&t, AgentStatus::NeedsYou));
        assert!(!wants(&t, AgentStatus::Finished));
        assert!(!wants(&t, AgentStatus::Working));
        assert!(!wants(&t, AgentStatus::Ended));
    }

    /// A listed Terminal without a process, running `spec`, whose current run needs you.
    fn needs_you(d: &Arc<Daemon>, spec: LaunchSpec, title: &str) -> Arc<Terminal> {
        let mut meta = Map::new();
        meta.insert("title".into(), title.into());
        let t = terminal::unresolved(
            d,
            PersistedTerminal {
                terminal: Uuid::new_v4(),
                spec,
                meta,
                cols: 80,
                rows: 24,
                created_at_ms: 0,
                leader: None,
            },
        );
        t.reset_status_for_test();
        assert_eq!(t.on_agent_event(t.run, AgentStatus::NeedsYou), Ok(true));
        d.reg.lock().unwrap().terminals.insert(t.id, t.clone());
        t
    }

    fn ev(t: &Terminal) -> AgentEvent {
        AgentEvent {
            terminal: t.id,
            run: t.run,
            status: AgentStatus::NeedsYou,
            at: 0,
        }
    }

    #[test]
    fn only_direct_agents_push_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let d = daemon(dir.path());
        let claude = |cwd: &str| LaunchSpec {
            agent: Some("claude".into()),
            shell_mode: Some("claude".into()),
            cwd: cwd.into(),
            ..Default::default()
        };
        let wrapped = needs_you(
            &d,
            LaunchSpec {
                shell_command: Some("/bin/sh".into()),
                shell_id: Some("bash".into()),
                ..claude("/wrapped")
            },
            "HIDDEN-wrapped",
        );
        let prefixed = needs_you(
            &d,
            LaunchSpec {
                launch_prefix: Some(vec!["env".into()]),
                ..claude("/prefixed")
            },
            "HIDDEN-prefixed",
        );
        let direct = needs_you(&d, claude("/direct"), "seen");
        let reg = d.reg.lock().unwrap();
        // Hook agents, listed, current run, needs you: still no push.
        for t in [&wrapped, &prefixed] {
            assert!(hook_agent(&t.spec()).is_some());
            assert!(Inner::current_in(&reg, &ev(t)).is_none());
        }
        let s = Inner::current_in(&reg, &ev(&direct)).expect("direct agent pushes");
        assert_eq!(s.project, "/direct");
        assert_eq!(s.title.as_deref(), Some("seen"));
        // Three Terminals need you; the Mobile is shown one.
        assert_eq!(s.needs_you, 1);
    }
}

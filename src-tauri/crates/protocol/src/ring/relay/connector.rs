//! The Connector: keeps one device connected to its Ring's Relay. [`RingClient`] dials once
//! and stops at the first loss; the Connector dials again with the shared backoff
//! ([`crate::backoff`]), follows the Roster when a new head names another Relay, finishes a
//! Relay move it owes the old Relay, and says goodbye when it stops. The Daemon and the
//! Desktop use it as is; the Mobile can use it through UniFFI later.
//!
//! **One attempt at a time.** One thread owns the lifecycle and makes every connect attempt
//! itself, so an attempt never starts before the one it supersedes has ended. Every attempt
//! has a generation: callbacks of an attempt that was superseded or stopped are ignored.
//! [`Connector::stop`] waits for an attempt in flight (bounded by the client's connect
//! timeout) and then says goodbye on it if it authenticated, so a successor never connects
//! while an older socket could still come up.
//!
//! **Relay moves.** A Desktop that changes the Relay URL owes the old Relay the new
//! version, or the devices still connected there never learn of the move. Until the old
//! Relay acknowledged it ([`MoveJob`]), the Connector stays on the old Relay, publishes the
//! versions it lacks, and only then moves. It retries in batches of
//! [`ConnectorConfig::move_attempts`] with the backoff in between, for as long as it runs,
//! and reports [`MoveState::Failed`] after each failed batch; it never moves without the
//! acknowledgement. The owner keeps the job on disk and passes it again at its next start.

use super::super::chain::RosterChain;
use super::super::{RingError, RosterError, SignKey, SignedRoster};
use super::client::{MemberStatus, PushRequest, RingClient, RingClientConfig, RingEvents, Ticket};
use super::wire::{close, ByeReason, CloseReason, ErrorCode, MemberPresence};
use crate::backoff::Backoff;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct ConnectorConfig {
    /// The chain this device trusts, its signer, TLS and timeouts. The chain's head names
    /// the Relay.
    pub client: RingClientConfig,
    /// The backoff schedule's unit (1 s in production).
    pub backoff_unit: Duration,
    /// A connection up this long resets the backoff.
    pub stable_after: Duration,
    /// A Relay move still owed to the old Relay. `None`: nothing owed.
    pub pending_move: Option<MoveJob>,
    /// Attempts at the old Relay per batch; [`MoveState::Failed`] after each failed batch.
    pub move_attempts: u32,
}

impl ConnectorConfig {
    pub fn new(client: RingClientConfig) -> Self {
        ConnectorConfig {
            client,
            backoff_unit: Duration::from_secs(1),
            stable_after: Duration::from_secs(10),
            pending_move: None,
            move_attempts: 5,
        }
    }
}

/// Where the Relay connection stands.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkState {
    /// Dialing; `attempt` counts the failures since the last stable connection.
    Connecting { attempt: u32 },
    /// Authenticated and synced. `limited`: a Hosted Relay without a Hosted entitlement.
    Connected { limited: bool },
    /// The last attempt failed or the connection dropped; the next attempt starts after
    /// `retry_in` (from when this state was entered).
    Waiting { retry_in: Duration, error: String },
    /// No more attempts: stopped by the owner (`error: None`), or this device is no longer
    /// in the Ring. A new chain ([`Connector::set_chain`]) starts it again.
    Stopped { error: Option<String> },
}

/// A Relay move owed to the old Relay, keyed by that Relay and the version it must get:
/// the versions after `from` up to `target` are published on `source` (the Relay that
/// version `from` names).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MoveJob {
    pub source: String,
    pub from: u64,
    pub target: u64,
}

impl MoveJob {
    /// The job for moving off the Relay that version `from` of `chain` names, to its head.
    pub fn new(chain: &RosterChain, from: u64) -> Option<MoveJob> {
        let old = chain.get(from)?;
        Some(MoveJob {
            source: old.roster().relay_url.clone(),
            from,
            target: chain.head().version(),
        })
    }

    /// Whether this job still means something for `chain`: version `from` names `source`,
    /// `target` is in the chain and newer, and names another Relay.
    fn live(&self, chain: &RosterChain) -> bool {
        match (chain.get(self.from), chain.get(self.target)) {
            (Some(old), Some(new)) => {
                old.roster().relay_url == self.source
                    && self.target > self.from
                    && new.roster().relay_url != self.source
            }
            _ => false,
        }
    }
}

/// A Relay move owed to the old Relay.
#[derive(Debug, Clone, PartialEq)]
pub enum MoveState {
    /// Publishing on the old Relay; `failures` in this batch so far.
    Moving {
        job: MoveJob,
        failures: u32,
        error: Option<String>,
    },
    /// The old Relay acknowledged `job`; the owner clears that job (and only that one).
    Done { job: MoveJob },
    /// A whole batch of attempts failed. The Connector stays on (or keeps dialing) the old
    /// Relay and tries again after the backoff.
    Failed { job: MoveJob, error: String },
}

/// What a Connector reports. Called on the Connector's or the client's threads, never under
/// a Connector lock, and only for the current attempt.
pub trait ConnectorEvents: Send + Sync {
    fn state(&self, _state: &LinkState) {}
    /// A newer verified chain (from the Relay, or a version this device published). Persist
    /// all of it.
    fn roster(&self, _chain: &RosterChain) {}
    fn presence(&self, _key: SignKey, _presence: &MemberPresence) {}
    fn moved(&self, _state: &MoveState) {}
    /// An envelope from a member of the trusted head (its `from` is the Relay's word until a
    /// Noise session authenticates it).
    fn envelope(&self, _from: SignKey, _payload: Vec<u8>) {}
    /// An `error` the Relay sent that answers no request (`offline`, `unknown_recipient`,
    /// `entitlement_required`, …), `to` naming the envelope's addressee. Every `quota`
    /// refusal comes here too, also one that answers a request (an
    /// [`Connector::put_entitlement`], say): its caller gets the error as its result first,
    /// so one place can show the Ring's quota notice.
    fn error(&self, _code: &ErrorCode, _to: Option<SignKey>) {}
    /// The Relay's entitlement slot as this connection sees it: called after every
    /// `Connected` report (with the token from `welcome`, or the latest broadcast) and on
    /// every `entitlement` broadcast. Calls from the Connector's thread and the client's may
    /// cross, and the same token may come again: the live slot is [`Connector::entitlement`].
    /// A broadcast arrives on the client's IO thread, so this must not wait on the Relay
    /// (no [`Connector::put_entitlement`] from here).
    fn entitlement(&self, _token: Option<&str>) {}
}

struct Stop {
    /// `None`: drop the connection without a goodbye.
    reason: Option<ByeReason>,
}

struct St {
    chain: RosterChain,
    pending_move: Option<MoveJob>,
    move_state: Option<MoveState>,
    /// Failed attempts at the old Relay for the move owed now.
    move_failures: u32,
    /// Whether a batch of that move failed already (Failed stays reported until Done).
    move_failed_batch: bool,
    /// The current attempt's generation. Bumped for every attempt and when an attempt is
    /// abandoned, so the callbacks of older ones are ignored.
    gen: u64,
    client: Option<Arc<RingClient>>,
    /// The current attempt's connection ended.
    closed: Option<CloseReason>,
    link: LinkState,
    /// `set_chain` since the worker last looked.
    dirty: bool,
    kick: bool,
    /// Limited state changed (entitlement); the worker re-reports `Connected`.
    refresh: bool,
    stop: Option<Stop>,
    /// The foreground state a Mobile last set: sent again on every new connection.
    foreground: Option<bool>,
}

struct Inner {
    st: Mutex<St>,
    cv: Condvar,
    events: Arc<dyn ConnectorEvents>,
    base: RingClientConfig,
    backoff_unit: Duration,
    stable_after: Duration,
    move_attempts: u32,
}

/// Keeps a device connected to its Ring's Relay; see the module docs.
pub struct Connector {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn lock(m: &Mutex<St>) -> MutexGuard<'_, St> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The chain `theirs` makes of `ours` if it is newer and extends it.
fn merge(ours: &RosterChain, theirs: &RosterChain) -> Option<RosterChain> {
    if theirs.ring_id() != ours.ring_id() || theirs.head().version() <= ours.head().version() {
        return None;
    }
    match ours.extended(theirs.since(ours.head().version())) {
        Ok((c, a)) if a.added > 0 => Some(c),
        _ => None,
    }
}

/// The first `version` versions of `chain`.
fn truncated(chain: &RosterChain, version: u64) -> Option<RosterChain> {
    let n = usize::try_from(version).ok()?;
    let v = chain.versions().get(..n)?;
    RosterChain::from_chain(v.to_vec()).ok()
}

/// The pending move, if it still means something for `chain`.
fn live_move(chain: &RosterChain, pending: Option<&MoveJob>) -> Option<MoveJob> {
    pending.filter(|j| j.live(chain)).cloned()
}

/// Whether the Relay said this device is no longer in the Ring: no point in dialing again.
fn removed_close(why: &CloseReason) -> bool {
    match why {
        CloseReason::Relay { close_code, error } => {
            matches!(error, Some(ErrorCode::Removed) | Some(ErrorCode::NotMember))
                || *close_code == Some(close::REMOVED)
        }
        _ => false,
    }
}

/// Whether the Relay `c` is connected to holds exactly `next` (as far as `c` has seen).
fn holds(c: &RingClient, next: &SignedRoster) -> bool {
    c.chain()
        .get(next.version())
        .is_some_and(|r| r.token() == next.token())
}

fn removed_error(e: &RingError) -> bool {
    match e {
        RingError::Relay { code, .. } => matches!(code, ErrorCode::NotMember | ErrorCode::Removed),
        RingError::Closed(why) => removed_close(why),
        RingError::Invalid(m) => m.contains("not in its Roster"),
        _ => false,
    }
}

struct Attempt {
    inner: Weak<Inner>,
    gen: u64,
}

impl RingEvents for Attempt {
    fn envelope(&self, from: SignKey, payload: Vec<u8>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.is_current(self.gen) {
            inner.events.envelope(from, payload);
        }
    }

    fn presence(&self, key: SignKey, presence: MemberPresence) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.is_current(self.gen) {
            inner.events.presence(key, &presence);
        }
    }

    fn roster(&self, _roster: &SignedRoster) {}

    fn chain(&self, chain: &RosterChain) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let merged = {
            let mut st = lock(&inner.st);
            if st.gen != self.gen || st.stop.is_some() {
                return;
            }
            let Some(m) = merge(&st.chain, chain) else {
                return;
            };
            let moved = m.head().roster().relay_url != st.chain.head().roster().relay_url;
            st.chain = m.clone();
            if moved {
                // The Ring moved: follow it.
                st.dirty = true;
                inner.cv.notify_all();
            }
            m
        };
        inner.events.roster(&merged);
    }

    fn roster_rejected(&self, _error: super::super::RosterError) {}

    fn error(&self, code: ErrorCode, to: Option<SignKey>, _detail: Option<String>) {
        if code == ErrorCode::EntitlementRequired {
            self.refresh();
        }
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.is_current(self.gen) {
            inner.events.error(&code, to);
        }
    }

    fn entitlement(&self, token: Option<&str>) {
        self.refresh();
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.is_current(self.gen) {
            inner.events.entitlement(token);
        }
    }

    fn closed(&self, why: CloseReason) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut st = lock(&inner.st);
        if st.gen == self.gen {
            st.closed = Some(why);
            inner.cv.notify_all();
        }
    }
}

impl Attempt {
    fn refresh(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut st = lock(&inner.st);
        if st.gen == self.gen {
            st.refresh = true;
            inner.cv.notify_all();
        }
    }
}

/// What ended a wait.
enum Woke {
    Stop,
    Dirty,
    Kick,
    Closed(CloseReason),
    Refresh,
    Timeout,
}

impl Inner {
    fn is_current(&self, gen: u64) -> bool {
        let st = lock(&self.st);
        st.gen == gen && st.stop.is_none()
    }

    fn set_link(&self, link: LinkState) {
        lock(&self.st).link = link.clone();
        self.events.state(&link);
    }

    /// Reports `Connected` for `c`, then the slot it holds (only for the current attempt).
    fn connected(&self, c: &RingClient, gen: u64) {
        self.set_link(LinkState::Connected {
            limited: c.limited(),
        });
        if self.is_current(gen) {
            self.events.entitlement(c.entitlement().as_deref());
        }
    }

    fn set_move(&self, m: MoveState) {
        lock(&self.st).move_state = Some(m.clone());
        self.events.moved(&m);
    }

    /// Waits until something the worker must act on, or `until`. `closed` and `refresh`
    /// count only while connected.
    fn wait(&self, until: Option<Instant>, connected: bool) -> Woke {
        let mut st = lock(&self.st);
        loop {
            if st.stop.is_some() {
                return Woke::Stop;
            }
            if st.dirty {
                st.dirty = false;
                return Woke::Dirty;
            }
            if connected {
                if let Some(why) = st.closed.take() {
                    return Woke::Closed(why);
                }
                if std::mem::take(&mut st.refresh) {
                    return Woke::Refresh;
                }
            } else if std::mem::take(&mut st.kick) {
                return Woke::Kick;
            }
            match until {
                None => st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
                Some(t) => {
                    let left = t.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Woke::Timeout;
                    }
                    st = self
                        .cv
                        .wait_timeout(st, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
    }

    /// Ends the current attempt's connection without a goodbye and fences its callbacks.
    fn abandon(&self) {
        let c = {
            let mut st = lock(&self.st);
            st.gen += 1;
            st.closed = None;
            st.client.take()
        };
        drop(c);
    }

    /// Publishes every version of the trusted chain after `from` (up to `to`) on `c`,
    /// oldest first.
    fn publish_since(&self, c: &RingClient, from: u64, to: Option<u64>) -> Result<(), RingError> {
        let versions: Vec<SignedRoster> = lock(&self.st)
            .chain
            .since(from)
            .iter()
            .filter(|r| to.is_none_or(|t| r.version() <= t))
            .cloned()
            .collect();
        for r in &versions {
            match c.publish_roster(r) {
                Ok(()) => {}
                // The Relay already holds it (another device published it first).
                Err(RingError::Roster(super::super::RosterError::Stale)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The stop request, taken: says goodbye on `c` if there is one.
    fn finish(&self, c: Option<Arc<RingClient>>) {
        let reason = {
            let mut st = lock(&self.st);
            st.gen += 1;
            st.client = None;
            st.stop.as_mut().and_then(|s| s.reason.take())
        };
        if let (Some(c), Some(reason)) = (c, reason) {
            let _ = c.goodbye(reason);
        }
    }

    fn run(self: Arc<Self>) {
        let mut backoff = Backoff::default();
        'attempt: loop {
            // One attempt: what to dial, under a new generation.
            let (target, moving, gen) = {
                let mut st = lock(&self.st);
                if st.stop.is_some() {
                    break;
                }
                st.dirty = false;
                st.kick = false;
                st.closed = None;
                st.refresh = false;
                st.pending_move = live_move(&st.chain, st.pending_move.as_ref());
                let moving = st.pending_move.clone();
                let target = match &moving {
                    Some(j) => truncated(&st.chain, j.from),
                    None => Some(st.chain.clone()),
                };
                st.gen += 1;
                (target, moving, st.gen)
            };
            let Some(target) = target else {
                // Cannot happen for a live move; drop it rather than loop.
                lock(&self.st).pending_move = None;
                continue;
            };
            self.set_link(LinkState::Connecting {
                attempt: backoff.failures,
            });
            // The Relay this attempt dials: the target head's, whatever the chain says later.
            let dialed = target.head().roster().relay_url.clone();
            let mut cfg = self.base.clone();
            cfg.chain = target;
            let events = Arc::new(Attempt {
                inner: Arc::downgrade(&self),
                gen,
            });
            let r = RingClient::connect(cfg, events);
            let c = {
                let mut st = lock(&self.st);
                match r {
                    Ok(c) => {
                        let c = Arc::new(c);
                        if st.stop.is_some() {
                            drop(st);
                            self.finish(Some(c));
                            break;
                        }
                        st.client = Some(c.clone());
                        if let Some(f) = st.foreground {
                            // Fire and forget, as every `state`.
                            let _ = c.set_foreground(f);
                        }
                        Ok(c)
                    }
                    Err(e) => {
                        if st.stop.is_some() {
                            drop(st);
                            self.finish(None);
                            break;
                        }
                        Err(e)
                    }
                }
            };
            let c = match c {
                Ok(c) => c,
                Err(e) if removed_error(&e) && moving.is_none() => {
                    self.set_link(LinkState::Stopped {
                        error: Some(e.to_string()),
                    });
                    match self.wait(None, false) {
                        Woke::Stop => {
                            self.finish(None);
                            break;
                        }
                        _ => continue,
                    }
                }
                Err(e) => {
                    if let Some(j) = &moving {
                        self.move_failed(j, e.to_string());
                    }
                    let retry_in = backoff.failed(self.backoff_unit);
                    if self.backoff_wait(retry_in, e.to_string()) {
                        break;
                    }
                    continue;
                }
            };
            let up_at = Instant::now();

            if let Some(job) = moving {
                // Stay on the old Relay, reachable for the devices there, until it
                // acknowledged the move.
                self.connected(&c, gen);
                let mut tries = Backoff::default();
                loop {
                    let r = self.publish_since(&c, job.from, Some(job.target));
                    match r {
                        Ok(()) => {
                            // A stop that came meanwhile ends here, with the goodbye.
                            if lock(&self.st).stop.is_some() {
                                {
                                    let mut st = lock(&self.st);
                                    if st.pending_move.as_ref() == Some(&job) {
                                        st.pending_move = None;
                                    }
                                }
                                self.set_move(MoveState::Done { job });
                                self.finish(Some(c));
                                break 'attempt;
                            }
                            self.abandon();
                            {
                                let mut st = lock(&self.st);
                                if st.pending_move.as_ref() == Some(&job) {
                                    st.pending_move = None;
                                }
                                st.move_failures = 0;
                                st.move_failed_batch = false;
                            }
                            self.set_move(MoveState::Done { job });
                            backoff = Backoff::default();
                            continue 'attempt;
                        }
                        Err(e) => {
                            self.move_failed(&job, e.to_string());
                            let retry_in = tries.failed(self.backoff_unit);
                            match self.wait(Some(Instant::now() + retry_in), true) {
                                Woke::Stop => {
                                    self.finish(Some(c));
                                    break 'attempt;
                                }
                                Woke::Closed(_) | Woke::Dirty => {
                                    self.abandon();
                                    continue 'attempt;
                                }
                                Woke::Refresh | Woke::Kick | Woke::Timeout => {}
                            }
                        }
                    }
                }
            }

            // Versions set while the connect ran.
            if c.chain().head().version() < lock(&self.st).chain.head().version() {
                let _ = self.sync(&c, &dialed);
            }
            self.connected(&c, gen);
            loop {
                match self.wait(None, true) {
                    Woke::Stop => {
                        self.finish(Some(c));
                        break 'attempt;
                    }
                    Woke::Refresh => self.connected(&c, gen),
                    Woke::Kick | Woke::Timeout => {}
                    Woke::Dirty => {
                        if !self.sync(&c, &dialed) {
                            // Another Ring or another Relay: dial again, at once.
                            self.abandon();
                            continue 'attempt;
                        }
                    }
                    Woke::Closed(why) => {
                        self.abandon();
                        if removed_close(&why) {
                            self.set_link(LinkState::Stopped {
                                error: Some(why.to_string()),
                            });
                            match self.wait(None, false) {
                                Woke::Stop => {
                                    self.finish(None);
                                    break 'attempt;
                                }
                                _ => continue 'attempt,
                            }
                        }
                        let retry_in =
                            backoff.ended(up_at.elapsed(), self.stable_after, self.backoff_unit);
                        if !retry_in.is_zero() && self.backoff_wait(retry_in, why.to_string()) {
                            break 'attempt;
                        }
                        continue 'attempt;
                    }
                }
            }
        }
        self.set_link(LinkState::Stopped { error: None });
    }

    /// Brings the Relay `c` is connected to up to the trusted chain. `false`: the chain is
    /// of another Ring, names another Relay, or owes a move, so `c` is the wrong connection.
    fn sync(&self, c: &RingClient, dialed: &str) -> bool {
        let (ours, owed) = {
            let st = lock(&self.st);
            let owed = live_move(&st.chain, st.pending_move.as_ref());
            (st.chain.clone(), owed)
        };
        let theirs = c.chain();
        if ours.ring_id() != theirs.ring_id()
            || ours.head().roster().relay_url != dialed
            || owed.is_some()
        {
            return false;
        }
        let from = theirs.head().version();
        if ours.head().version() > from && self.publish_since(c, from, None).is_err() {
            // The next connect stages what the Relay lacks.
            return false;
        }
        true
    }

    /// One failed attempt at the old Relay. A whole batch failed: `Failed`, which stays
    /// reported (with the latest error) until the move is done.
    fn move_failed(&self, job: &MoveJob, error: String) {
        let (failures, failed) = {
            let mut st = lock(&self.st);
            st.move_failures += 1;
            if st.move_failures >= self.move_attempts {
                st.move_failures = 0;
                st.move_failed_batch = true;
            }
            (st.move_failures, st.move_failed_batch)
        };
        let job = job.clone();
        if failed {
            self.set_move(MoveState::Failed { job, error });
        } else {
            self.set_move(MoveState::Moving {
                job,
                failures,
                error: Some(error),
            });
        }
    }

    /// Reports `Waiting` and waits out `retry_in`. `true`: stopped.
    fn backoff_wait(&self, retry_in: Duration, error: String) -> bool {
        self.set_link(LinkState::Waiting { retry_in, error });
        match self.wait(Some(Instant::now() + retry_in), false) {
            Woke::Stop => {
                self.finish(None);
                true
            }
            _ => false,
        }
    }
}

impl Connector {
    /// Starts connecting at once, on a thread of its own.
    pub fn start(
        cfg: ConnectorConfig,
        events: Arc<dyn ConnectorEvents>,
    ) -> std::io::Result<Connector> {
        let move_state =
            live_move(&cfg.client.chain, cfg.pending_move.as_ref()).map(|job| MoveState::Moving {
                job,
                failures: 0,
                error: None,
            });
        let inner = Arc::new(Inner {
            st: Mutex::new(St {
                chain: cfg.client.chain.clone(),
                pending_move: cfg.pending_move,
                move_state,
                move_failures: 0,
                move_failed_batch: false,
                gen: 0,
                client: None,
                closed: None,
                link: LinkState::Connecting { attempt: 0 },
                dirty: false,
                kick: false,
                refresh: false,
                stop: None,
                foreground: None,
            }),
            cv: Condvar::new(),
            events,
            base: cfg.client,
            backoff_unit: cfg.backoff_unit,
            stable_after: cfg.stable_after,
            move_attempts: cfg.move_attempts.max(1),
        });
        let i = inner.clone();
        let thread = std::thread::Builder::new()
            .name("ring-connector".into())
            .spawn(move || i.run())?;
        Ok(Connector {
            inner,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Trust `chain` from now on (it replaces the one held). Versions the Relay lacks are
    /// published; a head naming another Relay, or another Ring, makes the Connector dial
    /// again. `pending_move`: a Relay move now owed to the old Relay (see
    /// [`ConnectorConfig::pending_move`]). Never blocks.
    pub fn set_chain(&self, chain: RosterChain, pending_move: Option<MoveJob>) {
        let m = {
            let mut st = lock(&self.inner.st);
            if st.chain.ring_id() != chain.ring_id() {
                st.pending_move = None;
                st.move_state = None;
            }
            st.chain = chain;
            let mut m = None;
            if let Some(job) = live_move(&st.chain, pending_move.as_ref()) {
                if st.pending_move.as_ref() != Some(&job) {
                    st.move_failures = 0;
                    st.move_failed_batch = false;
                }
                st.pending_move = Some(job.clone());
                let s = MoveState::Moving {
                    job,
                    failures: 0,
                    error: None,
                };
                st.move_state = Some(s.clone());
                m = Some(s);
            }
            st.dirty = true;
            self.inner.cv.notify_all();
            m
        };
        if let Some(m) = m {
            self.inner.events.moved(&m);
        }
    }

    /// Uploads `next` on the current connection and waits for the Relay's answer. Best
    /// effort: `Err` when not connected (the next connect stages it anyway once it is in the
    /// chain given to [`Connector::set_chain`]). A caller that must know the Relay has it
    /// uses [`Connector::publish_until`].
    pub fn publish(&self, next: &SignedRoster) -> Result<(), RingError> {
        let c = lock(&self.inner.st).client.clone();
        match c {
            Some(c) if !c.is_closed() => c.publish_roster(next),
            _ => Err(RingError::Closed(CloseReason::Local)),
        }
    }

    /// Gets `next` to the Relay by `until`: done once the Relay holds exactly this version
    /// (same bytes), whether this call uploaded it or the worker's sync (or another device)
    /// did. Unlike [`Connector::publish`], it waits for a connection (kicking a Connector in
    /// backoff once), for the versions before `next` that the worker is still publishing
    /// (when `next` is in the trusted chain), and retries an upload the connection lost.
    /// Every wait, an unanswered upload included, ends at `until`, when the Connector stops,
    /// or when `cancel` is set (`Err(Closed)`). `Err` otherwise: the last error seen
    /// (`Closed` when never connected), or the Relay's refusal of different bytes.
    pub fn publish_until(
        &self,
        next: &SignedRoster,
        until: Instant,
        cancel: Option<&AtomicBool>,
    ) -> Result<(), RingError> {
        let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Acquire));
        let halted = || cancelled() || lock(&self.inner.st).stop.is_some();
        let closed = || RingError::Closed(CloseReason::Local);
        let mut kicked = false;
        let mut last: Option<RingError> = None;
        loop {
            if halted() {
                return Err(closed());
            }
            let c = lock(&self.inner.st)
                .client
                .clone()
                .filter(|c| !c.is_closed());
            match c {
                Some(c) if holds(&c, next) => {
                    // Recheck: a cancelled pairing does not report success.
                    return if halted() { Err(closed()) } else { Ok(()) };
                }
                Some(c) => {
                    if Instant::now() >= until {
                        return Err(last.unwrap_or_else(closed));
                    }
                    let r = c
                        .publish_roster_start(next)
                        .and_then(|t| t.wait_until(until, || halted() || c.is_closed()));
                    if halted() {
                        return Err(closed());
                    }
                    match r {
                        Ok(()) => return Ok(()),
                        Err(_) if holds(&c, next) => return Ok(()),
                        Err(e) if self.retryable(&e, next) => last = Some(e),
                        Err(e) => return Err(e),
                    }
                }
                None => {
                    if !kicked {
                        kicked = true;
                        self.kick();
                    }
                }
            }
            if Instant::now() >= until {
                return Err(last.unwrap_or_else(closed));
            }
            let left = until.saturating_duration_since(Instant::now());
            std::thread::sleep(left.min(Duration::from_millis(25)));
        }
    }

    /// Whether [`Connector::publish_until`] tries again after `e`: the connection was lost
    /// or the answer did not come, the Relay holds a newer version this client has not seen
    /// yet (the local check then decides), or the client lacks versions before `next` that
    /// the worker publishes in order (only when `next` is in the trusted chain).
    fn retryable(&self, e: &RingError, next: &SignedRoster) -> bool {
        match e {
            RingError::Closed(_) | RingError::Timeout => true,
            RingError::Relay {
                code: ErrorCode::RosterStale,
                ..
            } => true,
            RingError::Roster(RosterError::Gap) => lock(&self.inner.st)
                .chain
                .get(next.version())
                .is_some_and(|r| r.token() == next.token()),
            _ => false,
        }
    }

    /// Sends one envelope on the current connection. `Err(Backpressure)` when its queue is
    /// full, `Err(Closed)` when not connected. Delivery failures come back as
    /// [`ConnectorEvents::error`].
    pub fn send(&self, to: &SignKey, payload: &[u8]) -> Result<(), RingError> {
        let c = lock(&self.inner.st).client.clone();
        match c {
            Some(c) if !c.is_closed() => c.send(to, payload),
            _ => Err(RingError::Closed(CloseReason::Local)),
        }
    }

    /// What the current connection's Relay lists in `welcome.caps`; `None` while not
    /// connected.
    pub fn relay_caps(&self) -> Option<Vec<String>> {
        let c = lock(&self.inner.st).client.clone()?;
        (!c.is_closed()).then(|| c.relay_caps())
    }

    /// The current connection's generation: it changes with every new connection. `None`
    /// while not connected.
    pub fn connection(&self) -> Option<u64> {
        let st = lock(&self.inner.st);
        st.client
            .as_ref()
            .filter(|c| !c.is_closed())
            .map(|_| st.gen)
    }

    /// A Mobile entering or leaving the foreground: sent now if connected, and again on
    /// every connection from now on. Never blocks.
    pub fn set_foreground(&self, foreground: bool) {
        let c = {
            let mut st = lock(&self.inner.st);
            st.foreground = Some(foreground);
            st.client.clone()
        };
        if let Some(c) = c.filter(|c| !c.is_closed()) {
            let _ = c.set_foreground(foreground);
        }
    }

    /// Asks the Relay to forward one push (see [`RingClient::push`]); `Err(Closed)` when not
    /// connected. Nothing is queued for a later connection.
    pub fn push(&self, req: &PushRequest, timeout: Duration) -> Result<(), RingError> {
        let c = lock(&self.inner.st).client.clone();
        match c {
            Some(c) if !c.is_closed() => c.push(req, timeout),
            _ => Err(RingError::Closed(CloseReason::Local)),
        }
    }

    /// [`Connector::push`] in two steps (see [`RingClient::push_start`]): never blocks.
    pub fn push_start(&self, req: &PushRequest) -> Result<Ticket, RingError> {
        let c = lock(&self.inner.st).client.clone();
        match c {
            Some(c) if !c.is_closed() => c.push_start(req),
            _ => Err(RingError::Closed(CloseReason::Local)),
        }
    }

    /// The token in the Relay's entitlement slot as the current connection last saw it (from
    /// `welcome` or a broadcast); `None` while not connected or when the slot is empty.
    pub fn entitlement(&self) -> Option<String> {
        let c = lock(&self.inner.st).client.clone()?;
        (!c.is_closed()).then(|| c.entitlement()).flatten()
    }

    /// Puts a Push Gateway entitlement token in the Ring's slot on the Relay and waits for
    /// its answer (see [`RingClient::put_entitlement`]). A Hosted Relay acknowledges a valid
    /// token that is not better than the one it holds without storing it (protocol section
    /// 12): the slot then stays as [`Connector::entitlement`] reports it. `Err(Closed)` when
    /// not connected; nothing is queued for a later connection.
    pub fn put_entitlement(&self, token: &str) -> Result<(), RingError> {
        let c = lock(&self.inner.st).client.clone();
        match c {
            Some(c) if !c.is_closed() => c.put_entitlement(token),
            _ => Err(RingError::Closed(CloseReason::Local)),
        }
    }

    /// [`Connector::put_entitlement`] in two steps: queues the `entitlement.put` on the
    /// current connection without blocking (so a caller can do it under its own locks, right
    /// after its last check) and returns the ticket its answer comes through. `Err(Closed)`
    /// when not connected.
    pub fn put_entitlement_start(&self, token: &str) -> Result<Ticket, RingError> {
        let c = lock(&self.inner.st).client.clone();
        match c {
            Some(c) if !c.is_closed() => c.put_entitlement_start(token),
            _ => Err(RingError::Closed(CloseReason::Local)),
        }
    }

    /// Whether the current connection is up and this Ring's envelopes are routed (not a
    /// limited session).
    pub fn routing(&self) -> bool {
        let c = lock(&self.inner.st).client.clone();
        c.is_some_and(|c| !c.is_closed() && !c.limited())
    }

    /// Every member of the trusted head with what the Relay says about it; `None` while not
    /// connected.
    pub fn members(&self) -> Option<Vec<MemberStatus>> {
        let c = lock(&self.inner.st).client.clone()?;
        (!c.is_closed()).then(|| c.members())
    }

    pub fn chain(&self) -> RosterChain {
        lock(&self.inner.st).chain.clone()
    }

    pub fn state(&self) -> LinkState {
        lock(&self.inner.st).link.clone()
    }

    pub fn move_state(&self) -> Option<MoveState> {
        lock(&self.inner.st).move_state.clone()
    }

    /// Retry now instead of waiting out the backoff (or a stop for removal). A live
    /// connection is left alone.
    pub fn kick(&self) {
        let mut st = lock(&self.inner.st);
        st.kick = true;
        self.inner.cv.notify_all();
    }

    /// Stops for good: says goodbye with `reason` if connected, after waiting for an attempt
    /// in flight to finish (bounded by the connect timeout). Idempotent.
    pub fn stop(&self, reason: ByeReason) {
        self.stop_with(Some(reason));
    }

    /// Stops without a goodbye: the Relay reports this device unreachable.
    pub fn abandon(&self) {
        self.stop_with(None);
    }

    fn stop_with(&self, reason: Option<ByeReason>) {
        {
            let mut st = lock(&self.inner.st);
            if st.stop.is_none() {
                st.stop = Some(Stop { reason });
            }
            self.inner.cv.notify_all();
        }
        let Some(handle) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        // The worker ends within the connect timeout (an attempt in flight) plus the
        // goodbye; past that it is left to finish on its own, fenced by the stop flag.
        let t = self.inner.base.timeouts;
        let deadline = Instant::now() + t.connect + t.bye + Duration::from_secs(1);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

impl Drop for Connector {
    fn drop(&mut self) {
        let mut st = lock(&self.inner.st);
        if st.stop.is_none() {
            st.stop = Some(Stop { reason: None });
        }
        self.inner.cv.notify_all();
    }
}

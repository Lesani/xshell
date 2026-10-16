//! The Connector: keeps one device connected to its Ring's Relay. [`RingClient`] dials once
//! and stops at the first loss; the Connector dials again with the shared backoff
//! ([`crate::backoff`]), follows the Roster when a new head names another Relay, says
//! farewell to the old Relays it left, and says goodbye when it stops. The Daemon and the
//! Desktop use it as is; the Mobile can use it through UniFFI later.
//!
//! **One attempt at a time.** One thread owns the lifecycle and makes every connect attempt
//! itself, so an attempt never starts before the one it supersedes has ended. Every attempt
//! has a generation: callbacks of an attempt that was superseded or stopped are ignored.
//! [`Connector::stop`] waits for an attempt in flight (bounded by the client's connect
//! timeout) and then says goodbye on it if it authenticated, so a successor never connects
//! while an older socket could still come up.
//!
//! **Following a move.** The connection is always to the Relay the trusted head names. A head
//! from the Relay that names another Relay is followed, but first the rest of what this
//! Relay holds is fetched (`roster.get` until nothing new comes): a Relay that commits several
//! versions at once broadcasts each of them, and a device must not dial an intermediate
//! Relay when the newest head names a later one.
//!
//! **Farewells** (ADR-0012). A Desktop that changes the Relay URL owes each old Relay a
//! [`Farewell`], or the devices still connected there never learn of the move. The Connector
//! never waits for one: the link moves at once, and a worker of its own sends the farewells
//! in the background (see `farewell.rs`), each with the newest head, retried with a backoff
//! for as long as the Connector runs. The owner keeps them on disk, passes them in
//! [`ConnectorConfig::farewells`] and [`Connector::set_farewells`], and drops one when its
//! [`FarewellState::Done`] comes. [`ConnectorEvents::held`] tells the owner which Relays hold
//! the Ring: no farewell is owed to one that never did.

use super::super::chain::RosterChain;
use super::super::{RingError, RosterError, SignKey, SignedRoster};
use super::client::{MemberStatus, PushRequest, RingClient, RingClientConfig, RingEvents, Ticket};
use super::farewell::{Farewell, FarewellState, FwJob};
use super::wire::{close, ByeReason, CloseReason, ErrorCode, MemberPresence};
use crate::backoff::Backoff;
use std::collections::BTreeMap;
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
    /// The farewells owed to old Relays (at most one per Relay counts); those the chain no
    /// longer owes are dropped.
    pub farewells: Vec<Farewell>,
    /// The first wait after a failed farewell (5 s in production); it doubles with each
    /// further failure …
    pub farewell_backoff_unit: Duration,
    /// … up to this (15 min in production), and stays there.
    pub farewell_backoff_max: Duration,
}

impl ConnectorConfig {
    pub fn new(client: RingClientConfig) -> Self {
        ConnectorConfig {
            client,
            backoff_unit: Duration::from_secs(1),
            stable_after: Duration::from_secs(10),
            farewells: Vec::new(),
            farewell_backoff_unit: Duration::from_secs(5),
            farewell_backoff_max: Duration::from_secs(15 * 60),
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

/// What a Connector reports. Called on the Connector's or the client's threads, never under
/// a Connector lock, and only for the current attempt.
pub trait ConnectorEvents: Send + Sync {
    fn state(&self, _state: &LinkState) {}
    /// A newer verified chain (from the Relay, or a version this device published). Persist
    /// all of it.
    fn roster(&self, _chain: &RosterChain) {}
    fn presence(&self, _key: SignKey, _presence: &MemberPresence) {}
    /// A farewell's new state (see [`FarewellState`]). Never called from
    /// [`Connector::set_farewells`] or [`Connector::set_chain`] themselves.
    fn farewell(&self, _state: &FarewellState) {}
    /// `relay` holds this Ring, at least up to `version`: the main connection authenticated
    /// there (`version` is then the newest the Relay holds that names `relay`), a farewell was
    /// acknowledged there, or it refused this Ring for quota (only a Relay that holds a Ring
    /// counts its traffic). A report can cross a change of the chain: the owner fences it
    /// by its own state.
    fn held(&self, _relay: &str, _version: u64) {}
    /// An envelope from a member of the trusted head (its `from` is the Relay's word until a
    /// Noise session authenticates it).
    fn envelope(&self, _from: SignKey, _payload: Vec<u8>) {}
    /// An `error` the Relay sent that answers no request (`offline`, `unknown_recipient`,
    /// `entitlement_required`, …), `to` naming the envelope's addressee. Every `quota`
    /// refusal comes here too, also one that answers a request (an
    /// [`Connector::put_entitlement`], say): its caller gets the error as its result first,
    /// so one place can show the Ring's quota notice. Quota refusals come through
    /// [`ConnectorEvents::quota`], which by default passes them on here.
    fn error(&self, _code: &ErrorCode, _to: Option<SignKey>) {}
    /// A `quota` refusal on a connection to `relay` (the Relay the attempt dialed), `to` as
    /// for [`ConnectorEvents::error`]. A connect the Relay refused for quota and a close for
    /// quota ([`close::QUOTA`]) come here as `to: None`. Reported only while `relay` is the
    /// one the trusted head names: nothing comes from a farewell to an old Relay, whose quota
    /// is no longer the Ring's. A report can still cross a change of the
    /// Relay, so an owner that keeps quota state checks `relay` against its own head when it
    /// applies the report. By default: [`ConnectorEvents::error`] with [`ErrorCode::Quota`].
    fn quota(&self, _relay: &str, to: Option<SignKey>) {
        self.error(&ErrorCode::Quota, to);
    }
    /// The Relay accepted a counted frame of the current connection (see
    /// [`RingEvents::accepted`]): it ends a run of `quota` refusals there.
    fn accepted(&self) {}
    /// The Relay's entitlement slot as this connection sees it: called after every
    /// `Connected` report (with the token from `welcome`, or the latest broadcast) and on
    /// every `entitlement` broadcast. Calls from the Connector's thread and the client's may
    /// cross, and the same token may come again: the live slot is [`Connector::entitlement`].
    /// A broadcast arrives on the client's IO thread, so this must not wait on the Relay
    /// (no [`Connector::put_entitlement`] from here).
    fn entitlement(&self, _token: Option<&str>) {}
}

pub(super) struct Stop {
    /// `None`: drop the connection without a goodbye.
    pub(super) reason: Option<ByeReason>,
}

pub(super) struct St {
    pub(super) chain: RosterChain,
    /// The current attempt's generation. Bumped for every attempt and when an attempt is
    /// abandoned, so the callbacks of older ones are ignored.
    gen: u64,
    client: Option<Arc<RingClient>>,
    /// The current attempt's connection ended.
    closed: Option<CloseReason>,
    link: LinkState,
    /// `set_chain` (or a head naming another Relay) since the worker last looked.
    pub(super) dirty: bool,
    /// The current connection's Relay sent a head naming another Relay: fetch the rest of
    /// what it holds before following.
    follow: bool,
    kick: bool,
    /// Limited state changed (entitlement); the worker re-reports `Connected`.
    refresh: bool,
    pub(super) stop: Option<Stop>,
    /// The foreground state a Mobile last set: sent again on every new connection.
    foreground: Option<bool>,
    /// The farewells owed, by old Relay.
    pub(super) farewells: BTreeMap<String, FwJob>,
    /// The farewells or the chain changed since the farewell worker last looked.
    pub(super) fw_dirty: bool,
    /// The farewell attempt's generation, apart from `gen`: bumped for every attempt and
    /// when one is cut, so its callbacks are ignored.
    pub(super) fw_gen: u64,
    /// The old Relay of the farewell attempt in flight.
    pub(super) fw_inflight: Option<String>,
    /// That attempt's connection, once it is up.
    pub(super) fw_client: Option<Arc<RingClient>>,
}

pub(super) struct Inner {
    pub(super) st: Mutex<St>,
    pub(super) cv: Condvar,
    pub(super) events: Arc<dyn ConnectorEvents>,
    pub(super) base: RingClientConfig,
    backoff_unit: Duration,
    stable_after: Duration,
    pub(super) farewell_unit: Duration,
    pub(super) farewell_max: Duration,
    /// Test hook: runs on the worker right before a quota report's fence.
    #[cfg(test)]
    before_quota: Mutex<Option<Box<dyn Fn() + Send>>>,
}

/// Keeps a device connected to its Ring's Relay; see the module docs.
pub struct Connector {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// The farewell worker, started with the first farewell.
    farewell_thread: Mutex<Option<JoinHandle<()>>>,
}

pub(super) fn lock(m: &Mutex<St>) -> MutexGuard<'_, St> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The chain `theirs` makes of `ours` if it is newer and extends it.
pub(super) fn merge(ours: &RosterChain, theirs: &RosterChain) -> Option<RosterChain> {
    if theirs.ring_id() != ours.ring_id() || theirs.head().version() <= ours.head().version() {
        return None;
    }
    match ours.extended(theirs.since(ours.head().version())) {
        Ok((c, a)) if a.added > 0 => Some(c),
        _ => None,
    }
}

/// The newest version of `chain` that names `relay`.
fn named_by(chain: &RosterChain, relay: &str) -> Option<u64> {
    chain
        .versions()
        .iter()
        .rev()
        .find(|r| r.roster().relay_url == relay)
        .map(|r| r.version())
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

/// Whether the Relay closed the connection because the Ring is over its daily quota. Only
/// the close code says so: a close's `error` is the last error of the connection, not why it
/// closed.
fn quota_close(why: &CloseReason) -> bool {
    matches!(
        why,
        CloseReason::Relay {
            close_code: Some(close::QUOTA),
            ..
        }
    )
}

/// Whether a connect failed because the Ring is over its Relay's daily quota.
pub(super) fn quota_error(e: &RingError) -> bool {
    match e {
        RingError::Relay { code, .. } => *code == ErrorCode::Quota,
        RingError::Closed(why) => quota_close(why),
        _ => false,
    }
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
    /// The Relay this attempt dialed. Nothing about the quota is reported once the trusted
    /// head names another (a Relay change since).
    dialed: String,
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
        let (merged, cut) = {
            let mut st = lock(&inner.st);
            if st.gen != self.gen || st.stop.is_some() {
                return;
            }
            let Some(m) = merge(&st.chain, chain) else {
                return;
            };
            let moved = m.head().roster().relay_url != st.chain.head().roster().relay_url;
            st.chain = m.clone();
            st.fw_dirty = true;
            if moved {
                // The Ring moved: follow it, once the rest of what this Relay holds is in.
                st.dirty = true;
                st.follow = true;
            }
            inner.cv.notify_all();
            (m, Inner::prune_farewells(&mut st))
        };
        if let Some(c) = cut {
            c.shutdown();
        }
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
        if code == ErrorCode::Quota {
            if inner.quota_current(self.gen, &self.dialed) {
                inner.events.quota(&self.dialed, to);
            }
            return;
        }
        if inner.is_current(self.gen) {
            inner.events.error(&code, to);
        }
    }

    fn accepted(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.quota_current(self.gen, &self.dialed) {
            inner.events.accepted();
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

    /// Whether attempt `gen`, which dialed `dialed`, may report about the quota: it is
    /// current, no stop was asked for, and the trusted head still names that Relay.
    fn quota_current(&self, gen: u64, dialed: &str) -> bool {
        let st = lock(&self.st);
        st.gen == gen && st.stop.is_none() && st.chain.head().roster().relay_url == dialed
    }

    /// The Relay refused attempt `gen` for quota (a connect or a close): reported with
    /// `to: None`, only while [`Inner::quota_current`].
    fn report_quota(&self, gen: u64, dialed: &str) {
        #[cfg(test)]
        if let Some(h) = self
            .before_quota
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            h();
        }
        if self.quota_current(gen, dialed) {
            self.events.quota(dialed, None);
        }
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

    /// `relay` holds the Ring at least up to `version`: reported for attempt `gen` only.
    fn report_held(&self, gen: u64, relay: &str, version: u64) {
        if self.is_current(gen) {
            self.events.held(relay, version);
        }
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

    /// Publishes every version of the trusted chain after `from` on `c`, oldest first.
    fn publish_since(&self, c: &RingClient, from: u64) -> Result<(), RingError> {
        let versions: Vec<SignedRoster> = lock(&self.st).chain.since(from).to_vec();
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

    /// Fetches what the Relay `c` is connected to holds beyond `c`'s head, until it sends
    /// nothing new, bounded by the request timeout. Each version comes through the attempt's
    /// `chain` callback.
    fn catch_up(&self, c: &RingClient) {
        let until = Instant::now() + self.base.timeouts.request;
        loop {
            let before = c.chain().head().version();
            let r = c
                .probe_start()
                .and_then(|t| t.wait_until(until, || lock(&self.st).stop.is_some()));
            if r.is_err() || c.chain().head().version() == before {
                return;
            }
        }
    }

    /// The stop request: says goodbye on `c` if there is one.
    fn finish(&self, c: Option<Arc<RingClient>>) {
        let reason = {
            let mut st = lock(&self.st);
            st.gen += 1;
            st.client = None;
            st.stop.as_ref().and_then(|s| s.reason.clone())
        };
        if let (Some(c), Some(reason)) = (c, reason) {
            let _ = c.goodbye(reason);
        }
    }

    fn run(self: Arc<Self>) {
        let mut backoff = Backoff::default();
        'attempt: loop {
            // One attempt: what to dial, under a new generation.
            let (target, gen) = {
                let mut st = lock(&self.st);
                if st.stop.is_some() {
                    break;
                }
                st.dirty = false;
                st.follow = false;
                st.kick = false;
                st.closed = None;
                st.refresh = false;
                st.gen += 1;
                (st.chain.clone(), st.gen)
            };
            self.set_link(LinkState::Connecting {
                attempt: backoff.failures,
            });
            // The Relay this attempt dials: the target head's, whatever the chain says later.
            let dialed = target.head().roster().relay_url.clone();
            let dialed_version = target.head().version();
            let mut cfg = self.base.clone();
            cfg.chain = target;
            let events = Arc::new(Attempt {
                inner: Arc::downgrade(&self),
                gen,
                dialed: dialed.clone(),
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
                Err(e) if removed_error(&e) => {
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
                    if quota_error(&e) {
                        // Only a Relay that holds the Ring counts its traffic.
                        self.report_held(gen, &dialed, dialed_version);
                        self.report_quota(gen, &dialed);
                    }
                    let retry_in = backoff.failed(self.backoff_unit);
                    if self.backoff_wait(retry_in, e.to_string()) {
                        break;
                    }
                    continue;
                }
            };
            let up_at = Instant::now();

            // Versions set while the connect ran.
            if c.chain().head().version() < lock(&self.st).chain.head().version() {
                let _ = self.sync(&c, &dialed);
            }
            self.connected(&c, gen);
            let held = named_by(&c.chain(), &dialed).unwrap_or(dialed_version);
            self.report_held(gen, &dialed, held);
            loop {
                match self.wait(None, true) {
                    Woke::Stop => {
                        self.finish(Some(c));
                        break 'attempt;
                    }
                    Woke::Refresh => self.connected(&c, gen),
                    Woke::Kick | Woke::Timeout => {}
                    Woke::Dirty => {
                        if std::mem::take(&mut lock(&self.st).follow) {
                            self.catch_up(&c);
                        }
                        if !self.sync(&c, &dialed) {
                            // Another Ring or another Relay: dial again, at once.
                            self.abandon();
                            continue 'attempt;
                        }
                    }
                    Woke::Closed(why) => {
                        let quota = quota_close(&why);
                        if quota {
                            // Before `abandon`, which fences this attempt off.
                            let held = named_by(&c.chain(), &dialed).unwrap_or(dialed_version);
                            self.report_held(gen, &dialed, held);
                            self.report_quota(gen, &dialed);
                        }
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
                        // A close for quota is a failure however long the connection was up:
                        // a busy Ring must not redial through its quota closes at once.
                        let retry_in = if quota {
                            backoff.failed(self.backoff_unit)
                        } else {
                            backoff.ended(up_at.elapsed(), self.stable_after, self.backoff_unit)
                        };
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
    /// of another Ring or names another Relay, so `c` is the wrong connection.
    fn sync(&self, c: &RingClient, dialed: &str) -> bool {
        let ours = lock(&self.st).chain.clone();
        let theirs = c.chain();
        if ours.ring_id() != theirs.ring_id() || ours.head().roster().relay_url != dialed {
            return false;
        }
        let from = theirs.head().version();
        if ours.head().version() > from && self.publish_since(c, from).is_err() {
            // The next connect stages what the Relay lacks.
            return false;
        }
        true
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
        Connector::launch(Connector::inner(cfg, events))
    }

    fn inner(cfg: ConnectorConfig, events: Arc<dyn ConnectorEvents>) -> Inner {
        let farewells = cfg
            .farewells
            .into_iter()
            .filter(|f| f.still_owed(&cfg.client.chain))
            .map(|f| (f.source.clone(), FwJob::new(f)))
            .collect();
        Inner {
            st: Mutex::new(St {
                chain: cfg.client.chain.clone(),
                gen: 0,
                client: None,
                closed: None,
                link: LinkState::Connecting { attempt: 0 },
                dirty: false,
                follow: false,
                kick: false,
                refresh: false,
                stop: None,
                foreground: None,
                farewells,
                fw_dirty: false,
                fw_gen: 0,
                fw_inflight: None,
                fw_client: None,
            }),
            cv: Condvar::new(),
            events,
            base: cfg.client,
            backoff_unit: cfg.backoff_unit,
            stable_after: cfg.stable_after,
            farewell_unit: cfg.farewell_backoff_unit,
            farewell_max: cfg.farewell_backoff_max,
            #[cfg(test)]
            before_quota: Mutex::new(None),
        }
    }

    fn launch(inner: Inner) -> std::io::Result<Connector> {
        let inner = Arc::new(inner);
        let i = inner.clone();
        let thread = std::thread::Builder::new()
            .name("ring-connector".into())
            .spawn(move || i.run())?;
        let c = Connector {
            inner,
            thread: Mutex::new(Some(thread)),
            farewell_thread: Mutex::new(None),
        };
        c.start_farewells()?;
        Ok(c)
    }

    /// Starts the farewell worker once a farewell is owed (and the Connector runs).
    fn start_farewells(&self) -> std::io::Result<()> {
        let mut t = self
            .farewell_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        {
            let st = lock(&self.inner.st);
            if t.is_some() || st.stop.is_some() || st.farewells.is_empty() {
                return Ok(());
            }
        }
        let i = self.inner.clone();
        *t = Some(
            std::thread::Builder::new()
                .name("ring-farewell".into())
                .spawn(move || i.run_farewells())?,
        );
        Ok(())
    }

    /// Trust `chain` from now on (it replaces the one held). Versions the Relay lacks are
    /// published; a head naming another Relay, or another Ring, makes the Connector dial
    /// again at once. A farewell the chain no longer owes is dropped (another Ring drops
    /// them all). Never blocks, and reports nothing itself.
    pub fn set_chain(&self, chain: RosterChain) {
        let cut = {
            let mut st = lock(&self.inner.st);
            if st.chain.ring_id() != chain.ring_id() {
                st.farewells.clear();
            }
            st.chain = chain;
            st.dirty = true;
            st.fw_dirty = true;
            self.inner.cv.notify_all();
            Inner::prune_farewells(&mut st)
        };
        if let Some(c) = cut {
            c.shutdown();
        }
    }

    /// Replaces the farewells owed: one per old Relay (the last given counts), those the
    /// trusted chain no longer owes left out. A farewell to a Relay already in the set keeps
    /// its schedule; an attempt in flight at one left out is cut without a goodbye. Never
    /// blocks, and reports nothing itself: states come from the farewell worker.
    pub fn set_farewells(&self, farewells: Vec<Farewell>) {
        let cut = {
            let mut st = lock(&self.inner.st);
            let mut next = BTreeMap::new();
            for f in farewells {
                if !f.still_owed(&st.chain) {
                    continue;
                }
                let kept = next
                    .remove(&f.source)
                    .or_else(|| st.farewells.remove(&f.source));
                let job = match kept {
                    Some(mut j) => {
                        j.farewell = f.clone();
                        j
                    }
                    None => FwJob::new(f.clone()),
                };
                next.insert(f.source, job);
            }
            st.farewells = next;
            st.fw_dirty = true;
            self.inner.cv.notify_all();
            Inner::prune_farewells(&mut st)
        };
        if let Some(c) = cut {
            c.shutdown();
        }
        if let Err(e) = self.start_farewells() {
            // Retried with the next set; the link itself is unaffected.
            eprintln!("xshell: cannot start the farewell worker: {e}");
        }
    }

    /// Every farewell owed, with its last state (`Sending` before its first attempt).
    pub fn farewells(&self) -> Vec<FarewellState> {
        lock(&self.inner.st)
            .farewells
            .values()
            .map(FwJob::view)
            .collect()
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

    /// Probes the Relay with a counted frame without side effects (see
    /// [`RingClient::probe_start`]): `Ok` says the Relay accepts this Ring's traffic,
    /// `Err(Relay{quota})` that the Ring is over its daily quota. Never blocks; `Err(Closed)`
    /// when not connected.
    pub fn probe_start(&self) -> Result<Ticket, RingError> {
        let c = lock(&self.inner.st).client.clone();
        match c {
            Some(c) if !c.is_closed() => c.probe_start(),
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

    /// Retry now instead of waiting out the backoff (or a stop for removal). A live
    /// connection is left alone.
    pub fn kick(&self) {
        let mut st = lock(&self.inner.st);
        st.kick = true;
        self.inner.cv.notify_all();
    }

    /// Stops for good: says goodbye with `reason` if connected (a farewell's connection
    /// included), after waiting for an attempt in flight to finish (bounded by the connect
    /// timeout). Idempotent.
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
        let handles: Vec<JoinHandle<()>> = [&self.thread, &self.farewell_thread]
            .into_iter()
            .filter_map(|t| t.lock().unwrap_or_else(|e| e.into_inner()).take())
            .collect();
        // Each worker ends within the connect timeout (an attempt in flight) plus the
        // goodbye; past that it is left to finish on its own, fenced by the stop flag.
        let t = self.inner.base.timeouts;
        let deadline = Instant::now() + t.connect + t.bye + Duration::from_secs(1);
        while handles.iter().any(|h| !h.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for h in handles {
            if h.is_finished() {
                let _ = h.join();
            }
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

#[cfg(all(test, feature = "test-relay"))]
mod tests {
    use super::*;
    use crate::ring::relay::contract::{config, connect, RawConn, TestRing, WAIT};
    use crate::ring::relay::test_relay::{TestRelay, TestRelayOptions};
    use crate::ring::relay::wire::{ClientFrame, RelayFrame};
    use std::sync::mpsc;

    #[derive(Default)]
    struct Errors(Mutex<Vec<(ErrorCode, Option<SignKey>)>>);

    impl ConnectorEvents for Errors {
        fn error(&self, code: &ErrorCode, to: Option<SignKey>) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((code.clone(), to));
        }
    }

    /// A stop asked for while a connect refused for quota is about to be reported: the
    /// report is fenced off, and nothing about the quota comes during the stop either.
    #[test]
    fn a_stop_while_a_quota_report_is_held_reports_nothing() {
        let r = TestRelay::start_with(TestRelayOptions {
            auth_timeout: Duration::from_millis(500),
            quota_frames_per_day: Some(8),
            ..TestRelayOptions::default()
        });
        let mut ring = TestRing::new(&r.url());
        // The Relay is ahead of the Daemon's chain, so its connect asks for version 3 …
        let (desk, _) = connect(&r.target(), &ring.chain, ring.desktop.clone());
        let v3 = ring.next(|_| {});
        desk.publish_roster(&v3).unwrap();
        // … and the Ring's quota is used up, so the Relay refuses that.
        let mut m = RawConn::login(&r.target(), &ring.chain, &*ring.mobile);
        let mut refused = false;
        for id in 1..=100 {
            m.send(&ClientFrame::RosterGet { id, since: 3 }.encode())
                .unwrap();
            if let Some(RelayFrame::Error {
                code: ErrorCode::Quota,
                ..
            }) = m.frame(WAIT)
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "the quota was never reached");

        let (held_tx, held_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut cc = ConnectorConfig::new(config(&r.target(), &ring.up_to(2), ring.daemon.clone()));
        cc.backoff_unit = Duration::from_secs(30);
        let rec = Arc::new(Errors::default());
        let inner = Connector::inner(cc, rec.clone());
        *inner.before_quota.lock().unwrap() = Some(Box::new(move || {
            let _ = held_tx.send(());
            let _ = go_rx.recv_timeout(WAIT);
        }));
        let c = Connector::launch(inner).unwrap();
        held_rx
            .recv_timeout(WAIT)
            .expect("the worker reaches the quota report");
        // The stop is asked for while the worker is held right before the report.
        {
            let mut st = lock(&c.inner.st);
            st.stop = Some(Stop {
                reason: Some(ByeReason::quit()),
            });
            c.inner.cv.notify_all();
        }
        go_tx.send(()).unwrap();
        c.stop(ByeReason::quit());
        assert_eq!(c.state(), LinkState::Stopped { error: None });
        let errors = rec.0.lock().unwrap().clone();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[derive(Default)]
    struct Quotas(Mutex<Vec<String>>);

    impl ConnectorEvents for Quotas {
        fn quota(&self, relay: &str, _to: Option<SignKey>) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(relay.to_string());
        }
        fn accepted(&self) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push("accepted".into());
        }
    }

    /// A quota refusal of a current attempt whose Relay the trusted head no longer names (it
    /// arrived after a Relay change, before the worker abandoned the attempt) is not
    /// reported; one about the head's Relay is, with that Relay.
    #[test]
    fn a_quota_refusal_from_a_relay_the_head_no_longer_names_is_dropped() {
        let ring = TestRing::new("ws://127.0.0.1:1");
        let rec = Arc::new(Quotas::default());
        let cc = ConnectorConfig::new(RingClientConfig::new(
            ring.chain.clone(),
            ring.desktop.clone(),
        ));
        let inner = Arc::new(Connector::inner(cc, rec.clone()));
        let attempt = |dialed: &str| Attempt {
            inner: Arc::downgrade(&inner),
            gen: lock(&inner.st).gen,
            dialed: dialed.to_string(),
        };
        let old = attempt("ws://127.0.0.1:2");
        old.error(ErrorCode::Quota, None, None);
        old.accepted();
        inner.report_quota(old.gen, &old.dialed);
        assert!(rec.0.lock().unwrap().is_empty());
        let ours = attempt("ws://127.0.0.1:1");
        ours.error(ErrorCode::Quota, None, None);
        assert_eq!(*rec.0.lock().unwrap(), vec!["ws://127.0.0.1:1".to_string()]);
    }
}

//! Farewells: telling an old Relay where the Ring went (ADR-0012). The Connector never waits
//! for them. One worker thread (`ring-farewell`, started with the first farewell) sends them
//! one at a time, each on a short-lived connection of its own: it authenticates on the old
//! Relay with the whole trusted chain, staging what that Relay lacks (`auth.chain`), so the
//! Relay commits up to the head at once and its devices go straight to the newest Relay.
//! Versions made after the authentication follow as ordinary uploads. A farewell that fails
//! is retried after a backoff that doubles up to [`ConnectorConfig::farewell_backoff_max`],
//! for as long as the Connector runs; one that is no longer owed (the head names its Relay
//! again) is dropped, and an attempt at it in flight is cut without a goodbye.
//!
//! The same worker runs **fetch jobs**: a catch-up the main connection could not finish
//! (see the Connector's docs) is asked again here, on a connection to that Relay with the
//! chain this device has, until it answers (with something newer or not). Fetch jobs retry
//! on the same backoff, report nothing, and are dropped once the head names their Relay.
//!
//! [`ConnectorConfig::farewell_backoff_max`]: super::ConnectorConfig::farewell_backoff_max

use super::super::chain::RosterChain;
use super::super::{RingError, RosterError, SignKey, SignedRoster};
use super::client::{RingClient, RingEvents};
use super::connector::{lock, merge, quota_error, Inner, St};
use super::wire::{ByeReason, CloseReason, ErrorCode, MemberPresence};
use crate::backoff::capped;
use std::sync::{Arc, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// A farewell owed to an old Relay: `source`, the Relay that version `from` names, must learn
/// of the versions after it. `target` is the head the owner last asked for; a farewell always
/// delivers the trusted head of the moment it is sent, so devices still on `source` go
/// straight to the newest Relay.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Farewell {
    pub source: String,
    pub from: u64,
    pub target: u64,
}

impl Farewell {
    /// The farewell for leaving the Relay that version `from` of `chain` names, up to its
    /// head.
    pub fn new(chain: &RosterChain, from: u64) -> Option<Farewell> {
        let old = chain.get(from)?;
        Some(Farewell {
            source: old.roster().relay_url.clone(),
            from,
            target: chain.head().version(),
        })
    }

    /// Whether `chain` still owes it: version `from` names `source`, the head names another
    /// Relay, and `target` is newer than `from`.
    pub fn still_owed(&self, chain: &RosterChain) -> bool {
        chain
            .get(self.from)
            .is_some_and(|r| r.roster().relay_url == self.source)
            && chain.head().roster().relay_url != self.source
            && self.target > self.from
    }
}

/// Where a farewell stands.
#[derive(Debug, Clone, PartialEq)]
pub enum FarewellState {
    /// Being sent now (or about to be).
    Sending { farewell: Farewell },
    /// The last attempt failed after `failures` in a row; the next starts after `retry_in`
    /// (from when this state was reported).
    Waiting {
        farewell: Farewell,
        failures: u32,
        error: String,
        retry_in: Duration,
    },
    /// The old Relay acknowledged it: it holds the chain up to `farewell.target`. The owner
    /// drops its farewell to `farewell.source` if it asked for no newer target.
    Done { farewell: Farewell },
}

/// A farewell the worker owes, with its schedule.
pub(super) struct FwJob {
    pub(super) farewell: Farewell,
    pub(super) failures: u32,
    pub(super) next_at: Instant,
    /// The last state reported; `None` before the first attempt.
    pub(super) state: Option<FarewellState>,
}

impl FwJob {
    pub(super) fn new(farewell: Farewell) -> FwJob {
        FwJob {
            farewell,
            failures: 0,
            next_at: Instant::now(),
            state: None,
        }
    }

    /// What [`super::Connector::farewells`] reports for it.
    pub(super) fn view(&self) -> FarewellState {
        self.state
            .clone()
            .unwrap_or_else(|| FarewellState::Sending {
                farewell: self.farewell.clone(),
            })
    }
}

/// A worker job, by Relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Job {
    Farewell(String),
    Fetch(String),
}

/// A catch-up to ask a Relay again, with its schedule.
pub(super) struct FetchJob {
    failures: u32,
    next_at: Instant,
}

impl FetchJob {
    pub(super) fn new() -> FetchJob {
        FetchJob {
            failures: 0,
            next_at: Instant::now(),
        }
    }
}

/// What the worker picked.
enum Picked {
    Farewell(Farewell),
    Fetch(String),
}

/// How one attempt ended.
enum Sent {
    /// The old Relay holds the trusted head, this version.
    Done(u64),
    Failed(RingError),
    /// Stopped, or no longer owed.
    Aborted,
}

/// What a farewell's connection reports. Only a newer chain matters; its traffic is not this
/// device's session (the main connection is), so it is dropped, and quota refusals are a
/// failure of the farewell, never a quota report: the old Relay's quota is not the Ring's.
struct FarewellEvents {
    inner: Weak<Inner>,
    gen: u64,
}

impl RingEvents for FarewellEvents {
    fn envelope(&self, _from: SignKey, _payload: Vec<u8>) {}
    fn presence(&self, _key: SignKey, _presence: MemberPresence) {}
    fn roster(&self, _roster: &SignedRoster) {}

    /// The Relay knew versions this device had not seen: trusted like any verified chain, then
    /// published on the main connection's Relay, or followed when the head names another.
    fn chain(&self, chain: &RosterChain) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let merged = {
            let mut st = lock(&inner.st);
            if st.fw_gen != self.gen || st.stop.is_some() {
                return;
            }
            let Some(m) = merge(&st.chain, chain) else {
                return;
            };
            // The main connection syncs (it publishes what its Relay lacks) or, for a head
            // naming another Relay, follows; nothing of this came from its Relay, so no
            // `follow`.
            st.dirty = true;
            st.chain = m.clone();
            st.fw_dirty = true;
            inner.cv.notify_all();
            m
        };
        inner.events.roster(&merged);
    }

    fn roster_rejected(&self, _error: RosterError) {}
    fn error(&self, _code: ErrorCode, _to: Option<SignKey>, _detail: Option<String>) {}
    fn closed(&self, _why: CloseReason) {}
}

impl Inner {
    /// The farewell worker: sends the due farewell with the earliest schedule, one at a time,
    /// until the Connector stops.
    pub(super) fn run_farewells(self: Arc<Self>) {
        loop {
            let Some((picked, gen, chain)) = self.next_job() else {
                return;
            };
            let farewell = match picked {
                Picked::Farewell(f) => f,
                Picked::Fetch(source) => {
                    self.fetch(&source, gen, chain);
                    continue;
                }
            };
            self.events.farewell(&FarewellState::Sending {
                farewell: farewell.clone(),
            });
            match self.send_farewell(&farewell, gen, chain) {
                Sent::Done(target) => {
                    let done = Farewell { target, ..farewell };
                    self.events.farewell(&FarewellState::Done {
                        farewell: done.clone(),
                    });
                    self.events.held(&done.source, target);
                }
                Sent::Failed(e) => self.farewell_failed(&farewell, gen, e),
                Sent::Aborted => {}
            }
        }
    }

    /// Waits for a job that is due (a farewell or a fetch, the earliest first) and marks it in
    /// flight under a new generation. `None`: stopped.
    fn next_job(&self) -> Option<(Picked, u64, RosterChain)> {
        let mut st = lock(&self.st);
        loop {
            if st.stop.is_some() {
                return None;
            }
            st.fw_dirty = false;
            let _ = Inner::prune_farewells(&mut st);
            let chain = st.chain.clone();
            let next = st
                .farewells
                .values()
                .map(|j| (Job::Farewell(j.farewell.source.clone()), j.next_at))
                .chain(
                    st.fetches
                        .iter()
                        .map(|(s, j)| (Job::Fetch(s.clone()), j.next_at)),
                )
                .min_by_key(|(_, at)| *at);
            let now = Instant::now();
            match next {
                Some((job, at)) if at <= now => {
                    st.fw_gen += 1;
                    let gen = st.fw_gen;
                    st.fw_inflight = Some(job.clone());
                    let source = match job {
                        Job::Fetch(source) => return Some((Picked::Fetch(source), gen, chain)),
                        Job::Farewell(source) => source,
                    };
                    let job = st.farewells.get_mut(&source).expect("picked");
                    job.state = Some(FarewellState::Sending {
                        farewell: job.farewell.clone(),
                    });
                    return Some((Picked::Farewell(job.farewell.clone()), gen, chain));
                }
                Some((_, at)) => st = self.wait_fw(st, Some(at - now)),
                None => st = self.wait_fw(st, None),
            }
        }
    }

    fn wait_fw<'a>(&self, st: MutexGuard<'a, St>, d: Option<Duration>) -> MutexGuard<'a, St> {
        if st.fw_dirty {
            return st;
        }
        match d {
            None => self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
            Some(d) => {
                self.cv
                    .wait_timeout(st, d)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
        }
    }

    /// Whether the farewell attempt `gen` should end now: stopped, or no longer owed.
    fn fw_abort(st: &St, gen: u64) -> bool {
        st.stop.is_some() || st.fw_gen != gen
    }

    /// One attempt: authenticate on the old Relay with `chain` staged, then upload what the
    /// trusted chain gained since, until that Relay holds the head.
    fn send_farewell(self: &Arc<Self>, farewell: &Farewell, gen: u64, chain: RosterChain) -> Sent {
        let mut cfg = self.base.clone();
        cfg.chain = chain;
        cfg.dial = Some(farewell.source.clone());
        let events = Arc::new(FarewellEvents {
            inner: Arc::downgrade(self),
            gen,
        });
        let c = match RingClient::connect(cfg, events) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                return if Inner::fw_abort(&lock(&self.st), gen) {
                    self.fw_finish(None);
                    Sent::Aborted
                } else {
                    Sent::Failed(e)
                };
            }
        };
        {
            let mut st = lock(&self.st);
            if Inner::fw_abort(&st, gen) {
                drop(st);
                self.fw_finish(Some(c));
                return Sent::Aborted;
            }
            st.fw_client = Some(c.clone());
        }
        loop {
            let versions: Vec<SignedRoster> = {
                let mut st = lock(&self.st);
                if Inner::fw_abort(&st, gen) {
                    drop(st);
                    self.fw_finish(Some(c));
                    return Sent::Aborted;
                }
                let held = c.chain().head().version();
                let rest = st.chain.since(held).to_vec();
                if rest.is_empty() {
                    // Done under the lock, so a newer chain set meanwhile is never missed.
                    st.farewells.remove(&farewell.source);
                    st.fw_inflight = None;
                    st.fw_client = None;
                    drop(st);
                    let _ = c.goodbye(ByeReason::quit());
                    return Sent::Done(held);
                }
                rest
            };
            for r in &versions {
                let until = Instant::now() + self.base.timeouts.request;
                let res = c
                    .publish_roster_start(r)
                    .and_then(|t| t.wait_until(until, || Inner::fw_abort(&lock(&self.st), gen)));
                match res {
                    Ok(()) => {}
                    // The Relay already holds it (another device published it first).
                    Err(RingError::Roster(RosterError::Stale)) => {}
                    Err(_) if Inner::fw_abort(&lock(&self.st), gen) => {
                        self.fw_finish(Some(c));
                        return Sent::Aborted;
                    }
                    Err(e) => {
                        lock(&self.st).fw_client = None;
                        let _ = c.goodbye(ByeReason::quit());
                        return Sent::Failed(e);
                    }
                }
            }
        }
    }

    /// An attempt that ended without a result: a stop says goodbye on `c` (with the stop's
    /// reason, none for an abandon); a farewell no longer owed drops it.
    fn fw_finish(&self, c: Option<Arc<RingClient>>) {
        let reason = {
            let mut st = lock(&self.st);
            st.fw_client = None;
            if st.stop.is_none() {
                st.fw_inflight = None;
            }
            st.stop.as_ref().and_then(|s| s.reason.clone())
        };
        if let (Some(c), Some(reason)) = (c, reason) {
            let _ = c.goodbye(reason);
        }
    }

    /// One fetch job's attempt: connect to `source` with `chain` (whatever it holds beyond
    /// it comes through [`FarewellEvents::chain`], fenced by `gen`), then drop the connection.
    fn fetch(self: &Arc<Self>, source: &str, gen: u64, chain: RosterChain) {
        let mut cfg = self.base.clone();
        cfg.chain = chain;
        cfg.dial = Some(source.to_string());
        let events = Arc::new(FarewellEvents {
            inner: Arc::downgrade(self),
            gen,
        });
        let r = RingClient::connect(cfg, events);
        let mut st = lock(&self.st);
        if st.fw_gen != gen {
            // Removed meanwhile (or stopped): nothing of it counts.
            return;
        }
        st.fw_inflight = None;
        st.fw_client = None;
        match r {
            Ok(_) => {
                st.fetches.remove(source);
            }
            Err(_) => {
                let (unit, max) = (self.farewell_unit, self.farewell_max);
                if let Some(j) = st.fetches.get_mut(source) {
                    j.failures += 1;
                    j.next_at = Instant::now() + capped(j.failures - 1, unit, max);
                }
            }
        }
    }

    fn farewell_failed(&self, farewell: &Farewell, gen: u64, e: RingError) {
        let state = {
            let mut st = lock(&self.st);
            if st.fw_gen != gen {
                return;
            }
            st.fw_inflight = None;
            let unit = self.farewell_unit;
            let max = self.farewell_max;
            let Some(job) = st.farewells.get_mut(&farewell.source) else {
                return;
            };
            job.failures += 1;
            let retry_in = capped(job.failures - 1, unit, max);
            job.next_at = Instant::now() + retry_in;
            let s = FarewellState::Waiting {
                farewell: job.farewell.clone(),
                failures: job.failures,
                error: e.to_string(),
                retry_in,
            };
            job.state = Some(s.clone());
            s
        };
        self.events.farewell(&state);
        // A Relay that refuses this Ring for quota holds it.
        if quota_error(&e) {
            self.events.held(&farewell.source, farewell.from);
        }
    }

    /// After the set of farewells or the chain changed: drops the farewells the chain no
    /// longer owes and fences off an attempt in flight at one; its connection, if up, is
    /// returned to be cut (outside the lock, without a goodbye).
    pub(super) fn prune_farewells(st: &mut St) -> Option<Arc<RingClient>> {
        let chain = st.chain.clone();
        st.farewells.retain(|_, j| j.farewell.still_owed(&chain));
        let head = chain.head().roster().relay_url.clone();
        st.fetches.retain(|source, _| *source != head);
        let owed = match st.fw_inflight.as_ref()? {
            Job::Farewell(source) => st.farewells.contains_key(source),
            Job::Fetch(source) => st.fetches.contains_key(source),
        };
        if owed {
            return None;
        }
        st.fw_gen += 1;
        st.fw_inflight = None;
        st.fw_client.take()
    }
}

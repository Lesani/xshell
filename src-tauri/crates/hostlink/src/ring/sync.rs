//! Keeps every Host's Daemon in the Ring: the Local Host and the Remote Hosts alike. After
//! each connect (an install, an upgrade and a reconnect all end in one) the worker asks the
//! Daemon for its identity (`ring.identity`), adds it to the Roster as a `daemon` member,
//! and sends it the chain (`ring.join`). Desktop ↔ Daemon traffic stays on the Host link.
//!
//! - The Host Observer runs under the Host's lock, so [`HostSync::observe`] only records
//!   the connection and queues a job; the [`SyncWorker`] thread does the work. A job is for
//!   one connection (its generation): before each step the worker checks that it is still
//!   the current one, the Host link refuses a request meant for an older link
//!   ([`HostHandle::ring_identity`]), and the Roster transaction discards an identity whose
//!   connection is gone.
//! - A Daemon without the `ring` capability is never sent anything: it is `too-old`.
//! - Jobs arriving within [`BATCH_WINDOW`] form one batch: identities are asked for together,
//!   those answered within [`IDENTITY_DEADLINE`] go into one Roster transaction (at most one
//!   new version), and a late answer goes into the next batch.
//! - A Remote Host already in another Ring is left there (`other-ring`) until the user pairs
//!   it with this Desktop ([`SyncWorker::claim`]); the Local Host follows this Desktop's Ring.
//! - Joins are conditional when the Daemon has `ring.cjoin`: the Daemon refuses unless its
//!   membership is still what its identity reported, so two Desktops never take a Host from
//!   each other. A Daemon without `ring.cjoin` is joined only when it is in no Ring or in this
//!   one: between its identity and the join another Desktop could still pair it, and this
//!   join would then take it back (accepted for these development builds).

use super::desktop::{DesktopRing, HostOutcome, HostRef, LocalIdentity};
use crate::errors::HostError;
use crate::handle::{RING_CAPABILITY, RING_CJOIN_CAPABILITY};
use crate::{HostHandle, HostStatus, LOCAL_HOST_ID};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use xshell_protocol::msg::{JoinExpect, MEMBERSHIP_CHANGED};
use xshell_protocol::ring::{RingId, RosterChain};

/// How long the worker gathers jobs after the first one before it starts a batch.
pub const BATCH_WINDOW: Duration = Duration::from_millis(250);
/// How long a batch waits for identities; the ones answered by then are committed.
pub const IDENTITY_DEADLINE: Duration = Duration::from_secs(2);
/// How long `ring_enable` waits for the local Daemon's identity.
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(20);

/// A Host's connection as the Observer last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Seen {
    /// `Connected` or `UpgradePending`.
    pub usable: bool,
    /// The Daemon has the `ring` capability.
    pub ring: bool,
    /// The Daemon has `ring.cjoin` (conditional joins).
    pub cjoin: bool,
    /// The usable link's generation (0 while not usable).
    pub link_gen: u64,
}

impl Seen {
    fn of(s: &HostStatus) -> Seen {
        let usable = s.usable();
        let has = |c: &str| usable && s.daemon_capabilities.iter().any(|x| x == c);
        Seen {
            usable,
            ring: has(RING_CAPABILITY),
            cjoin: has(RING_CJOIN_CAPABILITY),
            link_gen: if usable { s.link_generation } else { 0 },
        }
    }
}

struct Entry {
    seen: Seen,
    /// Bumped whenever `seen` changes: a job queued for an older value is stale.
    gen: u64,
}

struct SyncState {
    hosts: BTreeMap<String, Entry>,
    next: u64,
    tx: Option<Sender<Msg>>,
    shutdown: bool,
}

/// Every Host's connection as the Observer last saw it, and the worker's queue.
pub struct HostSync(Mutex<SyncState>);

impl Default for HostSync {
    fn default() -> Self {
        Self::new()
    }
}

/// A claim of a Host that is in another Ring, bound to what was observed.
#[derive(Debug, Clone)]
struct Claim {
    gen: u64,
    identity: LocalIdentity,
    foreign: String,
    version: Option<u64>,
    /// This Desktop's Ring when the Host was observed.
    ours: RingId,
}

enum Msg {
    Job {
        host: String,
        gen: u64,
        claim: Option<Claim>,
        /// A second attempt after a refused conditional join.
        retry: bool,
    },
    Identity {
        host: String,
        gen: u64,
        r: Result<Value, HostError>,
    },
    Joined {
        host: String,
        gen: u64,
        r: Result<Value, HostError>,
        retry: bool,
    },
}

fn job(host: &str, gen: u64) -> Msg {
    Msg::Job {
        host: host.to_string(),
        gen,
        claim: None,
        retry: false,
    }
}

impl HostSync {
    pub const fn new() -> Self {
        HostSync(Mutex::new(SyncState {
            hosts: BTreeMap::new(),
            next: 0,
            tx: None,
            shutdown: false,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, SyncState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// From the Observer, under the Host's lock: records the Host's connection and queues a
    /// job when a usable one is new or changed. Never blocks.
    pub fn observe(&self, s: &HostStatus) {
        let seen = Seen::of(s);
        let mut st = self.lock();
        let st = &mut *st;
        if st.hosts.get(&s.host).is_some_and(|e| e.seen == seen) {
            return;
        }
        st.next += 1;
        let gen = st.next;
        st.hosts.insert(s.host.clone(), Entry { seen, gen });
        if seen.usable && !st.shutdown {
            if let Some(tx) = &st.tx {
                let _ = tx.send(job(&s.host, gen));
            }
        }
    }

    /// Hands over the worker's queue, and reconciles the Hosts connected already.
    fn attach(&self, tx: Sender<Msg>) {
        let mut st = self.lock();
        if !st.shutdown {
            for (h, e) in &st.hosts {
                if e.seen.usable {
                    let _ = tx.send(job(h, e.gen));
                }
            }
        }
        st.tx = Some(tx);
    }

    fn send_current(&self, only: Option<&str>) {
        let st = self.lock();
        if st.shutdown {
            return;
        }
        let Some(tx) = &st.tx else { return };
        for (h, e) in &st.hosts {
            if e.seen.usable && only.is_none_or(|o| o == h) {
                let _ = tx.send(job(h, e.gen));
            }
        }
    }

    /// Queues a job for every connected Host (after the Ring changed).
    pub fn poke_all(&self) {
        self.send_current(None);
    }

    /// The Host's display name changed: its member's name follows.
    pub fn renamed(&self, host: &str) {
        self.send_current(Some(host));
    }

    /// The connection a job of `gen` was queued for, while it is still the current one.
    pub fn current(&self, host: &str, gen: u64) -> Option<Seen> {
        let st = self.lock();
        match st.hosts.get(host) {
            Some(e) if !st.shutdown && e.gen == gen && e.seen.usable => Some(e.seen),
            _ => None,
        }
    }

    /// The Host's current connection and its generation.
    pub fn seen(&self, host: &str) -> Option<(Seen, u64)> {
        let st = self.lock();
        st.hosts.get(host).map(|e| (e.seen, e.gen))
    }

    pub fn shutdown(&self) {
        let mut st = self.lock();
        st.shutdown = true;
        st.tx = None;
    }
}

/// A Host that is not (yet) in the Ring, as Settings → Mobile lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRingState {
    pub host: String,
    /// Its Settings → Hosts name.
    pub name: String,
    /// `too-old` (its xshelld cannot join), `other-ring` (paired with another Desktop's
    /// devices; [`SyncWorker::claim`] pairs it here), `full` or `failed`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Note {
    state: &'static str,
    error: Option<String>,
    claim: Option<Claim>,
}

type Lookup = Box<dyn Fn(&str) -> Option<Arc<HostHandle>> + Send + Sync>;

/// One batch's jobs and answers.
#[derive(Default)]
struct Batch {
    /// The newest job per Host.
    jobs: BTreeMap<String, (u64, Option<Claim>, bool)>,
    /// Identities answered, per Host: (generation, answer).
    ready: BTreeMap<String, (u64, Result<Value, HostError>)>,
}

/// A Host ready for the Roster transaction.
struct Ready {
    host: HostRef,
    identity: LocalIdentity,
    gen: u64,
    link_gen: u64,
    expect: Option<JoinExpect>,
    retry: bool,
}

/// Keeps every Host's Daemon in the Ring. See the module docs.
pub struct SyncWorker {
    pub ring: Arc<DesktopRing>,
    sync: &'static HostSync,
    host: Lookup,
    changed: Box<dyn Fn() + Send + Sync>,
    notes: Mutex<BTreeMap<String, Note>>,
    /// The worker's own queue, for claims and the answers of requests it sent.
    tx: Mutex<Option<Sender<Msg>>>,
    /// Test hook: runs before each Roster transaction.
    #[cfg(test)]
    before_commit: Mutex<Option<Box<dyn Fn() + Send>>>,
}

fn tokens(c: &RosterChain) -> Vec<String> {
    c.versions().iter().map(|r| r.token().to_string()).collect()
}

/// The membership a `ring.identity` answer reports: the Ring's id and head version.
fn observed_ring(v: &Value) -> Option<(String, Option<u64>)> {
    let r = v.get("ring")?;
    let id = r.get("ringId")?.as_str()?.to_string();
    Some((id, r.get("version").and_then(Value::as_u64)))
}

/// Where a Remote Host is reached: a new key at the same place is a reinstall.
fn target_of(cfg: &crate::HostConfig) -> String {
    format!(
        "{}|{}",
        cfg.ssh_target,
        cfg.daemon_command.as_deref().unwrap_or_default()
    )
}

impl SyncWorker {
    pub fn new(
        ring: Arc<DesktopRing>,
        sync: &'static HostSync,
        host: Lookup,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Arc<SyncWorker> {
        Arc::new(SyncWorker {
            ring,
            sync,
            host,
            changed,
            notes: Mutex::new(BTreeMap::new()),
            tx: Mutex::new(None),
            #[cfg(test)]
            before_commit: Mutex::new(None),
        })
    }

    /// Starts the thread and attaches it to `sync` (which reconciles at once).
    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let (tx, rx) = mpsc::channel();
        *self.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
        let w = self.clone();
        std::thread::Builder::new()
            .name("ring-hosts".into())
            .spawn(move || w.run(rx))?;
        self.sync.attach(tx);
        Ok(())
    }

    fn notes(&self) -> MutexGuard<'_, BTreeMap<String, Note>> {
        self.notes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn post(&self, m: Msg) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(m);
        }
    }

    fn sender(&self) -> Option<Sender<Msg>> {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn note(&self, host: &str, state: &'static str, error: Option<String>, claim: Option<Claim>) {
        self.notes().insert(
            host.to_string(),
            Note {
                state,
                error,
                claim,
            },
        );
    }

    fn clear(&self, host: &str) {
        self.notes().remove(host);
    }

    /// The Hosts not in the Ring and why, for the Hosts still configured.
    pub fn host_states(&self) -> Vec<HostRingState> {
        let notes: Vec<_> = self
            .notes()
            .iter()
            .map(|(h, n)| (h.clone(), n.state, n.error.clone()))
            .collect();
        notes
            .into_iter()
            .filter_map(|(host, state, error)| {
                let h = (self.host)(&host)?;
                Some(HostRingState {
                    name: h.config().name,
                    host,
                    state,
                    error,
                })
            })
            .collect()
    }

    /// The state [`SyncWorker::host_states`] lists for `host`, if any.
    pub fn state_of(&self, host: &str) -> Option<&'static str> {
        self.notes().get(host).map(|n| n.state)
    }

    /// The Daemon's identity, asked now and waited for (Enable needs the local Daemon's for
    /// the Ring's first version). `None`: not connected, too old, or no answer.
    pub fn identity_of(&self, host: &str) -> Option<LocalIdentity> {
        let (seen, _) = self.sync.seen(host)?;
        if !seen.ring {
            return None;
        }
        let h = (self.host)(host)?;
        let (tx, rx) = mpsc::channel();
        h.ring_identity(
            seen.link_gen,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        match rx.recv_timeout(IDENTITY_TIMEOUT).ok()? {
            Ok(v) => LocalIdentity::from_json(&v).ok(),
            Err(e) => {
                eprintln!("xshell: ring.identity on {host} failed: {}", e.message);
                None
            }
        }
    }

    /// Pairs a Host that is in another Ring with this Desktop's: bound to the key and the
    /// Ring observed on its current connection, and conditional on the Daemon still being
    /// there. Fails when the Host is not waiting for that, reconnected since, or cannot join
    /// conditionally.
    pub fn claim(&self, host: &str) -> Result<(), String> {
        let c = self
            .notes()
            .get(host)
            .and_then(|n| n.claim.clone())
            .ok_or("this Host is not paired with another set of devices")?;
        let seen = self
            .sync
            .current(host, c.gen)
            .ok_or("this Host reconnected; try again in a moment")?;
        if !seen.cjoin {
            return Err("update xshelld on this Host first".into());
        }
        if self.ring.chain().map(|ch| ch.ring_id().clone()) != Some(c.ours.clone()) {
            return Err("your devices changed; try again in a moment".into());
        }
        self.post(Msg::Job {
            host: host.to_string(),
            gen: c.gen,
            claim: Some(c),
            retry: false,
        });
        Ok(())
    }

    fn run(&self, rx: Receiver<Msg>) {
        let mut backlog: VecDeque<Msg> = VecDeque::new();
        // Identity requests sent and not answered yet: Host → generation.
        let mut inflight: HashMap<String, u64> = HashMap::new();
        loop {
            let first = match backlog.pop_front() {
                Some(m) => m,
                None => match rx.recv() {
                    Ok(m) => m,
                    Err(_) => return,
                },
            };
            let mut b = Batch::default();
            self.absorb(&mut b, &mut inflight, first);
            while let Some(m) = backlog.pop_front() {
                self.absorb(&mut b, &mut inflight, m);
            }
            let until = Instant::now() + BATCH_WINDOW;
            loop {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(left) {
                    Ok(m) => self.absorb(&mut b, &mut inflight, m),
                    Err(_) => break,
                }
            }
            self.batch(b, &rx, &mut backlog, &mut inflight);
        }
    }

    fn absorb(&self, b: &mut Batch, inflight: &mut HashMap<String, u64>, m: Msg) {
        match m {
            Msg::Job {
                host,
                gen,
                claim,
                retry,
            } => {
                let newer = b.jobs.get(&host).is_none_or(|(g, c, _)| {
                    gen > *g || (gen == *g && claim.is_some() && c.is_none())
                });
                if newer {
                    b.jobs.insert(host, (gen, claim, retry));
                }
            }
            Msg::Identity { host, gen, r } => {
                if inflight.get(&host) == Some(&gen) {
                    inflight.remove(&host);
                }
                if b.ready.get(&host).is_none_or(|(g, _)| gen >= *g) {
                    b.ready.insert(host, (gen, r));
                }
            }
            Msg::Joined {
                host,
                gen,
                r,
                retry,
            } => self.joined(&host, gen, r, retry),
        }
    }

    /// A join's answer.
    fn joined(&self, host: &str, gen: u64, r: Result<Value, HostError>, retry: bool) {
        if self.sync.current(host, gen).is_none() {
            return;
        }
        match r {
            Ok(_) => self.clear(host),
            // The Daemon's membership changed since its identity: look again, once.
            Err(e) if e.message == MEMBERSHIP_CHANGED && !retry => {
                self.post(Msg::Job {
                    host: host.to_string(),
                    gen,
                    claim: None,
                    retry: true,
                });
            }
            Err(e) => {
                eprintln!("xshell: ring.join on {host} failed: {}", e.message);
                self.note(host, "failed", Some(e.message), None);
            }
        }
        (self.changed)();
    }

    fn batch(
        &self,
        mut b: Batch,
        rx: &Receiver<Msg>,
        backlog: &mut VecDeque<Msg>,
        inflight: &mut HashMap<String, u64>,
    ) {
        let ours = self.ring.chain().map(|c| c.ring_id().clone());
        let mut claims = Vec::new();
        let mut retries: HashMap<String, bool> = HashMap::new();
        let mut waiting: HashMap<String, u64> = HashMap::new();
        let mut noted = false;
        for (host, (gen, claim, retry)) in std::mem::take(&mut b.jobs) {
            let Some(seen) = self.sync.current(&host, gen) else {
                continue;
            };
            if !seen.ring {
                // Never sent anything: it cannot join until it is updated.
                self.note(&host, "too-old", None, None);
                noted = true;
                continue;
            }
            if self.state_of(&host) == Some("too-old") {
                self.clear(&host);
                noted = true;
            }
            if ours.is_none() {
                continue;
            }
            retries.insert(host.clone(), retry);
            if let Some(c) = claim {
                claims.push((host, c, seen));
                continue;
            }
            if b.ready.get(&host).is_some_and(|(g, _)| *g == gen)
                || inflight.get(&host) == Some(&gen)
            {
                continue;
            }
            let Some(h) = (self.host)(&host) else {
                continue;
            };
            let Some(tx) = self.sender() else { return };
            inflight.insert(host.clone(), gen);
            waiting.insert(host.clone(), gen);
            let name = host.clone();
            h.ring_identity(
                seen.link_gen,
                Box::new(move |r| {
                    let _ = tx.send(Msg::Identity { host: name, gen, r });
                }),
            );
        }
        // The answers, until all came or the deadline passed; the rest join a later batch.
        let until = Instant::now() + IDENTITY_DEADLINE;
        while waiting
            .iter()
            .any(|(h, g)| b.ready.get(h).is_none_or(|(rg, _)| rg != g))
        {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(m @ Msg::Identity { .. }) | Ok(m @ Msg::Joined { .. }) => {
                    self.absorb(&mut b, inflight, m)
                }
                Ok(m) => backlog.push_back(m),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let Some(ours) = ours else {
            if noted {
                (self.changed)();
            }
            return;
        };
        let mut ready = Vec::new();
        for (host, (gen, r)) in std::mem::take(&mut b.ready) {
            let Some(seen) = self.sync.current(&host, gen) else {
                continue;
            };
            let retry = retries.get(&host).copied().unwrap_or(false);
            let v = match r {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("xshell: ring.identity on {host} failed: {}", e.message);
                    self.note(&host, "failed", Some(e.message), None);
                    noted = true;
                    continue;
                }
            };
            let identity = match LocalIdentity::from_json(&v) {
                Ok(i) => i,
                Err(e) => {
                    self.note(&host, "failed", Some(e), None);
                    noted = true;
                    continue;
                }
            };
            let observed = observed_ring(&v);
            let local = host == LOCAL_HOST_ID;
            if let Some((rid, version)) = &observed {
                if !local && rid != ours.as_str() {
                    // Another Desktop's devices: left alone until the user pairs it here.
                    let claim = Claim {
                        gen,
                        identity,
                        foreign: rid.clone(),
                        version: *version,
                        ours: ours.clone(),
                    };
                    self.note(&host, "other-ring", None, Some(claim));
                    noted = true;
                    continue;
                }
            }
            let expect = seen.cjoin.then(|| JoinExpect {
                ring_id: observed.map(|(r, _)| r),
                version: None,
            });
            let Some(r) = self.host_ref(&host, &identity) else {
                continue;
            };
            ready.push(Ready {
                host: r,
                identity,
                gen,
                link_gen: seen.link_gen,
                expect,
                retry,
            });
        }
        for (host, c, seen) in claims {
            if c.ours != ours {
                continue;
            }
            let Some(r) = self.host_ref(&host, &c.identity) else {
                continue;
            };
            ready.push(Ready {
                host: r,
                identity: c.identity,
                gen: c.gen,
                link_gen: seen.link_gen,
                expect: Some(JoinExpect {
                    ring_id: Some(c.foreign),
                    version: c.version,
                }),
                retry: false,
            });
        }
        if ready.is_empty() {
            if noted {
                (self.changed)();
            }
            return;
        }
        self.commit(ready, &ours);
    }

    fn host_ref(&self, host: &str, identity: &LocalIdentity) -> Option<HostRef> {
        if host == LOCAL_HOST_ID {
            return Some(HostRef {
                id: host.to_string(),
                name: identity.name.clone(),
                target: String::new(),
            });
        }
        let cfg = (self.host)(host)?.config();
        Some(HostRef {
            id: host.to_string(),
            name: cfg.name.clone(),
            target: target_of(&cfg),
        })
    }

    /// One Roster transaction for the batch, then the joins.
    /// `ours`: the Ring the batch was decided for; the transaction refuses it for another.
    fn commit(&self, ready: Vec<Ready>, ours: &RingId) {
        #[cfg(test)]
        if let Some(h) = self.before_commit.lock().unwrap().as_ref() {
            h();
        }
        let gens: HashMap<String, u64> = ready.iter().map(|r| (r.host.id.clone(), r.gen)).collect();
        let batch: Vec<(HostRef, LocalIdentity)> = ready
            .iter()
            .map(|r| (r.host.clone(), r.identity.clone()))
            .collect();
        let valid = |id: &str| {
            gens.get(id)
                .is_some_and(|g| self.sync.current(id, *g).is_some())
        };
        let (chain, outcomes) = match self.ring.ensure_host_daemons(&batch, &valid, Some(ours)) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                eprintln!("xshell: cannot add Hosts to the ring: {e}");
                for r in &ready {
                    self.note(&r.host.id, "failed", Some(e.clone()), None);
                }
                (self.changed)();
                return;
            }
        };
        let toks = tokens(&chain);
        for r in ready {
            let id = r.host.id.as_str();
            let outcome = outcomes.iter().find(|(h, _)| h == id).map(|(_, o)| *o);
            match outcome {
                Some(HostOutcome::Joined) => {
                    if self.sync.current(id, r.gen).is_none() {
                        continue;
                    }
                    let (Some(h), Some(tx)) = ((self.host)(id), self.sender()) else {
                        continue;
                    };
                    let (host, gen, retry) = (id.to_string(), r.gen, r.retry);
                    h.ring_join(
                        toks.clone(),
                        r.expect,
                        r.link_gen,
                        Box::new(move |res| {
                            let _ = tx.send(Msg::Joined {
                                host,
                                gen,
                                r: res,
                                retry,
                            });
                        }),
                    );
                }
                Some(HostOutcome::OtherRole) => self.note(
                    id,
                    "failed",
                    Some("its key is already one of your devices, with another role".into()),
                    None,
                ),
                Some(HostOutcome::Full) => self.note(id, "full", None, None),
                // Decided for a Ring that is gone: look again, against the new one.
                Some(HostOutcome::RingChanged) => {
                    if self.sync.current(id, r.gen).is_some() {
                        self.post(Msg::Job {
                            host: id.to_string(),
                            gen: r.gen,
                            claim: None,
                            retry: r.retry,
                        });
                    }
                }
                Some(HostOutcome::Stale) | None => {}
            }
        }
        (self.changed)();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ring::{DesktopRingConfig, RingObserver, RingView};
    use crate::{
        FileSource, HostConfig, Manager, ManagerConfig, Observer, StatusKind, Transport,
        TransportFactory, UnixSocketDialer,
    };
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixListener;
    use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
    use xshell_protocol::msg::{
        decode_inbound, encode_msg, encode_res, ClientMsg, Hello, ProtocolRange, ServerMsg,
        TerminalInfo,
    };
    use xshell_protocol::ring::{DeviceKeys, Role};

    const VERSION: &str = "1.5.0";
    const WAIT: Duration = Duration::from_secs(10);
    const ALL_CAPS: &[&str] = &["call", "term", "ring", "ring.cjoin"];

    /// A Daemon that answers `ring.identity` (reporting `ring`) and `ring.join`, and records
    /// every message it gets.
    struct FakeDaemon {
        path: std::path::PathBuf,
        keys: Arc<DeviceKeys>,
        seen: Arc<Mutex<Vec<ClientMsg>>>,
        _dir: tempfile::TempDir,
    }

    struct Opts {
        caps: &'static [&'static str],
        /// Never answer `ring.identity`.
        silent: bool,
        ring: Value,
    }

    impl Default for Opts {
        fn default() -> Self {
            Opts {
                caps: ALL_CAPS,
                silent: false,
                ring: Value::Null,
            }
        }
    }

    impl FakeDaemon {
        fn start(o: Opts) -> FakeDaemon {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("d.sock");
            let l = UnixListener::bind(&path).unwrap();
            let keys = Arc::new(DeviceKeys::generate().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            // What `ring.identity` reports as the membership; a join changes it.
            let rg = Arc::new(Mutex::new(o.ring));
            let (k, sn) = (keys.clone(), seen.clone());
            let caps: Vec<String> = o.caps.iter().map(|c| c.to_string()).collect();
            let silent = o.silent;
            std::thread::spawn(move || {
                for s in l.incoming() {
                    let Ok(mut s) = s else { return };
                    let (k, sn, rg, caps) = (k.clone(), sn.clone(), rg.clone(), caps.clone());
                    std::thread::spawn(move || {
                        let hello = ServerMsg::Hello(Hello {
                            protocol: ProtocolRange { min: 1, max: 1 },
                            version: VERSION.into(),
                            capabilities: caps,
                        });
                        let list = ServerMsg::Terminals {
                            list: Vec::<TerminalInfo>::new(),
                        };
                        let _ = s.write_all(&encode_msg(&hello, None).unwrap());
                        let _ = s.write_all(&encode_msg(&list, None).unwrap());
                        let mut r = BufReader::new(s.try_clone().unwrap());
                        while let Ok(Some(Frame::Json(b))) = read_frame(&mut r, MAX_FRAME_LEN) {
                            let Ok(m) = decode_inbound(&b) else { continue };
                            sn.lock().unwrap().push(m.msg.clone());
                            let Some(id) = m.id else { continue };
                            let res = match m.msg {
                                ClientMsg::RingIdentity if silent => continue,
                                ClientMsg::RingIdentity => Ok(serde_json::json!({
                                    "signKey": k.sign_key(),
                                    "noiseKey": k.noise_key(),
                                    "name": "fake",
                                    "ring": rg.lock().unwrap().clone(),
                                })),
                                ClientMsg::RingJoin { rosters, .. } => {
                                    let c = RosterChain::from_tokens(&rosters).unwrap();
                                    *rg.lock().unwrap() = serde_json::json!({
                                        "ringId": c.ring_id(),
                                        "version": c.head().version(),
                                    });
                                    Ok(serde_json::json!({ "version": rosters.len() }))
                                }
                                _ => Ok(Value::Null),
                            };
                            let _ = s.write_all(&encode_res(id, res));
                        }
                    });
                }
            });
            FakeDaemon {
                path,
                keys,
                seen,
                _dir: dir,
            }
        }

        fn joins(&self) -> Vec<(Vec<String>, Option<JoinExpect>)> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter_map(|m| match m {
                    ClientMsg::RingJoin { rosters, expect } => {
                        Some((rosters.clone(), expect.clone()))
                    }
                    _ => None,
                })
                .collect()
        }

        fn ring_messages(&self) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|m| matches!(m, ClientMsg::RingIdentity | ClientMsg::RingJoin { .. }))
                .count()
        }

        fn identities(&self) -> usize {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|m| matches!(m, ClientMsg::RingIdentity))
                .count()
        }
    }

    /// Every Host reached directly on its fake Daemon's socket.
    struct Sockets(Mutex<HashMap<String, std::path::PathBuf>>);

    impl TransportFactory for Sockets {
        fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
            Box::new(crate::LocalShellTransport::default())
        }
        fn direct(&self, cfg: &HostConfig) -> Option<Box<dyn crate::Dialer>> {
            let path = self.0.lock().unwrap().get(&cfg.id).cloned()?;
            Some(Box::new(UnixSocketDialer { path }))
        }
    }

    /// The app's Observer, minus the events: only the hooks under test.
    struct Obs(&'static HostSync);

    impl Observer for Obs {
        fn status(&self, s: &HostStatus) {
            self.0.observe(s);
        }
        fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
        fn renamed(&self, host: &str) {
            self.0.renamed(host);
        }
    }

    struct Fx {
        m: Arc<Manager>,
        sockets: Arc<Sockets>,
        sync: &'static HostSync,
        ring: Arc<DesktopRing>,
        _dir: tempfile::TempDir,
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            self.sync.shutdown();
            self.m.shutdown();
            self.ring.quit();
        }
    }

    fn fx() -> Fx {
        let sync: &'static HostSync = Box::leak(Box::new(HostSync::new()));
        let sockets = Arc::new(Sockets(Mutex::new(HashMap::new())));
        let mut c = ManagerConfig::new(
            VERSION,
            sockets.clone(),
            Arc::new(FileSource("/nonexistent".into())),
            Arc::new(Obs(sync)),
        );
        c.backoff_unit = Duration::from_millis(20);
        let dir = tempfile::tempdir().unwrap();
        struct Quiet;
        impl RingObserver for Quiet {
            fn changed(&self, _: &RingView) {}
        }
        let mut cfg = DesktopRingConfig::new(dir.path().join("ring"), "desk".into());
        // Nothing listens there; the Relay plays no part here.
        cfg.default_relay_url = "ws://127.0.0.1:9".into();
        cfg.timeouts.connect = Duration::from_millis(500);
        Fx {
            m: Arc::new(Manager::new(c)),
            sockets,
            sync,
            ring: DesktopRing::open(cfg, Arc::new(Quiet)),
            _dir: dir,
        }
    }

    fn host_cfg(id: &str, name: &str) -> HostConfig {
        HostConfig {
            id: id.into(),
            name: name.into(),
            ssh_target: format!("{id}.example"),
            color: None,
            daemon_command: None,
            launch_prefixes: Default::default(),
        }
    }

    impl Fx {
        fn worker(&self) -> Arc<SyncWorker> {
            let m = self.m.clone();
            SyncWorker::new(
                self.ring.clone(),
                self.sync,
                Box::new(move |id| m.host(id)),
                Box::new(|| {}),
            )
        }

        fn serve(&self, id: &str, d: &FakeDaemon) {
            self.sockets
                .0
                .lock()
                .unwrap()
                .insert(id.into(), d.path.clone());
        }

        fn local(&self, d: &FakeDaemon) {
            self.serve(LOCAL_HOST_ID, d);
            self.m.set_local(HostConfig {
                id: LOCAL_HOST_ID.into(),
                name: LOCAL_HOST_ID.into(),
                ssh_target: String::new(),
                color: None,
                daemon_command: None,
                launch_prefixes: Default::default(),
            });
        }

        fn wait_usable(&self, id: &str) {
            wait("usable", || {
                self.m.host(id).is_some_and(|h| h.status().usable())
            });
        }

        fn member_name(&self, d: &FakeDaemon) -> Option<String> {
            self.ring
                .chain()?
                .head()
                .member(&d.keys.sign_key())
                .map(|m| m.name.clone())
        }
    }

    fn wait(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The latest join, once it carries version `n`. A join of a newer version may still be
    /// on its way when an older one already arrived, so wait for it rather than sampling.
    fn joined_with(d: &FakeDaemon, n: usize) -> RosterChain {
        let head = |d: &FakeDaemon| {
            d.joins()
                .last()
                .map(|(t, _)| RosterChain::from_tokens(t).unwrap())
        };
        let deadline = Instant::now() + WAIT;
        loop {
            let c = head(d);
            if let Some(c) = c.filter(|c| c.head().version() as usize == n) {
                return c;
            }
            assert!(
                Instant::now() < deadline,
                "no join of version {n}; the latest: {:?}",
                head(d).map(|c| c.head().version())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn status(host: &str, kind: StatusKind, caps: &[&str], link_gen: u64) -> HostStatus {
        let mut s = HostStatus::initial(host, VERSION, 1);
        s.set_kind(kind);
        s.daemon_capabilities = caps.iter().map(|c| c.to_string()).collect();
        s.link_generation = link_gen;
        s
    }

    #[test]
    fn jobs_of_an_old_connection_are_stale_per_host() {
        let s = HostSync::new();
        let (tx, rx) = mpsc::channel();
        let gen_of = |m: Msg| match m {
            Msg::Job { host, gen, .. } => (host, gen),
            _ => panic!("not a job"),
        };
        s.observe(&status("local", StatusKind::Connected, ALL_CAPS, 1));
        s.observe(&status("h_aaaaaaaa", StatusKind::Reconnecting, &[], 0));
        s.attach(tx);
        let (h, g) = gen_of(rx.try_recv().unwrap());
        assert_eq!(h, "local");
        assert!(rx.try_recv().is_err(), "nothing for a Host not connected");
        assert!(s.current("local", g).is_some());
        // Repeated reports of the same connection queue nothing new.
        s.observe(&status("local", StatusKind::Connected, ALL_CAPS, 1));
        assert!(rx.try_recv().is_err());
        // Another Host's changes leave this one's job current.
        s.observe(&status("h_aaaaaaaa", StatusKind::Connected, ALL_CAPS, 4));
        let (h2, g2) = gen_of(rx.try_recv().unwrap());
        assert_eq!(h2, "h_aaaaaaaa");
        assert!(s.current("local", g).is_some() && s.current(&h2, g2).is_some());
        // A reconnect seen only as a new link generation is a new connection.
        s.observe(&status("local", StatusKind::Connected, ALL_CAPS, 2));
        let (_, g3) = gen_of(rx.try_recv().unwrap());
        assert!(s.current("local", g).is_none() && s.current("local", g3).is_some());
        assert_eq!(s.current("local", g3).unwrap().link_gen, 2);
        s.observe(&status("local", StatusKind::Offline, &[], 0));
        assert!(s.current("local", g3).is_none());
        assert!(rx.try_recv().is_err());
        s.poke_all();
        assert_eq!(gen_of(rx.try_recv().unwrap()), (h2.clone(), g2));
        assert!(rx.try_recv().is_err());
        s.renamed(&h2);
        assert_eq!(gen_of(rx.try_recv().unwrap()), (h2.clone(), g2));
        s.shutdown();
        assert!(s.current(&h2, g2).is_none());
        s.observe(&status("local", StatusKind::Connected, ALL_CAPS, 3));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn old_daemon_without_ring_capability_gets_no_message() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let d = FakeDaemon::start(Opts {
            caps: &["call", "term"],
            ..Default::default()
        });
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Old box")])
            .unwrap();
        wait("too old", || w.state_of("h_aaaaaaaa") == Some("too-old"));
        let st = w.host_states();
        assert_eq!(st.len(), 1);
        assert_eq!((st[0].name.as_str(), st[0].state), ("Old box", "too-old"));
        std::thread::sleep(BATCH_WINDOW * 2);
        assert_eq!(d.ring_messages(), 0);
        assert_eq!(f.ring.chain().unwrap().head().version(), 1);
    }

    #[test]
    fn other_ring_left_alone_until_claimed() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let d = FakeDaemon::start(Opts {
            ring: serde_json::json!({"ringId": "foreign", "version": 3}),
            ..Default::default()
        });
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Shared")])
            .unwrap();
        wait("other ring", || {
            w.state_of("h_aaaaaaaa") == Some("other-ring")
        });
        std::thread::sleep(BATCH_WINDOW * 2);
        assert!(d.joins().is_empty(), "left alone");
        assert_eq!(f.ring.chain().unwrap().head().version(), 1);
        assert!(w.claim("h_bbbbbbbb").is_err(), "nothing to claim there");
        w.claim("h_aaaaaaaa").unwrap();
        let c = joined_with(&d, 2);
        assert!(c.head().member(&d.keys.sign_key()).is_some());
        let (_, expect) = d.joins().last().unwrap().clone();
        assert_eq!(
            expect,
            Some(JoinExpect {
                ring_id: Some("foreign".into()),
                version: Some(3)
            })
        );
        wait("claimed", || w.state_of("h_aaaaaaaa").is_none());
        assert!(w.host_states().is_empty());
    }

    /// The destination Ring is replaced between the claim's checks and the transaction: no
    /// transfer, no Roster change, and the Host is looked at again against the new Ring.
    #[test]
    fn a_claim_for_a_replaced_ring_transfers_nothing() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let d = FakeDaemon::start(Opts {
            ring: serde_json::json!({"ringId": "foreign", "version": 3}),
            ..Default::default()
        });
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        let dir = f._dir.path().join("ring");
        let replaced = Arc::new(Mutex::new(None::<RosterChain>));
        let rep = replaced.clone();
        *w.before_commit.lock().unwrap() = Some(Box::new(move || {
            let mut g = rep.lock().unwrap();
            if g.is_some() {
                return;
            }
            // Another window started over: a new Ring on disk.
            let keys = Arc::new(DeviceKeys::generate().unwrap());
            let v1 = xshell_protocol::ring::SignedRoster::genesis(
                &*keys,
                keys.noise_key(),
                "desk",
                "ws://127.0.0.1:9",
                1,
            )
            .unwrap();
            let chain = RosterChain::from_chain(vec![v1]).unwrap();
            let st = crate::ring::store::RingState {
                keys,
                chain: chain.clone(),
                local_member: None,
                pending_move: None,
                host_members: Default::default(),
            };
            crate::ring::store::Store::new(dir.clone())
                .transact(|_, _| Ok((Some(st), ())))
                .unwrap();
            *g = Some(chain);
        }));
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Shared")])
            .unwrap();
        wait("other ring", || {
            w.state_of("h_aaaaaaaa") == Some("other-ring")
        });
        w.claim("h_aaaaaaaa").unwrap();
        wait("replaced", || replaced.lock().unwrap().is_some());
        let new = replaced.lock().unwrap().clone().unwrap();
        // Looked at again: still another Ring's, against the new one too.
        wait("adopted", || {
            f.ring.chain().is_some_and(|c| c.ring_id() == new.ring_id())
        });
        std::thread::sleep(BATCH_WINDOW * 3);
        assert!(d.joins().is_empty(), "no transfer");
        let c = f.ring.chain().unwrap();
        assert_eq!(c, new, "no Roster change");
        assert!(c.head().member(&d.keys.sign_key()).is_none());
        assert_eq!(w.state_of("h_aaaaaaaa"), Some("other-ring"));
    }

    #[test]
    fn a_claim_needs_a_daemon_that_joins_conditionally() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let d = FakeDaemon::start(Opts {
            caps: &["call", "term", "ring"],
            ring: serde_json::json!({"ringId": "foreign", "version": 3}),
            ..Default::default()
        });
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Shared")])
            .unwrap();
        wait("other ring", || {
            w.state_of("h_aaaaaaaa") == Some("other-ring")
        });
        assert!(w.claim("h_aaaaaaaa").is_err());
        std::thread::sleep(BATCH_WINDOW * 2);
        assert!(d.joins().is_empty());
    }

    #[test]
    fn hosts_connecting_together_make_one_version() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let (a, b) = (
            FakeDaemon::start(Opts::default()),
            FakeDaemon::start(Opts::default()),
        );
        f.serve("h_aaaaaaaa", &a);
        f.serve("h_bbbbbbbb", &b);
        let w = f.worker();
        w.start().unwrap();
        f.m.configure(vec![
            host_cfg("h_aaaaaaaa", "Alpha"),
            host_cfg("h_bbbbbbbb", "Beta"),
        ])
        .unwrap();
        let ca = joined_with(&a, 2);
        let cb = joined_with(&b, 2);
        assert_eq!(ca, cb);
        assert_eq!(f.member_name(&a).as_deref(), Some("Alpha"));
        assert_eq!(f.member_name(&b).as_deref(), Some("Beta"));
        // Unpaired Daemons with `ring.cjoin` are joined on the condition they still are.
        for d in [&a, &b] {
            assert_eq!(
                d.joins()[0].1,
                Some(JoinExpect {
                    ring_id: None,
                    version: None
                })
            );
        }
        let v = f.ring.view();
        let ids: Vec<_> = v.members.iter().map(|m| m.host_id.clone()).collect();
        assert_eq!(
            ids,
            [None, Some("h_aaaaaaaa".into()), Some("h_bbbbbbbb".into())]
        );
        assert!(w.host_states().is_empty());
    }

    #[test]
    fn local_and_remote_hosts_sync_through_the_observer() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let (l, r) = (
            FakeDaemon::start(Opts::default()),
            FakeDaemon::start(Opts::default()),
        );
        f.serve("h_aaaaaaaa", &r);
        let w = f.worker();
        w.start().unwrap();
        f.local(&l);
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Remote")])
            .unwrap();
        wait("both joined", || {
            !l.joins().is_empty() && !r.joins().is_empty()
        });
        wait("both in the head", || {
            let c = f.ring.chain().unwrap();
            c.head().member(&l.keys.sign_key()).is_some()
                && c.head().member(&r.keys.sign_key()).is_some()
        });
        let v = f.ring.view();
        assert!(v
            .members
            .iter()
            .any(|m| m.this_computer && m.sign_key == l.keys.sign_key() && m.name == "fake"));
        assert!(v.members.iter().any(|m| m.sign_key == r.keys.sign_key()
            && m.name == "Remote"
            && m.host_id.as_deref() == Some("h_aaaaaaaa")
            && m.role == Role::Daemon));
    }

    #[test]
    fn host_connected_before_attach_is_reconciled() {
        let f = fx();
        let d = FakeDaemon::start(Opts::default());
        f.serve("h_aaaaaaaa", &d);
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Early")])
            .unwrap();
        f.wait_usable("h_aaaaaaaa");
        f.ring.enable(None, false).unwrap();
        // The worker comes after the connection: attaching reconciles it.
        let w = f.worker();
        w.start().unwrap();
        joined_with(&d, 2);
    }

    #[test]
    fn enabling_later_and_poking_adds_connected_hosts() {
        let f = fx();
        let d = FakeDaemon::start(Opts::default());
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Box")]).unwrap();
        f.wait_usable("h_aaaaaaaa");
        std::thread::sleep(BATCH_WINDOW * 2);
        assert_eq!(d.ring_messages(), 0, "no Ring: nothing asked");
        f.ring.enable(None, false).unwrap();
        f.sync.poke_all();
        joined_with(&d, 2);
    }

    #[test]
    fn removed_host_drops_from_host_states() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let d = FakeDaemon::start(Opts {
            caps: &["call", "term"],
            ..Default::default()
        });
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Old")]).unwrap();
        wait("too old", || w.host_states().len() == 1);
        f.m.configure(vec![]).unwrap();
        assert!(w.host_states().is_empty());
    }

    /// A Host that never answers does not hold up the others: they are committed after the
    /// identity deadline, and the silent one joins nothing.
    #[test]
    fn a_silent_host_does_not_hold_up_the_others() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let ok = FakeDaemon::start(Opts::default());
        let mute = FakeDaemon::start(Opts {
            silent: true,
            ..Default::default()
        });
        f.serve("h_aaaaaaaa", &ok);
        f.serve("h_bbbbbbbb", &mute);
        let w = f.worker();
        w.start().unwrap();
        let t = Instant::now();
        f.m.configure(vec![
            host_cfg("h_aaaaaaaa", "Fine"),
            host_cfg("h_bbbbbbbb", "Mute"),
        ])
        .unwrap();
        joined_with(&ok, 2);
        assert!(
            t.elapsed() < Duration::from_secs(8),
            "the healthy Host waited for the silent one: {:?}",
            t.elapsed()
        );
        assert!(mute.joins().is_empty());
        assert!(f.member_name(&mute).is_none());
    }

    /// A reconnect between the identity and the Roster transaction: that identity is
    /// discarded, and requests for the replaced connection are refused by the link.
    #[test]
    fn identity_of_a_replaced_connection_is_discarded() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let d = FakeDaemon::start(Opts::default());
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sync, fr) = (f.sync, fired.clone());
        *w.before_commit.lock().unwrap() = Some(Box::new(move || {
            if !fr.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // As the Observer reports a new link of the same Host.
                let (seen, _) = sync.seen("h_aaaaaaaa").unwrap();
                let mut s = status("h_aaaaaaaa", StatusKind::Connected, ALL_CAPS, 0);
                s.link_generation = seen.link_gen + 100;
                sync.observe(&s);
            }
        }));
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Box")]).unwrap();
        wait("fired", || fired.load(std::sync::atomic::Ordering::SeqCst));
        std::thread::sleep(BATCH_WINDOW * 3);
        assert_eq!(f.ring.chain().unwrap().head().version(), 1, "discarded");
        // The fake generation is not the link's: the link refuses requests for it, so no
        // join happens through it either.
        assert!(d.joins().is_empty());
        assert!(d.identities() >= 1);
    }

    #[test]
    fn rename_in_settings_renames_the_member() {
        let f = fx();
        f.ring.enable(None, false).unwrap();
        let d = FakeDaemon::start(Opts::default());
        f.serve("h_aaaaaaaa", &d);
        let w = f.worker();
        w.start().unwrap();
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "Before")])
            .unwrap();
        joined_with(&d, 2);
        let added = f
            .ring
            .chain()
            .unwrap()
            .head()
            .member(&d.keys.sign_key())
            .unwrap()
            .added_at;
        f.m.configure(vec![host_cfg("h_aaaaaaaa", "After")])
            .unwrap();
        wait("renamed", || f.member_name(&d).as_deref() == Some("After"));
        let c = f.ring.chain().unwrap();
        assert_eq!(c.head().version(), 3, "a name-only version");
        assert_eq!(c.head().member(&d.keys.sign_key()).unwrap().added_at, added);
        // The Daemon follows the new version on the same connection.
        joined_with(&d, 3);
    }
}

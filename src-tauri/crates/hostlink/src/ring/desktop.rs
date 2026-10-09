//! [`DesktopRing`]: this Desktop as the Ring's creator and signer. It creates the Ring,
//! signs every new Roster version (a new Relay URL, the local Daemon added or replaced),
//! keeps a Relay connection through the [`Connector`], and builds the [`RingView`] that
//! Settings → Mobile shows.
//!
//! Every mutation is a [`Store::transact`] transaction: serialized in this process and
//! across app instances, applied to the state as on disk now, written atomically. The
//! Connector's callbacks persist newer chains through the same transactions and never hold a
//! lock of their own while they do.

use super::store::{HostMember, RingState, Store};
use crate::LOCAL_HOST_ID;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use xshell_protocol::ring::relay::{
    ByeReason, Connector, ConnectorConfig, ConnectorEvents, LinkState, MemberPresence, MoveJob,
    MoveState, RingClientConfig, RingTimeouts,
};
use xshell_protocol::ring::roster::MAX_MEMBERS;
use xshell_protocol::ring::url::RelayUrl;
use xshell_protocol::ring::{
    member_name, verify_genesis, DeviceKeys, Member, NoiseKey, RingId, Role, Roster, RosterChain,
    SignKey, SignedRoster,
};

/// The local Daemon's Ring identity, from its `ring.identity` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdentity {
    pub sign_key: SignKey,
    pub noise_key: NoiseKey,
    pub name: String,
}

impl LocalIdentity {
    pub fn from_json(v: &Value) -> Result<LocalIdentity, String> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("ring.identity without {k}"))
        };
        Ok(LocalIdentity {
            sign_key: SignKey::parse(s("signKey")?).map_err(|e| e.to_string())?,
            noise_key: NoiseKey::parse(s("noiseKey")?).map_err(|e| e.to_string())?,
            name: xshell_protocol::ring::member_name(s("name").unwrap_or(""), "this computer"),
        })
    }
}

/// A Host whose Daemon belongs in the Ring, as the caller sees it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRef {
    /// The Host's id; [`crate::LOCAL_HOST_ID`] for the Local Host.
    pub id: String,
    /// Its member name: the Settings → Hosts name (the hostname for the Local Host).
    pub name: String,
    /// Where it is reached ([`HostMember::target`]); ignored for the Local Host.
    pub target: String,
}

/// What [`DesktopRing::ensure_host_daemons`] did for one Host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostOutcome {
    /// Its Daemon is in the head as a `daemon`: it can join.
    Joined,
    /// Its key is in the Ring with another role: refused for this Host only.
    OtherRole,
    /// The Roster has no room for it.
    Full,
    /// Its identity was for a connection that is no longer current: discarded.
    Stale,
    /// The batch was decided for a Ring that is no longer this Desktop's: discarded.
    RingChanged,
}

/// Per Host id, what [`DesktopRing::ensure_host_daemons`] did.
pub type HostOutcomes = Vec<(String, HostOutcome)>;

/// The changes one batch makes ([`plan_hosts`]).
struct HostPlan {
    remove: Vec<SignKey>,
    add: Vec<Member>,
    rename: Vec<(SignKey, String)>,
    /// Every Host's key (and target) afterwards, the Local Host's included.
    after: BTreeMap<String, (SignKey, String)>,
    outcomes: Vec<(String, HostOutcome)>,
}

impl HostPlan {
    fn edits_roster(&self) -> bool {
        !self.remove.is_empty() || !self.add.is_empty() || !self.rename.is_empty()
    }
}

/// Every Host's key before a batch: the Remote Hosts' entries and the Local Host.
fn mapping(s: &RingState) -> BTreeMap<String, (SignKey, String)> {
    let mut m: BTreeMap<String, (SignKey, String)> = s
        .host_members
        .iter()
        .map(|(id, h)| (id.clone(), (h.sign_key, h.target.clone())))
        .collect();
    if let Some(k) = s.local_member {
        m.insert(LOCAL_HOST_ID.to_string(), (k, String::new()));
    }
    m
}

/// What the batch changes, decided on the whole batch at once:
/// - a key already in the head as a `daemon` only updates the mapping (Host entries that
///   reach the same Daemon share its member);
/// - a key in the head with another role is refused for that Host;
/// - a new key is added; the Host's previous key is removed in the same version only when
///   it is a reinstall (the same place, a new key) and no Host, the Local Host included,
///   maps to it afterwards. A Host retargeted elsewhere keeps its old member (#22 removes
///   members);
/// - the member's name is that of the Host with the smallest id that maps to it, when that
///   Host is in the batch;
/// - new keys that do not fit under [`MAX_MEMBERS`] are refused as `full`, the last (by
///   their owner's id) first.
fn plan_hosts(s: &RingState, batch: &[&(HostRef, LocalIdentity)], now: u64) -> HostPlan {
    let head = s.chain.head().roster();
    let before = mapping(s);
    // One entry per Host (the last given), in id order.
    let mut want: BTreeMap<&str, &(HostRef, LocalIdentity)> = BTreeMap::new();
    for e in batch {
        want.insert(e.0.id.as_str(), e);
    }
    let mut outcomes = Vec::new();
    let mut candidates = Vec::new();
    for (id, e) in &want {
        match head.member(&e.1.sign_key) {
            Some(m) if m.role != Role::Daemon => {
                outcomes.push((id.to_string(), HostOutcome::OtherRole))
            }
            _ => candidates.push(*e),
        }
    }
    let target = |h: &HostRef| {
        if h.id == LOCAL_HOST_ID {
            String::new()
        } else {
            h.target.clone()
        }
    };
    let reinstall = |h: &HostRef, old_target: &str| h.id == LOCAL_HOST_ID || h.target == old_target;
    let mut rejected: Vec<SignKey> = Vec::new();
    let (after, remove, new_keys) = loop {
        let accepted: Vec<_> = candidates
            .iter()
            .filter(|e| !rejected.contains(&e.1.sign_key))
            .collect();
        let mut after = before.clone();
        for e in &accepted {
            after.insert(e.0.id.clone(), (e.1.sign_key, target(&e.0)));
        }
        let mut new_keys: Vec<SignKey> = Vec::new();
        for e in &accepted {
            if head.member(&e.1.sign_key).is_none() && !new_keys.contains(&e.1.sign_key) {
                new_keys.push(e.1.sign_key);
            }
        }
        let mut remove: Vec<SignKey> = Vec::new();
        // New keys that replace a removed one (net zero members).
        let mut replacing: Vec<SignKey> = Vec::new();
        for e in &accepted {
            let Some((old, _)) = before.get(&e.0.id) else {
                continue;
            };
            if *old == e.1.sign_key || remove.contains(old) {
                continue;
            }
            let still_mapped = after.values().any(|(k, _)| k == old);
            // Every Host moving away from `old` must be a reinstall, not a retarget.
            let all_reinstalls = accepted.iter().all(|x| match before.get(&x.0.id) {
                Some((k, t)) if k == old && x.1.sign_key != *old => reinstall(&x.0, t),
                _ => true,
            });
            let daemon = head.member(old).is_some_and(|m| m.role == Role::Daemon);
            if !still_mapped && all_reinstalls && daemon {
                remove.push(*old);
                for x in &accepted {
                    let moved = before.get(&x.0.id).is_some_and(|(k, _)| k == old);
                    if moved && new_keys.contains(&x.1.sign_key) {
                        replacing.push(x.1.sign_key);
                    }
                }
            }
        }
        let count = head.members.len() - remove.len() + new_keys.len();
        if count <= MAX_MEMBERS {
            break (after, remove, new_keys);
        }
        // `count` exceeds the (valid) head's size only through new keys. Refuse additions
        // first (the last by owner id), keeping the one-for-one replacements that fit.
        let pick = new_keys
            .iter()
            .rev()
            .find(|k| !replacing.contains(k))
            .or(new_keys.last())
            .expect("a new key");
        rejected.push(*pick);
    };
    for e in &candidates {
        let o = if rejected.contains(&e.1.sign_key) {
            HostOutcome::Full
        } else {
            HostOutcome::Joined
        };
        outcomes.push((e.0.id.clone(), o));
    }
    // The name each mapped key should carry: its smallest-id Host's, when that one is in the
    // batch (`after` iterates in id order, so the first owner is the smallest).
    let name_of = |key: &SignKey| -> Option<String> {
        let (owner, _) = after.iter().find(|(_, (k, _))| k == key)?;
        let e = want.get(owner.as_str())?;
        Some(member_name(&e.0.name, &e.1.name))
    };
    let mut add = Vec::new();
    for k in &new_keys {
        let e = candidates
            .iter()
            .find(|e| e.1.sign_key == *k)
            .expect("a new key has a candidate");
        let name = name_of(k).unwrap_or_else(|| member_name(&e.0.name, &e.1.name));
        add.push(Member::new(&name, Role::Daemon, *k, e.1.noise_key, now));
    }
    let mut rename = Vec::new();
    for m in &head.members {
        if m.role != Role::Daemon || remove.contains(&m.sign_key) {
            continue;
        }
        if let Some(name) = name_of(&m.sign_key) {
            if name != m.name {
                rename.push((m.sign_key, name));
            }
        }
    }
    HostPlan {
        remove,
        add,
        rename,
        after,
        outcomes,
    }
}

/// Told every time the view may have changed. Called without any Ring lock held.
pub trait RingObserver: Send + Sync {
    fn changed(&self, view: &RingView);
}

#[derive(Clone)]
pub struct DesktopRingConfig {
    /// The private directory holding `ring.json`.
    pub dir: PathBuf,
    /// This Desktop's member name (the hostname).
    pub name: String,
    /// What the UI offers as the Hosted Relay.
    pub hosted_relay_url: String,
    /// The Relay a new Ring starts on (the Hosted one unless overridden).
    pub default_relay_url: String,
    pub timeouts: RingTimeouts,
    pub backoff_unit: Duration,
    /// Attempts at the old Relay per batch before a Relay move is reported failed.
    pub move_attempts: u32,
    /// How often an instance that does not own the connection re-reads the state and tries
    /// to take the connection over.
    pub owner_retry: Duration,
}

impl DesktopRingConfig {
    pub fn new(dir: PathBuf, name: String) -> Self {
        let hosted = super::HOSTED_RELAY_URL.to_string();
        DesktopRingConfig {
            dir,
            name,
            default_relay_url: hosted.clone(),
            hosted_relay_url: hosted,
            timeouts: RingTimeouts::default(),
            backoff_unit: Duration::from_secs(1),
            move_attempts: 5,
            owner_retry: Duration::from_secs(2),
        }
    }
}

/// One member's presence as Settings → Mobile shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceView {
    /// `online`, `closed` (xshell closed), `unreachable`, `never` (never connected) or
    /// `unknown` (this Desktop is not connected to the Relay).
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

impl PresenceView {
    fn of(p: &MemberPresence) -> Self {
        let (kind, reason, at) = match p {
            MemberPresence::Online { since } => ("online", None, *since),
            MemberPresence::NeverConnected => ("never", None, None),
            MemberPresence::Closed { reason, at } => ("closed", Some(reason.clone()), *at),
            MemberPresence::Unreachable { at } => ("unreachable", None, *at),
        };
        PresenceView { kind, reason, at }
    }

    fn unknown() -> Self {
        PresenceView {
            kind: "unknown",
            reason: None,
            at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberView {
    pub name: String,
    /// `desktop`, `daemon` or `mobile`.
    pub role: Role,
    pub sign_key: SignKey,
    pub this_app: bool,
    pub this_computer: bool,
    /// The configured Remote Host (the smallest id) whose Daemon this member is.
    pub host_id: Option<String>,
    pub presence: PresenceView,
}

/// A Relay move still owed to the old Relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveView {
    /// `moving`, or `failed` (a batch of attempts failed; retried until acknowledged).
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What Settings → Mobile shows (the `ring:status` event and the commands' answer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RingView {
    pub enabled: bool,
    pub ring_id: Option<RingId>,
    pub version: Option<u64>,
    pub relay_url: Option<String>,
    pub hosted_relay_url: String,
    /// `off`, `connecting`, `connected`, `waiting`, `stopped` or `other-window` (another
    /// instance runs the connection).
    pub connection: &'static str,
    /// While `waiting`: seconds until the next attempt.
    pub retry_in: Option<u64>,
    pub connection_error: Option<String>,
    pub limited: bool,
    #[serde(rename = "move")]
    pub moving: Option<MoveView>,
    /// The stored state could not be used: `recovered` (an unreadable file was moved aside;
    /// Mobile access starts over) or `unreadable` (refused, left in place).
    pub problem: Option<String>,
    pub problem_detail: Option<String>,
    pub members: Vec<MemberView>,
}

/// What the user must do before a moved-aside Ring is replaced by a new one.
pub const START_OVER_REQUIRED: &str =
    "the mobile access settings were unreadable; start over to make a new ring";
/// A Relay change while the previous one is still being published on the old Relay.
pub const MOVE_PENDING: &str = "the previous relay change is still reaching your paired devices";

#[derive(Default)]
struct Live {
    state: Option<RingState>,
    connector: Option<Arc<Connector>>,
    /// `ring-connector.lock`, held while this instance runs the Connector.
    owner: Option<std::fs::File>,
    /// Another instance holds the owner lock: this one shows the state from disk.
    other_window: bool,
    link: Option<LinkState>,
    moving: Option<MoveState>,
    problem: Option<(String, String)>,
    quitting: bool,
}

pub struct DesktopRing {
    cfg: DesktopRingConfig,
    store: Store,
    observer: Arc<dyn RingObserver>,
    live: Mutex<Live>,
    /// Serializes every commit with the adoption of its result (into `live` and the
    /// Connector), so results are adopted in commit order.
    commit: Mutex<()>,
    me: Weak<DesktopRing>,
    /// Test hook: runs between a commit and the adoption of its result.
    #[cfg(test)]
    after_commit: Mutex<Option<Box<dyn Fn() + Send>>>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Events(Weak<DesktopRing>);

impl ConnectorEvents for Events {
    fn state(&self, s: &LinkState) {
        if let Some(r) = self.0.upgrade() {
            r.lock().link = Some(s.clone());
            r.emit();
        }
    }

    fn roster(&self, chain: &RosterChain) {
        if let Some(r) = self.0.upgrade() {
            r.persist_newer(chain);
            r.emit();
        }
    }

    fn presence(&self, _key: SignKey, _p: &MemberPresence) {
        if let Some(r) = self.0.upgrade() {
            r.emit();
        }
    }

    fn moved(&self, m: &MoveState) {
        if let Some(r) = self.0.upgrade() {
            if let MoveState::Done { job } = m {
                r.clear_pending_move(job);
            }
            r.lock().moving = Some(m.clone());
            r.emit();
        }
    }
}

impl DesktopRing {
    /// Opens the stored Ring, if any, and resumes its Relay connection (when no other
    /// instance runs it). Every `owner_retry` it checks the state on disk ([`Store::signature`])
    /// and reconciles with commits of other instances; one that does not own the connection
    /// also retries taking it over.
    pub fn open(cfg: DesktopRingConfig, observer: Arc<dyn RingObserver>) -> Arc<DesktopRing> {
        let store = Store::new(cfg.dir.clone());
        let ring = Arc::new_cyclic(|me| DesktopRing {
            cfg,
            store,
            observer,
            live: Mutex::new(Live::default()),
            commit: Mutex::new(()),
            me: me.clone(),
            #[cfg(test)]
            after_commit: Mutex::new(None),
        });
        let mut seen = ring.store.signature();
        ring.reload();
        let weak = Arc::downgrade(&ring);
        let every = ring.cfg.owner_retry;
        let _ = std::thread::Builder::new()
            .name("ring-reconcile".into())
            .spawn(move || loop {
                std::thread::sleep(every);
                let Some(r) = weak.upgrade() else { return };
                let (quitting, other) = {
                    let l = r.lock();
                    (l.quitting, l.other_window)
                };
                if quitting {
                    return;
                }
                let now = r.store.signature();
                if now != seen || other {
                    seen = now;
                    r.reload();
                    r.emit();
                }
            });
        ring
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_commit(&self) -> std::sync::MutexGuard<'_, ()> {
        self.commit.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self) {
        self.observer.changed(&self.view());
    }

    /// Re-reads the state from disk and adopts it (under the commit lock, fenced), then
    /// tries to run the Connector.
    fn reload(&self) {
        {
            let _c = self.lock_commit();
            if let Ok(Some(s)) = self.tx(|cur, _| Ok((None, cur))) {
                self.adopt(s);
            }
        }
        self.ensure_connector();
    }

    /// A transaction whose recovery metadata reaches the view, whether it succeeded or not.
    /// Moved-aside settings put the view in the recovery-required state: the Ring held in
    /// memory is dropped (its Connector retired) until an explicit start-over.
    fn tx<R>(
        &self,
        f: impl FnOnce(
            Option<RingState>,
            &super::store::Loaded,
        ) -> Result<(Option<RingState>, R), String>,
    ) -> Result<R, String> {
        let (r, loaded) = self.store.transact_full(f);
        let retire = {
            let mut l = self.lock();
            match (&loaded, &r) {
                (Some(loaded), _) => match &loaded.moved_aside {
                    Some(p) => {
                        l.problem = Some(("recovered".to_string(), p.display().to_string()));
                        l.state = None;
                        l.moving = None;
                        l.link = None;
                        l.connector.take()
                    }
                    None => {
                        l.problem = None;
                        None
                    }
                },
                // Not even loaded: the file or its directory is refused.
                (None, Err(e)) => {
                    l.problem = Some(("unreadable".to_string(), e.clone()));
                    None
                }
                (None, Ok(_)) => None,
            }
        };
        if let Some(old) = retire {
            let _ = std::thread::Builder::new()
                .name("ring-retire".into())
                .spawn(move || old.stop(ByeReason::quit()));
        }
        r
    }

    #[cfg(test)]
    fn hook(&self) {
        if let Some(h) = self.after_commit.lock().unwrap().as_ref() {
            h();
        }
    }

    #[cfg(not(test))]
    fn hook(&self) {}

    /// Adopts a committed state (this instance's or another's) into `live` and the
    /// Connector. Under the commit lock; a state older than the one held (same Ring, lower
    /// version) is refused as stale, and an unchanged one is not handed to the Connector.
    fn adopt(&self, s: RingState) -> bool {
        let (retire, connector) = {
            let mut l = self.lock();
            let mut unchanged = false;
            if let Some(cur) = &l.state {
                if cur.chain.ring_id() == s.chain.ring_id()
                    && s.chain.head().version() < cur.chain.head().version()
                {
                    return false;
                }
                unchanged = cur.chain == s.chain
                    && cur.pending_move == s.pending_move
                    && cur.local_member == s.local_member
                    && cur.host_members == s.host_members;
            }
            let other_ring = l
                .state
                .as_ref()
                .is_some_and(|c| c.chain.ring_id() != s.chain.ring_id());
            l.state = Some(s.clone());
            if other_ring {
                l.moving = None;
                l.link = None;
                (l.connector.take(), None)
            } else if unchanged && l.connector.is_some() {
                return true;
            } else {
                (None, l.connector.clone())
            }
        };
        if let Some(old) = retire {
            // A Ring started over: the old Connector says goodbye on its own thread (its
            // callbacks may wait for the commit lock held here).
            let _ = std::thread::Builder::new()
                .name("ring-retire".into())
                .spawn(move || old.stop(ByeReason::quit()));
        }
        match connector {
            Some(c) => c.set_chain(s.chain.clone(), s.pending_move.clone()),
            None => self.ensure_connector(),
        }
        true
    }

    /// Starts the Connector for the state held, once, if this instance can own it. Having
    /// just won the owner lock, it re-reads the state from disk first: the previous owner
    /// may have committed since this instance last looked.
    fn ensure_connector(&self) {
        let fresh = {
            let mut l = self.lock();
            if l.connector.is_some() || l.quitting || l.state.is_none() {
                return;
            }
            if l.owner.is_some() {
                false
            } else {
                let lock = super::store::open_lock(&self.cfg.dir.join("ring-connector.lock"));
                match lock.map(|f| f.try_lock().map(|_| f)) {
                    Ok(Ok(f)) => {
                        l.owner = Some(f);
                        l.other_window = false;
                        true
                    }
                    Ok(Err(std::fs::TryLockError::WouldBlock)) => {
                        l.other_window = true;
                        return;
                    }
                    Ok(Err(std::fs::TryLockError::Error(e))) | Err(e) => {
                        l.link = Some(LinkState::Stopped {
                            error: Some(format!("cannot take the connection lock: {e}")),
                        });
                        return;
                    }
                }
            }
        };
        if fresh {
            if let Ok((Some(disk), _)) = self.store.load() {
                let mut l = self.lock();
                let newer = l.state.as_ref().is_none_or(|cur| {
                    cur.chain.ring_id() != disk.chain.ring_id()
                        || disk.chain.head().version() >= cur.chain.head().version()
                });
                if newer {
                    l.state = Some(disk);
                }
            }
        }
        let mut l = self.lock();
        if l.connector.is_some() || l.quitting {
            return;
        }
        let Some(s) = l.state.clone() else {
            return;
        };
        let mut client = RingClientConfig::new(s.chain.clone(), s.keys.clone());
        client.timeouts = self.cfg.timeouts;
        let mut cfg = ConnectorConfig::new(client);
        cfg.backoff_unit = self.cfg.backoff_unit;
        cfg.pending_move = s.pending_move.clone();
        cfg.move_attempts = self.cfg.move_attempts;
        match Connector::start(cfg, Arc::new(Events(self.me.clone()))) {
            Ok(c) => l.connector = Some(Arc::new(c)),
            Err(e) => {
                l.link = Some(LinkState::Stopped {
                    error: Some(format!("cannot start the relay connection: {e}")),
                })
            }
        }
    }

    /// A newer chain from the Relay: stored if it extends what is on disk.
    fn persist_newer(&self, chain: &RosterChain) {
        let _c = self.lock_commit();
        let r = self.tx(|cur, _| {
            let Some(mut s) = cur else {
                return Ok((None, None));
            };
            if s.chain.ring_id() != chain.ring_id()
                || chain.head().version() <= s.chain.head().version()
            {
                return Ok((None, None));
            }
            match s.chain.extended(chain.since(s.chain.head().version())) {
                Ok((c, _)) => {
                    s.chain = c;
                    Ok((Some(s.clone()), Some(s)))
                }
                Err(_) => Ok((None, None)),
            }
        });
        match r {
            Ok(Some(s)) => {
                self.hook();
                self.adopt(s);
            }
            Ok(None) => {}
            Err(e) => eprintln!("xshell: cannot save the Roster: {e}"),
        }
    }

    /// The old Relay acknowledged `job`: that obligation, and only that one, is cleared.
    fn clear_pending_move(&self, job: &MoveJob) {
        let _c = self.lock_commit();
        let r = self.tx(|cur, _| match cur {
            Some(mut s) if s.pending_move.as_ref() == Some(job) => {
                s.pending_move = None;
                Ok((Some(s.clone()), Some(s)))
            }
            _ => Ok((None, None)),
        });
        match r {
            Ok(Some(s)) => {
                self.lock().state = Some(s);
            }
            Ok(None) => {}
            Err(e) => eprintln!("xshell: cannot save the Ring state: {e}"),
        }
    }

    /// Enables Mobile access: creates the Ring on the default Relay, with the local Daemon
    /// in version 1 when its identity is known. A Ring that exists already (made by this or
    /// another instance) is returned as it is. After an unreadable Ring was moved aside, a new
    /// one (a new identity) is made only with `start_over`.
    pub fn enable(
        &self,
        local: Option<LocalIdentity>,
        start_over: bool,
    ) -> Result<RingView, String> {
        {
            let _c = self.lock_commit();
            let (s, created) = self.tx(|cur, loaded| {
                if let Some(s) = cur {
                    return Ok((None, (s, false)));
                }
                if loaded.moved_aside.is_some() && !start_over {
                    return Err(START_OVER_REQUIRED.to_string());
                }
                let s = self.genesis(local.as_ref())?;
                Ok((Some(s.clone()), (s, true)))
            })?;
            if created {
                self.lock().problem = None;
            }
            self.hook();
            self.adopt(s);
        }
        self.emit();
        Ok(self.view())
    }

    fn genesis(&self, local: Option<&LocalIdentity>) -> Result<RingState, String> {
        let keys = Arc::new(DeviceKeys::generate().map_err(|e| e.to_string())?);
        let t = now();
        let url = &self.cfg.default_relay_url;
        let genesis = match local {
            Some(d) => {
                let me = keys.sign_key();
                let roster = Roster {
                    v: 1,
                    ring_id: RingId::derive(&me),
                    version: 1,
                    prev: None,
                    relay_url: url.clone(),
                    signed_by: me,
                    issued_at: t,
                    members: vec![
                        Member::new(&self.cfg.name, Role::Desktop, me, keys.noise_key(), t),
                        Member::new(&d.name, Role::Daemon, d.sign_key, d.noise_key, t),
                    ],
                    extra: Default::default(),
                };
                let g = roster.sign(&*keys).map_err(|e| e.to_string())?;
                verify_genesis(&g).map_err(|e| e.to_string())?;
                g
            }
            None => SignedRoster::genesis(&*keys, keys.noise_key(), &self.cfg.name, url, t)
                .map_err(|e| e.to_string())?,
        };
        Ok(RingState {
            keys,
            chain: RosterChain::from_chain(vec![genesis]).map_err(|e| e.to_string())?,
            local_member: local.map(|d| d.sign_key),
            pending_move: None,
            host_members: BTreeMap::new(),
        })
    }

    /// Moves the Ring to `url`: a new Roster version, owed to the old Relay until it
    /// acknowledged it (see `Connector`). Refused while a previous move is still owed.
    /// Returns the new chain, for the local Daemon. The same URL is a no-op.
    pub fn set_relay_url(&self, url: &str) -> Result<RosterChain, String> {
        let url = url.trim();
        RelayUrl::parse(url).map_err(|e| e.to_string())?;
        let chain = {
            let _c = self.lock_commit();
            let (s, changed) = self.tx(|cur, _| {
                let mut s = cur.ok_or("mobile access is not enabled")?;
                if s.pending_move.is_some() {
                    return Err(MOVE_PENDING.to_string());
                }
                if s.chain.head().roster().relay_url == url {
                    return Ok((None, (s, false)));
                }
                let old = s.chain.head().version();
                let next = s
                    .chain
                    .head()
                    .next(&*s.keys, now(), |d| d.relay_url = url.to_string())
                    .map_err(|e| e.to_string())?;
                s.chain
                    .accept(std::slice::from_ref(&next))
                    .map_err(|e| e.to_string())?;
                s.pending_move = MoveJob::new(&s.chain, old);
                Ok((Some(s.clone()), (s, true)))
            })?;
            self.hook();
            self.adopt(s.clone());
            if !changed {
                return Ok(s.chain);
            }
            s.chain
        };
        self.emit();
        Ok(chain)
    }

    /// The local Daemon is in the head under `local`'s key, as a `daemon`: added in a new
    /// version, or replacing an older key of this computer that no other Host maps to.
    /// Idempotent. `None`: no Ring.
    pub fn ensure_local_daemon(
        &self,
        local: &LocalIdentity,
    ) -> Result<Option<RosterChain>, String> {
        let h = HostRef {
            id: LOCAL_HOST_ID.to_string(),
            name: local.name.clone(),
            target: String::new(),
        };
        match self.ensure_host_daemons(&[(h, local.clone())], &|_| true, None)? {
            None => Ok(None),
            Some((c, out)) => match out.first().map(|o| o.1) {
                Some(HostOutcome::OtherRole) => {
                    Err("this computer's key is in the ring with another role".into())
                }
                Some(HostOutcome::Full) => Err(format!(
                    "the ring already has the maximum of {MAX_MEMBERS} devices"
                )),
                _ => Ok(Some(c)),
            },
        }
    }

    /// Every Daemon of `batch` in the head as a `daemon` ([`plan_hosts`] has the rules), in
    /// one transaction: at most one new Roster version for the whole batch. An entry for
    /// which `valid` (called once each, inside the transaction) is false is discarded as
    /// [`HostOutcome::Stale`]. With `ring`, the batch was decided for that Ring: when the
    /// state on disk is another Ring's, nothing changes and every entry is
    /// [`HostOutcome::RingChanged`]. Idempotent. `None`: no Ring.
    pub fn ensure_host_daemons(
        &self,
        batch: &[(HostRef, LocalIdentity)],
        valid: &dyn Fn(&str) -> bool,
        ring: Option<&RingId>,
    ) -> Result<Option<(RosterChain, HostOutcomes)>, String> {
        let (chain, outcomes, changed) = {
            let _c = self.lock_commit();
            let r = self.tx(|cur, _| {
                let Some(mut s) = cur else {
                    return Ok((None, None));
                };
                if ring.is_some_and(|r| r != s.chain.ring_id()) {
                    let out = batch
                        .iter()
                        .map(|e| (e.0.id.clone(), HostOutcome::RingChanged))
                        .collect();
                    return Ok((None, Some((s, out, false))));
                }
                let mut outcomes = Vec::new();
                let mut live = Vec::new();
                for e in batch {
                    if valid(&e.0.id) {
                        live.push(e);
                    } else {
                        outcomes.push((e.0.id.clone(), HostOutcome::Stale));
                    }
                }
                let t = now();
                let p = plan_hosts(&s, &live, t);
                outcomes.extend(p.outcomes.iter().cloned());
                let mut dirty = p.after != mapping(&s);
                if p.edits_roster() {
                    let next = s
                        .chain
                        .head()
                        .next(&*s.keys, t, |d| {
                            for k in &p.remove {
                                d.remove(k);
                            }
                            for (k, name) in &p.rename {
                                if let Some(m) = d.members.iter_mut().find(|m| m.sign_key == *k) {
                                    m.name = name.clone();
                                }
                            }
                            for m in &p.add {
                                d.add(m.clone());
                            }
                        })
                        .map_err(|e| e.to_string())?;
                    s.chain
                        .accept(std::slice::from_ref(&next))
                        .map_err(|e| e.to_string())?;
                    dirty = true;
                }
                s.local_member = p.after.get(LOCAL_HOST_ID).map(|(k, _)| *k);
                s.host_members = p
                    .after
                    .iter()
                    .filter(|(id, _)| id.as_str() != LOCAL_HOST_ID)
                    .map(|(id, (k, t))| {
                        (
                            id.clone(),
                            HostMember {
                                sign_key: *k,
                                target: t.clone(),
                            },
                        )
                    })
                    .collect();
                let out = (s.clone(), outcomes, dirty);
                Ok((dirty.then_some(s), Some(out)))
            })?;
            let Some((s, outcomes, changed)) = r else {
                return Ok(None);
            };
            if changed {
                self.hook();
                self.adopt(s.clone());
            }
            (s.chain, outcomes, changed)
        };
        if changed {
            self.emit();
        }
        Ok(Some((chain, outcomes)))
    }

    /// The chain held, if a Ring exists.
    pub fn chain(&self) -> Option<RosterChain> {
        self.lock().state.as_ref().map(|s| s.chain.clone())
    }

    pub fn view(&self) -> RingView {
        let (state, connector, link, moving, problem, other) = {
            let l = self.lock();
            (
                l.state.clone(),
                l.connector.clone(),
                l.link.clone(),
                l.moving.clone(),
                l.problem.clone(),
                l.other_window && l.connector.is_none(),
            )
        };
        let (problem, problem_detail) = match problem {
            Some((p, d)) => (Some(p), Some(d)),
            None => (None, None),
        };
        let mut v = RingView {
            enabled: state.is_some(),
            ring_id: None,
            version: None,
            relay_url: None,
            hosted_relay_url: self.cfg.hosted_relay_url.clone(),
            connection: "off",
            retry_in: None,
            connection_error: None,
            limited: false,
            moving: None,
            problem,
            problem_detail,
            members: Vec::new(),
        };
        // Recovery required (or the settings refused): nothing else counts until then.
        if v.problem.is_some() {
            v.enabled = false;
            return v;
        }
        let Some(s) = state else {
            return v;
        };
        let head = s.chain.head();
        v.ring_id = Some(head.ring_id().clone());
        v.version = Some(head.version());
        v.relay_url = Some(head.roster().relay_url.clone());
        match link.unwrap_or(LinkState::Connecting { attempt: 0 }) {
            _ if other => v.connection = "other-window",
            LinkState::Connecting { .. } => v.connection = "connecting",
            LinkState::Connected { limited } => {
                v.connection = "connected";
                v.limited = limited;
            }
            LinkState::Waiting { retry_in, error } => {
                v.connection = "waiting";
                v.retry_in = Some(retry_in.as_secs_f64().ceil() as u64);
                v.connection_error = Some(error);
            }
            LinkState::Stopped { error } => {
                v.connection = "stopped";
                v.connection_error = error;
            }
        }
        v.moving = s.pending_move.as_ref().map(|job| match moving {
            Some(MoveState::Failed { job: j, error }) if &j == job => MoveView {
                state: "failed",
                error: Some(error),
            },
            Some(MoveState::Moving { job: j, error, .. }) if &j == job => MoveView {
                state: "moving",
                error,
            },
            _ => MoveView {
                state: "moving",
                error: None,
            },
        });
        let me = s.keys.sign_key();
        let live = if v.connection == "connected" {
            connector.and_then(|c| c.members())
        } else {
            None
        };
        v.members = head
            .roster()
            .members
            .iter()
            .map(|m| {
                let presence = match &live {
                    _ if m.sign_key == me && v.connection == "connected" => {
                        PresenceView::of(&MemberPresence::Online { since: None })
                    }
                    Some(list) => list
                        .iter()
                        .find(|x| x.member.sign_key == m.sign_key)
                        .map(|x| PresenceView::of(&x.presence))
                        .unwrap_or_else(PresenceView::unknown),
                    None => PresenceView::unknown(),
                };
                MemberView {
                    name: m.name.clone(),
                    role: m.role,
                    sign_key: m.sign_key,
                    this_app: m.sign_key == me,
                    this_computer: s.local_member == Some(m.sign_key),
                    // `host_members` iterates in id order: the first is the smallest.
                    host_id: s
                        .host_members
                        .iter()
                        .find(|(_, h)| h.sign_key == m.sign_key)
                        .map(|(id, _)| id.clone()),
                    presence,
                }
            })
            .collect();
        v
    }

    /// The app quits: say goodbye (`quit`), stop connecting, and hand the connection over
    /// (the owner lock is released after the goodbye).
    pub fn quit(&self) {
        let c = {
            let mut l = self.lock();
            l.quitting = true;
            l.connector.take()
        };
        if let Some(c) = c {
            c.stop(ByeReason::quit());
        }
        let owner = self.lock().owner.take();
        if let Some(f) = owner {
            let _ = f.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::time::Instant;
    use xshell_protocol::ring::relay::contract;
    use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions};

    const WAIT: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct Views {
        last: Mutex<Option<RingView>>,
        cv: Condvar,
    }

    impl RingObserver for Views {
        fn changed(&self, v: &RingView) {
            *self.last.lock().unwrap() = Some(v.clone());
            self.cv.notify_all();
        }
    }

    fn relay() -> TestRelay {
        TestRelay::start_with(TestRelayOptions {
            auth_timeout: Duration::from_millis(500),
            ..TestRelayOptions::default()
        })
    }

    fn cfg(dir: &std::path::Path, url: &str) -> DesktopRingConfig {
        let mut c = DesktopRingConfig::new(dir.join("ring"), "desk".into());
        c.default_relay_url = url.into();
        c.hosted_relay_url = url.into();
        c.backoff_unit = Duration::from_millis(10);
        c.timeouts = RingTimeouts {
            connect: Duration::from_secs(3),
            ping_interval: Duration::from_millis(300),
            dead_after: Duration::from_secs(3),
            request: Duration::from_secs(1),
            bye: Duration::from_millis(500),
        };
        c.owner_retry = Duration::from_millis(50);
        c
    }

    fn open(c: DesktopRingConfig) -> (Arc<DesktopRing>, Arc<Views>) {
        let v = Arc::new(Views::default());
        (DesktopRing::open(c, v.clone()), v)
    }

    fn wait_view(r: &DesktopRing, what: &str, pred: impl Fn(&RingView) -> bool) -> RingView {
        let deadline = Instant::now() + WAIT;
        loop {
            let v = r.view();
            if pred(&v) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {v:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn identity() -> (Arc<DeviceKeys>, LocalIdentity) {
        let k = Arc::new(DeviceKeys::generate().unwrap());
        let id = LocalIdentity {
            sign_key: k.sign_key(),
            noise_key: k.noise_key(),
            name: "host".into(),
        };
        (k, id)
    }

    fn connected(v: &RingView) -> bool {
        v.connection == "connected"
    }

    #[test]
    fn enable_defaults_to_hosted_url() {
        let c = DesktopRingConfig::new("/x".into(), "desk".into());
        assert_eq!(c.default_relay_url, super::super::HOSTED_RELAY_URL);
        assert_eq!(c.hosted_relay_url, super::super::HOSTED_RELAY_URL);
        // With no input, the Ring is created on the default Relay.
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let before = ring.view();
        assert!(!before.enabled);
        assert_eq!(before.connection, "off");
        let v = ring.enable(None, false).unwrap();
        assert!(v.enabled);
        assert_eq!(v.version, Some(1));
        assert_eq!(v.relay_url.as_deref(), Some(r.url().as_str()));
        assert_eq!(v.relay_url.as_deref(), Some(v.hosted_relay_url.as_str()));
        assert_eq!(v.members.len(), 1);
        assert!(v.members[0].this_app);
        wait_view(&ring, "connected", connected);
        assert_eq!(r.head_version(v.ring_id.as_ref().unwrap()), Some(1));
        ring.quit();
    }

    #[test]
    fn enable_with_identity_has_two_members_in_v1() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let (_, id) = identity();
        let v = ring.enable(Some(id.clone()), false).unwrap();
        assert_eq!(v.version, Some(1));
        let roles: Vec<_> = v
            .members
            .iter()
            .map(|m| (m.role, m.this_app, m.this_computer))
            .collect();
        assert_eq!(
            roles,
            [(Role::Desktop, true, false), (Role::Daemon, false, true)]
        );
        assert_eq!(v.members[1].sign_key, id.sign_key);
        assert_eq!(v.members[1].name, "host");
        ring.quit();
    }

    #[test]
    fn enable_returns_the_existing_ring_in_process_and_across_instances() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (a, _) = open(cfg(t.path(), &r.url()));
        let (b, _) = open(cfg(t.path(), &r.url()));
        let ids: Vec<_> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|i| {
                    let ring = if i % 2 == 0 { a.clone() } else { b.clone() };
                    sc.spawn(move || ring.enable(None, false).unwrap().ring_id.unwrap())
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
        let (s, _) = Store::new(t.path().join("ring")).load().unwrap();
        assert_eq!(s.unwrap().chain.ring_id(), &ids[0]);
        a.quit();
        b.quit();
    }

    #[test]
    fn set_relay_url_bumps_version() {
        let (one, two) = (relay(), relay());
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &one.url()));
        let v = ring.enable(None, false).unwrap();
        let rid = v.ring_id.unwrap();
        wait_view(&ring, "connected", connected);
        assert!(ring.set_relay_url("http://x").is_err());
        assert!(
            ring.set_relay_url("ws://example.com").is_err(),
            "plain ws only to loopback"
        );
        assert_eq!(ring.view().version, Some(1));
        // The same URL: no new version.
        assert_eq!(ring.set_relay_url(&one.url()).unwrap().head().version(), 1);
        let c = ring.set_relay_url(&two.url()).unwrap();
        assert_eq!(c.head().version(), 2);
        assert_eq!(c.head().roster().relay_url, two.url());
        // The old Relay was told before the move, and the new one has the chain.
        wait_view(&ring, "connected to the new relay", |v| {
            connected(v) && v.moving.is_none()
        });
        assert_eq!(one.head_version(&rid), Some(2));
        // "Connected" can still be the old link's state for a moment: wait for the new
        // Relay to hold the chain.
        let deadline = Instant::now() + WAIT;
        while two.head_version(&rid) != Some(2) {
            assert!(Instant::now() < deadline, "the new relay has version 2");
            std::thread::sleep(Duration::from_millis(10));
        }
        let (s, _) = Store::new(t.path().join("ring")).load().unwrap();
        assert_eq!(s.unwrap().pending_move, None);
        ring.quit();
    }

    fn online(r: &TestRelay, rid: &RingId, key: &SignKey) -> bool {
        r.presence(rid, key).is_some_and(|p| p.online)
    }

    /// The first change's publication is held up; a second change is refused meanwhile, the
    /// app stays reachable on the old relay, and moves only after the acknowledgement, which
    /// clears exactly that job.
    #[test]
    fn a_relay_change_waits_for_the_old_relay_and_blocks_another_change() {
        let (one, two, three) = (relay(), relay(), relay());
        let t = tempfile::tempdir().unwrap();
        let mut c = cfg(t.path(), &one.url());
        c.move_attempts = 2;
        let (ring, _) = open(c.clone());
        let rid = ring.enable(None, false).unwrap().ring_id.unwrap();
        let me = ring.view().members[0].sign_key;
        wait_view(&ring, "connected", connected);
        one.refuse_roster_puts(true);
        ring.set_relay_url(&two.url()).unwrap();
        let v = wait_view(&ring, "the move failed", |v| {
            v.moving.as_ref().is_some_and(|m| m.state == "failed")
        });
        assert!(v.moving.unwrap().error.is_some());
        assert_eq!(ring.set_relay_url(&three.url()).unwrap_err(), MOVE_PENDING);
        std::thread::sleep(Duration::from_millis(200));
        assert!(online(&one, &rid, &me), "reachable on the old relay");
        assert!(!online(&two, &rid, &me), "not moved before the ack");
        assert_eq!(one.head_version(&rid), Some(1));
        let store = Store::new(t.path().join("ring"));
        let job = store.load().unwrap().0.unwrap().pending_move.unwrap();
        assert_eq!(
            (job.source.as_str(), job.from, job.target),
            (one.url().as_str(), 1, 2)
        );
        // An acknowledgement of another job clears nothing.
        ring.clear_pending_move(&MoveJob {
            source: three.url(),
            ..job.clone()
        });
        assert_eq!(store.load().unwrap().0.unwrap().pending_move, Some(job));

        one.refuse_roster_puts(false);
        wait_view(&ring, "moved", |v| connected(v) && v.moving.is_none());
        assert_eq!(one.head_version(&rid), Some(2));
        let deadline = Instant::now() + WAIT;
        while !online(&two, &rid, &me) || store.load().unwrap().0.unwrap().pending_move.is_some() {
            assert!(Instant::now() < deadline, "moved and cleared");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Now a further change is allowed.
        ring.set_relay_url(&three.url()).unwrap();
        ring.quit();
    }

    /// Results are adopted in commit order: a commit held between commit and adoption keeps
    /// a later one waiting, and a stale state is refused.
    #[test]
    fn adoption_follows_commit_order() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        *ring.after_commit.lock().unwrap() = Some(Box::new(move || {
            let _ = tx.send(());
            std::thread::sleep(Duration::from_millis(200));
        }));
        let first = {
            let ring = ring.clone();
            let url = relay().url();
            std::thread::spawn(move || ring.set_relay_url(&url).map(|c| c.head().version()))
        };
        rx.recv().unwrap();
        *ring.after_commit.lock().unwrap() = None;
        let (_, id) = identity();
        let second = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                ring.ensure_local_daemon(&id)
                    .map(|c| c.unwrap().head().version())
            })
        };
        assert_eq!(first.join().unwrap().unwrap(), 2);
        assert_eq!(second.join().unwrap().unwrap(), 3);
        let disk = Store::new(t.path().join("ring")).load().unwrap().0.unwrap();
        assert_eq!(
            ring.chain().unwrap(),
            disk.chain,
            "live state is the last commit"
        );
        // A stale state is refused.
        let mut old = disk.clone();
        old.chain = RosterChain::from_chain(disk.chain.versions()[..1].to_vec()).unwrap();
        assert!(!ring.adopt(old));
        assert_eq!(ring.chain().unwrap().head().version(), 3);
        ring.quit();
    }

    #[test]
    fn a_second_instance_waits_for_the_connection_and_takes_it_over() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (a, _) = open(cfg(t.path(), &r.url()));
        let rid = a.enable(None, false).unwrap().ring_id.unwrap();
        wait_view(&a, "a connected", connected);
        let (b, _) = open(cfg(t.path(), &r.url()));
        let v = wait_view(&b, "b defers", |v| v.connection == "other-window");
        assert_eq!(v.ring_id.as_ref(), Some(&rid));
        assert!(v.members.iter().all(|m| m.presence.kind == "unknown"));
        a.quit();
        wait_view(&b, "b took over", connected);
        b.quit();
    }

    #[test]
    fn ensure_local_daemon_replaces_stale_key() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let (_, old) = identity();
        assert_eq!(ring.ensure_local_daemon(&old).unwrap(), None, "no ring yet");
        ring.enable(Some(old.clone()), false).unwrap();
        // Same key: nothing new.
        assert_eq!(
            ring.ensure_local_daemon(&old)
                .unwrap()
                .unwrap()
                .head()
                .version(),
            1
        );
        let (_, new) = identity();
        let c = ring.ensure_local_daemon(&new).unwrap().unwrap();
        assert_eq!(c.head().version(), 2);
        assert!(c.head().member(&old.sign_key).is_none());
        assert_eq!(c.head().member(&new.sign_key).unwrap().role, Role::Daemon);
        assert_eq!(
            ring.ensure_local_daemon(&new)
                .unwrap()
                .unwrap()
                .head()
                .version(),
            2
        );
        let v = ring.view();
        assert!(v
            .members
            .iter()
            .any(|m| m.this_computer && m.sign_key == new.sign_key));
        // Published to the Relay too.
        wait_view(&ring, "published", |v| {
            connected(v) && r.head_version(v.ring_id.as_ref().unwrap()) == Some(2)
        });
        ring.quit();
    }

    #[test]
    fn view_maps_presence() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let (dk, id) = identity();
        ring.enable(Some(id.clone()), false).unwrap();
        let kind = |ring: &DesktopRing| {
            ring.view()
                .members
                .iter()
                .find(|m| m.sign_key == id.sign_key)
                .map(|m| m.presence.clone())
                .unwrap()
        };
        wait_view(&ring, "never connected", |v| {
            connected(v)
                && v.members
                    .iter()
                    .any(|m| m.this_app && m.presence.kind == "online")
        });
        assert_eq!(kind(&ring).kind, "never");
        let chain = ring.chain().unwrap();
        let (c, _) = contract::connect(&r.target(), &chain, dk.clone());
        wait_view(&ring, "online", |_| kind(&ring).kind == "online");
        c.bye(ByeReason::quit()).unwrap();
        wait_view(&ring, "closed", |_| kind(&ring).kind == "closed");
        assert_eq!(kind(&ring).reason.as_deref(), Some("quit"));
        let (c, _) = contract::connect(&r.target(), &chain, dk.clone());
        wait_view(&ring, "online again", |_| kind(&ring).kind == "online");
        drop(c);
        wait_view(&ring, "unreachable", |_| kind(&ring).kind == "unreachable");
        // Without a Relay connection of its own, the Desktop does not know.
        drop(r);
        wait_view(&ring, "unknown", |v| {
            !connected(v) && v.members.iter().all(|m| m.presence.kind == "unknown")
        });
        ring.quit();
    }

    #[test]
    fn unreadable_state_is_moved_aside_and_reported() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        super::super::store::ensure_private_dir(&dir).unwrap();
        std::fs::write(dir.join("ring.json"), b"{\"v\":1,\"signSe").unwrap();
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        let v = ring.view();
        assert!(!v.enabled);
        assert_eq!(v.problem.as_deref(), Some("recovered"));
        assert!(v.problem_detail.unwrap().contains("ring.json.bad-"));
        // No new identity without an explicit start-over.
        assert_eq!(ring.enable(None, false).unwrap_err(), START_OVER_REQUIRED);
        let v = ring.enable(None, true).unwrap();
        assert!(v.enabled && v.problem.is_none());
        ring.quit();
    }

    #[test]
    fn corruption_after_open_is_reported_and_never_replaced_implicitly() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let first = ring.enable(None, false).unwrap().ring_id.unwrap();
        std::fs::write(dir.join("ring.json"), b"torn").unwrap();
        let (_, id) = identity();
        assert_eq!(ring.ensure_local_daemon(&id).unwrap(), None);
        assert_eq!(ring.view().problem.as_deref(), Some("recovered"));
        assert!(ring.set_relay_url(&relay().url()).is_err());
        assert_eq!(ring.view().problem.as_deref(), Some("recovered"));
        assert_eq!(ring.enable(None, false).unwrap_err(), START_OVER_REQUIRED);
        let v = ring.enable(None, true).unwrap();
        assert_ne!(v.ring_id.unwrap(), first, "a new Ring, made explicitly");
        assert!(v.problem.is_none());
        ring.quit();
    }

    /// A window that does not own the connection changes the Relay: the owner picks the
    /// commit up from disk and publishes the move on the old Relay.
    #[test]
    fn the_owner_publishes_a_move_committed_by_another_window() {
        let (one, two) = (relay(), relay());
        let t = tempfile::tempdir().unwrap();
        // B is open before the Ring exists.
        let (b, _) = open(cfg(t.path(), &one.url()));
        let (a, _) = open(cfg(t.path(), &one.url()));
        let rid = a.enable(None, false).unwrap().ring_id.unwrap();
        let me = a.view().members[0].sign_key;
        // Either window may win the connection (B re-reads the state and may take the lock
        // first): the owner is the one connected, the other one defers.
        let deadline = Instant::now() + WAIT;
        let (owner, other) = loop {
            let (va, vb) = (a.view(), b.view());
            if connected(&va)
                && vb.ring_id.as_ref() == Some(&rid)
                && vb.connection == "other-window"
            {
                break (&a, &b);
            }
            if connected(&vb) && va.connection == "other-window" {
                break (&b, &a);
            }
            assert!(
                Instant::now() < deadline,
                "one owns, one defers: {va:?} {vb:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        other.set_relay_url(&two.url()).unwrap();
        wait_view(owner, "the owner adopted the move", |v| {
            v.version == Some(2)
        });
        let deadline = Instant::now() + WAIT;
        while one.head_version(&rid) != Some(2) || !online(&two, &rid, &me) {
            assert!(
                Instant::now() < deadline,
                "the owner published on the old relay and moved"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let store = Store::new(t.path().join("ring"));
        while store.load().unwrap().0.unwrap().pending_move.is_some() {
            assert!(Instant::now() < deadline, "the job cleared");
            std::thread::sleep(Duration::from_millis(10));
        }
        a.quit();
        b.quit();
    }

    /// A failing mutation still reports what loading found, and the view is in the
    /// recovery-required state whatever was held in memory.
    #[test]
    fn a_failing_mutation_reports_recovery() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        std::fs::write(dir.join("ring.json"), b"torn").unwrap();
        // The first thing after the corruption is a mutation that fails.
        assert!(ring.set_relay_url(&relay().url()).is_err());
        let v = ring.view();
        assert_eq!(v.problem.as_deref(), Some("recovered"));
        assert!(!v.enabled && v.members.is_empty());
        assert_eq!(ring.enable(None, false).unwrap_err(), START_OVER_REQUIRED);
        assert!(ring.enable(None, true).unwrap().enabled);
        ring.quit();
    }

    fn href(id: &str, name: &str, target: &str) -> HostRef {
        HostRef {
            id: id.into(),
            name: name.into(),
            target: target.into(),
        }
    }

    fn quiet(t: &tempfile::TempDir) -> Arc<DesktopRing> {
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        ring.enable(None, false).unwrap();
        ring
    }

    fn all(_: &str) -> bool {
        true
    }

    fn ensure(
        ring: &DesktopRing,
        batch: &[(HostRef, LocalIdentity)],
    ) -> (RosterChain, Vec<(String, HostOutcome)>) {
        ring.ensure_host_daemons(batch, &all, None)
            .unwrap()
            .unwrap()
    }

    fn has(ring: &DesktopRing, id: &LocalIdentity) -> bool {
        ring.chain().unwrap().head().member(&id.sign_key).is_some()
    }

    fn stored(t: &tempfile::TempDir) -> RingState {
        Store::new(t.path().join("ring")).load().unwrap().0.unwrap()
    }

    #[test]
    fn ensure_host_daemons_batches_into_one_version() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (_, b) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_aaaaaaaa", "Alpha", "a|"), a.clone()),
                (href("h_bbbbbbbb", "Beta", "b|"), b.clone()),
            ],
        );
        assert_eq!(c.head().version(), 2, "one version for the batch");
        assert_eq!(
            out,
            [
                ("h_aaaaaaaa".to_string(), HostOutcome::Joined),
                ("h_bbbbbbbb".to_string(), HostOutcome::Joined)
            ]
        );
        assert_eq!(c.head().member(&a.sign_key).unwrap().name, "Alpha");
        assert_eq!(c.head().member(&b.sign_key).unwrap().role, Role::Daemon);
        let s = stored(&t);
        assert_eq!(s.host_members["h_aaaaaaaa"].sign_key, a.sign_key);
        assert_eq!(s.host_members["h_bbbbbbbb"].target, "b|");
        assert_eq!(s.local_member, None);
        ring.quit();
    }

    #[test]
    fn ensure_host_daemons_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let batch = [(href("h_aaaaaaaa", "Alpha", "a|"), a)];
        ensure(&ring, &batch);
        let sig = Store::new(t.path().join("ring")).signature();
        let (c, out) = ensure(&ring, &batch);
        assert_eq!(c.head().version(), 2);
        assert_eq!(out[0].1, HostOutcome::Joined);
        assert_eq!(
            Store::new(t.path().join("ring")).signature(),
            sig,
            "nothing written"
        );
        ring.quit();
    }

    #[test]
    fn no_ring_adds_nothing() {
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        let (_, a) = identity();
        let r = ring
            .ensure_host_daemons(&[(href("h_aaaaaaaa", "A", "a|"), a)], &all, None)
            .unwrap();
        assert!(r.is_none());
        assert!(!ring.view().enabled);
        ring.quit();
    }

    #[test]
    fn reinstalled_host_replaces_its_old_key() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, old) = identity();
        let (_, new) = identity();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), old.clone())]);
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), new.clone())]);
        assert_eq!(c.head().version(), 3);
        assert!(c.head().member(&old.sign_key).is_none(), "replaced");
        assert!(c.head().member(&new.sign_key).is_some());
        assert_eq!(c.head().roster().members.len(), 2);
        ring.quit();
    }

    /// A Host pointed at another machine keeps the old machine's member (#22 removes it).
    #[test]
    fn retargeted_host_keeps_its_old_member() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, old) = identity();
        let (_, new) = identity();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), old.clone())]);
        ensure(
            &ring,
            &[(href("h_aaaaaaaa", "A", "elsewhere|"), new.clone())],
        );
        assert!(has(&ring, &old) && has(&ring, &new));
        assert_eq!(stored(&t).host_members["h_aaaaaaaa"].sign_key, new.sign_key);
        ring.quit();
    }

    /// Two Host entries reaching one Daemon share its member, named after the entry with
    /// the smallest id; one of them reinstalling does not remove the key the other still
    /// maps to.
    #[test]
    fn two_host_entries_share_one_daemon_member() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, d) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_bbbbbbbb", "Bee", "b|"), d.clone()),
                (href("h_aaaaaaaa", "Ay", "a|"), d.clone()),
            ],
        );
        assert_eq!(c.head().version(), 2);
        assert_eq!(c.head().roster().members.len(), 2, "one member");
        assert_eq!(c.head().member(&d.sign_key).unwrap().name, "Ay");
        assert!(out.iter().all(|(_, o)| *o == HostOutcome::Joined));
        // h_bbbbbbbb now reports a new key at the same place: h_aaaaaaaa still maps to the
        // old one, so it stays.
        let (_, n) = identity();
        ensure(&ring, &[(href("h_bbbbbbbb", "Bee", "b|"), n.clone())]);
        assert!(has(&ring, &d) && has(&ring, &n));
        ring.quit();
    }

    /// The Local Host and a Remote Host entry for the same computer, in either order and in
    /// one batch or two: one member, and a reinstall seen by both replaces it.
    #[test]
    fn local_and_remote_alias_share_one_member_in_both_orders() {
        for order in 0..3 {
            let t = tempfile::tempdir().unwrap();
            let ring = quiet(&t);
            let (_, d) = identity();
            let local = (href(LOCAL_HOST_ID, "host", ""), d.clone());
            let remote = (href("h_aaaaaaaa", "Me again", "me|"), d.clone());
            match order {
                0 => drop(ensure(&ring, &[local.clone(), remote.clone()])),
                1 => drop(ensure(&ring, &[remote.clone(), local.clone()])),
                _ => {
                    ring.ensure_local_daemon(&d).unwrap();
                    ensure(&ring, std::slice::from_ref(&remote));
                }
            }
            let c = ring.chain().unwrap();
            assert_eq!(c.head().roster().members.len(), 2, "order {order}");
            let v = ring.view();
            let m = v.members.iter().find(|m| m.sign_key == d.sign_key).unwrap();
            assert!(m.this_computer);
            assert_eq!(m.host_id.as_deref(), Some("h_aaaaaaaa"));
            // The smallest id names it: "h_…" sorts before "local".
            assert_eq!(m.name, "Me again", "order {order}");
            // Only the Local Host reports the new key: the Remote entry still maps to the
            // old one, which stays.
            let (_, n) = identity();
            ring.ensure_local_daemon(&n).unwrap();
            assert!(has(&ring, &d) && has(&ring, &n), "order {order}");
            // Now the Remote entry sees it too: the old key is nobody's and goes.
            let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "Me again", "me|"), n.clone())]);
            assert!(c.head().member(&d.sign_key).is_none(), "order {order}");
            assert_eq!(c.head().roster().members.len(), 2);
            ring.quit();
        }
    }

    #[test]
    fn rename_updates_member_name_keeping_added_at() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "Old", "a|"), a.clone())]);
        let before = c.head().member(&a.sign_key).unwrap().clone();
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "New", "a|"), a.clone())]);
        assert_eq!(c.head().version(), 3, "a name-only version");
        let after = c.head().member(&a.sign_key).unwrap();
        assert_eq!(after.name, "New");
        assert_eq!(after.added_at, before.added_at);
        assert_eq!(after.noise_key, before.noise_key);
        ring.quit();
    }

    fn fill(ring: &DesktopRing, n: usize) {
        let batch: Vec<_> = (0..n)
            .map(|i| (href(&format!("h_{i:08}"), "x", "x|"), identity().1))
            .collect();
        ensure(ring, &batch);
    }

    #[test]
    fn full_roster_marks_host_full() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        // The Desktop plus 62: room for one more.
        fill(&ring, MAX_MEMBERS - 2);
        let (_, a) = identity();
        let (_, b) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_zzzzzzza", "A", "a|"), a.clone()),
                (href("h_zzzzzzzb", "B", "b|"), b.clone()),
            ],
        );
        assert_eq!(c.head().roster().members.len(), MAX_MEMBERS);
        assert_eq!(
            out,
            [
                ("h_zzzzzzza".to_string(), HostOutcome::Joined),
                ("h_zzzzzzzb".to_string(), HostOutcome::Full)
            ]
        );
        assert!(has(&ring, &a) && !has(&ring, &b));
        assert!(!stored(&t).host_members.contains_key("h_zzzzzzzb"));
        ring.quit();
    }

    #[test]
    fn replacement_at_capacity_works() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        fill(&ring, MAX_MEMBERS - 2);
        let (_, old) = identity();
        ensure(&ring, &[(href("h_zzzzzzza", "A", "a|"), old.clone())]);
        assert_eq!(
            ring.chain().unwrap().head().roster().members.len(),
            MAX_MEMBERS
        );
        let (_, new) = identity();
        let (c, out) = ensure(&ring, &[(href("h_zzzzzzza", "A", "a|"), new.clone())]);
        assert_eq!(out[0].1, HostOutcome::Joined);
        assert_eq!(c.head().roster().members.len(), MAX_MEMBERS);
        assert!(has(&ring, &new) && !has(&ring, &old));
        ring.quit();
    }

    /// At capacity, a new Host and a reinstalled one in one batch, in either order: the
    /// reinstall replaces its key, the addition is refused.
    #[test]
    fn replacement_kept_before_an_addition_at_capacity() {
        for flip in [false, true] {
            let t = tempfile::tempdir().unwrap();
            let ring = quiet(&t);
            fill(&ring, MAX_MEMBERS - 2);
            let (_, old) = identity();
            ensure(&ring, &[(href("h_zzzzzzzb", "B", "b|"), old.clone())]);
            assert_eq!(
                ring.chain().unwrap().head().roster().members.len(),
                MAX_MEMBERS
            );
            let (_, a) = identity();
            let (_, new) = identity();
            let mut batch = vec![
                (href("h_zzzzzzza", "A", "a|"), a.clone()),
                (href("h_zzzzzzzb", "B", "b|"), new.clone()),
            ];
            if flip {
                batch.reverse();
            }
            let (c, mut out) = ensure(&ring, &batch);
            out.sort();
            assert_eq!(
                out,
                [
                    ("h_zzzzzzza".to_string(), HostOutcome::Full),
                    ("h_zzzzzzzb".to_string(), HostOutcome::Joined)
                ],
                "flip {flip}"
            );
            assert_eq!(c.head().roster().members.len(), MAX_MEMBERS);
            assert!(has(&ring, &new) && !has(&ring, &old) && !has(&ring, &a));
            ring.quit();
        }
    }

    #[test]
    fn a_batch_for_another_ring_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let other = RingId::derive(&identity().1.sign_key);
        let (_, a) = identity();
        let sig = Store::new(t.path().join("ring")).signature();
        let (c, out) = ring
            .ensure_host_daemons(
                &[(href("h_aaaaaaaa", "A", "a|"), a.clone())],
                &all,
                Some(&other),
            )
            .unwrap()
            .unwrap();
        assert_eq!(out, [("h_aaaaaaaa".to_string(), HostOutcome::RingChanged)]);
        assert_eq!(c.head().version(), 1);
        assert!(stored(&t).host_members.is_empty());
        assert_eq!(Store::new(t.path().join("ring")).signature(), sig);
        ring.quit();
    }

    #[test]
    fn other_role_refused_for_that_host_only() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        // This Desktop's own key, reported by a Host: refused; the other Host is added.
        let me = ring.view().members[0].sign_key;
        let mut mine = identity().1;
        mine.sign_key = me;
        let (_, b) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_aaaaaaaa", "A", "a|"), mine),
                (href("h_bbbbbbbb", "B", "b|"), b.clone()),
            ],
        );
        assert_eq!(
            out,
            [
                ("h_aaaaaaaa".to_string(), HostOutcome::OtherRole),
                ("h_bbbbbbbb".to_string(), HostOutcome::Joined)
            ]
        );
        assert_eq!(c.head().member(&me).unwrap().role, Role::Desktop);
        assert!(has(&ring, &b));
        assert!(!stored(&t).host_members.contains_key("h_aaaaaaaa"));
        ring.quit();
    }

    #[test]
    fn stale_identities_are_discarded_in_the_transaction() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (_, b) = identity();
        let (c, out) = ring
            .ensure_host_daemons(
                &[
                    (href("h_aaaaaaaa", "A", "a|"), a.clone()),
                    (href("h_bbbbbbbb", "B", "b|"), b.clone()),
                ],
                &|id| id != "h_aaaaaaaa",
                None,
            )
            .unwrap()
            .unwrap();
        assert!(out.contains(&("h_aaaaaaaa".to_string(), HostOutcome::Stale)));
        assert!(c.head().member(&a.sign_key).is_none());
        assert!(c.head().member(&b.sign_key).is_some());
        ring.quit();
    }

    #[test]
    fn member_view_carries_host_id() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (_, l) = identity();
        ring.ensure_local_daemon(&l).unwrap();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), a.clone())]);
        let v = ring.view();
        let of = |k: &SignKey| v.members.iter().find(|m| m.sign_key == *k).unwrap();
        assert_eq!(of(&a.sign_key).host_id.as_deref(), Some("h_aaaaaaaa"));
        assert_eq!(of(&l.sign_key).host_id, None);
        assert!(of(&l.sign_key).this_computer);
        let json = serde_json::to_value(of(&a.sign_key)).unwrap();
        assert_eq!(json["hostId"], "h_aaaaaaaa");
        ring.quit();
    }

    /// A Host added while a Relay move is still owed reaches the new Relay too.
    #[test]
    fn a_host_added_during_a_pending_move_reaches_the_new_relay() {
        let (one, two) = (relay(), relay());
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &one.url()));
        let rid = ring.enable(None, false).unwrap().ring_id.unwrap();
        wait_view(&ring, "connected", connected);
        one.refuse_roster_puts(true);
        ring.set_relay_url(&two.url()).unwrap();
        let (_, a) = identity();
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), a)]);
        assert_eq!(c.head().version(), 3);
        assert!(ring.view().moving.is_some());
        one.refuse_roster_puts(false);
        let deadline = Instant::now() + WAIT;
        while two.head_version(&rid) != Some(3) {
            assert!(Instant::now() < deadline, "v3 on the new relay");
            std::thread::sleep(Duration::from_millis(10));
        }
        ring.quit();
    }

    #[cfg(unix)]
    #[test]
    fn refused_settings_are_reported_as_unreadable() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        ring.enable(None, false).unwrap();
        let real = t.path().join("elsewhere.json");
        std::fs::rename(dir.join("ring.json"), &real).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("ring.json")).unwrap();
        assert!(ring.set_relay_url("wss://other.example").is_err());
        let v = ring.view();
        assert_eq!(v.problem.as_deref(), Some("unreadable"));
        assert!(!v.enabled);
        ring.quit();
    }
}

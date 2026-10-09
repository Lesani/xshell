//! [`DesktopRing`]: this Desktop as the Ring's creator and signer. It creates the Ring,
//! signs every new Roster version (a new Relay URL, the local Daemon added or replaced),
//! keeps a Relay connection through the [`Connector`], and builds the [`RingView`] that
//! Settings → Mobile shows.
//!
//! Every mutation is a [`Store::transact`] transaction: serialized in this process and
//! across app instances, applied to the state as on disk now, written atomically. The
//! Connector's callbacks persist newer chains through the same transactions and never hold a
//! lock of their own while they do.

use super::store::{RingState, Store};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use xshell_protocol::ring::relay::{
    ByeReason, Connector, ConnectorConfig, ConnectorEvents, LinkState, MemberPresence, MoveJob,
    MoveState, RingClientConfig, RingTimeouts,
};
use xshell_protocol::ring::url::RelayUrl;
use xshell_protocol::ring::{
    verify_genesis, DeviceKeys, Member, NoiseKey, RingId, Role, Roster, RosterChain, SignKey,
    SignedRoster,
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
                    && cur.local_member == s.local_member;
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
    /// version, or replacing an older key of this computer. Idempotent. `None`: no Ring.
    pub fn ensure_local_daemon(
        &self,
        local: &LocalIdentity,
    ) -> Result<Option<RosterChain>, String> {
        let (chain, changed) = {
            let _c = self.lock_commit();
            let r = self.tx(|cur, _| {
                let Some(mut s) = cur else {
                    return Ok((None, None));
                };
                let head = s.chain.head();
                match head.member(&local.sign_key) {
                    Some(m) if m.role == Role::Daemon => {
                        if s.local_member == Some(local.sign_key) {
                            return Ok((None, Some((s, false))));
                        }
                        s.local_member = Some(local.sign_key);
                        return Ok((Some(s.clone()), Some((s, false))));
                    }
                    Some(_) => {
                        return Err("this computer's key is in the ring with another role".into())
                    }
                    None => {}
                }
                let stale = s.local_member.filter(|k| head.member(k).is_some());
                let next = head
                    .next(&*s.keys, now(), |d| {
                        if let Some(k) = stale {
                            d.remove(&k);
                        }
                        d.add(Member::new(
                            &local.name,
                            Role::Daemon,
                            local.sign_key,
                            local.noise_key,
                            now(),
                        ));
                    })
                    .map_err(|e| e.to_string())?;
                s.chain
                    .accept(std::slice::from_ref(&next))
                    .map_err(|e| e.to_string())?;
                s.local_member = Some(local.sign_key);
                Ok((Some(s.clone()), Some((s, true))))
            })?;
            let Some((s, changed)) = r else {
                return Ok(None);
            };
            self.hook();
            self.adopt(s.clone());
            (s.chain, changed)
        };
        if changed {
            self.emit();
        }
        Ok(Some(chain))
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
        assert_eq!(two.head_version(&rid), Some(2));
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
        wait_view(&a, "a connected", connected);
        wait_view(&b, "b sees the Ring and defers", |v| {
            v.ring_id.as_ref() == Some(&rid) && v.connection == "other-window"
        });
        b.set_relay_url(&two.url()).unwrap();
        wait_view(&a, "a adopted the move", |v| v.version == Some(2));
        let deadline = Instant::now() + WAIT;
        while one.head_version(&rid) != Some(2) || !online(&two, &rid, &me) {
            assert!(
                Instant::now() < deadline,
                "a published on the old relay and moved"
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

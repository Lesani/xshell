//! This Host as a Ring member (`daemon` role): its device keys, the Roster chain it trusts,
//! and the Relay connection ([`Connector`]) that reports it online and carries the Noise
//! sessions Desktops and Mobiles open to it ([`super::relay_conn`]).
//!
//! State lives in [`Paths::ring_dir`](crate::paths::Paths::ring_dir), 0700:
//! - `keys.json` `{"v":1,"signSeed":b64u,"noiseSeed":b64u}`, created on first use (the first
//!   `ring.identity`, or `xshelld pair`); the private keys never leave this Host;
//! - `roster.json` `{"v":1,"rosters":["xro1…",…]}`, the whole verified chain from version 1;
//! - `ring.lock`, held (flock) for every change to the other two, by `serve` and by
//!   `xshelld pair` alike, so neither replaces the other's keys or rolls its chain back.
//!
//! The files are 0600, written through an exclusive temp file and a rename. An existing file
//! that is a symlink or another user's is refused; one with a broader mode is tightened. On
//! Windows the same holds with owners and a protected user-only DACL in place of modes,
//! and reparse points refused (`xshell_core::private_fs`).

#[cfg(unix)]
use crate::paths::ensure_private_dir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use xshell_protocol::msg::{JoinExpect, MEMBERSHIP_CHANGED};
use xshell_protocol::ring::relay::sessions::Sessions;
use xshell_protocol::ring::relay::wire::{ErrorCode, MemberPresence};
use xshell_protocol::ring::relay::{
    ByeReason, Connector, ConnectorConfig, ConnectorEvents, LinkState, RingClientConfig,
    RingTimeouts,
};
use xshell_protocol::ring::{
    member_name, DeviceKeys, Role, RosterChain, RosterError, SecretSeed, SignKey,
};

/// What a Host is called when its hostname is unusable.
const FALLBACK_NAME: &str = "this computer";

/// The private keys as stored. The seeds decode straight into wiping storage, so every
/// path, a failed parse included, wipes them.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeysFile {
    v: u32,
    sign_seed: SecretSeed,
    noise_seed: SecretSeed,
}

#[derive(Serialize, Deserialize)]
struct RosterFile {
    v: u32,
    rosters: Vec<String>,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Opens `path` for reading if it is a regular file of ours (not a symlink); tightens a mode
/// broader than 0600. `None`: it does not exist.
#[cfg(windows)]
pub(crate) fn read_private(path: &Path) -> io::Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
    let Some(f) = xshell_core::private_fs::open_read(path)? else {
        return Ok(None);
    };
    let len = f.metadata()?.len() as usize;
    let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(len + 1));
    (&f).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

#[cfg(unix)]
pub(crate) fn read_private(path: &Path) -> io::Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
    let f = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(io::Error::other(format!(
                "{} is a symlink; refusing it",
                path.display()
            )))
        }
        Err(e) => return Err(e),
    };
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {}, not by us",
                path.display(),
                meta.uid()
            ),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    // Sized up front, so no reallocation leaves a copy of a secret behind.
    let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(meta.len() as usize + 1));
    (&f).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

/// Writes `bytes` to `path` atomically: an exclusive 0600 temp file with a unique name (never
/// following a symlink), synced, then renamed over `path`.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.as_os_str().to_owned();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    name.push(format!(
        ".{}.{}.{nanos}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    let tmp = PathBuf::from(name);
    let r = (|| {
        #[cfg(windows)]
        let mut f = xshell_core::private_fs::create_new(&tmp)?;
        #[cfg(unix)]
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// This Host's name for the Roster: its hostname, cleaned up.
#[cfg(windows)]
pub(crate) fn host_name() -> String {
    let raw = std::env::var("COMPUTERNAME").unwrap_or_default();
    member_name(&raw, FALLBACK_NAME)
}

#[cfg(unix)]
pub(crate) fn host_name() -> String {
    let mut buf = [0u8; 256];
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let raw = if r == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    member_name(&raw, FALLBACK_NAME)
}

/// The files in the ring directory.
pub(crate) struct Store {
    pub dir: PathBuf,
}

/// What [`Store::save_chain`] settled on.
#[derive(Debug)]
pub(crate) struct Saved {
    /// The chain to trust from now on.
    pub trusted: RosterChain,
    /// The chain forked the stored one: refused, and `trusted` is the stored chain.
    pub conflict: Option<String>,
    /// `trusted` could not be written (it is in force all the same).
    pub write_error: Option<String>,
}

impl Saved {
    fn new(trusted: RosterChain, write_error: Option<String>) -> Saved {
        Saved {
            trusted,
            conflict: None,
            write_error,
        }
    }
}

/// `ring.lock`, held while it lives.
pub(crate) struct StoreLock(#[allow(dead_code)] fs::File);

impl Store {
    #[cfg_attr(windows, allow(dead_code))] // `xshelld pair` is Unix only
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    /// Takes `ring.lock`, waiting for another process (or thread) that holds it. Never
    /// nested: every caller takes it once around one read-modify-write.
    pub fn lock(&self) -> Result<StoreLock, String> {
        self.ensure_dir().map_err(|e| e.to_string())?;
        let path = self.dir.join("ring.lock");
        #[cfg(windows)]
        let f = xshell_core::private_fs::open_or_create(&path);
        #[cfg(unix)]
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path);
        let f = f.map_err(|e| format!("cannot open the ring lock: {e}"))?;
        f.lock()
            .map_err(|e| format!("cannot take the ring lock: {e}"))?;
        Ok(StoreLock(f))
    }

    /// The stored keys, or new ones, under the lock: a key file another process wrote
    /// meanwhile is read, never replaced.
    pub fn load_or_create_keys(&self) -> Result<DeviceKeys, String> {
        if let Some(k) = self.load_keys()? {
            return Ok(k);
        }
        let _l = self.lock()?;
        match self.load_keys()? {
            Some(k) => Ok(k),
            None => self.create_keys(),
        }
    }

    fn keys_path(&self) -> PathBuf {
        self.dir.join("keys.json")
    }

    fn roster_path(&self) -> PathBuf {
        self.dir.join("roster.json")
    }

    fn ensure_dir(&self) -> io::Result<()> {
        // Windows: created owned by this user with a user-only DACL, or made so (see
        // `xshell_core::private_fs`).
        #[cfg(windows)]
        {
            xshell_core::private_fs::ensure_dir(&self.dir)
        }
        #[cfg(unix)]
        ensure_private_dir(&self.dir)
    }

    /// The stored keys; `None` when there are none yet. An unreadable file is an error, never
    /// replaced: the keys are this Host's identity.
    pub fn load_keys(&self) -> Result<Option<DeviceKeys>, String> {
        if !self.dir.exists() {
            return Ok(None);
        }
        self.ensure_dir().map_err(|e| e.to_string())?;
        let p = self.keys_path();
        let Some(bytes) = read_private(&p).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let bad = || {
            format!(
                "{} is unreadable; move it away to make new keys",
                p.display()
            )
        };
        let f: KeysFile = serde_json::from_slice(&bytes).map_err(|_| bad())?;
        if f.v != 1 {
            return Err(bad());
        }
        Ok(Some(DeviceKeys::from_seeds(
            &f.sign_seed.0,
            &f.noise_seed.0,
        )))
    }

    /// Only under the lock, after `load_keys` found none.
    fn create_keys(&self) -> Result<DeviceKeys, String> {
        self.ensure_dir().map_err(|e| e.to_string())?;
        if self.keys_path().exists() || fs::symlink_metadata(self.keys_path()).is_ok() {
            return Err(format!(
                "{} appeared while creating keys; refusing to replace it",
                self.keys_path().display()
            ));
        }
        let k = DeviceKeys::generate().map_err(|e| e.to_string())?;
        let (s, n) = k.seeds();
        let f = KeysFile {
            v: 1,
            sign_seed: SecretSeed::new(&s),
            noise_seed: SecretSeed::new(&n),
        };
        // Serialized into a buffer sized up front and wiped on drop.
        let mut json = zeroize::Zeroizing::new(Vec::with_capacity(512));
        serde_json::to_writer(&mut *json, &f).map_err(|e| e.to_string())?;
        write_private(&self.keys_path(), &json).map_err(|e| e.to_string())?;
        Ok(k)
    }

    /// The stored chain as it is: `Ok(None)` when there is none, `Err` when it is
    /// unreadable or does not verify.
    pub fn read_chain(&self) -> Result<Option<RosterChain>, String> {
        let p = self.roster_path();
        let Some(bytes) = read_private(&p).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        serde_json::from_slice::<RosterFile>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|f| {
                if f.v != 1 {
                    return Err("unknown format".to_string());
                }
                RosterChain::from_tokens(&f.rosters).map_err(|e| e.to_string())
            })
            .map(Some)
    }

    /// The stored chain. An unreadable or unverifiable file is moved aside (under the lock)
    /// and logged: this Host is then in no Ring until a Desktop sends `ring.join` again.
    fn load_chain(&self) -> Option<RosterChain> {
        let p = self.roster_path();
        match read_private(&p) {
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(e) => {
                crate::log!("ERROR", "cannot read {}: {e}", p.display());
                return None;
            }
        }
        let _l = match self.lock() {
            Ok(l) => l,
            Err(e) => {
                crate::log!("ERROR", "ring: {e}");
                return None;
            }
        };
        match self.read_chain() {
            Ok(c) => c,
            Err(e) => {
                let aside = p.with_extension(format!("json.bad-{}", super::registry::now_ms()));
                let _ = fs::rename(&p, &aside);
                crate::log!(
                    "ERROR",
                    "{} is invalid ({e}); moved to {}",
                    p.display(),
                    aside.display()
                );
                None
            }
        }
    }

    /// Settles `chain` against the store under the lock, and saves the result. For the same
    /// Ring the stored chain must accept `chain` by the chain rules
    /// ([`RosterChain::extended`]): versions it holds are skipped, so a newer stored head
    /// (another process saved it meanwhile) wins. A fork of the stored chain is a trust
    /// conflict: nothing is written and the stored chain stays the trusted one. Another Ring
    /// (an explicit replacement), or no readable file: `chain` is the one. A write that fails
    /// is reported apart: the settled chain is trusted all the same.
    pub fn save_chain(&self, chain: &RosterChain) -> Saved {
        let _l = match self.lock() {
            Ok(l) => l,
            Err(e) => return Saved::new(chain.clone(), Some(e)),
        };
        let next = match self.read_chain() {
            Ok(Some(disk)) if disk.ring_id() == chain.ring_id() => {
                match disk.extended(chain.versions()) {
                    Ok((merged, _)) if merged == disk => return Saved::new(disk, None),
                    Ok((merged, _)) => merged,
                    Err(e) => {
                        return Saved {
                            trusted: disk,
                            conflict: Some(format!(
                                "the roster conflicts with the one stored here ({})",
                                e.as_code()
                            )),
                            write_error: None,
                        }
                    }
                }
            }
            _ => chain.clone(),
        };
        let err = self.write_chain(&next).err();
        Saved::new(next, err)
    }

    /// Writes `chain` as it is; only with the lock held.
    pub fn write_chain(&self, chain: &RosterChain) -> Result<(), String> {
        self.ensure_dir().map_err(|e| e.to_string())?;
        let f = RosterFile {
            v: 1,
            rosters: chain
                .versions()
                .iter()
                .map(|r| r.token().to_string())
                .collect(),
        };
        let json = serde_json::to_vec(&f).map_err(|e| e.to_string())?;
        write_private(&self.roster_path(), &json).map_err(|e| e.to_string())
    }
}

struct Joined {
    connector: Arc<Connector>,
    /// The Noise sessions over this Connector.
    sessions: Sessions,
}

/// The chain the store holds, shared with the Connector's callbacks.
struct Persist {
    store: Store,
    chain: Mutex<Option<RosterChain>>,
}

impl Persist {
    /// A newer chain from the Relay: settled against the store first, then trusted in
    /// memory, handed to the Connector when the store had more (or held a conflicting
    /// fork, which is refused), and swept, all under the trusted chain's lock, which local
    /// joins hold for the same sequence. A chain not newer than the trusted one (a callback
    /// that lost the race to a local join) changes nothing.
    fn keep_newer(&self, chain: &RosterChain, connector: Option<&Connector>, sessions: &Sessions) {
        let mut cur = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cur.as_ref() {
            if c.ring_id() != chain.ring_id() || c.head().version() >= chain.head().version() {
                return;
            }
        }
        let saved = self.store.save_chain(chain);
        if let Some(e) = &saved.conflict {
            crate::log!("ERROR", "ring: refused a roster from the relay: {e}");
        }
        if let Some(e) = &saved.write_error {
            crate::log!("ERROR", "cannot save the Roster: {e}");
        }
        let trusted = saved.trusted;
        if &trusted != chain {
            if let Some(c) = connector {
                c.set_chain(trusted.clone(), None);
            }
        }
        sessions.sweep(trusted.head());
        *cur = Some(trusted);
    }
}

struct Events {
    persist: Arc<Persist>,
    sessions: Sessions,
    /// The Connector these events are of (set once it exists).
    connector: Arc<std::sync::OnceLock<std::sync::Weak<Connector>>>,
}

impl ConnectorEvents for Events {
    fn state(&self, s: &LinkState) {
        self.sessions.state(s);
        match s {
            LinkState::Connected { limited } => {
                crate::log!("INFO", "ring: connected to the relay (limited: {limited})")
            }
            LinkState::Waiting { retry_in, error } => crate::log!(
                "INFO",
                "ring: relay connection failed ({error}); retrying in {retry_in:?}"
            ),
            LinkState::Stopped { error: Some(e) } => {
                crate::log!("WARN", "ring: no longer connecting: {e}")
            }
            LinkState::Connecting { .. } | LinkState::Stopped { error: None } => {}
        }
    }

    fn roster(&self, chain: &RosterChain) {
        let c = self.connector.get().and_then(std::sync::Weak::upgrade);
        self.persist.keep_newer(chain, c.as_deref(), &self.sessions);
    }

    fn presence(&self, key: SignKey, p: &MemberPresence) {
        self.sessions.presence(key, p);
    }

    fn envelope(&self, from: SignKey, payload: Vec<u8>) {
        self.sessions.envelope(from, &payload);
    }

    fn error(&self, code: &ErrorCode, to: Option<SignKey>) {
        self.sessions.error(code, to);
    }
}

/// The Connector's lifecycle, under one lock: whether the Daemon is exiting, the installed
/// Connector, and those retiring. A Connector is installed only with this lock held and
/// `stopped` false, and `stop` drains under it, so none is installed after the drain.
#[derive(Default)]
struct Life {
    stopped: bool,
    joined: Option<Joined>,
    /// Connectors of a Ring this Host left for another, still saying goodbye.
    retiring: Vec<JoinHandle<()>>,
}

/// This Host's Ring membership.
pub(crate) struct Ring {
    persist: Arc<Persist>,
    keys: Mutex<Option<Arc<DeviceKeys>>>,
    life: Mutex<Life>,
    /// Builds each Connector's sessions and hands accepted ones to the Daemon.
    hub: super::relay_conn::Hub,
    name: String,
    backoff_unit: Duration,
    timeouts: RingTimeouts,
    /// Test hook: runs in `join` between the `stopped` check and the install.
    #[cfg(test)]
    install_hook: Mutex<Option<Box<dyn Fn() + Send>>>,
}

fn link_json(s: &LinkState) -> (&'static str, Option<String>) {
    match s {
        LinkState::Connecting { .. } => ("connecting", None),
        LinkState::Connected { .. } => ("connected", None),
        LinkState::Waiting { error, .. } => ("waiting", Some(error.clone())),
        LinkState::Stopped { error } => ("stopped", error.clone()),
    }
}

impl Ring {
    pub fn new(
        dir: PathBuf,
        backoff_unit: Duration,
        timeouts: RingTimeouts,
        write_stall: Duration,
    ) -> Ring {
        Ring {
            persist: Arc::new(Persist {
                store: Store { dir },
                chain: Mutex::new(None),
            }),
            keys: Mutex::new(None),
            life: Mutex::new(Life::default()),
            hub: super::relay_conn::Hub::new(write_stall),
            name: host_name(),
            backoff_unit,
            timeouts,
            #[cfg(test)]
            install_hook: Mutex::new(None),
        }
    }

    pub fn hub(&self) -> &super::relay_conn::Hub {
        &self.hub
    }

    /// At start: rejoin the stored Ring, if this Host is a member of it.
    pub fn resume(&self) {
        let keys = match self.persist.store.load_keys() {
            Ok(Some(k)) => Arc::new(k),
            Ok(None) => return,
            Err(e) => {
                crate::log!("ERROR", "ring: {e}");
                return;
            }
        };
        *self.keys.lock().unwrap_or_else(|e| e.into_inner()) = Some(keys.clone());
        let Some(chain) = self.persist.store.load_chain() else {
            return;
        };
        *self.persist.chain.lock().unwrap_or_else(|e| e.into_inner()) = Some(chain.clone());
        if !is_daemon_member(&chain, &keys) {
            crate::log!("INFO", "ring: this Host is not in the stored Roster");
            return;
        }
        let mut life = self.lock_life();
        if let Err(e) = self.install(&mut life, chain, keys) {
            crate::log!("ERROR", "ring: cannot start the relay connection: {e}");
        }
    }

    fn keys(&self, create: bool) -> Result<Option<Arc<DeviceKeys>>, String> {
        let mut g = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let k = if create {
                Some(self.persist.store.load_or_create_keys()?)
            } else {
                self.persist.store.load_keys()?
            };
            *g = k.map(Arc::new);
        }
        Ok(g.clone())
    }

    fn lock_life(&self) -> std::sync::MutexGuard<'_, Life> {
        self.life.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Installs a Connector for `chain`, retiring the one installed (it says goodbye on a
    /// thread of its own, joined by `stop`). Only with the lifecycle lock held.
    fn install(
        &self,
        life: &mut Life,
        chain: RosterChain,
        keys: Arc<DeviceKeys>,
    ) -> Result<(), String> {
        if life.stopped {
            return Err("xshelld is exiting".into());
        }
        if let Some(old) = life.joined.take() {
            old.sessions.stop();
            match std::thread::Builder::new()
                .name("ring-leave".into())
                .spawn({
                    let c = old.connector.clone();
                    move || c.stop(ByeReason::quit())
                }) {
                Ok(h) => life.retiring.push(h),
                // No thread: retire it here (bounded by the connect timeout).
                Err(_) => old.connector.stop(ByeReason::quit()),
            }
        }
        let sessions = self.hub.sessions(keys.clone());
        let mut client = RingClientConfig::new(chain, keys);
        client.timeouts = self.timeouts;
        let mut cfg = ConnectorConfig::new(client);
        cfg.backoff_unit = self.backoff_unit;
        let slot: Arc<std::sync::OnceLock<std::sync::Weak<Connector>>> = Arc::default();
        let c = Connector::start(
            cfg,
            Arc::new(Events {
                persist: self.persist.clone(),
                sessions: sessions.clone(),
                connector: slot.clone(),
            }),
        )
        .map_err(|e| e.to_string())?;
        let connector = Arc::new(c);
        let _ = slot.set(Arc::downgrade(&connector));
        sessions.attach(&connector);
        life.joined = Some(Joined {
            connector,
            sessions,
        });
        Ok(())
    }

    /// `ring.identity`: the public keys (made on first use), the name, and the membership.
    pub fn identity(&self) -> Result<Value, String> {
        let keys = self.keys(true)?.ok_or("no keys")?;
        let chain = self
            .persist
            .chain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let state = self
            .lock_life()
            .joined
            .as_ref()
            .map(|j| j.connector.state());
        // The membership a conditional join compares against (see `membership`).
        let ring = match (chain, state) {
            (Some(c), Some(s)) => {
                let (state, error) = link_json(&s);
                let mut r = json!({
                    "ringId": c.ring_id(),
                    "version": c.head().version(),
                    "relayUrl": c.head().roster().relay_url,
                    "state": state,
                });
                if let Some(e) = error {
                    r["error"] = json!(e);
                }
                r
            }
            _ => Value::Null,
        };
        Ok(json!({
            "signKey": keys.sign_key(),
            "noiseKey": keys.noise_key(),
            "name": self.name,
            "ring": ring,
        }))
    }

    /// `ring.join`: trust `tokens` (a whole chain from version 1) and connect. With
    /// `expect`, only while this Host's membership (as `ring.identity` reports it) is still
    /// the expected one, checked under the same locks as the commit.
    pub fn join(&self, tokens: &[String], expect: Option<&JoinExpect>) -> Result<Value, String> {
        let refused = |e: RosterError| format!("roster refused: {}", e.as_code());
        let chain = RosterChain::from_tokens(tokens).map_err(refused)?;
        let keys = self.keys(true)?.ok_or("no keys")?;
        if !is_daemon_member(&chain, &keys) {
            return Err("not a member of this Roster".into());
        }
        // The lifecycle lock for the whole commit and install (never taken by the Connector's
        // callbacks), then the persisted chain's: the Connector's own saves wait.
        let mut life = self.lock_life();
        if life.stopped {
            return Err("xshelld is exiting".into());
        }
        #[cfg(test)]
        if let Some(h) = self.install_hook.lock().unwrap().as_ref() {
            h();
        }
        let mut cur = self.persist.chain.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = expect {
            if !expected(x, membership(cur.as_ref(), &life)) {
                return Err(MEMBERSHIP_CHANGED.into());
            }
        }
        let same_ring = cur.as_ref().is_some_and(|c| c.ring_id() == chain.ring_id());
        let next = match cur.as_ref() {
            Some(c) if same_ring => {
                let (merged, a) = c.extended(chain.versions()).map_err(refused)?;
                if a.added == 0 && chain.head().version() < c.head().version() {
                    return Err(refused(RosterError::Stale));
                }
                merged
            }
            _ => chain,
        };
        // Settled against the store first (under ring.lock): a newer stored head wins, and a
        // fork of the stored chain is refused, leaving the established trust in force.
        let changed = cur.as_ref() != Some(&next);
        let saved = if changed {
            self.persist.store.save_chain(&next)
        } else {
            Saved::new(next, None)
        };
        let trusted = saved.trusted;
        // Then in force at once, whether or not the write succeeded: in memory, in the
        // Connector (whose head new handshakes are checked against), and in the sweep.
        let same_ring = cur
            .as_ref()
            .is_some_and(|c| c.ring_id() == trusted.ring_id());
        let moved = cur.as_ref() != Some(&trusted);
        *cur = Some(trusted.clone());
        match life.joined.as_ref() {
            Some(j) if same_ring => {
                if moved {
                    j.connector.set_chain(trusted.clone(), None);
                } else {
                    j.connector.kick();
                }
                j.sessions.sweep(trusted.head());
            }
            // Another Ring (or none): replace the Connector.
            _ => self.install(&mut life, trusted.clone(), keys)?,
        }
        drop(cur);
        if let Some(e) = saved.conflict {
            crate::log!("ERROR", "ring: refused a roster: {e}");
            return Err(format!("roster refused: {e}"));
        }
        if let Some(e) = saved.write_error {
            crate::log!(
                "ERROR",
                "ring: the new roster is in force but not saved: {e}"
            );
            return Err(format!(
                "the roster is in force but could not be saved: {e}"
            ));
        }
        let version = trusted.head().version();
        Ok(json!({ "version": version }))
    }

    /// Exiting: say goodbye with `reason`. Waits for the Relay (bounded by the connect and
    /// goodbye timeouts). No connector starts afterwards.
    pub fn stop(&self, reason: ByeReason) {
        let (joined, retiring) = {
            let mut life = self.lock_life();
            life.stopped = true;
            (life.joined.take(), std::mem::take(&mut life.retiring))
        };
        if let Some(j) = joined {
            j.sessions.stop();
            j.connector.stop(reason);
        }
        for h in retiring {
            let _ = h.join();
        }
    }
}

/// This Host's membership: the stored Ring and its head version, while a Connector runs for
/// it. The same rule `ring.identity` reports `ring` by.
fn membership(chain: Option<&RosterChain>, life: &Life) -> Option<(String, u64)> {
    match (chain, &life.joined) {
        (Some(c), Some(_)) => Some((c.ring_id().as_str().to_string(), c.head().version())),
        _ => None,
    }
}

fn expected(x: &JoinExpect, now: Option<(String, u64)>) -> bool {
    match (&x.ring_id, now) {
        (None, None) => true,
        (Some(want), Some((id, version))) => *want == id && x.version.is_none_or(|v| v == version),
        _ => false,
    }
}

pub(crate) fn is_daemon_member(chain: &RosterChain, keys: &DeviceKeys) -> bool {
    chain
        .head()
        .member(&keys.sign_key())
        .is_some_and(|m| m.role == Role::Daemon)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    #[test]
    fn private_files_refuse_symlinks_and_tighten_modes() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("keys.json");
        write_private(&p, b"x").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_private(&p).unwrap().as_deref().map(|v| v.as_slice()),
            Some(&b"x"[..])
        );
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        let link = t.path().join("link.json");
        symlink(&p, &link).unwrap();
        assert!(read_private(&link).is_err());
        assert_eq!(read_private(&t.path().join("none")).unwrap(), None);
        // Only the target is left: no temp files.
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 2);
    }

    /// A `join` that passed the `stopped` check installs under the lifecycle lock; a `stop`
    /// arriving meanwhile drains after it, so nothing stays installed.
    #[test]
    fn join_racing_stop_never_leaves_a_connector_installed() {
        use std::sync::mpsc;
        use xshell_protocol::ring::{Member, SignedRoster};
        let t = tempfile::tempdir().unwrap();
        let timeouts = RingTimeouts {
            connect: Duration::from_millis(500),
            ..Default::default()
        };
        let ring = Arc::new(Ring::new(
            t.path().join("ring"),
            Duration::from_millis(10),
            timeouts,
            Duration::from_secs(5),
        ));
        let id = ring.identity().unwrap();
        let sign = xshell_protocol::ring::SignKey::parse(id["signKey"].as_str().unwrap()).unwrap();
        let noise =
            xshell_protocol::ring::NoiseKey::parse(id["noiseKey"].as_str().unwrap()).unwrap();
        let desk = DeviceKeys::generate().unwrap();
        // Nothing listens there; the Relay plays no part.
        let url = "ws://127.0.0.1:9";
        let g = SignedRoster::genesis(&desk, desk.noise_key(), "d", url, 1).unwrap();
        let v2 = g
            .next(&desk, 2, |d| {
                d.add(Member::new("h", Role::Daemon, sign, noise, 2))
            })
            .unwrap();
        let tokens = vec![g.token().to_string(), v2.token().to_string()];

        let (at_boundary, wait_boundary) = mpsc::channel();
        *ring.install_hook.lock().unwrap() = Some(Box::new(move || {
            let _ = at_boundary.send(());
            std::thread::sleep(Duration::from_millis(150));
        }));
        let r = ring.clone();
        let joiner = std::thread::spawn(move || r.join(&tokens, None));
        wait_boundary.recv().unwrap();
        // Stop while the join sits between its check and its install.
        let r = ring.clone();
        let stopper = std::thread::spawn(move || r.stop(ByeReason::quit()));
        assert!(
            joiner.join().unwrap().is_ok(),
            "the join was already past the check"
        );
        stopper.join().unwrap();
        let life = ring.lock_life();
        assert!(life.stopped);
        assert!(life.joined.is_none(), "drained after the install");
        assert!(life.retiring.is_empty());
        drop(life);
        // And a join after the drain is refused.
        *ring.install_hook.lock().unwrap() = None;
        let tokens = vec![g.token().to_string(), v2.token().to_string()];
        assert_eq!(ring.join(&tokens, None).unwrap_err(), "xshelld is exiting");
    }

    /// `serve` (`ring.identity`) and `xshelld pair` making this Host's keys at the same
    /// time: one set is created and both use it.
    #[test]
    fn simultaneous_first_use_makes_one_identity() {
        for _ in 0..5 {
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().join("ring");
            let ring = Arc::new(Ring::new(
                dir.clone(),
                Duration::from_millis(10),
                RingTimeouts::default(),
                Duration::from_secs(5),
            ));
            let barrier = Arc::new(std::sync::Barrier::new(5));
            let mut hs = Vec::new();
            for i in 0..5 {
                let (ring, dir, b) = (ring.clone(), dir.clone(), barrier.clone());
                hs.push(std::thread::spawn(move || {
                    b.wait();
                    if i == 0 {
                        ring.identity().unwrap()["signKey"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    } else {
                        Store::new(dir)
                            .load_or_create_keys()
                            .unwrap()
                            .sign_key()
                            .to_b64()
                    }
                }));
            }
            let keys: Vec<String> = hs.into_iter().map(|h| h.join().unwrap()).collect();
            assert!(keys.iter().all(|k| *k == keys[0]), "{keys:?}");
            let on_disk = Store::new(dir)
                .load_keys()
                .unwrap()
                .unwrap()
                .sign_key()
                .to_b64();
            assert_eq!(on_disk, keys[0]);
        }
    }

    /// `xshelld pair` and `serve` storing chains of one Ring in turn: the store only ever
    /// moves forward along the chain, a fork is refused (and the verified head still rules
    /// in memory), and what is trusted in memory follows the store's newer head.
    #[test]
    fn interleaved_stores_never_roll_back_or_fork() {
        use xshell_protocol::ring::{Member, SignedRoster};
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let ring = Ring::new(
            dir.clone(),
            Duration::from_millis(10),
            RingTimeouts {
                connect: Duration::from_millis(300),
                ..Default::default()
            },
            Duration::from_secs(5),
        );
        let id = ring.identity().unwrap();
        let sign = xshell_protocol::ring::SignKey::parse(id["signKey"].as_str().unwrap()).unwrap();
        let noise =
            xshell_protocol::ring::NoiseKey::parse(id["noiseKey"].as_str().unwrap()).unwrap();
        let desk = DeviceKeys::generate().unwrap();
        let g = SignedRoster::genesis(&desk, desk.noise_key(), "d", "ws://127.0.0.1:9", 1).unwrap();
        let mut v = vec![g.clone()];
        v.push(
            g.next(&desk, 2, |d| {
                d.add(Member::new("h", Role::Daemon, sign, noise, 2))
            })
            .unwrap(),
        );
        for i in 3..=5u64 {
            let k = DeviceKeys::generate().unwrap();
            let n = v.last().unwrap().next(&desk, i, |d| {
                d.add(Member::new(
                    "m",
                    Role::Mobile,
                    k.sign_key(),
                    k.noise_key(),
                    i,
                ))
            });
            v.push(n.unwrap());
        }
        let upto = |n: usize| RosterChain::from_chain(v[..n].to_vec()).unwrap();
        let tokens = |c: &RosterChain| -> Vec<String> {
            c.versions().iter().map(|r| r.token().to_string()).collect()
        };
        let store = Store::new(dir.clone());
        assert_eq!(ring.join(&tokens(&upto(2)), None).unwrap()["version"], 2);
        // `xshelld pair` stores v4; `serve` then joins v3: the store keeps v4, and so does
        // the trusted chain in memory.
        {
            let _l = store.lock().unwrap();
            store.write_chain(&upto(4)).unwrap();
        }
        assert_eq!(ring.join(&tokens(&upto(3)), None).unwrap()["version"], 4);
        assert_eq!(store.read_chain().unwrap().unwrap(), upto(4));
        assert_eq!(store.save_chain(&upto(2)).trusted, upto(4));
        // A fork of a higher version: refused by the store, which keeps its chain.
        let other = DeviceKeys::generate().unwrap();
        let fork5 = v[3]
            .next(&desk, 9, |d| {
                d.add(Member::new(
                    "o",
                    Role::Mobile,
                    other.sign_key(),
                    other.noise_key(),
                    9,
                ))
            })
            .unwrap();
        let mut fv = v[..4].to_vec();
        fv.push(fork5);
        let forked = RosterChain::from_chain(fv).unwrap();
        {
            let _l = store.lock().unwrap();
            store.write_chain(&upto(5)).unwrap();
        }
        let s5 = store.save_chain(&forked);
        assert!(s5.conflict.is_some());
        assert_eq!(s5.trusted, upto(5));
        assert_eq!(store.read_chain().unwrap().unwrap(), upto(5));
        // `serve` handed the fork: refused, and the established v5 is what is in force.
        let e = ring.join(&tokens(&forked), None).unwrap_err();
        assert!(e.contains("conflicts"), "{e}");
        assert_eq!(store.read_chain().unwrap().unwrap(), upto(5));
        assert_eq!(ring.identity().unwrap()["ring"]["version"], 5);
        // Another Ring is an explicit replacement, not a fork.
        let desk2 = DeviceKeys::generate().unwrap();
        let g2 =
            SignedRoster::genesis(&desk2, desk2.noise_key(), "d2", "ws://127.0.0.1:9", 1).unwrap();
        let v2b = g2
            .next(&desk2, 2, |d| {
                d.add(Member::new("h", Role::Daemon, sign, noise, 2))
            })
            .unwrap();
        let other_ring = RosterChain::from_chain(vec![g2, v2b]).unwrap();
        assert_eq!(ring.join(&tokens(&other_ring), None).unwrap()["version"], 2);
        assert_eq!(store.read_chain().unwrap().unwrap(), other_ring);
        ring.stop(ByeReason::quit());
    }

    /// A Relay callback that lost the race to a local `ring.join` (it carries an older head)
    /// changes nothing: the Connector keeps the joined head.
    #[test]
    fn a_stale_relay_callback_never_rolls_the_connector_back() {
        use xshell_protocol::ring::{Member, SignedRoster};
        let t = tempfile::tempdir().unwrap();
        let ring = Ring::new(
            t.path().join("ring"),
            Duration::from_millis(10),
            RingTimeouts {
                connect: Duration::from_millis(300),
                ..Default::default()
            },
            Duration::from_secs(5),
        );
        let id = ring.identity().unwrap();
        let sign = xshell_protocol::ring::SignKey::parse(id["signKey"].as_str().unwrap()).unwrap();
        let noise =
            xshell_protocol::ring::NoiseKey::parse(id["noiseKey"].as_str().unwrap()).unwrap();
        let desk = DeviceKeys::generate().unwrap();
        let g = SignedRoster::genesis(&desk, desk.noise_key(), "d", "ws://127.0.0.1:9", 1).unwrap();
        let mut v = vec![g.clone()];
        v.push(
            g.next(&desk, 2, |d| {
                d.add(Member::new("h", Role::Daemon, sign, noise, 2))
            })
            .unwrap(),
        );
        let k = DeviceKeys::generate().unwrap();
        let v3 = v[1]
            .next(&desk, 3, |d| {
                d.add(Member::new(
                    "m",
                    Role::Mobile,
                    k.sign_key(),
                    k.noise_key(),
                    3,
                ))
            })
            .unwrap();
        v.push(v3);
        let upto = |n: usize| RosterChain::from_chain(v[..n].to_vec()).unwrap();
        let tokens: Vec<String> = upto(3)
            .versions()
            .iter()
            .map(|r| r.token().to_string())
            .collect();
        assert_eq!(ring.join(&tokens, None).unwrap()["version"], 3);
        let (connector, sessions) = {
            let life = ring.lock_life();
            let j = life.joined.as_ref().unwrap();
            (j.connector.clone(), j.sessions.clone())
        };
        // The callback of the older head arrives after the join.
        ring.persist
            .keep_newer(&upto(2), Some(&connector), &sessions);
        assert_eq!(connector.chain(), upto(3));
        assert_eq!(
            ring.persist.chain.lock().unwrap().as_ref().unwrap(),
            &upto(3)
        );
        ring.stop(ByeReason::quit());
    }

    #[test]
    fn host_name_is_a_valid_member_name() {
        let n = host_name();
        assert!(!n.is_empty() && n.len() <= 64);
    }
}

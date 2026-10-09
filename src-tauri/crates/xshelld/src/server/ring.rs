//! This Host as a Ring member (`daemon` role): its device keys, the Roster chain it trusts,
//! and the Relay connection ([`Connector`]) that reports it online. The Relay carries only
//! presence and the Roster here; envelopes are ignored until the Noise sessions (#9).
//!
//! State lives in [`Paths::ring_dir`](crate::paths::Paths::ring_dir), 0700:
//! - `keys.json` `{"v":1,"signSeed":b64u,"noiseSeed":b64u}`, created on the first
//!   `ring.identity`; the private keys never leave this Host;
//! - `roster.json` `{"v":1,"rosters":["xro1…",…]}`, the whole verified chain from version 1.
//!
//! Both are 0600, written through an exclusive temp file and a rename. An existing file that
//! is a symlink or another user's is refused; one with a broader mode is tightened.

use crate::paths::ensure_private_dir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use xshell_protocol::msg::{JoinExpect, MEMBERSHIP_CHANGED};
use xshell_protocol::ring::relay::{
    ByeReason, Connector, ConnectorConfig, ConnectorEvents, LinkState, RingClientConfig,
    RingTimeouts,
};
use xshell_protocol::ring::{member_name, DeviceKeys, Role, RosterChain, RosterError, SecretSeed};

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
struct Store {
    dir: PathBuf,
}

impl Store {
    fn keys_path(&self) -> PathBuf {
        self.dir.join("keys.json")
    }

    fn roster_path(&self) -> PathBuf {
        self.dir.join("roster.json")
    }

    fn ensure_dir(&self) -> io::Result<()> {
        ensure_private_dir(&self.dir)
    }

    /// The stored keys; `None` when there are none yet. An unreadable file is an error, never
    /// replaced: the keys are this Host's identity.
    fn load_keys(&self) -> Result<Option<DeviceKeys>, String> {
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

    fn create_keys(&self) -> Result<DeviceKeys, String> {
        self.ensure_dir().map_err(|e| e.to_string())?;
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

    /// The stored chain. An unreadable or unverifiable file is moved aside and logged: this
    /// Host is then in no Ring until a Desktop sends `ring.join` again.
    fn load_chain(&self) -> Option<RosterChain> {
        let p = self.roster_path();
        let bytes = match read_private(&p) {
            Ok(Some(b)) => b,
            Ok(None) => return None,
            Err(e) => {
                crate::log!("ERROR", "cannot read {}: {e}", p.display());
                return None;
            }
        };
        let chain = serde_json::from_slice::<RosterFile>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|f| {
                if f.v != 1 {
                    return Err("unknown format".to_string());
                }
                RosterChain::from_tokens(&f.rosters).map_err(|e| e.to_string())
            });
        match chain {
            Ok(c) => Some(c),
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

    fn save_chain(&self, chain: &RosterChain) -> Result<(), String> {
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
}

/// The chain the store holds, shared with the Connector's callbacks.
struct Persist {
    store: Store,
    chain: Mutex<Option<RosterChain>>,
}

impl Persist {
    /// Keeps `chain` if it is the stored Ring's and newer (or there is none).
    fn keep_newer(&self, chain: &RosterChain) {
        let mut cur = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cur.as_ref() {
            if c.ring_id() != chain.ring_id() || c.head().version() >= chain.head().version() {
                return;
            }
        }
        match self.store.save_chain(chain) {
            Ok(()) => *cur = Some(chain.clone()),
            Err(e) => crate::log!("ERROR", "cannot save the Roster: {e}"),
        }
    }
}

struct Events {
    persist: Arc<Persist>,
}

impl ConnectorEvents for Events {
    fn state(&self, s: &LinkState) {
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
        self.persist.keep_newer(chain);
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
    pub fn new(dir: PathBuf, backoff_unit: Duration, timeouts: RingTimeouts) -> Ring {
        Ring {
            persist: Arc::new(Persist {
                store: Store { dir },
                chain: Mutex::new(None),
            }),
            keys: Mutex::new(None),
            life: Mutex::new(Life::default()),
            name: host_name(),
            backoff_unit,
            timeouts,
            #[cfg(test)]
            install_hook: Mutex::new(None),
        }
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
            let k = match self.persist.store.load_keys()? {
                Some(k) => Some(k),
                None if create => Some(self.persist.store.create_keys()?),
                None => None,
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
        let mut client = RingClientConfig::new(chain, keys);
        client.timeouts = self.timeouts;
        let mut cfg = ConnectorConfig::new(client);
        cfg.backoff_unit = self.backoff_unit;
        let c = Connector::start(
            cfg,
            Arc::new(Events {
                persist: self.persist.clone(),
            }),
        )
        .map_err(|e| e.to_string())?;
        life.joined = Some(Joined {
            connector: Arc::new(c),
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
        let version = next.head().version();
        let changed = cur.as_ref() != Some(&next);
        if changed {
            self.persist.store.save_chain(&next)?;
            *cur = Some(next.clone());
        }
        drop(cur);
        match life.joined.as_ref() {
            Some(j) if same_ring => {
                if changed {
                    j.connector.set_chain(next, None);
                } else {
                    j.connector.kick();
                }
            }
            // Another Ring (or none): replace the Connector.
            _ => self.install(&mut life, next, keys)?,
        }
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

fn is_daemon_member(chain: &RosterChain, keys: &DeviceKeys) -> bool {
    chain
        .head()
        .member(&keys.sign_key())
        .is_some_and(|m| m.role == Role::Daemon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

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

    #[test]
    fn host_name_is_a_valid_member_name() {
        let n = host_name();
        assert!(!n.is_empty() && n.len() <= 64);
    }
}

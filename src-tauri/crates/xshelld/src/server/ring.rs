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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
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

pub(crate) mod ownership;

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

/// Stored objects have different semantic sizes; none is implicitly one Host frame.
#[derive(Clone, Copy)]
pub(crate) enum PrivateRingFile {
    Keys,
    RosterHistory,
    PushRegistrations,
    #[cfg(any(unix, windows))]
    CompanionPolicy,
    #[cfg(any(unix, windows))]
    StandaloneJournal,
    #[cfg(any(unix, windows))]
    CompletionMailbox,
}
#[cfg(unix)]
const MAX_STORED_TOKEN_ARRAY_JSON_BYTES: usize = xshell_protocol::ring::chain::MAX_CHAIN_LEN
    .checked_mul(
        xshell_protocol::ring::roster::MAX_ROSTER_TOKEN
            .checked_add(3)
            .expect("token delimiters"),
    )
    .expect("stored token history")
    .checked_add(2)
    .expect("array framing");
#[cfg(unix)]
impl PrivateRingFile {
    fn maximum(self) -> usize {
        match self {
            // Two canonical base64url32-byte seeds, u32 version and fixed JSON labels.
            Self::Keys => 512,
            // Tokens' ASCII base64url/prefix alphabet needs no JSON escaping. Account
            // for every supported token, quotes/comma, and the normal writer's wrapper.
            Self::RosterHistory => MAX_STORED_TOKEN_ARRAY_JSON_BYTES
                .checked_add(br#"{"v":4294967295,"rosters":}"#.len())
                .expect("roster file framing"),
            // on_head prunes to at most64 roster members; each registration has a
            // MAX_BLOB ASCII value, fixed keys/triggers/counters and bounded dormant code.
            Self::PushRegistrations => xshell_protocol::ring::roster::MAX_MEMBERS
                .checked_mul(
                    xshell_protocol::ring::push::MAX_BLOB
                        .checked_add(512)
                        .expect("push registration framing"),
                )
                .expect("push member storage")
                .checked_add(512)
                .expect("push file framing"),
            Self::CompanionPolicy => {
                xshell_core::companion_reservation::MAX_COMPANION_POLICY_JSON_BYTES
            }
            // Standalone stores full proposal-predecessor and accepted histories.
            // Completion stores full initial/accepted histories plus a prepared proposal
            // (whose additional predecessor is bounded by the companion envelope).
            Self::StandaloneJournal | Self::CompletionMailbox => MAX_STORED_TOKEN_ARRAY_JSON_BYTES
                .checked_mul(2)
                .expect("journal histories")
                .checked_add(xshell_core::companion_reservation::MAX_COMPANION_POLICY_JSON_BYTES)
                .expect("journal metadata/proposal framing"),
        }
    }
}

/// Opens `path` for reading if it is a regular file of ours (not a symlink); tightens a mode
/// broader than 0600. `None`: it does not exist.
#[cfg(windows)]
pub(crate) fn read_private(
    path: &Path,
    _kind: PrivateRingFile,
) -> io::Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
    let Some(f) = xshell_core::private_fs::open_read(path)? else {
        return Ok(None);
    };
    let len = f.metadata()?.len() as usize;
    let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(len + 1));
    (&f).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

#[cfg(unix)]
pub(crate) fn read_private(
    path: &Path,
    kind: PrivateRingFile,
) -> io::Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
    let f = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
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
    let maximum = kind.maximum();
    if meta.len() > maximum as u64 {
        return Err(io::Error::other(
            "private Ring file exceeds stored-object bound",
        ));
    }
    // Sized up front, so no reallocation leaves a copy of a secret behind.
    let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(meta.len() as usize + 1));
    (&f).take(maximum as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > maximum {
        return Err(io::Error::other(
            "private Ring file exceeds stored-object bound",
        ));
    }
    Ok(Some(buf))
}

/// Writes `bytes` to `path` atomically: an exclusive 0600 temp file with a unique name (never
/// following a symlink), synced, then renamed over `path`.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_private_inner(path, bytes, None)
}

fn write_private_inner(
    path: &Path,
    bytes: &[u8],
    roster_hook: Option<&super::TestHook>,
) -> io::Result<()> {
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
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if roster_hook
            .is_some_and(|hook| (hook.0)(uuid::Uuid::nil(), super::TestPoint::RingRosterTempSynced))
        {
            return Err(io::Error::other(
                "injected roster commit failure after temp sync",
            ));
        }
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

/// A stopped or unreadable registration is still a binding, never headless permission.
pub(crate) fn companion_registered(dir: &Path) -> bool {
    ["companion.json", "companion-pending.json"].iter().any(|name|
        !matches!(fs::symlink_metadata(dir.join(name)), Err(error) if error.kind() == io::ErrorKind::NotFound))
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
    fn open_lock(&self, name: &str) -> Result<fs::File, String> {
        self.ensure_dir().map_err(|e| e.to_string())?;
        let path = self.dir.join(name);
        #[cfg(windows)]
        let f = xshell_core::private_fs::open_or_create(&path);
        #[cfg(unix)]
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path);
        let f = f.map_err(|e| format!("cannot open {name}: {e}"))?;
        #[cfg(unix)]
        {
            let metadata = f.metadata().map_err(|e| e.to_string())?;
            if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err("foreign or nonregular Ring lock".into());
            }
            f.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        Ok(f)
    }
    pub fn lock(&self) -> Result<StoreLock, String> {
        let file = self.open_lock("ring.lock")?;
        file.lock()
            .map_err(|e| format!("cannot take the ring lock: {e}"))?;
        Ok(StoreLock(file))
    }
    pub(super) fn lock_until(
        &self,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<StoreLock, String> {
        budget.check().map_err(|e| e.to_string())?;
        let file = self.open_lock("ring.lock")?;
        loop {
            budget.check().map_err(|e| e.to_string())?;
            match file.try_lock() {
                Ok(()) => return Ok(StoreLock(file)),
                Err(fs::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
                Err(fs::TryLockError::Error(error)) => return Err(error.to_string()),
            }
        }
    }
    #[cfg(any(unix, windows))]
    fn offer_lock(&self) -> Result<StoreLock, String> {
        let file = self.open_lock("ring-legacy-offer.lock")?;
        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => {
                "legacy_offer_active: a legacy pairing offer is still open".into()
            }
            fs::TryLockError::Error(error) => error.to_string(),
        })?;
        Ok(StoreLock(file))
    }
    /// A nonblocking, process-lifetime admission lease. The Ring transaction lock is
    /// released before the legacy offer waits for a person/network; process death closes it.
    #[cfg(unix)]
    #[cfg(any(test, feature = "legacy-pair-test-control"))]
    pub(crate) fn reserve_legacy_offer(&self) -> Result<StoreLock, String> {
        let offer = self.offer_lock()?;
        let _ring = self.lock()?;
        self.refuse_registered_join()?;
        Ok(offer)
    }
    #[cfg(unix)]
    pub(super) fn reserve_standalone_offer(
        &self,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<StoreLock, String> {
        let offer = self.offer_lock()?;
        let _ring = self.lock_until(budget)?;
        self.refuse_registered_join()?;
        Ok(offer)
    }
    /// Called only while ring.lock is held; refuses instead of waiting for a live offer.
    #[cfg(any(unix, windows))]
    pub(super) fn registration_admission(&self) -> Result<StoreLock, String> {
        self.offer_lock()
    }
    pub(crate) fn refuse_registered_join(&self) -> Result<(), String> {
        #[cfg(any(unix, windows))]
        super::standalone_journal::refuse_unresolved_standalone(self)?;
        self.refuse_registered_binding()
    }
    pub(super) fn refuse_registered_binding(&self) -> Result<(), String> {
        if companion_registered(&self.dir) {
            return Err("protected_counterpart_required: registered Computer installation is not a legacy/Host commit".into());
        }
        Ok(())
    }
    #[cfg(any(unix, windows))]
    pub(super) fn keys_locked(&self) -> Result<DeviceKeys, String> {
        match self.load_keys()? {
            Some(keys) => Ok(keys),
            None => self.create_keys(),
        }
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

    pub(crate) fn ensure_dir(&self) -> io::Result<()> {
        // Windows: created owned by this user with a user-only DACL, or made so (see
        // `xshell_core::private_fs`).
        #[cfg(windows)]
        {
            // Validate the existing ancestor before recursive private creation can traverse it.
            for ancestor in self.dir.ancestors() {
                match fs::symlink_metadata(ancestor) {
                    Ok(_) => {
                        xshell_core::server::no_reparse(ancestor)?;
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                }
            }
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
        let Some(bytes) = read_private(&p, PrivateRingFile::Keys).map_err(|e| e.to_string())?
        else {
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
        let Some(bytes) =
            read_private(&p, PrivateRingFile::RosterHistory).map_err(|e| e.to_string())?
        else {
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
        match read_private(&p, PrivateRingFile::RosterHistory) {
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
    /// conflict: nothing is written and the stored chain stays the trusted one. Foreign Ring
    /// replacement requires Leave. A write that fails
    /// is reported apart: the settled chain is trusted all the same.
    pub fn save_chain(&self, chain: &RosterChain) -> Saved {
        let _l = match self.lock() {
            Ok(l) => l,
            Err(e) => return Saved::new(chain.clone(), Some(e)),
        };
        self.settle_locked(chain, None)
    }
    pub(super) fn save_join_chain(
        &self,
        chain: &RosterChain,
        effective_base: Option<&RosterChain>,
        origin: &JoinOrigin,
        budget: Option<&xshell_core::local_ipc::RequestBudget>,
        hook: Option<&super::TestHook>,
    ) -> Result<Saved, String> {
        let _lock = match budget {
            Some(budget) => self.lock_until(budget)?,
            None => self.lock()?,
        };
        match origin {
            JoinOrigin::Host => self.refuse_registered_join()?,
            #[cfg(unix)]
            JoinOrigin::Standalone { operation } => {
                super::standalone_journal::validate_standalone_install(self, operation, chain)?
            }
            #[cfg(any(unix, windows))]
            JoinOrigin::Companion { operation } => {
                super::companion::validate_install_locked(self, operation, chain)?
            }
        }
        if let Some(budget) = budget {
            budget.check().map_err(|e| e.to_string())?;
        }
        self.settle_accepted_locked(chain, effective_base, budget, hook)
    }

    /// `chain` has already been verified against the guarded effective owner. Any
    /// newer disk suffix must extend that effective ancestry BEFORE a write or adoption.
    fn settle_accepted_locked(
        &self,
        chain: &RosterChain,
        effective_base: Option<&RosterChain>,
        budget: Option<&xshell_core::local_ipc::RequestBudget>,
        hook: Option<&super::TestHook>,
    ) -> Result<Saved, String> {
        let disk = match self.read_chain() {
            Ok(disk) => disk,
            // Keep an independently validated revocation in force even when the
            // private file is unavailable. Never label that as a durable receipt or
            // overwrite unreadable evidence with a guessed disk ancestry.
            Err(error) => return Ok(Saved::new(chain.clone(), Some(error))),
        };
        let next = match disk {
            Some(disk) if disk.ring_id() == chain.ring_id() => {
                // Establish disk authority from the guarded base first. A valid newer
                // disk wins a candidate fork, but a terminal-signed disk suffix cannot.
                let durable = match effective_base {
                    Some(base) => {
                        base.extended(disk.versions())
                            .map_err(|e| {
                                format!("effective accepted ancestry refuses durable suffix: {e}")
                            })?
                            .0
                    }
                    None => disk.clone(),
                };
                let merged = match durable.extended(chain.versions()) {
                    Ok((merged, _)) => merged,
                    Err(error) => {
                        return Ok(Saved {
                            trusted: durable,
                            conflict: Some(format!(
                                "the roster conflicts with the one stored here ({})",
                                error.as_code()
                            )),
                            write_error: None,
                        })
                    }
                };
                if merged.versions() == disk.versions() {
                    return Ok(Saved::new(merged, None));
                }
                merged
            }
            Some(_) => {
                return Err("leave_required: accepted install cannot replace a foreign Ring".into())
            }
            None => chain.clone(),
        };
        if let Some(budget) = budget {
            budget.check().map_err(|e| e.to_string())?;
        }
        let error = self.write_chain_inner(&next, hook).err();
        Ok(Saved::new(next, error))
    }

    fn settle_locked(&self, chain: &RosterChain, hook: Option<&super::TestHook>) -> Saved {
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
            Ok(Some(disk)) => {
                return Saved {
                    trusted: disk,
                    conflict: Some("leave_required: this Host already retains another Ring".into()),
                    write_error: None,
                }
            }
            _ => chain.clone(),
        };
        let err = match hook {
            Some(hook) => self.write_chain_inner(&next, Some(hook)).err(),
            None => self.write_chain(&next).err(),
        };
        Saved::new(next, err)
    }

    /// Writes `chain` as it is; only with the lock held.
    pub fn write_chain(&self, chain: &RosterChain) -> Result<(), String> {
        self.write_chain_inner(chain, None)
    }
    fn write_chain_inner(
        &self,
        chain: &RosterChain,
        hook: Option<&super::TestHook>,
    ) -> Result<(), String> {
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
        if hook.is_some_and(|hook| {
            (hook.0)(uuid::Uuid::nil(), super::TestPoint::BeforeRingRosterWrite)
        }) {
            return Err("injected roster write failure after prerequisite reads".into());
        }
        write_private_inner(&self.roster_path(), &json, hook).map_err(|e| e.to_string())
    }
}

struct Joined {
    connector: Arc<Connector>,
    /// The Noise sessions over this Connector.
    sessions: Sessions,
}

/// Told every newly trusted chain with its adoption epoch (the push registrations prune
/// against it). The hook runs without the chain's lock, so calls can arrive out of order:
/// the epoch, taken under the lock, orders them, and a lower one is stale.
pub(crate) type HeadHook = Arc<dyn Fn(u64, &RosterChain) + Send + Sync>;

/// The chain the store holds, shared with the Connector's callbacks.
struct Persist {
    admission: std::sync::OnceLock<std::sync::Weak<ownership::StoreAdmission>>,
    test_hook: std::sync::OnceLock<super::TestHook>,
    store: Store,
    chain: Mutex<Option<RosterChain>>,
    on_head: std::sync::OnceLock<HeadHook>,
    /// Bumped, with the chain's lock held, every time a chain is adopted.
    epoch: AtomicU64,
}

impl Persist {
    /// The epoch of a chain being adopted now; only with the chain's lock held.
    fn adopt(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Tells the hook about `chain`, adopted at `epoch`; never with the chain's lock held.
    fn head_changed(&self, epoch: u64, chain: &RosterChain) {
        if let Some(h) = self.on_head.get() {
            h(epoch, chain);
        }
    }
}

impl Persist {
    /// A newer chain from the Relay: settled against the store first, then trusted in
    /// memory, handed to the Connector when the store had more (or held a conflicting
    /// fork, which is refused), and swept, all under the trusted chain's lock, which local
    /// joins hold for the same sequence. A chain not newer than the trusted one (a callback
    /// that lost the race to a local join) changes nothing.
    fn keep_newer(&self, chain: &RosterChain, connector: Option<&Connector>, sessions: &Sessions) {
        let _operation = if let Some(admission) = self.admission.get() {
            let Some(admission) = admission.upgrade() else {
                return;
            };
            let Ok(operation) = admission.admit() else {
                return;
            };
            Some(operation)
        } else {
            None
        };
        if let Some(hook) = self.test_hook.get() {
            (hook.0)(uuid::Uuid::nil(), super::TestPoint::RosterCallbackAdmitted);
        }
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
                c.set_chain(trusted.clone());
            }
        }
        sessions.sweep(trusted.head());
        *cur = Some(trusted.clone());
        let epoch = self.adopt();
        drop(cur);
        self.head_changed(epoch, &trusted);
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

pub(super) enum JoinOrigin {
    Host,
    #[cfg(unix)]
    Standalone {
        operation: String,
    },
    #[cfg(any(unix, windows))]
    Companion {
        operation: String,
    },
}

#[cfg(test)]
type HostBudgetHook = Box<dyn Fn(&xshell_core::local_ipc::RequestBudget) + Send>;

/// This Host's Ring membership.
pub(crate) struct Ring {
    test_hook: std::sync::OnceLock<super::TestHook>,
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
    #[cfg(test)]
    host_budget_hook: Mutex<Option<HostBudgetHook>>,
    #[cfg(test)]
    effective_query_hook: Mutex<Option<Box<dyn Fn() + Send>>>,
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
            test_hook: std::sync::OnceLock::new(),
            persist: Arc::new(Persist {
                admission: std::sync::OnceLock::new(),
                test_hook: std::sync::OnceLock::new(),
                store: Store { dir },
                chain: Mutex::new(None),
                on_head: std::sync::OnceLock::new(),
                epoch: AtomicU64::new(0),
            }),
            keys: Mutex::new(None),
            life: Mutex::new(Life::default()),
            hub: super::relay_conn::Hub::new(write_stall),
            name: host_name(),
            backoff_unit,
            timeouts,
            #[cfg(test)]
            install_hook: Mutex::new(None),
            #[cfg(test)]
            host_budget_hook: Mutex::new(None),
            #[cfg(test)]
            effective_query_hook: Mutex::new(None),
        }
    }

    pub(super) fn set_test_hook(&self, hook: super::TestHook) {
        let _ = self.persist.test_hook.set(hook.clone());
        let _ = self.test_hook.set(hook);
    }

    pub(super) fn bind_store_admission(&self, admission: &Arc<ownership::StoreAdmission>) {
        let _ = self.persist.admission.set(Arc::downgrade(admission));
    }

    pub fn hub(&self) -> &super::relay_conn::Hub {
        &self.hub
    }

    /// Calls `f` with every new trusted head (and the stored one at `resume`). Set once,
    /// before `resume`.
    pub fn set_on_head(&self, f: HeadHook) {
        let _ = self.persist.on_head.set(f);
    }

    /// The chain this Host trusts, if it is in a Ring.
    #[cfg(any(unix, windows))]
    pub(super) fn lock_trusted_chain_until<'a>(
        &'a self,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<std::sync::MutexGuard<'a, Option<RosterChain>>, String> {
        budget.lock(&self.persist.chain).map_err(|e| e.to_string())
    }
    /// Read effective evidence under lifecycle -> cache -> Connector -> file order.
    /// Identity and evidence stay fixed through the local read/receipt; no network,
    /// session/event callback or join is allowed in the closure. Observations release
    /// Connector state before calling persistence, so no reverse cache lock is held.
    #[cfg(any(unix, windows))]
    pub(super) fn with_effective_chain_until<T>(
        &self,
        budget: &xshell_core::local_ipc::RequestBudget,
        read: impl FnOnce(Option<RosterChain>) -> Result<T, String>,
    ) -> Result<T, String> {
        let life = budget.lock(&self.life).map_err(|e| e.to_string())?;
        if life.stopped {
            return Err("Daemon Ring is stopping".into());
        }
        let cached = self.lock_trusted_chain_until(budget)?;
        // Keep the read guard through the local file comparison/receipt so an owner
        // observation cannot become effective halfway through that transaction.
        let owner_guard = if let Some(joined) = &life.joined {
            Some(loop {
                budget.check().map_err(|e| e.to_string())?;
                if let Some(guard) = joined.connector.try_accepted_install() {
                    break guard;
                }
                std::thread::yield_now();
            })
        } else {
            None
        };
        let mut effective = cached.clone();
        if let Some(guard) = &owner_guard {
            let owner = guard.current();
            effective = Some(match effective {
                Some(mut chain) => {
                    chain.merge_terminal_from(owner);
                    chain
                        .extended(owner.versions())
                        .map_err(|e| e.to_string())?
                        .0
                }
                None => owner.clone(),
            });
        }
        budget.check().map_err(|e| e.to_string())?;
        #[cfg(test)]
        if let Some(hook) = self.effective_query_hook.lock().unwrap().as_ref() {
            hook();
        }
        let result = read(effective);
        drop(owner_guard);
        result
    }

    /// Unit-test coordination at the guarded local-read boundary; never authority.
    #[cfg(test)]
    // Used only by the Unix-only adoption reconciliation tests.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(super) fn set_effective_query_hook(&self, hook: Box<dyn Fn() + Send>) {
        *self.effective_query_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(unix)]
    pub(super) fn standalone_installed_before_journal(&self) {
        if let Some(hook) = self.test_hook.get() {
            (hook.0)(
                uuid::Uuid::nil(),
                super::TestPoint::StandaloneInstalledBeforeJournal,
            );
        }
    }

    #[cfg(unix)]
    pub(super) fn standalone_store(&self) -> &Store {
        &self.persist.store
    }

    /// The established order is runtime chain, then ring.lock. Never hold a file lock
    /// while waiting for the chain owner (including a paused ordinary Host commit).
    #[cfg(unix)]
    pub(super) fn standalone_transaction<T>(
        &self,
        budget: &xshell_core::local_ipc::RequestBudget,
        change: impl FnOnce(&Store) -> Result<T, String>,
    ) -> Result<T, String> {
        let live = self.lock_trusted_chain_until(budget)?;
        let store = &self.persist.store;
        let _lock = store.lock_until(budget)?;
        store.refuse_registered_join()?;
        if live.is_some() || super::companion::trusted(store)?.is_some() {
            return Err("leave_required: standalone adoption requires an unpaired Daemon".into());
        }
        budget.check().map_err(|e| e.to_string())?;
        change(store)
    }

    pub fn trusted_chain(&self) -> Option<RosterChain> {
        self.persist
            .chain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// What a push goes through: the Connector, this Host's keys and the trusted chain;
    /// `None` while no Connector runs.
    pub fn push_link(&self) -> Option<(Arc<Connector>, Arc<DeviceKeys>, RosterChain)> {
        let connector = self.lock_life().joined.as_ref()?.connector.clone();
        let keys = self
            .keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        Some((connector, keys, self.trusted_chain()?))
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
        let epoch = {
            let mut cur = self.persist.chain.lock().unwrap_or_else(|e| e.into_inner());
            *cur = Some(chain.clone());
            self.persist.adopt()
        };
        self.persist.head_changed(epoch, &chain);
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
        // Host IPC supplies no deadline. Bound this local install once at ingress
        // using the existing request timeout; never renew internal accepted budgets.
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(
            Instant::now() + self.timeouts.request,
            Arc::new(AtomicBool::new(false)),
        );
        #[cfg(test)]
        if let Some(hook) = self.host_budget_hook.lock().unwrap().as_ref() {
            hook(&budget);
        }
        self.join_inner(tokens, expect, JoinOrigin::Host, Some(&budget))
    }
    #[cfg(test)]
    // Used only by the Unix-only adoption reconciliation tests.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(super) fn test_host_join_until(
        &self,
        tokens: &[String],
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<Value, String> {
        self.join_inner(tokens, None, JoinOrigin::Host, Some(budget))
    }

    #[cfg(unix)]
    pub(super) fn join_accepted_standalone(
        &self,
        tokens: &[String],
        operation: &str,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<Value, String> {
        self.join_inner(
            tokens,
            None,
            JoinOrigin::Standalone {
                operation: operation.into(),
            },
            Some(budget),
        )
    }
    #[cfg(any(unix, windows))]
    pub(super) fn join_accepted_companion(
        &self,
        tokens: &[String],
        operation: &str,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<Value, String> {
        self.join_inner(
            tokens,
            None,
            JoinOrigin::Companion {
                operation: operation.into(),
            },
            Some(budget),
        )
    }
    fn join_inner(
        &self,
        tokens: &[String],
        expect: Option<&JoinExpect>,
        origin: JoinOrigin,
        budget: Option<&xshell_core::local_ipc::RequestBudget>,
    ) -> Result<Value, String> {
        if let Some(budget) = budget {
            budget.check().map_err(|e| e.to_string())?;
        }
        let refused = |e: RosterError| format!("roster refused: {}", e.as_code());
        let chain = RosterChain::from_tokens(tokens).map_err(refused)?;
        let keys = match budget {
            Some(budget) => {
                let mut cached = budget.lock(&self.keys).map_err(|e| e.to_string())?;
                if cached.is_none() {
                    *cached = self.persist.store.load_keys()?.map(Arc::new);
                }
                cached.clone()
            }
            None => self.keys(false)?,
        }
        .ok_or("no retained Daemon identity; request ring.identity first")?;
        if !is_daemon_member(&chain, &keys) {
            return Err("not a member of this Roster".into());
        }
        // The lifecycle lock for the whole commit and install (never taken by the Connector's
        // callbacks), then the persisted chain's: the Connector's own saves wait.
        let mut life = match budget {
            Some(budget) => budget.lock(&self.life).map_err(|e| e.to_string())?,
            None => self.lock_life(),
        };
        if life.stopped {
            return Err("xshelld is exiting".into());
        }
        #[cfg(test)]
        if let Some(h) = self.install_hook.lock().unwrap().as_ref() {
            h();
        }
        let mut cur = match budget {
            Some(budget) => budget
                .lock(&self.persist.chain)
                .map_err(|e| e.to_string())?,
            None => self.persist.chain.lock().unwrap_or_else(|e| e.into_inner()),
        };
        if let Some(x) = expect {
            if !expected(x, membership(cur.as_ref(), &life)) {
                return Err(MEMBERSHIP_CHANGED.into());
            }
        }
        let owner = life.joined.as_ref().map(|j| j.connector.clone());
        // The earlier lifecycle/cache locks serialize all file -> Connector paths in
        // this Ring. Owner callbacks drop Connector state before taking the cache lock.
        let install_guard = if let Some(owner) = &owner {
            let budget = budget.ok_or("accepted install requires its original request budget")?;
            Some(loop {
                budget.check().map_err(|e| e.to_string())?;
                if let Some(guard) = owner.try_accepted_install() {
                    break guard;
                }
                std::thread::yield_now();
            })
        } else {
            None
        };
        let current = match (&install_guard, cur.as_ref()) {
            (Some(guard), Some(cached)) => Some(guard.extend(cached).map_err(|e| e.to_string())?),
            (Some(guard), None) => Some(guard.current().clone()),
            (None, cached) => cached.cloned(),
        };
        if current
            .as_ref()
            .is_some_and(|c| c.ring_id() != chain.ring_id() || !is_daemon_member(c, &keys))
        {
            return Err(
                "leave_required: retained foreign or terminal membership cannot be replaced".into(),
            );
        }
        let same_ring = current
            .as_ref()
            .is_some_and(|c| c.ring_id() == chain.ring_id());
        let next = match current.as_ref() {
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
        if matches!(origin, JoinOrigin::Host) {
            if let Some(hook) = self.test_hook.get() {
                (hook.0)(
                    uuid::Uuid::nil(),
                    super::TestPoint::BeforeHostRingJoinCommit,
                );
            }
        }
        let saved = self.persist.store.save_join_chain(
            &next,
            current.as_ref(),
            &origin,
            budget,
            self.test_hook.get(),
        )?;
        let trusted = saved.trusted;
        // Settlement may already have changed disk, or produced an unsaved removal
        // that must rule now. Report budget failure only after coherent enforcement.
        let committed = match install_guard
            .map(|guard| guard.commit(&trusted))
            .transpose()
        {
            Ok(committed) => committed,
            Err(error) => {
                // The same guarded ancestry was verified before settlement, so this
                // is an invariant failure. Never return with the old owner live.
                *cur = Some(trusted.clone());
                let epoch = self.persist.adopt();
                life.stopped = true;
                let failed = life.joined.take();
                drop(cur);
                drop(life);
                if let Some(failed) = failed {
                    failed.sessions.stop();
                    failed.connector.stop(ByeReason::quit());
                }
                self.persist.head_changed(epoch, &trusted);
                return Err(format!(
                    "accepted state application failed; Ring owner stopped: {error}"
                ));
            }
        };
        // The guard is now released; no session callback runs under Connector state.
        // Then in force at once, whether or not the write succeeded: in memory, in the
        // Connector (whose head new handshakes are checked against), and in the sweep.
        // An existing guarded owner always uses the non-fallible same-Ring branch;
        // initial Connector creation below has no old authority to leave running.
        let same_ring = current
            .as_ref()
            .is_some_and(|c| c.ring_id() == trusted.ring_id());
        let moved = cur.as_ref() != Some(&trusted);
        *cur = Some(trusted.clone());
        let epoch = moved.then(|| self.persist.adopt());
        match life.joined.as_ref() {
            Some(j) if same_ring => {
                if committed.is_none() {
                    if moved {
                        j.connector.set_chain(trusted.clone());
                    } else {
                        j.connector.kick();
                    }
                }
                j.sessions.sweep(trusted.head());
            }
            // Another Ring (or none): replace the Connector.
            _ => self.install(&mut life, trusted.clone(), keys)?,
        }
        drop(cur);
        drop(life);
        if let Some(committed) = committed {
            committed.finish();
        }
        if let Some(epoch) = epoch {
            self.persist.head_changed(epoch, &trusted);
        }
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
        if let Some(budget) = budget {
            budget.check().map_err(|e| e.to_string())?;
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
        .authorized_member(&keys.sign_key())
        .is_some_and(|m| m.role == Role::Daemon)
}

/// Signed historical-format data for storage compatibility tests only, not an adoption
/// fixture or a principal/counterpart authorization receipt.
#[cfg(test)]
pub(super) fn historical_storage_chain(daemon: &DeviceKeys) -> (DeviceKeys, RosterChain) {
    use xshell_protocol::ring::{Member, SignedRoster};
    let signer = DeviceKeys::generate().unwrap();
    let genesis = SignedRoster::genesis(
        &signer,
        signer.noise_key(),
        "historical Desktop",
        "ws://127.0.0.1:9",
        1,
    )
    .unwrap();
    let mut versions = vec![genesis];
    for version in 2..=1401 {
        let next = versions
            .last()
            .unwrap()
            .next(&signer, version, |draft| {
                if version == 2 {
                    draft.add(Member::new(
                        "historical Daemon",
                        Role::Daemon,
                        daemon.sign_key(),
                        daemon.noise_key(),
                        2,
                    ));
                    draft.extra.insert(
                        "historicalStoragePadding".into(),
                        serde_json::Value::String("x".repeat(40 * 1024)),
                    );
                }
            })
            .unwrap();
        assert!(next.token().len() <= xshell_protocol::ring::roster::MAX_ROSTER_TOKEN);
        versions.push(next);
    }
    assert!(versions.len() <= xshell_protocol::ring::chain::MAX_CHAIN_LEN);
    (signer, RosterChain::from_chain(versions).unwrap())
}
#[cfg(test)]
pub(super) fn storage_test_directory() -> tempfile::TempDir {
    let scratch = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    tempfile::Builder::new()
        .prefix("signed-history-storage-")
        .tempdir_in(scratch)
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    /// Historical v1 storage compatibility, not a new adoption/bootstrap authority.
    #[test]
    fn valid_signed_roster_history_larger_than_a_host_frame_survives_storage_restart() {
        let temp = storage_test_directory();
        let store = Store::new(temp.path().join("ring"));
        let daemon = store.load_or_create_keys().unwrap();
        let (_, chain) = historical_storage_chain(&daemon);
        {
            let _lock = store.lock().unwrap();
            store.write_chain(&chain).unwrap();
        }
        let stored_bytes = fs::metadata(store.roster_path()).unwrap().len();
        assert!(
            stored_bytes > xshell_protocol::frame::MAX_FRAME_LEN as u64,
            "fixture must exceed the old Host-frame cap"
        );
        eprintln!(
            "STORAGE_COMPAT roster.json bytes={stored_bytes} signed_versions={}",
            chain.versions().len()
        );
        let readback = store
            .read_chain()
            .expect("valid signed retained history must be readable")
            .unwrap();
        assert!(
            readback == chain,
            "exact verified historical tokens must survive readback"
        );
        drop(readback);
        drop(store);
        let restarted_store = Store::new(temp.path().join("ring"));
        // This is the same retained-trust loader used by Ring::resume; no Relay is dialed.
        let restarted = restarted_store
            .load_chain()
            .expect("restart must retain valid signed history");
        assert!(
            restarted == chain,
            "restart must retain exact history, not truncate or replace it"
        );
        assert_eq!(
            restarted_store.load_keys().unwrap().unwrap().sign_key(),
            daemon.sign_key()
        );
        assert_eq!(
            fs::metadata(restarted_store.roster_path()).unwrap().len(),
            stored_bytes
        );
        assert!(!fs::read_dir(&restarted_store.dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".bad-")));
    }
    #[cfg(unix)]
    #[test]
    fn stored_object_bounds_cover_histories_and_reject_each_sparse_oversize() {
        let temp = storage_test_directory();
        for kind in [
            PrivateRingFile::Keys,
            PrivateRingFile::RosterHistory,
            PrivateRingFile::PushRegistrations,
            PrivateRingFile::CompanionPolicy,
            PrivateRingFile::StandaloneJournal,
            PrivateRingFile::CompletionMailbox,
        ] {
            let path = temp.path().join("object.json");
            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            let limit = kind.maximum();
            file.set_len(limit as u64 + 1).unwrap();
            assert!(read_private(&path, kind).is_err());
            assert_eq!(fs::metadata(&path).unwrap().len(), limit as u64 + 1);
        }
        let history = PrivateRingFile::RosterHistory.maximum();
        assert!(history > xshell_protocol::frame::MAX_FRAME_LEN as usize);
        assert!(
            PrivateRingFile::StandaloneJournal.maximum() >= 2 * MAX_STORED_TOKEN_ARRAY_JSON_BYTES
        );
        assert!(
            PrivateRingFile::CompletionMailbox.maximum()
                >= 2 * MAX_STORED_TOKEN_ARRAY_JSON_BYTES
                    + xshell_protocol::ring::companion::MAX_COMPANION_FRAME
        );
        assert!(PrivateRingFile::Keys.maximum() < 1024);
        assert!(PrivateRingFile::PushRegistrations.maximum() < 1024 * 1024);
        assert_eq!(
            PrivateRingFile::CompanionPolicy.maximum(),
            xshell_core::companion_reservation::MAX_COMPANION_POLICY_JSON_BYTES
        );
    }
    #[cfg(unix)]
    #[test]
    fn private_ring_file_size_bound_rejects_sparse_oversize_before_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keys.json");
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let length = PrivateRingFile::Keys.maximum() as u64 + 1;
        file.set_len(length).unwrap();
        assert!(read_private(&path, PrivateRingFile::Keys).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), length);
    }
    #[cfg(unix)]
    #[test]
    fn private_files_refuse_symlinks_and_tighten_modes() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("keys.json");
        write_private(&p, b"x").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_private(&p, PrivateRingFile::Keys)
                .unwrap()
                .as_deref()
                .map(|v| v.as_slice()),
            Some(&b"x"[..])
        );
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        let link = t.path().join("link.json");
        symlink(&p, &link).unwrap();
        assert!(read_private(&link, PrivateRingFile::Keys).is_err());
        assert_eq!(
            read_private(&t.path().join("none"), PrivateRingFile::Keys).unwrap(),
            None
        );
        // Only the target is left: no temp files.
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn effective_chain_query_bounds_lifecycle_chain_and_file_admission() {
        use std::sync::atomic::AtomicBool;
        use std::time::Instant;
        let dir = tempfile::tempdir().unwrap();
        let ring = Ring::new(
            dir.path().join("ring"),
            Duration::from_millis(10),
            RingTimeouts::default(),
            Duration::from_secs(5),
        );
        let budget = || {
            xshell_core::local_ipc::RequestBudget::for_operation(
                Instant::now() + Duration::from_millis(30),
                Arc::new(AtomicBool::new(false)),
            )
        };
        let life = ring.lock_life();
        let error = ring
            .with_effective_chain_until(&budget(), |_| Ok(()))
            .unwrap_err();
        assert!(error.contains("deadline"));
        drop(life);
        let cached = ring.persist.chain.lock().unwrap();
        let error = ring
            .with_effective_chain_until(&budget(), |_| Ok(()))
            .unwrap_err();
        assert!(error.contains("deadline"));
        drop(cached);
        let _file = ring.persist.store.lock().unwrap();
        let budget = budget();
        let error = ring
            .with_effective_chain_until(&budget, |_| {
                let _file = ring.persist.store.lock_until(&budget)?;
                Ok(())
            })
            .unwrap_err();
        assert!(error.contains("deadline"));
        assert!(ring.life.try_lock().is_ok());
        assert!(ring.persist.chain.try_lock().is_ok());
    }

    #[test]
    fn host_join_spends_one_budget_across_lifecycle_and_file_admission() {
        use std::sync::mpsc;
        use xshell_protocol::ring::{Member, SignedRoster};
        let home = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(180);
        let ring = Arc::new(Ring::new(
            home.path().join("ring"),
            Duration::from_millis(10),
            RingTimeouts {
                request: timeout,
                ..Default::default()
            },
            Duration::from_secs(5),
        ));
        ring.identity().unwrap();
        let keys = ring.persist.store.load_keys().unwrap().unwrap();
        let desktop = DeviceKeys::generate().unwrap();
        let genesis = SignedRoster::genesis(
            &desktop,
            desktop.noise_key(),
            "desktop",
            "ws://127.0.0.1:9",
            1000,
        )
        .unwrap();
        let joined = genesis
            .next(&desktop, 1001, |r| {
                r.add(Member::new(
                    "D",
                    Role::Daemon,
                    keys.sign_key(),
                    keys.noise_key(),
                    1001,
                ))
            })
            .unwrap();
        let chain = RosterChain::from_chain(vec![genesis, joined]).unwrap();
        let tokens: Vec<String> = chain.versions().iter().map(|r| r.token().into()).collect();
        ring.join(&tokens, None).unwrap();
        let successor = chain.head().next(&desktop, 1002, |_| {}).unwrap();
        let next = chain.extended(&[successor]).unwrap().0;
        let next_tokens: Vec<String> = next.versions().iter().map(|r| r.token().into()).collect();
        let (entry, entered) = mpsc::channel();
        *ring.host_budget_hook.lock().unwrap() = Some(Box::new(move |b| {
            entry.send(b.clone()).unwrap();
        }));
        let life = ring.lock_life();
        let file = ring.persist.store.lock().unwrap();
        let before = std::fs::read(ring.persist.store.roster_path()).unwrap();
        let r = ring.clone();
        let worker = std::thread::spawn(move || r.join(&next_tokens, None));
        let original = entered.recv_timeout(Duration::from_secs(2)).unwrap();
        // Deliberately spend part of this known captured request budget at the first
        // admission; this is a deadline control, not a fixture-stabilization sleep.
        let (_unreleased, wait) = mpsc::channel::<()>();
        assert!(wait.recv_timeout(timeout / 2).is_err());
        original.check().unwrap();
        drop(life);
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.contains("deadline"), "{error}");
        assert!(
            original.check().is_err(),
            "later file admission cannot renew the entry budget"
        );
        assert_eq!(
            std::fs::read(ring.persist.store.roster_path()).unwrap(),
            before
        );
        assert!(ring.life.try_lock().is_ok());
        assert!(ring.persist.chain.try_lock().is_ok());
        let owner = ring.lock_life().joined.as_ref().unwrap().connector.clone();
        assert!(owner.try_accepted_install().is_some());
        drop(file);
        ring.stop(ByeReason::quit());
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
        // Foreign membership requires an explicit Leave, even with a matching expected head.
        let desk2 = DeviceKeys::generate().unwrap();
        let g2 =
            SignedRoster::genesis(&desk2, desk2.noise_key(), "d2", "ws://127.0.0.1:9", 1).unwrap();
        let v2b = g2
            .next(&desk2, 2, |d| {
                d.add(Member::new("h", Role::Daemon, sign, noise, 2))
            })
            .unwrap();
        let other_ring = RosterChain::from_chain(vec![g2, v2b]).unwrap();
        assert!(ring
            .join(&tokens(&other_ring), None)
            .unwrap_err()
            .starts_with("leave_required:"));
        let expect = JoinExpect {
            ring_id: Some(upto(5).ring_id().to_string()),
            version: Some(5),
        };
        assert!(ring
            .join(&tokens(&other_ring), Some(&expect))
            .unwrap_err()
            .starts_with("leave_required:"));
        let refused = store.save_chain(&other_ring);
        assert!(refused
            .conflict
            .as_ref()
            .is_some_and(|e| e.starts_with("leave_required:")));
        assert_eq!(store.read_chain().unwrap().unwrap(), upto(5));
        assert_eq!(ring.identity().unwrap()["ring"]["version"], 5);
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

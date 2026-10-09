//! The Desktop's Ring state on disk: one file, `ring.json`, in a private directory, so the
//! keys, the chain and the local Daemon's membership never disagree after a crash.
//!
//! ```json
//! {"v":1,"signSeed":"b64u","noiseSeed":"b64u","rosters":["xro1…",…],
//!  "localMember":"<signKey>"|null,
//!  "pendingMove":{"source":"wss://old","from":N,"target":M}|null,
//!  "hostMembers":{"<hostId>":{"signKey":"<signKey>","target":"…"},…}}
//! ```
//!
//! `hostMembers` (the Remote Hosts' Daemons as last added to the Roster) is written only
//! when not empty, so a Ring without Remote Hosts keeps the earlier shape.
//!
//! Every change is a transaction ([`Store::transact`]): an in-process mutex and an exclusive
//! lock on `ring.lock` (so a second app instance waits), the state reloaded from disk, the
//! change, and an atomic write (an exclusive temp file with a unique name, synced, renamed
//! over `ring.json`). A file that does not parse or verify is moved aside and reported,
//! never silently replaced: until a new Ring is made explicitly, every transaction reports
//! the moved-aside file ([`Loaded::moved_aside`]).
//!
//! Secrets (the seeds, the file's bytes) live in buffers wiped when dropped, on every path.
//!
//! On Unix the directory is 0700 and the files 0600, owned by this user; a directory or file
//! that is a symlink or another user's is refused, and broader modes are tightened.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use xshell_protocol::ring::relay::MoveJob;
use xshell_protocol::ring::{DeviceKeys, RosterChain, SecretSeed, SignKey};
use zeroize::Zeroizing;

/// The Ring state this Desktop holds.
#[derive(Clone)]
pub struct RingState {
    pub keys: Arc<DeviceKeys>,
    pub chain: RosterChain,
    /// The sign key of the Daemon on this computer, as last added to the Roster.
    pub local_member: Option<SignKey>,
    /// A Relay move still owed to the old Relay (see `Connector`).
    pub pending_move: Option<MoveJob>,
    /// Per configured Remote Host (by id): the sign key of its Daemon, as last added to the
    /// Roster.
    pub host_members: BTreeMap<String, HostMember>,
}

/// A Remote Host's Daemon in the Roster.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostMember {
    pub sign_key: SignKey,
    /// Where the Host was reached when its key was recorded (its SSH target and Daemon
    /// command): a new key at the same place is a reinstall, at another place another
    /// machine.
    pub target: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct PendingFile {
    source: String,
    from: u64,
    target: u64,
}

/// The file as stored. The seeds decode straight into wiping storage, so every path, a
/// failed parse included, wipes them.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct File {
    v: u32,
    sign_seed: SecretSeed,
    noise_seed: SecretSeed,
    rosters: Vec<String>,
    local_member: Option<SignKey>,
    pending_move: Option<PendingFile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    host_members: BTreeMap<String, HostMember>,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct Store {
    dir: PathBuf,
    /// Serializes this process's transactions (the file lock covers other processes).
    tx: Mutex<()>,
}

/// What loading found besides the state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Loaded {
    /// An unreadable `ring.json` was moved here.
    pub moved_aside: Option<PathBuf>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store {
            dir,
            tx: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self) -> PathBuf {
        self.dir.join("ring.json")
    }

    /// Runs `f` on the state as on disk now, holding both locks; when `f` returns
    /// `Ok((Some(new), _))`, `new` is written before the locks are released. What loading
    /// found comes back with the result, whatever `f` decided.
    pub fn transact<R>(
        &self,
        f: impl FnOnce(Option<RingState>, &Loaded) -> Result<(Option<RingState>, R), String>,
    ) -> Result<(R, Loaded), String> {
        match self.transact_full(f) {
            (Ok(r), Some(l)) => Ok((r, l)),
            (Ok(r), None) => Ok((r, Loaded::default())),
            (Err(e), _) => Err(e),
        }
    }

    /// [`Store::transact`], with what loading found returned also when the transaction
    /// fails: `None` only when the state could not be loaded at all (the directory, the lock
    /// or the file itself was refused), and the error then says why.
    pub fn transact_full<R>(
        &self,
        f: impl FnOnce(Option<RingState>, &Loaded) -> Result<(Option<RingState>, R), String>,
    ) -> (Result<R, String>, Option<Loaded>) {
        let _g = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = ensure_private_dir(&self.dir) {
            return (Err(e.to_string()), None);
        }
        let lock = match open_lock(&self.dir.join("ring.lock")) {
            Ok(l) => l,
            Err(e) => return (Err(e.to_string()), None),
        };
        if let Err(e) = lock.lock() {
            return (Err(e.to_string()), None);
        }
        let out = match self.load_locked() {
            Err(e) => (Err(e), None),
            Ok((state, loaded)) => {
                let r = f(state, &loaded).and_then(|(new, out)| {
                    if let Some(n) = new {
                        self.save_locked(&n)?;
                        self.notify();
                    }
                    Ok(out)
                });
                (r, Some(loaded))
            }
        };
        let _ = lock.unlock();
        out
    }

    /// Tells other instances a commit happened (they watch [`Store::signature`]).
    fn notify(&self) {
        let stamp = format!(
            "{}.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let _ = write_private(&self.dir.join("ring.notify"), stamp.as_bytes());
    }

    /// A cheap fingerprint of the state on disk: `ring.json`'s mtime and size and the last
    /// commit notification. It changes with every commit, from any instance.
    pub fn signature(&self) -> (Option<std::time::SystemTime>, u64, Vec<u8>) {
        let m = fs::symlink_metadata(self.file()).ok();
        let note = fs::read(self.dir.join("ring.notify")).unwrap_or_default();
        (
            m.as_ref().and_then(|m| m.modified().ok()),
            m.map(|m| m.len()).unwrap_or(0),
            note,
        )
    }

    /// The state on disk, read under the locks.
    pub fn load(&self) -> Result<(Option<RingState>, Loaded), String> {
        self.transact(|s, _| Ok((None, s)))
    }

    fn load_locked(&self) -> Result<(Option<RingState>, Loaded), String> {
        let p = self.file();
        let bytes = match read_private(&p) {
            Ok(Some(b)) => b,
            // No Ring, unless one was set aside and not yet started over.
            Ok(None) => {
                return Ok((
                    None,
                    Loaded {
                        moved_aside: self.newest_bad(),
                    },
                ))
            }
            // A symlink or another user's file: refuse, leave it where it is.
            Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
        };
        match parse(&bytes) {
            Ok(s) => Ok((Some(s), Loaded::default())),
            Err(why) => {
                let aside = self.dir.join(format!(
                    "ring.json.bad-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                ));
                fs::rename(&p, &aside).map_err(|e| {
                    format!(
                        "{} is unreadable ({why}) and cannot be moved: {e}",
                        p.display()
                    )
                })?;
                eprintln!(
                    "xshell: {} was unreadable ({why}); moved to {}",
                    p.display(),
                    aside.display()
                );
                Ok((
                    None,
                    Loaded {
                        moved_aside: Some(aside),
                    },
                ))
            }
        }
    }

    /// The newest `ring.json.bad-*` in the directory.
    fn newest_bad(&self) -> Option<PathBuf> {
        let mut bad: Vec<PathBuf> = fs::read_dir(&self.dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("ring.json.bad-"))
            })
            .collect();
        bad.sort();
        bad.pop()
    }

    fn save_locked(&self, s: &RingState) -> Result<(), String> {
        let (sign, noise) = s.keys.seeds();
        let f = File {
            v: 1,
            sign_seed: SecretSeed::new(&sign),
            noise_seed: SecretSeed::new(&noise),
            rosters: s
                .chain
                .versions()
                .iter()
                .map(|r| r.token().to_string())
                .collect(),
            local_member: s.local_member,
            pending_move: s.pending_move.as_ref().map(|j| PendingFile {
                source: j.source.clone(),
                from: j.from,
                target: j.target,
            }),
            host_members: s.host_members.clone(),
        };
        let json = encode(&f)?;
        write_private(&self.file(), &json).map_err(|e| e.to_string())
    }
}

/// An upper bound of `f`'s JSON: every free-form string counted as if each byte were
/// escaped (`\u00XX`, 6 bytes); the tokens are base64url and dots, never escaped.
fn capacity(f: &File) -> usize {
    let esc = |s: &str| 6 * s.len() + 2;
    // The keys, the seeds, `localMember`, the numbers and the punctuation.
    1024 + f.rosters.iter().map(|t| t.len() + 3).sum::<usize>()
        + f.pending_move.as_ref().map_or(0, |p| esc(&p.source))
        + f.host_members
            .iter()
            .map(|(id, m)| esc(id) + esc(&m.target) + 96)
            .sum::<usize>()
}

/// Serialized into a buffer sized up front (no reallocation leaves a copy of the seeds
/// behind) and wiped on drop.
fn encode(f: &File) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut json = Zeroizing::new(Vec::with_capacity(capacity(f)));
    serde_json::to_writer(&mut *json, f).map_err(|e| e.to_string())?;
    Ok(json)
}

fn parse(bytes: &[u8]) -> Result<RingState, String> {
    let f: File = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if f.v != 1 {
        return Err(format!("unknown format {}", f.v));
    }
    let keys = DeviceKeys::from_seeds(&f.sign_seed.0, &f.noise_seed.0);
    let chain = RosterChain::from_tokens(&f.rosters).map_err(|e| e.to_string())?;
    Ok(RingState {
        keys: Arc::new(keys),
        chain,
        local_member: f.local_member,
        pending_move: f.pending_move.as_ref().map(|p| MoveJob {
            source: p.source.clone(),
            from: p.from,
            target: p.target,
        }),
        host_members: f.host_members.clone(),
    })
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Creates `dir` (and parents) private: on Unix 0700, owned by this user, not a symlink.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory (a symlink?); refusing it",
                dir.display()
            )));
        }
        if meta.uid() != uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is owned by uid {}, not by us",
                    dir.display(),
                    meta.uid()
                ),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // TODO(#24): a user-only DACL (the SDDL helpers #24 brings); for now only the
        // symlink check.
        fs::create_dir_all(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory (a symlink?); refusing it",
                dir.display()
            )));
        }
        Ok(())
    }
}

pub(crate) fn open_lock(p: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    o.open(p)
}

/// `path` read if it is a regular file of ours (not a symlink), a broader mode tightened.
/// `None`: it does not exist.
pub fn read_private(path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        Ok(m) if !m.is_file() => {
            return Err(io::Error::other(format!(
                "{} is not a regular file (a symlink?); refusing it",
                path.display()
            )))
        }
        Ok(_) => {}
    }
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = f.metadata()?;
        if meta.uid() != uid() {
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
    }
    // Sized up front, so no reallocation leaves a copy of a secret behind.
    let len = f.metadata()?.len() as usize;
    let mut buf = Zeroizing::new(Vec::with_capacity(len + 1));
    (&f).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

/// Writes `bytes` to `path` atomically through an exclusive (`O_CREAT|O_EXCL`, no symlink
/// following) 0600 temp file with a unique name.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}.{}.{nanos}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(name);
    let r = (|| {
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_protocol::ring::SignedRoster;

    fn state() -> RingState {
        let keys = Arc::new(DeviceKeys::generate().unwrap());
        let g =
            SignedRoster::genesis(&*keys, keys.noise_key(), "desk", "wss://r.example", 1).unwrap();
        RingState {
            chain: RosterChain::from_chain(vec![g]).unwrap(),
            keys,
            local_member: None,
            pending_move: Some(MoveJob {
                source: "wss://old.example".into(),
                from: 1,
                target: 2,
            }),
            host_members: BTreeMap::new(),
        }
    }

    fn key() -> SignKey {
        DeviceKeys::generate().unwrap().sign_key()
    }

    #[test]
    fn old_file_without_host_members_parses() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        // #8's shape, exactly: no `hostMembers` key.
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("ring.json")).unwrap()).unwrap();
        let keys: Vec<_> = raw.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "localMember",
                "noiseSeed",
                "pendingMove",
                "rosters",
                "signSeed",
                "v"
            ]
        );
        let got = s.load().unwrap().0.unwrap();
        assert!(got.host_members.is_empty());
    }

    #[test]
    fn empty_host_members_not_written() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        let text = fs::read_to_string(dir.join("ring.json")).unwrap();
        assert!(!text.contains("hostMembers"), "{text}");
    }

    #[test]
    fn host_members_round_trip() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        let mut st = state();
        let (a, b) = (key(), key());
        st.host_members.insert(
            "h_aaaaaaaa".into(),
            HostMember {
                sign_key: a,
                target: "dev|".into(),
            },
        );
        st.host_members.insert(
            "h_bbbbbbbb".into(),
            HostMember {
                sign_key: b,
                target: "box|~/xd".into(),
            },
        );
        s.transact(|_, _| Ok((Some(st.clone()), ()))).unwrap();
        let text = fs::read_to_string(dir.join("ring.json")).unwrap();
        assert!(
            text.contains("\"hostMembers\":{\"h_aaaaaaaa\":{\"signKey\""),
            "{text}"
        );
        let got = s.load().unwrap().0.unwrap();
        assert_eq!(got.host_members, st.host_members);
    }

    /// The buffer holding the seeds is never reallocated (no copy left behind), whatever
    /// the Hosts' ids and targets need escaped, at the Roster's limits.
    #[test]
    fn serialization_never_reallocates() {
        use xshell_protocol::ring::roster::MAX_MEMBERS;
        use xshell_protocol::ring::{Member, Role};
        let mut st = state();
        let keys = st.keys.clone();
        let mut next = st.chain.head().clone();
        for i in 0..MAX_MEMBERS - 1 {
            let k = DeviceKeys::generate().unwrap();
            let long = format!("{i}-{}", "\u{1}\"".repeat(20));
            let name = xshell_protocol::ring::member_name(&long, "x");
            next = next
                .next(&*keys, 1, |d| {
                    d.add(Member::new(
                        &name,
                        Role::Daemon,
                        k.sign_key(),
                        k.noise_key(),
                        1,
                    ))
                })
                .unwrap();
            st.chain.accept(std::slice::from_ref(&next)).unwrap();
            st.host_members.insert(
                format!("h_{i:08}\u{1}\"\\"),
                HostMember {
                    sign_key: k.sign_key(),
                    target: "\u{2}\"".repeat(40),
                },
            );
        }
        st.pending_move.as_mut().unwrap().source = "\u{3}".repeat(200);
        let (sign, noise) = st.keys.seeds();
        let f = File {
            v: 1,
            sign_seed: SecretSeed::new(&sign),
            noise_seed: SecretSeed::new(&noise),
            rosters: st
                .chain
                .versions()
                .iter()
                .map(|r| r.token().to_string())
                .collect(),
            local_member: Some(key()),
            pending_move: st.pending_move.as_ref().map(|j| PendingFile {
                source: j.source.clone(),
                from: j.from,
                target: j.target,
            }),
            host_members: st.host_members.clone(),
        };
        let cap = capacity(&f);
        let json = encode(&f).unwrap();
        assert_eq!(json.capacity(), cap, "reallocated");
        assert!(json.len() <= cap);
    }

    #[test]
    fn round_trip() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::new(t.path().join("ring"));
        assert!(s.load().unwrap().0.is_none());
        let st = state();
        s.transact(|_, _| Ok((Some(st.clone()), ()))).unwrap();
        let (got, l) = s.load().unwrap();
        let got = got.unwrap();
        assert_eq!(got.pending_move, st.pending_move);
        assert_eq!(got.keys.sign_key(), st.keys.sign_key());
        assert_eq!(got.keys.noise_key(), st.keys.noise_key());
        assert_eq!(got.chain, st.chain);
        assert_eq!(l, Loaded::default());
    }

    #[cfg(unix)]
    #[test]
    fn private_modes_and_refusals() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let mode = |p: &Path| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(p).unwrap().mode() & 0o777
        };
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("ring.json")), 0o600);
        // Broader modes found later are tightened.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dir.join("ring.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(s.load().unwrap().0.is_some());
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("ring.json")), 0o600);
        // No temp file left.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");

        // A ring.json that is a symlink is refused and left alone.
        let real = t.path().join("elsewhere.json");
        fs::rename(dir.join("ring.json"), &real).unwrap();
        symlink(&real, dir.join("ring.json")).unwrap();
        assert!(s.load().is_err());
        assert!(fs::symlink_metadata(dir.join("ring.json"))
            .unwrap()
            .file_type()
            .is_symlink());

        // A ring dir that is a symlink is refused.
        let other = t.path().join("other");
        fs::create_dir(&other).unwrap();
        let linked = t.path().join("linked");
        symlink(&other, &linked).unwrap();
        let s2 = Store::new(linked);
        assert!(s2.transact(|_, _| Ok((Some(state()), ()))).is_err());
        assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
    }

    #[test]
    fn interrupted_write_is_moved_aside_not_replaced() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        let full = fs::read(dir.join("ring.json")).unwrap();
        // A torn write: half the file, plus a stray temp file from the crash.
        fs::write(dir.join("ring.json"), &full[..full.len() / 2]).unwrap();
        fs::write(dir.join("ring.json.123.0.0.tmp"), b"junk").unwrap();
        let (got, l) = s.load().unwrap();
        assert!(got.is_none());
        let aside = l.moved_aside.expect("moved aside");
        assert_eq!(fs::read(&aside).unwrap(), &full[..full.len() / 2]);
        assert!(!dir.join("ring.json").exists());
        // Every later transaction keeps reporting it, until a new Ring exists.
        assert_eq!(s.load().unwrap().1.moved_aside, Some(aside));
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        assert_eq!(s.load().unwrap().1.moved_aside, None);
    }
}

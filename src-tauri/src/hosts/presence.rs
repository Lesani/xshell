//! Which Desktop processes run on this machine, for the quit prompt (ADR-0017, A6). Each
//! Desktop holds an exclusive `flock` on its own `desktop-<pid>-<nanos>.lock` for its life. A
//! closing Desktop takes the election lock `close.lock` exclusively and then counts the others,
//! so two Desktops closing at once agree on exactly one "last" one: the first leaves while it
//! holds the election lock, the second then finds itself alone.
//!
//! Everything is in a machine-local runtime directory (`$XDG_RUNTIME_DIR/xshell`, else
//! `/tmp/xshell-<uid>`, 0700), never under the home (it may be shared by several machines).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ELECTION: &str = "close.lock";
const PREFIX: &str = "desktop-";

/// `$XDG_RUNTIME_DIR/xshell` when it is absolute, else `/tmp/xshell-<uid>`.
pub fn runtime_dir(xdg_runtime_dir: Option<&Path>, uid: u32) -> PathBuf {
    match xdg_runtime_dir.filter(|p| p.is_absolute()) {
        Some(x) => x.join("xshell"),
        None => PathBuf::from(format!("/tmp/xshell-{uid}")),
    }
}

/// This user's runtime directory for this process.
#[cfg(unix)]
pub fn default_dir() -> PathBuf {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    runtime_dir(xdg.as_deref(), unsafe { libc::getuid() })
}

/// Create `dir` 0700; an existing one must be ours and is tightened.
#[cfg(unix)]
fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let m = fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory of ours", dir.display()),
        ));
    }
    if m.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

fn open_lock(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Whether a Desktop of a build before "Ring just works" runs on this machine: such a build
/// has an agent-hook socket `desktop-<pid>.sock` (in one of `socket_dirs`) for a live `xshell`
/// process, but registers no presence (`desktop-<pid>-*.lock` in `presence_dir`). A newer
/// Desktop registers its presence before it creates that socket. `me` is never counted.
pub fn older_desktop_running(socket_dirs: &[PathBuf], presence_dir: &Path, me: u32) -> bool {
    let registered = |pid: u32| {
        fs::read_dir(presence_dir).is_ok_and(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{PREFIX}{pid}-"))
            })
        })
    };
    socket_dirs.iter().any(|dir| {
        let Ok(rd) = fs::read_dir(dir) else {
            return false;
        };
        rd.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(pid) = name
                .strip_prefix(PREFIX)
                .and_then(|r| r.strip_suffix(".sock"))
                .and_then(|p| p.parse::<u32>().ok())
            else {
                return false;
            };
            pid != me && desktop_alive(pid) && !registered(pid)
        })
    })
}

/// Whether `pid` is a live xshell Desktop process.
fn desktop_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid <= 1 || pid > i32::MAX as u32 || unsafe { libc::kill(pid as i32, 0) } != 0 {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            let state = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let zombie = state
                .rfind(')')
                .and_then(|i| state[i + 1..].split_whitespace().next())
                == Some("Z");
            !zombie
                && fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|c| c.trim_end() == "xshell")
        }
        #[cfg(not(target_os = "linux"))]
        true
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// This Desktop's presence.
pub struct Presence {
    dir: PathBuf,
    own: Mutex<Option<(PathBuf, File)>>,
}

/// The outcome of a close election; the election lock is held until this is dropped.
pub struct Election {
    /// No other Desktop runs on this machine (or the election lock failed: ask rather than
    /// skip).
    pub last: bool,
    _lock: Option<File>,
}

impl Presence {
    /// Register this process in `dir`.
    pub fn register(dir: &Path) -> io::Result<Presence> {
        private_dir(dir)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!("{PREFIX}{}-{nanos}.lock", std::process::id()));
        let f = open_lock(&path)?;
        f.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => io::Error::other("presence file is locked"),
            fs::TryLockError::Error(e) => e,
        })?;
        Ok(Presence {
            dir: dir.to_path_buf(),
            own: Mutex::new(Some((path, f))),
        })
    }

    /// How many other Desktops are alive: a presence file whose lock is held. A file that can
    /// be locked is stale (its process is gone) and is removed.
    pub fn others_alive(&self) -> usize {
        let own = self.own.lock().unwrap().as_ref().map(|(p, _)| p.clone());
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut n = 0;
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(PREFIX) || !name.ends_with(".lock") || Some(&path) == own.as_ref()
            {
                continue;
            }
            let Ok(f) = OpenOptions::new().read(true).write(true).open(&path) else {
                continue;
            };
            match f.try_lock() {
                Err(fs::TryLockError::WouldBlock) => n += 1,
                Ok(()) => {
                    let _ = fs::remove_file(&path);
                    let _ = f.unlock();
                }
                Err(fs::TryLockError::Error(_)) => n += 1,
            }
        }
        n
    }

    /// Take the election lock (waiting at most `within` for another closing Desktop), then
    /// count the others. Any lock error counts as "last".
    pub fn elect(&self, within: Duration) -> Election {
        let lock = match open_lock(&self.dir.join(ELECTION)) {
            Ok(f) => f,
            Err(_) => {
                return Election {
                    last: true,
                    _lock: None,
                }
            }
        };
        let deadline = Instant::now() + within;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => {
                    return Election {
                        last: true,
                        _lock: None,
                    }
                }
            }
        }
        Election {
            last: self.others_alive() == 0,
            _lock: Some(lock),
        }
    }

    /// This Desktop is going: remove its presence (call while holding the election).
    pub fn leave(&self) {
        if let Some((path, f)) = self.own.lock().unwrap().take() {
            let _ = fs::remove_file(&path);
            let _ = f.unlock();
        }
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_is_machine_local() {
        assert_eq!(
            runtime_dir(Some(Path::new("/run/user/7")), 7),
            PathBuf::from("/run/user/7/xshell")
        );
        assert_eq!(runtime_dir(None, 7), PathBuf::from("/tmp/xshell-7"));
        assert_eq!(
            runtime_dir(Some(Path::new("rel")), 7),
            PathBuf::from("/tmp/xshell-7")
        );
    }

    #[test]
    fn counts_live_others_removes_stale() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("rt");
        let a = Presence::register(&dir).unwrap();
        let b = Presence::register(&dir).unwrap();
        // A stale presence: a file nobody holds.
        let stale = dir.join("desktop-1-1.lock");
        fs::write(&stale, "").unwrap();
        assert_eq!(a.others_alive(), 1);
        assert!(!stale.exists(), "the stale file was removed");
        assert_eq!(b.others_alive(), 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(b);
        assert_eq!(a.others_alive(), 0);
        assert!(a.elect(Duration::from_secs(1)).last);
    }

    #[test]
    fn drop_removes_own_file() {
        let t = tempfile::tempdir().unwrap();
        let a = Presence::register(t.path()).unwrap();
        let own = a.own.lock().unwrap().as_ref().unwrap().0.clone();
        assert!(own.exists());
        drop(a);
        assert!(!own.exists());
    }

    /// Two Desktops closing at once: exactly one is the last.
    #[test]
    fn two_closing_at_once_elect_exactly_one_last() {
        for _ in 0..20 {
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().to_path_buf();
            let a = std::sync::Arc::new(Presence::register(&dir).unwrap());
            let b = std::sync::Arc::new(Presence::register(&dir).unwrap());
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let close = |p: std::sync::Arc<Presence>, bar: std::sync::Arc<std::sync::Barrier>| {
                std::thread::spawn(move || {
                    bar.wait();
                    let e = p.elect(Duration::from_secs(5));
                    let last = e.last;
                    if !last {
                        // Not last: leaves while holding the election, as the app does.
                        p.leave();
                    }
                    drop(e);
                    last
                })
            };
            let ta = close(a.clone(), barrier.clone());
            let tb = close(b.clone(), barrier.clone());
            let lasts = [ta.join().unwrap(), tb.join().unwrap()];
            assert_eq!(lasts.iter().filter(|l| **l).count(), 1, "{lasts:?}");
        }
    }

    /// An older Desktop: a live `xshell` process with an agent socket and no presence.
    #[test]
    fn older_desktop_is_one_without_presence() {
        let t = tempfile::tempdir().unwrap();
        let socks = t.path().join("socks");
        let pres = t.path().join("presence");
        fs::create_dir_all(&socks).unwrap();
        fs::create_dir_all(&pres).unwrap();
        // A process named `xshell` (a copy of sleep).
        let exe = t.path().join("xshell");
        fs::copy("/bin/sleep", &exe).unwrap();
        let mut child = std::process::Command::new(&exe).arg("30").spawn().unwrap();
        let pid = child.id();
        let dirs = [socks.clone()];
        let me = std::process::id();
        assert!(!older_desktop_running(&dirs, &pres, me), "no socket yet");
        fs::write(socks.join(format!("desktop-{pid}.sock")), "").unwrap();
        assert!(older_desktop_running(&dirs, &pres, me));
        // Registered presence: a newer Desktop.
        fs::write(pres.join(format!("desktop-{pid}-123.lock")), "").unwrap();
        assert!(!older_desktop_running(&dirs, &pres, me));
        fs::remove_file(pres.join(format!("desktop-{pid}-123.lock"))).unwrap();
        // Our own socket never counts; a socket of a dead process neither.
        fs::write(socks.join(format!("desktop-{me}.sock")), "").unwrap();
        assert!(older_desktop_running(&dirs, &pres, me));
        let _ = child.kill();
        let _ = child.wait();
        assert!(!older_desktop_running(&dirs, &pres, me));
        // A live process that is not an xshell Desktop (pid reuse): not counted.
        if cfg!(target_os = "linux") {
            let mut other = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            fs::write(socks.join(format!("desktop-{}.sock", other.id())), "").unwrap();
            assert!(!older_desktop_running(&dirs, &pres, me));
            let _ = other.kill();
            let _ = other.wait();
        }
    }

    #[test]
    fn election_lock_error_counts_as_last() {
        let t = tempfile::tempdir().unwrap();
        let a = Presence::register(t.path()).unwrap();
        let _b = Presence::register(t.path()).unwrap();
        // The election lock cannot be opened: a directory in its place.
        fs::create_dir(t.path().join(ELECTION)).unwrap();
        assert!(a.elect(Duration::from_millis(50)).last);
    }
}

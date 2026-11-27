//! The update trial (ADR-0019; S4 amendments C1–C5). When a Daemon re-executes itself into
//! another build, the new build gets one trial: it must come up, restore every Terminal and
//! serve, or the machine goes back to the previous build with the Terminals it had.
//!
//! Files under `~/.xshell/server/` (all private, every write durable: temp file, fsync,
//! rename, fsync of the directory):
//! - `trial`: the open trial ([`Trial`], `key=value` lines so the launcher script can read
//!   `previous=`). Written before `selected`, removed only on acceptance or rollback.
//! - `trial.lock`: held (flock, exclusive) by the old Daemon from before it writes `trial`,
//!   then by its watchdog (`xshelld upgrade-watch`, which inherits the descriptor) for the
//!   whole trial. Every launch path waits while it is held ([`wait_free`]). A `trial` file
//!   nobody holds the lock for (power loss, a killed watchdog) is rolled back.
//! - `trial.terminals.json`: the Terminal records as they were before the re-exec (C1);
//!   a rollback puts them back.
//! - `trial.ready`: written by the trial build once it started, restored every record and
//!   (when it was before) reconnected to the Ring: `ready`, or `failed: <reason>`.
//! - `upgrade-failed`: the last rollback (JSON [`UpdateFailure`]), reported for 24 hours.
//!
//! Only the serve-lock holder and the watchdog change `trial` and `selected` (C4).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use xshell_protocol::msg::UpdateFailure;

pub const TRIAL: &str = "trial";
pub const TRIAL_LOCK: &str = "trial.lock";
pub const SNAPSHOT: &str = "trial.terminals.json";
pub const READY: &str = "trial.ready";
pub const FAILED: &str = "upgrade-failed";
/// How long a rollback is reported by `daemon.update-info`.
pub const FAILURE_TTL: Duration = Duration::from_secs(24 * 3600);
/// The longest a launch path waits for a trial to end (a trial ends well before: start,
/// Ring reconnect and the settle time are bounded by the watchdog).
pub const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// An open trial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trial {
    /// The install directory of the build on trial.
    pub target: String,
    /// The install directory of the build that ran before; `None` when it did not run from
    /// one.
    pub previous: Option<String>,
    /// The executable of the build that ran before.
    pub previous_exe: PathBuf,
    /// The Daemon was connected to its Ring before the re-exec: the trial build must
    /// reconnect (C2).
    pub ring: bool,
    /// ms since the epoch.
    pub at_ms: u64,
    /// The Daemon's process (kept across the re-exec).
    pub pid: u32,
}

impl Trial {
    fn encode(&self) -> String {
        format!(
            "target={}\nprevious={}\nprevious_exe={}\nring={}\nat={}\npid={}\n",
            self.target,
            self.previous.as_deref().unwrap_or(""),
            self.previous_exe.display(),
            u8::from(self.ring),
            self.at_ms,
            self.pid
        )
    }

    fn decode(text: &str) -> Option<Trial> {
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
                .map(str::to_string)
        };
        let target = get("target").filter(|t| xshell_core::server::valid_dir_name(t))?;
        let previous = match get("previous")? {
            p if p.is_empty() => None,
            p if xshell_core::server::valid_dir_name(&p) => Some(p),
            _ => return None,
        };
        let previous_exe = PathBuf::from(get("previous_exe")?);
        if !previous_exe.is_absolute() {
            return None;
        }
        Some(Trial {
            target,
            previous,
            previous_exe,
            ring: get("ring")? == "1",
            at_ms: get("at")?.parse().ok()?,
            pid: get("pid")?.parse().ok()?,
        })
    }
}

/// `home/.xshell/server`.
pub fn root(home: &Path) -> PathBuf {
    xshell_core::server::server_root(home)
}

/// The Daemon's Terminal records (`xshelld::paths::Paths::state`).
pub fn state_path(home: &Path) -> PathBuf {
    home.join(".xshell").join("daemon").join("terminals.json")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// fsync a directory, so a rename in it survives power loss.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Write `bytes` to `path` durably: a new private temp file in the same directory, synced,
/// renamed over `path`, then the directory synced.
#[cfg(unix)]
pub fn durable_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no directory"))?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x"),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let r = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        sync_dir(dir)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Read a private regular file of ours (not a symlink, not group/world-writable), at most
/// 64 KiB. `Ok(None)` when it does not exist.
#[cfg(unix)]
fn read_private(path: &Path) -> io::Result<Option<String>> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let m = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !m.file_type().is_file() || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!(
            "{} is not a private file",
            path.display()
        )));
    }
    let mut s = String::new();
    fs::File::open(path)?
        .take(64 * 1024)
        .read_to_string(&mut s)?;
    Ok(Some(s))
}

/// The open trial, if any. A file that does not parse is an error (it is never trusted).
#[cfg(unix)]
pub fn read(home: &Path) -> io::Result<Option<Trial>> {
    match read_private(&root(home).join(TRIAL))? {
        None => Ok(None),
        Some(t) => Trial::decode(&t)
            .map(Some)
            .ok_or_else(|| io::Error::other("malformed update trial record")),
    }
}

/// Whether an update trial of this machine is open: its lock is held (a watchdog runs) or
/// its record is there. A record that cannot be read counts as open.
#[cfg(unix)]
pub fn open(home: &Path) -> bool {
    held(home) || !matches!(read(home), Ok(None))
}

#[cfg(unix)]
pub fn write(home: &Path, t: &Trial) -> io::Result<()> {
    durable_write(&root(home).join(TRIAL), t.encode().as_bytes())
}

/// `trial.ready`: `Some(Ok(()))` ready, `Some(Err(reason))` failed, `None` not yet.
#[cfg(unix)]
pub fn ready(home: &Path) -> Option<Result<(), String>> {
    let text = read_private(&root(home).join(READY)).ok()??;
    let text = text.trim();
    if text == "stopped" {
        return None;
    }
    Some(match text.strip_prefix("failed: ") {
        Some(reason) => Err(reason.to_string()),
        None if text == "ready" => Ok(()),
        None => Err(format!("unexpected trial report {text:?}")),
    })
}

/// Written by the build on trial (the serve-lock holder).
#[cfg(unix)]
pub fn report(home: &Path, outcome: Result<(), &str>) -> io::Result<()> {
    let text = match outcome {
        Ok(()) => "ready\n".to_string(),
        Err(reason) => format!("failed: {}\n", reason.replace('\n', " ")),
    };
    durable_write(&root(home).join(READY), text.as_bytes())
}

/// Written by the build on trial when it was asked to stop (SIGTERM, a service stop)
/// before the trial ended: the update is rolled back, but nothing is started again.
#[cfg(unix)]
pub fn report_stopped(home: &Path) -> io::Result<()> {
    durable_write(&root(home).join(READY), b"stopped\n")
}

/// Whether the build on trial reported that it was asked to stop.
#[cfg(unix)]
pub fn stopped(home: &Path) -> bool {
    read_private(&root(home).join(READY))
        .ok()
        .flatten()
        .is_some_and(|t| t.trim() == "stopped")
}

/// Remove `trial`, `trial.ready` and the snapshot (acceptance or after a rollback).
pub fn remove(home: &Path) -> io::Result<()> {
    let r = root(home);
    // The trial record first: without it the rest means nothing.
    for name in [TRIAL, READY, SNAPSHOT] {
        match fs::remove_file(r.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    sync_dir(&r)
}

#[cfg(unix)]
fn open_lock(home: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = root(home);
    fs::create_dir_all(&dir)?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(dir.join(TRIAL_LOCK))
}

/// Take `trial.lock` exclusively: `Ok(None)` when another process holds it.
#[cfg(unix)]
pub fn try_lock(home: &Path) -> io::Result<Option<fs::File>> {
    let f = open_lock(home)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Whether some process holds `trial.lock` (a watchdog: a trial is running).
#[cfg(unix)]
pub fn held(home: &Path) -> bool {
    crate::local::lock_held(&root(home).join(TRIAL_LOCK))
}

/// Wait (at most `max`) while a trial is running. Returns whether it is free now.
#[cfg(unix)]
pub fn wait_free(home: &Path, max: Duration) -> bool {
    let until = Instant::now() + max;
    while held(home) {
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Test seam: per home, how many [`select`] calls fail their directory sync after the rename
/// (the selection changed but is not confirmed durable).
#[doc(hidden)]
pub static FAIL_SELECT_SYNC: std::sync::Mutex<Vec<(PathBuf, u32)>> =
    std::sync::Mutex::new(Vec::new());

/// Record `dir` as the build to start next, durably. An error after the rename means the
/// selection may have changed without being durable: callers check `selected` again.
#[cfg(unix)]
pub fn select(home: &Path, dir: &str) -> io::Result<()> {
    xshell_core::server::write_selected(home, dir)?;
    {
        let mut f = FAIL_SELECT_SYNC.lock().unwrap();
        if let Some(e) = f.iter_mut().find(|(h, n)| h == home && *n > 0) {
            e.1 -= 1;
            return Err(io::Error::other("injected directory sync failure"));
        }
    }
    sync_dir(&root(home))
}

/// Select `dir` only on a machine where nothing else decides the selection (S4 P1-2): no
/// trial, no Daemon of this home alive, holding `trial.lock`. Returns whether it selected.
#[cfg(unix)]
pub fn select_if_fresh(home: &Path, dir: &str) -> io::Result<bool> {
    let Some(_lock) = try_lock(home)? else {
        return Ok(false);
    };
    if read(home)?.is_some() || daemon_alive(home) {
        return Ok(false);
    }
    select(home, dir)?;
    Ok(true)
}

/// The rollback still reported (younger than [`FAILURE_TTL`]).
#[cfg(unix)]
pub fn failure(home: &Path, now_ms: u64) -> Option<UpdateFailure> {
    let text = read_private(&root(home).join(FAILED)).ok()??;
    let f: UpdateFailure = serde_json::from_str(&text).ok()?;
    (now_ms.saturating_sub(f.at_ms) < FAILURE_TTL.as_millis() as u64).then_some(f)
}

#[cfg(unix)]
pub fn clear_failure(home: &Path) -> io::Result<()> {
    match fs::remove_file(root(home).join(FAILED)) {
        Ok(()) => sync_dir(&root(home)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copy the Terminal records aside before a re-exec (C1). Nothing when there are none.
#[cfg(unix)]
pub fn snapshot(home: &Path) -> io::Result<()> {
    match fs::read(state_path(home)) {
        Ok(bytes) => durable_write(&root(home).join(SNAPSHOT), &bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Go back to the build before `t` (C1, C3): the Terminal records as they were before the
/// re-exec, `selected` naming the previous build, the failure recorded, the trial files
/// removed. Without a previous install directory, `selected` is removed and the failed
/// build's directory is moved aside, so the newest-installed fallback cannot pick it. The
/// caller holds `trial.lock` and has made sure no Daemon of this home runs.
#[cfg(unix)]
pub fn rollback(home: &Path, t: &Trial, reason: &str) -> io::Result<()> {
    let r = root(home);
    match fs::read(r.join(SNAPSHOT)) {
        Ok(bytes) => {
            let state = state_path(home);
            if let Some(dir) = state.parent() {
                fs::create_dir_all(dir)?;
            }
            durable_write(&state, &bytes)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let previous = t
        .previous
        .as_deref()
        .filter(|p| xshell_core::server::valid_install(&r, p).is_some());
    match previous {
        Some(p) => select(home, p)?,
        None => {
            match fs::remove_file(r.join(xshell_core::server::SELECTED)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let aside = r.join(format!(".failed-{}-{}", t.target, now_ms()));
            let _ = fs::rename(r.join(&t.target), aside);
            sync_dir(&r)?;
        }
    }
    let f = UpdateFailure {
        target: t.target.clone(),
        previous: t.previous.clone(),
        reason: reason.to_string(),
        at_ms: now_ms(),
    };
    durable_write(
        &r.join(FAILED),
        serde_json::to_string(&f).expect("serializes").as_bytes(),
    )?;
    remove(home)
}

/// Whether a Daemon of this home is alive or starting (its owner or start lock is held).
#[cfg(unix)]
pub fn daemon_alive(home: &Path) -> bool {
    crate::local::lock_held(&crate::local::local_owner_path(home))
        || crate::local::lock_held(&crate::local::local_start_path(home))
}

/// What every launch path does before it picks a build (C4): wait while a trial runs and
/// no Daemon is up (a launch path never starts a second Daemon next to a live one, so
/// waiting longer would only delay a reconnect); a trial left behind by nobody (no
/// `trial.lock` holder, no Daemon running) is rolled back. A trial whose build still runs
/// after its watchdog died is left alone: the next start rolls it back. Returns the trial
/// still open, if any.
#[cfg(unix)]
pub fn settle(home: &Path) -> io::Result<Option<Trial>> {
    let until = Instant::now() + MAX_WAIT;
    loop {
        while held(home) {
            if daemon_alive(home) {
                return read(home);
            }
            if Instant::now() >= until {
                return Err(io::Error::other(
                    "an update of this machine is still running",
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if read(home)?.is_none() {
            return Ok(None);
        }
        let Some(lock) = try_lock(home)? else {
            // A watchdog (or another launcher) took it meanwhile: wait for it.
            continue;
        };
        let result = match read(home)? {
            None => Ok(None),
            Some(t) if daemon_alive(home) => Ok(Some(t)),
            Some(t) => rollback(home, &t, "the update was interrupted").map(|()| None),
        };
        drop(lock);
        return result;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        let h = tempfile::tempdir().unwrap();
        install(h.path(), "1.5.0+aaaaaaaaaaaa");
        install(h.path(), "1.6.0+bbbbbbbbbbbb");
        h
    }

    fn install(home: &Path, dir: &str) {
        crate::install::install_local_bytes(b"#!/bin/sh\n", home, dir).unwrap();
    }

    fn trial(home: &Path, previous: Option<&str>) -> Trial {
        Trial {
            target: "1.6.0+bbbbbbbbbbbb".into(),
            previous: previous.map(String::from),
            previous_exe: root(home).join("1.5.0+aaaaaaaaaaaa/xshelld"),
            ring: true,
            at_ms: 5,
            pid: 42,
        }
    }

    fn selected(home: &Path) -> Option<String> {
        fs::read_to_string(root(home).join("selected"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    #[test]
    fn trial_record_round_trips_and_rejects_bad_names() {
        let h = home();
        let t = trial(h.path(), Some("1.5.0+aaaaaaaaaaaa"));
        write(h.path(), &t).unwrap();
        assert_eq!(read(h.path()).unwrap(), Some(t.clone()));
        let text = fs::read_to_string(root(h.path()).join(TRIAL)).unwrap();
        assert!(text.contains("\nprevious=1.5.0+aaaaaaaaaaaa\n"), "{text}");
        durable_write(
            &root(h.path()).join(TRIAL),
            text.replace("1.6.0+bbbbbbbbbbbb", "../x").as_bytes(),
        )
        .unwrap();
        assert!(read(h.path()).is_err());
    }

    #[test]
    fn trial_commit_removes_marker() {
        let h = home();
        write(h.path(), &trial(h.path(), Some("1.5.0+aaaaaaaaaaaa"))).unwrap();
        report(h.path(), Ok(())).unwrap();
        assert_eq!(ready(h.path()), Some(Ok(())));
        remove(h.path()).unwrap();
        assert_eq!(read(h.path()).unwrap(), None);
        assert_eq!(ready(h.path()), None);
    }

    #[test]
    fn uncommitted_trial_rolls_back_on_next_start() {
        let h = home();
        fs::create_dir_all(state_path(h.path()).parent().unwrap()).unwrap();
        fs::write(state_path(h.path()), b"[\"before\"]").unwrap();
        snapshot(h.path()).unwrap();
        write(h.path(), &trial(h.path(), Some("1.5.0+aaaaaaaaaaaa"))).unwrap();
        select(h.path(), "1.6.0+bbbbbbbbbbbb").unwrap();
        // The trial build restored fewer Terminals and saved that.
        fs::write(state_path(h.path()), b"[]").unwrap();
        // Nobody holds trial.lock and no Daemon runs: power loss.
        assert_eq!(settle(h.path()).unwrap(), None);
        assert_eq!(selected(h.path()).as_deref(), Some("1.5.0+aaaaaaaaaaaa"));
        assert_eq!(fs::read(state_path(h.path())).unwrap(), b"[\"before\"]");
        assert_eq!(read(h.path()).unwrap(), None);
        let f = failure(h.path(), now_ms()).unwrap();
        assert_eq!(f.target, "1.6.0+bbbbbbbbbbbb");
        assert_eq!(f.previous.as_deref(), Some("1.5.0+aaaaaaaaaaaa"));
    }

    #[test]
    fn rollback_without_previous_dir_clears_selected() {
        let h = home();
        write(h.path(), &trial(h.path(), None)).unwrap();
        select(h.path(), "1.6.0+bbbbbbbbbbbb").unwrap();
        rollback(h.path(), &trial(h.path(), None), "x").unwrap();
        assert_eq!(selected(h.path()), None);
        // The failed build can no longer be picked as the newest install.
        assert!(
            xshell_core::server::valid_install(&root(h.path()), "1.6.0+bbbbbbbbbbbb").is_none()
        );
        assert!(
            xshell_core::server::valid_install(&root(h.path()), "1.5.0+aaaaaaaaaaaa").is_some()
        );
    }

    #[test]
    fn failure_record_expires_after_24h() {
        let h = home();
        rollback(h.path(), &trial(h.path(), Some("1.5.0+aaaaaaaaaaaa")), "x").unwrap();
        let at = failure(h.path(), now_ms()).unwrap().at_ms;
        assert!(failure(h.path(), at + FAILURE_TTL.as_millis() as u64 - 1).is_some());
        assert!(failure(h.path(), at + FAILURE_TTL.as_millis() as u64).is_none());
        clear_failure(h.path()).unwrap();
        assert!(failure(h.path(), at).is_none());
    }

    #[test]
    fn owned_trial_is_waited_for_not_rolled_back() {
        let h = home();
        write(h.path(), &trial(h.path(), Some("1.5.0+aaaaaaaaaaaa"))).unwrap();
        select(h.path(), "1.6.0+bbbbbbbbbbbb").unwrap();
        let lock = try_lock(h.path()).unwrap().unwrap();
        assert!(held(h.path()));
        assert!(!wait_free(h.path(), Duration::from_millis(300)));
        let home = h.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            let t0 = Instant::now();
            let r = settle(&home).unwrap();
            (t0.elapsed(), r)
        });
        std::thread::sleep(Duration::from_millis(400));
        // The watchdog accepts the trial, then lets go.
        remove(h.path()).unwrap();
        drop(lock);
        let (waited, r) = waiter.join().unwrap();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        assert_eq!(r, None);
        assert_eq!(selected(h.path()).as_deref(), Some("1.6.0+bbbbbbbbbbbb"));
        assert!(failure(h.path(), now_ms()).is_none());
    }
}

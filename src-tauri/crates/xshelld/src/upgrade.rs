//! Updating this Daemon in place (ADR-0019; S4 C1–C5). Linux only: the watchdog follows the
//! Daemon through a pidfd.
//!
//! `daemon.upgrade-selected` (and a plain `daemon.upgrade` whose `selected` names another
//! build) opens a trial while the old Daemon still holds its locks ([`begin`]): it takes
//! `trial.lock`, copies the Terminal records aside, writes the `trial` record, then
//! `selected`, all durably and in that order. The Daemon then ends its Terminals as for any
//! upgrade, and [`reexec`] starts the watchdog (`xshelld upgrade-watch`, from the old build;
//! it inherits `trial.lock`) and executes the new build in this same process: same PID, so a
//! systemd `Type=simple` unit and the Desktop's dialer just see the Daemon come back.
//!
//! The new build, at start ([`on_start`]), knows it is on trial: once it restored every
//! Terminal and (when the old one was) reconnected to its Ring, it writes `trial.ready`. The
//! watchdog then says hello over the Daemon's own socket and waits for it to stay up; only
//! then is the trial accepted (its files removed). Anything else (exit, no report in time,
//! a failed report) and the watchdog stops the process through the pidfd (SIGTERM, then
//! SIGKILL), makes sure the serve lock is free, puts back the records and `selected`, starts
//! the previous build and records the failure for `daemon.update-info`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use xshell_hostlink::trial::{self, Trial};

/// This build's install directory name.
pub fn own_dir() -> String {
    xshell_protocol::build::install_dir_name(
        env!("CARGO_PKG_VERSION"),
        crate::build_id::BUILD.as_ref(),
    )
}

/// Whether this process updates itself in place (Linux). Test hook (debug builds):
/// `XSHELLD_TEST_NO_REEXEC=1` makes it exit on an upgrade instead, as a Daemon from before
/// in-place updates did (tests that play such a Daemon with this build).
pub fn in_place() -> bool {
    cfg!(target_os = "linux")
        && !(cfg!(debug_assertions)
            && std::env::var_os("XSHELLD_TEST_NO_REEXEC").is_some_and(|v| v == "1"))
}

/// Durations of a trial, shortened by tests (debug builds only, environment variables in ms:
/// `XSHELLD_TEST_TRIAL_READY_MS`, `_RING_MS`, `_SETTLE_MS`, `_TERM_MS`).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// From the re-exec until the new build reports.
    pub ready: std::time::Duration,
    /// From the new build's start until it is connected to its Ring again (C2).
    pub ring: std::time::Duration,
    /// How long the new build must then stay up (C2).
    pub settle: std::time::Duration,
    /// From SIGTERM until SIGKILL (C3).
    pub term: std::time::Duration,
}

impl Limits {
    pub fn get() -> Limits {
        use std::time::Duration;
        let ms = |k: &str, d: u64| {
            let v = if cfg!(debug_assertions) {
                std::env::var(k).ok().and_then(|v| v.parse().ok())
            } else {
                None
            };
            Duration::from_millis(v.unwrap_or(d))
        };
        Limits {
            ready: ms("XSHELLD_TEST_TRIAL_READY_MS", 180_000),
            ring: ms("XSHELLD_TEST_TRIAL_RING_MS", 120_000),
            settle: ms("XSHELLD_TEST_TRIAL_SETTLE_MS", 60_000),
            term: ms("XSHELLD_TEST_TRIAL_TERM_MS", 10_000),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs;
    use std::io::{self, BufRead, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// What [`begin`] left for [`reexec`], per home (tests run several Daemons in one
    /// process; only a real `serve` process re-executes).
    struct Pending {
        home: PathBuf,
        lock: fs::File,
        trial: Trial,
    }

    static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
    /// Homes whose Daemon was asked to restart in place (a plain `daemon.upgrade` that
    /// selects no other build).
    static RESTART: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// A plain `daemon.upgrade` without another build: the Daemon restarts in place.
    pub fn restart(home: &Path) {
        let mut r = RESTART.lock().unwrap();
        if !r.iter().any(|h| h == home) {
            r.push(home.to_path_buf());
        }
    }

    /// Open a trial for the installed build `target` (C4 order: staged directory complete →
    /// trial file → `selected`). The caller holds the registry lock and has just saved the
    /// Terminal records. `ring`: this Daemon is connected to its Ring now.
    pub fn begin(home: &Path, target: &str, ring: bool) -> Result<(), String> {
        let root = trial::root(home);
        if xshell_core::server::valid_install(&root, target).is_none() {
            return Err(format!("{target} is not installed"));
        }
        let own = own_dir();
        if own == target {
            return Err("that build is already running".into());
        }
        // The previous build must be installed under its name, so a rollback can select it.
        let previous_exe = match xshell_core::server::valid_install(&root, &own) {
            Some(p) => p,
            None => {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                xshell_hostlink::install::install_local(&exe, home, &own)
                    .map_err(|e| format!("cannot keep this build for a rollback: {e}"))?
            }
        };
        let lock = trial::try_lock(home)
            .map_err(|e| e.to_string())?
            .ok_or("an update of this machine is already running")?;
        if trial::read(home).map_err(|e| e.to_string())?.is_some() {
            return Err("an earlier update of this machine is not finished".into());
        }
        let t = Trial {
            target: target.to_string(),
            previous: Some(own),
            previous_exe,
            ring,
            at_ms: trial::now_ms(),
            pid: std::process::id(),
        };
        let r = (|| {
            trial::snapshot(home)?;
            let _ = fs::remove_file(root.join(trial::READY));
            trial::write(home, &t)?;
            trial::select(home, target)
        })();
        if let Err(e) = r {
            // `selected` may already name the target although its directory sync failed
            // (P1-4): then put the previous selection back durably before the trial record
            // goes, or keep the record, so the next start rolls back.
            let changed = fs::read_to_string(root.join(xshell_core::server::SELECTED))
                .is_ok_and(|s| s.lines().next() == Some(target));
            if !changed {
                let _ = trial::remove(home);
            } else if let Err(e2) = trial::select(home, &own_dir()) {
                crate::log!(
                    "ERROR",
                    "cannot restore the selection after a failed update ({e2}); the next start rolls back"
                );
            } else {
                let _ = trial::remove(home);
            }
            return Err(format!("cannot prepare the update: {e}"));
        }
        let mut p = PENDING.lock().unwrap();
        p.retain(|x| x.home != home);
        p.push(Pending {
            home: home.to_path_buf(),
            lock,
            trial: t,
        });
        Ok(())
    }

    fn take(home: &Path) -> Option<Pending> {
        let mut p = PENDING.lock().unwrap();
        let i = p.iter().position(|x| x.home == home)?;
        Some(p.swap_remove(i))
    }

    /// `exec` `bin` with this process's arguments, guarded against the shell profile
    /// starting another Daemon. Returns only on failure.
    fn exec(bin: &Path) -> io::Error {
        let mut cmd = Command::new(bin);
        cmd.args(std::env::args_os().skip(1))
            .env(crate::service::GUARD_ENV, "1");
        cmd.exec()
    }

    /// After `serve` ended with `ExitReason::Upgrade` (Terminals ended, records saved, the
    /// socket gone, the locks released): with an open trial, start its watchdog and execute
    /// the selected build in this process. Returns the exit code when nothing was executed.
    pub fn reexec(home: &Path, paths: &crate::paths::Paths) -> i32 {
        let Some(p) = take(home) else {
            // No trial: a plain `daemon.upgrade` that selects no other build restarts this
            // build in place (P2-9).
            let asked = {
                let mut r = RESTART.lock().unwrap();
                let at = r.iter().position(|h| h == home);
                at.map(|i| r.swap_remove(i)).is_some()
            };
            if !asked {
                return 0;
            }
            crate::log!("INFO", "restarting in place (pid {})", std::process::id());
            let e = exec(Path::new("/proc/self/exe"));
            crate::log!("ERROR", "cannot restart in place: {e}");
            return 1;
        };
        let root = trial::root(home);
        let Some(target) = xshell_core::server::valid_install(&root, &p.trial.target) else {
            return give_up(home, p, "the new build is no longer installed");
        };
        if let Err(e) = spawn_watchdog(home, paths, &p) {
            crate::log!("ERROR", "cannot watch the update: {e}");
            return give_up(home, p, "the update could not be watched");
        }
        crate::log!(
            "INFO",
            "updating to {} in place (pid {})",
            p.trial.target,
            std::process::id()
        );
        // Our descriptor of trial.lock closes with the exec; the watchdog's copy keeps it.
        let e = exec(&target);
        crate::log!("ERROR", "cannot start {}: {e}", target.display());
        // The watchdog sees this process end and rolls back.
        1
    }

    /// The update cannot go ahead: roll back here (we still hold `trial.lock` and no
    /// Daemon runs), then start the previous build in this process.
    fn give_up(home: &Path, p: Pending, reason: &str) -> i32 {
        if let Err(e) = trial::rollback(home, &p.trial, reason) {
            crate::log!("ERROR", "cannot roll back the update: {e}");
        }
        drop(p.lock);
        let e = exec(&p.trial.previous_exe);
        crate::log!("ERROR", "cannot restart: {e}");
        1
    }

    fn spawn_watchdog(home: &Path, paths: &crate::paths::Paths, p: &Pending) -> io::Result<()> {
        let lock_fd: RawFd = p.lock.as_raw_fd();
        let log = crate::log::open_log(&paths.log)?;
        let mut cmd = Command::new(&p.trial.previous_exe);
        cmd.arg("upgrade-watch")
            .arg("--home")
            .arg(home)
            .arg("--pid")
            .arg(std::process::id().to_string())
            .arg("--lock-fd")
            .arg("3")
            .arg("--socket")
            .arg(&paths.socket)
            .arg("--serve-lock")
            .arg(&paths.lock)
            .arg("--")
            .args(std::env::args_os().skip(1))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .current_dir("/");
        unsafe {
            cmd.pre_exec(move || {
                // fd 3 without close-on-exec: the watchdog's copy of trial.lock.
                if lock_fd == 3 {
                    let flags = libc::fcntl(3, libc::F_GETFD);
                    if flags == -1 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                    {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(lock_fd, 3) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let out = child.stdout.take().expect("piped");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = io::BufReader::new(out).read_line(&mut line);
            let _ = tx.send(line);
        });
        let line = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
        // The first watchdog process forks and exits at once; its child watches.
        let _ = child.wait();
        if line.trim() != "ready" {
            return Err(io::Error::other(format!(
                "the watchdog did not start ({})",
                line.trim()
            )));
        }
        Ok(())
    }

    /// What `serve` does with an update trial before it starts.
    pub enum Decision {
        /// No trial for this process.
        Normal,
        /// Another process runs a trial of this home: this `serve` must not start (P1-1).
        Busy,
        /// This process is the build on trial (its watchdog runs).
        Trial(Trial),
        /// A trial nobody owns was rolled back and this build was its target: execute the
        /// previous build instead.
        Exec(PathBuf),
    }

    /// Called by `serve` before it takes the start lock (P1-1): while another process owns a
    /// trial of this home, wait until it is settled. The build on trial itself (this
    /// process, re-executed) and a trial nobody owns (rolled back by [`on_start`]) do not
    /// wait.
    pub fn wait_for_foreign_trial(home: &Path) {
        let mut said = None::<Instant>;
        loop {
            let t = match trial::read(home) {
                Ok(Some(t)) => t,
                _ => return,
            };
            if (t.pid == std::process::id() && t.target == own_dir()) || !trial::held(home) {
                return;
            }
            if said.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                crate::log!("INFO", "waiting for the update of this machine to finish");
                said = Some(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Called by `serve` holding the start lock (no other Daemon of this home runs).
    pub fn on_start(home: &Path) -> Decision {
        let t = match trial::read(home) {
            Ok(Some(t)) => t,
            Ok(None) => return Decision::Normal,
            Err(e) => {
                crate::log!("WARN", "ignoring the update trial record: {e}");
                return Decision::Normal;
            }
        };
        let mine = t.target == own_dir();
        if trial::held(home) {
            return if mine && t.pid == std::process::id() {
                Decision::Trial(t)
            } else {
                Decision::Busy
            };
        }
        // Nobody owns it (power loss, a killed watchdog): go back (C4).
        let lock = match trial::try_lock(home) {
            Ok(Some(l)) => l,
            _ => return Decision::Normal,
        };
        if let Err(e) = trial::rollback(home, &t, "the update was interrupted") {
            crate::log!("ERROR", "cannot roll back an interrupted update: {e}");
            return Decision::Normal;
        }
        drop(lock);
        crate::log!("WARN", "rolled back an interrupted update to {}", t.target);
        if !mine {
            return Decision::Normal;
        }
        match xshell_hostlink::local::start_binary(home) {
            Ok(Some(bin)) => Decision::Exec(bin),
            _ => Decision::Normal,
        }
    }

    pub fn exec_previous(bin: &Path) -> i32 {
        let e = exec(bin);
        crate::log!("ERROR", "cannot start {}: {e}", bin.display());
        1
    }

    /// The build on trial reports, on its own thread once the server started (C1, C2):
    /// every record restored, and connected to the Ring again if it was before.
    pub fn report(home: PathBuf, t: Trial, probe: crate::server::DaemonProbe) {
        let limits = Limits::get();
        let started = Instant::now();
        let outcome = (|| {
            let failed = probe.restore_failures();
            if failed > 0 {
                return Err(format!("{failed} terminals did not start again"));
            }
            // Every record the old build had is running again (C1, P1-5).
            let running = probe.terminal_ids();
            let missing = snapshot_ids(&home)?
                .into_iter()
                .filter(|id| !running.contains(id))
                .count();
            if missing > 0 {
                return Err(format!("{missing} terminals did not start again"));
            }
            if t.ring {
                while !probe.ring_connected() {
                    if started.elapsed() >= limits.ring || probe.exiting() {
                        return Err("it did not reconnect to the Ring".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            Ok(())
        })();
        let r = trial::report(&home, outcome.as_ref().map(|_| ()).map_err(String::as_str));
        match (&outcome, r) {
            (_, Err(e)) => crate::log!("ERROR", "cannot report the update trial: {e}"),
            (Ok(()), Ok(())) => crate::log!("INFO", "update trial: ready"),
            (Err(e), Ok(())) => crate::log!("WARN", "update trial failed: {e}"),
        }
        crash_after_ready_for_tests(outcome.is_ok());
    }

    /// The Terminal UUIDs in the trial's snapshot (none when there was no state file).
    fn snapshot_ids(home: &Path) -> Result<Vec<uuid::Uuid>, String> {
        let bytes = match fs::read(trial::root(home).join(trial::SNAPSHOT)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read the Terminal records: {e}")),
        };
        let v: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("cannot read the Terminal records: {e}"))?;
        Ok(v["terminals"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t["terminal"].as_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Test hook (debug builds): `XSHELLD_TEST_CORRUPT_STATE=<install dir>:unreadable|corrupt`:
    /// that build, on an update trial, finds the Terminal records unreadable or corrupt.
    pub fn corrupt_state_for_tests(home: &Path) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Ok(v) = std::env::var("XSHELLD_TEST_CORRUPT_STATE") else {
            return;
        };
        let Some((dir, how)) = v.rsplit_once(':') else {
            return;
        };
        if dir != own_dir() {
            return;
        }
        let state = trial::state_path(home);
        use std::os::unix::fs::PermissionsExt;
        match how {
            "unreadable" => {
                let _ = fs::set_permissions(&state, fs::Permissions::from_mode(0o000));
            }
            "corrupt" => {
                let _ = fs::write(&state, b"{not json");
            }
            _ => {}
        }
    }

    /// Test hook (debug builds): `XSHELLD_TEST_TRIAL_CRASH=<install dir>:<ms>`: that build
    /// aborts `<ms>` after it reported ready.
    fn crash_after_ready_for_tests(ready: bool) {
        if !cfg!(debug_assertions) || !ready {
            return;
        }
        let Ok(v) = std::env::var("XSHELLD_TEST_TRIAL_CRASH") else {
            return;
        };
        let Some((dir, ms)) = v.rsplit_once(':') else {
            return;
        };
        if dir == own_dir() {
            if let Ok(ms) = ms.parse() {
                std::thread::sleep(Duration::from_millis(ms));
                std::process::abort();
            }
        }
    }

    // ---- the watchdog ----

    /// The build on trial was asked to stop: roll back, start nothing.
    const STOPPED: &str = "it was stopped during the update";

    /// After `serve` of the build on trial ended orderly (asked to stop) while its trial was
    /// still open: tell the watchdog, so it rolls back without starting anything.
    pub fn stopped_during_trial(home: &Path) {
        if trial::read(home).ok().flatten().is_some() && trial::held(home) {
            if let Err(e) = trial::report_stopped(home) {
                crate::log!("ERROR", "cannot report the stop to the update trial: {e}");
            }
        }
    }

    struct Watch {
        home: PathBuf,
        pid: i32,
        pidfd: OwnedFd,
        socket: PathBuf,
        serve_lock: PathBuf,
        serve_args: Vec<OsString>,
    }

    fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    impl Watch {
        /// Whether the Daemon process has ended, waiting at most `wait`.
        fn ended(&self, wait: Duration) -> bool {
            let mut pfd = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = wait.as_millis().min(i32::MAX as u128) as i32;
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            n > 0
        }

        fn signal(&self, sig: i32) {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }

        /// Wait for the build on trial to report, then to answer a hello, then to stay up.
        fn judge(&self, t: &Trial, limits: Limits) -> Result<(), String> {
            let until = Instant::now() + limits.ready;
            loop {
                if self.ended(Duration::from_millis(100)) {
                    if trial::stopped(&self.home) {
                        return Err(STOPPED.into());
                    }
                    return Err(format!("xshell {} stopped while starting", t.target));
                }
                match trial::ready(&self.home) {
                    Some(Ok(())) => break,
                    Some(Err(reason)) => return Err(reason),
                    None => {}
                }
                if Instant::now() >= until {
                    return Err(format!("xshell {} did not start in time", t.target));
                }
            }
            self.hello(t)?;
            let until = Instant::now() + limits.settle;
            while Instant::now() < until {
                if self.ended(until.saturating_duration_since(Instant::now())) {
                    if trial::stopped(&self.home) {
                        return Err(STOPPED.into());
                    }
                    return Err(format!("xshell {} stopped after it started", t.target));
                }
            }
            Ok(())
        }

        /// A hello over the Daemon's own socket, answered by the build on trial.
        fn hello(&self, t: &Trial) -> Result<(), String> {
            use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
            use xshell_protocol::msg::{decode_server, encode_msg, ClientMsg, Hello, ServerMsg};
            let socket = xshell_hostlink::local::local_live_socket(&self.home, &self.socket);
            let fail = |e: String| format!("xshell {} does not answer: {e}", t.target);
            let mut s = std::os::unix::net::UnixStream::connect(&socket)
                .map_err(|e| fail(e.to_string()))?;
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
            let mut r = io::BufReader::new(s.try_clone().map_err(|e| fail(e.to_string()))?);
            let theirs = match read_frame(&mut r, MAX_FRAME_LEN) {
                Ok(Some(Frame::Json(j))) => match decode_server(&j) {
                    Ok(ServerMsg::Hello(h)) => h,
                    _ => return Err(fail("no hello".into())),
                },
                _ => return Err(fail("no hello".into())),
            };
            let dir =
                xshell_protocol::build::install_dir_name(&theirs.version, theirs.build.as_ref());
            if dir != t.target {
                return Err(format!("{dir} answers instead of {}", t.target));
            }
            let ours = ClientMsg::Hello(Hello {
                protocol: xshell_protocol::PROTOCOL,
                version: env!("CARGO_PKG_VERSION").into(),
                capabilities: Vec::new(),
                build: crate::build_id::BUILD.clone(),
            });
            let frame = encode_msg(&ours, None).map_err(|e| fail(e.to_string()))?;
            s.write_all(&frame).map_err(|e| fail(e.to_string()))?;
            match read_frame(&mut r, MAX_FRAME_LEN) {
                Ok(Some(Frame::Json(j))) => match decode_server(&j) {
                    Ok(ServerMsg::Terminals { .. }) => Ok(()),
                    Ok(other) => Err(fail(format!("unexpected answer {other:?}"))),
                    Err(e) => Err(fail(e.to_string())),
                },
                _ => Err(fail("no terminal list".into())),
            }
        }

        /// Stop the build on trial (C3): SIGTERM through the pidfd, then SIGKILL, then wait
        /// until no Daemon of this home holds its locks.
        /// Never on a timeout alone (P1-1): it returns only once the process is gone
        /// (pidfd) and both home locks are free, escalating and reporting until then.
        fn stop(&self, limits: Limits) {
            if !self.ended(Duration::ZERO) {
                self.signal(libc::SIGTERM);
                if !self.ended(limits.term) {
                    crate::log!("WARN", "update trial: the new build ignored SIGTERM");
                    let mut said = Instant::now();
                    loop {
                        self.signal(libc::SIGKILL);
                        if self.ended(Duration::from_secs(1)) {
                            break;
                        }
                        if said.elapsed() >= Duration::from_secs(30) {
                            crate::log!("ERROR", "update trial: pid {} does not end", self.pid);
                            said = Instant::now();
                        }
                    }
                }
            }
            let mut said = Instant::now();
            loop {
                let serve_free = match fs::OpenOptions::new().read(true).open(&self.serve_lock) {
                    Ok(f) => match f.try_lock() {
                        Ok(()) => {
                            let _ = f.unlock();
                            true
                        }
                        Err(_) => false,
                    },
                    Err(_) => true,
                };
                if serve_free && !trial::daemon_alive(&self.home) {
                    return;
                }
                if said.elapsed() >= Duration::from_secs(5) {
                    crate::log!(
                        "WARN",
                        "update trial: the new build ended but this home's locks are still held; waiting before rolling back"
                    );
                    said = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        /// Start the previous build's `serve`, detached, as the old Daemon was started.
        fn start_previous(&self, t: &Trial) {
            let log = crate::log::open_log(&crate::paths::resolve(&self.home, None, None).log);
            let mut cmd = Command::new(&t.previous_exe);
            cmd.args(&self.serve_args)
                .env(crate::service::GUARD_ENV, "1")
                .stdin(Stdio::null())
                .current_dir("/");
            match log.and_then(|l| Ok((l.try_clone()?, l))) {
                Ok((a, b)) => {
                    cmd.stdout(a).stderr(b);
                }
                Err(_) => {
                    cmd.stdout(Stdio::null()).stderr(Stdio::null());
                }
            }
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            match cmd.spawn() {
                Ok(_) => crate::log!(
                    "INFO",
                    "update rolled back; started {}",
                    t.previous_exe.display()
                ),
                Err(e) => crate::log!("ERROR", "cannot start the previous build: {e}"),
            }
        }
    }

    /// `xshelld upgrade-watch --home H --pid P --lock-fd N --socket S --serve-lock L -- ARGS`
    /// (hidden; started by [`reexec`]).
    pub fn watch_main(args: Vec<OsString>) -> i32 {
        let mut it = args.into_iter();
        let (mut home, mut pid, mut lock_fd, mut socket, mut serve_lock) =
            (None, None, None, None, None);
        let mut serve_args = Vec::new();
        while let Some(a) = it.next() {
            let a = a.to_string_lossy().into_owned();
            if a == "--" {
                serve_args = it.by_ref().collect();
                break;
            }
            let v = it.next();
            match (a.as_str(), v) {
                ("--home", Some(v)) => home = Some(PathBuf::from(v)),
                ("--pid", Some(v)) => pid = v.to_str().and_then(|v| v.parse::<i32>().ok()),
                ("--lock-fd", Some(v)) => {
                    lock_fd = v.to_str().and_then(|v| v.parse::<RawFd>().ok())
                }
                ("--socket", Some(v)) => socket = Some(PathBuf::from(v)),
                ("--serve-lock", Some(v)) => serve_lock = Some(PathBuf::from(v)),
                _ => {
                    eprintln!("xshelld: upgrade-watch: bad arguments");
                    return 2;
                }
            }
        }
        let (Some(home), Some(pid), Some(lock_fd), Some(socket), Some(serve_lock)) =
            (home, pid, lock_fd, socket, serve_lock)
        else {
            eprintln!("xshelld: upgrade-watch: missing arguments");
            return 2;
        };
        // Held for the whole trial; never inherited by the build we may start.
        let lock = unsafe { fs::File::from_raw_fd(lock_fd) };
        unsafe {
            let flags = libc::fcntl(lock_fd, libc::F_GETFD);
            libc::fcntl(lock_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
        let pidfd = match pidfd_open(pid) {
            Ok(fd) => fd,
            Err(e) => {
                println!("error: {e}");
                return 1;
            }
        };
        // Leave the Daemon's children: our parent waits for this process, not the watch.
        match unsafe { libc::fork() } {
            -1 => {
                println!("error: {}", io::Error::last_os_error());
                return 1;
            }
            0 => {}
            _ => return 0,
        }
        let t = match trial::read(&home) {
            Ok(Some(t)) if t.pid == pid as u32 => t,
            _ => {
                println!("error: no update trial for pid {pid}");
                return 1;
            }
        };
        println!("ready");
        let _ = io::stdout().flush();
        // Nobody reads stdout any more.
        if let Ok(null) = fs::OpenOptions::new().write(true).open("/dev/null") {
            unsafe {
                libc::dup2(null.as_raw_fd(), 1);
            }
        }
        let w = Watch {
            home: home.clone(),
            pid,
            pidfd,
            socket,
            serve_lock,
            serve_args,
        };
        let limits = Limits::get();
        crate::log!(
            "INFO",
            "update trial: watching pid {} on {} (previous {})",
            w.pid,
            t.target,
            t.previous.as_deref().unwrap_or("none")
        );
        match w.judge(&t, limits) {
            Ok(()) => {
                if let Err(e) = trial::remove(&home).and_then(|()| trial::clear_failure(&home)) {
                    crate::log!("ERROR", "update trial: cannot finish: {e}");
                }
                crate::log!("INFO", "update trial: {} accepted", t.target);
            }
            Err(reason) => {
                crate::log!("WARN", "update trial failed: {reason}; rolling back");
                w.stop(limits);
                match trial::rollback(&home, &t, &reason) {
                    // A stop was asked for: the next start runs the previous build.
                    Ok(()) if reason == STOPPED => {}
                    Ok(()) => w.start_previous(&t),
                    Err(e) => crate::log!("ERROR", "update trial: cannot roll back: {e}"),
                }
            }
        }
        drop(lock);
        0
    }

    #[allow(dead_code)]
    fn read_all(mut r: impl Read) -> String {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    begin, corrupt_state_for_tests, exec_previous, on_start, reexec, report, restart,
    stopped_during_trial, wait_for_foreign_trial, watch_main, Decision,
};

/// Not Linux: no in-place update; the Daemon exits as before.
#[cfg(not(target_os = "linux"))]
pub fn reexec(_home: &Path, _paths: &crate::paths::Paths) -> i32 {
    0
}

#[cfg(not(target_os = "linux"))]
pub fn watch_main(_args: Vec<OsString>) -> i32 {
    eprintln!("xshelld: upgrade-watch is Linux only");
    1
}

/// `xshelld self-install [--select] [--switch] [--in-ring] [--home DIR]` (Unix; what
/// `install.sh` runs):
/// - `--select` copies this binary into `~/.xshell/server/<version>+<build>/` and prints
///   that directory. It records it as `selected` only on a fresh machine (no Daemon of this
///   home alive, no update trial; under `trial.lock`, S4 P1-2); with a running Daemon the
///   selection goes through that Daemon's transactional update (`--switch`).
/// - `--switch` brings a running Daemon of another build onto this one:
///   `daemon.upgrade-selected` where it is offered; an older Daemon gets `daemon.upgrade`,
///   is waited for until it has gone, and then this build is selected (P1-3).
/// - `--in-ring`: exit 0 when the running Daemon is in a Ring.
pub fn self_install_main(args: Vec<OsString>) -> i32 {
    let mut select = false;
    let mut switch = false;
    let mut in_ring = false;
    let mut home: Option<PathBuf> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--select") => select = true,
            Some("--switch") => switch = true,
            Some("--in-ring") => in_ring = true,
            Some("--home") => match it.next() {
                Some(v) => home = Some(v.into()),
                None => {
                    eprintln!("xshelld: --home needs a value");
                    return 2;
                }
            },
            _ => {
                eprintln!(
                    "xshelld: usage: xshelld self-install [--select] [--switch] [--home DIR]"
                );
                return 2;
            }
        }
    }
    let Some(home) = home
        .or_else(|| std::env::var_os("XSHELLD_HOME").map(PathBuf::from))
        .or_else(dirs::home_dir)
    else {
        eprintln!("xshelld: cannot determine the home directory; pass --home");
        return 1;
    };
    #[cfg(unix)]
    {
        if select {
            match select_self(&home) {
                Ok(dir) => println!("{dir}"),
                Err(e) => {
                    eprintln!("xshelld: {e}");
                    return 1;
                }
            }
        }
        if switch {
            let code = switch_running(&home);
            if code != 0 {
                return code;
            }
        }
        if in_ring {
            return i32::from(!running_in_ring(&home));
        }
        0
    }
    #[cfg(not(unix))]
    {
        let _ = (select, switch, in_ring, home);
        eprintln!("xshelld: self-install is Linux and macOS only");
        1
    }
}

#[cfg(unix)]
fn select_self(home: &Path) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = own_dir();
    xshell_hostlink::install::install_local(&exe, home, &dir)
        .map_err(|e| format!("cannot install: {e}"))?;
    trial::select_if_fresh(home, &dir).map_err(|e| format!("cannot select {dir}: {e}"))?;
    Ok(dir)
}

/// Whether the running Daemon of `home` is a member of a Ring (`ring.identity`).
#[cfg(unix)]
fn running_in_ring(home: &Path) -> bool {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let paths = crate::paths::resolve(home, xdg.as_deref(), None);
    let socket = crate::paths::live_socket(home, &paths.socket);
    Client::connect(&socket)
        .and_then(|mut c| {
            c.request(&xshell_protocol::msg::ClientMsg::RingIdentity)
                .map_err(std::io::Error::other)
        })
        .is_ok_and(|v| !v["ring"].is_null())
}

/// The install directory of the Daemon of `home` that answers a hello now (within 2 s).
#[cfg(unix)]
pub fn serving_build(home: &Path, env_socket: &Path) -> Option<String> {
    use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
    use xshell_protocol::msg::{decode_server, ServerMsg};
    let socket = crate::paths::live_socket(home, env_socket);
    let s = std::os::unix::net::UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .ok()?;
    let mut r = std::io::BufReader::new(s);
    match read_frame(&mut r, MAX_FRAME_LEN).ok()?? {
        Frame::Json(j) => match decode_server(&j).ok()? {
            ServerMsg::Hello(h) => Some(xshell_protocol::build::install_dir_name(
                &h.version,
                h.build.as_ref(),
            )),
            _ => None,
        },
        _ => None,
    }
}

/// Bring a running Daemon of another build onto this one. Exit 0 also when none runs.
#[cfg(unix)]
fn switch_running(home: &Path) -> i32 {
    use xshell_protocol::msg::ClientMsg;
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let paths = crate::paths::resolve(home, xdg.as_deref(), None);
    let socket = crate::paths::live_socket(home, &paths.socket);
    let dir = own_dir();
    if !crate::paths::daemon_alive(home, &paths.socket) {
        return select_fresh(home, &dir);
    }
    let mut c = match Client::connect(&socket) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xshelld: cannot reach the running xshelld: {e}");
            return 1;
        }
    };
    let theirs = xshell_protocol::build::install_dir_name(&c.hello.version, c.hello.build.as_ref());
    if theirs == dir {
        return 0;
    }
    let has = |cap: &str| c.hello.capabilities.iter().any(|x| x == cap);
    if let (Some(build), true) = (
        crate::build_id::BUILD.clone(),
        has(xshell_protocol::cap::DAEMON_STAGE),
    ) {
        // The Daemon selects this build in its own update transaction.
        let msg = ClientMsg::DaemonUpgradeSelected {
            version: env!("CARGO_PKG_VERSION").into(),
            build,
        };
        return match c.request(&msg) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("xshelld: the running xshelld refused the update: {e}");
                1
            }
        };
    }
    // An older Daemon: ask it to exit, wait until it has gone (its locks are free), then
    // select this build on the now quiet machine (P1-3).
    if let Err(e) = c.request(&ClientMsg::DaemonUpgrade) {
        eprintln!("xshelld: the running xshelld refused the update: {e}");
        return 1;
    }
    drop(c);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while crate::paths::daemon_alive(home, &paths.socket) {
        if std::time::Instant::now() >= until {
            eprintln!("xshelld: the running xshelld did not exit");
            return 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    select_fresh(home, &dir)
}

/// Select `dir` when nothing else decides the selection; 0 also when something does.
#[cfg(unix)]
fn select_fresh(home: &Path, dir: &str) -> i32 {
    match trial::select_if_fresh(home, dir) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("xshelld: cannot select {dir}: {e}");
            1
        }
    }
}

/// A minimal protocol client for `self-install --switch`.
#[cfg(unix)]
struct Client {
    s: std::os::unix::net::UnixStream,
    r: std::io::BufReader<std::os::unix::net::UnixStream>,
    hello: xshell_protocol::msg::Hello,
}

#[cfg(unix)]
impl Client {
    fn connect(socket: &Path) -> std::io::Result<Client> {
        use std::io::Write;
        use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
        use xshell_protocol::msg::{decode_server, encode_msg, ClientMsg, Hello, ServerMsg};
        let s = std::os::unix::net::UnixStream::connect(socket)?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        let mut r = std::io::BufReader::new(s.try_clone()?);
        let bad = || std::io::Error::other("no hello from the running xshelld");
        let hello = match read_frame(&mut r, MAX_FRAME_LEN).map_err(std::io::Error::other)? {
            Some(Frame::Json(j)) => match decode_server(&j) {
                Ok(ServerMsg::Hello(h)) => h,
                _ => return Err(bad()),
            },
            _ => return Err(bad()),
        };
        let ours = ClientMsg::Hello(Hello {
            protocol: xshell_protocol::PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            capabilities: Vec::new(),
            build: crate::build_id::BUILD.clone(),
        });
        let mut w = s.try_clone()?;
        w.write_all(&encode_msg(&ours, None).map_err(std::io::Error::other)?)?;
        Ok(Client { s, r, hello })
    }

    fn request(
        &mut self,
        msg: &xshell_protocol::msg::ClientMsg,
    ) -> Result<serde_json::Value, String> {
        use std::io::Write;
        use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
        use xshell_protocol::msg::{decode_server, encode_msg, ServerMsg};
        let frame = encode_msg(msg, Some(1)).map_err(|e| e.to_string())?;
        self.s.write_all(&frame).map_err(|e| e.to_string())?;
        loop {
            match read_frame(&mut self.r, MAX_FRAME_LEN).map_err(|e| e.to_string())? {
                Some(Frame::Json(j)) => match decode_server(&j) {
                    Ok(ServerMsg::Res(res)) if res.id == 1 => {
                        return res.outcome.into_result();
                    }
                    _ => continue,
                },
                Some(_) => continue,
                None => return Err("the connection closed".into()),
            }
        }
    }
}

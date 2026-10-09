//! The Local Host's Daemon (ADR-0005): the Desktop starts `xshelld serve --gui-bound` as its
//! own child when none answers on this user's socket, and reaches it like any Daemon on this
//! machine. A Daemon that already runs for this user is used instead (ADR-0003) and never
//! signalled by the Desktop.

use crate::cancel::CancelToken;
use crate::dial::{connect_unix, DialError, Dialed, Dialer, CONNECT_TIMEOUT};
use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// A started Daemon may need this long to listen: its login shell alone may take 5 s.
pub const SPAWN_WAIT: Duration = Duration::from_secs(15);

/// `serve` exits with this when another `serve` holds the lock: that one will listen.
const LOST_LOCK_EXIT: i32 = 3;

/// The Daemon socket of this user, as `xshelld`'s `paths::resolve` places it.
pub fn local_socket_path(home: &Path, xdg_runtime_dir: Option<&Path>) -> PathBuf {
    match xdg_runtime_dir.filter(|p| p.is_absolute()) {
        Some(x) => x.join("xshell").join("daemon.sock"),
        None => home.join(".xshell").join("run").join("daemon.sock"),
    }
}

/// The Daemon's log, as `xshelld`'s `paths::resolve` places it.
pub fn local_log_path(home: &Path) -> PathBuf {
    home.join(".xshell").join("log").join("xshelld.log")
}

type SpawnReply = mpsc::Sender<io::Result<Child>>;

/// The GUI-bound Daemon this Desktop starts, at most one at a time.
///
/// Children are started from one keeper thread that lives as long as this value: on Linux
/// the Daemon ends when the thread that started it does (`PR_SET_PDEATHSIG`), so it must not
/// be a supervisor thread that may end earlier.
pub struct GuiBoundDaemon {
    bin: PathBuf,
    env: Vec<(OsString, OsString)>,
    pub log: PathBuf,
    keeper: Mutex<mpsc::Sender<(Command, SpawnReply)>>,
    child: Mutex<Option<Child>>,
    quitting: AtomicBool,
}

impl GuiBoundDaemon {
    /// `bin` is the `xshelld` to start; `env` is added to the inherited environment.
    pub fn new(bin: PathBuf, env: Vec<(OsString, OsString)>, log: PathBuf) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<(Command, SpawnReply)>();
        std::thread::Builder::new()
            .name("daemon-keeper".into())
            .spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            })?;
        Ok(Self {
            bin,
            env,
            log,
            keeper: Mutex::new(tx),
            child: Mutex::new(None),
            quitting: AtomicBool::new(false),
        })
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// The pid of our running child, starting one if there is none. Refused after
    /// [`GuiBoundDaemon::hang_up`].
    pub fn ensure_spawned(&self) -> io::Result<u32> {
        let mut child = self.child.lock().unwrap();
        if self.quitting.load(Ordering::SeqCst) {
            return Err(io::Error::other("xshell is quitting"));
        }
        if let Some(c) = child.as_mut() {
            if c.try_wait()?.is_none() {
                return Ok(c.id());
            }
        }
        let mut cmd = Command::new(&self.bin);
        cmd.arg("serve")
            .arg("--gui-bound")
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir("/")
            // Its own process group: Ctrl+C in the terminal `tauri dev` runs in reaches the
            // Desktop only, which then ends the Daemon in order.
            .process_group(0);
        let (tx, rx) = mpsc::channel();
        self.keeper
            .lock()
            .unwrap()
            .send((cmd, tx))
            .map_err(|_| io::Error::other("the keeper thread is gone"))?;
        let c = rx
            .recv()
            .map_err(|_| io::Error::other("the keeper thread is gone"))??;
        let pid = c.id();
        *child = Some(c);
        Ok(pid)
    }

    /// If our child has exited: reap it and return its exit code (`None`: by a signal).
    pub fn take_exit(&self) -> Option<Option<i32>> {
        let mut child = self.child.lock().unwrap();
        let st = child.as_mut()?.try_wait().ok()??;
        *child = None;
        Some(st.code())
    }

    /// Our child's pid while it runs.
    pub fn pid(&self) -> Option<u32> {
        let mut child = self.child.lock().unwrap();
        let c = child.as_mut()?;
        matches!(c.try_wait(), Ok(None)).then(|| c.id())
    }

    /// Quitting: SIGTERM our child (an orderly shutdown that ends its Terminals) and start no
    /// other. A Daemon we did not start is left alone.
    pub fn hang_up(&self) {
        let mut child = self.child.lock().unwrap();
        self.quitting.store(true, Ordering::SeqCst);
        if let Some(c) = child.as_mut() {
            if matches!(c.try_wait(), Ok(None)) {
                unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
            }
        }
    }

    /// After [`GuiBoundDaemon::hang_up`]: wait for our child until `deadline`, then SIGKILL
    /// and reap it.
    pub fn reap(&self, deadline: Instant) {
        let Some(mut c) = self.child.lock().unwrap().take() else {
            return;
        };
        while Instant::now() < deadline {
            if !matches!(c.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = c.kill();
        let _ = c.wait();
    }

    /// [`GuiBoundDaemon::hang_up`], then [`GuiBoundDaemon::reap`] within `within`.
    pub fn terminate(&self, within: Duration) {
        self.hang_up();
        self.reap(Instant::now() + within);
    }
}

/// The Local Host's [`Dialer`]: the user's Daemon socket, starting [`GuiBoundDaemon`] only
/// when nothing listens there (the socket is missing or refuses).
pub struct GuiBoundDialer {
    pub socket: PathBuf,
    pub daemon: Arc<GuiBoundDaemon>,
}

impl GuiBoundDialer {
    fn connected(
        &self,
        s: std::os::unix::net::UnixStream,
        cancel: &CancelToken,
    ) -> Result<Dialed, DialError> {
        Dialed::from_unix_stream(s, cancel).map_err(|e| {
            DialError::failed(
                format!("cannot connect to {}: {e}", self.socket.display()),
                None,
            )
        })
    }

    fn see_log(&self, what: String) -> DialError {
        DialError::failed(format!("{what}; see {}", self.daemon.log.display()), None)
    }
}

impl Dialer for GuiBoundDialer {
    fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError> {
        match connect_unix(&self.socket, cancel, CONNECT_TIMEOUT) {
            Ok(s) => return self.connected(s, cancel),
            Err(e) if e.no_listener() => {}
            Err(e) => return Err(e),
        }
        self.daemon.ensure_spawned().map_err(|e| {
            self.see_log(format!("cannot start {}: {e}", self.daemon.bin.display()))
        })?;
        let deadline = Instant::now() + SPAWN_WAIT;
        let mut delay = Duration::from_millis(10);
        let mut lost_lock = false;
        loop {
            if cancel.sleep(delay) {
                return Err(DialError::Cancelled);
            }
            delay = (delay * 2).min(Duration::from_millis(200));
            let left = deadline.saturating_duration_since(Instant::now());
            match connect_unix(&self.socket, cancel, left.max(Duration::from_millis(1))) {
                Ok(s) => return self.connected(s, cancel),
                Err(e) if e.no_listener() => {}
                Err(e) => return Err(e),
            }
            if !lost_lock {
                match self.daemon.take_exit() {
                    // Another `serve` won the lock; it will listen shortly.
                    Some(Some(LOST_LOCK_EXIT)) => lost_lock = true,
                    Some(code) => {
                        let how = code.map_or("by a signal".to_string(), |c| format!("with {c}"));
                        return Err(self.see_log(format!("xshelld serve exited {how}")));
                    }
                    None => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(self.see_log(format!(
                    "xshelld serve did not start listening within {SPAWN_WAIT:?}"
                )));
            }
        }
    }

    fn describe(&self) -> String {
        self.socket.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    /// A fake `xshelld` script: logs each start, then runs `body`.
    fn fake_bin(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("xshelld");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\n[ \"$1\" = probe ] && exit 0\necho \"$@\" >> '{}'\n{body}\n",
                dir.join("starts").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Another test's fork may briefly hold the new file open for writing (ETXTBSY).
        for _ in 0..100 {
            if Command::new(&p).arg("probe").status().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        p
    }

    fn starts(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("starts"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn dialer(dir: &Path, body: &str) -> GuiBoundDialer {
        let bin = fake_bin(dir, body);
        GuiBoundDialer {
            socket: dir.join("s.sock"),
            daemon: Arc::new(GuiBoundDaemon::new(bin, vec![], dir.join("log")).unwrap()),
        }
    }

    #[test]
    fn local_socket_path_mirrors_xshelld() {
        let h = Path::new("/h");
        assert_eq!(
            local_socket_path(h, Some(Path::new("/r"))),
            PathBuf::from("/r/xshell/daemon.sock")
        );
        assert_eq!(
            local_socket_path(h, None),
            PathBuf::from("/h/.xshell/run/daemon.sock")
        );
        assert_eq!(
            local_socket_path(h, Some(Path::new("r"))),
            local_socket_path(h, None)
        );
        assert_eq!(
            local_log_path(h),
            PathBuf::from("/h/.xshell/log/xshelld.log")
        );
    }

    #[test]
    fn gui_bound_dialer_spawns_only_on_missing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        // Something listens: it is used and nothing starts.
        let l = UnixListener::bind(dir.path().join("s.sock")).unwrap();
        let ok = d.dial(&CancelToken::new());
        assert!(ok.is_ok());
        assert!(starts(dir.path()).is_empty());
        drop(ok);
        drop(l);
        // A stale socket file refuses: a Daemon is started, with the GUI-bound flags.
        let c = CancelToken::new();
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            c2.cancel();
        });
        assert!(matches!(d.dial(&c), Err(DialError::Cancelled)));
        let s = starts(dir.path());
        assert_eq!(
            s,
            vec![format!(
                "serve --gui-bound --parent-pid {}",
                std::process::id()
            )]
        );
        // Still running: a second dial starts no other.
        let c = CancelToken::new();
        c.cancel();
        let _ = d.dial(&c);
        assert_eq!(starts(dir.path()).len(), 1);
        d.daemon.terminate(Duration::from_secs(2));
        assert_eq!(d.daemon.pid(), None);
    }

    #[test]
    fn gui_bound_dialer_retries_after_lost_lock() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        // The fake loses the lock at once; the winner listens a little later.
        let d = dialer(dir.path(), "exit 3");
        let s2 = sock.clone();
        let winner = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            let l = UnixListener::bind(&s2).unwrap();
            let _ = l.accept();
        });
        let t0 = Instant::now();
        let r = d.dial(&CancelToken::new());
        assert!(r.is_ok(), "{:?}", r.err());
        assert!(t0.elapsed() >= Duration::from_millis(500));
        assert_eq!(starts(dir.path()).len(), 1);
        drop(r);
        winner.join().unwrap();
    }

    #[test]
    fn gui_bound_dialer_cancel_is_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        let c = CancelToken::new();
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.cancel();
        });
        let t0 = Instant::now();
        assert!(matches!(d.dial(&c), Err(DialError::Cancelled)));
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        d.daemon.terminate(Duration::from_secs(2));
    }

    #[test]
    fn gui_bound_dialer_spawn_failure_is_failed() {
        let dir = tempfile::tempdir().unwrap();
        // The child exits at once with an error.
        let d = dialer(dir.path(), "exit 1");
        match d.dial(&CancelToken::new()) {
            Err(DialError::Failed { message, .. }) => {
                assert!(message.contains("exited with 1"), "{message}");
                assert!(message.contains(&dir.path().join("log").display().to_string()));
            }
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("connected to nothing"),
        }
        // A binary that cannot run at all.
        let d = GuiBoundDialer {
            socket: dir.path().join("s.sock"),
            daemon: Arc::new(
                GuiBoundDaemon::new(dir.path().join("absent"), vec![], dir.path().join("log"))
                    .unwrap(),
            ),
        };
        match d.dial(&CancelToken::new()) {
            Err(DialError::Failed { message, .. }) => {
                assert!(message.contains("cannot start"), "{message}")
            }
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("connected to nothing"),
        }
    }

    /// Failures other than a missing listener never start a Daemon.
    #[test]
    fn gui_bound_dialer_other_failures_never_spawn() {
        let dir = tempfile::tempdir().unwrap();
        // Permission denied: the socket's directory is not searchable.
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let _l = UnixListener::bind(locked.join("s.sock")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root = unsafe { libc::geteuid() } == 0;
        let mut d = dialer(dir.path(), "exec sleep 30");
        if !root {
            d.socket = locked.join("s.sock");
            assert!(matches!(
                d.dial(&CancelToken::new()),
                Err(DialError::Failed { .. })
            ));
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        // An invalid path: too long for `sun_path`.
        d.socket = dir.path().join("x".repeat(200));
        assert!(matches!(
            d.dial(&CancelToken::new()),
            Err(DialError::Failed { .. })
        ));
        assert!(starts(dir.path()).is_empty());
        d.daemon.terminate(Duration::from_secs(2));
    }

    /// A listener whose backlog stays full times out without ever starting a Daemon.
    #[cfg(target_os = "linux")]
    #[test]
    fn gui_bound_dialer_full_backlog_never_spawns() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        let l = UnixListener::bind(&d.socket).unwrap();
        assert_eq!(unsafe { libc::listen(l.as_raw_fd(), 0) }, 0);
        let mut queued = Vec::new();
        loop {
            match crate::dial::connect_nb(&d.socket) {
                Ok(crate::dial::Pending::Done(s)) => queued.push(s),
                Ok(crate::dial::Pending::InProgress(_)) => panic!("unexpected in-progress connect"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
            assert!(queued.len() < 1000, "the backlog never filled");
        }
        let c = CancelToken::new();
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c2.cancel();
        });
        let r = d.dial(&c);
        assert!(matches!(r, Err(DialError::Cancelled)), "{:?}", r.err());
        assert!(starts(dir.path()).is_empty());
        drop(l);
    }
}

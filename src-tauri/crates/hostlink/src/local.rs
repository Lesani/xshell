//! The Local Host's Daemon (ADR-0005). By default the Desktop starts
//! `xshelld serve --gui-bound` as its own child when none answers on this user's socket, and
//! reaches it like any Daemon on this machine. With the Persistent Daemon setting on (#25) it
//! starts `serve --interactive-env` instead: detached, from the copy installed under
//! `~/.xshell/server/<version>`, so the Terminals outlive the Desktop.
//!
//! A Daemon that already runs for this user is used instead (ADR-0003) and never signalled by
//! the Desktop, except by a switch or an upgrade the user confirmed.

use crate::cancel::CancelToken;
use crate::dial::{connect_unix, DialError, Dialed, Dialer, CONNECT_TIMEOUT};
pub use crate::errors::SwitchError;
use crate::handle::HostHandle;
use crate::status::StatusKind;
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

/// A Daemon's orderly shutdown ends its Terminals within about 8 s.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// The longest wait for `daemon.upgrade` to settle (its reply or its own request timeout).
const UPGRADE_SETTLE_WAIT: Duration = Duration::from_secs(120);

/// What `serve` records in the mode marker, as `xshelld`'s `paths::Mode` spells it.
pub const MODE_GUI_BOUND: &str = "gui-bound";
pub const MODE_PERSISTENT: &str = "persistent";

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

/// The mode marker (`gui-bound` / `persistent`): how the Daemon that last held the lock runs.
pub fn local_mode_path(home: &Path) -> PathBuf {
    home.join(".xshell").join("daemon").join("mode")
}

/// The pidfile next to `socket`, written by the `serve` that holds the lock.
pub fn local_pid_path(socket: &Path) -> PathBuf {
    socket.parent().unwrap_or(Path::new("/")).join("daemon.pid")
}

/// The recorded mode; `None` when there is no marker or it is unreadable.
pub fn read_local_mode(path: &Path) -> Option<&'static str> {
    match std::fs::read_to_string(path).ok()?.trim() {
        MODE_GUI_BOUND => Some(MODE_GUI_BOUND),
        MODE_PERSISTENT => Some(MODE_PERSISTENT),
        _ => None,
    }
}

fn mode_name(persistent: bool) -> &'static str {
    if persistent {
        MODE_PERSISTENT
    } else {
        MODE_GUI_BOUND
    }
}

/// How a Daemon this Desktop started runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Our child for good: it ends with this Desktop, and quitting ends it.
    GuiBound,
    /// Detached: it outlives this Desktop and is never signalled at quit.
    Persistent,
}

struct Spawned {
    kind: Kind,
    child: Child,
}

/// Wait for a child on its own thread, so it never lingers as a zombie, and never signal it.
fn reap_in_background(mut child: Child) {
    let _ = std::thread::Builder::new()
        .name("reap-daemon".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

type SpawnReply = mpsc::Sender<io::Result<Child>>;

/// What [`LocalDaemon`] needs to know about this machine.
pub struct LocalDaemonConfig {
    /// The app's `xshelld` (the sidecar).
    pub bin: PathBuf,
    /// Added to the inherited environment of every Daemon started.
    pub env: Vec<(OsString, OsString)>,
    pub home: PathBuf,
    pub socket: PathBuf,
    /// The Desktop's version: a Persistent Daemon runs from `~/.xshell/server/<version>`.
    pub version: String,
}

/// The Daemon this Desktop starts for the Local Host, at most one at a time, in the mode the
/// Persistent Daemon setting asks for ([`LocalDaemon::persistent`]).
///
/// GUI-bound children are started from one keeper thread that lives as long as this value:
/// on Linux the Daemon ends when the thread that started it does (`PR_SET_PDEATHSIG`), so it
/// must not be a supervisor thread that may end earlier.
pub struct LocalDaemon {
    bin: PathBuf,
    env: Vec<(OsString, OsString)>,
    pub log: PathBuf,
    home: PathBuf,
    socket: PathBuf,
    version: String,
    /// The mode new Daemons start in: the setting.
    persistent: AtomicBool,
    keeper: Mutex<mpsc::Sender<(Command, SpawnReply)>>,
    child: Mutex<Option<Spawned>>,
    quitting: AtomicBool,
    /// Test hook: runs in `stop_verified` between the verification and the signal.
    #[cfg(test)]
    before_signal: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl LocalDaemon {
    pub fn new(cfg: LocalDaemonConfig) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel::<(Command, SpawnReply)>();
        std::thread::Builder::new()
            .name("daemon-keeper".into())
            .spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            })?;
        Ok(Self {
            log: local_log_path(&cfg.home),
            bin: cfg.bin,
            env: cfg.env,
            home: cfg.home,
            socket: cfg.socket,
            version: cfg.version,
            persistent: AtomicBool::new(false),
            keeper: Mutex::new(tx),
            child: Mutex::new(None),
            quitting: AtomicBool::new(false),
            #[cfg(test)]
            before_signal: Mutex::new(None),
        })
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Whether new Daemons start Persistent (the setting).
    pub fn persistent(&self) -> bool {
        self.persistent.load(Ordering::SeqCst)
    }

    /// Set the mode new Daemons start in. Changes nothing that runs: see
    /// [`LocalDaemon::switch`].
    pub fn set_persistent(&self, on: bool) {
        self.persistent.store(on, Ordering::SeqCst);
    }

    /// The mode marker: how the Daemon that holds (or last held) the lock runs.
    pub fn running_mode(&self) -> Option<&'static str> {
        read_local_mode(&local_mode_path(&self.home))
    }

    /// The pid of our running child, starting one in the current mode if there is none.
    /// Refused after [`LocalDaemon::hang_up`].
    pub fn ensure_spawned(&self) -> io::Result<u32> {
        let mut slot = self.child.lock().unwrap();
        if self.quitting.load(Ordering::SeqCst) {
            return Err(io::Error::other("xshell is quitting"));
        }
        if let Some(s) = slot.as_mut() {
            if s.child.try_wait()?.is_none() {
                return Ok(s.child.id());
            }
        }
        let (kind, child) = if self.persistent() {
            (Kind::Persistent, self.spawn_persistent()?)
        } else {
            (Kind::GuiBound, self.spawn_gui_bound()?)
        };
        let pid = child.id();
        *slot = Some(Spawned { kind, child });
        Ok(pid)
    }

    fn spawn_gui_bound(&self) -> io::Result<Child> {
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
        rx.recv()
            .map_err(|_| io::Error::other("the keeper thread is gone"))?
    }

    /// Install this Desktop's `xshelld` under `~/.xshell/server/<version>` and start it there
    /// in a new session, its output to the Daemon log, as `xshelld connect` would.
    fn spawn_persistent(&self) -> io::Result<Child> {
        let exe = crate::install::install_local(&self.bin, &self.home, &self.version)?;
        let log = open_log(&self.log)?;
        let mut cmd = Command::new(exe);
        cmd.arg("serve")
            .arg("--interactive-env")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .current_dir("/");
        // A new session: no terminal hangup or process-group signal of the app reaches it.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // The copy was just written: a fork elsewhere in this process may still hold its
        // write descriptor until that child execs (ETXTBSY). Retry briefly.
        let mut tries = 0;
        loop {
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 100 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                r => return r,
            }
        }
    }

    /// If our child has exited: reap it and return its exit code (`None`: by a signal).
    pub fn take_exit(&self) -> Option<Option<i32>> {
        let mut slot = self.child.lock().unwrap();
        let st = slot.as_mut()?.child.try_wait().ok()??;
        *slot = None;
        Some(st.code())
    }

    /// A Daemon answers on the socket: a Persistent child of ours is no longer watched here,
    /// only waited for.
    fn listening_now(&self) {
        let mut slot = self.child.lock().unwrap();
        if slot.as_ref().is_some_and(|s| s.kind == Kind::Persistent) {
            reap_in_background(slot.take().unwrap().child);
        }
    }

    fn running_child(&self, kind: Option<Kind>) -> Option<u32> {
        let mut slot = self.child.lock().unwrap();
        let s = slot.as_mut()?;
        if kind.is_some_and(|k| k != s.kind) {
            return None;
        }
        matches!(s.child.try_wait(), Ok(None)).then(|| s.child.id())
    }

    /// Our child's pid while it runs (a Persistent one only until it listens).
    pub fn pid(&self) -> Option<u32> {
        self.running_child(None)
    }

    /// Our GUI-bound child's pid while it runs.
    pub fn gui_pid(&self) -> Option<u32> {
        self.running_child(Some(Kind::GuiBound))
    }

    /// Quitting: SIGTERM a GUI-bound child (an orderly shutdown that ends its Terminals) and
    /// start no other. A Persistent child, even one still starting, is left running and only
    /// waited for; a Daemon we did not start is left alone.
    pub fn hang_up(&self) {
        let mut slot = self.child.lock().unwrap();
        self.quitting.store(true, Ordering::SeqCst);
        match slot.take() {
            Some(mut s) if s.kind == Kind::GuiBound => {
                if matches!(s.child.try_wait(), Ok(None)) {
                    unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) };
                }
                *slot = Some(s);
            }
            Some(s) => reap_in_background(s.child),
            None => {}
        }
    }

    /// After [`LocalDaemon::hang_up`]: wait for a GUI-bound child until `deadline`, then
    /// SIGKILL and reap it. A Persistent child is never waited for or killed here.
    pub fn reap(&self, deadline: Instant) {
        let s = {
            let mut slot = self.child.lock().unwrap();
            if !slot.as_ref().is_some_and(|s| s.kind == Kind::GuiBound) {
                return;
            }
            slot.take().unwrap()
        };
        wait_or_kill(s.child, deadline);
    }

    /// [`LocalDaemon::hang_up`], then [`LocalDaemon::reap`] within `within`.
    pub fn terminate(&self, within: Duration) {
        self.hang_up();
        self.reap(Instant::now() + within);
    }

    /// End our GUI-bound child in order (its Terminals end, their state is kept) without
    /// quitting: the next dial starts a Daemon again. Returns whether there was one.
    pub fn stop_gui_child(&self, within: Duration) -> bool {
        let s = {
            let mut slot = self.child.lock().unwrap();
            if !slot.as_ref().is_some_and(|s| s.kind == Kind::GuiBound) {
                return false;
            }
            slot.take().unwrap()
        };
        let mut child = s.child;
        if matches!(child.try_wait(), Ok(None)) {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        }
        wait_or_kill(child, Instant::now() + within);
        true
    }

    /// Whether a Daemon answers on the socket.
    fn listening(&self) -> bool {
        connect_unix(&self.socket, &CancelToken::new(), Duration::from_secs(2)).is_ok()
    }

    /// SIGTERM the Persistent Daemon serving the socket, once its pidfile and the socket agree
    /// on its pid, and wait until it is gone. Like a Remote Host's upgrade kill script, its
    /// Terminals end and their state is kept. A GUI-bound Daemon (another xshell's, or ours)
    /// is never stopped here: `OtherApp`.
    ///
    /// The signal goes through a handle pinned to the verified process, so a Daemon that exits
    /// and is replaced in between is never signalled: a pidfd on Linux; on macOS a re-check of
    /// the socket's peer and the process start time just before `kill` (best effort).
    pub fn stop_verified(&self, within: Duration) -> Result<(), SwitchError> {
        let failed = |m: String| SwitchError::Failed(m);
        let s = match connect_unix(&self.socket, &CancelToken::new(), Duration::from_secs(2)) {
            Ok(s) => s,
            Err(e) if e.no_listener() => return Ok(()),
            Err(e) => return Err(failed(format!("{e:?}"))),
        };
        if self.running_mode() != Some(MODE_PERSISTENT) {
            return Err(SwitchError::OtherApp);
        }
        let pidfile = local_pid_path(&self.socket);
        let named: i32 = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .filter(|p| *p > 1)
            .ok_or_else(|| failed(format!("{} names no process", pidfile.display())))?;
        // Pinned while the connection still holds the server.
        let pinned =
            Pinned::peer(&s).map_err(|e| failed(format!("cannot identify the Daemon: {e}")))?;
        drop(s);
        if pinned.pid != named {
            return Err(failed(format!(
                "{} names pid {named}, but pid {} serves {}",
                pidfile.display(),
                pinned.pid,
                self.socket.display()
            )));
        }
        if self.gui_pid() == Some(named as u32) {
            return Err(SwitchError::OtherApp);
        }
        #[cfg(test)]
        if let Some(h) = self.before_signal.lock().unwrap().take() {
            h();
        }
        match pinned.signal(&self.socket, libc::SIGTERM) {
            Ok(()) => {}
            // Gone already, or replaced: nothing of ours to stop.
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
            Err(e) => return Err(failed(format!("cannot stop xshelld (pid {named}): {e}"))),
        }
        let deadline = Instant::now() + within;
        loop {
            let gone = pinned.exited()
                || matches!(
                    connect_unix(&self.socket, &CancelToken::new(), Duration::from_millis(500)),
                    Err(e) if e.no_listener()
                );
            if gone {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(failed(format!(
                    "xshelld (pid {named}) did not exit within {within:?}"
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Run the Local Host's Terminals in a Daemon of the other mode: Persistent (`persistent`)
    /// or GUI-bound. The setting ([`LocalDaemon::persistent`]) is always updated; the Terminals
    /// restart (agents resume, shells start fresh) unless a usable connection already shows
    /// the target mode, for GUI-bound in a child of ours.
    ///
    /// - On: our GUI-bound child ends in order, and the dial starts a Persistent Daemon that
    ///   restores its Terminals. Another xshell's GUI-bound Daemon is refused.
    /// - Off: `daemon.upgrade` asks the Persistent Daemon to persist its Terminals and exit;
    ///   the dial starts a GUI-bound child that restores them.
    ///
    /// `confirmed`: how many Terminals the user agreed to restart. Right before the hand-over
    /// the live list (of a usable connection) is checked again; more Terminals than that
    /// refuse with `ConfirmAgain`.
    ///
    /// On failure the switch is undone where that is safe, and the setting is left matching
    /// what runs: read [`LocalDaemon::persistent`] afterwards.
    pub fn switch(
        &self,
        host: &HostHandle,
        persistent: bool,
        confirmed: usize,
        timeout: Duration,
    ) -> Result<(), SwitchError> {
        let prev = self.persistent.swap(persistent, Ordering::SeqCst);
        let want = mode_name(persistent);
        if usable(host)
            && self.running_mode() == Some(want)
            && (persistent || self.gui_pid().is_some())
        {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        let listening = self.listening();
        let running = self.running_mode();
        // A GUI-bound Daemon that is not ours belongs to another xshell.
        if listening && running == Some(MODE_GUI_BOUND) && self.gui_pid().is_none() {
            self.set_persistent(prev);
            return Err(SwitchError::OtherApp);
        }
        let gen = if usable(host) {
            host.link_generation()
        } else {
            0
        };
        let gen = if persistent {
            if listening && running == Some(MODE_GUI_BOUND) {
                if let Err(e) = confirmed_list(host, confirmed) {
                    self.set_persistent(prev);
                    return Err(e);
                }
                self.stop_gui_child(STOP_WAIT);
            }
            gen
        } else if listening && running == Some(MODE_PERSISTENT) {
            if !wait_for(deadline, || {
                host.kick();
                usable(host)
            }) {
                self.set_persistent(prev);
                return Err(failure(host));
            }
            if let Err(e) = confirmed_list(host, confirmed) {
                self.set_persistent(prev);
                return Err(e);
            }
            let gen = host.link_generation();
            let (tx, rx) = mpsc::channel();
            host.upgrade(Box::new(move |r| {
                let _ = tx.send(r);
            }));
            // Settled once the reply (or the request's own timeout) arrives: until then the
            // Daemon may still act on it. Bounded in case the waiter is never called.
            let r = rx.recv_timeout(UPGRADE_SETTLE_WAIT);
            match r {
                Ok(Ok(_)) => {}
                // Uncertain: the Daemon may have exited and a successor may be starting.
                // Reconcile with what actually runs.
                other => {
                    let err = match other {
                        Ok(Err(e)) => SwitchError::Failed(e.message),
                        _ => SwitchError::Timeout,
                    };
                    self.undo_off(host, gen);
                    return Err(err);
                }
            }
            gen
        } else {
            gen
        };
        host.kick();
        if self.wait_mode(host, gen, persistent, deadline) {
            return Ok(());
        }
        let err = failure(host);
        if persistent {
            self.undo_on(host, gen);
        } else {
            self.undo_off(host, gen);
        }
        Err(err)
    }

    /// Wait until a link newer than `gen` is usable, the marker says the target mode and (for
    /// GUI-bound) the Daemon is our child.
    fn wait_mode(&self, host: &HostHandle, gen: u64, persistent: bool, deadline: Instant) -> bool {
        wait_for(deadline, || {
            let up = usable(host);
            if !up {
                // A kick that arrived while the old link still looked up was dropped.
                host.kick();
            }
            host.link_generation() > gen
                && up
                && self.running_mode() == Some(mode_name(persistent))
                && (persistent || self.gui_pid().is_some())
        })
    }

    /// A Persistent Daemon did not come up after switching on. One that runs keeps running
    /// unless it has no Terminals (it may have restored them, and other devices may use it);
    /// one still starting is left to start. Otherwise back to GUI-bound. `gen`: the link
    /// generation before the switch.
    fn undo_on(&self, host: &HostHandle, gen: u64) {
        let deadline = Instant::now() + SPAWN_WAIT;
        if self.listening() && self.running_mode() == Some(MODE_PERSISTENT) {
            let empty = usable(host) && host.snapshot().terminals.is_some_and(|t| t.is_empty());
            if !empty || self.stop_verified(STOP_WAIT).is_err() {
                return;
            }
        } else if self.running_child(Some(Kind::Persistent)).is_some() {
            return;
        }
        self.set_persistent(false);
        host.kick();
        if !self.wait_mode(host, gen, false, deadline)
            && self.running_mode() == Some(MODE_PERSISTENT)
        {
            // Something else started a Persistent Daemon meanwhile.
            self.set_persistent(self.listening());
        }
    }

    /// A GUI-bound Daemon did not come up after switching off: end an owned one that is
    /// stuck, and go back to Persistent operation. `gen`: the link generation before the
    /// switch.
    fn undo_off(&self, host: &HostHandle, gen: u64) {
        let deadline = Instant::now() + SPAWN_WAIT;
        if self.gui_pid().is_none()
            && usable(host)
            && host.link_generation() == gen
            && self.running_mode() == Some(MODE_PERSISTENT)
        {
            // The hand-over never started: the Persistent Daemon still serves the same link.
            self.set_persistent(true);
            return;
        }
        if self.gui_pid().is_some() {
            if usable(host) && self.running_mode() == Some(MODE_GUI_BOUND) {
                // It came up after all.
                return;
            }
            self.stop_gui_child(STOP_WAIT);
        }
        self.set_persistent(true);
        host.kick();
        if !self.wait_mode(host, gen, true, deadline)
            && self.running_mode() == Some(MODE_GUI_BOUND)
            && self.gui_pid().is_some()
        {
            self.set_persistent(false);
        }
    }
}

fn usable(host: &HostHandle) -> bool {
    matches!(
        host.status().status,
        StatusKind::Connected | StatusKind::UpgradePending
    )
}

/// The live Terminal list allows the restart the user confirmed: a usable connection's
/// list, no longer than `confirmed`.
fn confirmed_list(host: &HostHandle, confirmed: usize) -> Result<(), SwitchError> {
    if !usable(host) {
        return Err(failure(host));
    }
    match host.snapshot().terminals {
        Some(t) if t.len() <= confirmed => Ok(()),
        Some(t) => Err(SwitchError::ConfirmAgain(t.len())),
        None => Err(SwitchError::Failed(
            "the terminal list is not known yet".into(),
        )),
    }
}

fn failure(host: &HostHandle) -> SwitchError {
    let st = host.status();
    match st.last_error {
        Some(e) if !usable(host) => SwitchError::Failed(e),
        _ => SwitchError::Timeout,
    }
}

fn wait_for(deadline: Instant, mut done: impl FnMut() -> bool) -> bool {
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_or_kill(mut c: Child, deadline: Instant) {
    while Instant::now() < deadline {
        if !matches!(c.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = c.kill();
    let _ = c.wait();
}

/// Open the Daemon log for appending (0600, its directory 0700), as `xshelld` does.
fn open_log(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// The pid of the process serving the other end of a Unix socket.
fn peer_pid(s: &std::os::unix::net::UnixStream) -> io::Result<i32> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.pid)
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SOL_LOCAL is 0.
        let r = unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                0,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = s.as_raw_fd();
        Err(io::Error::other("unsupported on this platform"))
    }
}

/// A process, pinned so a signal never reaches another one that later got its pid.
struct Pinned {
    pid: i32,
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
    #[cfg(target_os = "macos")]
    start: Option<(u64, u64)>,
}

impl Pinned {
    /// The server at the other end of `s`, while `s` is still connected.
    fn peer(s: &std::os::unix::net::UnixStream) -> io::Result<Pinned> {
        let pid = peer_pid(s)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            // The kernel's own pidfd of the peer (Linux 6.5), else one opened by pid.
            let mut fd: libc::c_int = -1;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let r = unsafe {
                libc::getsockopt(
                    s.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERPIDFD,
                    (&mut fd as *mut libc::c_int).cast(),
                    &mut len,
                )
            };
            if r == 0 && fd >= 0 {
                return Ok(Pinned {
                    pid,
                    fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) },
                });
            }
            Self::open(pid)
        }
        #[cfg(not(target_os = "linux"))]
        Self::open(pid)
    }

    #[cfg(target_os = "linux")]
    fn open(pid: i32) -> io::Result<Pinned> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pinned {
            pid,
            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) },
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn open(pid: i32) -> io::Result<Pinned> {
        Ok(Pinned {
            pid,
            #[cfg(target_os = "macos")]
            start: start_time(pid),
        })
    }

    /// Signal the pinned process. `ESRCH`: it is gone (or, on macOS, was replaced).
    fn signal(&self, socket: &Path, sig: libc::c_int) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let _ = socket;
            let r = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Best effort: the socket's server and the process start time must be unchanged
            // right before the kill.
            let still = connect_unix(socket, &CancelToken::new(), Duration::from_secs(2))
                .ok()
                .and_then(|s| peer_pid(&s).ok())
                == Some(self.pid);
            #[cfg(target_os = "macos")]
            let still = still && self.start.is_some() && start_time(self.pid) == self.start;
            if !still {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            if unsafe { libc::kill(self.pid, sig) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    /// Whether the pinned process has exited.
    fn exited(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let mut p = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut p, 1, 0) > 0 }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let gone = unsafe { libc::kill(self.pid, 0) } != 0;
            #[cfg(target_os = "macos")]
            let gone = gone || start_time(self.pid) != self.start;
            gone
        }
    }
}

/// A process's start time (seconds, microseconds), to tell it from a later one with its pid.
#[cfg(target_os = "macos")]
fn start_time(pid: i32) -> Option<(u64, u64)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

/// The Local Host's [`Dialer`]: the user's Daemon socket, starting [`LocalDaemon`] only when
/// nothing listens there (the socket is missing or refuses).
pub struct LocalDialer {
    pub daemon: Arc<LocalDaemon>,
}

impl LocalDialer {
    fn connected(
        &self,
        s: std::os::unix::net::UnixStream,
        cancel: &CancelToken,
    ) -> Result<Dialed, DialError> {
        Dialed::from_unix_stream(s, cancel).map_err(|e| {
            DialError::failed(
                format!("cannot connect to {}: {e}", self.daemon.socket.display()),
                None,
            )
        })
    }

    fn see_log(&self, what: String) -> DialError {
        DialError::failed(format!("{what}; see {}", self.daemon.log.display()), None)
    }
}

impl Dialer for LocalDialer {
    fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError> {
        let socket = &self.daemon.socket;
        match connect_unix(socket, cancel, CONNECT_TIMEOUT) {
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
            match connect_unix(socket, cancel, left.max(Duration::from_millis(1))) {
                Ok(s) => {
                    self.daemon.listening_now();
                    return self.connected(s, cancel);
                }
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
        self.daemon.socket.display().to_string()
    }

    /// While the setting is on, the Local Host's Daemon is this Desktop's to upgrade.
    fn upgradable(&self) -> bool {
        self.daemon.persistent()
    }

    fn stop_incompatible(&self, _cancel: &CancelToken) -> Option<Result<(), crate::HostError>> {
        self.daemon.persistent().then(|| {
            self.daemon.stop_verified(STOP_WAIT).map_err(|e| match e {
                SwitchError::OtherApp => crate::HostError::new(
                    crate::HostErrorCode::Incompatible,
                    "another xshell runs this computer's terminals; quit it and try again",
                ),
                SwitchError::Failed(m) => crate::HostError::offline(m),
                e => crate::HostError::offline(format!("{e:?}")),
            })
        })
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

    /// The starts logged once at least `n` are in: the fake logs from its own process, which
    /// may run after the dial that spawned it returned (a loaded machine).
    fn wait_starts(dir: &Path, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = starts(dir);
            if s.len() >= n || Instant::now() >= deadline {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn daemon_with(dir: &Path, bin: PathBuf) -> Arc<LocalDaemon> {
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Arc::new(
            LocalDaemon::new(LocalDaemonConfig {
                bin,
                env: vec![],
                home,
                socket: dir.join("s.sock"),
                version: "1.5.0".into(),
            })
            .unwrap(),
        )
    }

    fn dialer(dir: &Path, body: &str) -> LocalDialer {
        LocalDialer {
            daemon: daemon_with(dir, fake_bin(dir, body)),
        }
    }

    fn alive(pid: u32) -> bool {
        // A zombie still answers kill(0); ask the child table instead where we can.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn cancel_after(ms: u64) -> CancelToken {
        let c = CancelToken::new();
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            c2.cancel();
        });
        c
    }

    #[test]
    fn local_paths_mirror_xshelld() {
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
        assert_eq!(local_mode_path(h), PathBuf::from("/h/.xshell/daemon/mode"));
        assert_eq!(
            local_pid_path(Path::new("/r/xshell/daemon.sock")),
            PathBuf::from("/r/xshell/daemon.pid")
        );
    }

    #[test]
    fn read_local_mode_values() {
        let t = tempfile::tempdir().unwrap();
        let m = t.path().join("mode");
        assert_eq!(read_local_mode(&m), None);
        std::fs::write(&m, "gui-bound\n").unwrap();
        assert_eq!(read_local_mode(&m), Some(MODE_GUI_BOUND));
        std::fs::write(&m, "persistent\n").unwrap();
        assert_eq!(read_local_mode(&m), Some(MODE_PERSISTENT));
        std::fs::write(&m, "other").unwrap();
        assert_eq!(read_local_mode(&m), None);
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
        // A child another test forks meanwhile may hold the listener until it execs: wait
        // until the socket file is really stale.
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(dir.path().join("s.sock")).is_ok() {
            assert!(Instant::now() < deadline, "the listener stays open");
            std::thread::sleep(Duration::from_millis(10));
        }
        // A stale socket file refuses: a Daemon is started, with the GUI-bound flags.
        let r = d.dial(&cancel_after(500)).map(|_| ());
        assert!(matches!(r, Err(DialError::Cancelled)), "{r:?}");
        let s = wait_starts(dir.path(), 1);
        assert_eq!(
            s,
            vec![format!(
                "serve --gui-bound --parent-pid {}",
                std::process::id()
            )]
        );
        assert!(!d.upgradable());
        // Still running: a second dial starts no other.
        let c = CancelToken::new();
        c.cancel();
        let _ = d.dial(&c);
        assert_eq!(starts(dir.path()).len(), 1);
        d.daemon.terminate(Duration::from_secs(2));
        assert_eq!(d.daemon.pid(), None);
    }

    #[test]
    fn local_dialer_spawns_persistent_flags() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        d.daemon.set_persistent(true);
        assert!(d.upgradable());
        assert!(matches!(
            d.dial(&cancel_after(500)),
            Err(DialError::Cancelled)
        ));
        assert_eq!(
            wait_starts(dir.path(), 1),
            vec!["serve --interactive-env".to_string()]
        );
        // It runs from the installed copy, in a session of its own.
        let installed = dir.path().join("home/.xshell/server/1.5.0/xshelld");
        assert_eq!(
            std::fs::read(&installed).unwrap(),
            std::fs::read(dir.path().join("xshelld")).unwrap()
        );
        let pid = d.daemon.pid().expect("a starting Persistent child");
        assert_eq!(unsafe { libc::getsid(pid as i32) }, pid as i32);
        assert_eq!(d.daemon.gui_pid(), None);
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }

    /// Quitting while a Persistent Daemon is still starting neither signals nor kills it.
    #[test]
    fn hang_up_leaves_persistent_child() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        d.daemon.set_persistent(true);
        let pid = d.daemon.ensure_spawned().unwrap();
        let t0 = Instant::now();
        d.daemon.hang_up();
        d.daemon.reap(Instant::now() + Duration::from_secs(5));
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        std::thread::sleep(Duration::from_millis(200));
        assert!(alive(pid), "the Persistent Daemon was ended at quit");
        assert!(d.daemon.ensure_spawned().is_err(), "started after quit");
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        // The background reaper collects it: kill(0) fails once nobody holds the zombie.
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(pid) {
            assert!(Instant::now() < deadline, "not reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A GUI-bound child is still ended at quit.
    #[test]
    fn hang_up_ends_gui_bound_child() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        let pid = d.daemon.ensure_spawned().unwrap();
        d.daemon.terminate(Duration::from_secs(2));
        assert!(!alive(pid));
    }

    #[test]
    fn stop_gui_child_does_not_quit() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        let first = d.daemon.ensure_spawned().unwrap();
        assert_eq!(d.daemon.gui_pid(), Some(first));
        assert!(d.daemon.stop_gui_child(Duration::from_secs(2)));
        assert!(!alive(first));
        assert_eq!(d.daemon.pid(), None);
        assert!(!d.daemon.stop_gui_child(Duration::from_secs(2)));
        // Not quitting: the next start works.
        let second = d.daemon.ensure_spawned().unwrap();
        assert_ne!(second, first);
        d.daemon.terminate(Duration::from_secs(2));
    }

    /// A Persistent child is never stopped as if it were GUI-bound.
    #[test]
    fn stop_gui_child_ignores_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        d.daemon.set_persistent(true);
        let pid = d.daemon.ensure_spawned().unwrap();
        assert!(!d.daemon.stop_gui_child(Duration::from_secs(1)));
        assert!(alive(pid));
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }

    /// A pidfile that names someone other than the socket's server is never signalled.
    #[test]
    fn stop_verified_checks_the_socket_peer() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        set_mode(dir.path(), "persistent");
        let _l = UnixListener::bind(dir.path().join("s.sock")).unwrap();
        let mut stranger = Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::write(
            dir.path().join("daemon.pid"),
            format!("{}\n", stranger.id()),
        )
        .unwrap();
        let e = d.daemon.stop_verified(Duration::from_secs(1)).unwrap_err();
        assert!(
            matches!(&e, SwitchError::Failed(m) if m.contains("serves")),
            "{e:?}"
        );
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "the stranger was signalled"
        );
        let _ = stranger.kill();
        let _ = stranger.wait();
        // Nothing listening: nothing to stop.
        drop(_l);
        std::fs::remove_file(dir.path().join("s.sock")).unwrap();
        assert_eq!(d.daemon.stop_verified(Duration::from_secs(1)), Ok(()));
        assert!(d.stop_incompatible(&CancelToken::new()).is_none());
    }

    fn set_mode(dir: &Path, mode: &str) {
        let m = local_mode_path(&dir.join("home"));
        std::fs::create_dir_all(m.parent().unwrap()).unwrap();
        std::fs::write(m, format!("{mode}\n")).unwrap();
    }

    /// Only a Persistent Daemon is stopped: a GUI-bound one belongs to an xshell window.
    #[test]
    fn stop_verified_refuses_gui_bound() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        set_mode(dir.path(), "gui-bound");
        let _l = UnixListener::bind(dir.path().join("s.sock")).unwrap();
        let mut other = Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::write(dir.path().join("daemon.pid"), format!("{}\n", other.id())).unwrap();
        assert_eq!(
            d.daemon.stop_verified(Duration::from_secs(1)),
            Err(SwitchError::OtherApp)
        );
        d.daemon.set_persistent(true);
        let e = d
            .stop_incompatible(&CancelToken::new())
            .unwrap()
            .unwrap_err();
        assert_eq!(e.code, crate::HostErrorCode::Incompatible);
        assert!(other.try_wait().unwrap().is_none());
        let _ = other.kill();
        let _ = other.wait();
    }

    /// A pinned handle never signals a process that later took the place of the pinned one.
    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_signal_never_reaches_a_replacement() {
        let mut a = Command::new("sleep").arg("30").spawn().unwrap();
        let p = Pinned::open(a.id() as i32).unwrap();
        assert!(!p.exited());
        let _ = a.kill();
        let _ = a.wait();
        assert!(p.exited());
        let mut b = Command::new("sleep").arg("30").spawn().unwrap();
        let e = p
            .signal(Path::new("/nonexistent"), libc::SIGTERM)
            .unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ESRCH));
        assert!(
            b.try_wait().unwrap().is_none(),
            "the replacement was signalled"
        );
        let _ = b.kill();
        let _ = b.wait();
    }

    /// A Unix socket server in its own process (python3), ready once `ready` exists.
    #[cfg(target_os = "linux")]
    fn socket_server(sock: &Path, ready: &Path) -> Option<std::process::Child> {
        let script = "import socket,os,sys\n\
try:\n    os.unlink(sys.argv[1])\nexcept OSError:\n    pass\n\
s=socket.socket(socket.AF_UNIX)\ns.bind(sys.argv[1])\ns.listen(8)\n\
open(sys.argv[2],'w').close()\nconns=[]\n\
while True:\n    conns.append(s.accept()[0])\n";
        let _ = std::fs::remove_file(ready);
        let c = Command::new("python3")
            .args(["-I", "-c", script])
            .arg(sock)
            .arg(ready)
            .stdin(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the socket server did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(c)
    }

    /// The verified Daemon exits and another process serves the socket before the signal:
    /// nobody is signalled.
    #[cfg(target_os = "linux")]
    #[test]
    fn stop_verified_never_signals_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let d = dialer(dir.path(), "exec sleep 30");
        set_mode(dir.path(), "persistent");
        let sock = dir.path().join("s.sock");
        let ready = dir.path().join("ready");
        let Some(mut a) = socket_server(&sock, &ready) else {
            eprintln!("skipped: no python3");
            return;
        };
        std::fs::write(dir.path().join("daemon.pid"), format!("{}\n", a.id())).unwrap();
        let b = Arc::new(Mutex::new(None::<std::process::Child>));
        let b2 = b.clone();
        let (sock2, ready2) = (sock.clone(), ready.clone());
        let a_pid = a.id() as i32;
        *d.daemon.before_signal.lock().unwrap() = Some(Box::new(move || {
            unsafe { libc::kill(a_pid, libc::SIGKILL) };
            std::thread::sleep(Duration::from_millis(100));
            *b2.lock().unwrap() = socket_server(&sock2, &ready2);
        }));
        assert_eq!(d.daemon.stop_verified(Duration::from_secs(2)), Ok(()));
        let _ = a.wait();
        let mut b = b.lock().unwrap().take().expect("the replacement");
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            b.try_wait().unwrap().is_none(),
            "the replacement was signalled"
        );
        let _ = b.kill();
        let _ = b.wait();
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
        let t0 = Instant::now();
        assert!(matches!(
            d.dial(&cancel_after(200)),
            Err(DialError::Cancelled)
        ));
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        d.daemon.terminate(Duration::from_secs(2));
    }

    #[test]
    fn gui_bound_dialer_spawn_failure_is_failed() {
        let dir = tempfile::tempdir().unwrap();
        // The child exits at once with an error.
        let d = dialer(dir.path(), "exit 1");
        let log = d.daemon.log.display().to_string();
        match d.dial(&CancelToken::new()) {
            Err(DialError::Failed { message, .. }) => {
                assert!(message.contains("exited with 1"), "{message}");
                assert!(message.contains(&log), "{message}");
            }
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("connected to nothing"),
        }
        // A binary that cannot run at all, in either mode.
        let d = LocalDialer {
            daemon: daemon_with(dir.path(), dir.path().join("absent")),
        };
        for persistent in [false, true] {
            d.daemon.set_persistent(persistent);
            match d.dial(&CancelToken::new()) {
                Err(DialError::Failed { message, .. }) => {
                    assert!(message.contains("cannot start"), "{message}")
                }
                Err(e) => panic!("{e:?}"),
                Ok(_) => panic!("connected to nothing"),
            }
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
        let bin = fake_bin(dir.path(), "exec sleep 30");
        let at = |socket: PathBuf| {
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            LocalDialer {
                daemon: Arc::new(
                    LocalDaemon::new(LocalDaemonConfig {
                        bin: bin.clone(),
                        env: vec![],
                        home,
                        socket,
                        version: "1.5.0".into(),
                    })
                    .unwrap(),
                ),
            }
        };
        if !root {
            let d = at(locked.join("s.sock"));
            assert!(matches!(
                d.dial(&CancelToken::new()),
                Err(DialError::Failed { .. })
            ));
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        // An invalid path: too long for `sun_path`.
        let d = at(dir.path().join("x".repeat(200)));
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
        let l = UnixListener::bind(d.daemon.socket()).unwrap();
        assert_eq!(unsafe { libc::listen(l.as_raw_fd(), 0) }, 0);
        let mut queued = Vec::new();
        loop {
            match crate::dial::connect_nb(d.daemon.socket()) {
                Ok(crate::dial::Pending::Done(s)) => queued.push(s),
                Ok(crate::dial::Pending::InProgress(_)) => panic!("unexpected in-progress connect"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
            assert!(queued.len() < 1000, "the backlog never filled");
        }
        let r = d.dial(&cancel_after(300));
        assert!(matches!(r, Err(DialError::Cancelled)), "{:?}", r.err());
        assert!(starts(dir.path()).is_empty());
        drop(l);
    }
}

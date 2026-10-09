//! `xshelld serve`: the socket server, the Terminal registry and the Daemon lifecycle.
//!
//! Threads only, blocking std I/O. Lock order, never reversed: `Registry` →
//! `Terminal.record` → `Terminal.io` → `Terminal.out` → `Outbox`; `Terminal.life` and
//! `Terminal.input` are taken last and alone; `Terminal.status` (the Agent Status) is taken
//! last, and nothing is locked while it is held. No lock is held across a blocking PTY or
//! socket write: writers own their sockets, input threads own PTY writers.

mod agent;
mod calls;
mod conn;
mod orphans;
pub use orphans::Cleanup;
mod outbox;
pub mod parent;
mod registry;
mod relaunch;
mod role;
pub use role::Role;
mod size;
mod terminal;

use crate::paths::{check_socket_path_len, ensure_private_dir, write_mode, Mode, Paths};
use registry::{Daemon, Registry};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::terminal::state::Leader;
use xshell_core::HostCtx;

pub(crate) type ConnId = u64;

#[derive(Debug, Clone)]
pub struct Config {
    pub home: PathBuf,
    pub paths: Paths,
    /// Exit after this long with no Terminals and no connections.
    pub idle_timeout: Duration,
    /// SIGHUP → SIGKILL delay when ending a Terminal.
    pub kill_grace: Duration,
    /// A connection whose socket accepts no bytes for this long is dropped.
    pub write_stall_timeout: Duration,
    /// The first message must arrive within this.
    pub hello_timeout: Duration,
    pub replay_capacity: usize,
    /// Queued output bytes per connection before output is dropped with a notice.
    pub conn_output_cap: usize,
    /// Queued bytes of any kind per connection before the connection is dropped.
    pub conn_total_cap: usize,
    /// Gap between the two size changes of a redraw nudge.
    pub nudge_delay: Duration,
    pub max_calls_per_conn: usize,
    /// Size changes are persisted at most this often per Terminal.
    pub resize_persist_delay: Duration,
    /// Largest spec + metadata one Terminal may carry, serialized.
    pub max_terminal_bytes: usize,
    /// Largest serialized `terminals` list; keeps it far below the 64 MiB frame limit.
    pub max_list_bytes: usize,
    /// The agent hook client agents run (`<exe> event …`): this executable by default.
    /// `None` launches agents without hooks, so they report no Agent Status.
    pub event_exe: Option<PathBuf>,
    /// Run for the xshell app on this machine (ADR-0005): no idle exit, no `daemon.upgrade`,
    /// and the mode marker says `gui-bound`. `None`: a Persistent Daemon.
    pub gui_bound: Option<GuiBound>,
    /// Stops the server from another thread, also while it is still starting: a trigger
    /// during restore abandons the start (Terminals ended, state file kept).
    pub abort: Option<Arc<StopLatch>>,
    /// Test hook: replaces crash-leftover cleanup during restore.
    #[doc(hidden)]
    pub cleanup_override: Option<fn(&Leader, Duration) -> Cleanup>,
    /// Test hook: runs at the [`TestPoint`]s of a Terminal's start, exit and Relaunch.
    #[doc(hidden)]
    pub test_hook: Option<TestHook>,
}

/// Where a [`TestHook`] runs. Points other than [`TestPoint::ExitHandled`] run on the thread
/// doing the work, with no lock held, so a hook may block to order a race.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestPoint {
    /// A Relaunch was accepted and is about to start its worker thread (registry locked: do
    /// not block). Returning `true` makes the thread start fail.
    StartWorker,
    /// The Relaunch worker signalled the old process.
    Signalled,
    /// The Relaunch worker stopped waiting for the old process; `exited` says whether it
    /// ended in time. Returning `true` treats it as a timeout.
    Waited { exited: bool },
    /// A Relaunch started the replacement process `pid` and persisted its identity; none of
    /// its threads run yet (registry locked: do not block on the Daemon).
    ReplacementSpawned { pid: u32 },
    /// A Terminal's reader and waiter threads are running and its input thread is next.
    /// Returning `true` makes that thread start fail.
    StartInput,
    /// A process's exit was handled: published, held for a Relaunch, or dropped because its
    /// Terminal was never listed.
    ExitHandled { pid: Option<u32> },
    /// Restore is about to relaunch this persisted Terminal (registry locked: the Daemon does
    /// not serve yet, so a hook may block to stall the start).
    Restore,
}

/// `serve --gui-bound --parent-pid N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuiBound {
    pub parent_pid: u32,
}

/// Stops a `serve` from any thread (signals, the parent watch): before the server is up the
/// trigger is remembered, so a start in progress gives up; afterwards it is a
/// [`Stopper::shutdown`].
#[derive(Default)]
pub struct StopLatch(Mutex<(bool, Option<Weak<Daemon>>)>);

impl StopLatch {
    pub fn trigger(&self) {
        let d = {
            let mut g = self.0.lock().unwrap();
            g.0 = true;
            g.1.as_ref().and_then(Weak::upgrade)
        };
        if let Some(d) = d {
            d.exit(ExitReason::Shutdown);
        }
    }

    pub fn is_set(&self) -> bool {
        self.0.lock().unwrap().0
    }

    /// From now on a trigger shuts `h` down; at once when one came already.
    fn arm(&self, h: &ServerHandle) {
        let set = {
            let mut g = self.0.lock().unwrap();
            g.1 = Some(Arc::downgrade(&h.d));
            g.0
        };
        if set {
            h.d.exit(ExitReason::Shutdown);
        }
    }
}

impl std::fmt::Debug for StopLatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StopLatch({})", self.is_set())
    }
}

/// A test hook: called with the Terminal and the point reached.
#[doc(hidden)]
#[derive(Clone)]
pub struct TestHook(pub Arc<dyn Fn(Uuid, TestPoint) -> bool + Send + Sync>);

impl std::fmt::Debug for TestHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestHook")
    }
}

impl Config {
    pub fn new(home: PathBuf, paths: Paths) -> Self {
        Self {
            home,
            paths,
            idle_timeout: Duration::from_secs(3600),
            kill_grace: Duration::from_secs(2),
            write_stall_timeout: Duration::from_secs(60),
            hello_timeout: Duration::from_secs(30),
            replay_capacity: xshell_core::terminal::replay::DEFAULT_REPLAY_CAPACITY,
            conn_output_cap: 8 * 1024 * 1024,
            conn_total_cap: 16 * 1024 * 1024,
            nudge_delay: Duration::from_millis(60),
            max_calls_per_conn: 32,
            resize_persist_delay: Duration::from_secs(1),
            max_terminal_bytes: 256 * 1024,
            max_list_bytes: 16 * 1024 * 1024,
            event_exe: std::env::current_exe().ok(),
            gui_bound: None,
            abort: None,
            cleanup_override: None,
            test_hook: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// No Terminals and no connections for `idle_timeout`.
    Idle,
    /// `daemon.upgrade`: state persisted, Terminals ended.
    Upgrade,
    /// SIGTERM/SIGINT/SIGHUP or [`ServerHandle::shutdown`]: Terminals ended, state file kept.
    Shutdown,
}

#[derive(Debug)]
pub enum StartError {
    /// Another `serve` holds the lock.
    AlreadyRunning,
    /// [`Config::abort`] fired while starting: the restored Terminals were ended and the
    /// state file kept.
    Aborted,
    Io(io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::AlreadyRunning => write!(f, "another xshelld serve is running"),
            StartError::Aborted => write!(f, "stopped while starting"),
            StartError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for StartError {
    fn from(e: io::Error) -> Self {
        StartError::Io(e)
    }
}

pub struct Server;

impl Server {
    /// Lock, bind, restore the persisted Terminals, then serve in background threads.
    pub fn start(cfg: Config) -> Result<ServerHandle, StartError> {
        let paths = cfg.paths.clone();
        check_socket_path_len(&paths.socket)?;
        ensure_private_dir(paths.socket_dir())?;
        for p in [&paths.state, &paths.log] {
            if let Some(dir) = p.parent() {
                ensure_private_dir(dir)?;
            }
        }
        ensure_private_dir(&paths.tmp)?;

        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lock)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(StartError::AlreadyRunning),
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let mut starting = Starting {
            paths: &paths,
            lock: Some(lock),
            pid: false,
            socket: false,
        };
        // Holding the lock, record how this Daemon runs: `connect` starts none while xshell
        // owns it, and a Persistent `serve` hands it back.
        let mode = if cfg.gui_bound.is_some() {
            Mode::GuiBound
        } else {
            Mode::Persistent
        };
        // Without it, `connect` would start or refuse Daemons against how this one runs.
        write_mode(&paths.mode, mode).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot write {}: {e}", paths.mode.display()),
            )
        })?;
        // Holding the lock makes the socket ours: a leftover one is stale.
        let mut pidf = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&paths.pid)?;
        starting.pid = true;
        writeln!(pidf, "{}", std::process::id())?;
        match fs::remove_file(&paths.socket) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let listener = UnixListener::bind(&paths.socket)?;
        starting.socket = true;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;

        let ctx = HostCtx::with_home(cfg.home.clone(), paths.tmp.clone());
        xshell_core::files::cleanup_old_dropped_files(&ctx);
        let hooks = agent::hooks(&cfg);
        let d = Arc::new(Daemon {
            cfg,
            ctx: Arc::new(ctx),
            reg: Mutex::new(Registry::default()),
            exiting: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            exit: Mutex::new(None),
            exit_cv: Condvar::new(),
            lock_file: Mutex::new(starting.lock.take()),
            next_conn: AtomicU64::new(1),
            hooks,
            // Runs are told apart across restarts too: a hook of the previous Daemon's
            // process never matches a restored Terminal's run.
            next_run: AtomicU64::new(registry::now_ms()),
            escalations: Default::default(),
        });
        if !d.restore() {
            crate::log!("INFO", "stopped while restoring Terminals; exiting");
            d.exit(ExitReason::Shutdown);
            return Err(StartError::Aborted);
        }
        let da = d.clone();
        let ds = d.clone();
        let threads = std::thread::Builder::new()
            .name("accept".into())
            .spawn(move || da.accept_loop(listener))
            .and_then(|_| {
                std::thread::Builder::new()
                    .name("supervisor".into())
                    .spawn(move || ds.supervise())
            });
        if let Err(e) = threads {
            d.exit(ExitReason::Shutdown);
            return Err(e.into());
        }
        crate::log!(
            "INFO",
            "xshelld {} serving {} (pid {})",
            env!("CARGO_PKG_VERSION"),
            paths.socket.display(),
            std::process::id()
        );
        drop(starting);
        Ok(ServerHandle {
            socket: paths.socket,
            d,
        })
    }
}

/// A start in progress: if it fails, the pidfile and socket it created are removed and the
/// lock is released, explicitly (a child forked meanwhile may still share the lock's file).
struct Starting<'a> {
    paths: &'a Paths,
    /// Taken by the Daemon once it exists; from then on its `exit` cleans up.
    lock: Option<fs::File>,
    pid: bool,
    socket: bool,
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        let Some(lock) = self.lock.take() else {
            return;
        };
        if self.socket {
            let _ = fs::remove_file(&self.paths.socket);
        }
        if self.pid {
            let _ = fs::remove_file(&self.paths.pid);
        }
        let _ = lock.unlock();
    }
}

/// A running server. Dropping it without waiting shuts the server down (Terminals ended,
/// state file kept), so a failing test never leaves processes behind.
pub struct ServerHandle {
    pub socket: PathBuf,
    d: Arc<Daemon>,
}

/// Requests a shutdown from another thread (the signal handler).
#[derive(Clone)]
pub struct Stopper(Arc<Daemon>);

impl Stopper {
    pub fn shutdown(&self) {
        self.0.exit(ExitReason::Shutdown);
    }
}

impl ServerHandle {
    pub fn wait(self) -> ExitReason {
        let mut g = self.d.exit.lock().unwrap();
        loop {
            if let Some(r) = *g {
                return r;
            }
            g = self.d.exit_cv.wait(g).unwrap();
        }
    }

    pub fn wait_timeout(&self, d: Duration) -> Option<ExitReason> {
        let deadline = Instant::now() + d;
        let mut g = self.d.exit.lock().unwrap();
        loop {
            if let Some(r) = *g {
                return Some(r);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            g = self.d.exit_cv.wait_timeout(g, left).unwrap().0;
        }
    }

    /// Like SIGTERM: end every Terminal, keep the state file, stop serving.
    pub fn shutdown(self) -> ExitReason {
        self.d.exit(ExitReason::Shutdown);
        self.wait()
    }

    /// Test hook: connections remembered by the Terminals' size arbiters, summed.
    #[doc(hidden)]
    pub fn size_tracked(&self) -> usize {
        let reg = self.d.reg.lock().unwrap();
        reg.terminals.values().map(|t| t.size_tracked()).sum()
    }

    /// Test hook: connections attached to the Terminals' output, summed.
    #[doc(hidden)]
    pub fn attached(&self) -> usize {
        let reg = self.d.reg.lock().unwrap();
        reg.terminals.values().map(|t| t.attached()).sum()
    }

    /// Test hook: a connection served in this process with `role`, as a transport other than
    /// the socket would hand it over. Returns the client's end.
    #[doc(hidden)]
    pub fn connect_in_process(&self, role: Role) -> io::Result<UnixStream> {
        let (client, server) = UnixStream::pair()?;
        let id = self.d.next_conn.fetch_add(1, Ordering::SeqCst);
        let d = self.d.clone();
        std::thread::Builder::new()
            .name(format!("conn-{id}-r"))
            .spawn(move || conn::handle(d, server, id, role))?;
        Ok(client)
    }

    pub fn stopper(&self) -> Stopper {
        Stopper(self.d.clone())
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if self.d.exit.lock().unwrap().is_none() {
            self.d.exit(ExitReason::Shutdown);
        }
    }
}

/// `serve` as a process: shut down orderly when `stop` fires (see [`watch_exit_signals`]).
pub fn run_serve(mut cfg: Config, stop: Arc<StopLatch>) -> i32 {
    cfg.abort = Some(stop.clone());
    let h = match Server::start(cfg) {
        Ok(h) => h,
        Err(StartError::AlreadyRunning) => {
            crate::log!("INFO", "another xshelld serve holds the lock; exiting");
            hold_loser_for_tests();
            return 3;
        }
        Err(StartError::Aborted) => return 0,
        Err(e) => {
            crate::log!("ERROR", "cannot start: {e}");
            return 1;
        }
    };
    stop.arm(&h);
    let reason = h.wait();
    crate::log!("INFO", "exiting: {reason:?}");
    0
}

/// Fire `stop` on SIGTERM, SIGINT or SIGHUP, from a thread of its own. The caller must have
/// blocked those signals (see [`block_exit_signals`]) before starting any thread.
pub fn watch_exit_signals(stop: Arc<StopLatch>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || loop {
            let sig = wait_exit_signal();
            crate::log!("INFO", "signal {sig}: shutting down");
            stop.trigger();
        })?;
    Ok(())
}

/// Test hook: with `XSHELLD_TEST_HOLD_LOSER=<path>`, a `serve` that lost the lock waits
/// (at most 30 s) until `<path>` exists before exiting, so a test can order its exit after
/// `connect` has already bridged to the winner.
fn hold_loser_for_tests() {
    let Some(p) = std::env::var_os("XSHELLD_TEST_HOLD_LOSER") else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::path::Path::new(&p).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn exit_signal_set() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

/// Block the exit signals in this thread and every thread it spawns afterwards, so only the
/// `sigwait` thread sees them. Children get a clean mask: std and portable-pty reset it.
pub fn block_exit_signals() {
    let set = exit_signal_set();
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn wait_exit_signal() -> i32 {
    let set = exit_signal_set();
    let mut sig: libc::c_int = 0;
    loop {
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
            return sig;
        }
    }
}

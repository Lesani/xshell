//! Harness for driving the Desktop's host link (xshell-hostlink) against the real `xshelld`.
#![allow(dead_code)]

use crate::common::{alive, bin, DaemonGuard, TestHome};
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_hostlink::dial::{connect_unix, CONNECT_TIMEOUT};
use xshell_hostlink::transport::sh_quote;
use xshell_hostlink::{
    BinarySource, CancelToken, DialError, Dialed, Dialer, FileSource, HostConfig, HostError,
    HostHandle, HostStatus, LocalShellTransport, Manager, ManagerConfig, Observer, StatusKind,
    TermSink, Transport, TransportFactory,
};
use xshell_protocol::msg::{OpenSpec, TerminalInfo};
use xshelld::server::ServerHandle;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ID: &str = "h_test0001";
pub const W: Duration = Duration::from_secs(10);

// ── Observer ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct Recorder {
    pub statuses: Mutex<Vec<HostStatus>>,
    pub lists: Mutex<Vec<Vec<TerminalInfo>>>,
    // One condvar per mutex: macOS refuses a condvar used with two.
    cv: Condvar,
    list_cv: Condvar,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn count(&self) -> usize {
        self.statuses.lock().unwrap().len()
    }

    /// The first status at index ≥ `from` matching `pred`; returns it and its index.
    pub fn wait_status_from(
        &self,
        from: usize,
        what: &str,
        pred: impl Fn(&HostStatus) -> bool,
    ) -> (usize, HostStatus) {
        let deadline = Instant::now() + W;
        let mut g = self.statuses.lock().unwrap();
        loop {
            if let Some(i) = (from..g.len()).find(|&i| pred(&g[i])) {
                return (i, g[i].clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "no status {what}; got {:#?}",
                &g[from.min(g.len())..]
            );
            g = self.cv.wait_timeout(g, left).unwrap().0;
        }
    }

    pub fn wait_status(&self, what: &str, pred: impl Fn(&HostStatus) -> bool) -> HostStatus {
        self.wait_status_from(0, what, pred).1
    }

    /// Wait for a list (received at or after list index `from`) matching `pred`.
    pub fn wait_list_from(
        &self,
        from: usize,
        what: &str,
        pred: impl Fn(&[TerminalInfo]) -> bool,
    ) -> Vec<TerminalInfo> {
        let deadline = Instant::now() + W;
        let mut g = self.lists.lock().unwrap();
        loop {
            if let Some(l) = g.iter().skip(from).find(|l| pred(l)) {
                return l.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no list {what}; got {:#?}", g.last());
            g = self.list_cv.wait_timeout(g, left).unwrap().0;
        }
    }

    pub fn wait_list(
        &self,
        what: &str,
        pred: impl Fn(&[TerminalInfo]) -> bool,
    ) -> Vec<TerminalInfo> {
        self.wait_list_from(0, what, pred)
    }

    pub fn list_count(&self) -> usize {
        self.lists.lock().unwrap().len()
    }
}

impl Observer for Recorder {
    fn status(&self, s: &HostStatus) {
        self.statuses.lock().unwrap().push(s.clone());
        self.cv.notify_all();
    }
    fn terminals(&self, _host: &str, list: &[TerminalInfo]) {
        self.lists.lock().unwrap().push(list.to_vec());
        self.list_cv.notify_all();
    }
}

pub fn usable(s: &HostStatus) -> bool {
    matches!(s.status, StatusKind::Connected | StatusKind::UpgradePending)
}

// ── Sinks ─────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct VecSink {
    pub data: Mutex<Vec<u8>>,
    pub exits: Mutex<Vec<(i32, u64)>>,
}

impl TermSink for VecSink {
    fn data(&self, b: &[u8]) -> bool {
        self.data.lock().unwrap().extend_from_slice(b);
        true
    }
    fn exit(&self, code: i32, bytes: u64) {
        self.exits.lock().unwrap().push((code, bytes));
    }
}

impl VecSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    pub fn text_from(&self, from: usize) -> String {
        let d = self.data.lock().unwrap();
        String::from_utf8_lossy(&d[from.min(d.len())..]).into_owned()
    }

    /// Wait until the output after byte `from` contains `needle`.
    pub fn wait_from(&self, from: usize, needle: &str) -> String {
        let deadline = Instant::now() + W;
        loop {
            let t = self.text_from(from);
            if t.contains(needle) {
                return t;
            }
            assert!(Instant::now() < deadline, "no {needle:?} in {t:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn wait_exit(&self) -> (i32, u64) {
        let deadline = Instant::now() + W;
        loop {
            if let Some(e) = self.exits.lock().unwrap().first() {
                return *e;
            }
            assert!(Instant::now() < deadline, "no exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

// ── Blocking wrappers over the callback API ───────────────────────────────

fn sync<T: Send + 'static>(f: impl FnOnce(Box<dyn FnOnce(T) + Send>)) -> T {
    let (tx, rx) = mpsc::channel();
    f(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    rx.recv_timeout(Duration::from_secs(40)).expect("no reply")
}

pub fn call(h: &HostHandle, method: &str, params: Value) -> Result<Value, HostError> {
    sync(|w| h.call(method.into(), params, w))
}

pub fn open(h: &HostHandle, t: Uuid, launch: LaunchSpec, sink: Arc<VecSink>) -> Option<u32> {
    sync(|w| {
        h.term_open(
            OpenSpec {
                terminal: t,
                launch,
                cols: 80,
                rows: 24,
                meta: Map::new(),
            },
            sink,
            w,
        )
    })
    .unwrap_or_else(|e| panic!("term_open: {e:?}"))
}

pub fn attach(h: &HostHandle, t: Uuid, sink: Arc<VecSink>) -> Option<i32> {
    sync(|w| h.term_attach(t, sink, w)).unwrap_or_else(|e| panic!("term_attach: {e:?}"))
}

pub fn close(h: &HostHandle, t: Uuid) {
    sync(|w| h.term_close(t, w)).unwrap_or_else(|e| panic!("term_close: {e:?}"));
}

pub fn relaunch(h: &HostHandle, t: Uuid, skip: bool) -> Result<Option<u32>, HostError> {
    sync(|w| h.term_relaunch(t, skip, w))
}

pub fn upgrade(h: &HostHandle) -> Result<Value, HostError> {
    sync(|w| h.upgrade(w))
}

/// Type `echo ab""c` and wait for the joined word in the sink.
pub fn marker(h: &HostHandle, sink: &VecSink, t: Uuid, word: &str) {
    let from = sink.len();
    let (a, b) = word.split_at(word.len() / 2);
    h.term_input(t, format!("echo {a}\"\"{b}\n")).unwrap();
    sink.wait_from(from, word);
}

// ── Transports and fixtures ───────────────────────────────────────────────

pub struct Fixed(pub Arc<dyn Fn() -> Box<dyn Transport> + Send + Sync>);

impl TransportFactory for Fixed {
    fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
        (self.0)()
    }
}

/// `sh -c` on this machine with the test home's environment.
pub fn local_factory(h: &TestHome) -> Arc<dyn TransportFactory> {
    let env: Vec<(OsString, OsString)> = vec![
        ("HOME".into(), h.home().into()),
        ("XDG_RUNTIME_DIR".into(), h.run().into()),
        ("XSHELLD_LOGIN_ENV".into(), "0".into()),
        ("XSHELLD_IDLE_TIMEOUT_MS".into(), "5000".into()),
    ];
    Arc::new(Fixed(Arc::new(move || {
        Box::new(LocalShellTransport { env: env.clone() })
    })))
}

pub fn manager_config(
    transports: Arc<dyn TransportFactory>,
    binaries: Arc<dyn BinarySource>,
    rec: Arc<Recorder>,
) -> ManagerConfig {
    let mut c = ManagerConfig::new(VERSION, transports, binaries, rec);
    c.backoff_unit = Duration::from_millis(100);
    c.hello_timeout = Duration::from_secs(15);
    c
}

/// The daemon command override that runs the workspace-built binary.
pub fn override_cmd() -> String {
    sh_quote(bin())
}

pub fn host_config(daemon_command: Option<String>) -> HostConfig {
    HostConfig {
        id: ID.into(),
        name: "Test".into(),
        ssh_target: "local".into(),
        color: None,
        daemon_command,
        launch_prefixes: Default::default(),
    }
}

/// A Desktop (manager) connected to a test home's Daemon. Dropping it shuts the manager
/// down, then ends the Daemon through its pidfile (SIGTERM, then SIGKILL), then removes the
/// home: on assertion failures too.
pub struct Desk {
    pub m: Manager,
    pub rec: Arc<Recorder>,
}

impl Desk {
    pub fn new(cfg: ManagerConfig, rec: Arc<Recorder>, host: HostConfig) -> Desk {
        let m = Manager::new(cfg);
        m.configure(vec![host]).unwrap();
        Desk { m, rec }
    }

    pub fn host(&self) -> Arc<HostHandle> {
        self.m.host(ID).expect("host configured")
    }

    pub fn wait_usable(&self) -> HostStatus {
        self.rec.wait_status("connected", usable)
    }
}

/// How the Desktop reaches the test home's Daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// `xshelld connect` through `sh -c` and the daemon-command override; it starts a
    /// `serve` process.
    Command,
    /// The Daemon's local socket, served in this process.
    Socket,
}

pub enum Daemon {
    /// Ended through its pidfile on drop.
    Spawned(DaemonGuard),
    /// Shut down on drop. Never guard it by pidfile: that pid is this test process.
    InProcess(ServerHandle),
}

/// Every connection a socket Desktop dialed, in order (clones of its end of the socket).
#[derive(Default)]
pub struct Tap {
    streams: Mutex<Vec<UnixStream>>,
    /// Per connection: set when the link's reader dropped its read half (its thread ended).
    readers_done: Mutex<Vec<Arc<AtomicBool>>>,
}

impl Tap {
    pub fn dials(&self) -> usize {
        self.streams.lock().unwrap().len()
    }

    /// Cut the latest connection, as a dropped ssh would end.
    pub fn sever(&self) {
        let s = self.streams.lock().unwrap();
        s.last()
            .expect("a dialed connection")
            .shutdown(Shutdown::Both)
            .unwrap();
    }

    /// Wait (bounded) until connection `i` reads EOF, draining what is still buffered.
    /// Polls and reads without blocking, so the socket's own options stay untouched. Only
    /// a read of 0 bytes is EOF; any error but a retryable one fails.
    pub fn reads_eof(&self, i: usize, within: Duration) -> Result<(), String> {
        use std::io::ErrorKind::{Interrupted, TimedOut, WouldBlock};
        let fd = self.streams.lock().unwrap()[i].try_clone().unwrap();
        let deadline = Instant::now() + within;
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            let mut p = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut p, 1, 50) };
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == Interrupted {
                    continue;
                }
                return Err(format!("poll: {e}"));
            }
            if r == 0 {
                continue;
            }
            let n = unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                return Ok(());
            }
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if !matches!(e.kind(), WouldBlock | TimedOut | Interrupted) {
                    return Err(format!("recv: {e}"));
                }
            }
        }
        Err(format!("no EOF within {within:?}"))
    }

    /// Wait (bounded) until connection `i`'s link reader has ended.
    pub fn reader_ended(&self, i: usize, within: Duration) -> bool {
        let done = self.readers_done.lock().unwrap()[i].clone();
        let deadline = Instant::now() + within;
        while !done.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

/// The link's read half; dropping it marks the reader thread as ended.
struct TapRead {
    inner: Box<dyn std::io::Read + Send>,
    done: Arc<AtomicBool>,
}

impl std::io::Read for TapRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Drop for TapRead {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

struct TapDialer {
    path: PathBuf,
    tap: Arc<Tap>,
}

impl Dialer for TapDialer {
    fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError> {
        let s = connect_unix(&self.path, cancel, CONNECT_TIMEOUT)?;
        let failed = |e: std::io::Error| DialError::failed(e.to_string(), None);
        self.tap
            .streams
            .lock()
            .unwrap()
            .push(s.try_clone().map_err(failed)?);
        let mut d = Dialed::from_unix_stream(s, cancel).map_err(failed)?;
        let done = Arc::new(AtomicBool::new(false));
        self.tap.readers_done.lock().unwrap().push(done.clone());
        d.io.read = Box::new(TapRead {
            inner: d.io.read,
            done,
        });
        Ok(d)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// Every Host is reached directly on the Daemon's socket, whatever its configuration says.
pub struct SocketFactory {
    pub path: PathBuf,
    pub tap: Arc<Tap>,
}

impl TransportFactory for SocketFactory {
    fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
        // Never used: a direct Host runs no command.
        Box::new(LocalShellTransport::default())
    }

    fn direct(&self, _: &HostConfig) -> Option<Box<dyn Dialer>> {
        Some(Box::new(TapDialer {
            path: self.path.clone(),
            tap: self.tap.clone(),
        }))
    }
}

/// Where the current connection stood: its transport process, or its dial.
#[derive(Debug, Clone, Copy)]
pub enum Mark {
    Pid(i32),
    /// The number of dials so far; the connection is the last of them.
    Dial(usize),
}

pub struct Fx {
    pub a: Desk,
    pub daemon: Daemon,
    pub home: TestHome,
    pub tap: Option<Arc<Tap>>,
    via: Via,
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.a.m.shutdown();
    }
}

impl Fx {
    /// A Desktop on the daemon-command override (no install).
    pub fn new() -> Fx {
        Self::with(|_| {})
    }

    pub fn with(tweak: impl FnOnce(&mut ManagerConfig)) -> Fx {
        Self::with_via(Via::Command, tweak)
    }

    pub fn via(via: Via) -> Fx {
        Self::with_via(via, |_| {})
    }

    pub fn with_via(via: Via, tweak: impl FnOnce(&mut ManagerConfig)) -> Fx {
        let home = TestHome::new();
        match via {
            Via::Command => {
                let guard = DaemonGuard::new(&home);
                let a = Self::desk(&home, tweak);
                Fx {
                    a,
                    daemon: Daemon::Spawned(guard),
                    home,
                    tap: None,
                    via,
                }
            }
            Via::Socket => {
                let server = crate::common::start(&home, |_| {});
                let tap = Arc::new(Tap::default());
                let a = socket_desk(&server.socket, tap.clone(), tweak);
                Fx {
                    a,
                    daemon: Daemon::InProcess(server),
                    home,
                    tap: Some(tap),
                    via,
                }
            }
        }
    }

    pub fn desk(home: &TestHome, tweak: impl FnOnce(&mut ManagerConfig)) -> Desk {
        let rec = Recorder::new();
        let mut cfg = manager_config(
            local_factory(home),
            Arc::new(FileSource(bin().into())),
            rec.clone(),
        );
        tweak(&mut cfg);
        Desk::new(cfg, rec, host_config(Some(override_cmd())))
    }

    /// A second Desktop, reaching the same Daemon the same way.
    pub fn another_desk(&self, tweak: impl FnOnce(&mut ManagerConfig)) -> Desk {
        match &self.daemon {
            Daemon::Spawned(_) => Self::desk(&self.home, tweak),
            Daemon::InProcess(s) => socket_desk(&s.socket, Arc::new(Tap::default()), tweak),
        }
    }

    pub fn project(&self) -> LaunchSpec {
        crate::common::sh_spec(&self.home.project("p"))
    }

    pub fn via_socket(&self) -> bool {
        self.via == Via::Socket
    }

    fn tap(&self) -> &Tap {
        self.tap.as_deref().expect("a socket fixture")
    }

    /// The `serve` process's pid; `None` in process.
    pub fn daemon_pid(&self) -> Option<i32> {
        match &self.daemon {
            Daemon::Spawned(g) => g.pid(),
            Daemon::InProcess(_) => None,
        }
    }

    /// Desktop A's current connection.
    pub fn mark(&self) -> Mark {
        match self.via {
            Via::Command => Mark::Pid(self.a.host().child_pid().expect("transport pid") as i32),
            Via::Socket => Mark::Dial(self.tap().dials()),
        }
    }

    /// Cut Desktop A's connection from under it: kill the transport's whole process group,
    /// or shut its socket down.
    pub fn sever(&self) -> Mark {
        let m = self.mark();
        match m {
            Mark::Pid(pid) => unsafe {
                libc::kill(-pid, libc::SIGKILL);
            },
            Mark::Dial(_) => self.tap().sever(),
        }
        m
    }

    /// Desktop A connected anew since `m`.
    pub fn redialed_since(&self, m: Mark) -> bool {
        match m {
            Mark::Pid(pid) => self.a.host().child_pid() != Some(pid as u32),
            Mark::Dial(n) => self.tap().dials() > n,
        }
    }

    /// The connection at `m` is gone: the transport reaped, or the socket shut down.
    pub fn assert_conn_gone(&self, m: Mark) {
        match m {
            Mark::Pid(pid) => assert!(reaped(pid), "transport {pid} survived or was not reaped"),
            Mark::Dial(n) => {
                if let Err(e) = self.tap().reads_eof(n - 1, Duration::from_secs(2)) {
                    panic!("connection {n} is not closed: {e}");
                }
            }
        }
    }

    /// The link reader of the connection at `m` ended (a socket connection only: a
    /// transport's reader ends with the killed process, as `assert_conn_gone` checks).
    pub fn assert_reader_ended(&self, m: Mark) {
        if let Mark::Dial(n) = m {
            assert!(
                self.tap().reader_ended(n - 1, Duration::from_secs(2)),
                "the reader of connection {n} did not end"
            );
        }
    }

    /// Nothing connected after `m` (or what did is gone).
    pub fn assert_no_new_conn(&self, m: Mark, handle: &HostHandle) {
        match m {
            Mark::Pid(pid) => match handle.child_pid() {
                Some(p) if p as i32 != pid => {
                    assert!(wait_dead(p as i32), "a new transport {p} runs")
                }
                _ => {}
            },
            Mark::Dial(n) => assert_eq!(self.tap().dials(), n, "a new connection was dialed"),
        }
    }
}

pub fn socket_desk(
    socket: &std::path::Path,
    tap: Arc<Tap>,
    tweak: impl FnOnce(&mut ManagerConfig),
) -> Desk {
    let rec = Recorder::new();
    let factory = Arc::new(SocketFactory {
        path: socket.into(),
        tap,
    });
    let mut cfg = manager_config(factory, Arc::new(FileSource(bin().into())), rec.clone());
    tweak(&mut cfg);
    // The Daemon command is ignored for a direct Host; changing it still replaces the
    // configuration, as the bodies shared with the command transport expect.
    Desk::new(cfg, rec, host_config(Some(override_cmd())))
}

/// Gone and reaped: `kill(0)` fails only once nobody holds the zombie.
pub fn reaped(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(pid, 0) } == 0 {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

pub fn wait_dead(pid: i32) -> bool {
    crate::common::wait_dead(pid, Duration::from_secs(2))
}

pub fn pid_alive(pid: i32) -> bool {
    alive(pid)
}

// ── The Local Host in a GUI-bound Daemon ──────────────────────────────────

/// The app's Local Host: the user's Daemon socket, starting the workspace's `xshelld` as a
/// GUI-bound child of this test process when nothing listens there.
pub struct LocalFactory {
    pub socket: PathBuf,
    pub daemon: Arc<xshell_hostlink::GuiBoundDaemon>,
}

impl TransportFactory for LocalFactory {
    fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
        Box::new(LocalShellTransport::default())
    }

    fn direct(&self, cfg: &HostConfig) -> Option<Box<dyn Dialer>> {
        (cfg.id == xshell_hostlink::LOCAL_HOST_ID).then(|| {
            Box::new(xshell_hostlink::GuiBoundDialer {
                socket: self.socket.clone(),
                daemon: self.daemon.clone(),
            }) as Box<dyn Dialer>
        })
    }
}

/// A Desktop in local Daemon mode, as `src-tauri` sets it up. Dropping it quits like the
/// app does.
pub struct LocalDesk {
    pub m: Manager,
    pub rec: Arc<Recorder>,
    pub daemon: Arc<xshell_hostlink::GuiBoundDaemon>,
}

impl LocalDesk {
    pub fn new(h: &TestHome) -> LocalDesk {
        let env: Vec<(OsString, OsString)> = vec![
            ("HOME".into(), h.home().into()),
            ("XDG_RUNTIME_DIR".into(), h.run().into()),
            ("XSHELLD_LOGIN_ENV".into(), "0".into()),
        ];
        let daemon = Arc::new(
            xshell_hostlink::GuiBoundDaemon::new(bin().into(), env, h.paths().log).unwrap(),
        );
        let socket = xshell_hostlink::local::local_socket_path(&h.home(), Some(&h.run()));
        let rec = Recorder::new();
        let factory = Arc::new(LocalFactory {
            socket,
            daemon: daemon.clone(),
        });
        let m = Manager::new(manager_config(
            factory,
            Arc::new(FileSource(bin().into())),
            rec.clone(),
        ));
        m.set_local(HostConfig {
            id: xshell_hostlink::LOCAL_HOST_ID.into(),
            name: "local".into(),
            ssh_target: String::new(),
            color: None,
            daemon_command: None,
            launch_prefixes: Default::default(),
        });
        LocalDesk { m, rec, daemon }
    }

    pub fn host(&self) -> Arc<HostHandle> {
        self.m
            .host(xshell_hostlink::LOCAL_HOST_ID)
            .expect("local host")
    }

    pub fn wait_usable(&self) -> HostStatus {
        self.rec.wait_status("connected", usable)
    }

    /// Quit as the app does: SIGTERM our Daemon, stop the links, wait for it, then kill.
    pub fn quit(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        self.daemon.hang_up();
        self.m.shutdown();
        self.daemon.reap(deadline);
    }
}

impl Drop for LocalDesk {
    fn drop(&mut self) {
        self.quit();
    }
}

//! Harness for driving the Desktop's host link (xshell-hostlink) against the real `xshelld`.
#![allow(dead_code)]

use crate::common::{alive, bin, DaemonGuard, TestHome};
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_core::protocol::msg::{OpenSpec, TerminalInfo};
use xshell_hostlink::transport::sh_quote;
use xshell_hostlink::{
    BinarySource, FileSource, HostConfig, HostError, HostHandle, HostStatus, LocalShellTransport,
    Manager, ManagerConfig, Observer, StatusKind, TermSink, Transport, TransportFactory,
};

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

pub struct Fx {
    pub a: Desk,
    pub guard: DaemonGuard,
    pub home: TestHome,
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
        let home = TestHome::new();
        let guard = DaemonGuard::new(&home);
        let a = Self::desk(&home, tweak);
        Fx { a, guard, home }
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

    pub fn project(&self) -> LaunchSpec {
        crate::common::sh_spec(&self.home.project("p"))
    }
}

pub fn wait_dead(pid: i32) -> bool {
    crate::common::wait_dead(pid, Duration::from_secs(2))
}

pub fn pid_alive(pid: i32) -> bool {
    alive(pid)
}

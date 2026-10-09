//! Settings → Mobile: Tauri glue over `xshell_hostlink::ring::DesktopRing`. Commands
//! `ring_status`, `ring_enable` and `ring_set_relay_url`; the event `ring:status`.
//!
//! The local Daemon joins the Ring whenever the Local Host connects. The Host Observer runs
//! under the Host's lock, so it only records the connection ([`observe`], [`LOCAL_SYNC`]);
//! a worker thread does the work (`ring.identity`, then a new Roster version if the
//! Daemon is missing, then `ring.join`), and checks before each step that the connection
//! it was queued for is still the current one. [`LocalSync::attach`] reconciles a Local
//! Host that connected before the Ring state existed.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use xshell_hostlink::ring::{
    default_relay_url, DesktopRing, DesktopRingConfig, LocalIdentity, RingObserver, RingView,
};
use xshell_hostlink::{HostErrorCode, HostHandle, HostStatus, StatusKind, LOCAL_HOST_ID};
use xshell_protocol::ring::RosterChain;

/// How long the worker waits for one answer of the local Daemon.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
struct SyncState {
    connected: bool,
    /// Bumped on every connect and disconnect of the Local Host: a job queued for an older
    /// connection is stale.
    gen: u64,
    tx: Option<Sender<u64>>,
    shutdown: bool,
}

/// The Local Host's connection as the Observer last saw it, and the worker's queue.
pub struct LocalSync(Mutex<SyncState>);

/// The one the app's Host Observer feeds.
pub static LOCAL_SYNC: LocalSync = LocalSync::new();

impl LocalSync {
    pub const fn new() -> Self {
        LocalSync(Mutex::new(SyncState {
            connected: false,
            gen: 0,
            tx: None,
            shutdown: false,
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SyncState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// From the Observer, under the Host's lock: records the state, queues a job on a new
    /// connection. Never blocks.
    pub fn observe(&self, connected: bool) {
        let mut st = self.lock();
        if st.connected == connected {
            return;
        }
        st.connected = connected;
        st.gen += 1;
        if connected && !st.shutdown {
            if let Some(tx) = &st.tx {
                let _ = tx.send(st.gen);
            }
        }
    }

    /// Hands over the worker's queue, and reconciles a Local Host that is connected already.
    pub fn attach(&self, tx: Sender<u64>) {
        let mut st = self.lock();
        if st.connected && !st.shutdown {
            let _ = tx.send(st.gen);
        }
        st.tx = Some(tx);
    }

    /// Queues a job for the current connection, if any (after the Ring changed).
    pub fn poke(&self) {
        let st = self.lock();
        if st.connected && !st.shutdown {
            if let Some(tx) = &st.tx {
                let _ = tx.send(st.gen);
            }
        }
    }

    /// Whether a job of `gen` is still for the current connection.
    pub fn current(&self, gen: u64) -> bool {
        let st = self.lock();
        st.connected && !st.shutdown && st.gen == gen
    }

    pub fn shutdown(&self) {
        let mut st = self.lock();
        st.shutdown = true;
        st.tx = None;
    }
}

/// Whether `s` is the Local Host with a usable link.
fn local_usable(s: &HostStatus) -> bool {
    s.host == LOCAL_HOST_ID
        && matches!(s.status, StatusKind::Connected | StatusKind::UpgradePending)
}

/// The Observer's hook: records the Local Host's state. No work here (see the module docs).
pub fn observe(sync: &LocalSync, s: &HostStatus) {
    if s.host == LOCAL_HOST_ID {
        sync.observe(local_usable(s));
    }
}

fn blocking<T: Send + 'static>(
    f: impl FnOnce(Box<dyn FnOnce(T) + Send>),
    timeout: Duration,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    f(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    rx.recv_timeout(timeout).ok()
}

fn tokens(c: &RosterChain) -> Vec<String> {
    c.versions().iter().map(|r| r.token().to_string()).collect()
}

/// Keeps the local Daemon in the Ring. See the module docs.
pub struct Worker {
    pub ring: Arc<DesktopRing>,
    sync: &'static LocalSync,
    host: Box<dyn Fn() -> Option<Arc<HostHandle>> + Send + Sync>,
    /// The local Daemon cannot join (no `ring` capability).
    too_old: AtomicBool,
    changed: Box<dyn Fn() + Send + Sync>,
}

impl Worker {
    pub fn new(
        ring: Arc<DesktopRing>,
        sync: &'static LocalSync,
        host: Box<dyn Fn() -> Option<Arc<HostHandle>> + Send + Sync>,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Arc<Worker> {
        Arc::new(Worker {
            ring,
            sync,
            host,
            too_old: AtomicBool::new(false),
            changed,
        })
    }

    /// Starts the thread and attaches it to `sync` (which reconciles at once).
    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let (tx, rx) = mpsc::channel();
        let w = self.clone();
        std::thread::Builder::new()
            .name("ring-local".into())
            .spawn(move || w.run(rx))?;
        self.sync.attach(tx);
        Ok(())
    }

    fn run(&self, rx: Receiver<u64>) {
        while let Ok(gen) = rx.recv() {
            if self.sync.current(gen) {
                self.reconcile(gen);
            }
        }
    }

    pub fn too_old(&self) -> bool {
        self.too_old.load(Ordering::Acquire)
    }

    /// The local Daemon's identity, if the Local Host is usable and its Daemon can join.
    pub fn local_identity(&self) -> Option<LocalIdentity> {
        let h = (self.host)()?;
        if !local_usable(&h.status()) {
            return None;
        }
        let r = blocking(|w| h.ring_identity(w), LOCAL_TIMEOUT)?;
        match r {
            Ok(v) => {
                self.too_old.store(false, Ordering::Release);
                LocalIdentity::from_json(&v).ok()
            }
            Err(e) => {
                if e.code == HostErrorCode::Invalid {
                    self.too_old.store(true, Ordering::Release);
                }
                eprintln!(
                    "xshell: ring.identity on this computer failed: {}",
                    e.message
                );
                None
            }
        }
    }

    /// The local Daemon in the Ring: identity, Roster, join. Each step checks that `gen` is
    /// still the current connection.
    fn reconcile(&self, gen: u64) {
        let local = self.local_identity();
        (self.changed)();
        let Some(local) = local else {
            return;
        };
        if !self.sync.current(gen) {
            return;
        }
        let chain = match self.ring.ensure_local_daemon(&local) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                eprintln!("xshell: cannot add this computer to the ring: {e}");
                return;
            }
        };
        if !self.sync.current(gen) {
            return;
        }
        let Some(h) = (self.host)() else {
            return;
        };
        match blocking(|w| h.ring_join(tokens(&chain), w), LOCAL_TIMEOUT) {
            Some(Ok(_)) => {}
            Some(Err(e)) => eprintln!("xshell: ring.join on this computer failed: {}", e.message),
            None => eprintln!("xshell: ring.join on this computer timed out"),
        }
        (self.changed)();
    }
}

/// What the frontend gets: the Ring's view plus how this computer's terminals stand.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RingStatus {
    #[serde(flatten)]
    pub view: RingView,
    /// `daemon` (can join), `in-process` (terminals run inside the app) or `too-old`.
    pub local: &'static str,
}

pub struct RingState {
    pub worker: Arc<Worker>,
}

fn host_name() -> String {
    #[cfg(unix)]
    let raw = {
        let mut buf = [0u8; 256];
        let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if r == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            String::new()
        }
    };
    #[cfg(not(unix))]
    let raw = std::env::var("COMPUTERNAME").unwrap_or_default();
    xshell_protocol::ring::member_name(&raw, "this computer")
}

use tauri::{AppHandle, Emitter, Manager as _};

fn status_of(app: &AppHandle, view: RingView) -> RingStatus {
    let in_process = app.try_state::<crate::hosts::Hosts>().is_some_and(|h| {
        matches!(
            h.local_mode,
            crate::hosts::local::LocalMode::InProcess { .. }
        )
    });
    let too_old = app
        .try_state::<RingState>()
        .is_some_and(|r| r.worker.too_old());
    RingStatus {
        view,
        local: if in_process {
            "in-process"
        } else if too_old {
            "too-old"
        } else {
            "daemon"
        },
    }
}

struct TauriRingObserver(AppHandle);

impl RingObserver for TauriRingObserver {
    fn changed(&self, view: &RingView) {
        let _ = self.0.emit("ring:status", status_of(&self.0, view.clone()));
    }
}

/// Opens the Ring state and starts the local worker. After `Hosts` is managed.
pub fn setup(app: &AppHandle) {
    let dir = match app.path().app_data_dir() {
        Ok(d) => d.join("ring"),
        Err(e) => {
            eprintln!("xshell: mobile access is unavailable: no app data dir: {e}");
            return;
        }
    };
    let mut cfg = DesktopRingConfig::new(dir, host_name());
    cfg.default_relay_url = default_relay_url(&|k| std::env::var(k).ok());
    let ring = DesktopRing::open(cfg, Arc::new(TauriRingObserver(app.clone())));
    let (a, b) = (app.clone(), app.clone());
    let worker = Worker::new(
        ring,
        &LOCAL_SYNC,
        Box::new(move || {
            a.try_state::<crate::hosts::Hosts>()
                .and_then(|h| h.manager.host(LOCAL_HOST_ID))
        }),
        Box::new(move || {
            if let Some(r) = b.try_state::<RingState>() {
                let _ = b.emit("ring:status", status_of(&b, r.worker.ring.view()));
            }
        }),
    );
    app.manage(RingState {
        worker: worker.clone(),
    });
    // The Local Host may be connected already: record it before attaching, so the start
    // reconciles it.
    if let Some(h) = app
        .try_state::<crate::hosts::Hosts>()
        .and_then(|h| h.manager.host(LOCAL_HOST_ID))
    {
        observe(&LOCAL_SYNC, &h.status());
    }
    if let Err(e) = worker.start() {
        eprintln!("xshell: cannot start the ring worker: {e}");
    }
}

/// Quitting: no more local jobs, and the Relay hears goodbye. Blocks up to the goodbye.
pub fn quit(app: &AppHandle) {
    LOCAL_SYNC.shutdown();
    if let Some(r) = app.try_state::<RingState>() {
        r.worker.ring.quit();
    }
}

fn worker(app: &AppHandle) -> Result<Arc<Worker>, String> {
    app.try_state::<RingState>()
        .map(|r| r.worker.clone())
        .ok_or_else(|| "mobile access is unavailable".to_string())
}

async fn run<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn ring_status(app: AppHandle) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || Ok(status_of(&app, w.ring.view()))).await
}

/// Enables Mobile access: creates the Ring with this computer's Daemon when it can join.
/// After unreadable settings were set aside, a new Ring needs `startOver`.
#[tauri::command]
pub async fn ring_enable(app: AppHandle, start_over: Option<bool>) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || {
        let local = w.local_identity();
        let view = w.ring.enable(local, start_over.unwrap_or(false))?;
        // The Daemon joins (and is added, if it was not reachable a moment ago).
        LOCAL_SYNC.poke();
        Ok(status_of(&app, view))
    })
    .await
}

/// Moves the Ring to `url`: a new Roster version; this computer's Daemon follows.
#[tauri::command]
pub async fn ring_set_relay_url(app: AppHandle, url: String) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || {
        w.ring.set_relay_url(&url)?;
        LOCAL_SYNC.poke();
        Ok(status_of(&app, w.ring.view()))
    })
    .await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Instant;
    use xshell_hostlink::{
        FileSource, HostConfig, Manager, ManagerConfig, Observer, Transport, TransportFactory,
        UnixSocketDialer,
    };
    use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
    use xshell_protocol::msg::{
        decode_inbound, encode_msg, encode_res, ClientMsg, Hello, ProtocolRange, ServerMsg,
        TerminalInfo,
    };
    use xshell_protocol::ring::DeviceKeys;

    const VERSION: &str = "1.5.0";

    /// A Daemon that answers `ring.identity` and records `ring.join`.
    struct FakeDaemon {
        path: std::path::PathBuf,
        keys: Arc<DeviceKeys>,
        joins: Arc<Mutex<Vec<Vec<String>>>>,
        _dir: tempfile::TempDir,
    }

    fn fake_daemon() -> FakeDaemon {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let l = UnixListener::bind(&path).unwrap();
        let keys = Arc::new(DeviceKeys::generate().unwrap());
        let joins = Arc::new(Mutex::new(Vec::new()));
        let (k, j) = (keys.clone(), joins.clone());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let (k, j) = (k.clone(), j.clone());
                std::thread::spawn(move || {
                    let hello = ServerMsg::Hello(Hello {
                        protocol: ProtocolRange { min: 1, max: 1 },
                        version: VERSION.into(),
                        capabilities: vec!["call".into(), "term".into(), "ring".into()],
                    });
                    let list = ServerMsg::Terminals {
                        list: Vec::<TerminalInfo>::new(),
                    };
                    let _ = s.write_all(&encode_msg(&hello, None).unwrap());
                    let _ = s.write_all(&encode_msg(&list, None).unwrap());
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    while let Ok(Some(Frame::Json(b))) = read_frame(&mut r, MAX_FRAME_LEN) {
                        let Ok(m) = decode_inbound(&b) else { continue };
                        let Some(id) = m.id else { continue };
                        let res = match m.msg {
                            ClientMsg::RingIdentity => Ok(serde_json::json!({
                                "signKey": k.sign_key(),
                                "noiseKey": k.noise_key(),
                                "name": "fake",
                                "ring": null,
                            })),
                            ClientMsg::RingJoin { rosters } => {
                                let n = rosters.len();
                                j.lock().unwrap().push(rosters);
                                Ok(serde_json::json!({ "version": n }))
                            }
                            _ => Ok(serde_json::Value::Null),
                        };
                        let _ = s.write_all(&encode_res(id, res));
                    }
                });
            }
        });
        FakeDaemon {
            path,
            keys,
            joins,
            _dir: dir,
        }
    }

    struct Direct(std::path::PathBuf);

    impl TransportFactory for Direct {
        fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
            Box::new(xshell_hostlink::LocalShellTransport::default())
        }
        fn direct(&self, _: &HostConfig) -> Option<Box<dyn xshell_hostlink::Dialer>> {
            Some(Box::new(UnixSocketDialer {
                path: self.0.clone(),
            }))
        }
    }

    /// The app's Observer, minus the events: only the hook under test.
    struct Obs(&'static LocalSync);

    impl Observer for Obs {
        fn status(&self, s: &HostStatus) {
            observe(self.0, s);
        }
        fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
    }

    fn manager(d: &FakeDaemon, sync: &'static LocalSync) -> Arc<Manager> {
        let mut c = ManagerConfig::new(
            VERSION,
            Arc::new(Direct(d.path.clone())),
            Arc::new(FileSource("/nonexistent".into())),
            Arc::new(Obs(sync)),
        );
        c.backoff_unit = Duration::from_millis(20);
        Arc::new(Manager::new(c))
    }

    fn connect_local(m: &Manager) {
        m.set_local(HostConfig {
            id: LOCAL_HOST_ID.into(),
            name: LOCAL_HOST_ID.into(),
            ssh_target: String::new(),
            color: None,
            daemon_command: None,
            launch_prefixes: Default::default(),
        });
    }

    fn ring(dir: &std::path::Path) -> Arc<DesktopRing> {
        struct Quiet;
        impl RingObserver for Quiet {
            fn changed(&self, _: &RingView) {}
        }
        let mut cfg = DesktopRingConfig::new(dir.join("ring"), "desk".into());
        // Nothing listens there; the Relay plays no part here.
        cfg.default_relay_url = "ws://127.0.0.1:9".into();
        cfg.timeouts.connect = Duration::from_millis(500);
        DesktopRing::open(cfg, Arc::new(Quiet))
    }

    fn worker(ring: Arc<DesktopRing>, m: &Arc<Manager>, sync: &'static LocalSync) -> Arc<Worker> {
        let m = m.clone();
        Worker::new(
            ring,
            sync,
            Box::new(move || m.host(LOCAL_HOST_ID)),
            Box::new(|| {}),
        )
    }

    fn wait_join(d: &FakeDaemon, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(j) = d.joins.lock().unwrap().iter().find(|j| j.len() == n) {
                return j.clone();
            }
            assert!(Instant::now() < deadline, "no ring.join with {n} versions");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn leak() -> &'static LocalSync {
        Box::leak(Box::new(LocalSync::new()))
    }

    #[test]
    fn local_daemon_joins_when_the_local_host_connects() {
        let d = fake_daemon();
        let sync = leak();
        let m = manager(&d, sync);
        let t = tempfile::tempdir().unwrap();
        let ring = ring(t.path());
        ring.enable(None, false).unwrap();
        let w = worker(ring.clone(), &m, sync);
        w.start().unwrap();
        // Through the Manager's Observer: the hook queues, the worker adds and joins.
        connect_local(&m);
        let join = wait_join(&d, 2);
        let chain = RosterChain::from_tokens(&join).unwrap();
        assert!(chain.head().member(&d.keys.sign_key()).is_some());
        assert_eq!(ring.chain().unwrap(), chain);
        assert!(!w.too_old());
        m.shutdown();
        ring.quit();
    }

    #[test]
    fn a_local_host_connected_before_the_ring_state_is_reconciled() {
        let d = fake_daemon();
        let sync = leak();
        let m = manager(&d, sync);
        connect_local(&m);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !m
            .host(LOCAL_HOST_ID)
            .is_some_and(|h| local_usable(&h.status()))
        {
            assert!(Instant::now() < deadline, "local host connected");
            std::thread::sleep(Duration::from_millis(10));
        }
        let t = tempfile::tempdir().unwrap();
        let ring = ring(t.path());
        ring.enable(None, false).unwrap();
        // The Ring state comes after the connection: attaching reconciles it.
        let w = worker(ring.clone(), &m, sync);
        w.start().unwrap();
        wait_join(&d, 2);
        m.shutdown();
        ring.quit();
    }

    #[test]
    fn jobs_of_an_old_connection_are_stale() {
        let s = LocalSync::new();
        let (tx, rx) = mpsc::channel();
        s.observe(true);
        s.attach(tx);
        let g = rx.try_recv().unwrap();
        assert!(s.current(g));
        // Repeated connected reports queue nothing new.
        s.observe(true);
        assert!(rx.try_recv().is_err());
        s.observe(false);
        assert!(!s.current(g));
        s.observe(true);
        let g2 = rx.try_recv().unwrap();
        assert!(g2 > g && s.current(g2) && !s.current(g));
        s.poke();
        assert_eq!(rx.try_recv().unwrap(), g2);
        s.shutdown();
        assert!(!s.current(g2));
        s.observe(false);
        s.observe(true);
        assert!(rx.try_recv().is_err());
    }
}

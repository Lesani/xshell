//! Remote Host connections: Tauri glue over `xshell_hostlink::Manager`. Status changes and
//! `terminals` lists become events; Terminal output goes through the same `Channel`s local
//! Terminals use. The Local Host is one of the Manager's Hosts too (id `local`) when its
//! Terminals run in a Daemon ([`local::LocalMode::Daemon`]): on Linux and macOS always the
//! one always-on Daemon (ADR-0017), on Windows the GUI-bound one.

pub mod available;
pub mod binaries;
pub mod commands;
pub mod local;
pub mod presence;
pub mod quit;
pub mod service;

use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Emitter, Manager as _};
use xshell_hostlink::{
    HostConfig, HostStatus, Manager, ManagerConfig, Observer, SshTransport, TermSink, Transport,
    TransportFactory, LOCAL_HOST_ID,
};
use xshell_protocol::msg::TerminalInfo;

type DaemonSlot = Arc<Mutex<Option<Arc<xshell_hostlink::LocalDaemon>>>>;

pub struct Hosts {
    pub manager: Arc<Manager>,
    pub available: Arc<available::AvailableState>,
    /// How local Tabs run, as resolved at startup (see [`Hosts::local_mode_now`]).
    pub local_mode: local::LocalMode,
    /// The Daemon this app may start for the Local Host, as set up at startup (Daemon mode
    /// only; see [`Hosts::local_daemon_now`] for one a Retry set up later).
    pub local_daemon: Option<Arc<xshell_hostlink::LocalDaemon>>,
    /// `settings.json`.
    pub store: Option<Arc<dyn local::SettingStore>>,
    /// One switch of the Daemon mode at a time (legacy Persistent switch).
    pub switch_lock: tokio::sync::Mutex<()>,
    /// What keeps the Daemon running (Unix).
    pub service: Arc<service::ServiceState>,
    /// The mode and Daemon now: a Retry from `Failed` replaces them.
    current: Mutex<local::LocalMode>,
    daemon: DaemonSlot,
    app: AppHandle,
    version: String,
    /// The home directory (a seam: tests make it unknown, then known).
    home_of: HomeFn,
}

type HomeFn = Arc<dyn Fn() -> Option<std::path::PathBuf> + Send + Sync>;

/// Linux or macOS.
pub fn linux_or_macos() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

/// `settings.json` through the store plugin: the instance the frontend's `load` also gets.
struct TauriStore(Arc<tauri_plugin_store::Store<tauri::Wry>>);

impl local::RawStore for TauriStore {
    fn get(&self, key: &str) -> Option<serde_json::Value> {
        self.0.get(key)
    }
    fn set(&self, key: &str, value: serde_json::Value) {
        self.0.set(key, value);
    }
    fn delete(&self, key: &str) {
        self.0.delete(key);
    }
    fn save(&self) -> Result<(), String> {
        self.0.save().map_err(|e| e.to_string())
    }
}

/// `~/.xshell/server/<version>+<build>/xshelld`: where the embedded Daemon is installed.
fn own_install_bin(version: &str, home: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let home = home?;
    let dir = xshell_protocol::build::install_dir_name(version, crate::build_id::BUILD.as_ref());
    Some(xshell_hostlink::install::local_server_bin(home, &dir))
}

/// Resolve how local Tabs run and set up the Local Host's Daemon for it.
fn setup_local(
    version: &str,
    exe_dir: Option<&std::path::Path>,
    svc: &Arc<service::ServiceState>,
    home: Option<std::path::PathBuf>,
) -> (local::LocalMode, Option<Arc<xshell_hostlink::LocalDaemon>>) {
    let install_bin = own_install_bin(version, home.as_deref());
    let mut mode = local::resolve_local(
        exe_dir,
        &|k| std::env::var_os(k),
        cfg!(unix),
        binaries::embedded_own().is_some(),
        install_bin.as_deref(),
    );
    let daemon = match &mode {
        local::LocalMode::Daemon { bin, source } => {
            match local_daemon(bin.clone(), version, home) {
                Ok(d) => {
                    configure(&d, source, svc);
                    Some(d)
                }
                Err(reason) => {
                    mode = if cfg!(unix) {
                        local::LocalMode::Failed { reason }
                    } else {
                        local::LocalMode::InProcess { reason }
                    };
                    None
                }
            }
        }
        _ => None,
    };
    match &mode {
        local::LocalMode::Daemon { bin, .. } => {
            eprintln!("xshell: local terminals run in {}", bin.display())
        }
        local::LocalMode::InProcess { reason } => {
            eprintln!("xshell: local terminals run in the app: {reason}")
        }
        local::LocalMode::Failed { reason } => {
            eprintln!("xshell: the local Daemon cannot be set up: {reason}")
        }
    }
    (mode, daemon)
}

/// Before the Local Host starts dialing. Unix: always-on (ADR-0017), `serve` first, and a
/// prepare hook that installs the embedded Daemon (or reports that there is none) and waits
/// once, briefly, for the service. Windows: the stored GUI-bound mode as before.
#[cfg_attr(not(unix), allow(unused_variables))]
fn configure(
    d: &Arc<xshell_hostlink::LocalDaemon>,
    source: &local::DaemonSource,
    svc: &Arc<service::ServiceState>,
) {
    #[cfg(unix)]
    {
        d.set_persistent(true);
        d.set_serve_first(true);
        let svc = svc.clone();
        let hook: xshell_hostlink::local::PrepareHook = match source.clone() {
            local::DaemonSource::Missing(reason) => Arc::new(move || Err(reason.clone())),
            local::DaemonSource::Embedded => {
                let bin = d.bin().to_path_buf();
                Arc::new(move || {
                    install_embedded(&bin)?;
                    svc.wait_first(service::FIRST_WAIT);
                    Ok(())
                })
            }
            local::DaemonSource::File => Arc::new(move || {
                svc.wait_first(service::FIRST_WAIT);
                Ok(())
            }),
        };
        d.set_prepare(hook);
    }
    // Windows: GUI-bound only (the Persistent setting is not offered there).
    #[cfg(not(unix))]
    d.set_persistent(false);
}

/// Install the embedded Daemon at `bin` (`~/.xshell/server/<dir>/xshelld`).
#[cfg(unix)]
fn install_embedded(bin: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let bytes = binaries::embedded_own().ok_or("this build carries no xshelld")?;
    let dir = bin
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        .ok_or("bad install path")?;
    let home = bin
        .ancestors()
        .nth(4)
        .ok_or("bad install path")?
        .to_path_buf();
    xshell_hostlink::install::install_local_bytes(bytes, &home, dir)
        .map_err(|e| format!("cannot install the Daemon to {}: {e}", bin.display()))
}

/// The installed Daemon to run `service install` with, per source; `Err`: why there is none.
#[cfg(unix)]
fn service_binary(
    d: &xshell_hostlink::LocalDaemon,
    source: &local::DaemonSource,
) -> Result<std::path::PathBuf, String> {
    match source {
        local::DaemonSource::Missing(r) => Err(r.clone()),
        local::DaemonSource::Embedded => install_embedded(d.bin()),
        local::DaemonSource::File => {
            d.check_bin().map_err(|e| e.to_string())?;
            let home = dirs::home_dir().ok_or("the home directory is unknown")?;
            xshell_hostlink::install::install_local(d.bin(), &home, &d.install_dir())
                .map_err(|e| e.to_string())
        }
    }
}

impl Hosts {
    pub fn new(app: &AppHandle) -> Self {
        Self::with_home(app, Arc::new(dirs::home_dir))
    }

    fn with_home(app: &AppHandle, home_of: HomeFn) -> Self {
        use tauri_plugin_store::StoreExt as _;
        let version = app.package_info().version.to_string();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let store: Option<Arc<dyn local::SettingStore>> = match app.store("settings.json") {
            Ok(s) => Some(Arc::new(local::CommitStore::new(TauriStore(s)))),
            Err(e) => {
                eprintln!("xshell: cannot open settings.json: {e}");
                None
            }
        };
        let svc = Arc::new(service::ServiceState::default());
        let (local_mode, local_daemon) = setup_local(&version, exe_dir.as_deref(), &svc, home_of());
        let cache_dir = app
            .path()
            .app_cache_dir()
            .unwrap_or_else(|_| std::env::temp_dir().join("xshell-cache"));
        let slot: DaemonSlot = Arc::new(Mutex::new(local_daemon.clone()));
        let factory = SshFactory {
            local: slot.clone(),
        };
        let available = Arc::new(available::AvailableState::default());
        let mut cfg = ManagerConfig::new(
            version.clone(),
            Arc::new(factory),
            Arc::new(binaries::production_source(exe_dir, cache_dir)),
            Arc::new(TauriObserver(app.clone(), available.clone())),
        );
        cfg.desktop_build = crate::build_id::BUILD.clone();
        let manager = Arc::new(Manager::new(cfg));
        available.bind_manager(&manager);
        if local_daemon.is_some() {
            manager.set_local(local_host_config());
        }
        let hosts = Self {
            manager,
            available,
            current: Mutex::new(local_mode.clone()),
            local_mode,
            local_daemon,
            store,
            switch_lock: tokio::sync::Mutex::new(()),
            service: svc,
            daemon: slot,
            app: app.clone(),
            version,
            home_of,
        };
        hosts.start_service_install();
        hosts
    }

    /// The mode now (a Retry may have set the Daemon up after a failed start).
    pub fn local_mode_now(&self) -> local::LocalMode {
        self.current.lock().unwrap().clone()
    }

    /// The Local Host's Daemon now.
    pub fn local_daemon_now(&self) -> Option<Arc<xshell_hostlink::LocalDaemon>> {
        self.daemon.lock().unwrap().clone()
    }

    /// Unix: `service install` on its own thread, then the report and a `local:daemon` event.
    /// Development builds skip it unless `XSHELL_SERVICE=1`.
    fn start_service_install(&self) {
        #[cfg(unix)]
        {
            let (local::LocalMode::Daemon { source, .. }, Some(d)) =
                (self.local_mode_now(), self.local_daemon_now())
            else {
                return;
            };
            let svc = self.service.clone();
            svc.reset();
            if !service::should_install(cfg!(debug_assertions), &|k| std::env::var_os(k)) {
                svc.set(local::ServiceInfo {
                    mechanism: None,
                    linger: None,
                    error: Some("not installed by a development build (XSHELL_SERVICE=1)".into()),
                });
                return;
            }
            let app = self.app.clone();
            let _ = std::thread::Builder::new()
                .name("daemon-service".into())
                .spawn(move || {
                    let info = match service_binary(&d, &source) {
                        Ok(bin) => service::run_install(&bin, service::INSTALL_TIMEOUT),
                        Err(e) => local::ServiceInfo {
                            mechanism: None,
                            linger: None,
                            error: Some(e),
                        },
                    };
                    if let Some(e) = &info.error {
                        eprintln!("xshell: Daemon service: {e}");
                    }
                    svc.set(info);
                    emit_local_info(&app);
                });
        }
    }

    /// What `local_host_info` answers and `local:daemon` carries.
    pub fn local_info(&self) -> local::LocalHostInfo {
        info_of(
            &self.local_mode_now(),
            self.local_daemon_now().as_deref(),
            &self.manager,
            &self.service,
            (self.home_of)().as_deref(),
        )
    }

    /// `local_daemon_retry`: clear the start error and dial again; in `Failed` set the
    /// Daemon up again first (A8); rerun the service install when it failed.
    pub fn retry_local(&self) -> local::LocalHostInfo {
        let set_up = retry_setup(
            &self.current,
            &self.daemon,
            &self.manager,
            &self.version,
            &self.service,
            (self.home_of)(),
        );
        if set_up || self.service.get().is_some_and(|s| s.error.is_some()) {
            self.start_service_install();
        }
        self.local_info()
    }

    /// Quitting: stop every Host link. On Unix the always-on Daemon keeps running (its
    /// Terminals too: the quit prompt already asked); on Windows the GUI-bound Daemon this app
    /// started ends (and with it every local Terminal), within about 8 s before it is killed.
    pub fn quit(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let d = self.local_daemon_now();
        if let Some(d) = &d {
            d.hang_up();
        }
        // Kill every ssh; Remote Daemons and their Terminals keep running.
        self.manager.shutdown();
        if let Some(d) = &d {
            d.reap(deadline);
        }
    }
}

/// [`Hosts::local_info`] from its parts.
fn info_of(
    mode: &local::LocalMode,
    d: Option<&xshell_hostlink::LocalDaemon>,
    manager: &Manager,
    svc: &service::ServiceState,
    home: Option<&std::path::Path>,
) -> local::LocalHostInfo {
    let daemon = match (mode, d) {
        // No LocalDaemon could be built: still the error with this home's real log path.
        (local::LocalMode::Failed { reason }, _) if cfg!(unix) => {
            let error = Some(reason.clone());
            Some(local::DaemonInfo {
                state: "failed",
                error_kind: local::error_kind(error.as_deref()),
                error,
                log: home.map(|h| {
                    xshell_hostlink::local::local_log_path(h)
                        .display()
                        .to_string()
                }),
            })
        }
        (local::LocalMode::Daemon { .. }, Some(d)) => {
            let snap = manager.access(LOCAL_HOST_ID).map(|h| h.snapshot());
            let (state, error) = local::daemon_state(
                d.start_error(),
                snap.as_ref().map(|s| &s.status),
                snap.as_ref().is_some_and(|s| s.terminals.is_some()),
            );
            Some(local::DaemonInfo {
                state,
                error_kind: local::error_kind(error.as_deref()),
                error,
                log: Some(d.log.display().to_string()),
            })
        }
        _ => None,
    };
    let service = match (mode, cfg!(unix)) {
        (local::LocalMode::Daemon { .. }, true) => Some(svc.get().unwrap_or_default()),
        _ => None,
    };
    mode.info(local::PersistentInfo::default(), daemon, service)
}

/// A Retry: in `Failed`, set the Daemon up again (and add the Local Host); always clear the
/// start error and kick the Local Host. Returns whether a Daemon was newly set up.
fn retry_setup(
    current: &Mutex<local::LocalMode>,
    slot: &DaemonSlot,
    manager: &Manager,
    version: &str,
    svc: &Arc<service::ServiceState>,
    home: Option<std::path::PathBuf>,
) -> bool {
    let mut set_up = false;
    let failed = matches!(*current.lock().unwrap(), local::LocalMode::Failed { .. });
    if failed {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let (mode, daemon) = setup_local(version, exe_dir.as_deref(), svc, home);
        *current.lock().unwrap() = mode;
        if daemon.is_some() {
            *slot.lock().unwrap() = daemon;
            manager.set_local(local_host_config());
            set_up = true;
        }
    }
    if let Some(d) = slot.lock().unwrap().clone() {
        d.clear_start_error();
    }
    if let Some(h) = manager.access(LOCAL_HOST_ID) {
        h.kick();
    }
    set_up
}

fn local_host_config() -> HostConfig {
    HostConfig {
        id: LOCAL_HOST_ID.into(),
        name: LOCAL_HOST_ID.into(),
        ssh_target: String::new(),
        color: None,
        daemon_command: None,
        launch_prefixes: Default::default(),
    }
}

/// Send `local:daemon` with the current info. Observer callbacks may run under hostlink's
/// locks, so the info is read and sent on a worker thread; bursts collapse into one event.
pub fn emit_local_info(app: &AppHandle) {
    static WORKER: std::sync::OnceLock<Mutex<std::sync::mpsc::Sender<()>>> =
        std::sync::OnceLock::new();
    let tx = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let app = app.clone();
        let _ = std::thread::Builder::new()
            .name("local-info".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    while rx.try_recv().is_ok() {}
                    if let Some(h) = app.try_state::<Hosts>() {
                        let _ = app.emit("local:daemon", h.local_info());
                    }
                }
            });
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(());
}

/// The Local Host's Daemon, reached on this user's Daemon socket (Windows: named pipe).
fn local_daemon(
    bin: std::path::PathBuf,
    version: &str,
    home: Option<std::path::PathBuf>,
) -> Result<Arc<xshell_hostlink::LocalDaemon>, String> {
    let home = home.ok_or("the home directory is unknown")?;
    #[cfg(unix)]
    let socket = {
        let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
        xshell_hostlink::local::local_socket_path(&home, xdg.as_deref())
    };
    #[cfg(windows)]
    let socket = xshell_hostlink::local::local_pipe_name()
        .map_err(|e| format!("cannot name this user's Daemon pipe: {e}"))?;
    xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin,
        env: vec![],
        home,
        socket,
        version: version.into(),
        build: crate::build_id::BUILD.clone(),
    })
    .map(Arc::new)
    .map_err(|e| format!("cannot set up the Local Host's Daemon: {e}"))
}

#[derive(Serialize, Clone)]
struct TerminalsEvent<'a> {
    host: &'a str,
    list: &'a [TerminalInfo],
}

struct TauriObserver(AppHandle, Arc<available::AvailableState>);

impl Observer for TauriObserver {
    fn available(&self, hosts: &[xshell_hostlink::ring::hosts::AvailableHost]) {
        self.1.publish(hosts, |snapshot| {
            let _ = self.0.emit("hosts:available", snapshot);
        });
    }
    fn status(&self, s: &HostStatus) {
        // Records the Host's connection for the Ring; the work runs elsewhere.
        crate::ring::observe(&crate::ring::HOST_SYNC, s);
        let _ = self.0.emit("hosts:status", s);
        if s.host == LOCAL_HOST_ID {
            emit_local_info(&self.0);
        }
    }
    fn terminals(&self, host: &str, list: &[TerminalInfo]) {
        let _ = self
            .0
            .emit("hosts:terminals", TerminalsEvent { host, list });
        // Readiness needs the first list (A8).
        if host == LOCAL_HOST_ID {
            emit_local_info(&self.0);
        }
    }
    fn renamed(&self, host: &str) {
        // The Host's Ring member takes the new name.
        crate::ring::HOST_SYNC.renamed(host);
    }
}

/// The exit channel's payload: the code plus how many bytes `on_data` carried before it, so
/// the frontend applies the exit only after that much output.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteExit {
    pub code: i32,
    pub bytes: u64,
}

pub struct ChannelSink {
    pub data: Channel<Response>,
    pub exit: Channel<RemoteExit>,
    pub resync: Option<Channel<()>>,
}

impl TermSink for ChannelSink {
    fn data(&self, bytes: &[u8]) -> bool {
        self.data.send(Response::new(bytes.to_vec())).is_ok()
    }
    fn exit(&self, code: i32, bytes: u64) {
        let _ = self.exit.send(RemoteExit { code, bytes });
    }
    fn resync_required(&self) {
        if let Some(channel) = &self.resync {
            let _ = channel.send(());
        }
    }
}

struct SshFactory {
    local: DaemonSlot,
}

impl TransportFactory for SshFactory {
    fn for_host(&self, cfg: &HostConfig) -> Box<dyn Transport> {
        Box::new(SshTransport::new(cfg.ssh_target.clone()))
    }

    /// The Local Host: this user's Daemon socket (pipe), starting its Daemon if need be.
    fn direct(&self, cfg: &HostConfig) -> Option<Box<dyn xshell_hostlink::Dialer>> {
        if cfg.id != LOCAL_HOST_ID {
            return None;
        }
        let daemon = self.local.lock().unwrap().clone()?;
        Some(Box::new(xshell_hostlink::LocalDialer { daemon }))
    }
}

#[cfg(test)]
mod channel_sink_tests {
    use super::*;
    use std::sync::Mutex;
    use tauri::ipc::InvokeResponseBody;

    #[test]
    fn resync_is_a_scoped_null_callback_not_output_or_exit() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let channel = |name: &'static str| {
            let messages = messages.clone();
            move |body| {
                messages.lock().unwrap().push((name, body));
                Ok(())
            }
        };
        let first = ChannelSink {
            data: Channel::new(channel("data")),
            exit: Channel::new(channel("exit")),
            resync: Some(Channel::new(channel("resync"))),
        };
        let other = ChannelSink {
            data: Channel::new(channel("other-data")),
            exit: Channel::new(channel("other-exit")),
            resync: Some(Channel::new(channel("other-resync"))),
        };
        let sink: &dyn TermSink = &first;
        assert!(sink.data(&[0, 128, 255]));
        sink.resync_required();
        // The other viewer remains independently usable.
        assert!(other.data(&[7]));
        other.exit(3, 1);
        let messages = messages.lock().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].0, "data");
        assert!(
            matches!(&messages[0].1, InvokeResponseBody::Raw(bytes) if bytes == &[0, 128, 255])
        );
        assert_eq!(messages[1].0, "resync");
        assert!(matches!(&messages[1].1, InvokeResponseBody::Json(body) if body == "null"));
        assert_eq!(messages[2].0, "other-data");
        assert!(matches!(&messages[2].1, InvokeResponseBody::Raw(bytes) if bytes == &[7]));
        assert_eq!(messages[3].0, "other-exit");
        match &messages[3].1 {
            InvokeResponseBody::Json(body) => assert_eq!(
                serde_json::from_str::<serde_json::Value>(body).unwrap(),
                serde_json::json!({"code": 3, "bytes": 1})
            ),
            _ => panic!("exit must retain its JSON watermark payload"),
        }
    }

    #[test]
    fn older_callers_may_omit_the_resync_channel() {
        let sink = ChannelSink {
            data: Channel::new(|_| panic!("resync must not fabricate data")),
            exit: Channel::new(|_| panic!("resync must not fabricate exit")),
            resync: None,
        };
        sink.resync_required();
    }
}

/// A8 with real parts: hostlink's Manager dials a Local Host Daemon that refuses hello.
#[cfg(all(test, unix))]
mod local_readiness_tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    struct Quiet;
    impl Observer for Quiet {
        fn status(&self, _: &HostStatus) {}
        fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
    }
    struct NoBinaries;
    impl xshell_hostlink::BinarySource for NoBinaries {
        fn fetch(
            &self,
            _: &xshell_hostlink::Wanted,
            _: &xshell_hostlink::CancelToken,
        ) -> Result<xshell_hostlink::Fetched, String> {
            Err("none".into())
        }
    }

    #[test]
    fn refused_hello_is_failed_with_the_reason() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let sock = t.path().join("d.sock");
        let l = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut c = c;
                let body = br#"{"t":"error","code":"refused","message":"hello refused by test"}"#;
                let _ = xshell_protocol::frame::write_frame(
                    &mut c,
                    &xshell_protocol::frame::Frame::Json(body.to_vec()),
                );
                let _ = c.flush();
            }
        });
        let d = Arc::new(
            xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
                bin: t.path().join("no-xshelld"),
                env: vec![],
                home,
                socket: sock,
                version: "1.5.0".into(),
                build: None,
            })
            .unwrap(),
        );
        d.set_persistent(true);
        let slot: DaemonSlot = Arc::new(Mutex::new(Some(d.clone())));
        let m = Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(SshFactory { local: slot }),
            Arc::new(NoBinaries),
            Arc::new(Quiet),
        ));
        m.set_local(local_host_config());
        let deadline = Instant::now() + Duration::from_secs(10);
        let (state, error) = loop {
            let snap = m.access(LOCAL_HOST_ID).unwrap().snapshot();
            let r = local::daemon_state(
                d.start_error(),
                Some(&snap.status),
                snap.terminals.is_some(),
            );
            if r.0 == "failed" || Instant::now() >= deadline {
                break r;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(state, "failed");
        assert!(
            error
                .as_deref()
                .unwrap_or("")
                .contains("hello refused by test"),
            "{error:?} / {:#?}",
            m.access(LOCAL_HOST_ID).unwrap().snapshot().status
        );
        m.shutdown();
    }

    /// A8: a real retry from `LocalMode::Failed` sets the Daemon up (with the Local Host)
    /// once the cause is gone, and reports Daemon mode.
    #[test]
    fn retry_from_failed_sets_the_daemon_up() {
        let t = tempfile::tempdir().unwrap();
        let svc = Arc::new(service::ServiceState::default());
        // Home unknown: Failed (Unix), no Daemon.
        let (mode, daemon) = setup_local("1.5.0", Some(t.path()), &svc, None);
        assert!(daemon.is_none());
        if !cfg!(unix) {
            return;
        }
        assert!(
            matches!(&mode, local::LocalMode::Failed { reason } if reason.contains("home directory is unknown"))
        );
        let current = Mutex::new(mode);
        let slot: DaemonSlot = Arc::new(Mutex::new(None));
        let m = Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(SshFactory {
                local: slot.clone(),
            }),
            Arc::new(NoBinaries),
            Arc::new(Quiet),
        ));
        let info = info_of(&current.lock().unwrap(), None, &m, &svc, None);
        assert_eq!(info.mode, "failed");
        // The Daemon object is there for the error view, with the log path when the home
        // is known.
        let d0 = info.daemon.expect("daemon info in mode failed");
        assert_eq!(d0.state, "failed");
        assert!(d0.error.unwrap().contains("home directory is unknown"));
        assert_eq!(d0.log, None);
        let info = info_of(&current.lock().unwrap(), None, &m, &svc, Some(t.path()));
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["daemon"]["state"], "failed");
        assert_eq!(json["daemon"]["errorKind"], serde_json::Value::Null);
        assert_eq!(
            json["daemon"]["log"],
            t.path()
                .join(".xshell/log/xshelld.log")
                .display()
                .to_string()
        );
        // Still unknown: stays failed, no Local Host.
        assert!(!retry_setup(&current, &slot, &m, "1.5.0", &svc, None));
        assert!(m.access(LOCAL_HOST_ID).is_none());
        // Known now: set up, the Local Host exists, the info says daemon.
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(retry_setup(
            &current,
            &slot,
            &m,
            "1.5.0",
            &svc,
            Some(home.clone())
        ));
        let d = slot.lock().unwrap().clone().expect("a Daemon");
        assert!(m.access(LOCAL_HOST_ID).is_some());
        let info = info_of(&current.lock().unwrap(), Some(&d), &m, &svc, Some(&home));
        assert_eq!(info.mode, "daemon", "{info:?}");
        assert!(info
            .daemon
            .unwrap()
            .log
            .unwrap()
            .starts_with(&home.display().to_string()));
        assert!(info.service.is_some());
        d.hang_up();
        m.shutdown();
    }
}

//! Remote Host connections: Tauri glue over `xshell_hostlink::Manager`. Status changes and
//! `terminals` lists become events; Terminal output goes through the same `Channel`s local
//! Terminals use. The Local Host is one of the Manager's Hosts too (id `local`) when its
//! Terminals run in a Daemon ([`local::LocalMode::Daemon`]): on Linux and macOS always the
//! one always-on Daemon (ADR-0017), on Windows the GUI-bound one.

pub mod available;
pub mod binaries;
pub mod commands;
pub mod latest;
pub mod local;
pub mod presence;
pub mod quit;
pub mod service;
pub mod shim;

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

/// `xshell pair` (#118): the Local Host's Daemon binary, resolved exactly as the running
/// Desktop resolves it ([`setup_local`]), with the embedded Daemon installed if it is not
/// there yet. Starts nothing.
pub fn local_daemon_bin_for_cli() -> Result<std::path::PathBuf, String> {
    let home = dirs::home_dir();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let install_bin = own_install_bin(env!("CARGO_PKG_VERSION"), home.as_deref());
    let matches = |p: &std::path::Path| local::binary_is_build(p, crate::build_id::BUILD.as_ref());
    match local::resolve_local(
        exe_dir.as_deref(),
        &|k| std::env::var_os(k),
        binaries::embedded_own().is_some(),
        install_bin.as_deref(),
        &matches,
    ) {
        local::LocalMode::Daemon { bin, source } => match source {
            local::DaemonSource::Embedded => install_embedded(&bin),
            local::DaemonSource::File => Ok(bin),
            local::DaemonSource::Missing(reason) => Err(reason),
        },
        local::LocalMode::InProcess { reason } => Err(format!(
            "this computer's terminals run inside the app ({reason})"
        )),
        local::LocalMode::Failed { reason } => Err(reason),
    }
}

/// Resolve how local Tabs run and set up the Local Host's Daemon for it.
fn setup_local(
    version: &str,
    exe_dir: Option<&std::path::Path>,
    svc: &Arc<service::ServiceState>,
    home: Option<std::path::PathBuf>,
) -> (local::LocalMode, Option<Arc<xshell_hostlink::LocalDaemon>>) {
    let install_bin = own_install_bin(version, home.as_deref());
    let embedded = binaries::embedded_own().is_some();
    let matches = |p: &std::path::Path| local::binary_is_build(p, crate::build_id::BUILD.as_ref());
    if std::env::var_os("XSHELL_DAEMON_BIN").is_none_or(|v| v.is_empty())
        && local::sidecar_ignored(exe_dir, embedded, &matches)
    {
        eprintln!("{}", local::IGNORED_SIDECAR);
    }
    let mut mode = local::resolve_local(
        exe_dir,
        &|k| std::env::var_os(k),
        embedded,
        install_bin.as_deref(),
        &matches,
    );
    let daemon = match &mode {
        local::LocalMode::Daemon { bin, source } => {
            match local_daemon(bin.clone(), version, home.clone()) {
                Ok(d) => {
                    configure(&d, source, svc, home.as_deref());
                    Some(d)
                }
                Err(reason) => {
                    // Never in-process (W2): the error view with the log path and Retry.
                    mode = local::LocalMode::Failed { reason };
                    None
                }
            }
        }
        _ => None,
    };
    // Windows: the stable console command `xshelld` for this Daemon (#118). Never fatal.
    #[cfg(windows)]
    if let (local::LocalMode::Daemon { bin, source }, Some(home)) = (&mode, home.as_deref()) {
        if !matches!(source, local::DaemonSource::Missing(_)) {
            if let Err(e) = shim::write(home, bin) {
                eprintln!("xshell: cannot write the xshelld command: {e}");
            }
        }
    }
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
/// once, briefly, for the service. Windows: GUI-bound in this Desktop's Job (W1), with a
/// prepare hook that installs the embedded Daemon (or reports that there is none).
#[cfg_attr(not(unix), allow(unused_variables))]
fn configure(
    d: &Arc<xshell_hostlink::LocalDaemon>,
    source: &local::DaemonSource,
    svc: &Arc<service::ServiceState>,
    home: Option<&std::path::Path>,
) {
    #[cfg(unix)]
    {
        d.set_persistent(true);
        d.set_serve_first(true);
        // An older xshell window that still runs (whose GUI-bound Daemon may not have started
        // yet) keeps this Desktop from starting the machine's Daemon (A10).
        let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
        let socket_dirs: Vec<std::path::PathBuf> = [
            xdg.as_deref()
                .filter(|p| p.is_absolute())
                .map(|x| x.join("xshell")),
            home.map(|h| h.join(".xshell").join("run")),
        ]
        .into_iter()
        .flatten()
        .collect();
        let presence_dir = presence::default_dir();
        d.set_blocker(Arc::new(move || {
            presence::older_desktop_running(&socket_dirs, &presence_dir, std::process::id())
                .then(|| xshell_hostlink::local::OLDER_DESKTOP.to_string())
        }));
        let svc = svc.clone();
        let hook: xshell_hostlink::local::PrepareHook = match source.clone() {
            local::DaemonSource::Missing(reason) => Arc::new(move || Err(reason.clone())),
            local::DaemonSource::Embedded => {
                let bin = d.bin().to_path_buf();
                // Installed now, not only when nothing listens: replacing a Daemon that runs
                // (an older build's) installs and selects this file first.
                if let Err(e) = install_embedded(&bin) {
                    eprintln!("xshell: {e}");
                }
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
    // Windows: GUI-bound only (W1). No service and no blocker: a Daemon another window
    // started is used as it is.
    #[cfg(not(unix))]
    {
        d.set_persistent(false);
        match source.clone() {
            local::DaemonSource::Missing(reason) => {
                d.set_prepare(Arc::new(move || Err(reason.clone())));
            }
            local::DaemonSource::Embedded => {
                let bin = d.bin().to_path_buf();
                if let Err(e) = install_embedded(&bin) {
                    eprintln!("xshell: {e}");
                }
                d.set_prepare(Arc::new(move || install_embedded(&bin).map(drop)));
            }
            local::DaemonSource::File => {}
        }
    }
}

/// Install the embedded Daemon at `bin` (`~/.xshell/server/<dir>/xshelld`, `xshelld.exe`
/// on Windows).
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
        // The latest release, for Ring Hosts that update themselves from it (D3); never
        // waited for.
        latest::spawn(
            hosts.manager.clone(),
            latest::get_base(&|k| std::env::var_os(k)),
        );
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
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let set_up = retry_setup(
            &self.current,
            &self.daemon,
            &self.manager,
            &self.version,
            &self.service,
            (self.home_of)(),
            exe_dir.as_deref(),
            quit::Lifetime::of_platform(),
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
        (local::LocalMode::Failed { reason }, _) => {
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

/// A Retry rebuilds the selection from scratch (always-on; window-bound only from `Failed`,
/// A1): the Daemon binary is resolved again (a stale sidecar may be gone, the home may be
/// known now) and a new Local Host Daemon set up, which the Local Host's dialer uses from
/// its next dial. A window-bound `LocalDaemon` is never replaced: dropping it closes its Job
/// and would end the Daemon it started with every Terminal; Retry then only redials (and
/// respawns in the same Job). Always clears the start error and kicks the Local Host.
/// Returns whether a Daemon was set up anew.
#[allow(clippy::too_many_arguments)]
fn retry_setup(
    current: &Mutex<local::LocalMode>,
    slot: &DaemonSlot,
    manager: &Manager,
    version: &str,
    svc: &Arc<service::ServiceState>,
    home: Option<std::path::PathBuf>,
    exe_dir: Option<&std::path::Path>,
    lifetime: quit::Lifetime,
) -> bool {
    let mut set_up = false;
    let failed = matches!(*current.lock().unwrap(), local::LocalMode::Failed { .. });
    if failed || lifetime == quit::Lifetime::AlwaysOn {
        let (mode, daemon) = setup_local(version, exe_dir, svc, home);
        *current.lock().unwrap() = mode;
        let had = slot.lock().unwrap().is_some();
        *slot.lock().unwrap() = daemon.clone();
        if daemon.is_some() {
            if !had || manager.access(LOCAL_HOST_ID).is_none() {
                manager.set_local(local_host_config());
            }
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

/// Move this machine off an older or other build's Local Host Daemon by itself (Unix,
/// ADR-0017): the same update as "Replace" (this Desktop's build is installed and selected,
/// the old Daemon persists its Terminals and exits, the new one restores them: agents resume,
/// shells start fresh in the same directory). Once per connection and build; on a worker,
/// since observer callbacks may run under hostlink's locks.
#[cfg(unix)]
fn auto_replace_local(app: &AppHandle, s: &HostStatus) {
    type Seen = (u64, u64, Option<xshell_protocol::build::BuildId>);
    static WORKER: std::sync::OnceLock<Mutex<std::sync::mpsc::Sender<HostStatus>>> =
        std::sync::OnceLock::new();
    let version = app.package_info().version.to_string();
    if !local::auto_replace_wanted(s, &version, crate::build_id::BUILD.as_ref()) {
        return;
    }
    let tx = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<HostStatus>();
        let app = app.clone();
        let _ = std::thread::Builder::new()
            .name("local-replace".into())
            .spawn(move || {
                let mut seen: Option<Seen> = None;
                while let Ok(st) = rx.recv() {
                    let key = (
                        st.config_generation,
                        st.link_generation,
                        st.daemon_build.clone(),
                    );
                    if seen.as_ref() == Some(&key) {
                        continue;
                    }
                    let Some(h) = app.try_state::<Hosts>() else {
                        continue;
                    };
                    let Some(access) = h.manager.access(LOCAL_HOST_ID) else {
                        continue;
                    };
                    seen = Some(key);
                    eprintln!(
                        "xshell: moving this machine's terminals to this version's background program"
                    );
                    access.upgrade(Box::new(|r| {
                        if let Err(e) = r {
                            eprintln!("xshell: the background program was not replaced: {e:?}");
                        }
                    }));
                }
            });
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(s.clone());
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
            #[cfg(unix)]
            auto_replace_local(&self.0, s);
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
        self.local.lock().unwrap().as_ref()?;
        Some(Box::new(SlotDialer(self.local.clone())))
    }
}

/// The Local Host's dialer: the Local Host Daemon *now* (a Retry may replace it), dialed as
/// a [`xshell_hostlink::LocalDialer`] on every call.
struct SlotDialer(DaemonSlot);

impl SlotDialer {
    fn now(&self) -> Option<xshell_hostlink::LocalDialer> {
        let daemon = self.0.lock().unwrap().clone()?;
        Some(xshell_hostlink::LocalDialer { daemon })
    }
}

impl xshell_hostlink::Dialer for SlotDialer {
    fn dial(
        &self,
        cancel: &xshell_hostlink::CancelToken,
    ) -> Result<xshell_hostlink::Dialed, xshell_hostlink::DialError> {
        match self.now() {
            Some(d) => d.dial(cancel),
            None => Err(xshell_hostlink::DialError::failed(
                "the local Daemon is not set up",
                None,
            )),
        }
    }
    fn describe(&self) -> String {
        self.now().map(|d| d.describe()).unwrap_or_default()
    }
    fn authorized(&self) -> bool {
        self.now().is_some_and(|d| d.authorized())
    }
    fn upgradable(&self) -> bool {
        self.now().is_some_and(|d| d.upgradable())
    }
    fn stop_incompatible(
        &self,
        cancel: &xshell_hostlink::CancelToken,
    ) -> Option<Result<(), xshell_hostlink::HostError>> {
        self.now()?.stop_incompatible(cancel)
    }
    fn select_build(&self, dir: &str) -> Option<Result<(), String>> {
        self.now()?.select_build(dir)
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

/// The Local Host's setup and Retry, the same on every platform (W2, A1).
#[cfg(test)]
mod lifetime_tests {
    use super::*;

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

    fn manager(slot: &DaemonSlot) -> Manager {
        Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(SshFactory {
                local: slot.clone(),
            }),
            Arc::new(NoBinaries),
            Arc::new(Quiet),
        ))
    }

    /// A Daemon that cannot be set up is the error view with this home's log, on every
    /// platform (never in-process).
    #[test]
    fn failed_info_names_the_log_on_every_platform() {
        let t = tempfile::tempdir().unwrap();
        let svc = Arc::new(service::ServiceState::default());
        let slot: DaemonSlot = Arc::new(Mutex::new(None));
        let m = manager(&slot);
        let mode = local::LocalMode::Failed {
            reason: "the home directory is unknown".into(),
        };
        let info = info_of(&mode, None, &m, &svc, Some(t.path()));
        assert_eq!(info.mode, "failed");
        let d = info.daemon.expect("daemon info in mode failed");
        assert_eq!(d.state, "failed");
        assert_eq!(
            d.log,
            Some(
                xshell_hostlink::local::local_log_path(t.path())
                    .display()
                    .to_string()
            )
        );
        assert_eq!(info.service, None);
        // Without a home: no Daemon, and still not in-process.
        let (mode, daemon) = setup_local("1.5.0", Some(t.path()), &svc, None);
        assert!(daemon.is_none());
        assert!(
            matches!(&mode, local::LocalMode::Failed { reason } if reason.contains("home directory is unknown")),
            "{mode:?}"
        );
    }

    /// A1: Retry never replaces a window-bound `LocalDaemon` (dropping it would close its
    /// Job and end the Daemon it started); it only clears the error and redials. From
    /// `Failed` (no `LocalDaemon` was ever built) it sets one up.
    #[test]
    fn retry_never_replaces_a_window_bound_daemon() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let svc = Arc::new(service::ServiceState::default());
        let d = local_daemon(t.path().join("no-xshelld"), "1.5.0", Some(home.clone())).unwrap();
        let slot: DaemonSlot = Arc::new(Mutex::new(Some(d.clone())));
        let m = manager(&slot);
        m.set_local(local_host_config());
        let current = Mutex::new(local::LocalMode::Daemon {
            bin: t.path().join("no-xshelld"),
            source: local::DaemonSource::File,
        });
        assert!(!retry_setup(
            &current,
            &slot,
            &m,
            "1.5.0",
            &svc,
            Some(home.clone()),
            Some(t.path()),
            quit::Lifetime::WindowBound
        ));
        let kept = slot.lock().unwrap().clone().unwrap();
        assert!(
            Arc::ptr_eq(&kept, &d),
            "the window-bound Daemon was replaced"
        );
        // From Failed: set up.
        *slot.lock().unwrap() = None;
        *current.lock().unwrap() = local::LocalMode::Failed {
            reason: "the home directory is unknown".into(),
        };
        assert!(retry_setup(
            &current,
            &slot,
            &m,
            "1.5.0",
            &svc,
            Some(home.clone()),
            Some(t.path()),
            quit::Lifetime::WindowBound
        ));
        assert!(slot.lock().unwrap().is_some());
        assert!(!matches!(
            &*current.lock().unwrap(),
            local::LocalMode::Failed { .. }
        ));
        m.shutdown();
    }
}

/// Native Windows tests of the window-bound Local Host (W8). A test build must embed the
/// `xshelld.exe` of the same build (`XSHELL_DAEMON_ASSET`/`_SHA256`).
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
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

    fn unique_pipe() -> std::path::PathBuf {
        std::path::PathBuf::from(format!(
            r"\\.\pipe\xshell-desktop-test-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// A user-owned private test home (an elevated token's default owner may be
    /// Administrators).
    fn private_home(t: &tempfile::TempDir) -> std::path::PathBuf {
        let home = t.path().join("home");
        xshell_core::private_fs::ensure_dir(&home).unwrap();
        xshell_core::private_fs::ensure_dir(&home.join(".xshell")).unwrap();
        home
    }

    /// A `LocalDaemon` on its own pipe and home, as `local_daemon` builds it.
    fn test_daemon(
        bin: std::path::PathBuf,
        home: &std::path::Path,
        pipe: &std::path::Path,
    ) -> Arc<xshell_hostlink::LocalDaemon> {
        Arc::new(
            xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
                bin,
                env: vec![
                    ("XSHELLD_HOME".into(), home.as_os_str().to_owned()),
                    ("XSHELLD_SOCKET".into(), pipe.as_os_str().to_owned()),
                ],
                home: home.to_path_buf(),
                socket: pipe.to_path_buf(),
                version: env!("CARGO_PKG_VERSION").into(),
                build: crate::build_id::BUILD.clone(),
            })
            .unwrap(),
        )
    }

    fn manager_for(d: &Arc<xshell_hostlink::LocalDaemon>) -> Manager {
        let slot: DaemonSlot = Arc::new(Mutex::new(Some(d.clone())));
        let m = Manager::new(ManagerConfig::new(
            env!("CARGO_PKG_VERSION"),
            Arc::new(SshFactory { local: slot }),
            Arc::new(NoBinaries),
            Arc::new(Quiet),
        ));
        m.set_local(local_host_config());
        m
    }

    fn embedded() -> &'static [u8] {
        binaries::embedded_own()
            .expect("build the Windows Desktop tests with XSHELL_DAEMON_ASSET/_SHA256 set")
    }

    /// W2: a Daemon that cannot start is the failed state with the log path, never
    /// in-process.
    #[test]
    fn no_in_process_fallback_when_daemon_cannot_start() {
        let t = tempfile::tempdir().unwrap();
        let home = private_home(&t);
        let not_a_daemon = t.path().join("not-xshelld.exe");
        std::fs::write(&not_a_daemon, b"MZ not a Daemon").unwrap();
        let env: std::collections::HashMap<&str, std::ffi::OsString> =
            [("XSHELL_DAEMON_BIN", not_a_daemon.clone().into_os_string())].into();
        let mode = local::resolve_local(
            Some(t.path()),
            &|k| env.get(k).cloned(),
            true,
            None,
            &|_| true,
        );
        assert_eq!(
            mode,
            local::LocalMode::Daemon {
                bin: not_a_daemon.clone(),
                source: local::DaemonSource::File
            }
        );
        let d = test_daemon(not_a_daemon.clone(), &home, &unique_pipe());
        let m = manager_for(&d);
        let svc = Arc::new(service::ServiceState::default());
        let deadline = Instant::now() + Duration::from_secs(30);
        let info = loop {
            let info = info_of(&mode, Some(&d), &m, &svc, Some(&home));
            if info.daemon.as_ref().is_some_and(|d| d.state == "failed")
                || Instant::now() >= deadline
            {
                break info;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(info.mode, "daemon");
        let daemon = info.daemon.expect("daemon info");
        assert_eq!(daemon.state, "failed", "{daemon:?}");
        assert_eq!(
            daemon.log,
            Some(
                xshell_hostlink::local::local_log_path(&home)
                    .display()
                    .to_string()
            )
        );
        assert_eq!(info.service, None);
        m.shutdown();
    }

    /// W2: the embedded `xshelld.exe` is installed privately under
    /// `~\.xshell\server\<dir>`; installing again while it runs leaves it alone; the
    /// GUI-bound spawn runs that very file.
    #[test]
    fn embedded_install_and_start() {
        let t = tempfile::tempdir().unwrap();
        let home = private_home(&t);
        let dir = xshell_protocol::build::install_dir_name(
            env!("CARGO_PKG_VERSION"),
            crate::build_id::BUILD.as_ref(),
        );
        let bin = xshell_hostlink::install::install_local_bytes(embedded(), &home, &dir).unwrap();
        assert_eq!(bin, xshell_hostlink::install::local_server_bin(&home, &dir));
        assert_eq!(
            own_install_bin(env!("CARGO_PKG_VERSION"), Some(&home)),
            Some(bin.clone())
        );
        assert_eq!(std::fs::read(&bin).unwrap(), embedded());
        let id = xshell_core::server::native_identity(&bin).unwrap();
        assert_eq!(id.version, env!("CARGO_PKG_VERSION"));
        let d = test_daemon(bin.clone(), &home, &unique_pipe());
        let pid = d.ensure_spawned().unwrap();
        assert_eq!(d.gui_pid(), Some(pid));
        let before = std::fs::metadata(&bin).unwrap().modified().unwrap();
        // Installed again while it runs: accepted, not rewritten.
        assert_eq!(
            install_embedded(&bin).unwrap(),
            bin,
            "a second install while it runs"
        );
        assert_eq!(std::fs::metadata(&bin).unwrap().modified().unwrap(), before);
        d.hang_up();
        d.reap(Instant::now() + Duration::from_secs(10));
    }

    /// W4: the process whose `LocalDaemon` spawned the Daemon owns it (closing asks); a
    /// second `LocalDaemon` that only connected to it does not (closing never asks).
    #[test]
    fn close_decision_owner_vs_non_owner() {
        let t = tempfile::tempdir().unwrap();
        let home = private_home(&t);
        let dir = xshell_protocol::build::install_dir_name(
            env!("CARGO_PKG_VERSION"),
            crate::build_id::BUILD.as_ref(),
        );
        let bin = xshell_hostlink::install::install_local_bytes(embedded(), &home, &dir).unwrap();
        let pipe = unique_pipe();
        let first = test_daemon(bin.clone(), &home, &pipe);
        let cancel = xshell_hostlink::CancelToken::new();
        drop(first.connect_or_start(&cancel).unwrap());
        let second = test_daemon(bin.clone(), &home, &pipe);
        drop(second.connect_or_start(&cancel).unwrap());
        let inputs = |d: &xshell_hostlink::LocalDaemon| quit::CloseInputs {
            lifetime: quit::Lifetime::WindowBound,
            attached: true,
            bypass: false,
            keep: local::KeepSessions::Ask,
            last_desktop: true,
            last_window: true,
            running: Some(1),
            owner: d.gui_pid().is_some(),
            session_ending: false,
        };
        assert_eq!(
            quit::decide(&inputs(&first)),
            quit::CloseDecision::Confirm(Some(1))
        );
        assert_eq!(quit::decide(&inputs(&second)), quit::CloseDecision::Close);
        first.hang_up();
        first.reap(Instant::now() + Duration::from_secs(10));
    }

    /// A1: Retry while the dial is spawning leaves the one `LocalDaemon` (and its Daemon)
    /// in place; this process stays the owner.
    #[test]
    fn retry_while_spawning_keeps_one_daemon_and_owner() {
        let t = tempfile::tempdir().unwrap();
        let home = private_home(&t);
        let dir = xshell_protocol::build::install_dir_name(
            env!("CARGO_PKG_VERSION"),
            crate::build_id::BUILD.as_ref(),
        );
        let bin = xshell_hostlink::install::install_local_bytes(embedded(), &home, &dir).unwrap();
        let d = test_daemon(bin.clone(), &home, &unique_pipe());
        let slot: DaemonSlot = Arc::new(Mutex::new(Some(d.clone())));
        let m = Manager::new(ManagerConfig::new(
            env!("CARGO_PKG_VERSION"),
            Arc::new(SshFactory {
                local: slot.clone(),
            }),
            Arc::new(NoBinaries),
            Arc::new(Quiet),
        ));
        m.set_local(local_host_config());
        let current = Mutex::new(local::LocalMode::Daemon {
            bin: bin.clone(),
            source: local::DaemonSource::File,
        });
        let svc = Arc::new(service::ServiceState::default());
        let pid = d.ensure_spawned().unwrap();
        assert!(!retry_setup(
            &current,
            &slot,
            &m,
            env!("CARGO_PKG_VERSION"),
            &svc,
            Some(home.clone()),
            Some(t.path()),
            quit::Lifetime::of_platform()
        ));
        let kept = slot.lock().unwrap().clone().unwrap();
        assert!(Arc::ptr_eq(&kept, &d));
        assert_eq!(kept.gui_pid(), Some(pid), "the spawned Daemon still runs");
        m.shutdown();
        d.hang_up();
        d.reap(Instant::now() + Duration::from_secs(10));
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
        assert!(!retry_setup(
            &current,
            &slot,
            &m,
            "1.5.0",
            &svc,
            None,
            Some(t.path()),
            quit::Lifetime::AlwaysOn
        ));
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
            Some(home.clone()),
            Some(t.path()),
            quit::Lifetime::AlwaysOn
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

    /// An `xshelld` stand-in answering `--version` with `build` (a real file on disk).
    fn version_script(path: &std::path::Path, build: &serde_json::Value) {
        use std::os::unix::fs::PermissionsExt;
        let line = serde_json::json!({
            "name": "xshelld", "version": "1.5.0", "protocol": {"min": 1, "max": 1},
            "os": "Linux", "arch": "x86_64", "build": build
        });
        std::fs::write(path, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A Daemon like 42c9be9 (the build before "Ring just works"): answers the old hello
    /// with its own build, lists `terminals`, refuses requests it does not know, and on
    /// `daemon.upgrade` answers and exits (its socket goes away). Returns what it was asked.
    fn fake_old_daemon(sock: std::path::PathBuf) -> Arc<Mutex<Vec<String>>> {
        use xshell_protocol::frame::{read_frame, write_frame, Frame, MAX_FRAME_LEN};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let log = seen.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut r = c.try_clone().unwrap();
                let mut w = c;
                let send = |w: &mut std::os::unix::net::UnixStream, v: serde_json::Value| {
                    let _ = write_frame(w, &Frame::Json(serde_json::to_vec(&v).unwrap()));
                };
                let mut exit = false;
                while let Ok(Some(Frame::Json(body))) = read_frame(&mut r, MAX_FRAME_LEN) {
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let t = v["t"].as_str().unwrap_or("").to_string();
                    log.lock().unwrap().push(t.clone());
                    match t.as_str() {
                        "hello" => {
                            use std::io::Write as _;
                            let hello = xshell_protocol::msg::ServerMsg::Hello(
                                xshell_protocol::msg::Hello {
                                    protocol: xshell_protocol::msg::ProtocolRange {
                                        min: 1,
                                        max: 1,
                                    },
                                    version: "1.5.0".into(),
                                    capabilities: vec![
                                        "call".into(),
                                        "term".into(),
                                        "daemon.upgrade".into(),
                                    ],
                                    build: Some(xshell_protocol::build::BuildId {
                                        commit: "42c9be9636a9".into(),
                                        dirty: None,
                                        time: 1,
                                    }),
                                },
                            );
                            let _ = w.write_all(
                                &xshell_protocol::msg::encode_msg(&hello, None).unwrap(),
                            );
                            send(&mut w, serde_json::json!({"t": "terminals", "list": []}));
                        }
                        "daemon.upgrade" => {
                            send(
                                &mut w,
                                serde_json::json!({"t": "res", "id": v["id"], "ok": null}),
                            );
                            exit = true;
                            break;
                        }
                        _ if v.get("id").is_some() => send(
                            &mut w,
                            serde_json::json!({"t": "res", "id": v["id"], "err": format!("unknown request {t}")}),
                        ),
                        _ => {}
                    }
                }
                if exit {
                    // The old Daemon exits: nothing listens any more.
                    let _ = std::fs::remove_file(&sock);
                    return;
                }
            }
        });
        seen
    }

    /// P2: a 42c9be9-like Daemon left running by the old Desktop is replaced by this
    /// Desktop's build by itself: the status asks for it, the update installs and selects
    /// this build and sends `daemon.upgrade`; no error is left for the UI.
    #[test]
    fn old_build_daemon_is_replaced_by_itself() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("h");
        std::fs::create_dir_all(&home).unwrap();
        let sock = t.path().join("d.sock");
        let seen = fake_old_daemon(sock.clone());
        // The app's xshelld: this Desktop's build (so the update may install it); never
        // serves in this test.
        let bin = t.path().join("xshelld");
        version_script(&bin, &serde_json::json!(*crate::build_id::BUILD));
        let d = Arc::new(
            xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
                bin,
                env: vec![],
                home: home.clone(),
                socket: sock,
                version: env!("CARGO_PKG_VERSION").into(),
                build: crate::build_id::BUILD.clone(),
            })
            .unwrap(),
        );
        d.set_persistent(true);
        let slot: DaemonSlot = Arc::new(Mutex::new(Some(d.clone())));
        let mut cfg = ManagerConfig::new(
            env!("CARGO_PKG_VERSION"),
            Arc::new(SshFactory { local: slot }),
            Arc::new(NoBinaries),
            Arc::new(Quiet),
        );
        cfg.desktop_build = crate::build_id::BUILD.clone();
        let m = Manager::new(cfg);
        m.set_local(local_host_config());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let st = loop {
            let st = m.access(LOCAL_HOST_ID).unwrap().status();
            if local::auto_replace_wanted(
                &st,
                env!("CARGO_PKG_VERSION"),
                crate::build_id::BUILD.as_ref(),
            ) || std::time::Instant::now() >= deadline
            {
                break st;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        if crate::build_id::BUILD.is_none() {
            return;
        }
        assert!(
            local::auto_replace_wanted(
                &st,
                env!("CARGO_PKG_VERSION"),
                crate::build_id::BUILD.as_ref()
            ),
            "{st:#?} seen {:?}",
            seen.lock().unwrap()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        m.access(LOCAL_HOST_ID).unwrap().upgrade(Box::new(move |r| {
            let _ = tx.send(r.map(|_| ()).map_err(|e| format!("{e:?}")));
        }));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !seen.lock().unwrap().iter().any(|t| t == "daemon.upgrade") {
            assert!(
                std::time::Instant::now() < deadline,
                "no daemon.upgrade: {:?} / {:?}",
                seen.lock().unwrap(),
                rx.try_recv()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // This Desktop's build was installed and selected first.
        let dir = d.install_dir();
        assert_eq!(
            std::fs::read_to_string(home.join(".xshell/server/selected"))
                .unwrap()
                .trim(),
            dir
        );
        d.hang_up();
        m.shutdown();
    }

    /// Retry rebuilds the selection from scratch: after the stale sidecar is renamed away,
    /// the old resolved path is no longer used.
    #[test]
    fn retry_re_resolves_after_the_stale_sidecar_is_gone() {
        if std::env::var_os("XSHELL_DAEMON_BIN").is_some() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("bin");
        let home = t.path().join("home");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let sidecar = app.join("xshelld");
        version_script(
            &sidecar,
            &serde_json::json!({"commit": "42c9be9636a9", "dirty": null, "time": 1}),
        );
        let svc = Arc::new(service::ServiceState::default());
        let (mode, daemon) = setup_local("1.5.0", Some(&app), &svc, Some(home.clone()));
        let current = Mutex::new(mode.clone());
        let slot: DaemonSlot = Arc::new(Mutex::new(daemon));
        let m = Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(SshFactory {
                local: slot.clone(),
            }),
            Arc::new(NoBinaries),
            Arc::new(Quiet),
        ));
        m.set_local(local_host_config());
        let first = slot.lock().unwrap().clone().unwrap();
        if binaries::embedded_own().is_none() {
            // No embedded Daemon: the stale sidecar is selected (and refused at start).
            assert_eq!(
                mode,
                local::LocalMode::Daemon {
                    bin: sidecar.clone(),
                    source: local::DaemonSource::File
                }
            );
        }
        // The cause is removed: the stale file renamed away. Retry selects anew.
        std::fs::rename(&sidecar, app.join("xshelld.old")).unwrap();
        assert!(retry_setup(
            &current,
            &slot,
            &m,
            "1.5.0",
            &svc,
            Some(home.clone()),
            Some(&app),
            quit::Lifetime::AlwaysOn
        ));
        let second = slot.lock().unwrap().clone().unwrap();
        assert!(
            !Arc::ptr_eq(&first, &second),
            "the old Daemon setup was kept"
        );
        // The stale file is no longer selected: now the embedded Daemon, or (a build without
        // one) "this build carries no xshelld".
        assert!(!matches!(
            &*current.lock().unwrap(),
            local::LocalMode::Daemon { bin, source: local::DaemonSource::File } if bin == &sidecar
        ));
        // The Local Host's dialer follows the new setup at its next dial.
        let factory = SshFactory {
            local: slot.clone(),
        };
        let dialer = factory.direct(&local_host_config()).unwrap();
        assert_eq!(
            dialer.describe(),
            second.live_socket().display().to_string()
        );
        m.shutdown();
    }
}

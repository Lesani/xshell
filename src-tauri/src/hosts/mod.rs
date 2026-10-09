//! Remote Host connections: Tauri glue over `xshell_hostlink::Manager`. Status changes and
//! `terminals` lists become events; Terminal output goes through the same `Channel`s local
//! Terminals use. The Local Host is one of the Manager's Hosts too (id `local`) when its
//! Terminals run in a Daemon ([`local::LocalMode::Daemon`]), GUI-bound or Persistent.

pub mod binaries;
pub mod commands;
pub mod local;

use serde::Serialize;
use std::sync::Arc;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Emitter, Manager as _};
#[cfg(unix)]
use xshell_hostlink::LOCAL_HOST_ID;
use xshell_hostlink::{
    HostConfig, HostStatus, Manager, ManagerConfig, Observer, SshTransport, TermSink, Transport,
    TransportFactory,
};
use xshell_protocol::msg::TerminalInfo;

pub struct Hosts {
    pub manager: Arc<Manager>,
    /// How local Tabs run, as resolved at startup.
    pub local_mode: local::LocalMode,
    /// The Daemon this app may start for the Local Host (Daemon mode only).
    #[cfg(unix)]
    pub local_daemon: Option<Arc<xshell_hostlink::LocalDaemon>>,
    /// `settings.json`, for the Persistent Daemon setting.
    pub store: Option<Arc<dyn local::SettingStore>>,
    /// One switch of the Daemon mode at a time.
    pub switch_lock: tokio::sync::Mutex<()>,
}

/// Linux or macOS: where the Persistent Daemon setting is offered.
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

impl Hosts {
    pub fn new(app: &AppHandle) -> Self {
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
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut local_mode =
            local::resolve_daemon_binary(exe_dir.as_deref(), &|k| std::env::var_os(k));
        #[cfg(unix)]
        let local_daemon = match &local_mode {
            local::LocalMode::Daemon { bin } => match local_daemon(bin.clone(), &version) {
                Ok(d) => Some(d),
                Err(reason) => {
                    local_mode = local::LocalMode::InProcess { reason };
                    None
                }
            },
            local::LocalMode::InProcess { .. } => None,
        };
        // Before the Local Host starts dialing: the first Daemon starts in the stored mode.
        #[cfg(unix)]
        if let Some(d) = &local_daemon {
            let on = local::persistent_supported(&local_mode, linux_or_macos())
                && local::read_persistent(store.as_deref());
            d.set_persistent(on);
        }
        match &local_mode {
            local::LocalMode::Daemon { bin } => {
                eprintln!("xshell: local terminals run in {}", bin.display())
            }
            local::LocalMode::InProcess { reason } => {
                eprintln!("xshell: local terminals run in the app: {reason}")
            }
        }
        let cache_dir = app
            .path()
            .app_cache_dir()
            .unwrap_or_else(|_| std::env::temp_dir().join("xshell-cache"));
        let factory = SshFactory {
            #[cfg(unix)]
            local: local_daemon.clone(),
        };
        let cfg = ManagerConfig::new(
            version,
            Arc::new(factory),
            Arc::new(binaries::production_source(exe_dir, cache_dir)),
            Arc::new(TauriObserver(app.clone())),
        );
        let manager = Arc::new(Manager::new(cfg));
        #[cfg(unix)]
        if local_daemon.is_some() {
            manager.set_local(HostConfig {
                id: LOCAL_HOST_ID.into(),
                name: LOCAL_HOST_ID.into(),
                ssh_target: String::new(),
                color: None,
                daemon_command: None,
                launch_prefixes: Default::default(),
            });
        }
        Self {
            manager,
            local_mode,
            #[cfg(unix)]
            local_daemon,
            store,
            switch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// What `local_host_info` answers.
    pub fn local_info(&self) -> local::LocalHostInfo {
        let supported = local::persistent_supported(&self.local_mode, linux_or_macos());
        #[cfg(unix)]
        let p = match (&self.local_daemon, supported) {
            (Some(d), true) => {
                let usable = self.manager.host(LOCAL_HOST_ID).is_some_and(|h| {
                    matches!(
                        h.status().status,
                        xshell_hostlink::StatusKind::Connected
                            | xshell_hostlink::StatusKind::UpgradePending
                    )
                });
                local::PersistentInfo {
                    supported,
                    enabled: d.persistent(),
                    running: usable.then(|| d.running_mode()).flatten(),
                    log: Some(d.log.display().to_string()),
                }
            }
            _ => local::PersistentInfo::default(),
        };
        #[cfg(not(unix))]
        let p = {
            let _ = supported;
            local::PersistentInfo::default()
        };
        self.local_mode.info(p)
    }

    /// Quitting: end the GUI-bound Daemon this app started (and with it every local
    /// Terminal), stop every Host link, then wait for its orderly exit (it ends Terminals
    /// within about 8 s) before killing it. A Persistent Daemon, and a Daemon this app did
    /// not start, are left alone.
    pub fn quit(&self) {
        #[cfg(unix)]
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        #[cfg(unix)]
        if let Some(d) = &self.local_daemon {
            d.hang_up();
        }
        // Kill every ssh; Remote Daemons and their Terminals keep running.
        self.manager.shutdown();
        #[cfg(unix)]
        if let Some(d) = &self.local_daemon {
            d.reap(deadline);
        }
    }
}

/// The Local Host's Daemon, reached on this user's Daemon socket.
#[cfg(unix)]
fn local_daemon(
    bin: std::path::PathBuf,
    version: &str,
) -> Result<Arc<xshell_hostlink::LocalDaemon>, String> {
    let home = dirs::home_dir().ok_or("the home directory is unknown")?;
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let socket = xshell_hostlink::local::local_socket_path(&home, xdg.as_deref());
    xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin,
        env: vec![],
        home,
        socket,
        version: version.into(),
    })
    .map(Arc::new)
    .map_err(|e| format!("cannot start the keeper thread: {e}"))
}

#[derive(Serialize, Clone)]
struct TerminalsEvent<'a> {
    host: &'a str,
    list: &'a [TerminalInfo],
}

struct TauriObserver(AppHandle);

impl Observer for TauriObserver {
    fn status(&self, s: &HostStatus) {
        let _ = self.0.emit("hosts:status", s);
    }
    fn terminals(&self, host: &str, list: &[TerminalInfo]) {
        let _ = self
            .0
            .emit("hosts:terminals", TerminalsEvent { host, list });
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
}

impl TermSink for ChannelSink {
    fn data(&self, bytes: &[u8]) -> bool {
        self.data.send(Response::new(bytes.to_vec())).is_ok()
    }
    fn exit(&self, code: i32, bytes: u64) {
        let _ = self.exit.send(RemoteExit { code, bytes });
    }
}

struct SshFactory {
    #[cfg(unix)]
    local: Option<Arc<xshell_hostlink::LocalDaemon>>,
}

impl TransportFactory for SshFactory {
    fn for_host(&self, cfg: &HostConfig) -> Box<dyn Transport> {
        Box::new(SshTransport::new(cfg.ssh_target.clone()))
    }

    /// The Local Host: this user's Daemon socket, starting its Daemon if need be.
    #[cfg(unix)]
    fn direct(&self, cfg: &HostConfig) -> Option<Box<dyn xshell_hostlink::Dialer>> {
        if cfg.id != LOCAL_HOST_ID {
            return None;
        }
        let daemon = self.local.clone()?;
        Some(Box::new(xshell_hostlink::LocalDialer { daemon }))
    }
}

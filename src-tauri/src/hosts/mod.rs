//! Remote Host connections: Tauri glue over `xshell_hostlink::Manager`. Status changes and
//! `terminals` lists become events; Terminal output goes through the same `Channel`s local
//! Terminals use.

pub mod binaries;
pub mod commands;

use serde::Serialize;
use std::sync::Arc;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Emitter, Manager as _};
use xshell_hostlink::{
    HostConfig, HostStatus, Manager, ManagerConfig, Observer, SshTransport, TermSink, Transport,
    TransportFactory,
};
use xshell_protocol::msg::TerminalInfo;

pub struct Hosts {
    pub manager: Arc<Manager>,
}

impl Hosts {
    pub fn new(app: &AppHandle) -> Self {
        let version = app.package_info().version.to_string();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let cache_dir = app
            .path()
            .app_cache_dir()
            .unwrap_or_else(|_| std::env::temp_dir().join("xshell-cache"));
        let cfg = ManagerConfig::new(
            version,
            Arc::new(SshFactory),
            Arc::new(binaries::production_source(exe_dir, cache_dir)),
            Arc::new(TauriObserver(app.clone())),
        );
        Self {
            manager: Arc::new(Manager::new(cfg)),
        }
    }
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

struct SshFactory;

impl TransportFactory for SshFactory {
    fn for_host(&self, cfg: &HostConfig) -> Box<dyn Transport> {
        Box::new(SshTransport::new(cfg.ssh_target.clone()))
    }
}

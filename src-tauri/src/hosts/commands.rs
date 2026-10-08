//! The `host_*` / `hosts_*` commands. Errors are `HostError` objects, except where the
//! contract says `String` (invalid configuration). `host_term_input` and `host_term_resize`
//! are sync so they run in IPC order on the main thread; they only enqueue.

use super::{ChannelSink, Hosts, RemoteExit};
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use tauri::ipc::{Channel, Response};
use tauri::State;
use tokio::sync::oneshot;
use uuid::Uuid;
use xshell_core::protocol::msg::OpenSpec;
use xshell_core::LaunchSpec;
use xshell_hostlink::config::validate_one;
use xshell_hostlink::{HostConfig, HostError, HostHandle, HostSnapshot, HostTestResult};

fn handle(state: &Hosts, host: &str) -> Result<Arc<HostHandle>, HostError> {
    state
        .manager
        .host(host)
        .ok_or_else(|| HostError::unknown_host(host))
}

fn uuid(s: &str) -> Result<Uuid, HostError> {
    Uuid::parse_str(s).map_err(|_| HostError::invalid(format!("invalid terminal id {s:?}")))
}

/// Bridge a hostlink callback to an await.
async fn reply<T: Send + 'static>(
    start: impl FnOnce(Box<dyn FnOnce(Result<T, HostError>) + Send>),
) -> Result<T, HostError> {
    let (tx, rx) = oneshot::channel();
    start(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    rx.await
        .unwrap_or_else(|_| Err(HostError::offline("the request was dropped")))
}

#[tauri::command]
pub async fn hosts_configure(
    state: State<'_, Hosts>,
    hosts: Vec<HostConfig>,
) -> Result<(), String> {
    let m = state.manager.clone();
    // Restarting a Host joins its supervisor (bounded); keep that off the runtime workers.
    tauri::async_runtime::spawn_blocking(move || m.configure(hosts))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn hosts_status(state: State<'_, Hosts>) -> Vec<HostSnapshot> {
    state.manager.snapshot()
}

#[tauri::command]
pub fn hosts_kick(state: State<'_, Hosts>, host: Option<String>) {
    match host {
        Some(h) => {
            if let Some(h) = state.manager.host(&h) {
                h.kick();
            }
        }
        None => state.manager.kick_all(),
    }
}

#[tauri::command]
pub async fn host_call(
    state: State<'_, Hosts>,
    host: String,
    method: String,
    params: Value,
) -> Result<Value, HostError> {
    let h = handle(&state, &host)?;
    reply(|w| h.call(method, params, w)).await
}

#[derive(Serialize)]
pub struct OpenResult {
    pid: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachResult {
    exit_code: Option<i32>,
}

// The argument list is the frontend IPC contract.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn host_term_open(
    state: State<'_, Hosts>,
    host: String,
    terminal: String,
    spec: LaunchSpec,
    meta: Map<String, Value>,
    cols: u16,
    rows: u16,
    on_data: Channel<Response>,
    on_exit: Channel<RemoteExit>,
) -> Result<OpenResult, HostError> {
    let h = handle(&state, &host)?;
    let open = OpenSpec {
        terminal: uuid(&terminal)?,
        launch: spec,
        cols,
        rows,
        meta,
    };
    let sink = Arc::new(ChannelSink {
        data: on_data,
        exit: on_exit,
    });
    let pid = reply(|w| h.term_open(open, sink, w)).await?;
    Ok(OpenResult { pid })
}

#[tauri::command]
pub async fn host_term_attach(
    state: State<'_, Hosts>,
    host: String,
    terminal: String,
    on_data: Channel<Response>,
    on_exit: Channel<RemoteExit>,
) -> Result<AttachResult, HostError> {
    let h = handle(&state, &host)?;
    let t = uuid(&terminal)?;
    let sink = Arc::new(ChannelSink {
        data: on_data,
        exit: on_exit,
    });
    let exit_code = reply(|w| h.term_attach(t, sink, w)).await?;
    Ok(AttachResult { exit_code })
}

#[tauri::command]
pub fn host_term_detach(
    state: State<'_, Hosts>,
    host: String,
    terminal: String,
) -> Result<(), HostError> {
    handle(&state, &host)?.term_detach(uuid(&terminal)?)
}

#[tauri::command]
pub fn host_term_input(
    state: State<'_, Hosts>,
    host: String,
    terminal: String,
    data: String,
) -> Result<(), HostError> {
    handle(&state, &host)?.term_input(uuid(&terminal)?, data)
}

#[tauri::command]
pub fn host_term_resize(
    state: State<'_, Hosts>,
    host: String,
    terminal: String,
    cols: u16,
    rows: u16,
) -> Result<(), HostError> {
    handle(&state, &host)?.term_resize(uuid(&terminal)?, cols, rows)
}

#[tauri::command]
pub async fn host_term_close(
    state: State<'_, Hosts>,
    host: String,
    terminal: String,
) -> Result<(), HostError> {
    let h = handle(&state, &host)?;
    let t = uuid(&terminal)?;
    reply(|w| h.term_close(t, w)).await.map(|_| ())
}

#[tauri::command]
pub async fn host_term_update(
    state: State<'_, Hosts>,
    host: String,
    terminal: String,
    session_id: Option<String>,
    meta: Option<Map<String, Value>>,
) -> Result<(), HostError> {
    let h = handle(&state, &host)?;
    let t = uuid(&terminal)?;
    reply(|w| h.term_update(t, session_id, meta, w))
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn host_upgrade(state: State<'_, Hosts>, host: String) -> Result<(), HostError> {
    let h = handle(&state, &host)?;
    reply(|w| h.upgrade(w)).await.map(|_| ())
}

#[tauri::command]
pub async fn host_test(
    state: State<'_, Hosts>,
    config: HostConfig,
) -> Result<HostTestResult, String> {
    validate_one(&config)?;
    let m = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || m.test(&config))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_ssh_hosts() -> Vec<String> {
    match dirs::home_dir() {
        Some(home) => xshell_hostlink::ssh_config::list_hosts(&home),
        None => Vec::new(),
    }
}

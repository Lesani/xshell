mod hosts;
mod local_pty;

use local_pty::LocalPtys;
use std::fs;
use std::sync::Arc;
use tauri::ipc::{Channel, Response};
use tauri::State;
use xshell_core::agents::AgentBinaryProbe;
use xshell_core::antigravity::AntigravityContext;
use xshell_core::claude::BranchInfo;
use xshell_core::codex::{CodexContext, CodexUsage};
use xshell_core::cursor::CursorContext;
use xshell_core::files::DirItem;
use xshell_core::git::{GitBranch, GitCommit, GitStatus};
use xshell_core::memories::ProjectMemories;
use xshell_core::opencode::OpencodeContext;
use xshell_core::sessions::{CodexProjectInfo, MessagePreview, ProjectInfo, SessionInfo};
use xshell_core::skills::ProjectSkills;
use xshell_core::stats::{ClaudeCostSummary, GlobalRateLimits, StatuslineProbe};
use xshell_core::{HostCtx, LaunchSpec};

pub struct AppState {
    ptys: Arc<LocalPtys>,
}

#[tauri::command]
fn read_image_base64(path: String) -> Result<String, String> {
    let data = fs::read(&path).map_err(|e| format!("Failed to read image: {}", e))?;
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        _ => "image/png",
    };
    use std::fmt::Write as FmtWrite;
    let mut base64 = String::new();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i] as u32;
        let b1 = if i + 1 < data.len() {
            data[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            data[i + 2] as u32
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        base64.push(alphabet[((triple >> 18) & 0x3F) as usize] as char);
        base64.push(alphabet[((triple >> 12) & 0x3F) as usize] as char);
        if i + 1 < data.len() {
            base64.push(alphabet[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            base64.push('=');
        }
        if i + 2 < data.len() {
            base64.push(alphabet[(triple & 0x3F) as usize] as char);
        } else {
            base64.push('=');
        }
        i += 3;
    }
    let _ = write!(base64, "");
    Ok(format!("data:{};base64,{}", mime, base64))
}

#[tauri::command]
fn reveal_in_explorer(path: String) -> Result<(), String> {
    use std::process::Command;
    // For file paths we want to open the containing folder (with the file selected on
    // platforms that support it). Without this, Windows' `explorer.exe <file>` would *open*
    // the file in its default app — e.g. launching VS Code for a .md — which is not what
    // "Reveal in Explorer" should do.
    let p = std::path::Path::new(&path);
    let is_file = p.is_file();
    let mut cmd;
    #[cfg(target_os = "windows")]
    {
        cmd = Command::new("explorer.exe");
        if is_file {
            // `/select,<path>` opens the parent folder and highlights the file.
            cmd.arg(format!("/select,{}", path));
        } else {
            cmd.arg(&path);
        }
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    #[cfg(target_os = "macos")]
    {
        cmd = Command::new("open");
        if is_file {
            cmd.arg("-R");
        } // reveal in Finder
        cmd.arg(&path);
    }
    #[cfg(target_os = "linux")]
    {
        cmd = Command::new("xdg-open");
        let target = if is_file {
            p.parent()
                .map(|pp| pp.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone())
        } else {
            path.clone()
        };
        cmd.arg(target);
    }
    cmd.spawn()
        .map_err(|e| format!("Failed to open explorer: {}", e))?;
    Ok(())
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    use std::process::Command;
    // Only http(s) — open_url is invoked from the frontend, and refusing other schemes
    // prevents an attacker-controlled URL from launching an arbitrary local handler.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http(s) urls are allowed".into());
    }
    let mut cmd;
    #[cfg(target_os = "windows")]
    {
        cmd = Command::new("cmd");
        // Empty "" arg is the window title slot — without it, `start` treats the URL as the title.
        cmd.args(["/c", "start", "", &url]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    #[cfg(target_os = "macos")]
    {
        cmd = Command::new("open");
        cmd.arg(&url);
    }
    #[cfg(target_os = "linux")]
    {
        cmd = Command::new("xdg-open");
        cmd.arg(&url);
    }
    cmd.spawn()
        .map_err(|e| format!("Failed to open url: {}", e))?;
    Ok(())
}

/// A Tab's output and exit channels.
struct ChannelSink {
    data: Channel<Response>,
    exit: Channel<i32>,
}

impl local_pty::Sink for ChannelSink {
    fn data(&self, bytes: Vec<u8>) -> bool {
        self.data.send(Response::new(bytes)).is_ok()
    }
    fn exit(&self, code: i32) {
        let _ = self.exit.send(code);
    }
}

// The argument list is the frontend IPC contract (`invoke('spawn_terminal', {...})`).
#[allow(clippy::too_many_arguments)]
#[tauri::command]
fn spawn_terminal(
    state: State<'_, AppState>,
    id: String,
    session_id: Option<String>,
    cwd: String,
    cols: u16,
    rows: u16,
    shell_mode: Option<String>,
    shell_command: Option<String>,
    shell_id: Option<String>,
    agent: Option<String>,
    fullscreen_rendering: Option<bool>,
    force_sync_output: Option<bool>,
    skip_permissions: Option<bool>,
    on_data: Channel<Response>,
    on_exit: Channel<i32>,
) -> Result<(), String> {
    let spec = LaunchSpec {
        agent,
        session_id,
        cwd,
        shell_mode,
        shell_command,
        shell_id,
        fullscreen_rendering,
        force_sync_output,
        skip_permissions,
        // Launch prefixes are per Remote Host; local Terminals run the agent directly.
        launch_prefix: None,
    };
    let sink = Arc::new(ChannelSink {
        data: on_data,
        exit: on_exit,
    });
    state.ptys.spawn(id, spec, cols, rows, sink)
}

#[tauri::command]
fn write_terminal(state: State<'_, AppState>, id: String, data: String) -> Result<(), String> {
    state.ptys.write(&id, data.as_bytes())
}

#[tauri::command]
fn resize_terminal(
    state: State<'_, AppState>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    state.ptys.resize(&id, cols, rows)
}

#[tauri::command]
fn close_terminal(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.ptys.close(&id);
    Ok(())
}

/// Restart a Terminal with `skipPermissions` changed, resuming `session_id` (the Tab's
/// current session). Answers whether it restarted (`false`: the value already applied).
#[tauri::command]
async fn relaunch_terminal(
    state: State<'_, AppState>,
    id: String,
    skip_permissions: bool,
    session_id: Option<String>,
    agent: Option<String>,
) -> Result<bool, String> {
    let ptys = state.ptys.clone();
    // Ending the old process waits for it; keep that off the runtime workers.
    tauri::async_runtime::spawn_blocking(move || {
        ptys.relaunch(&id, skip_permissions, session_id, agent)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── Host commands ──────────────────────────────────────────────────────
// Thin wrappers over `xshell_core`. Parameter names are the frontend IPC contract (Tauri maps
// them to camelCase keys). The `async` ones stay `async` so Tauri runs them off the main
// thread; core itself is fully synchronous.

// Commands that only make sense on the machine running the window, so a remote host never
// serves them: project icons are local files, and the other two open local apps. The tests
// below check that these two lists plus `xshell_core::METHODS` cover every registered command.
#[cfg(test)]
const DESKTOP_ONLY_COMMANDS: &[&str] = &[
    "open_url",
    "reveal_in_explorer",
    "read_image_base64",
    "list_ssh_hosts",
];
// The Remote Host connection commands (`hosts::commands`).
#[cfg(test)]
const HOST_LINK_COMMANDS: &[&str] = &[
    "hosts_configure",
    "hosts_status",
    "hosts_kick",
    "host_call",
    "host_term_open",
    "host_term_attach",
    "host_term_detach",
    "host_term_input",
    "host_term_resize",
    "host_term_close",
    "host_term_update",
    "host_term_relaunch",
    "host_upgrade",
    "host_test",
];
// The PTY commands; a remote host serves terminals through its own protocol.
#[cfg(test)]
const TERMINAL_COMMANDS: &[&str] = &[
    "spawn_terminal",
    "write_terminal",
    "resize_terminal",
    "close_terminal",
    "relaunch_terminal",
];

// Built per call, as each command used to call `dirs::home_dir()` per call.
fn ctx() -> HostCtx {
    HostCtx::from_env()
}

#[tauri::command]
fn list_claude_projects() -> Vec<ProjectInfo> {
    xshell_core::claude::list_claude_projects(&ctx())
}

#[tauri::command]
fn get_sessions(encoded_name: String) -> Vec<SessionInfo> {
    xshell_core::sessions::get_sessions(&ctx(), encoded_name)
}

#[tauri::command]
fn get_all_recent_sessions(limit: usize) -> Vec<SessionInfo> {
    xshell_core::sessions::get_all_recent_sessions(&ctx(), limit)
}

#[tauri::command]
fn get_session_messages(
    encoded_name: String,
    session_id: String,
    limit: usize,
) -> Vec<MessagePreview> {
    xshell_core::claude::get_session_messages(&ctx(), encoded_name, session_id, limit)
}

#[tauri::command]
fn save_dropped_file(bytes_base64: String, name: String) -> Result<String, String> {
    xshell_core::files::save_dropped_file(&ctx(), bytes_base64, name)
}

#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    xshell_core::files::read_text_file(path)
}

#[tauri::command]
async fn list_dir(path: String) -> Result<Vec<DirItem>, String> {
    xshell_core::files::list_dir(path)
}

#[tauri::command]
async fn search_dir(root: String, query: String, limit: Option<usize>) -> Vec<DirItem> {
    xshell_core::files::search_dir(root, query, limit)
}

#[tauri::command]
fn get_username() -> String {
    xshell_core::files::get_username()
}

#[tauri::command]
fn get_home_dir() -> String {
    xshell_core::files::get_home_dir(&ctx())
}

#[tauri::command]
fn get_project_skills(project_path: String) -> ProjectSkills {
    xshell_core::skills::get_project_skills(&ctx(), project_path)
}

#[tauri::command]
fn get_project_memories(project_path: String) -> ProjectMemories {
    xshell_core::memories::get_project_memories(&ctx(), project_path)
}

#[tauri::command]
async fn get_git_status(cwd: String) -> GitStatus {
    xshell_core::git::get_git_status(cwd)
}

#[tauri::command]
async fn get_git_log(cwd: String, limit: Option<u32>) -> Vec<GitCommit> {
    xshell_core::git::get_git_log(cwd, limit)
}

#[tauri::command]
async fn git_diff(cwd: String, path: String, mode: String) -> Result<String, String> {
    xshell_core::git::git_diff(cwd, path, mode)
}

#[tauri::command]
fn git_stage(cwd: String, paths: Vec<String>) -> Result<(), String> {
    xshell_core::git::git_stage(cwd, paths)
}

#[tauri::command]
fn git_unstage(cwd: String, paths: Vec<String>) -> Result<(), String> {
    xshell_core::git::git_unstage(cwd, paths)
}

#[tauri::command]
async fn git_discard(cwd: String, path: String, mode: String) -> Result<(), String> {
    xshell_core::git::git_discard(cwd, path, mode)
}

#[tauri::command]
fn list_git_branches(cwd: String) -> Vec<GitBranch> {
    xshell_core::git::list_git_branches(cwd)
}

#[tauri::command]
fn git_checkout(cwd: String, branch: String) -> Result<(), String> {
    xshell_core::git::git_checkout(cwd, branch)
}

#[tauri::command]
fn list_project_session_ids(cwd: String) -> Vec<String> {
    xshell_core::claude::list_project_session_ids(&ctx(), cwd)
}

#[tauri::command]
fn detect_session_branch(
    cwd: String,
    current_session_id: String,
    known_session_ids: Vec<String>,
) -> Option<BranchInfo> {
    xshell_core::claude::detect_session_branch(&ctx(), cwd, current_session_id, known_session_ids)
}

#[tauri::command]
fn probe_statusline_setup() -> StatuslineProbe {
    xshell_core::stats::probe_statusline_setup(&ctx())
}

#[tauri::command]
fn get_global_rate_limits() -> GlobalRateLimits {
    xshell_core::stats::get_global_rate_limits(&ctx())
}

#[tauri::command]
async fn detect_agent_binary(binary: String) -> Result<AgentBinaryProbe, String> {
    xshell_core::agents::detect_agent_binary(binary)
}

#[tauri::command]
fn list_codex_projects() -> Vec<CodexProjectInfo> {
    xshell_core::codex::list_codex_projects(&ctx())
}

#[tauri::command]
fn list_cursor_projects() -> Vec<CodexProjectInfo> {
    xshell_core::cursor::list_cursor_projects(&ctx())
}

#[tauri::command]
fn list_opencode_projects() -> Vec<CodexProjectInfo> {
    xshell_core::opencode::list_opencode_projects(&ctx())
}

#[tauri::command]
fn list_antigravity_projects() -> Vec<CodexProjectInfo> {
    xshell_core::antigravity::list_antigravity_projects(&ctx())
}

#[tauri::command]
fn get_codex_context(project_path: String) -> CodexContext {
    xshell_core::codex::get_codex_context(&ctx(), project_path)
}

#[tauri::command]
fn get_cursor_context(project_path: String) -> CursorContext {
    xshell_core::cursor::get_cursor_context(&ctx(), project_path)
}

#[tauri::command]
fn get_opencode_context(project_path: String) -> OpencodeContext {
    xshell_core::opencode::get_opencode_context(&ctx(), project_path)
}

#[tauri::command]
fn get_antigravity_context(project_path: String) -> AntigravityContext {
    xshell_core::antigravity::get_antigravity_context(&ctx(), project_path)
}

#[tauri::command]
fn get_claude_cost_summary() -> ClaudeCostSummary {
    xshell_core::stats::get_claude_cost_summary(&ctx())
}

#[tauri::command]
fn get_codex_usage() -> CodexUsage {
    xshell_core::codex::get_codex_usage(&ctx())
}

pub fn run() {
    xshell_core::files::cleanup_old_dropped_files(&ctx());
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(AppState {
            ptys: Arc::new(LocalPtys::new(Arc::new(|spec| {
                xshell_core::plan_command(&ctx(), spec)
            }))),
        })
        .setup(|app| {
            use tauri::Manager as _;
            app.manage(hosts::Hosts::new(app.handle()));
            // The main window is built here, after the state its page calls into, rather than
            // from tauri.conf.json (`create: false`): the config cannot enable clipboard
            // access. Without it WebKitGTK rejects `navigator.clipboard` reads, so Ctrl+V and
            // right-click paste did nothing on Linux. macOS ignores the flag.
            let main = app
                .config()
                .app
                .windows
                .first()
                .ok_or("tauri.conf.json defines no window")?
                .clone();
            tauri::WebviewWindowBuilder::from_config(app.handle(), &main)?
                .enable_clipboard_access()
                .build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_claude_projects,
            get_sessions,
            get_all_recent_sessions,
            get_session_messages,
            read_image_base64,
            save_dropped_file,
            read_text_file,
            reveal_in_explorer,
            list_dir,
            search_dir,
            open_url,
            get_username,
            get_home_dir,
            get_project_skills,
            get_project_memories,
            get_git_status,
            get_git_log,
            git_diff,
            git_stage,
            git_unstage,
            git_discard,
            list_git_branches,
            git_checkout,
            list_project_session_ids,
            detect_session_branch,
            probe_statusline_setup,
            get_global_rate_limits,
            detect_agent_binary,
            list_codex_projects,
            list_cursor_projects,
            list_opencode_projects,
            list_antigravity_projects,
            get_codex_context,
            get_cursor_context,
            get_opencode_context,
            get_antigravity_context,
            get_claude_cost_summary,
            get_codex_usage,
            spawn_terminal,
            write_terminal,
            resize_terminal,
            close_terminal,
            relaunch_terminal,
            hosts::commands::hosts_configure,
            hosts::commands::hosts_status,
            hosts::commands::hosts_kick,
            hosts::commands::host_call,
            hosts::commands::host_term_open,
            hosts::commands::host_term_attach,
            hosts::commands::host_term_detach,
            hosts::commands::host_term_input,
            hosts::commands::host_term_resize,
            hosts::commands::host_term_close,
            hosts::commands::host_term_update,
            hosts::commands::host_term_relaunch,
            hosts::commands::host_upgrade,
            hosts::commands::host_test,
            hosts::commands::list_ssh_hosts
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, ev| {
            if let tauri::RunEvent::Exit = ev {
                use tauri::Manager as _;
                // Kill every ssh; the Daemons and their Terminals keep running.
                if let Some(h) = app.try_state::<hosts::Hosts>() {
                    h.manager.shutdown();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // The names inside `tauri::generate_handler![...]` in this file.
    fn registered_commands() -> BTreeSet<String> {
        let src = include_str!("lib.rs");
        let start =
            src.find("generate_handler![").expect("handler list") + "generate_handler![".len();
        let end = start + src[start..].find(']').expect("handler list end");
        src[start..end]
            .split(',')
            .map(|s| s.trim().rsplit("::").next().unwrap_or("").to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    #[test]
    fn every_host_command_is_dispatchable() {
        let excluded: BTreeSet<&str> = DESKTOP_ONLY_COMMANDS
            .iter()
            .chain(TERMINAL_COMMANDS)
            .chain(HOST_LINK_COMMANDS)
            .copied()
            .collect();
        let host: BTreeSet<String> = registered_commands()
            .into_iter()
            .filter(|c| !excluded.contains(c.as_str()))
            .collect();
        let methods: BTreeSet<String> =
            xshell_core::METHODS.iter().map(|s| s.to_string()).collect();
        assert_eq!(host, methods);
    }

    #[test]
    fn host_link_and_desktop_lists_registered() {
        let registered = registered_commands();
        assert_eq!(registered.len(), 58);
        for c in DESKTOP_ONLY_COMMANDS
            .iter()
            .chain(TERMINAL_COMMANDS)
            .chain(HOST_LINK_COMMANDS)
        {
            assert!(registered.contains(*c), "{c} is not registered");
        }
    }

    // The Desktop installs the xshelld of its own version on Remote Hosts.
    #[test]
    fn versions_agree() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let daemon = include_str!("../crates/xshelld/Cargo.toml")
            .lines()
            .find_map(|l| l.strip_prefix("version = "))
            .map(|v| v.trim().trim_matches('"'))
            .expect("xshelld version");
        assert_eq!(conf["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(daemon, env!("CARGO_PKG_VERSION"));
    }
}

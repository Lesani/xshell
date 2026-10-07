//! One entry point for every host-side command, keyed by the same method names and JSON
//! argument keys the desktop frontend passes to Tauri's `invoke`.

use crate::ctx::HostCtx;
use crate::{
    agents, antigravity, claude, codex, cursor, files, git, memories, opencode, sessions, skills,
    stats,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every method [`dispatch`] routes. These are the desktop's `#[tauri::command]`s minus the
/// terminal commands and the desktop-only ones (`open_url`, `reveal_in_explorer`,
/// `read_image_base64`).
pub const METHODS: &[&str] = &[
    "list_claude_projects",
    "get_sessions",
    "get_all_recent_sessions",
    "get_session_messages",
    "save_dropped_file",
    "read_text_file",
    "list_dir",
    "search_dir",
    "get_username",
    "get_home_dir",
    "get_project_skills",
    "get_project_memories",
    "get_git_status",
    "get_git_log",
    "git_diff",
    "git_stage",
    "git_unstage",
    "git_discard",
    "list_git_branches",
    "git_checkout",
    "list_project_session_ids",
    "detect_session_branch",
    "probe_statusline_setup",
    "get_global_rate_limits",
    "detect_agent_binary",
    "list_codex_projects",
    "list_cursor_projects",
    "list_opencode_projects",
    "list_antigravity_projects",
    "get_codex_context",
    "get_cursor_context",
    "get_opencode_context",
    "get_antigravity_context",
    "get_claude_cost_summary",
    "get_codex_usage",
];

// Parameter shapes. Keys are camelCase like Tauri's argument mapping; missing `Option`
// fields become `None` and unknown keys are ignored, as with Tauri.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncodedName {
    encoded_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limit {
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMessages {
    encoded_name: String,
    session_id: String,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DroppedFile {
    bytes_base64: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathArg {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchDir {
    root: String,
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectPath {
    project_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cwd {
    cwd: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitLog {
    cwd: String,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitPathMode {
    cwd: String,
    path: String,
    mode: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitPaths {
    cwd: String,
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitCheckout {
    cwd: String,
    branch: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionBranch {
    cwd: String,
    current_session_id: String,
    known_session_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Binary {
    binary: String,
}

/// Run `method` with JSON `params` (`null` counts as `{}`) and return its JSON result.
/// `()` and `None` results are `null`; a command's own `Err` string is passed through as is.
pub fn dispatch(ctx: &HostCtx, method: &str, params: Value) -> Result<Value, String> {
    let params = if params.is_null() {
        Value::Object(Default::default())
    } else {
        params
    };
    let p = Params { method, params };
    match method {
        "list_claude_projects" => out(claude::list_claude_projects(ctx)),
        "get_sessions" => {
            let a: EncodedName = p.parse()?;
            out(sessions::get_sessions(ctx, a.encoded_name))
        }
        "get_all_recent_sessions" => {
            let a: Limit = p.parse()?;
            out(sessions::get_all_recent_sessions(ctx, a.limit))
        }
        "get_session_messages" => {
            let a: SessionMessages = p.parse()?;
            out(claude::get_session_messages(
                ctx,
                a.encoded_name,
                a.session_id,
                a.limit,
            ))
        }
        "save_dropped_file" => {
            let a: DroppedFile = p.parse()?;
            out(files::save_dropped_file(ctx, a.bytes_base64, a.name)?)
        }
        "read_text_file" => {
            let a: PathArg = p.parse()?;
            out(files::read_text_file(a.path)?)
        }
        "list_dir" => {
            let a: PathArg = p.parse()?;
            out(files::list_dir(a.path)?)
        }
        "search_dir" => {
            let a: SearchDir = p.parse()?;
            out(files::search_dir(a.root, a.query, a.limit))
        }
        "get_username" => out(files::get_username()),
        "get_home_dir" => out(files::get_home_dir(ctx)),
        "get_project_skills" => {
            let a: ProjectPath = p.parse()?;
            out(skills::get_project_skills(ctx, a.project_path))
        }
        "get_project_memories" => {
            let a: ProjectPath = p.parse()?;
            out(memories::get_project_memories(ctx, a.project_path))
        }
        "get_git_status" => {
            let a: Cwd = p.parse()?;
            out(git::get_git_status(a.cwd))
        }
        "get_git_log" => {
            let a: GitLog = p.parse()?;
            out(git::get_git_log(a.cwd, a.limit))
        }
        "git_diff" => {
            let a: GitPathMode = p.parse()?;
            out(git::git_diff(a.cwd, a.path, a.mode)?)
        }
        "git_stage" => {
            let a: GitPaths = p.parse()?;
            out(git::git_stage(a.cwd, a.paths)?)
        }
        "git_unstage" => {
            let a: GitPaths = p.parse()?;
            out(git::git_unstage(a.cwd, a.paths)?)
        }
        "git_discard" => {
            let a: GitPathMode = p.parse()?;
            out(git::git_discard(a.cwd, a.path, a.mode)?)
        }
        "list_git_branches" => {
            let a: Cwd = p.parse()?;
            out(git::list_git_branches(a.cwd))
        }
        "git_checkout" => {
            let a: GitCheckout = p.parse()?;
            out(git::git_checkout(a.cwd, a.branch)?)
        }
        "list_project_session_ids" => {
            let a: Cwd = p.parse()?;
            out(claude::list_project_session_ids(ctx, a.cwd))
        }
        "detect_session_branch" => {
            let a: SessionBranch = p.parse()?;
            out(claude::detect_session_branch(
                ctx,
                a.cwd,
                a.current_session_id,
                a.known_session_ids,
            ))
        }
        "probe_statusline_setup" => out(stats::probe_statusline_setup(ctx)),
        "get_global_rate_limits" => out(stats::get_global_rate_limits(ctx)),
        "detect_agent_binary" => {
            let a: Binary = p.parse()?;
            out(agents::detect_agent_binary(a.binary)?)
        }
        "list_codex_projects" => out(codex::list_codex_projects(ctx)),
        "list_cursor_projects" => out(cursor::list_cursor_projects(ctx)),
        "list_opencode_projects" => out(opencode::list_opencode_projects(ctx)),
        "list_antigravity_projects" => out(antigravity::list_antigravity_projects(ctx)),
        "get_codex_context" => {
            let a: ProjectPath = p.parse()?;
            out(codex::get_codex_context(ctx, a.project_path))
        }
        "get_cursor_context" => {
            let a: ProjectPath = p.parse()?;
            out(cursor::get_cursor_context(ctx, a.project_path))
        }
        "get_opencode_context" => {
            let a: ProjectPath = p.parse()?;
            out(opencode::get_opencode_context(ctx, a.project_path))
        }
        "get_antigravity_context" => {
            let a: ProjectPath = p.parse()?;
            out(antigravity::get_antigravity_context(ctx, a.project_path))
        }
        "get_claude_cost_summary" => out(stats::get_claude_cost_summary(ctx)),
        "get_codex_usage" => out(codex::get_codex_usage(ctx)),
        _ => Err(format!("unknown method `{method}`")),
    }
}

struct Params<'a> {
    method: &'a str,
    params: Value,
}

impl Params<'_> {
    fn parse<T: DeserializeOwned>(self) -> Result<T, String> {
        serde_json::from_value(self.params)
            .map_err(|e| format!("invalid args for command `{}`: {e}", self.method))
    }
}

fn out<T: Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

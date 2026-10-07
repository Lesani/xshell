use crate::antigravity::parse_antigravity_sessions;
use crate::claude::{encode_project_name, get_claude_projects_dir, parse_session};
use crate::codex::{codex_rollout_files, codex_session_names, parse_codex_session};
use crate::cursor::{cursor_chat_dirs, cursor_workspace_map, parse_cursor_session};
use crate::opencode::parse_opencode_sessions;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
    pub encoded_name: String,
    pub session_count: usize,
    pub last_active: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub timestamp: String,
    pub message_count: usize,
    pub project_name: String,
    pub project_path: String,
    pub git_branch: String,
    pub claude_version: String,
    pub tool_use_count: usize,
    pub duration_ms: u64,
    // Raw model id from the latest assistant turn (e.g. "claude-opus-4-7-20260101"). The
    // frontend formats this into a short label like "Opus 4.7".
    pub model: String,
    // Current context usage = last assistant turn's input + cache_creation + cache_read.
    // This is what Claude actually sees — good proxy for the "200k" budget indicator.
    pub context_tokens: u64,
    pub context_limit: u64,
    // Lifetime cost in USD, sourced from the xshell-stats statusline hook (kept monotonic
    // there). 0 when no hook data exists for this session — we don't synthesize.
    pub cost_usd: f64,
    // True when cost/context/model came from the xshell-stats file (statusline hook).
    pub is_authoritative_stats: bool,
    // { "YYYY-MM-DD": usd } — daily breakdown produced by the hook (delta since previous
    // tick added to today's bucket). Lets the UI render trend / per-day totals.
    pub daily_cost: std::collections::BTreeMap<String, f64>,
    // Rate-limit usage from the statusline hook (only present when authoritative).
    pub rate_limit_5h_pct: Option<f64>,
    pub rate_limit_7d_pct: Option<f64>,
    // Lifetime token totals summed from every non-synthetic assistant turn in the JSONL.
    // Always populated — independent of the xshell-stats hook.
    pub total_input_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    // Per-day token breakdown keyed by YYYY-MM-DD. Each value is [input, cache_creation,
    // cache_read, output] so the UI can render a stacked area chart with the four bands.
    pub daily_tokens: std::collections::BTreeMap<String, [u64; 4]>,
    // Which coding agent produced this session: "claude" (JSONL under ~/.claude/projects)
    // or "codex" (rollout under ~/.codex/sessions). Drives the row icon, model formatting,
    // and which resume command a terminal tab spawns.
    pub agent: String,
}

pub fn get_sessions(encoded_name: String) -> Vec<SessionInfo> {
    let mut sessions: Vec<SessionInfo> = vec![];

    // Claude sessions live under ~/.claude/projects/<encoded_name>/. A project can be
    // Codex-only (no such directory) — that must not short-circuit the Codex pass below.
    if let Some(project_dir) = get_claude_projects_dir().map(|d| d.join(&encoded_name)) {
        if project_dir.exists() {
            // Get project path from first JSONL
            let mut project_path = String::new();
            let mut project_name = String::new();
            for e in fs::read_dir(&project_dir)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
            {
                let p = e.path();
                if p.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                if let Ok(file) = fs::File::open(&p) {
                    for line in BufReader::new(file).lines().take(30).flatten() {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                            if let Some(c) = json.get("cwd").and_then(|c| c.as_str()) {
                                project_path = c.to_string();
                                project_name = std::path::Path::new(c)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                break;
                            }
                        }
                    }
                }
                if !project_path.is_empty() {
                    break;
                }
            }

            sessions.extend(
                fs::read_dir(&project_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        if p.extension().is_none_or(|ext| ext != "jsonl") {
                            return None;
                        }
                        parse_session(&p, &project_name, &project_path)
                    }),
            );
        }
    }

    // Codex sessions have no per-project directory — match rollouts whose recorded cwd
    // encodes to the same project directory name Claude would use.
    let codex_names = codex_session_names();
    for p in codex_rollout_files() {
        if let Some(s) = parse_codex_session(&p, &codex_names) {
            if encode_project_name(&s.project_path) == encoded_name {
                sessions.push(s);
            }
        }
    }

    // Cursor chats — same approach: resolve each chat's cwd, then match by encoded name.
    let cursor_ws = cursor_workspace_map();
    for dir in cursor_chat_dirs() {
        if let Some(s) = parse_cursor_session(&dir, &cursor_ws) {
            if !s.project_path.is_empty() && encode_project_name(&s.project_path) == encoded_name {
                sessions.push(s);
            }
        }
    }

    // opencode sessions — each row records its cwd directly; match by encoded name.
    sessions.extend(
        parse_opencode_sessions()
            .into_iter()
            .filter(|s| encode_project_name(&s.project_path) == encoded_name),
    );

    // Antigravity conversations — workspace-scoped by design; match by encoded name.
    sessions.extend(
        parse_antigravity_sessions()
            .into_iter()
            .filter(|s| encode_project_name(&s.project_path) == encoded_name),
    );

    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sessions
}

pub fn get_all_recent_sessions(limit: usize) -> Vec<SessionInfo> {
    let mut all_sessions: Vec<SessionInfo> = vec![];

    // A machine can have Codex sessions but no ~/.claude/projects (or vice versa) — each
    // agent's pass is independent.
    let projects_dir = get_claude_projects_dir().filter(|d| d.exists());
    for entry in projects_dir
        .iter()
        .flat_map(|d| fs::read_dir(d).ok().into_iter().flatten().flatten())
    {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }

        let project_dir = entry.path();
        let mut project_path = String::new();
        let mut project_name = String::new();

        // Get project info from first JSONL
        for jsonl in fs::read_dir(&project_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = jsonl.path();
            if p.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Ok(file) = fs::File::open(&p) {
                for line in BufReader::new(file).lines().take(30).flatten() {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                        if let Some(c) = json.get("cwd").and_then(|c| c.as_str()) {
                            project_path = c.to_string();
                            project_name = std::path::Path::new(c)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default();
                            break;
                        }
                    }
                }
            }
            if !project_path.is_empty() {
                break;
            }
        }

        if project_path.is_empty() {
            continue;
        }

        for jsonl in fs::read_dir(&project_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = jsonl.path();
            if p.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Some(session) = parse_session(&p, &project_name, &project_path) {
                all_sessions.push(session);
            }
        }
    }

    // Codex sessions across all directories — same recency pool as the Claude ones.
    let codex_names = codex_session_names();
    all_sessions.extend(
        codex_rollout_files()
            .iter()
            .filter_map(|p| parse_codex_session(p, &codex_names)),
    );

    // Cursor chats across all workspaces — same recency pool.
    let cursor_ws = cursor_workspace_map();
    all_sessions.extend(
        cursor_chat_dirs()
            .iter()
            .filter_map(|d| parse_cursor_session(d, &cursor_ws)),
    );

    // opencode sessions across all directories — same recency pool.
    all_sessions.extend(parse_opencode_sessions());

    // Antigravity conversations across all workspaces — same recency pool.
    all_sessions.extend(parse_antigravity_sessions());

    all_sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    all_sessions.truncate(limit);
    all_sessions
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessagePreview {
    pub role: String,
    pub text: String,
}

// ── Codex project discovery ───────────────────────────────────────────
// Codex has no per-project directory layout like ~/.claude/projects — sessions land in
// ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl and each file's first line is a
// `session_meta` record carrying the session's cwd. Group by cwd to get the set of
// directories Codex has been used in, for the Add Projects picker's agent marks.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CodexProjectInfo {
    pub path: String,
    pub session_count: usize,
    pub last_active: String,
}

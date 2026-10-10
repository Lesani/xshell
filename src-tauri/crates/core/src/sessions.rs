use crate::antigravity::parse_antigravity_sessions;
use crate::claude::{encode_project_name, parse_session};
use crate::codex::{codex_rollout_files, codex_session_names, parse_codex_session};
use crate::ctx::HostCtx;
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

/// A session id safe to pass on: it becomes an agent argument (`codex resume <id>`) and a
/// file name (`<id>.jsonl`), so it may not look like an option or a path.
pub fn valid_session_id(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn get_sessions(ctx: &HostCtx, encoded_name: String) -> Vec<SessionInfo> {
    let mut sessions: Vec<SessionInfo> = vec![];

    // Claude sessions live under ~/.claude/projects/<encoded_name>/. A project can be
    // Codex-only (no such directory) — that must not short-circuit the Codex pass below.
    if let Some(project_dir) = ctx.claude_projects_dir().map(|d| d.join(&encoded_name)) {
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
                        parse_session(ctx, &p, &project_name, &project_path)
                    }),
            );
        }
    }

    // Codex sessions have no per-project directory — match rollouts whose recorded cwd
    // encodes to the same project directory name Claude would use.
    let codex_names = codex_session_names(ctx);
    for p in codex_rollout_files(ctx) {
        if let Some(s) = parse_codex_session(&p, &codex_names) {
            if encode_project_name(&s.project_path) == encoded_name {
                sessions.push(s);
            }
        }
    }

    // Cursor chats — same approach: resolve each chat's cwd, then match by encoded name.
    let cursor_ws = cursor_workspace_map(ctx);
    for dir in cursor_chat_dirs(ctx) {
        if let Some(s) = parse_cursor_session(&dir, &cursor_ws) {
            if !s.project_path.is_empty() && encode_project_name(&s.project_path) == encoded_name {
                sessions.push(s);
            }
        }
    }

    // opencode sessions — each row records its cwd directly; match by encoded name.
    sessions.extend(
        parse_opencode_sessions(ctx)
            .into_iter()
            .filter(|s| encode_project_name(&s.project_path) == encoded_name),
    );

    // Antigravity conversations — workspace-scoped by design; match by encoded name.
    sessions.extend(
        parse_antigravity_sessions(ctx)
            .into_iter()
            .filter(|s| encode_project_name(&s.project_path) == encoded_name),
    );

    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sessions
}

pub fn get_all_recent_sessions(ctx: &HostCtx, limit: usize) -> Vec<SessionInfo> {
    let mut all_sessions: Vec<SessionInfo> = vec![];

    // A machine can have Codex sessions but no ~/.claude/projects (or vice versa) — each
    // agent's pass is independent.
    let projects_dir = ctx.claude_projects_dir().filter(|d| d.exists());
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
            if let Some(session) = parse_session(ctx, &p, &project_name, &project_path) {
                all_sessions.push(session);
            }
        }
    }

    // Codex sessions across all directories — same recency pool as the Claude ones.
    let codex_names = codex_session_names(ctx);
    all_sessions.extend(
        codex_rollout_files(ctx)
            .iter()
            .filter_map(|p| parse_codex_session(p, &codex_names)),
    );

    // Cursor chats across all workspaces — same recency pool.
    let cursor_ws = cursor_workspace_map(ctx);
    all_sessions.extend(
        cursor_chat_dirs(ctx)
            .iter()
            .filter_map(|d| parse_cursor_session(d, &cursor_ws)),
    );

    // opencode sessions across all directories — same recency pool.
    all_sessions.extend(parse_opencode_sessions(ctx));

    // Antigravity conversations across all workspaces — same recency pool.
    all_sessions.extend(parse_antigravity_sessions(ctx));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use serde_json::json;

    #[test]
    fn session_id_charset() {
        for ok in [
            "a",
            "0",
            "11111111-2222-3333-4444-555555555555",
            "ses_ABC-1",
        ] {
            assert!(valid_session_id(ok), "{ok}");
        }
        for bad in ["", "-cx", "_a", "a b", "a/b", "../a", "a.b", "a\0", "é"] {
            assert!(!valid_session_id(bad), "{bad:?}");
        }
    }

    fn claude_session(fx: &Fixture, cwd: &str, sid: &str, ts: &str) {
        fx.write_jsonl(
            format!(
                "home/.claude/projects/{}/{sid}.jsonl",
                encode_project_name(cwd)
            ),
            &[json!({"type": "user", "cwd": cwd, "timestamp": ts,
                     "message": {"role": "user", "content": format!("prompt {sid}")}})],
        );
    }

    fn codex_session(fx: &Fixture, cwd: &str, sid: &str, ts: &str) {
        fx.write_jsonl(
            format!("home/.codex/sessions/2026/01/02/rollout-{sid}.jsonl"),
            &[
                json!({"type": "session_meta", "timestamp": ts,
                       "payload": {"id": sid, "cwd": cwd, "cli_version": "0.1"}}),
                json!({"type": "event_msg", "timestamp": ts,
                       "payload": {"type": "user_message", "message": "codex prompt"}}),
            ],
        );
    }

    // opencode.db with the two tables parse_opencode_sessions reads.
    fn opencode_db(fx: &Fixture, rows: &[(&str, &str, i64)]) {
        let dir = fx.home().join(".local/share/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT, title TEXT, directory TEXT, model TEXT, \
             version TEXT, time_created INTEGER, time_updated INTEGER, tokens_input INTEGER, \
             tokens_output INTEGER, tokens_reasoning INTEGER, tokens_cache_read INTEGER, \
             tokens_cache_write INTEGER, parent_id TEXT, time_archived INTEGER);
             CREATE TABLE message (session_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        for (id, dir, updated_ms) in rows {
            conn.execute(
                "INSERT INTO session VALUES (?1, 'oc title', ?2, NULL, '1.0', ?3, ?3, 0, 0, 0, 0, 0, NULL, NULL)",
                rusqlite::params![id, dir, updated_ms],
            )
            .unwrap();
        }
    }

    fn ids(sessions: &[SessionInfo]) -> Vec<(&str, &str)> {
        sessions
            .iter()
            .map(|s| (s.id.as_str(), s.agent.as_str()))
            .collect()
    }

    #[test]
    fn get_sessions_merges_agents_for_encoded_name() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        claude_session(&fx, cwd, "claude-1", "2026-01-02T10:00:00Z");
        claude_session(&fx, "/work/beta", "claude-other", "2026-01-09T10:00:00Z");
        codex_session(&fx, cwd, "codex-1", "2026-01-04T10:00:00Z");
        codex_session(&fx, "/work/beta", "codex-other", "2026-01-08T10:00:00Z");
        // 2026-01-03T00:00:00Z and 2026-01-07T00:00:00Z in unix ms.
        opencode_db(
            &fx,
            &[
                ("oc-1", cwd, 1_767_398_400_000),
                ("oc-other", "/work/beta", 1_767_744_000_000),
            ],
        );
        let got = get_sessions(&fx.ctx(), encode_project_name(cwd));
        assert_eq!(
            ids(&got),
            vec![
                ("codex-1", "codex"),
                ("oc-1", "opencode"),
                ("claude-1", "claude")
            ]
        );
        assert_eq!(got[1].timestamp, "2026-01-03T00:00:00Z");
    }

    #[test]
    fn get_all_recent_sessions_truncates_to_limit() {
        let fx = Fixture::new();
        claude_session(&fx, "/work/alpha", "s-old", "2026-01-01T10:00:00Z");
        claude_session(&fx, "/work/alpha", "s-mid", "2026-01-02T10:00:00Z");
        claude_session(&fx, "/work/beta", "s-new", "2026-01-03T10:00:00Z");
        codex_session(&fx, "/work/gamma", "codex-x", "2026-01-02T12:00:00Z");
        let ctx = fx.ctx();
        assert_eq!(
            ids(&get_all_recent_sessions(&ctx, 2)),
            vec![("s-new", "claude"), ("codex-x", "codex")]
        );
        assert_eq!(get_all_recent_sessions(&ctx, 100).len(), 4);
    }
}

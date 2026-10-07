use crate::agent_context::{AgentContextItem, AgentContextSection};
use crate::claude::list_claude_projects;
use crate::codex::list_codex_projects;
use crate::ctx::HostCtx;
use crate::sessions::{CodexProjectInfo, SessionInfo};
use crate::time::{system_time_to_iso, unix_ms_to_iso};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

// ── Cursor project context ────────────────────────────────────────────
// Cursor reads its own .cursor/rules, plus AGENTS.md and CLAUDE.md at the project root, and
// MCP servers from mcp.json (project + global). Returned as the same generic titled sections
// as Codex so the context tree renders it with no agent-specific frontend code.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CursorContext {
    pub present: bool,
    pub sections: Vec<AgentContextSection>,
}

pub fn get_cursor_context(ctx: &HostCtx, project_path: String) -> CursorContext {
    let pp = std::path::Path::new(&project_path);
    let mut sections: Vec<AgentContextSection> = vec![];

    // Rules — .cursor/rules/**/*.{mdc,md}. Cursor allows nested rule folders, so walk the tree.
    let rules_dir = pp.join(".cursor").join("rules");
    let mut rules: Vec<AgentContextItem> = vec![];
    let mut stack = vec![rules_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
            let p = entry.path();
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|ext| ext != "mdc" && ext != "md") {
                continue;
            }
            let name = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Show the sub-path under rules/ as the detail when the rule is nested.
            let detail = p
                .parent()
                .and_then(|par| par.strip_prefix(&rules_dir).ok())
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            rules.push(AgentContextItem {
                name,
                detail,
                path: p.to_string_lossy().into_owned(),
            });
        }
    }
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    if !rules.is_empty() {
        sections.push(AgentContextSection {
            title: "Rules".into(),
            items: rules,
        });
    }

    // Instructions — AGENTS.md / CLAUDE.md at the project root (Cursor applies both as rules).
    let instructions: Vec<AgentContextItem> = ["AGENTS.md", "CLAUDE.md"]
        .iter()
        .map(|f| pp.join(f))
        .filter(|p| p.exists())
        .map(|p| AgentContextItem {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            detail: "project".into(),
            path: p.to_string_lossy().into_owned(),
        })
        .collect();
    if !instructions.is_empty() {
        sections.push(AgentContextSection {
            title: "Instructions".into(),
            items: instructions,
        });
    }

    // MCP servers — project .cursor/mcp.json then global ~/.cursor/mcp.json. Same
    // { "mcpServers": { name: {...} } } shape Claude/Cursor share.
    let mut mcp_items: Vec<AgentContextItem> = vec![];
    let mut read_mcp = |path: std::path::PathBuf, scope: &str| {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(servers) = json.get("mcpServers").and_then(|v| v.as_object()) {
                    for (name, cfg) in servers {
                        let detail = cfg
                            .get("command")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| scope.to_string());
                        mcp_items.push(AgentContextItem {
                            name: name.clone(),
                            detail,
                            path: String::new(),
                        });
                    }
                }
            }
        }
    };
    read_mcp(pp.join(".cursor").join("mcp.json"), "project");
    if let Some(home) = ctx.home.clone() {
        read_mcp(home.join(".cursor").join("mcp.json"), "global");
    }
    if !mcp_items.is_empty() {
        sections.push(AgentContextSection {
            title: "MCP servers".into(),
            items: mcp_items,
        });
    }

    CursorContext {
        present: !sections.is_empty(),
        sections,
    }
}

// ── Cursor session parsing ────────────────────────────────────────────
// Cursor stores chats under ~/.cursor/chats/<md5(cwd)>/<chat-uuid>/ as a small meta.json
// (title, timestamps, hasConversation) plus a SQLite store.db whose single `meta` row holds
// hex-encoded JSON with the model + mode. Cursor exposes no token/cost/rate-limit data
// locally, so those stay zero. The workspace folder is md5 of the exact cwd string and isn't
// reversible — we resolve it via a md5(path)→path map built below.

pub fn cursor_workspace_map(ctx: &HostCtx) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let Some(home) = ctx.home.clone() else {
        return map;
    };
    let mut add = |path: &str| {
        if path.is_empty() {
            return;
        }
        let digest = format!("{:x}", md5::compute(path.as_bytes()));
        map.entry(digest).or_insert_with(|| path.to_string());
    };
    // Authoritative: every ~/.cursor/projects/<id>/.workspace-trusted records its workspacePath.
    let projects = home.join(".cursor").join("projects");
    for entry in fs::read_dir(&projects).ok().into_iter().flatten().flatten() {
        let wt = entry.path().join(".workspace-trusted");
        if let Ok(c) = fs::read_to_string(&wt) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&c) {
                if let Some(p) = j.get("workspacePath").and_then(|v| v.as_str()) {
                    add(p);
                }
            }
        }
    }
    // Safety net: any project the user also uses in Claude or Codex resolves even if Cursor
    // never wrote a trust file for it.
    for p in list_claude_projects(ctx) {
        add(&p.path);
    }
    for p in list_codex_projects(ctx) {
        add(&p.path);
    }
    map
}

pub fn cursor_chats_dir(ctx: &HostCtx) -> Option<PathBuf> {
    ctx.home.clone().map(|h| h.join(".cursor").join("chats"))
}

// Read the model id from a chat's store.db. The `meta` row's value is a TEXT column holding
// hex-encoded JSON; the freshest copy may live in the WAL, so we open it as a real SQLite
// connection (read-only) rather than scraping the file.
pub fn cursor_model_from_store(store_db: &std::path::Path) -> Option<String> {
    let conn =
        rusqlite::Connection::open_with_flags(store_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let hex: String = conn
        .query_row("SELECT value FROM meta LIMIT 1", [], |r| r.get(0))
        .ok()?;
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|i| {
            hex.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
        })
        .collect();
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("lastUsedModel")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn parse_cursor_session(
    chat_dir: &std::path::Path,
    ws_map: &HashMap<String, String>,
) -> Option<SessionInfo> {
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(chat_dir.join("meta.json")).ok()?).ok()?;
    // Skip empty stubs — Cursor creates a chat folder the moment a session is opened, before
    // any conversation happens; hasConversation flips true once there's real content.
    if !meta
        .get("hasConversation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return None;
    }

    let chat_id = chat_dir.file_name()?.to_string_lossy().into_owned();
    let workspace_hash = chat_dir
        .parent()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    let cwd = ws_map.get(&workspace_hash).cloned().unwrap_or_default();
    let project_name = if cwd.is_empty() {
        String::new()
    } else {
        std::path::Path::new(&cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };

    let updated_ms = meta
        .get("updatedAtMs")
        .or_else(|| meta.get("createdAtMs"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let timestamp = if updated_ms > 0 {
        unix_ms_to_iso(updated_ms)
    } else {
        fs::metadata(chat_dir.join("meta.json"))
            .ok()
            .and_then(|m| m.modified().ok())
            .map(system_time_to_iso)
            .unwrap_or_default()
    };

    let title = meta
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("Session {}", &chat_id[..8.min(chat_id.len())]));

    let model = cursor_model_from_store(&chat_dir.join("store.db")).unwrap_or_default();

    Some(SessionInfo {
        id: chat_id,
        title,
        timestamp,
        message_count: 0,
        project_name,
        project_path: cwd,
        git_branch: String::new(),
        claude_version: String::new(),
        tool_use_count: 0,
        duration_ms: 0,
        model,
        context_tokens: 0,
        context_limit: 0,
        cost_usd: 0.0,
        is_authoritative_stats: false,
        daily_cost: Default::default(),
        rate_limit_5h_pct: None,
        rate_limit_7d_pct: None,
        total_input_tokens: 0,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: 0,
        total_output_tokens: 0,
        daily_tokens: Default::default(),
        agent: "cursor".into(),
    })
}

// Directories Cursor has been used in — for the Add Projects picker's per-agent marks.
// Same shape as the Claude/Codex project lists; grouped by each chat's resolved cwd.
pub fn list_cursor_projects(ctx: &HostCtx) -> Vec<CodexProjectInfo> {
    let ws = cursor_workspace_map(ctx);
    let mut by_cwd: HashMap<String, (usize, String)> = HashMap::new();
    for dir in cursor_chat_dirs(ctx) {
        if let Some(s) = parse_cursor_session(&dir, &ws) {
            if s.project_path.is_empty() {
                continue;
            }
            let slot = by_cwd.entry(s.project_path).or_insert((0, String::new()));
            slot.0 += 1;
            if s.timestamp > slot.1 {
                slot.1 = s.timestamp;
            }
        }
    }
    let mut projects: Vec<CodexProjectInfo> = by_cwd
        .into_iter()
        .map(|(path, (session_count, last_active))| CodexProjectInfo {
            path,
            session_count,
            last_active,
        })
        .collect();
    projects.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    projects
}

// Enumerate ~/.cursor/chats/<hash>/<chat-uuid>/ session directories.
pub fn cursor_chat_dirs(ctx: &HostCtx) -> Vec<PathBuf> {
    let mut dirs = vec![];
    let Some(chats) = cursor_chats_dir(ctx) else {
        return dirs;
    };
    for ws in fs::read_dir(&chats).ok().into_iter().flatten().flatten() {
        if !ws.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        for chat in fs::read_dir(ws.path()).ok().into_iter().flatten().flatten() {
            if chat.file_type().is_ok_and(|ft| ft.is_dir()) {
                dirs.push(chat.path());
            }
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{path_str, sections_view, Fixture};

    fn md5_hex(s: &str) -> String {
        format!("{:x}", md5::compute(s.as_bytes()))
    }

    // A chat's store.db: one `meta` row whose TEXT value is hex-encoded JSON.
    fn write_store(path: &std::path::Path, meta: &serde_json::Value) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute("CREATE TABLE meta (key TEXT, value TEXT)", [])
            .unwrap();
        let hex: String = meta
            .to_string()
            .bytes()
            .map(|b| format!("{:02x}", b))
            .collect();
        conn.execute("INSERT INTO meta VALUES ('0', ?1)", [hex])
            .unwrap();
    }

    #[test]
    fn parse_cursor_session_resolves_cwd_and_model() {
        let f = Fixture::new();
        let cwd = "/work/cursor-proj";
        f.write(
            "home/.cursor/projects/work-cursor-proj/.workspace-trusted",
            serde_json::json!({ "workspacePath": cwd }).to_string(),
        );
        let chat_rel = format!("home/.cursor/chats/{}/0d1e2f3a-chat", md5_hex(cwd));
        f.write(
            format!("{chat_rel}/meta.json"),
            serde_json::json!({
                "title": "Fix login",
                "hasConversation": true,
                "createdAtMs": 1_000u64,
                "updatedAtMs": 1_709_210_096_999u64,
            })
            .to_string(),
        );
        let chat_dir = f.dir.path().join(&chat_rel);
        write_store(
            &chat_dir.join("store.db"),
            &serde_json::json!({ "lastUsedModel": "gpt-5", "mode": "agent" }),
        );

        let ctx = f.ctx();
        let ws = cursor_workspace_map(&ctx);
        assert_eq!(ws.get(&md5_hex(cwd)).map(String::as_str), Some(cwd));
        assert_eq!(cursor_chat_dirs(&ctx), vec![chat_dir.clone()]);

        let s = parse_cursor_session(&chat_dir, &ws).expect("session");
        assert_eq!(s.id, "0d1e2f3a-chat");
        assert_eq!(s.title, "Fix login");
        assert_eq!(s.project_path, cwd);
        assert_eq!(s.project_name, "cursor-proj");
        assert_eq!(s.model, "gpt-5");
        assert_eq!(s.timestamp, "2024-02-29T12:34:56Z");
        assert_eq!(s.agent, "cursor");
        assert!(!s.is_authoritative_stats);

        // Unknown workspace hash: no cwd, and no store.db means no model.
        let other = f.dir.path().join("home/.cursor/chats/ffff/abcdef123456");
        f.write(
            "home/.cursor/chats/ffff/abcdef123456/meta.json",
            r#"{"title":"  ","hasConversation":true,"createdAtMs":1709210096999}"#,
        );
        let s = parse_cursor_session(&other, &ws).expect("session");
        assert_eq!(s.project_path, "");
        assert_eq!(s.project_name, "");
        assert_eq!(s.model, "");
        assert_eq!(s.title, "Session abcdef12");
        assert_eq!(s.timestamp, "2024-02-29T12:34:56Z");
    }

    #[test]
    fn parse_cursor_session_skips_empty_chats() {
        let f = Fixture::new();
        let ws = HashMap::new();
        f.write(
            "home/.cursor/chats/h/stub/meta.json",
            r#"{"title":"x","hasConversation":false}"#,
        );
        assert!(
            parse_cursor_session(&f.dir.path().join("home/.cursor/chats/h/stub"), &ws).is_none()
        );
        f.write("home/.cursor/chats/h/nokey/meta.json", r#"{"title":"x"}"#);
        assert!(
            parse_cursor_session(&f.dir.path().join("home/.cursor/chats/h/nokey"), &ws).is_none()
        );
        // No meta.json at all.
        assert!(
            parse_cursor_session(&f.dir.path().join("home/.cursor/chats/h/none"), &ws).is_none()
        );
    }

    #[test]
    fn get_cursor_context_collects_rules_instructions_mcp() {
        let f = Fixture::new();
        let top = f.write("proj/.cursor/rules/top.mdc", "rule");
        let inner = f.write("proj/.cursor/rules/nested/deep/inner.md", "rule");
        f.write("proj/.cursor/rules/ignored.txt", "x");
        let agents = f.write("proj/AGENTS.md", "# a");
        f.write(
            "proj/.cursor/mcp.json",
            r#"{"mcpServers":{"zeta":{"command":"npx"},"alpha":{"url":"http://x"}}}"#,
        );
        f.write(
            "home/.cursor/mcp.json",
            r#"{"mcpServers":{"global-one":{"command":"g-cmd"},"plain":{}}}"#,
        );
        let project = f.dir.path().join("proj");

        let c = get_cursor_context(&f.ctx(), path_str(&project));
        assert!(c.present);
        let item = |n: &str, d: &str, p: String| (n.to_string(), d.to_string(), p);
        assert_eq!(
            sections_view(&c.sections),
            vec![
                (
                    "Rules".to_string(),
                    vec![
                        item("inner", "nested/deep", path_str(&inner)),
                        item("top", "", path_str(&top)),
                    ]
                ),
                (
                    "Instructions".to_string(),
                    vec![item("AGENTS.md", "project", path_str(&agents))]
                ),
                (
                    "MCP servers".to_string(),
                    vec![
                        // Object keys iterate alphabetically; project file before global.
                        item("alpha", "project", String::new()),
                        item("zeta", "npx", String::new()),
                        item("global-one", "g-cmd", String::new()),
                        item("plain", "global", String::new()),
                    ]
                ),
            ]
        );

        let empty = Fixture::new();
        let c = get_cursor_context(&empty.ctx(), path_str(&empty.dir.path().join("p")));
        assert!(!c.present && c.sections.is_empty());
    }
}

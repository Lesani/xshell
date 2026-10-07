use crate::agent_context::{AgentContextItem, AgentContextSection};
use crate::sessions::{CodexProjectInfo, SessionInfo};
use crate::time::unix_ms_to_iso;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

// ── opencode session parsing ──────────────────────────────────────────
// opencode keeps everything in a single SQLite database (~/.local/share/opencode/opencode.db,
// WAL mode — always opened read-only so a running opencode is never disturbed). The `session`
// table carries title, cwd, model, and lifetime token sums directly; the `message` table holds
// one JSON blob per turn whose per-turn usage feeds the per-day bands and the current-context
// figure. Context limits come from opencode's cached models.dev catalog. Cost stays 0 even
// though opencode records it — cost is a Claude-only surface by design — while
// is_authoritative_stats is true so the context bar renders: the numbers come from opencode
// itself, not an estimate.

pub fn opencode_data_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".local").join("share").join("opencode"))
}

pub fn opencode_open_db() -> Option<rusqlite::Connection> {
    let db = opencode_data_dir()?.join("opencode.db");
    if !db.exists() {
        return None;
    }
    rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

// Context window per "providerID/modelID" from ~/.cache/opencode/models.json (opencode's
// snapshot of the models.dev catalog). A model missing from the cache leaves the limit at 0,
// which hides the context bar — better than guessing a wrong budget.
pub fn opencode_context_limits() -> HashMap<String, u64> {
    let mut map = HashMap::new();
    let Some(home) = dirs::home_dir() else {
        return map;
    };
    let Ok(content) = fs::read_to_string(home.join(".cache").join("opencode").join("models.json"))
    else {
        return map;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return map;
    };
    for (pid, provider) in json.as_object().into_iter().flatten() {
        for (mid, model) in provider
            .get("models")
            .and_then(|m| m.as_object())
            .into_iter()
            .flatten()
        {
            if let Some(ctx) = model
                .get("limit")
                .and_then(|l| l.get("context"))
                .and_then(|v| v.as_u64())
            {
                map.insert(format!("{}/{}", pid, mid), ctx);
            }
        }
    }
    map
}

pub fn parse_opencode_sessions() -> Vec<SessionInfo> {
    let Some(conn) = opencode_open_db() else {
        return vec![];
    };
    let limits = opencode_context_limits();

    // One pass over the message table collects everything per-turn: user-message count,
    // per-day token bands, and the newest assistant turn's context size (input + cached
    // input ≈ what the model saw last turn). Band mapping onto Claude's [input,
    // cache_creation, cache_read, output]: input, cache.write, cache.read, output+reasoning.
    #[derive(Default)]
    struct Usage {
        user_msgs: usize,
        daily: std::collections::BTreeMap<String, [u64; 4]>,
        context: u64,
    }
    let mut usage: HashMap<String, Usage> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT session_id, data FROM message ORDER BY time_created")
    {
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
        for (sid, data) in rows.ok().into_iter().flatten().flatten() {
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            let slot = usage.entry(sid).or_default();
            match json.get("role").and_then(|v| v.as_str()) {
                Some("user") => slot.user_msgs += 1,
                Some("assistant") => {
                    let Some(t) = json.get("tokens") else {
                        continue;
                    };
                    let g = |k: &str| t.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    let cache = |k: &str| {
                        t.get("cache")
                            .and_then(|c| c.get(k))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                    };
                    let (input, output, reasoning, cache_read, cache_write) = (
                        g("input"),
                        g("output"),
                        g("reasoning"),
                        cache("read"),
                        cache("write"),
                    );
                    let created = json
                        .get("time")
                        .and_then(|tm| tm.get("created"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if input + output + reasoning + cache_read + cache_write > 0 && created > 0 {
                        let day = unix_ms_to_iso(created)[..10].to_string();
                        let band = slot.daily.entry(day).or_insert([0, 0, 0, 0]);
                        band[0] += input;
                        band[1] += cache_write;
                        band[2] += cache_read;
                        band[3] += output + reasoning;
                        slot.context = input + cache_read + cache_write; // rows arrive oldest→newest, so the last write wins
                    }
                }
                _ => {}
            }
        }
    }

    // The sessions themselves. parent_id IS NULL drops subagent child sessions; archived
    // sessions stay hidden, matching opencode's own session list.
    let mut out: Vec<SessionInfo> = vec![];
    let Ok(mut stmt) = conn.prepare("SELECT id, title, directory, model, version, time_created, time_updated, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write FROM session WHERE parent_id IS NULL AND time_archived IS NULL") else { return out };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, i64>(10)?,
            r.get::<_, i64>(11)?,
        ))
    });
    for (
        id,
        title,
        directory,
        model_json,
        version,
        created_ms,
        updated_ms,
        tok_in,
        tok_out,
        tok_reason,
        tok_cread,
        tok_cwrite,
    ) in rows.ok().into_iter().flatten().flatten()
    {
        // The directory is recorded with forward slashes even on Windows — normalize to the
        // platform separator so it merges with Claude's recording of the same project.
        let cwd = if cfg!(windows) {
            directory.replace('/', "\\")
        } else {
            directory
        };
        if cwd.is_empty() {
            continue;
        }
        let project_name = std::path::Path::new(&cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.clone());

        // model column: JSON like {"id":"...","providerID":"..."} — id feeds the badge,
        // provider/id together look up the context window.
        let (model, context_limit) = model_json
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .map(|m| {
                let id = m
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let provider = m
                    .get("providerID")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let limit = limits
                    .get(&format!("{}/{}", provider, id))
                    .copied()
                    .unwrap_or(0);
                (id, limit)
            })
            .unwrap_or_default();

        let title = if title.trim().is_empty() {
            format!("Session {}", &id[..8.min(id.len())])
        } else {
            title
        };
        let u = usage.remove(&id).unwrap_or_default();

        out.push(SessionInfo {
            id,
            title,
            timestamp: unix_ms_to_iso(updated_ms.max(0) as u64),
            message_count: u.user_msgs,
            project_name,
            project_path: cwd,
            git_branch: String::new(),
            claude_version: version,
            tool_use_count: 0,
            duration_ms: (updated_ms - created_ms).max(0) as u64,
            model,
            context_tokens: u.context,
            context_limit,
            cost_usd: 0.0,
            is_authoritative_stats: true,
            daily_cost: Default::default(),
            rate_limit_5h_pct: None,
            rate_limit_7d_pct: None,
            total_input_tokens: tok_in.max(0) as u64,
            total_cache_creation_tokens: tok_cwrite.max(0) as u64,
            total_cache_read_tokens: tok_cread.max(0) as u64,
            total_output_tokens: (tok_out + tok_reason).max(0) as u64,
            daily_tokens: u.daily,
            agent: "opencode".into(),
        });
    }
    out
}

// Directories opencode has been used in — for the Add Projects picker's per-agent marks.
// Grouped from the sessions' recorded directories, same shape as the other agents' lists.
pub fn list_opencode_projects() -> Vec<CodexProjectInfo> {
    let mut by_cwd: HashMap<String, (usize, String)> = HashMap::new();
    for s in parse_opencode_sessions() {
        let slot = by_cwd.entry(s.project_path).or_insert((0, String::new()));
        slot.0 += 1;
        if s.timestamp > slot.1 {
            slot.1 = s.timestamp;
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

// ── opencode project context ──────────────────────────────────────────
// What the context tree shows for opencode: AGENTS.md instructions (project + global),
// custom commands and agents (markdown files under .opencode/ and ~/.config/opencode/),
// and MCP servers from opencode.json(c). Returned as the same generic titled sections as
// Codex/Cursor so the tree renders it with no agent-specific frontend code.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpencodeContext {
    pub present: bool,
    pub sections: Vec<AgentContextSection>,
}

// opencode configs are JSONC (comments + trailing commas allowed). Try strict JSON first,
// then strip comments and trailing commas outside string literals and retry.
pub fn parse_jsonc(content: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str(content) {
        return Some(v);
    }
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == '/' && chars.peek() == Some(&'/') {
            while let Some(&n) = chars.peek() {
                if n == '\n' {
                    break;
                }
                chars.next();
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev = ' ';
            for n in chars.by_ref() {
                if prev == '*' && n == '/' {
                    break;
                }
                prev = n;
            }
        } else if c == ',' {
            // Drop a comma whose next non-whitespace char closes the container.
            let mut it = chars.clone();
            let mut next_sig = None;
            while let Some(&n) = it.peek() {
                if n.is_whitespace() {
                    it.next();
                } else {
                    next_sig = Some(n);
                    break;
                }
            }
            if next_sig != Some('}') && next_sig != Some(']') {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    serde_json::from_str(&out).ok()
}

// The opencode config files that apply to a project, nearest first: project opencode.json(c),
// then global ~/.config/opencode/opencode.json(c).
pub fn opencode_config_files(project_path: &std::path::Path) -> Vec<(PathBuf, &'static str)> {
    let mut files = vec![];
    for name in ["opencode.json", "opencode.jsonc"] {
        files.push((project_path.join(name), "project"));
    }
    if let Some(home) = dirs::home_dir() {
        for name in ["opencode.json", "opencode.jsonc"] {
            files.push((home.join(".config").join("opencode").join(name), "global"));
        }
    }
    files.into_iter().filter(|(p, _)| p.exists()).collect()
}

pub fn get_opencode_context(project_path: String) -> OpencodeContext {
    let pp = std::path::Path::new(&project_path);
    let mut sections: Vec<AgentContextSection> = vec![];
    let configs: Vec<(serde_json::Value, &str)> = opencode_config_files(pp)
        .into_iter()
        .filter_map(|(p, scope)| {
            fs::read_to_string(&p)
                .ok()
                .and_then(|c| parse_jsonc(&c))
                .map(|v| (v, scope))
        })
        .collect();

    // Instructions — AGENTS.md at the project root and the global one, plus any literal
    // (non-glob) paths from the configs' `instructions` arrays that resolve to real files.
    let mut instructions: Vec<AgentContextItem> = vec![];
    let agents_md = pp.join("AGENTS.md");
    if agents_md.exists() {
        instructions.push(AgentContextItem {
            name: "AGENTS.md".into(),
            detail: "project".into(),
            path: agents_md.to_string_lossy().into_owned(),
        });
    }
    if let Some(home) = dirs::home_dir() {
        let global = home.join(".config").join("opencode").join("AGENTS.md");
        if global.exists() {
            instructions.push(AgentContextItem {
                name: "AGENTS.md".into(),
                detail: "global".into(),
                path: global.to_string_lossy().into_owned(),
            });
        }
    }
    for (cfg, scope) in &configs {
        for entry in cfg
            .get("instructions")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let Some(rel) = entry.as_str().filter(|s| !s.contains('*')) else {
                continue;
            };
            let p = pp.join(rel);
            if p.exists() && !instructions.iter().any(|i| i.path == p.to_string_lossy()) {
                instructions.push(AgentContextItem {
                    name: p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| rel.to_string()),
                    detail: format!("{} config", scope),
                    path: p.to_string_lossy().into_owned(),
                });
            }
        }
    }
    if !instructions.is_empty() {
        sections.push(AgentContextSection {
            title: "Instructions".into(),
            items: instructions,
        });
    }

    // Commands and custom agents — markdown files under .opencode/{command,agent}/ (project)
    // and ~/.config/opencode/{command,agent}/ (global). opencode's slash-command equivalent.
    let mut md_section = |subdir: &str, title: &str| {
        let mut items: Vec<AgentContextItem> = vec![];
        let mut scan = |dir: PathBuf, scope: &str| {
            let mut stack = vec![dir];
            while let Some(d) = stack.pop() {
                for entry in fs::read_dir(&d).ok().into_iter().flatten().flatten() {
                    let p = entry.path();
                    if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                        stack.push(p);
                        continue;
                    }
                    if p.extension().is_none_or(|ext| ext != "md") {
                        continue;
                    }
                    items.push(AgentContextItem {
                        name: p
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        detail: scope.to_string(),
                        path: p.to_string_lossy().into_owned(),
                    });
                }
            }
        };
        scan(pp.join(".opencode").join(subdir), "project");
        if let Some(home) = dirs::home_dir() {
            scan(home.join(".config").join("opencode").join(subdir), "global");
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
        if !items.is_empty() {
            sections.push(AgentContextSection {
                title: title.into(),
                items,
            });
        }
    };
    md_section("command", "Commands");
    md_section("agent", "Agents");

    // MCP servers — the `mcp` object in each config; detail shows the transport type or the
    // local command, falling back to the config's scope.
    let mut mcp_items: Vec<AgentContextItem> = vec![];
    for (cfg, scope) in &configs {
        for (name, server) in cfg
            .get("mcp")
            .and_then(|v| v.as_object())
            .into_iter()
            .flatten()
        {
            if mcp_items.iter().any(|i| i.name == *name) {
                continue;
            }
            let detail = server
                .get("command")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .or_else(|| {
                    server
                        .get("url")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| scope.to_string());
            mcp_items.push(AgentContextItem {
                name: name.clone(),
                detail,
                path: String::new(),
            });
        }
    }
    if !mcp_items.is_empty() {
        sections.push(AgentContextSection {
            title: "MCP servers".into(),
            items: mcp_items,
        });
    }

    OpencodeContext {
        present: !sections.is_empty(),
        sections,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_jsonc_strips_comments_and_trailing_commas_outside_strings() {
        let src = "{\"u\": \"http://x\", // c\n \"a\": [1,2,], /* b */ \"k\": 3,\n}";
        assert_eq!(
            parse_jsonc(src),
            Some(serde_json::json!({"u": "http://x", "a": [1, 2], "k": 3}))
        );
    }

    #[test]
    fn parse_jsonc_comment_after_trailing_comma_is_none_today() {
        // Pins current behaviour: the trailing-comma check looks at the next raw character, so
        // a comment between the comma and the closing brace leaves the comma in place and the
        // final parse fails. Not fixed here on purpose.
        assert_eq!(parse_jsonc("{\"a\": 1, /* b */}"), None);
    }
}

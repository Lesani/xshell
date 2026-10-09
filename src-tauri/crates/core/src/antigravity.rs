use crate::agent_context::{AgentContextItem, AgentContextSection};
use crate::ctx::HostCtx;
use crate::sessions::{CodexProjectInfo, SessionInfo};
use crate::time::system_time_to_iso;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

// ── Antigravity session parsing ───────────────────────────────────────
// Google Antigravity's CLI (`agy`) stores one SQLite DB per conversation under
// ~/.gemini/antigravity-cli/conversations/<uuid>.db. The tables hold protobuf blobs with no
// published schema, but every field we need is a length-prefixed UTF-8 string we can recover
// by scanning for printable runs: the workspace cwd lives in trajectory_metadata_blob as a
// file:// URI, the first user prompt in the first step_type=14 row of `steps`, and the model
// id in executor_metadata. Titles additionally overlay from cache/conversation_metadata.json
// (written when agy's own picker loads a conversation; carries /rename titles and generated
// previews). Antigravity persists no token/cost/rate-limit data locally (usage is cloud-side
// "AI Credits"), so those stay zero and the context bar stays hidden.

pub fn antigravity_data_dir(ctx: &HostCtx) -> Option<PathBuf> {
    ctx.home
        .clone()
        .map(|h| h.join(".gemini").join("antigravity-cli"))
}

// Printable UTF-8 runs from a protobuf blob. Bytes 0x20..0x7E plus complete, valid multi-byte
// UTF-8 sequences extend a run; anything else (control bytes, protobuf wire noise, malformed
// high bytes) terminates it. Validating sequences here — instead of on the whole run — keeps
// an ASCII model id from being discarded just because adjacent wire bytes weren't UTF-8.
pub fn printable_runs(data: &[u8], min_len: usize) -> Vec<String> {
    let mut runs: Vec<String> = vec![];
    let mut cur = String::new();
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if (0x20..0x7f).contains(&b) {
            cur.push(b as char);
            i += 1;
            continue;
        }
        // Multi-byte sequence: lead byte determines length; append only if fully valid.
        let seq_len = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        if seq_len > 0 && i + seq_len <= data.len() {
            if let Ok(s) = std::str::from_utf8(&data[i..i + seq_len]) {
                cur.push_str(s);
                i += seq_len;
                continue;
            }
        }
        if cur.len() >= min_len {
            runs.push(std::mem::take(&mut cur));
        } else {
            cur.clear();
        }
        i += 1;
    }
    if cur.len() >= min_len {
        runs.push(cur);
    }
    runs
}

// A run often starts with the protobuf length prefix when that byte happens to be printable
// (strings of 32..126 bytes). Detect it — first byte's value equals the remaining byte count —
// and strip it. Shorter strings have a non-printable prefix and arrive clean.
pub fn strip_len_prefix(run: &str) -> &str {
    let b = run.as_bytes();
    if !b.is_empty() && (b[0] as usize) == b.len() - 1 {
        &run[1..]
    } else {
        run
    }
}

// Title/preview overlay from cache/conversation_metadata.json: id → display name.
// Precedence within a summary: user rename (Title) > generated preview.
pub fn antigravity_conversation_names(ctx: &HostCtx) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(dir) = antigravity_data_dir(ctx) else {
        return names;
    };
    let Ok(content) = fs::read_to_string(dir.join("cache").join("conversation_metadata.json"))
    else {
        return names;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return names;
    };
    for (id, entry) in json
        .get("conversations")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten()
    {
        let Some(summary) = entry.get("summary") else {
            continue;
        };
        let title = summary
            .get("Title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                summary
                    .get("Preview")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
            });
        if let Some(t) = title {
            names.insert(id.clone(), t.to_string());
        }
    }
    names
}

pub fn parse_antigravity_conversation(
    path: &std::path::Path,
    names: &HashMap<String, String>,
) -> Option<SessionInfo> {
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;

    // Workspace cwd — the file:// URI in the trajectory metadata blob.
    let meta: Vec<u8> = conn
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id='main'",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let cwd = printable_runs(&meta, 8).iter().find_map(|run| {
        let start = run.find("file:///")?;
        let raw = &run[start + "file:///".len()..];
        // Minimal percent-decoding (paths with spaces arrive as %20).
        let mut decoded = String::with_capacity(raw.len());
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                    decoded.push(v as char);
                    i += 3;
                    continue;
                }
            }
            decoded.push(bytes[i] as char);
            i += 1;
        }
        Some(if cfg!(windows) {
            decoded.replace('/', "\\")
        } else {
            format!("/{}", decoded)
        })
    })?;

    // User messages — step_type 14 rows. A freshly-opened conversation that never received a
    // prompt has none; skip those stubs (they'd be untitled noise in the listings).
    let message_count: usize = conn
        .query_row("SELECT COUNT(*) FROM steps WHERE step_type = 14", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
        .max(0) as usize;
    if message_count == 0 {
        return None;
    }

    // First prompt (title fallback). Filter the blob's noise runs — UUID references carry
    // '$' separators, paths and URIs carry slashes — then the first survivor is the prompt.
    let first_prompt = conn
        .query_row(
            "SELECT step_payload FROM steps WHERE step_type = 14 ORDER BY idx LIMIT 1",
            [],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|payload| {
            printable_runs(&payload, 4)
                .into_iter()
                .find(|run| !run.contains('$') && !run.contains('/') && !run.contains('\\'))
        })
        .map(|run| {
            strip_len_prefix(&run)
                .trim()
                .chars()
                .take(120)
                .collect::<String>()
        })
        .unwrap_or_default();

    // Model — executor_metadata mentions the resolved model id (e.g. "gemini-3.5-flash-low").
    // Scan for the longest known-family match so surrounding prose never wins.
    let mut model = String::new();
    if let Ok(mut stmt) = conn.prepare("SELECT data FROM executor_metadata") {
        for blob in stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            for run in printable_runs(&blob, 6) {
                for family in ["gemini-", "claude-", "gpt-"] {
                    let Some(start) = run.find(family) else {
                        continue;
                    };
                    let candidate: String = run[start..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
                        .collect();
                    if candidate.len() > model.len() {
                        model = candidate;
                    }
                }
            }
        }
    }

    let title = if let Some(name) = names.get(&id) {
        name.clone()
    } else if !first_prompt.is_empty() {
        first_prompt
    } else {
        format!("Session {}", &id[..8.min(id.len())])
    };
    let timestamp = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(system_time_to_iso)
        .unwrap_or_default();
    let project_name = std::path::Path::new(&cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.clone());

    Some(SessionInfo {
        id,
        title,
        timestamp,
        message_count,
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
        agent: "antigravity".into(),
    })
}

pub fn parse_antigravity_sessions(ctx: &HostCtx) -> Vec<SessionInfo> {
    let Some(dir) = antigravity_data_dir(ctx).map(|d| d.join("conversations")) else {
        return vec![];
    };
    let names = antigravity_conversation_names(ctx);
    fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "db"))
        .filter_map(|p| parse_antigravity_conversation(&p, &names))
        .collect()
}

// Directories Antigravity has been used in — for the Add Projects picker's per-agent marks.
pub fn list_antigravity_projects(ctx: &HostCtx) -> Vec<CodexProjectInfo> {
    let mut by_cwd: HashMap<String, (usize, String)> = HashMap::new();
    for s in parse_antigravity_sessions(ctx) {
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

// ── Antigravity project context ───────────────────────────────────────
// What the context tree shows for Antigravity: skills (project .agents/skills + global
// ~/.gemini/antigravity-cli/skills), rules (.agents/rules), installed plugins, and MCP
// servers (~/.gemini/config/mcp_config.json + per-plugin mcp_config.json). Same generic
// titled sections as the other agents.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AntigravityContext {
    pub present: bool,
    pub sections: Vec<AgentContextSection>,
}

pub fn get_antigravity_context(ctx: &HostCtx, project_path: String) -> AntigravityContext {
    let pp = std::path::Path::new(&project_path);
    let home = ctx.home.clone();
    let data_dir = antigravity_data_dir(ctx);
    let mut sections: Vec<AgentContextSection> = vec![];

    // Markdown files under a directory tree (skills and rules folders allow nesting).
    let scan_md = |dir: PathBuf, scope: &str, items: &mut Vec<AgentContextItem>| {
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

    // Skills — become slash commands in the TUI; project-level ones live in .agents/skills.
    let mut skills: Vec<AgentContextItem> = vec![];
    scan_md(pp.join(".agents").join("skills"), "project", &mut skills);
    if let Some(d) = &data_dir {
        scan_md(d.join("skills"), "global", &mut skills);
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    if !skills.is_empty() {
        sections.push(AgentContextSection {
            title: "Skills".into(),
            items: skills,
        });
    }

    // Rules — project-scoped codebase constraints.
    let mut rules: Vec<AgentContextItem> = vec![];
    scan_md(pp.join(".agents").join("rules"), "project", &mut rules);
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    if !rules.is_empty() {
        sections.push(AgentContextSection {
            title: "Rules".into(),
            items: rules,
        });
    }

    // Plugins — one entry per installed bundle; the manifest's description is the detail.
    let mut plugins: Vec<AgentContextItem> = vec![];
    if let Some(d) = &data_dir {
        for entry in fs::read_dir(d.join("plugins"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let manifest = entry.path().join("plugin.json");
            let Ok(content) = fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
                continue;
            };
            let Some(name) = json.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let detail = json
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("plugin")
                .to_string();
            plugins.push(AgentContextItem {
                name: name.to_string(),
                detail,
                path: manifest.to_string_lossy().into_owned(),
            });
        }
    }
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    if !plugins.is_empty() {
        sections.push(AgentContextSection {
            title: "Plugins".into(),
            items: plugins,
        });
    }

    // MCP servers — global config plus each plugin's bundled mcp_config.json.
    let mut mcp_items: Vec<AgentContextItem> = vec![];
    let mut read_mcp = |path: PathBuf, scope: &str| {
        let Ok(content) = fs::read_to_string(&path) else {
            return;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
            return;
        };
        for (name, cfg) in json
            .get("mcpServers")
            .and_then(|v| v.as_object())
            .into_iter()
            .flatten()
        {
            if mcp_items.iter().any(|i| i.name == *name) {
                continue;
            }
            let detail = cfg
                .get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    cfg.get("url")
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
    };
    if let Some(h) = &home {
        read_mcp(
            h.join(".gemini").join("config").join("mcp_config.json"),
            "global",
        );
    }
    if let Some(d) = &data_dir {
        for entry in fs::read_dir(d.join("plugins"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            read_mcp(entry.path().join("mcp_config.json"), "plugin");
        }
    }
    if !mcp_items.is_empty() {
        sections.push(AgentContextSection {
            title: "MCP servers".into(),
            items: mcp_items,
        });
    }

    AntigravityContext {
        present: !sections.is_empty(),
        sections,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;

    // A length-delimited protobuf string field: tag byte, length byte, payload.
    fn field(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![tag, payload.len() as u8];
        v.extend_from_slice(payload);
        v
    }

    // A conversation DB with the three tables the parser reads. `prompts` are (idx, payload)
    // rows of step_type 14.
    fn write_conversation(path: &std::path::Path, cwd_uri: &str, prompts: &[(i64, Vec<u8>)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE trajectory_metadata_blob (id TEXT, data BLOB);
             CREATE TABLE steps (idx INTEGER, step_type INTEGER, step_payload BLOB);
             CREATE TABLE executor_metadata (data BLOB);",
        )
        .unwrap();
        let mut meta = field(0x0a, b"\x01\x02");
        meta.extend(field(0x12, cwd_uri.as_bytes()));
        meta.extend(field(0x1a, b"other"));
        conn.execute(
            "INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)",
            [meta],
        )
        .unwrap();
        for (idx, payload) in prompts {
            conn.execute(
                "INSERT INTO steps VALUES (?1, 14, ?2)",
                rusqlite::params![idx, payload],
            )
            .unwrap();
        }
        // A non-prompt step.
        conn.execute(
            "INSERT INTO steps VALUES (0, 15, ?1)",
            [field(0x0a, b"tool output text")],
        )
        .unwrap();
        let mut exec = field(0x0a, b"runs on gemini-3 family");
        exec.extend(field(0x12, b"gemini-3.5-flash-low"));
        exec.push(0xff);
        conn.execute("INSERT INTO executor_metadata VALUES (?1)", [exec])
            .unwrap();
    }

    #[test]
    fn printable_runs_keeps_valid_utf8_and_splits_on_wire_bytes() {
        let mut data = b"\x0a\x05hello\x12".to_vec();
        data.extend_from_slice("grüße".as_bytes());
        // A truncated multi-byte lead (0xC3 with no continuation byte) ends the run.
        data.extend_from_slice(b"\x00ab\x1aworld\xc3");
        data.extend_from_slice(b"tail");
        assert_eq!(
            printable_runs(&data, 3),
            vec!["hello", "grüße", "world", "tail"]
        );
        // Runs shorter than min_len are dropped ("ab" here, and "tail" at min 5).
        assert_eq!(printable_runs(&data, 5), vec!["hello", "grüße", "world"]);
        assert!(printable_runs(b"\x00\x01\x02", 1).is_empty());
    }

    #[test]
    fn strip_len_prefix_removes_matching_length_byte() {
        // '(' is 40: the remaining 40 bytes are the string.
        let s = "Refactor the parser into smaller modules";
        assert_eq!(s.len(), 40);
        let run = format!("({}", s);
        assert_eq!(strip_len_prefix(&run), s);
        // First byte does not match the remaining length: untouched.
        assert_eq!(strip_len_prefix("(short"), "(short");
        assert_eq!(strip_len_prefix("abc"), "abc");
        assert_eq!(strip_len_prefix(""), "");
    }

    #[test]
    fn parse_antigravity_conversation_extracts_cwd_prompt_model() {
        let f = Fixture::new();
        let prompt = "Refactor the parser into smaller modules";
        // Noise run with '$' (a UUID reference) first, then the prompt with a printable
        // length prefix ('(' == 40).
        let mut first = field(0x0a, b"ref$1234-5678$abcd");
        first.extend(field(0x12, prompt.as_bytes()));
        let second = field(0x12, b"a later prompt");
        let db = f
            .home()
            .join(".gemini/antigravity-cli/conversations/conv-1234-abcd.db");
        write_conversation(&db, "file:///home/u/my%20proj", &[(2, second), (1, first)]);

        let s = parse_antigravity_conversation(&db, &HashMap::new()).expect("session");
        assert_eq!(s.id, "conv-1234-abcd");
        let expected_cwd = if cfg!(windows) {
            "home\\u\\my proj"
        } else {
            "/home/u/my proj"
        };
        assert_eq!(s.project_path, expected_cwd);
        assert_eq!(s.project_name, "my proj");
        assert_eq!(s.title, prompt);
        assert_eq!(s.message_count, 2);
        // The longest known-family match wins over the shorter mention.
        assert_eq!(s.model, "gemini-3.5-flash-low");
        assert_eq!(s.agent, "antigravity");
        assert!(!s.is_authoritative_stats);

        // A name from conversation_metadata.json overrides the prompt.
        let names = HashMap::from([("conv-1234-abcd".to_string(), "Renamed".to_string())]);
        assert_eq!(
            parse_antigravity_conversation(&db, &names).unwrap().title,
            "Renamed"
        );
    }

    #[test]
    fn parse_antigravity_skips_conversations_without_prompts() {
        let f = Fixture::new();
        let dir = f.home().join(".gemini/antigravity-cli/conversations");
        write_conversation(&dir.join("empty.db"), "file:///work/p", &[]);
        write_conversation(
            &dir.join("full.db"),
            "file:///work/p",
            &[(1, field(0x12, b"do the thing"))],
        );
        f.write("home/.gemini/antigravity-cli/conversations/notes.txt", "x");

        assert!(parse_antigravity_conversation(&dir.join("empty.db"), &HashMap::new()).is_none());
        let sessions = parse_antigravity_sessions(&f.ctx());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "full");
        assert_eq!(sessions[0].title, "do the thing");

        let projects = list_antigravity_projects(&f.ctx());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].session_count, 1);
        assert_eq!(projects[0].path, sessions[0].project_path);
    }

    #[test]
    fn antigravity_names_prefer_title_over_preview() {
        let f = Fixture::new();
        f.write(
            "home/.gemini/antigravity-cli/cache/conversation_metadata.json",
            serde_json::json!({"conversations": {
                "a": {"summary": {"Title": "Renamed", "Preview": "preview a"}},
                "b": {"summary": {"Title": "  ", "Preview": "preview b"}},
                "c": {"summary": {}},
                "d": {},
            }})
            .to_string(),
        );
        let names = antigravity_conversation_names(&f.ctx());
        let mut got: Vec<(&str, &str)> = names
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        got.sort();
        assert_eq!(got, vec![("a", "Renamed"), ("b", "preview b")]);
        assert!(antigravity_conversation_names(&Fixture::new().ctx()).is_empty());
    }
}

use crate::agent_context::{AgentContextItem, AgentContextSection};
use crate::paths::find_git_root;
use crate::sessions::{CodexProjectInfo, SessionInfo};
use crate::time::system_time_to_iso;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::time::SystemTime;

// ── Codex session parsing ─────────────────────────────────────────────
// Codex rollouts (~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl) parse into the same
// SessionInfo shape Claude sessions use. Mapping notes: title = first user message (Codex
// has no /rename or auto-summary), model from the latest turn_context, context usage from
// the latest token_count's last_token_usage (the final turn's prompt+completion ≈ current
// conversation size), claude_version carries Codex's cli_version. Cost stays 0 — Codex
// subscription plans have no per-use cost — while is_authoritative_stats is true so the
// context bar renders: the numbers come from Codex itself, not an estimate.

pub fn codex_rollout_files() -> Vec<std::path::PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    let mut files = vec![];
    let mut stack = vec![home.join(".codex").join("sessions")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
            let p = entry.path();
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                stack.push(p);
                continue;
            }
            if p.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(p);
            }
        }
    }
    files
}

// User-assigned session names (Codex's rename feature) don't live in the rollout files —
// they land in ~/.codex/session_index.jsonl, one JSON line per named session. Load once
// per listing call and overlay onto parsed sessions; for repeated renames of the same id
// the last line wins (insertion order preserves that).
pub fn codex_session_names() -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(home) = dirs::home_dir() else {
        return names;
    };
    let Ok(content) = fs::read_to_string(home.join(".codex").join("session_index.jsonl")) else {
        return names;
    };
    for line in content.lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            if let (Some(id), Some(name)) = (
                json.get("id").and_then(|v| v.as_str()),
                json.get("thread_name").and_then(|v| v.as_str()),
            ) {
                if !name.trim().is_empty() {
                    names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    names
}

pub fn parse_codex_session(
    path: &std::path::Path,
    names: &HashMap<String, String>,
) -> Option<SessionInfo> {
    let content = fs::read_to_string(path).ok()?;

    let mut session_id = String::new();
    let mut cwd = String::new();
    let mut git_branch = String::new();
    let mut cli_version = String::new();
    let mut model = String::new();
    let mut first_user_message = String::new();
    let mut message_count = 0usize;
    let mut context_tokens = 0u64;
    let mut context_limit = 0u64;
    let mut last_ts = String::new();
    // Token accounting: total_token_usage snapshots are cumulative and monotonic, so the
    // per-day buckets come from diffing consecutive snapshots (robust against Codex writing
    // several token_count events per turn). Band mapping onto Claude's [input,
    // cache_creation, cache_read, output]: non-cached input, 0 (no such concept), cached
    // input, output (already includes reasoning tokens — total = input + output holds).
    let mut daily_tokens: std::collections::BTreeMap<String, [u64; 4]> = Default::default();
    let mut prev_totals: (u64, u64, u64) = (0, 0, 0); // (input, cached_input, output)
    let mut last_totals: (u64, u64, u64) = (0, 0, 0);

    for line in content.lines() {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(ts) = json.get("timestamp").and_then(|v| v.as_str()) {
            last_ts = ts.to_string();
        }
        let Some(payload) = json.get("payload") else {
            continue;
        };
        match json.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => {
                session_id = payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                cwd = payload
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                cli_version = payload
                    .get("cli_version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                git_branch = payload
                    .get("git")
                    .and_then(|g| g.get("branch"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
            }
            Some("turn_context") => {
                if let Some(m) = payload.get("model").and_then(|v| v.as_str()) {
                    model = m.to_string();
                }
            }
            Some("event_msg") => match payload.get("type").and_then(|v| v.as_str()) {
                Some("user_message") => {
                    message_count += 1;
                    if first_user_message.is_empty() {
                        if let Some(m) = payload.get("message").and_then(|v| v.as_str()) {
                            first_user_message =
                                m.trim().replace('\n', " ").chars().take(120).collect();
                        }
                    }
                }
                Some("token_count") => {
                    if let Some(info) = payload.get("info") {
                        if let Some(last) = info.get("last_token_usage") {
                            let turn = last
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                                + last
                                    .get("output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                            if turn > 0 {
                                context_tokens = turn;
                            }
                        }
                        if let Some(w) = info.get("model_context_window").and_then(|v| v.as_u64()) {
                            context_limit = w;
                        }
                        if let Some(totals) = info.get("total_token_usage") {
                            let input = totals
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let cached = totals
                                .get("cached_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let output = totals
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let (d_input, d_cached, d_output) = (
                                input.saturating_sub(prev_totals.0),
                                cached.saturating_sub(prev_totals.1),
                                output.saturating_sub(prev_totals.2),
                            );
                            if (d_input + d_output > 0) && last_ts.len() >= 10 {
                                let day = daily_tokens
                                    .entry(last_ts[..10].to_string())
                                    .or_insert([0, 0, 0, 0]);
                                day[0] += d_input.saturating_sub(d_cached);
                                day[2] += d_cached;
                                day[3] += d_output;
                            }
                            prev_totals = (input, cached, output);
                            last_totals = (input, cached, output);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    if session_id.is_empty() || cwd.is_empty() {
        return None;
    }
    let timestamp = if last_ts.is_empty() {
        fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(system_time_to_iso)
            .unwrap_or_default()
    } else {
        last_ts
    };
    let project_name = std::path::Path::new(&cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.clone());
    // Title precedence mirrors the Claude side: user rename > first prompt > bare id.
    let title = if let Some(name) = names.get(&session_id) {
        name.clone()
    } else if !first_user_message.is_empty() {
        first_user_message
    } else {
        format!("Session {}", &session_id[..8.min(session_id.len())])
    };

    Some(SessionInfo {
        id: session_id,
        title,
        timestamp,
        message_count,
        project_name,
        project_path: cwd,
        git_branch,
        claude_version: cli_version,
        tool_use_count: 0,
        duration_ms: 0,
        model,
        context_tokens,
        context_limit,
        cost_usd: 0.0,
        is_authoritative_stats: true,
        daily_cost: Default::default(),
        rate_limit_5h_pct: None,
        rate_limit_7d_pct: None,
        total_input_tokens: last_totals.0.saturating_sub(last_totals.1),
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: last_totals.1,
        total_output_tokens: last_totals.2,
        daily_tokens,
        agent: "codex".into(),
    })
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CodexContext {
    pub present: bool,               // any Codex artifacts found for this project
    pub trust_level: Option<String>, // from [projects.'<path>'] in config.toml
    pub sections: Vec<AgentContextSection>,
}

pub fn get_codex_context(project_path: String) -> CodexContext {
    let home = dirs::home_dir();
    let mut sections: Vec<AgentContextSection> = vec![];

    // Instructions — AGENTS.md at the project root (plus git root when different) and the
    // global ~/.codex/AGENTS.md; Codex's counterpart of CLAUDE.md files.
    let pp = std::path::Path::new(&project_path);
    let mut candidates: Vec<(std::path::PathBuf, &str)> = vec![(pp.join("AGENTS.md"), "project")];
    if let Some(root) = find_git_root(pp) {
        if root.as_path() != pp {
            candidates.push((root.join("AGENTS.md"), "repo root"));
        }
    }
    if let Some(h) = &home {
        candidates.push((h.join(".codex").join("AGENTS.md"), "global"));
    }
    let instructions: Vec<AgentContextItem> = candidates
        .into_iter()
        .filter(|(p, _)| p.exists())
        .map(|(p, scope)| AgentContextItem {
            name: "AGENTS.md".into(),
            detail: scope.into(),
            path: p.to_string_lossy().into_owned(),
        })
        .collect();
    if !instructions.is_empty() {
        sections.push(AgentContextSection {
            title: "Instructions".into(),
            items: instructions,
        });
    }

    // Prompts — ~/.codex/prompts/*.md, Codex's slash-command equivalent (always global).
    if let Some(h) = &home {
        let mut prompts: Vec<AgentContextItem> = fs::read_dir(h.join(".codex").join("prompts"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_none_or(|ext| ext != "md") {
                    return None;
                }
                let stem = p.file_stem()?.to_string_lossy().into_owned();
                Some(AgentContextItem {
                    name: format!("/{}", stem),
                    detail: "global".into(),
                    path: p.to_string_lossy().into_owned(),
                })
            })
            .collect();
        prompts.sort_by(|a, b| a.name.cmp(&b.name));
        if !prompts.is_empty() {
            sections.push(AgentContextSection {
                title: "Prompts".into(),
                items: prompts,
            });
        }
    }

    // MCP servers + per-project trust level from ~/.codex/config.toml. The file is simple
    // enough that a line scan beats pulling in a TOML dependency: section headers carry the
    // server name / project path, `command =` and `trust_level =` live inside them.
    let mut mcp_items: Vec<AgentContextItem> = vec![];
    let mut trust_level: Option<String> = None;
    if let Some(h) = &home {
        if let Ok(cfg) = fs::read_to_string(h.join(".codex").join("config.toml")) {
            let norm_project = project_path
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_lowercase();
            let unquote = |s: &str| s.trim().trim_matches('"').trim_matches('\'').to_string();
            let mut current_section = String::new();
            for raw in cfg.lines() {
                let line = raw.trim();
                if line.starts_with('[') && line.ends_with(']') {
                    current_section = line[1..line.len() - 1].trim().to_string();
                    if let Some(name) = current_section.strip_prefix("mcp_servers.") {
                        mcp_items.push(AgentContextItem {
                            name: unquote(name),
                            detail: String::new(),
                            path: String::new(),
                        });
                    }
                    continue;
                }
                if current_section.starts_with("mcp_servers.") && line.starts_with("command") {
                    if let (Some(last), Some(v)) =
                        (mcp_items.last_mut(), line.split_once('=').map(|(_, v)| v))
                    {
                        if last.detail.is_empty() {
                            last.detail = unquote(v);
                        }
                    }
                } else if let Some(key) = current_section.strip_prefix("projects.") {
                    let key = unquote(key)
                        .replace('/', "\\")
                        .trim_end_matches('\\')
                        .to_lowercase();
                    if key == norm_project && line.starts_with("trust_level") {
                        if let Some(v) = line.split_once('=').map(|(_, v)| v) {
                            let v = unquote(v);
                            if !v.is_empty() {
                                trust_level = Some(v);
                            }
                        }
                    }
                }
            }
        }
    }
    if !mcp_items.is_empty() {
        sections.push(AgentContextSection {
            title: "MCP servers".into(),
            items: mcp_items,
        });
    }

    CodexContext {
        present: !sections.is_empty() || trust_level.is_some(),
        trust_level,
        sections,
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CodexRateWindow {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<u64>, // unix seconds
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DailySessionCount {
    pub date: String, // YYYY-MM-DD from the sessions/YYYY/MM/DD/ directory layout (local date)
    pub count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CodexUsage {
    pub present: bool,                      // any rollout files at all
    pub primary: Option<CodexRateWindow>,   // 5h window
    pub secondary: Option<CodexRateWindow>, // 7d window
    pub plan_type: Option<String>,
    // When the token_count event we read was written — rate limits are only as fresh as the
    // last Codex run, so the UI shows "as of X ago" instead of presenting them as live.
    pub rate_limits_updated_iso: Option<String>,
    pub daily_sessions: Vec<DailySessionCount>,
}

pub fn get_codex_usage() -> CodexUsage {
    let mut out = CodexUsage {
        present: false,
        primary: None,
        secondary: None,
        plan_type: None,
        rate_limits_updated_iso: None,
        daily_sessions: vec![],
    };
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    let sessions_dir = home.join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return out;
    }

    // Collect rollout files with their mtime and the local date encoded in the directory path.
    let mut files: Vec<(std::path::PathBuf, Option<SystemTime>, Option<String>)> = vec![];
    let mut stack = vec![sessions_dir];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
            let p = entry.path();
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let date = (|| {
                let dd = p.parent()?.file_name()?.to_str()?.to_string();
                let mm = p.parent()?.parent()?.file_name()?.to_str()?.to_string();
                let yyyy = p
                    .parent()?
                    .parent()?
                    .parent()?
                    .file_name()?
                    .to_str()?
                    .to_string();
                if yyyy.len() == 4 && yyyy.chars().all(|c| c.is_ascii_digit()) {
                    Some(format!("{}-{}-{}", yyyy, mm, dd))
                } else {
                    None
                }
            })();
            let mtime = fs::metadata(&p).ok().and_then(|m| m.modified().ok());
            files.push((p, mtime, date));
        }
    }
    if files.is_empty() {
        return out;
    }
    out.present = true;

    let mut by_date: HashMap<String, usize> = HashMap::new();
    for (_, _, date) in &files {
        if let Some(d) = date {
            *by_date.entry(d.clone()).or_insert(0) += 1;
        }
    }
    out.daily_sessions = by_date
        .into_iter()
        .map(|(date, count)| DailySessionCount { date, count })
        .collect();
    out.daily_sessions.sort_by(|a, b| a.date.cmp(&b.date));

    // Rate limits: the last token_count event of the most recently touched rollout that has
    // one (a just-started session may not have emitted any yet — fall back to the next file).
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    let parse_window = |w: &serde_json::Value| CodexRateWindow {
        used_percent: w.get("used_percent").and_then(|v| v.as_f64()),
        window_minutes: w.get("window_minutes").and_then(|v| v.as_u64()),
        resets_at: w.get("resets_at").and_then(|v| v.as_u64()),
    };
    for (path, _, _) in &files {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let Some(line) = content
            .lines()
            .rev()
            .find(|l| l.contains("\"token_count\""))
        else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(payload) = json.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
            continue;
        }
        let Some(rl) = payload.get("rate_limits") else {
            continue;
        };
        out.primary = rl.get("primary").map(parse_window);
        out.secondary = rl.get("secondary").map(parse_window);
        out.plan_type = rl
            .get("plan_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        out.rate_limits_updated_iso = json
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        break;
    }
    out
}

pub fn list_codex_projects() -> Vec<CodexProjectInfo> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    let sessions_dir = home.join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return vec![];
    }

    let mut by_cwd: HashMap<String, (usize, Option<SystemTime>)> = HashMap::new();
    let mut stack = vec![sessions_dir];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
            let p = entry.path();
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }

            let mut cwd: Option<String> = None;
            if let Ok(file) = fs::File::open(&p) {
                let mut first = String::new();
                if BufReader::new(file).read_line(&mut first).is_ok() {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&first) {
                        cwd = json
                            .get("payload")
                            .and_then(|pl| pl.get("cwd"))
                            .and_then(|c| c.as_str())
                            .map(|s| s.to_string());
                    }
                }
            }
            let Some(cwd) = cwd else { continue };

            let slot = by_cwd.entry(cwd).or_insert((0, None));
            slot.0 += 1;
            if let Some(modified) = fs::metadata(&p).ok().and_then(|m| m.modified().ok()) {
                if slot.1.is_none_or(|prev| modified > prev) {
                    slot.1 = Some(modified);
                }
            }
        }
    }

    let mut projects: Vec<CodexProjectInfo> = by_cwd
        .into_iter()
        .map(|(path, (session_count, latest))| CodexProjectInfo {
            path,
            session_count,
            last_active: latest.map(system_time_to_iso).unwrap_or_default(),
        })
        .collect();
    projects.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    projects
}

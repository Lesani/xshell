use crate::ctx::HostCtx;
use crate::sessions::{MessagePreview, ProjectInfo, SessionInfo};
use crate::time::system_time_to_iso;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

// Mirror Claude Code's encoding of a project path into the directory name under
// `~/.claude/projects/`. Every non-alphanumeric character collapses to `-` — including
// `.` and `_`, which Claude Code also converts (e.g. `CalcApps.Framework` →
// `CalcApps-Framework`, `SSY2_Lab` → `SSY2-Lab`).
// e.g. `C:\Users\alex\projects\my-app`  →  `C--Users-alex-projects-my-app`
pub fn encode_project_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// Cached SessionInfo keyed by JSONL path. The cache is invalidated whenever the JSONL's
// mtime changes (active session got a new turn) OR the xshell-stats sidecar's mtime
// changes (cost/rate-limit refresh). Active sessions still re-parse on every tick — those
// are 1-2 files. Idle sessions cost only two stat() calls. Drops the title-sync poll cost
// from "parse every JSONL in the project every 5s" (tens of MB) to a few syscalls.
pub struct SessionCacheEntry {
    jsonl_mtime: SystemTime,
    stats_mtime: Option<SystemTime>,
    project_path: String,
    info: SessionInfo,
}

pub fn session_cache() -> &'static Mutex<HashMap<PathBuf, SessionCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, SessionCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn stats_path_for(ctx: &HostCtx, session_id: &str) -> Option<PathBuf> {
    Some(
        ctx.home
            .clone()?
            .join(".claude")
            .join("xshell-stats")
            .join(format!("{}.json", session_id)),
    )
}

// `BufRead::lines` yields `Err` for a line that is not valid UTF-8 and then keeps going, so
// `flatten` skips just that line. clippy's suggestion (`map_while(Result::ok)`) would stop
// parsing at the first such line, which changes behaviour.
#[allow(clippy::lines_filter_map_ok)]
pub fn parse_session(
    ctx: &HostCtx,
    path: &std::path::Path,
    project_name: &str,
    project_path: &str,
) -> Option<SessionInfo> {
    let session_id = path.file_stem()?.to_string_lossy().to_string();
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let mtime_iso = system_time_to_iso(modified);

    // Stats-file mtime (sidecar from the xshell-stats statusline hook). May not exist —
    // None is a valid cache key value, so a session without stats stays cached cleanly.
    let stats_mtime = stats_path_for(ctx, &session_id)
        .and_then(|p| fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok());

    // Cache lookup — return clone if all three keys match (jsonl mtime, stats mtime, project_path).
    if let Ok(cache) = session_cache().lock() {
        if let Some(entry) = cache.get(path) {
            if entry.jsonl_mtime == modified
                && entry.stats_mtime == stats_mtime
                && entry.project_path == project_path
            {
                return Some(entry.info.clone());
            }
        }
    }

    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    // Three independent title sources. `custom-title` is what `/rename` writes (also `claude -n`,
    // though the app no longer uses that flag). `agent-name` mirrors `custom-title` for branched
    // sessions. `ai-title` is Claude's auto-summary, emitted after the first turn — only present
    // when no custom title exists. We resolve precedence at the end so a user-chosen name always
    // beats the AI summary.
    let mut custom_title = String::new();
    let mut agent_name = String::new();
    let mut ai_title = String::new();
    let mut first_human_message = String::new();
    let mut message_count: usize = 0;
    let mut git_branch = String::new();
    let mut claude_version = String::new();
    let mut tool_use_count: usize = 0;
    let mut duration_ms: u64 = 0;
    // Track the latest per-line timestamp on real conversation turns (user/assistant).
    // ISO-8601 strings are lexicographically sortable, so String::max() works.
    let mut last_message_ts: String = String::new();

    // Model + usage — we care about the LAST assistant turn (for the context-used bar).
    // Cost is NOT computed here; we rely entirely on the xshell-stats statusline hook below
    // for authoritative numbers. Synthesizing a price-table estimate would drift every time
    // pricing changes and only ever undercount (no system-prompt / tools tokens in JSONL).
    let mut latest_model = String::new();
    let mut last_input: u64 = 0;
    let mut last_cache_creation: u64 = 0;
    let mut last_cache_read: u64 = 0;
    // Track max observed context across the whole session — used to auto-detect 1M context.
    // If we ever see > 200k, the session must be on the 1M beta (a normal 200k session would
    // have auto-compacted before hitting the limit).
    let mut max_context_observed: u64 = 0;
    // Lifetime per-category token totals across every non-synthetic assistant turn. Powers
    // the Tokens view of the project stats panel without depending on the xshell-stats hook.
    let mut total_input_tokens: u64 = 0;
    let mut total_cache_creation_tokens: u64 = 0;
    let mut total_cache_read_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    // Per-day breakdown keyed by YYYY-MM-DD. Each value is [input, cache_creation, cache_read,
    // output] so the UI can render a stacked area chart (cost-impact ordering at render time).
    let mut daily_tokens: std::collections::BTreeMap<String, [u64; 4]> =
        std::collections::BTreeMap::new();
    // Claude Code splits one assistant API response into multiple JSONL lines — one per content
    // block (text / thinking / tool_use) — but stamps the SAME `usage` block on every line.
    // Summing usage on every line over-counts the same API call N times. Dedup by `message.id`
    // so each real API call contributes its tokens exactly once. Only gates the lifetime totals
    // and per-day buckets — the "latest" / "max" trackers are idempotent under duplicates.
    let mut seen_message_ids: HashSet<String> = HashSet::new();

    for line in reader.lines().flatten() {
        let json: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(b) = json.get("gitBranch").and_then(|v| v.as_str()) {
            git_branch = b.to_string();
        }
        if let Some(v) = json.get("version").and_then(|v| v.as_str()) {
            claude_version = v.to_string();
        }
        if let Some(d) = json.get("durationMs").and_then(|v| v.as_u64()) {
            duration_ms += d;
        }
        if json.get("toolUseResult").is_some() {
            tool_use_count += 1;
        }

        let ty = json.get("type").and_then(|t| t.as_str());
        // Bump the "last activity" timestamp only on user/assistant lines — tool results,
        // permission-mode flips, and file-history snapshots don't represent user activity.
        if matches!(ty, Some("user") | Some("human") | Some("assistant")) {
            if let Some(ts) = json.get("timestamp").and_then(|t| t.as_str()) {
                if ts > last_message_ts.as_str() {
                    last_message_ts = ts.to_string();
                }
            }
        }

        match ty {
            Some("custom-title") => {
                if let Some(t) = json.get("customTitle").and_then(|t| t.as_str()) {
                    custom_title = t.to_string();
                }
            }
            Some("ai-title") => {
                if let Some(t) = json.get("aiTitle").and_then(|t| t.as_str()) {
                    ai_title = t.to_string();
                }
            }
            Some("agent-name") => {
                if let Some(t) = json.get("agentName").and_then(|t| t.as_str()) {
                    agent_name = t.to_string();
                }
            }
            Some("human") | Some("user") => {
                // Both real user prompts AND tool-result responses arrive as `type: "user"`.
                // Tool results are NOT something the user typed — Claude requested a tool,
                // the runtime sent back the result as a `user`-role turn with content like
                // `[{ "type": "tool_result", ... }]`. Counting those as messages overstates
                // the actual conversation length by 2-3×.
                let content_node = json
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .or_else(|| json.get("content"));
                let mut is_real_prompt = false;
                let mut prompt_text: Option<String> = None;
                if let Some(content) = content_node {
                    if let Some(s) = content.as_str() {
                        is_real_prompt = !s.is_empty();
                        prompt_text = Some(s.chars().take(120).collect());
                    } else if let Some(arr) = content.as_array() {
                        // Real prompt = at least one text/image part AND no tool_result parts.
                        let has_tool_result = arr.iter().any(|item| {
                            item.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        });
                        let has_text_or_image = arr.iter().any(|item| {
                            let ty = item.get("type").and_then(|t| t.as_str());
                            ty == Some("text")
                                || ty == Some("image")
                                || ty.is_none() && item.get("text").is_some()
                        });
                        is_real_prompt = !has_tool_result && has_text_or_image;
                        if is_real_prompt {
                            for item in arr {
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    prompt_text = Some(text.chars().take(120).collect());
                                    break;
                                }
                            }
                        }
                    }
                }
                if is_real_prompt {
                    message_count += 1;
                    if first_human_message.is_empty() {
                        if let Some(t) = prompt_text {
                            first_human_message = t;
                        }
                    }
                }
            }
            Some("assistant") => {
                // Pull model + usage from the assistant's `message` object. We price each
                // turn against the model that handled it (sessions can switch models), and
                // remember the last usage for the "current context used" bar.
                if let Some(msg) = json.get("message") {
                    let turn_model = msg
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    // Branch / resume sessions inject `<synthetic>` model entries — those
                    // are placeholder rows that didn't actually run through a real model,
                    // so they shouldn't pollute the latest-model badge or get priced.
                    let is_synthetic = turn_model.starts_with('<') && turn_model.ends_with('>');
                    if !turn_model.is_empty() && !is_synthetic {
                        latest_model = turn_model.clone();
                    }
                    if let Some(u) = msg.get("usage") {
                        if is_synthetic {
                            continue;
                        }
                        let inp = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        let cc = u
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cr = u
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let out = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        last_input = inp;
                        last_cache_creation = cc;
                        last_cache_read = cr;
                        let turn_context = inp + cc + cr;
                        if turn_context > max_context_observed {
                            max_context_observed = turn_context;
                        }
                        // Skip the lifetime/per-day accumulation if we've already seen this
                        // message.id — see `seen_message_ids` declaration above for why.
                        let message_id = msg
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let first_seen = match &message_id {
                            Some(id) => seen_message_ids.insert(id.clone()),
                            None => true,
                        };
                        if first_seen {
                            total_input_tokens += inp;
                            total_cache_creation_tokens += cc;
                            total_cache_read_tokens += cr;
                            total_output_tokens += out;
                            // Per-day bucket. Use this turn's own ISO timestamp (top-level) so
                            // days line up with when usage actually happened, not when the
                            // session ended.
                            if let Some(ts) = json.get("timestamp").and_then(|v| v.as_str()) {
                                if ts.len() >= 10 {
                                    let day = ts[..10].to_string();
                                    let entry = daily_tokens.entry(day).or_insert([0; 4]);
                                    entry[0] = entry[0].saturating_add(inp);
                                    entry[1] = entry[1].saturating_add(cc);
                                    entry[2] = entry[2].saturating_add(cr);
                                    entry[3] = entry[3].saturating_add(out);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut context_tokens = last_input + last_cache_creation + last_cache_read;
    // Auto-detect 1M context: a 200k-limit session would have auto-compacted well before
    // exceeding 200k, so any observed context above that threshold is proof the session is
    // running on the 1M beta (Opus 4.7 (1M context) / similar).
    let mut context_limit: u64 = if max_context_observed > 200_000 {
        1_000_000
    } else {
        200_000
    };
    // Stays at 0 unless the xshell-stats hook overwrites it just below — no JSONL fallback.
    let mut cost_usd: f64 = 0.0;
    let mut model_out = latest_model;
    let mut is_authoritative_stats = false;
    let mut rate_limit_5h_pct: Option<f64> = None;
    let mut rate_limit_7d_pct: Option<f64> = None;
    let mut daily_cost: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();

    // If the user has set up the xshell-stats statusline hook, prefer its values — Claude
    // Code computes cost and context% authoritatively, including system-prompt + tools
    // tokens we can't see in the per-turn `message.usage`. The file is keyed by session id
    // and refreshed every Claude Code refresh tick.
    if let Some(home) = ctx.home.clone() {
        let stats_path = home
            .join(".claude")
            .join("xshell-stats")
            .join(format!("{}.json", session_id));
        if let Ok(content) = fs::read_to_string(&stats_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                is_authoritative_stats = true;
                if let Some(c) = json
                    .get("cost")
                    .and_then(|v| v.get("total_cost_usd"))
                    .and_then(|v| v.as_f64())
                {
                    cost_usd = c;
                }
                if let Some(cw) = json.get("context_window") {
                    if let Some(size) = cw.get("context_window_size").and_then(|v| v.as_u64()) {
                        context_limit = size;
                    }
                    // Prefer the explicit current_usage breakdown for context_tokens — same
                    // shape we used from JSONL but now sourced from Claude Code itself.
                    if let Some(cu) = cw.get("current_usage") {
                        let inp = cu.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        let cc = cu
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cr = cu
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        // If used_percentage is present, use it to back-compute tokens that
                        // include system+tools overhead (the source of our current ~25k drift).
                        if let Some(pct) = cw.get("used_percentage").and_then(|v| v.as_f64()) {
                            context_tokens = ((pct / 100.0) * context_limit as f64) as u64;
                        } else {
                            context_tokens = inp + cc + cr;
                        }
                    }
                }
                // Model display name carries the "[1m]" / "(1M context)" markers Claude
                // Code adds at runtime — strictly better than the raw model id we'd parse
                // from JSONL.
                if let Some(m) = json.get("model") {
                    if let Some(disp) = m.get("display_name").and_then(|v| v.as_str()) {
                        model_out = disp.to_string();
                    } else if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                        model_out = id.to_string();
                    }
                }
                if let Some(rl) = json.get("rate_limits") {
                    rate_limit_5h_pct = rl
                        .get("five_hour")
                        .and_then(|v| v.get("used_percentage"))
                        .and_then(|v| v.as_f64());
                    rate_limit_7d_pct = rl
                        .get("seven_day")
                        .and_then(|v| v.get("used_percentage"))
                        .and_then(|v| v.as_f64());
                }
                // Per-day breakdown the hook accumulates. BTreeMap so the UI gets dates in
                // chronological order without sorting on the JS side.
                if let Some(d) = json.get("xshell_daily_cost").and_then(|v| v.as_object()) {
                    for (k, v) in d {
                        if let Some(n) = v.as_f64() {
                            daily_cost.insert(k.clone(), n);
                        }
                    }
                }
            }
        }
    }

    // Title precedence: user-chosen names (custom-title from /rename, agent-name from /branch)
    // beat Claude's auto-summary, which beats the first prompt, which beats the bare session id.
    let display_title = if !custom_title.is_empty() {
        custom_title
    } else if !agent_name.is_empty() {
        agent_name
    } else if !ai_title.is_empty() {
        ai_title
    } else if !first_human_message.is_empty() {
        first_human_message
    } else {
        format!("Session {}", &session_id[..8.min(session_id.len())])
    };
    // Prefer the real last-message timestamp; fall back to file mtime for brand-new sessions
    // that haven't produced a user/assistant line yet.
    let timestamp = if last_message_ts.is_empty() {
        mtime_iso
    } else {
        last_message_ts
    };

    let info = SessionInfo {
        id: session_id,
        title: display_title,
        timestamp,
        message_count,
        project_name: project_name.to_string(),
        project_path: project_path.to_string(),
        git_branch,
        claude_version,
        tool_use_count,
        duration_ms,
        model: model_out,
        context_tokens,
        context_limit,
        cost_usd,
        is_authoritative_stats,
        daily_cost,
        rate_limit_5h_pct,
        rate_limit_7d_pct,
        total_input_tokens,
        total_cache_creation_tokens,
        total_cache_read_tokens,
        total_output_tokens,
        daily_tokens,
        agent: "claude".into(),
    };
    if let Ok(mut cache) = session_cache().lock() {
        cache.insert(
            path.to_path_buf(),
            SessionCacheEntry {
                jsonl_mtime: modified,
                stats_mtime,
                project_path: project_path.to_string(),
                info: info.clone(),
            },
        );
    }
    Some(info)
}

pub fn list_claude_projects(ctx: &HostCtx) -> Vec<ProjectInfo> {
    let projects_dir = match ctx.claude_projects_dir() {
        Some(d) if d.exists() => d,
        _ => return vec![],
    };

    let mut projects: Vec<ProjectInfo> = vec![];

    for entry in fs::read_dir(&projects_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }

        let encoded_name = entry.file_name().to_string_lossy().to_string();
        let project_dir = entry.path();

        // Count JSONL files and find cwd from the first one
        let mut session_count = 0usize;
        let mut cwd = String::new();
        let mut latest_modified: Option<SystemTime> = None;

        for jsonl_entry in fs::read_dir(&project_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = jsonl_entry.path();
            if p.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            session_count += 1;

            if let Ok(meta) = fs::metadata(&p) {
                if let Ok(modified) = meta.modified() {
                    if latest_modified.is_none_or(|prev| modified > prev) {
                        latest_modified = Some(modified);
                    }
                }
            }

            if cwd.is_empty() {
                if let Ok(file) = fs::File::open(&p) {
                    for line in BufReader::new(file).lines().take(30).flatten() {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                            if let Some(c) = json.get("cwd").and_then(|c| c.as_str()) {
                                cwd = c.to_string();
                                break;
                            }
                        }
                    }
                }
            }
        }

        if session_count == 0 {
            continue;
        }
        if cwd.is_empty() {
            continue;
        }

        let name = std::path::Path::new(&cwd)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| encoded_name.clone());
        let last_active = latest_modified.map(system_time_to_iso).unwrap_or_default();

        projects.push(ProjectInfo {
            name,
            path: cwd,
            encoded_name,
            session_count,
            last_active,
        });
    }

    projects.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    projects
}

// `BufRead::lines` yields `Err` for a line that is not valid UTF-8 and then keeps going, so
// `flatten` skips just that line. clippy's suggestion (`map_while(Result::ok)`) would stop
// parsing at the first such line, which changes behaviour.
#[allow(clippy::lines_filter_map_ok)]
pub fn get_session_messages(
    ctx: &HostCtx,
    encoded_name: String,
    session_id: String,
    limit: usize,
) -> Vec<MessagePreview> {
    let projects_dir = match ctx.claude_projects_dir() {
        Some(d) => d,
        None => return vec![],
    };
    let path = projects_dir
        .join(&encoded_name)
        .join(format!("{}.jsonl", session_id));
    if !path.exists() {
        return vec![];
    }
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let reader = BufReader::new(file);
    let mut messages: Vec<MessagePreview> = vec![];
    for line in reader.lines().flatten() {
        let json: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if msg_type != "user" && msg_type != "assistant" {
            continue;
        }
        let msg = match json.get("message") {
            Some(m) => m,
            None => continue,
        };
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let content = msg.get("content");
        let text = if let Some(s) = content.and_then(|c| c.as_str()) {
            s.chars().take(200).collect()
        } else if let Some(arr) = content.and_then(|c| c.as_array()) {
            arr.iter()
                .filter_map(|item| {
                    if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                        item.get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.chars().take(200).collect::<String>())
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or_default()
        } else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        messages.push(MessagePreview { role, text });
    }
    // Return the last N messages
    let start = if messages.len() > limit {
        messages.len() - limit
    } else {
        0
    };
    messages[start..].to_vec()
}

// ── Branch detection ──────────────────────────────────────────────────
//
// When the user types `/branch` inside a running claude session, claude creates a new
// JSONL that clones the parent's message history — keeping the same message UUIDs. That
// UUID overlap is our structural fingerprint: no unrelated file (manual or otherwise) can
// reproduce 128-bit random UUIDs by accident, so high overlap == this is a real branch.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BranchInfo {
    pub new_session_id: String,
    pub title: String,
}

// Read the first few lines of a jsonl and extract the `forkedFrom.sessionId` field if
// present. Claude writes this on every line of a session that was created via `/branch`,
// so the very first line is enough. Returns None for non-branched sessions.
pub fn read_forked_from(path: &std::path::Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(5).flatten() {
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(sid) = v
            .get("forkedFrom")
            .and_then(|f| f.get("sessionId"))
            .and_then(|s| s.as_str())
        {
            return Some(sid.to_string());
        }
    }
    None
}

pub fn list_project_session_ids(ctx: &HostCtx, cwd: String) -> Vec<String> {
    let projects_dir = match ctx.claude_projects_dir() {
        Some(d) => d,
        None => return vec![],
    };
    let project_dir = projects_dir.join(encode_project_name(&cwd));
    if !project_dir.exists() {
        return vec![];
    }
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&project_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out
}

pub fn detect_session_branch(
    ctx: &HostCtx,
    cwd: String,
    current_session_id: String,
    known_session_ids: Vec<String>,
) -> Option<BranchInfo> {
    // Project dir derivation mirrors how Claude Code encodes paths (slashes/backslashes/colons → dashes).
    let projects_dir = ctx.claude_projects_dir()?;
    let encoded = encode_project_name(&cwd);
    let project_dir = projects_dir.join(&encoded);
    if !project_dir.exists() {
        return None;
    }

    // `known_session_ids` = snapshot of sibling jsonls that existed at tab startup. Anything
    // NOT in this set is a freshly-created file — the only kind we consider. This also rules
    // out false positives when the user resumes an ancestor session in another tab.
    let known: std::collections::HashSet<String> = known_session_ids.into_iter().collect();

    // Scan new sibling .jsonl files. For each, read the first few lines and check the
    // `forkedFrom.sessionId` field — Claude writes this on every line of a branched session.
    // If it matches our current session id, it's definitively our fork. No heuristics needed.
    for entry in fs::read_dir(&project_dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem == current_session_id {
            continue;
        }
        if known.contains(&stem) {
            continue;
        }
        match read_forked_from(&p) {
            Some(parent_id) if parent_id == current_session_id => {
                let project_name = std::path::Path::new(&cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let title = parse_session(ctx, &p, &project_name, &cwd)
                    .map(|s| s.title)
                    .unwrap_or_else(|| format!("Branch {}", &stem[..8.min(stem.len())]));
                return Some(BranchInfo {
                    new_session_id: stem,
                    title,
                });
            }
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_project_name_replaces_every_non_alphanumeric() {
        assert_eq!(
            encode_project_name(r"C:\Users\alex\my-app"),
            "C--Users-alex-my-app"
        );
        assert_eq!(
            encode_project_name("/home/u/CalcApps.Framework"),
            "-home-u-CalcApps-Framework"
        );
        assert_eq!(encode_project_name("SSY2_Lab"), "SSY2-Lab");
    }

    use crate::testutil::Fixture;
    use serde_json::{json, Value};
    use std::time::Duration;

    const CWD: &str = "/work/alpha";

    fn session_path(encoded: &str, sid: &str) -> String {
        format!("home/.claude/projects/{encoded}/{sid}.jsonl")
    }

    // Write a Claude session JSONL for `CWD` and parse it.
    fn parse(fx: &Fixture, sid: &str, lines: &[Value]) -> SessionInfo {
        let p = fx.write_jsonl(session_path(&encode_project_name(CWD), sid), lines);
        parse_session(&fx.ctx(), &p, "alpha", CWD).expect("parsed")
    }

    fn user(text: &str) -> Value {
        json!({"type": "user", "cwd": CWD, "timestamp": "2026-01-02T10:00:00Z",
               "message": {"role": "user", "content": text}})
    }

    fn assistant(id: &str, model: &str, ts: &str, usage: Value) -> Value {
        json!({"type": "assistant", "timestamp": ts,
               "message": {"id": id, "role": "assistant", "model": model, "usage": usage,
                           "content": [{"type": "text", "text": "ok"}]}})
    }

    fn usage(inp: u64, cc: u64, cr: u64, out: u64) -> Value {
        json!({"input_tokens": inp, "cache_creation_input_tokens": cc,
               "cache_read_input_tokens": cr, "output_tokens": out})
    }

    #[test]
    fn parse_session_title_precedence() {
        let fx = Fixture::new();
        let custom = json!({"type": "custom-title", "customTitle": "Custom"});
        let agent = json!({"type": "agent-name", "agentName": "Agent"});
        let ai = json!({"type": "ai-title", "aiTitle": "AI"});
        let prompt = user("first prompt");
        let all = [prompt.clone(), ai.clone(), agent.clone(), custom];
        assert_eq!(parse(&fx, "s1-aaaaaaaa", &all).title, "Custom");
        let no_custom = [prompt.clone(), ai.clone(), agent];
        assert_eq!(parse(&fx, "s2-aaaaaaaa", &no_custom).title, "Agent");
        assert_eq!(parse(&fx, "s3-aaaaaaaa", &[prompt.clone(), ai]).title, "AI");
        assert_eq!(parse(&fx, "s4-aaaaaaaa", &[prompt]).title, "first prompt");
        let bare = [json!({"type": "permission-mode", "cwd": CWD})];
        assert_eq!(parse(&fx, "abcdef123456", &bare).title, "Session abcdef12");
    }

    #[test]
    fn parse_session_counts_only_real_prompts() {
        let fx = Fixture::new();
        let long = "x".repeat(200);
        let lines = [
            user(&long),
            json!({"type": "user", "message": {"role": "user",
                   "content": [{"type": "text", "text": "text part"}]}}),
            json!({"type": "user", "toolUseResult": {}, "message": {"role": "user",
                   "content": [{"type": "tool_result", "content": "out"}]}}),
            user(""),
        ];
        let s = parse(&fx, "count-session", &lines);
        assert_eq!(s.message_count, 2);
        assert_eq!(s.tool_use_count, 1);
        assert_eq!(s.title, "x".repeat(120));
    }

    #[test]
    fn parse_session_dedups_usage_by_message_id() {
        let fx = Fixture::new();
        let ts = "2026-01-02T10:00:00Z";
        let lines = [
            assistant("msg_1", "claude-x", ts, usage(10, 20, 30, 5)),
            // Same API response split into a second content-block line: same id, same usage.
            assistant("msg_1", "claude-x", ts, usage(10, 20, 30, 5)),
            assistant(
                "msg_2",
                "claude-x",
                "2026-01-03T10:00:00Z",
                usage(1, 2, 3, 4),
            ),
        ];
        let s = parse(&fx, "dedup-session", &lines);
        assert_eq!(
            (
                s.total_input_tokens,
                s.total_cache_creation_tokens,
                s.total_cache_read_tokens,
                s.total_output_tokens
            ),
            (11, 22, 33, 9)
        );
        assert_eq!(s.daily_tokens["2026-01-02"], [10, 20, 30, 5]);
        assert_eq!(s.daily_tokens["2026-01-03"], [1, 2, 3, 4]);
        // Context is the last turn's input side.
        assert_eq!(s.context_tokens, 6);
        assert_eq!(s.context_limit, 200_000);
        assert_eq!(s.timestamp, "2026-01-03T10:00:00Z");
    }

    #[test]
    fn parse_session_ignores_synthetic_model() {
        let fx = Fixture::new();
        let lines = [
            assistant(
                "msg_1",
                "claude-real",
                "2026-01-02T10:00:00Z",
                usage(1, 0, 0, 1),
            ),
            assistant(
                "msg_2",
                "<synthetic>",
                "2026-01-02T11:00:00Z",
                usage(500, 0, 0, 500),
            ),
        ];
        let s = parse(&fx, "synthetic-session", &lines);
        assert_eq!(s.model, "claude-real");
        assert_eq!((s.total_input_tokens, s.total_output_tokens), (1, 1));
        assert_eq!(s.context_tokens, 1);
    }

    #[test]
    fn parse_session_detects_1m_context() {
        let fx = Fixture::new();
        let lines = [
            assistant(
                "m1",
                "claude-x",
                "2026-01-02T10:00:00Z",
                usage(150_000, 0, 60_000, 1),
            ),
            assistant(
                "m2",
                "claude-x",
                "2026-01-02T11:00:00Z",
                usage(1_000, 0, 9_000, 1),
            ),
        ];
        let s = parse(&fx, "big-session", &lines);
        assert_eq!(s.context_limit, 1_000_000);
        assert_eq!(s.context_tokens, 10_000);
    }

    fn sidecar() -> Value {
        json!({
            "cost": {"total_cost_usd": 1.5},
            "context_window": {"context_window_size": 1_000_000, "used_percentage": 10.0,
                               "current_usage": {"input_tokens": 1}},
            "model": {"id": "claude-opus-x", "display_name": "Opus X"},
            "rate_limits": {"five_hour": {"used_percentage": 12.0},
                            "seven_day": {"used_percentage": 34.0}},
            "xshell_daily_cost": {"2026-01-02": 0.5}
        })
    }

    #[test]
    fn parse_session_overlays_xshell_stats() {
        let fx = Fixture::new();
        fx.write(
            "home/.claude/xshell-stats/stats-session.json",
            sidecar().to_string(),
        );
        let lines = [assistant(
            "m1",
            "claude-x",
            "2026-01-02T10:00:00Z",
            usage(5, 0, 0, 1),
        )];
        let s = parse(&fx, "stats-session", &lines);
        assert!(s.is_authoritative_stats);
        assert_eq!(s.cost_usd, 1.5);
        assert_eq!(s.context_limit, 1_000_000);
        assert_eq!(s.context_tokens, 100_000);
        assert_eq!(s.model, "Opus X");
        assert_eq!(s.rate_limit_5h_pct, Some(12.0));
        assert_eq!(s.rate_limit_7d_pct, Some(34.0));
        assert_eq!(s.daily_cost.get("2026-01-02"), Some(&0.5));
    }

    #[test]
    fn parse_session_cache_refreshes_when_stats_sidecar_appears() {
        let fx = Fixture::new();
        let lines = [assistant(
            "m1",
            "claude-x",
            "2026-01-02T10:00:00Z",
            usage(5, 0, 0, 1),
        )];
        let first = parse(&fx, "late-stats", &lines);
        assert!(!first.is_authoritative_stats);
        assert_eq!(first.cost_usd, 0.0);
        // The JSONL is unchanged; only the sidecar appears, which changes the cache key.
        fx.write(
            "home/.claude/xshell-stats/late-stats.json",
            sidecar().to_string(),
        );
        let p = fx
            .home()
            .join(".claude/projects/-work-alpha/late-stats.jsonl");
        let second = parse_session(&fx.ctx(), &p, "alpha", CWD).unwrap();
        assert!(second.is_authoritative_stats);
        assert_eq!(second.cost_usd, 1.5);
    }

    #[test]
    fn list_claude_projects_reads_cwd_counts_and_sorts() {
        let fx = Fixture::new();
        let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let alpha = |sid: &str| {
            fx.write_jsonl(
                session_path("-work-alpha", sid),
                &[
                    json!({"type": "permission-mode"}),
                    json!({"cwd": "/work/alpha"}),
                ],
            )
        };
        let a1 = alpha("a1");
        let a2 = alpha("a2");
        let b1 = fx.write_jsonl(
            session_path("-work-beta", "b1"),
            &[json!({"cwd": "/work/beta"})],
        );
        fx.set_mtime(&a1, at(1_700_000_000));
        fx.set_mtime(&a2, at(1_700_000_100));
        fx.set_mtime(&b1, at(1_800_000_000));
        // Skipped: a project dir whose JSONL has no cwd, and one with no JSONL at all.
        fx.write_jsonl(session_path("-no-cwd", "x"), &[json!({"type": "user"})]);
        fx.write("home/.claude/projects/-no-jsonl/notes.txt", "x");

        let projects = list_claude_projects(&fx.ctx());
        let got: Vec<(&str, &str, &str, usize)> = projects
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    p.path.as_str(),
                    p.encoded_name.as_str(),
                    p.session_count,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("beta", "/work/beta", "-work-beta", 1),
                ("alpha", "/work/alpha", "-work-alpha", 2),
            ]
        );
        assert_eq!(
            projects[0].last_active,
            system_time_to_iso(at(1_800_000_000))
        );
        assert_eq!(
            projects[1].last_active,
            system_time_to_iso(at(1_700_000_100))
        );
    }

    #[test]
    fn list_claude_projects_is_empty_without_home() {
        let fx = Fixture::new();
        fx.write_jsonl(session_path("-work-alpha", "a1"), &[json!({"cwd": CWD})]);
        let ctx = HostCtx {
            home: None,
            ..fx.ctx()
        };
        assert!(list_claude_projects(&ctx).is_empty());
        assert_eq!(list_claude_projects(&fx.ctx()).len(), 1);
    }

    #[test]
    fn get_session_messages_returns_last_n_text_messages() {
        let fx = Fixture::new();
        let long = "y".repeat(300);
        fx.write_jsonl(
            session_path("-work-alpha", "msgs"),
            &[
                user("one"),
                json!({"type": "assistant", "message": {"role": "assistant",
                       "content": [{"type": "text", "text": "two"}]}}),
                // No text part: skipped.
                json!({"type": "assistant", "message": {"role": "assistant",
                       "content": [{"type": "tool_use", "name": "Bash"}]}}),
                json!({"type": "summary", "message": {"role": "user", "content": "ignored"}}),
                user(&long),
            ],
        );
        let ctx = fx.ctx();
        let got = get_session_messages(&ctx, "-work-alpha".into(), "msgs".into(), 2);
        let got: Vec<(&str, String)> = got
            .iter()
            .map(|m| (m.role.as_str(), m.text.clone()))
            .collect();
        assert_eq!(
            got,
            vec![("assistant", "two".to_string()), ("user", "y".repeat(200))]
        );
        let all = get_session_messages(&ctx, "-work-alpha".into(), "msgs".into(), 10);
        assert_eq!(all.len(), 3);
        assert!(get_session_messages(&ctx, "-work-alpha".into(), "missing".into(), 10).is_empty());
    }

    #[test]
    fn list_project_session_ids_lists_jsonl_stems() {
        let fx = Fixture::new();
        fx.write_jsonl(session_path("-work-alpha", "s-b"), &[]);
        fx.write_jsonl(session_path("-work-alpha", "s-a"), &[]);
        fx.write("home/.claude/projects/-work-alpha/notes.txt", "x");
        let mut ids = list_project_session_ids(&fx.ctx(), CWD.into());
        ids.sort();
        assert_eq!(ids, vec!["s-a", "s-b"]);
        assert!(list_project_session_ids(&fx.ctx(), "/work/none".into()).is_empty());
    }

    fn fork_of(parent: &str) -> Value {
        json!({"type": "user", "forkedFrom": {"sessionId": parent}, "cwd": CWD,
               "timestamp": "2026-01-02T10:00:00Z",
               "message": {"role": "user", "content": "branched prompt"}})
    }

    #[test]
    fn detect_session_branch_finds_new_fork() {
        let fx = Fixture::new();
        fx.write_jsonl(session_path("-work-alpha", "parent-01"), &[user("hi")]);
        fx.write_jsonl(
            session_path("-work-alpha", "child-001"),
            &[
                fork_of("parent-01"),
                json!({"type": "custom-title", "customTitle": "My branch"}),
            ],
        );
        let got = detect_session_branch(
            &fx.ctx(),
            CWD.into(),
            "parent-01".into(),
            vec!["parent-01".into()],
        )
        .expect("branch found");
        assert_eq!(got.new_session_id, "child-001");
        assert_eq!(got.title, "My branch");
    }

    #[test]
    fn detect_session_branch_ignores_known_and_unrelated() {
        let fx = Fixture::new();
        fx.write_jsonl(session_path("-work-alpha", "parent-01"), &[user("hi")]);
        // A fork of ours that already existed when the tab started.
        fx.write_jsonl(
            session_path("-work-alpha", "known-001"),
            &[fork_of("parent-01")],
        );
        // A new fork, but of a different session.
        fx.write_jsonl(
            session_path("-work-alpha", "other-001"),
            &[fork_of("someone-else")],
        );
        // A new session that is not a fork at all.
        fx.write_jsonl(session_path("-work-alpha", "plain-001"), &[user("x")]);
        let got = detect_session_branch(
            &fx.ctx(),
            CWD.into(),
            "parent-01".into(),
            vec!["parent-01".into(), "known-001".into()],
        );
        assert!(got.is_none());
    }
}

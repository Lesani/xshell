use crate::agent_context::{AgentContextItem, AgentContextSection};
use crate::ctx::HostCtx;
use crate::paths::find_git_root;
use crate::sessions::{CodexProjectInfo, SessionInfo};
use crate::time::system_time_to_iso;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;
use xshell_protocol::msg::{LastLine, Speaker};

// ── Codex session parsing ─────────────────────────────────────────────
// Codex rollouts (~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl) parse into the same
// SessionInfo shape Claude sessions use. Mapping notes: title = first user message (Codex
// has no /rename or auto-summary), model from the latest turn_context, context usage from
// the latest token_count's last_token_usage (the final turn's prompt+completion ≈ current
// conversation size), claude_version carries Codex's cli_version. Cost stays 0 — Codex
// subscription plans have no per-use cost — while is_authoritative_stats is true so the
// context bar renders: the numbers come from Codex itself, not an estimate.

pub fn codex_rollout_files(ctx: &HostCtx) -> Vec<std::path::PathBuf> {
    rollout_files_within(ctx, usize::MAX).unwrap_or_default()
}

/// [`codex_rollout_files`], visiting at most `max_entries` directory entries: `None` past
/// that.
fn rollout_files_within(ctx: &HostCtx, max_entries: usize) -> Option<Vec<PathBuf>> {
    let Some(home) = ctx.home.clone() else {
        return Some(vec![]);
    };
    let mut files = vec![];
    let mut seen = 0usize;
    let mut stack = vec![home.join(".codex").join("sessions")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
            seen += 1;
            if seen > max_entries {
                return None;
            }
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
    Some(files)
}

/// The rollout of Codex session `sid`: the file under `~/.codex/sessions` whose name ends
/// in `-{sid}.jsonl`. The newest by path (the date directories sort by date) when several do.
pub fn rollout_for(ctx: &HostCtx, sid: &str) -> Option<PathBuf> {
    rollout_for_within(ctx, sid, usize::MAX)
}

/// [`rollout_for`], giving up (`None`) after visiting `max_entries` directory entries.
pub fn rollout_for_within(ctx: &HostCtx, sid: &str, max_entries: usize) -> Option<PathBuf> {
    if sid.is_empty() {
        return None;
    }
    let suffix = format!("-{sid}.jsonl");
    rollout_files_within(ctx, max_entries)?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
        })
        .max()
}

/// What a rollout's first line (`session_meta`) says about its session. `id` may be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RolloutMeta {
    pub id: String,
    pub cwd: String,
}

/// A rollout file under `~/.codex/sessions`, from [`rollouts`].
#[derive(Clone, Debug)]
pub struct Rollout {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub meta: RolloutMeta,
}

/// The most of a rollout's first line read for its [`RolloutMeta`].
const META_LINE_MAX: u64 = 1 << 20;
/// The most rollouts whose [`RolloutMeta`] is cached; past that an entry is evicted.
const META_CACHE_MAX: usize = 50_000;

type MetaCache = HashMap<PathBuf, (SystemTime, u64, Option<RolloutMeta>)>;

/// [`RolloutMeta`] by rollout path, valid while the file's mtime and length are unchanged.
fn meta_cache() -> &'static Mutex<MetaCache> {
    static CACHE: OnceLock<Mutex<MetaCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Insert into a bounded cache, evicting an arbitrary other entry when it is full.
pub(crate) fn bounded_insert<V>(
    cache: &mut HashMap<PathBuf, V>,
    max: usize,
    key: PathBuf,
    value: V,
) {
    if cache.len() >= max && !cache.contains_key(&key) {
        if let Some(k) = cache.keys().next().cloned() {
            cache.remove(&k);
        }
    }
    cache.insert(key, value);
}

/// Every rollout under `~/.codex/sessions` that is a regular file (symlinks, and symlinked
/// directories, are skipped) and whose first line names a cwd. The first
/// lines are cached by path, mtime and length: a repeat call costs one `stat` per rollout.
pub fn rollouts(ctx: &HostCtx) -> Vec<Rollout> {
    let Some(home) = ctx.home.clone() else {
        return vec![];
    };
    let mut found = vec![];
    let mut stack = vec![home.join(".codex").join("sessions")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let p = entry.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && p.extension().is_some_and(|ext| ext == "jsonl") {
                // Not followed: `entry.metadata` describes the entry itself.
                let Some((modified, len)) = entry
                    .metadata()
                    .ok()
                    .and_then(|m| Some((m.modified().ok()?, m.len())))
                else {
                    continue;
                };
                if let Some(meta) = rollout_meta(&p, modified, len) {
                    found.push(Rollout {
                        path: p,
                        modified,
                        meta,
                    });
                }
            }
        }
    }
    found
}

/// The [`RolloutMeta`] of the rollout at `path`, which had `modified` and `len` when listed.
fn rollout_meta(path: &Path, modified: SystemTime, len: u64) -> Option<RolloutMeta> {
    if let Ok(cache) = meta_cache().lock() {
        if let Some((m, l, meta)) = cache.get(path) {
            if *m == modified && *l == len {
                return meta.clone();
            }
        }
    }
    let meta = read_rollout_meta(path);
    if let Ok(mut cache) = meta_cache().lock() {
        bounded_insert(
            &mut cache,
            META_CACHE_MAX,
            path.to_path_buf(),
            (modified, len, meta.clone()),
        );
    }
    meta
}

fn read_rollout_meta(path: &Path) -> Option<RolloutMeta> {
    use std::io::Read;
    let f = crate::last_line::open_no_follow(path)?;
    let mut first = Vec::new();
    BufReader::new(f.take(META_LINE_MAX))
        .read_until(b'\n', &mut first)
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&first).ok()?;
    let payload = json.get("payload")?;
    let s = |k: &str| payload.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let cwd = s("cwd").filter(|c| !c.is_empty())?;
    Some(RolloutMeta {
        id: s("id").unwrap_or_default(),
        cwd,
    })
}

/// What a rollout's first line (`session_meta`) says about the session, for linking a
/// Terminal to it: see [`session_meta_in`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexSessionMeta {
    pub id: String,
    pub cwd: String,
    /// The session is a subagent's thread (its `source` is an object, or names a subagent),
    /// never the one a Terminal runs.
    pub subagent: bool,
}

/// `~/.codex/sessions`, where Codex keeps its rollouts.
pub fn sessions_root(ctx: &HostCtx) -> Option<PathBuf> {
    Some(ctx.home()?.join(".codex").join("sessions"))
}

/// The [`CodexSessionMeta`] of the rollout at `path`, opened only if it is a regular file
/// whose real path stays inside `root` (see `last_line::open_confined`). Reads only the
/// first line, at most 1 MiB of it (the meta line can hold long instructions): `None` when
/// that is not a complete `session_meta` line naming an id and a cwd.
pub fn session_meta_in(root: &Path, path: &Path) -> Option<CodexSessionMeta> {
    use std::io::Read;
    let f = crate::last_line::open_confined(root, path)?;
    let mut first = Vec::new();
    BufReader::new(f.take(META_LINE_MAX))
        .read_until(b'\n', &mut first)
        .ok()?;
    if first.last() != Some(&b'\n') {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&first).ok()?;
    if json.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    let payload = json.get("payload")?;
    let s = |k: &str| payload.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let subagent = match payload.get("source") {
        Some(serde_json::Value::String(src)) => src.to_ascii_lowercase().contains("subagent"),
        Some(serde_json::Value::Null) | None => false,
        Some(_) => true,
    };
    Some(CodexSessionMeta {
        id: s("id").filter(|i| !i.is_empty())?,
        cwd: s("cwd").filter(|c| !c.is_empty())?,
        subagent,
    })
}

/// The newest user or agent message in the tail of a Codex rollout: the `message` of an
/// `event_msg` of type `user_message` or `agent_message`.
pub fn last_line_in(path: &Path) -> Option<LastLine> {
    last_line_in_window(path, crate::last_line::TAIL_WINDOW)
}

pub(crate) fn last_line_in_window(path: &Path, window: u64) -> Option<LastLine> {
    last_line_from(&mut crate::last_line::open_no_follow(path)?, window)
}

/// [`last_line_in`] for a file already opened (and checked).
pub(crate) fn last_line_from(f: &mut std::fs::File, window: u64) -> Option<LastLine> {
    crate::last_line::newest_in_tail(f, window, |json| {
        event_message(json).map(|(from, text)| (from, text.to_string()))
    })
}

/// The speaker and text of a rollout line that is a user or agent message: an `event_msg`
/// of type `user_message` or `agent_message` (the `response_item` messages repeat them).
pub(crate) fn event_message(json: &serde_json::Value) -> Option<(Speaker, &str)> {
    if json.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = json.get("payload")?;
    let from = match payload.get("type").and_then(|v| v.as_str()) {
        Some("user_message") => Speaker::User,
        Some("agent_message") => Speaker::Agent,
        _ => return None,
    };
    let text = payload.get("message").and_then(|v| v.as_str())?;
    Some((from, text))
}

// User-assigned session names (Codex's rename feature) don't live in the rollout files —
// they land in ~/.codex/session_index.jsonl, one JSON line per named session. Load once
// per listing call and overlay onto parsed sessions; for repeated renames of the same id
// the last line wins (insertion order preserves that).
pub fn codex_session_names(ctx: &HostCtx) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(home) = ctx.home.clone() else {
        return names;
    };
    let Ok(content) = fs::read_to_string(home.join(".codex").join("session_index.jsonl")) else {
        return names;
    };
    for line in content.lines() {
        if let Some((id, name)) = index_name(line.as_bytes()) {
            names.insert(id, name);
        }
    }
    names
}

/// The session id and non-blank name of one `session_index.jsonl` line.
fn index_name(line: &[u8]) -> Option<(String, String)> {
    let json: serde_json::Value = serde_json::from_slice(line).ok()?;
    let id = json.get("id")?.as_str()?;
    let name = json.get("thread_name")?.as_str()?;
    (!name.trim().is_empty()).then(|| (id.to_string(), name.to_string()))
}

/// The most of `session_index.jsonl` read for [`session_names_for`]: its end, since renames
/// are appended and the last one wins.
pub(crate) const INDEX_TAIL_BYTES: u64 = 4 << 20;
/// The most names kept from that window.
const INDEX_NAMES_MAX: usize = 50_000;

type IndexCache = Option<(PathBuf, SystemTime, u64, Arc<HashMap<String, String>>)>;

fn index_cache() -> &'static Mutex<IndexCache> {
    static CACHE: OnceLock<Mutex<IndexCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The Codex names (renames) of the sessions `ids`, for a listing that must stay bounded:
/// `~/.codex/session_index.jsonl` is opened only as a regular file (never through a symlink,
/// never a FIFO), only its last [`INDEX_TAIL_BYTES`] are read, and what it holds is cached by
/// mtime and length, so a rename that lands only in the index shows on the next call. Any
/// failure reads as no renames.
pub fn session_names_for(ctx: &HostCtx, ids: &[&str]) -> HashMap<String, String> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let Some(all) = ctx
        .home
        .as_ref()
        .and_then(|h| index_names(&h.join(".codex").join("session_index.jsonl")))
    else {
        return HashMap::new();
    };
    ids.iter()
        .filter_map(|id| Some((id.to_string(), all.get(*id)?.clone())))
        .collect()
}

fn index_names(path: &Path) -> Option<Arc<HashMap<String, String>>> {
    let mut f = crate::last_line::open_no_follow(path)?;
    let m = f.metadata().ok()?;
    let (modified, len) = (m.modified().ok()?, m.len());
    if let Ok(cache) = index_cache().lock() {
        if let Some((p, t, l, names)) = cache.as_ref() {
            if p == path && *t == modified && *l == len {
                return Some(names.clone());
            }
        }
    }
    let tail = read_tail(&mut f, len, INDEX_TAIL_BYTES).ok()?;
    let mut names = HashMap::new();
    for line in tail.split(|b| *b == b'\n') {
        if let Some((id, name)) = index_name(line) {
            if names.len() < INDEX_NAMES_MAX || names.contains_key(&id) {
                names.insert(id, name);
            }
        }
    }
    let names = Arc::new(names);
    if let Ok(mut cache) = index_cache().lock() {
        *cache = Some((path.to_path_buf(), modified, len, names.clone()));
    }
    Some(names)
}

/// The whole lines in the last `max` bytes of a file `len` bytes long.
pub(crate) fn read_tail(f: &mut fs::File, len: u64, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let start = len.saturating_sub(max);
    let mut buf = Vec::new();
    f.seek(SeekFrom::Start(start))?;
    f.take(max).read_to_end(&mut buf)?;
    if start > 0 {
        // Started inside a line, or just after one: drop up to the first newline.
        let cut = buf
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buf.len(), |i| i + 1);
        buf.drain(..cut);
    }
    Ok(buf)
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

pub fn get_codex_context(ctx: &HostCtx, project_path: String) -> CodexContext {
    let home = ctx.home.clone();
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

pub fn get_codex_usage(ctx: &HostCtx) -> CodexUsage {
    let mut out = CodexUsage {
        present: false,
        primary: None,
        secondary: None,
        plan_type: None,
        rate_limits_updated_iso: None,
        daily_sessions: vec![],
    };
    let Some(home) = ctx.home.clone() else {
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

pub fn list_codex_projects(ctx: &HostCtx) -> Vec<CodexProjectInfo> {
    let Some(home) = ctx.home.clone() else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{path_str, sections_view, Fixture};
    use std::time::Duration;

    const ROLLOUT: &str = include_str!("../tests/fixtures/codex/rollout.jsonl");

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn meta_line(id: &str, cwd: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-01-01T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": id, "cwd": cwd},
        })
        .to_string()
    }

    #[test]
    fn parse_codex_session_maps_meta_model_and_tokens() {
        let f = Fixture::new();
        let p = f.write("home/.codex/sessions/2026/01/02/rollout-a.jsonl", ROLLOUT);
        let s = parse_codex_session(&p, &HashMap::new()).expect("session");
        assert_eq!(s.id, "0199aaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(s.project_path, "/work/proj");
        assert_eq!(s.project_name, "proj");
        assert_eq!(s.git_branch, "main");
        assert_eq!(s.claude_version, "0.50.0");
        // The latest turn_context wins.
        assert_eq!(s.model, "gpt-5.1-codex");
        // First prompt, trimmed, newlines flattened.
        assert_eq!(s.title, "fix the bug");
        assert_eq!(s.message_count, 2);
        assert_eq!(s.timestamp, "2026-01-02T09:00:02Z");
        // Context = last turn's input + output; limit from the latest token_count.
        assert_eq!(s.context_tokens, 350);
        assert_eq!(s.context_limit, 400_000);
        // Daily bands come from differences between cumulative snapshots:
        // [non-cached input, 0, cached input, output].
        let daily: Vec<(&str, [u64; 4])> = s
            .daily_tokens
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        assert_eq!(
            daily,
            vec![
                ("2026-01-01", [60, 0, 40, 20]),
                ("2026-01-02", [140, 0, 160, 50])
            ]
        );
        assert_eq!(s.total_input_tokens, 200);
        assert_eq!(s.total_cache_creation_tokens, 0);
        assert_eq!(s.total_cache_read_tokens, 200);
        assert_eq!(s.total_output_tokens, 70);
        assert_eq!(s.cost_usd, 0.0);
        assert!(s.is_authoritative_stats);
        assert_eq!(s.agent, "codex");
    }

    #[test]
    fn session_meta_in_reads_first_line_confined() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        fs::create_dir_all(root.join("2026")).unwrap();
        let write = |name: &str, body: &str| {
            let p = root.join("2026").join(name);
            fs::write(&p, body).unwrap();
            p
        };
        let meta = |src: serde_json::Value| {
            serde_json::json!({"type":"session_meta","payload":{"id":"s1","cwd":"/p","source":src}})
                .to_string()
                + "\n{\"type\":\"event_msg\"}\n"
        };
        let cli = write("rollout-a-s1.jsonl", &meta(serde_json::json!("cli")));
        assert_eq!(
            session_meta_in(&root, &cli),
            Some(CodexSessionMeta {
                id: "s1".into(),
                cwd: "/p".into(),
                subagent: false
            })
        );
        let sub = write(
            "rollout-b-s1.jsonl",
            &meta(serde_json::json!({"subagent": {"thread_spawn": {}}})),
        );
        assert!(session_meta_in(&root, &sub).unwrap().subagent);
        let named = write("rollout-c-s1.jsonl", &meta(serde_json::json!("SubAgent")));
        assert!(session_meta_in(&root, &named).unwrap().subagent);
        // No source at all: not a subagent.
        let bare = write(
            "rollout-d-s1.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\",\"cwd\":\"/p\"}}\n",
        );
        assert!(!session_meta_in(&root, &bare).unwrap().subagent);
        // An incomplete first line, another first record, no id: nothing.
        let part = write(
            "rollout-e-s1.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\",\"cwd\":\"/p\"}}",
        );
        assert_eq!(session_meta_in(&root, &part), None);
        let other = write(
            "rollout-f-s1.jsonl",
            "{\"type\":\"event_msg\",\"payload\":{\"id\":\"s1\",\"cwd\":\"/p\"}}\n",
        );
        assert_eq!(session_meta_in(&root, &other), None);
        let noid = write(
            "rollout-g-s1.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/p\"}}\n",
        );
        assert_eq!(session_meta_in(&root, &noid), None);
        // Outside the root, or a symlink: refused.
        let outside = dir.path().join("rollout-x-s1.jsonl");
        fs::write(&outside, meta(serde_json::json!("cli"))).unwrap();
        assert_eq!(session_meta_in(&root, &outside), None);
        #[cfg(unix)]
        {
            let link = root.join("2026").join("rollout-h-s1.jsonl");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert_eq!(session_meta_in(&root, &link), None);
        }
    }

    #[test]
    fn parse_codex_session_requires_meta() {
        let f = Fixture::new();
        // Everything but the session_meta line.
        let no_meta: String = ROLLOUT
            .lines()
            .filter(|l| !l.contains("session_meta"))
            .collect::<Vec<_>>()
            .join("\n");
        let p = f.write("home/.codex/sessions/no-meta.jsonl", no_meta);
        assert!(parse_codex_session(&p, &HashMap::new()).is_none());
        // A meta line without a cwd is not enough either.
        let p = f.write(
            "home/.codex/sessions/no-cwd.jsonl",
            meta_line("0199ffff-0000", ""),
        );
        assert!(parse_codex_session(&p, &HashMap::new()).is_none());
        // Missing file.
        assert!(parse_codex_session(&f.home().join("nope.jsonl"), &HashMap::new()).is_none());
    }

    #[test]
    fn codex_session_names_override_title_last_wins() {
        let f = Fixture::new();
        f.write(
            "home/.codex/session_index.jsonl",
            [
                r#"{"id":"0199aaaa-bbbb-cccc-dddd-eeeeeeeeeeee","thread_name":"first name"}"#,
                "garbage",
                r#"{"id":"0199aaaa-bbbb-cccc-dddd-eeeeeeeeeeee","thread_name":"second name"}"#,
                r#"{"id":"blank","thread_name":"   "}"#,
                r#"{"id":"no-name"}"#,
            ]
            .join("\n"),
        );
        let names = codex_session_names(&f.ctx());
        assert_eq!(names.len(), 1);
        assert_eq!(
            names
                .get("0199aaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                .map(String::as_str),
            Some("second name")
        );
        let p = f.write("home/.codex/sessions/2026/01/02/rollout-a.jsonl", ROLLOUT);
        let s = parse_codex_session(&p, &names).unwrap();
        assert_eq!(s.title, "second name");
        // No home: no names.
        let mut ctx = f.ctx();
        ctx.home = None;
        assert!(codex_session_names(&ctx).is_empty());
    }

    #[test]
    fn list_codex_projects_groups_by_cwd() {
        let f = Fixture::new();
        let a1 = f.write(
            "home/.codex/sessions/2026/01/01/r1.jsonl",
            meta_line("1", "/a"),
        );
        let a2 = f.write(
            "home/.codex/sessions/2026/01/03/r2.jsonl",
            meta_line("2", "/a"),
        );
        let b1 = f.write(
            "home/.codex/sessions/2026/01/02/r3.jsonl",
            meta_line("3", "/b"),
        );
        // Skipped: first line has no cwd, and a non-jsonl file.
        f.write(
            "home/.codex/sessions/2026/01/02/r4.jsonl",
            "{\"type\":\"x\"}\n",
        );
        f.write(
            "home/.codex/sessions/2026/01/02/notes.txt",
            meta_line("5", "/c"),
        );
        f.set_mtime(&a1, at(1_000_000));
        f.set_mtime(&a2, at(3_000_000));
        f.set_mtime(&b1, at(2_000_000));

        let projects: Vec<(String, usize, String)> = list_codex_projects(&f.ctx())
            .into_iter()
            .map(|p| (p.path, p.session_count, p.last_active))
            .collect();
        assert_eq!(
            projects,
            vec![
                ("/a".into(), 2, system_time_to_iso(at(3_000_000))),
                ("/b".into(), 1, system_time_to_iso(at(2_000_000))),
            ]
        );
        assert!(list_codex_projects(&Fixture::new().ctx()).is_empty());
    }

    #[test]
    fn rollouts_cache_first_lines_by_mtime_and_length() {
        let f = Fixture::new();
        let p = f.write(
            "home/.codex/sessions/2026/01/01/r1.jsonl",
            meta_line("id-1", "/a") + "\n",
        );
        f.write("home/.codex/sessions/2026/01/01/bad.jsonl", "{}\n");
        f.set_mtime(&p, at(1_000));
        let ctx = f.ctx();
        let metas = |ctx: &HostCtx| -> Vec<(String, String)> {
            rollouts(ctx)
                .into_iter()
                .map(|r| (r.meta.id, r.meta.cwd))
                .collect()
        };
        assert_eq!(metas(&ctx), vec![("id-1".into(), "/a".into())]);
        // Same length and mtime: the cached first line is used, the file is not read.
        fs::write(&p, meta_line("id-1", "/b") + "\n").unwrap();
        f.set_mtime(&p, at(1_000));
        assert_eq!(metas(&ctx), vec![("id-1".into(), "/a".into())]);
        // A new mtime reads it again.
        f.set_mtime(&p, at(2_000));
        assert_eq!(metas(&ctx), vec![("id-1".into(), "/b".into())]);
        // A first line longer than the cap is not read past it.
        let long = serde_json::json!({"type": "session_meta",
            "payload": {"id": "id-2", "cwd": "/c", "pad": "x".repeat(META_LINE_MAX as usize)}});
        f.write("home/.codex/sessions/2026/01/02/r2.jsonl", long.to_string());
        assert_eq!(metas(&ctx).len(), 1);
        let mut none = f.ctx();
        none.home = None;
        assert!(rollouts(&none).is_empty());
    }

    #[test]
    fn get_codex_usage_reads_rate_limits_and_daily_counts() {
        let f = Fixture::new();
        let rl = |used: f64| {
            serde_json::json!({
                "timestamp": format!("2026-01-01T0{}:00:00Z", used as u64 % 10),
                "type": "event_msg",
                "payload": {"type": "token_count", "rate_limits": {
                    "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_700_000_000u64},
                    "secondary": {"used_percent": 40.0, "window_minutes": 10080},
                    "plan_type": "plus",
                }},
            })
            .to_string()
        };
        // Older file with two token_count events: the last one counts.
        let older = f.write(
            "home/.codex/sessions/2026/01/01/r1.jsonl",
            [meta_line("1", "/a"), rl(11.0), rl(12.0)].join("\n"),
        );
        // Newest file has no token_count yet, so usage falls back to the older file.
        let newest = f.write(
            "home/.codex/sessions/2026/01/02/r2.jsonl",
            meta_line("2", "/a"),
        );
        // A rollout outside the YYYY/MM/DD layout counts as present but has no date.
        let odd = f.write("home/.codex/sessions/misc/r3.jsonl", meta_line("3", "/a"));
        f.set_mtime(&odd, at(1_000));
        f.set_mtime(&older, at(2_000));
        f.set_mtime(&newest, at(3_000));

        let u = get_codex_usage(&f.ctx());
        assert!(u.present);
        let primary = u.primary.expect("primary");
        assert_eq!(primary.used_percent, Some(12.0));
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, Some(1_700_000_000));
        let secondary = u.secondary.expect("secondary");
        assert_eq!(secondary.used_percent, Some(40.0));
        assert_eq!(secondary.window_minutes, Some(10080));
        assert_eq!(secondary.resets_at, None);
        assert_eq!(u.plan_type.as_deref(), Some("plus"));
        assert_eq!(
            u.rate_limits_updated_iso.as_deref(),
            Some("2026-01-01T02:00:00Z")
        );
        let daily: Vec<(String, usize)> = u
            .daily_sessions
            .into_iter()
            .map(|d| (d.date, d.count))
            .collect();
        assert_eq!(
            daily,
            vec![("2026-01-01".into(), 1), ("2026-01-02".into(), 1)]
        );

        let empty = get_codex_usage(&Fixture::new().ctx());
        assert!(!empty.present);
        assert!(empty.primary.is_none() && empty.daily_sessions.is_empty());
    }

    #[test]
    fn get_codex_context_lists_instructions_prompts_mcp_and_trust() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.dir.path().join("repo/.git")).unwrap();
        let project = f.dir.path().join("repo").join("sub");
        let project_agents = f.write("repo/sub/AGENTS.md", "# p");
        let root_agents = f.write("repo/AGENTS.md", "# r");
        let global_agents = f.write("home/.codex/AGENTS.md", "# g");
        let prompt_b = f.write("home/.codex/prompts/b.md", "b");
        let prompt_a = f.write("home/.codex/prompts/a.md", "a");
        f.write("home/.codex/prompts/ignored.txt", "x");
        let project_str = path_str(&project);
        f.write(
            "home/.codex/config.toml",
            format!(
                "model = \"gpt-5\"\n\
                 [mcp_servers.docs]\n\
                 command = \"npx\"\n\
                 args = [\"-y\"]\n\
                 [mcp_servers.\"quoted\"]\n\
                 url = \"http://x\"\n\
                 [projects.'{project_str}']\n\
                 trust_level = \"trusted\"\n\
                 [projects.'/elsewhere']\n\
                 trust_level = \"untrusted\"\n"
            ),
        );

        let c = get_codex_context(&f.ctx(), project_str.clone());
        assert!(c.present);
        assert_eq!(c.trust_level.as_deref(), Some("trusted"));
        let item = |n: &str, d: &str, p: String| (n.to_string(), d.to_string(), p);
        assert_eq!(
            sections_view(&c.sections),
            vec![
                (
                    "Instructions".to_string(),
                    vec![
                        item("AGENTS.md", "project", path_str(&project_agents)),
                        item("AGENTS.md", "repo root", path_str(&root_agents)),
                        item("AGENTS.md", "global", path_str(&global_agents)),
                    ]
                ),
                (
                    "Prompts".to_string(),
                    vec![
                        item("/a", "global", path_str(&prompt_a)),
                        item("/b", "global", path_str(&prompt_b)),
                    ]
                ),
                (
                    "MCP servers".to_string(),
                    vec![
                        item("docs", "npx", String::new()),
                        item("quoted", "", String::new()),
                    ]
                ),
            ]
        );

        // Nothing configured: not present.
        let empty = Fixture::new();
        let c = get_codex_context(&empty.ctx(), path_str(&empty.dir.path().join("p")));
        assert!(!c.present && c.trust_level.is_none() && c.sections.is_empty());
    }
}

//! Read-only Pi 1.3+ default-storage history and conventional context resources.
//! Tree metadata follows Pi's linked entries; accounting deliberately includes all branches.

use crate::agent_context::{AgentContextItem, AgentContextSection};
use crate::ctx::HostCtx;
use crate::sessions::{CodexProjectInfo, SessionInfo};
use crate::time::{iso_to_unix_ms, unix_ms_to_iso};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const SUMMARY_CACHE_MAX: usize = 512;
type SummaryCache = HashMap<PathBuf, (SystemTime, u64, Option<SessionInfo>)>;

fn summary_cache() -> &'static Mutex<SummaryCache> {
    static CACHE: OnceLock<Mutex<SummaryCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

// Called only for the current scan's confined regular files. The lock protects summaries,
// never file I/O or parsing, and no JSON entry trees survive in the cache.
fn cached_summary(
    path: &Path,
    cache: &Mutex<SummaryCache>,
    load: &mut impl FnMut(&Path) -> Option<SessionInfo>,
) -> Option<SessionInfo> {
    let metadata = fs::metadata(path).ok()?;
    let (modified, len) = (metadata.modified().ok()?, metadata.len());
    if let Ok(cache) = cache.lock() {
        if let Some((t, l, summary)) = cache.get(path) {
            if *t == modified && *l == len {
                return summary.clone();
            }
        }
    }
    let summary = load(path);
    if let Ok(mut cache) = cache.lock() {
        crate::codex::bounded_insert(
            &mut cache,
            SUMMARY_CACHE_MAX,
            path.to_path_buf(),
            (modified, len, summary.clone()),
        );
    }
    summary
}

/// Pi IDs are project-scoped. Windows paths compare by separator/case, Unix paths do not.
/// Used by discovery and the Daemon without changing the LaunchSpec wire contract.
pub fn project_key(cwd: &str) -> String {
    // A leading // is ambiguous on Unix; only the Windows Host treats it as UNC.
    let windows_path = crate::direct_exec::windows_fully_qualified(cwd)
        && (cfg!(windows) || !cwd.starts_with("//"));
    if windows_path {
        cwd.replace('\\', "/").trim_end_matches('/').to_lowercase()
    } else {
        let path = cwd.trim_end_matches('/');
        if path.is_empty() && cwd.starts_with('/') {
            "/".into()
        } else {
            path.to_string()
        }
    }
}

// Check every component below a trusted base, not just the final file. Never descend into
// symlinks (including .pi/agent/sessions itself). Missing components also fail closed.
fn confined(base: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(base) else {
        return false;
    };
    let mut p = base.to_path_buf();
    if !fs::symlink_metadata(&p).is_ok_and(|m| !m.file_type().is_symlink()) {
        return false;
    }
    for c in rel.components() {
        if !matches!(c, std::path::Component::Normal(_)) {
            return false;
        }
        p.push(c);
        if !fs::symlink_metadata(&p).is_ok_and(|m| !m.file_type().is_symlink()) {
            return false;
        }
    }
    true
}

pub fn parse_pi_sessions(ctx: &HostCtx) -> Vec<SessionInfo> {
    parse_pi_sessions_with(ctx, summary_cache(), &mut parse_session)
}

fn parse_pi_sessions_with(
    ctx: &HostCtx,
    cache: &Mutex<SummaryCache>,
    load: &mut impl FnMut(&Path) -> Option<SessionInfo>,
) -> Vec<SessionInfo> {
    let Some(home) = ctx.home().and_then(|p| std::path::absolute(p).ok()) else {
        return vec![];
    };
    let root = home.join(".pi/agent/sessions");
    if !confined(&home, &root) {
        return vec![];
    }
    // Default storage is exactly sessions/<group>/<file>.jsonl; no configuration or
    // parentSession paths are followed. Sort paths so duplicate-file winners are stable.
    let mut files = Vec::new();
    for group in fs::read_dir(&root).ok().into_iter().flatten().flatten() {
        if !group.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        for file in fs::read_dir(group.path())
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = file.path();
            if file.file_type().is_ok_and(|t| t.is_file())
                && p.extension().is_some_and(|e| e == "jsonl")
                && confined(&home, &p)
            {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut sessions = BTreeMap::new();
    for p in files {
        if let Some(s) = cached_summary(&p, cache, load) {
            sessions.insert((project_key(&s.project_path), s.id.clone()), s);
        }
    }
    sessions.into_values().collect()
}

pub fn list_pi_projects(ctx: &HostCtx) -> Vec<CodexProjectInfo> {
    let mut projects: BTreeMap<String, CodexProjectInfo> = BTreeMap::new();
    for s in parse_pi_sessions(ctx) {
        let p = projects
            .entry(project_key(&s.project_path))
            .or_insert(CodexProjectInfo {
                path: s.project_path,
                session_count: 0,
                last_active: String::new(),
            });
        p.session_count += 1;
        p.last_active = p.last_active.clone().max(s.timestamp);
    }
    projects.into_values().collect()
}

fn timestamp(v: &Value) -> Option<u64> {
    v["timestamp"]
        .as_str()
        .and_then(iso_to_unix_ms)
        .or_else(|| {
            // Typed fallback only. Bound untrusted milliseconds before the date formatter.
            v["message"]["timestamp"]
                .as_u64()
                .filter(|ms| *ms <= 253_402_300_799_999)
        })
}

fn usage(v: &Value) -> Option<&Value> {
    match v["type"].as_str()? {
        "message"
            if matches!(
                v["message"]["role"].as_str(),
                Some("assistant" | "toolResult")
            ) =>
        {
            v["message"].get("usage")
        }
        "usage" | "compaction" | "branch_summary" => v.get("usage"),
        _ => None,
    }
}

fn tokens(u: &Value) -> [u64; 4] {
    ["input", "cacheWrite", "cacheRead", "output"].map(|k| u[k].as_u64().unwrap_or(0))
}

fn text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_session(path: &Path) -> Option<SessionInfo> {
    let file = fs::File::open(path).ok()?;
    let mut header = None;
    let mut name = String::new();
    let mut entries = Vec::new();
    let mut index = HashMap::new();
    let mut leaf = None;
    let mut first_ms = None::<u64>;
    let mut last_ms = None::<u64>;
    for bytes in BufReader::new(file).split(b'\n').map_while(Result::ok) {
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(ms) = timestamp(&v) {
            first_ms = Some(first_ms.map_or(ms, |n| n.min(ms)));
            last_ms = Some(last_ms.map_or(ms, |n| n.max(ms)));
        }
        if v["type"] == "session" {
            if header.is_none()
                && v["version"] == 3
                && v["id"].as_str().is_some_and(|s| !s.is_empty())
                && v["cwd"].as_str().is_some_and(|s| !s.is_empty())
            {
                header = Some(v);
            }
            continue;
        }
        if v["type"] == "session_info" {
            if let Some(n) = v["name"].as_str() {
                name = n.trim().to_string();
            }
        }
        // Linked session_info IS a tree entry and advances the leaf, just like unknown
        // linked entry types. Legacy unlinked metadata updates only the global name.
        if let Some(id) = v["id"].as_str().filter(|s| !s.is_empty()) {
            if v.get("parentId")
                .is_some_and(|p| p.is_null() || p.is_string())
            {
                let id = id.to_string();
                if let Some(&at) = index.get(&id) {
                    entries[at] = v;
                } else {
                    index.insert(id.clone(), entries.len());
                    entries.push(v);
                }
                leaf = Some(id);
            }
        }
    }
    let h = header?;
    let cwd = h["cwd"].as_str()?.to_string();
    let mut s = SessionInfo {
        id: h["id"].as_str()?.to_string(),
        title: String::new(),
        timestamp: last_ms.map(unix_ms_to_iso).unwrap_or_default(),
        message_count: 0,
        project_name: cwd
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .into(),
        project_path: cwd,
        git_branch: String::new(),
        claude_version: String::new(),
        tool_use_count: 0,
        duration_ms: last_ms.unwrap_or(0).saturating_sub(first_ms.unwrap_or(0)),
        model: String::new(),
        context_tokens: 0,
        context_limit: 0,
        cost_usd: 0.0,
        is_authoritative_stats: false,
        daily_cost: BTreeMap::new(),
        rate_limit_5h_pct: None,
        rate_limit_7d_pct: None,
        total_input_tokens: 0,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens: 0,
        total_output_tokens: 0,
        daily_tokens: BTreeMap::new(),
        agent: "pi".into(),
    };
    // Lifetime recorded accounting, once per entry ID, regardless of the active branch.
    for v in &entries {
        let Some(u) = usage(v) else { continue };
        let t = tokens(u);
        s.total_input_tokens = s.total_input_tokens.saturating_add(t[0]);
        s.total_cache_creation_tokens = s.total_cache_creation_tokens.saturating_add(t[1]);
        s.total_cache_read_tokens = s.total_cache_read_tokens.saturating_add(t[2]);
        s.total_output_tokens = s.total_output_tokens.saturating_add(t[3]);
        let cost = u["cost"]["total"]
            .as_f64()
            .filter(|c| c.is_finite() && *c >= 0.0)
            .unwrap_or(0.0);
        s.cost_usd += cost;
        if let Some(ms) = timestamp(v) {
            let day = unix_ms_to_iso(ms)[..10].to_string();
            *s.daily_cost.entry(day.clone()).or_default() += cost;
            let bucket = s.daily_tokens.entry(day).or_default();
            for (sum, n) in bucket.iter_mut().zip(t) {
                *sum = sum.saturating_add(n);
            }
        }
    }
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    while let Some(id) = leaf {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(&at) = index.get(&id) else { break };
        let v = &entries[at];
        chain.push(v);
        leaf = v["parentId"].as_str().map(str::to_string);
    }
    for v in chain.into_iter().rev() {
        if v["type"] == "model_change" {
            if let Some(m) = v["modelId"].as_str() {
                s.model = m.into();
            }
        }
        if v["type"] != "message" {
            continue;
        }
        let m = &v["message"];
        match m["role"].as_str() {
            Some("user") => {
                s.message_count += 1;
                if s.title.is_empty() {
                    s.title = text(&m["content"]).trim().chars().take(200).collect();
                }
            }
            Some("assistant") => {
                s.message_count += 1;
                if let Some(model) = m["model"].as_str() {
                    s.model = model.into();
                }
                s.tool_use_count += m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "toolCall")
                    .count();
                // Only a main assistant's prompt usage is current context. No price/window
                // estimates, no reasoning or cacheWrite1h subsets added again.
                s.context_tokens = 0;
                if let Some(u) = m.get("usage") {
                    let t = tokens(u);
                    s.context_tokens = t[0].saturating_add(t[1]).saturating_add(t[2]);
                }
            }
            _ => {}
        }
    }
    if !name.is_empty() {
        s.title = name;
    }
    if s.title.is_empty() {
        s.title = "New Pi session".into();
    }
    Some(s)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PiContext {
    pub present: bool,
    pub sections: Vec<AgentContextSection>,
}

/// Names/files only: not Pi's resolved package loadout, config values or extension output.
pub fn get_pi_context(ctx: &HostCtx, project_path: String) -> PiContext {
    let mut result = PiContext {
        present: false,
        sections: Vec::new(),
    };
    let project = PathBuf::from(project_path);
    let mut roots = Vec::new();
    if let Some(home) = ctx.home() {
        let global = home.join(".pi/agent");
        if confined(home, &global) {
            roots.push((global, "Global"));
        }
    }
    if project.is_absolute() && confined(&project, &project) {
        roots.push((project, "Project"));
    }
    for (base, scope) in roots {
        let resources = if scope == "Project" {
            base.join(".pi")
        } else {
            base.clone()
        };
        result.present |= confined(&base, &resources) && resources.is_dir();
        let mut instructions = Vec::new();
        let variants = if confined(&base, &base.join("AGENTS.override.md")) {
            vec!["AGENTS.override.md"]
        } else {
            vec!["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"]
        };
        for name in variants {
            add_resource(&base, &base.join(name), scope, &mut instructions);
        }
        for name in ["SYSTEM.md", "APPEND_SYSTEM.md"] {
            add_resource(&base, &resources.join(name), scope, &mut instructions);
        }
        // Case-insensitive volumes can resolve two spelling variants to one file.
        // File identity also preserves genuinely distinct case-sensitive Unix files;
        // different logical names (e.g. SYSTEM.md) retain their separate roles.
        let mut instruction_files = HashSet::new();
        instructions.retain(|item| {
            crate::last_line::open_confined(&base, Path::new(&item.path))
                .and_then(|file| crate::last_line::file_id(&file))
                .is_none_or(|identity| {
                    instruction_files.insert((item.name.to_lowercase(), identity))
                })
        });
        if !instructions.is_empty() {
            result.sections.push(AgentContextSection {
                title: format!("{scope} instructions"),
                items: instructions,
            });
        }
        for (dir, title) in [
            ("skills", "skills"),
            ("prompts", "prompts"),
            ("extensions", "extensions"),
        ] {
            let mut items = Vec::new();
            let root = resources.join(dir);
            if !confined(&base, &root) {
                continue;
            }
            let mut stack = vec![(root, 0)];
            let mut budget = 4096usize;
            while let Some((p, depth)) = stack.pop() {
                let mut children: Vec<_> = fs::read_dir(&p)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .take(budget)
                    .collect();
                children.sort_by_key(|e| e.path());
                for e in children {
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    let p = e.path();
                    let Ok(ft) = e.file_type() else { continue };
                    if ft.is_dir() && depth < 8 {
                        stack.push((p, depth + 1));
                    } else if ft.is_file()
                        && match dir {
                            "skills" => p.file_name().is_some_and(|n| n == "SKILL.md"),
                            "prompts" => p.extension().is_some_and(|e| e == "md"),
                            _ => p.extension().is_some_and(|e| {
                                e == "ts" || e == "js" || e == "mjs" || e == "cjs"
                            }),
                        }
                    {
                        add_resource(&base, &p, scope, &mut items);
                    }
                }
                if budget == 0 {
                    break;
                }
            }
            items.sort_by(|a, b| a.path.cmp(&b.path));
            if !items.is_empty() {
                result.sections.push(AgentContextSection {
                    title: format!("{scope} {title}"),
                    items,
                });
            }
        }
    }
    result.present |= !result.sections.is_empty();
    result
}

fn add_resource(base: &Path, p: &Path, scope: &str, items: &mut Vec<AgentContextItem>) {
    if confined(base, p) && p.is_file() {
        let path = p.to_string_lossy().into_owned();
        // Windows resolves case aliases to one file (AGENTS.md/AGENTS.MD).
        if items
            .iter()
            .any(|item| project_key(&item.path) == project_key(&path))
        {
            return;
        }
        items.push(AgentContextItem {
            name: if p.file_name().is_some_and(|n| n == "SKILL.md") {
                p.parent()
                    .and_then(Path::file_name)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            },
            detail: format!("{scope} conventional resource"),
            path,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use serde_json::json;

    fn header(cwd: &str, id: &str) -> Value {
        json!({"type":"session", "version":3, "id":id, "cwd":cwd, "timestamp":"2026-01-01T00:00:00Z"})
    }
    fn user(id: &str, parent: Value, content: Value) -> Value {
        json!({"type":"message", "id":id, "parentId":parent, "message":{"role":"user", "content":content}})
    }
    fn assistant(id: &str, parent: Value, model: &str, usage: Value) -> Value {
        json!({"type":"message", "id":id, "parentId":parent,
            "timestamp":"2026-01-02T00:00:00Z", "message":{"role":"assistant", "model":model,
                "content":[{"type":"text", "text":"answer"}, {"type":"toolCall", "id":"call", "name":"read"}], "usage":usage}})
    }
    fn parse(entries: Vec<Value>) -> SessionInfo {
        let fx = Fixture::new();
        let p = fx.write_jsonl("home/.pi/agent/sessions/arbitrary/file.jsonl", &entries);
        parse_session(&p).unwrap()
    }
    fn recorded() -> Value {
        json!({"input":10,"cacheWrite":20,"cacheWrite1h":20,"cacheRead":30,
            "output":40,"reasoning":15,"totalTokens":100,"cost":{"total":0.25}})
    }

    #[test]
    fn pi_active_branch_excludes_abandoned_text() {
        let s = parse(vec![
            header("/work/a", "same"),
            user("a", Value::Null, json!("abandoned")),
            assistant("b", json!("a"), "wrong", recorded()),
            user("c", Value::Null, json!("active")),
            assistant("d", json!("c"), "right", json!({"input":5})),
        ]);
        assert_eq!(s.title, "active");
        assert_eq!(s.model, "right");
        assert_eq!((s.message_count, s.tool_use_count), (2, 1));
        assert_eq!(s.context_tokens, 5);
        assert_eq!(s.total_input_tokens, 15);
    }

    #[test]
    fn pi_linked_session_info_advances_leaf_and_global_blank_name_clears() {
        let entries = vec![
            header("/work/a", "s"),
            user("a", Value::Null, json!("first branch")),
            user("b", Value::Null, json!("second branch")),
            json!({"type":"session_info", "id":"name", "parentId":"a", "name":"Global name"}),
        ];
        let s = parse(entries.clone());
        assert_eq!((s.title.as_str(), s.message_count), ("Global name", 1));
        let mut child = entries.clone();
        child.push(assistant(
            "child",
            json!("name"),
            "child-model",
            Value::Null,
        ));
        let s = parse(child);
        assert_eq!((s.model.as_str(), s.message_count), ("child-model", 2));
        let mut blank = entries;
        blank.push(json!({"type":"session_info", "name":"  "}));
        assert_eq!(parse(blank.clone()).title, "first branch");
        blank.push(json!({"type":"session_info", "name":"Legacy global"}));
        let s = parse(blank);
        assert_eq!((s.title.as_str(), s.message_count), ("Legacy global", 1));
    }

    #[test]
    fn pi_multiple_roots_and_unknown_entries() {
        let s = parse(vec![
            header("/a", "s"),
            user("a", Value::Null, json!("old root")),
            user("b", Value::Null, json!("new root")),
            json!({"type":"future", "id":"x", "parentId":"b"}),
            assistant("c", json!("x"), "new-model", Value::Null),
            json!({"type":"unknown", "data":"not a tree entry"}),
        ]);
        assert_eq!(
            (s.title.as_str(), s.model.as_str()),
            ("new root", "new-model")
        );
        assert_eq!(s.message_count, 2);
    }

    #[test]
    fn pi_missing_parent_cycle_and_truncated_tail() {
        let fx = Fixture::new();
        let p = fx.write_jsonl(
            "home/s.jsonl",
            &[
                header("/a", "s"),
                user("old", Value::Null, json!("abandoned")),
                user("orphan", json!("missing"), json!("orphan")),
            ],
        );
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"\xff\n{\"type\":")
            .unwrap();
        assert_eq!(parse_session(&p).unwrap().title, "orphan");
        let s = parse(vec![
            header("/a", "s"),
            user("a", json!("b"), json!("cycle")),
            assistant("b", json!("a"), "model", Value::Null),
        ]);
        assert_eq!(s.message_count, 2);
        assert_eq!(s.title, "cycle");
        assert_eq!(parse(vec![header("/a", "s")]).title, "New Pi session");
    }

    #[test]
    fn pi_text_blocks_tools_and_model_changes() {
        let s = parse(vec![
            header(r"C:\work\Project", "s"),
            user(
                "a",
                Value::Null,
                json!([{"type":"image", "data":"ignore"}, {"type":"text", "text":"one"}, {"type":"text", "text":"two"}]),
            ),
            assistant("b", json!("a"), "physical", recorded()),
            json!({"type":"message", "id":"c", "parentId":"b", "message":{"role":"toolResult", "isError":true,"content":[{"type":"text","text":"not assistant"}]}}),
            json!({"type":"model_change", "id":"d", "parentId":"c", "modelId":"selected"}),
        ]);
        assert_eq!(
            (s.title.as_str(), s.model.as_str(), s.project_name.as_str()),
            ("one\ntwo", "selected", "Project")
        );
        assert_eq!((s.message_count, s.tool_use_count), (2, 1));
    }

    #[test]
    fn pi_entry_iso_and_message_millisecond_timestamps() {
        let mut a = assistant("b", json!("a"), "model", recorded());
        a["timestamp"] = json!("2026-01-02T01:00:00+02:00");
        let mut b = assistant("c", json!("b"), "model", recorded());
        b["timestamp"] = json!("bad");
        b["message"]["timestamp"] = json!(1_767_312_000_123u64); // Jan 2, 00:00 UTC
        let s = parse(vec![
            header("/a", "s"),
            user("a", Value::Null, json!("hi")),
            a,
            b,
        ]);
        assert_eq!(s.timestamp, "2026-01-02T00:00:00Z");
        assert_eq!(s.duration_ms, 86_400_123);
        assert_eq!(s.daily_tokens["2026-01-01"], [10, 20, 30, 40]);
        assert_eq!(s.daily_tokens["2026-01-02"], [10, 20, 30, 40]);
    }

    #[test]
    fn pi_lifetime_usage_counts_all_recorded_sources_once() {
        let mut entries = vec![
            header("/a", "s"),
            assistant("a", Value::Null, "main", recorded()),
        ];
        // A repeated physical entry is not another charge.
        entries.push(entries[1].clone());
        for (i, ty) in ["usage", "compaction", "branch_summary"].iter().enumerate() {
            entries.push(json!({"type":ty, "id":format!("top{i}"), "parentId":null,
                "timestamp":"2026-01-03T00:00:00Z", "usage":recorded(), "tokensBefore":999999, "fromId":"a"}));
        }
        entries.push(json!({"type":"message", "id":"tool", "parentId":null,
            "message":{"role":"toolResult", "usage":recorded(), "timestamp":1767398400000u64}}));
        let s = parse(entries);
        assert_eq!(
            (
                s.total_input_tokens,
                s.total_cache_creation_tokens,
                s.total_cache_read_tokens,
                s.total_output_tokens
            ),
            (50, 100, 150, 200)
        );
        assert_eq!(s.cost_usd, 1.25);
        assert_eq!(s.daily_cost.values().sum::<f64>(), s.cost_usd);
        assert_eq!(s.context_tokens, 0); // no active main assistant
        assert_eq!(s.model, "");
    }

    #[test]
    fn pi_reasoning_and_cache_write_subsets_not_double_counted() {
        let s = parse(vec![
            header("/a", "s"),
            assistant("a", Value::Null, "m", recorded()),
        ]);
        assert_eq!(s.total_output_tokens, 40);
        assert_eq!(s.total_cache_creation_tokens, 20);
        assert_eq!(s.context_tokens, 60);
    }

    #[test]
    fn pi_recorded_costs_only_and_unknown_context_limit() {
        let s = parse(vec![
            header("/a", "s"),
            assistant(
                "a",
                Value::Null,
                "known-model",
                json!({"input":100000,"output":100}),
            ),
        ]);
        assert_eq!(s.cost_usd, 0.0);
        assert_eq!(s.context_limit, 0);
        assert!(!s.is_authoritative_stats);
        assert_eq!((s.rate_limit_5h_pct, s.rate_limit_7d_pct), (None, None));
    }

    #[test]
    fn pi_summary_cache_reuses_unchanged_summaries_and_none_without_locking_io() {
        let fx = Fixture::new();
        fx.write_jsonl(
            "home/.pi/agent/sessions/group/good.jsonl",
            &[header("/work/a", "s")],
        );
        fx.write("home/.pi/agent/sessions/group/invalid.jsonl", "not JSON\n");
        let cache = Mutex::new(SummaryCache::new());
        let loads = std::cell::Cell::new(0);
        let mut load = |p: &Path| {
            assert!(cache.try_lock().is_ok(), "cache locked across loader I/O");
            loads.set(loads.get() + 1);
            parse_session(p)
        };
        let first = parse_pi_sessions_with(&fx.ctx(), &cache, &mut load);
        assert_eq!(first.len(), 1);
        assert_eq!(loads.get(), 2);
        let second = parse_pi_sessions_with(&fx.ctx(), &cache, &mut load);
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
        assert_eq!(loads.get(), 2);
        assert_eq!(cache.lock().unwrap().len(), 2);
        assert!(cache.lock().unwrap().values().any(|(_, _, s)| s.is_none()));
    }

    #[test]
    fn pi_summary_cache_invalidates_append_and_equal_size_rewrite() {
        use std::io::Write;
        use std::time::Duration;
        let fx = Fixture::new();
        let path = fx.write_jsonl(
            "home/.pi/agent/sessions/group/file.jsonl",
            &[header("/work/a", "s")],
        );
        let cache = Mutex::new(SummaryCache::new());
        let loads = std::cell::Cell::new(0);
        let mut load = |p: &Path| {
            loads.set(loads.get() + 1);
            parse_session(p)
        };
        assert_eq!(
            parse_pi_sessions_with(&fx.ctx(), &cache, &mut load)[0].title,
            "New Pi session"
        );
        let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            json!({"type":"session_info", "name":"one"})
        )
        .unwrap();
        // Length alone must invalidate even if the timestamp is unchanged.
        fx.set_mtime(&path, original_mtime);
        assert_eq!(
            parse_pi_sessions_with(&fx.ctx(), &cache, &mut load)[0].title,
            "one"
        );
        let old_len = fs::metadata(&path).unwrap().len();
        let rewritten = fs::read_to_string(&path).unwrap().replace("one", "two");
        fs::write(&path, rewritten).unwrap();
        fx.set_mtime(&path, original_mtime + Duration::from_secs(10));
        assert_eq!(fs::metadata(&path).unwrap().len(), old_len);
        assert_eq!(
            parse_pi_sessions_with(&fx.ctx(), &cache, &mut load)[0].title,
            "two"
        );
        assert_eq!(loads.get(), 3);
    }

    #[test]
    fn pi_summary_cache_lists_current_files_and_preserves_duplicate_winner() {
        let fx = Fixture::new();
        let first = fx.write_jsonl(
            "home/.pi/agent/sessions/group/a.jsonl",
            &[
                header("/work/a", "same"),
                json!({"type":"session_info", "name":"first"}),
            ],
        );
        let winner = fx.write_jsonl(
            "home/.pi/agent/sessions/group/z.jsonl",
            &[
                header("/work/a", "same"),
                json!({"type":"session_info", "name":"winner"}),
            ],
        );
        let cache = Mutex::new(SummaryCache::new());
        let loads = std::cell::Cell::new(0);
        let mut load = |p: &Path| {
            loads.set(loads.get() + 1);
            parse_session(p)
        };
        for _ in 0..2 {
            let rows = parse_pi_sessions_with(&fx.ctx(), &cache, &mut load);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].title, "winner");
        }
        assert_eq!(loads.get(), 2);
        fs::remove_file(winner).unwrap();
        let rows = parse_pi_sessions_with(&fx.ctx(), &cache, &mut load);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "first");
        fs::remove_file(first).unwrap();
        assert!(parse_pi_sessions_with(&fx.ctx(), &cache, &mut load).is_empty());
        assert_eq!(loads.get(), 2);
    }

    #[test]
    fn pi_summary_cache_is_bounded() {
        let fx = Fixture::new();
        for i in 0..=SUMMARY_CACHE_MAX {
            fx.write_jsonl(
                format!("home/.pi/agent/sessions/group/{i}.jsonl"),
                &[header("/work/a", &format!("s{i}"))],
            );
        }
        let cache = Mutex::new(SummaryCache::new());
        assert_eq!(
            parse_pi_sessions_with(&fx.ctx(), &cache, &mut parse_session).len(),
            SUMMARY_CACHE_MAX + 1
        );
        assert_eq!(cache.lock().unwrap().len(), SUMMARY_CACHE_MAX);
        assert!(cache.lock().unwrap().keys().all(|p| p.is_absolute()));
    }

    #[cfg(unix)]
    #[test]
    fn pi_summary_cache_never_bypasses_confinement() {
        use std::os::unix::fs::symlink;
        let fx = Fixture::new();
        let path = fx.write_jsonl(
            "home/.pi/agent/sessions/group/file.jsonl",
            &[header("/work/a", "s")],
        );
        let cache = Mutex::new(SummaryCache::new());
        let loads = std::cell::Cell::new(0);
        let mut load = |p: &Path| {
            loads.set(loads.get() + 1);
            parse_session(p)
        };
        assert_eq!(
            parse_pi_sessions_with(&fx.ctx(), &cache, &mut load).len(),
            1
        );
        // Each replacement still resolves to exactly the cached bytes and mtime.
        // A cached summary must not authorize a newly symlinked file or ancestor.
        let outside = fx.dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let moved_file = outside.join("file.jsonl");
        fs::rename(&path, &moved_file).unwrap();
        symlink(&moved_file, &path).unwrap();
        assert!(parse_pi_sessions_with(&fx.ctx(), &cache, &mut load).is_empty());
        fs::remove_file(&path).unwrap();
        fs::rename(&moved_file, &path).unwrap();
        assert_eq!(
            parse_pi_sessions_with(&fx.ctx(), &cache, &mut load).len(),
            1
        );
        let group = path.parent().unwrap();
        let moved_group = outside.join("group");
        fs::rename(group, &moved_group).unwrap();
        symlink(&moved_group, group).unwrap();
        assert!(parse_pi_sessions_with(&fx.ctx(), &cache, &mut load).is_empty());
        fs::remove_file(group).unwrap();
        fs::rename(&moved_group, group).unwrap();
        let root = group.parent().unwrap();
        let moved_root = outside.join("sessions");
        fs::rename(root, &moved_root).unwrap();
        symlink(&moved_root, root).unwrap();
        assert!(parse_pi_sessions_with(&fx.ctx(), &cache, &mut load).is_empty());
        assert_eq!(loads.get(), 1);
    }

    #[test]
    fn pi_only_project_is_discoverable() {
        let fx = Fixture::new();
        fx.write_jsonl(
            "home/.pi/agent/sessions/not-encoded/a.jsonl",
            &[header("/work/pi-only", "custom")],
        );
        fx.write_jsonl(
            "home/.pi/agent/sessions/not-encoded/b.jsonl",
            &[header("/work/other", "custom")],
        );
        fx.write_jsonl(
            "home/.pi/agent/sessions/duplicate/c.jsonl",
            &[header("/work/pi-only", "custom")],
        );
        let projects = list_pi_projects(&fx.ctx());
        assert_eq!(projects.len(), 2);
        assert!(projects.iter().all(|p| p.session_count == 1));
        let rows = crate::sessions::get_sessions(
            &fx.ctx(),
            crate::claude::encode_project_name("/work/pi-only"),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent, "pi");
        assert_eq!(
            crate::sessions::get_all_recent_sessions(&fx.ctx(), 10).len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn pi_discovery_uses_header_cwd_and_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let fx = Fixture::new();
        let outside = fx.write_jsonl("outside/file.jsonl", &[header("/outside", "bad")]);
        let good = fx.write_jsonl(
            "home/.pi/agent/sessions/wrong-encoded/good.jsonl",
            &[header("/actual/cwd", "good")],
        );
        let root = fx.home().join(".pi/agent/sessions");
        symlink(&outside, root.join("wrong-encoded/link.jsonl")).unwrap();
        symlink(outside.parent().unwrap(), root.join("linked-group")).unwrap();
        let rows = parse_pi_sessions(&fx.ctx());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project_path, "/actual/cwd");
        fs::remove_file(good).unwrap();
        fs::remove_dir_all(root.join("wrong-encoded")).unwrap();
        fs::remove_file(root.join("linked-group")).unwrap();
        fs::remove_dir(&root).unwrap();
        symlink(outside.parent().unwrap(), &root).unwrap();
        assert!(parse_pi_sessions(&fx.ctx()).is_empty());
        fs::remove_file(&root).unwrap();
        fs::remove_dir(root.parent().unwrap()).unwrap();
        symlink(outside.parent().unwrap(), fx.home().join(".pi/agent")).unwrap();
        assert!(parse_pi_sessions(&fx.ctx()).is_empty());
    }

    #[test]
    fn pi_project_keys_preserve_unix_case_and_normalize_windows_spelling() {
        assert_ne!(project_key("/Work/A"), project_key("/work/a"));
        assert_eq!(project_key("/Work/A/"), project_key("/Work/A"));
        assert_eq!(project_key("/"), "/");
        #[cfg(not(windows))]
        assert_ne!(project_key("//Work/A"), project_key("//work/a"));
        assert_eq!(project_key(r"C:\Work\A\"), project_key("c:/work/a"));
        assert_eq!(
            project_key(r"\\Host\Share\A"),
            project_key("//host/share/a")
        );
    }

    #[test]
    fn pi_context_deduplicates_physical_instruction_aliases() {
        let fx = Fixture::new();
        let original = fx.write("home/.pi/agent/AGENTS.md", "instructions");
        let alias = original.with_file_name("AGENTS.MD");
        // Case-sensitive volumes can reproduce a case-insensitive volume's aliases
        // with a hardlink to the same physical file.
        if !alias.exists() {
            fs::hard_link(&original, &alias).unwrap();
        }
        let context = get_pi_context(&fx.ctx(), String::new());
        let items: Vec<_> = context.sections.iter().flat_map(|s| &s.items).collect();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn pi_context_keeps_distinct_instruction_files_and_roles() {
        let fx = Fixture::new();
        let original = fx.write("home/.pi/agent/AGENTS.md", "instructions");
        let alias = original.with_file_name("AGENTS.MD");
        let case_sensitive = !alias.exists();
        if case_sensitive {
            fs::write(&alias, "distinct instructions").unwrap();
        }
        fs::hard_link(&original, original.with_file_name("APPEND_SYSTEM.md")).unwrap();
        let context = get_pi_context(&fx.ctx(), String::new());
        let items: Vec<_> = context.sections.iter().flat_map(|s| &s.items).collect();
        assert_eq!(items.len(), if case_sensitive { 3 } else { 2 });
        assert!(items.iter().any(|item| item.name == "APPEND_SYSTEM.md"));
    }

    #[cfg(windows)]
    #[test]
    fn pi_context_deduplicates_windows_case_aliases() {
        let fx = Fixture::new();
        fx.write("home/.pi/agent/CLAUDE.md", "instructions");
        let context = get_pi_context(&fx.ctx(), String::new());
        let items: Vec<_> = context.sections.iter().flat_map(|s| &s.items).collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "CLAUDE.md");
    }

    #[test]
    fn pi_context_is_conventional_read_only_resources() {
        let fx = Fixture::new();
        fx.write("home/.claude/CLAUDE.md", "not Pi");
        let project = fx.dir.path().join("project");
        fs::create_dir(&project).unwrap();
        assert!(!get_pi_context(&fx.ctx(), project.to_string_lossy().into()).present);
        for (path, content) in [
            ("home/.pi/agent/AGENTS.md", "suppressed"),
            ("home/.pi/agent/AGENTS.override.md", "instructions"),
            ("home/.pi/agent/auth.json", "secret"),
            ("home/.pi/agent/settings.json", "secret"),
            ("home/.pi/agent/skills/test/SKILL.md", "skill"),
            ("home/.pi/agent/prompts/review.md", "prompt"),
            (
                "home/.pi/agent/extensions/a.ts",
                "throw new Error('must not run')",
            ),
            ("project/AGENTS.MD", "project"),
            ("project/.pi/SYSTEM.md", "system"),
            ("project/.pi/APPEND_SYSTEM.md", "append"),
            ("project/.pi/skills/p/SKILL.md", "skill"),
            ("project/.pi/prompts/p.md", "prompt"),
            ("project/.pi/extensions/e/index.js", "extension"),
        ] {
            fx.write(path, content);
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                fx.home().join(".pi/agent/prompts"),
                project.join(".pi/prompts/link"),
            )
            .unwrap();
        }
        let c = get_pi_context(&fx.ctx(), project.to_string_lossy().into());
        assert!(c.present);
        let items: Vec<_> = c.sections.iter().flat_map(|s| &s.items).collect();
        assert_eq!(items.len(), 10);
        assert!(items.iter().all(|i| !["auth.json", "settings.json"]
            .iter()
            .any(|name| i.name.eq_ignore_ascii_case(name))));
        let global_instructions = c
            .sections
            .iter()
            .find(|s| s.title == "Global instructions")
            .unwrap();
        assert_eq!(global_instructions.items.len(), 1);
        assert_eq!(global_instructions.items[0].name, "AGENTS.override.md");
        let path_key = |p: &Path| project_key(&p.to_string_lossy());
        assert_eq!(
            project_key(&global_instructions.items[0].path),
            path_key(&fx.home().join(".pi/agent/AGENTS.override.md"))
        );
        assert!(items
            .iter()
            .all(|i| project_key(&i.path) != path_key(&fx.home().join(".claude/CLAUDE.md"))));
        // The Project's file remains visible even when a case-insensitive volume
        // reports AGENTS.MD through the AGENTS.md spelling alias.
        let project_instruction =
            crate::last_line::open_no_follow(&project.join("AGENTS.MD")).unwrap();
        let project_instruction_id = crate::last_line::file_id(&project_instruction).unwrap();
        assert!(items.iter().any(|item| {
            item.detail.starts_with("Project")
                && crate::last_line::open_no_follow(Path::new(&item.path))
                    .and_then(|file| crate::last_line::file_id(&file))
                    == Some(project_instruction_id)
        }));
        assert_eq!(
            fs::read_to_string(fx.home().join(".pi/agent/auth.json")).unwrap(),
            "secret"
        );
    }
}

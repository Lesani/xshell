//! One Project's past Claude Code and Codex sessions, newest first and paged: the answer to
//! `get_project_sessions` (capability `project.sessions`), which a Mobile's drawer lists
//! and resumes from.
//!
//! Only the agents a Mobile may resume are listed. Candidates are found by `stat` alone:
//! - Claude Code: the regular `*.jsonl` files in the Project's directory under
//!   `~/.claude/projects` (the grouping `claude --resume` uses);
//! - Codex: the regular rollouts whose first line names exactly this cwd, one per session id
//!   (the newest file, then the greatest path), read through [`codex::rollouts`]'s cache;
//!   their renames come from [`codex::session_names_for`], which reads a bounded window.
//!
//! The order is the file's mtime (newest first), then agent, then id. It can differ slightly
//! from the Desktop's, which orders by the last message's timestamp. Only the sessions of the
//! page are read, and of each at most [`HEAD_BYTES`] from the start and [`TAIL_BYTES`] from
//! the end; the result is cached by path, mtime and length.

use crate::claude::{encode_project_name, user_prompt};
use crate::codex::{self, bounded_insert};
use crate::ctx::HostCtx;
use crate::sessions::valid_session_id;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use xshell_protocol::msg::{
    PastSession, PastSessionsCursor, PastSessionsPage, PAST_SESSIONS_PAGE_DEFAULT,
    PAST_SESSIONS_PAGE_MAX, PAST_SESSION_BRANCH_MAX_CHARS, PAST_SESSION_TITLE_MAX_CHARS,
};

/// The most read from the start of a session's file.
pub(crate) const HEAD_BYTES: u64 = 512 * 1024;
/// The most read from the end of a session's file, past what the head covered.
pub(crate) const TAIL_BYTES: u64 = 128 * 1024;
/// The most session summaries cached; past that an entry is evicted.
const SUMMARY_CACHE_MAX: usize = 4096;

const CLAUDE: &str = "claude";
const CODEX: &str = "codex";

/// A session found by `stat`, before its file is read.
struct Candidate {
    agent: &'static str,
    id: String,
    path: PathBuf,
    modified_ms: u64,
}

impl Candidate {
    /// Whether this comes after `c` in page order.
    fn after(&self, c: &PastSessionsCursor) -> bool {
        self.modified_ms < c.modified_ms
            || self.modified_ms == c.modified_ms
                && (self.agent, self.id.as_str()) > (c.agent.as_str(), c.id.as_str())
    }
}

/// What a session's file says, before the Codex names are applied.
#[derive(Clone, Default, Debug, PartialEq)]
struct Summary {
    title: String,
    message_count: u32,
    git_branch: String,
}

/// The page of `cwd`'s past sessions after `before` (from the newest when `None`), at most
/// `limit` long (default 50, clamped to `1..=PAST_SESSIONS_PAGE_MAX`).
pub fn project_sessions(
    ctx: &HostCtx,
    cwd: &str,
    limit: Option<u32>,
    before: Option<&PastSessionsCursor>,
) -> PastSessionsPage {
    let limit = limit
        .unwrap_or(PAST_SESSIONS_PAGE_DEFAULT)
        .clamp(1, PAST_SESSIONS_PAGE_MAX) as usize;
    let mut all = if cwd.is_empty() {
        vec![]
    } else {
        let mut v = claude_candidates(ctx, cwd);
        v.extend(codex_candidates(ctx, cwd));
        v
    };
    all.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| a.agent.cmp(b.agent))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut rest = all
        .into_iter()
        .filter(|c| before.is_none_or(|b| c.after(b)));
    let page: Vec<Candidate> = rest.by_ref().take(limit).collect();
    let more = rest.next().is_some();

    let codex_ids: Vec<&str> = page
        .iter()
        .filter(|c| c.agent == CODEX)
        .map(|c| c.id.as_str())
        .collect();
    let names = codex::session_names_for(ctx, &codex_ids);
    let sessions: Vec<PastSession> = page
        .into_iter()
        .map(|c| {
            let s = summary(&c.path, c.agent).unwrap_or_default();
            let title = match names.get(&c.id).filter(|_| c.agent == CODEX) {
                Some(name) => name.clone(),
                None => s.title,
            };
            let branch = clip(&s.git_branch, PAST_SESSION_BRANCH_MAX_CHARS);
            PastSession {
                title: clip(&title, PAST_SESSION_TITLE_MAX_CHARS),
                message_count: s.message_count,
                git_branch: (!branch.is_empty()).then_some(branch),
                modified_ms: c.modified_ms,
                agent: c.agent.to_string(),
                id: c.id,
            }
        })
        .collect();
    let next = if more {
        sessions.last().map(PastSession::cursor)
    } else {
        None
    };
    PastSessionsPage { sessions, next }
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Whitespace collapsed, at most `max` characters.
fn clip(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (n, w) in s.split_whitespace().enumerate() {
        if n > 0 {
            out.push(' ');
        }
        out.push_str(w);
        if out.chars().count() >= max {
            break;
        }
    }
    out.chars().take(max).collect()
}

fn claude_candidates(ctx: &HostCtx, cwd: &str) -> Vec<Candidate> {
    let Some(dir) = ctx
        .claude_projects_dir()
        .map(|d| d.join(encode_project_name(cwd)))
    else {
        return vec![];
    };
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            if !e.file_type().ok()?.is_file() {
                return None;
            }
            let p = e.path();
            if p.extension()? != "jsonl" {
                return None;
            }
            let id = p.file_stem()?.to_str()?.to_string();
            if !valid_session_id(&id) {
                return None;
            }
            let modified_ms = millis(e.metadata().ok()?.modified().ok()?);
            Some(Candidate {
                agent: CLAUDE,
                id,
                path: p,
                modified_ms,
            })
        })
        .collect()
}

fn codex_candidates(ctx: &HostCtx, cwd: &str) -> Vec<Candidate> {
    // One rollout per session id: the newest, then the greatest path.
    let mut by_id: HashMap<String, codex::Rollout> = HashMap::new();
    for r in codex::rollouts(ctx) {
        if r.meta.cwd != cwd || !valid_session_id(&r.meta.id) {
            continue;
        }
        match by_id.get(&r.meta.id) {
            Some(have) if (have.modified, &have.path) >= (r.modified, &r.path) => {}
            _ => {
                by_id.insert(r.meta.id.clone(), r);
            }
        }
    }
    by_id
        .into_iter()
        .map(|(id, r)| Candidate {
            agent: CODEX,
            id,
            modified_ms: millis(r.modified),
            path: r.path,
        })
        .collect()
}

type SummaryCache = HashMap<PathBuf, (SystemTime, u64, Summary)>;

fn summary_cache() -> &'static Mutex<SummaryCache> {
    static CACHE: OnceLock<Mutex<SummaryCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The [`Summary`] of the session file at `path`, cached by path, mtime and length.
fn summary(path: &Path, agent: &str) -> Option<Summary> {
    let mut f = crate::last_line::open_no_follow(path)?;
    let m = f.metadata().ok()?;
    let (modified, len) = (m.modified().ok()?, m.len());
    if let Ok(cache) = summary_cache().lock() {
        if let Some((t, l, s)) = cache.get(path) {
            if *t == modified && *l == len {
                return Some(s.clone());
            }
        }
    }
    let (head, tail) = read_windows(&mut f, len).ok()?;
    let mut lines = json_lines(&head).chain(json_lines(&tail));
    let s = if agent == CODEX {
        codex_summary(&mut lines)
    } else {
        claude_summary(&mut lines)
    };
    if let Ok(mut cache) = summary_cache().lock() {
        bounded_insert(
            &mut cache,
            SUMMARY_CACHE_MAX,
            path.to_path_buf(),
            (modified, len, s.clone()),
        );
    }
    Some(s)
}

/// The whole lines of a file `len` bytes long that lie in its first [`HEAD_BYTES`] and its
/// last [`TAIL_BYTES`]: never more than those two windows are read.
fn read_windows(f: &mut File, len: u64) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut head = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.by_ref().take(HEAD_BYTES).read_to_end(&mut head)?;
    if (head.len() as u64) < HEAD_BYTES || head.len() as u64 >= len {
        // The whole file.
        return Ok((head, vec![]));
    }
    // Only whole lines: the head ends at its last newline.
    let head_end = head.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    head.truncate(head_end);
    let start = (head_end as u64).max(len.saturating_sub(TAIL_BYTES));
    let mut tail = Vec::new();
    f.seek(SeekFrom::Start(start))?;
    f.take(TAIL_BYTES).read_to_end(&mut tail)?;
    if start > head_end as u64 {
        // Started inside a line: drop its rest.
        let cut = tail
            .iter()
            .position(|b| *b == b'\n')
            .map_or(tail.len(), |i| i + 1);
        tail.drain(..cut);
    }
    Ok((head, tail))
}

fn json_lines(buf: &[u8]) -> impl Iterator<Item = serde_json::Value> + '_ {
    buf.split(|b| *b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
}

/// Title precedence as in `claude::parse_session`: `/rename`, then a branch's agent name, then
/// Claude's summary, then the first prompt.
fn claude_summary(lines: &mut dyn Iterator<Item = serde_json::Value>) -> Summary {
    let (mut custom, mut agent_name, mut ai, mut first) =
        (String::new(), String::new(), String::new(), String::new());
    let mut s = Summary::default();
    for json in lines {
        if let Some(b) = json.get("gitBranch").and_then(|v| v.as_str()) {
            if !b.is_empty() {
                s.git_branch = b.to_string();
            }
        }
        let text = |k: &str| json.get(k).and_then(|t| t.as_str()).map(str::to_string);
        match json.get("type").and_then(|t| t.as_str()) {
            Some("custom-title") => custom = text("customTitle").unwrap_or(custom),
            Some("agent-name") => agent_name = text("agentName").unwrap_or(agent_name),
            Some("ai-title") => ai = text("aiTitle").unwrap_or(ai),
            Some("human" | "user") => {
                if let Some(t) = user_prompt(&json) {
                    s.message_count = s.message_count.saturating_add(1);
                    if first.is_empty() {
                        first = t;
                    }
                }
            }
            _ => {}
        }
    }
    s.title = [custom, agent_name, ai, first]
        .into_iter()
        .find(|t| !t.trim().is_empty())
        .unwrap_or_default();
    s
}

/// The first prompt is the title; a rename in `session_index.jsonl` is applied on listing.
fn codex_summary(lines: &mut dyn Iterator<Item = serde_json::Value>) -> Summary {
    let mut s = Summary::default();
    for json in lines {
        let Some(payload) = json.get("payload") else {
            continue;
        };
        match json.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => {
                if let Some(b) = payload
                    .get("git")
                    .and_then(|g| g.get("branch"))
                    .and_then(|v| v.as_str())
                {
                    s.git_branch = b.to_string();
                }
            }
            Some("event_msg")
                if payload.get("type").and_then(|v| v.as_str()) == Some("user_message") =>
            {
                s.message_count = s.message_count.saturating_add(1);
                if s.title.is_empty() {
                    if let Some(m) = payload.get("message").and_then(|v| v.as_str()) {
                        s.title = m.trim().chars().take(120).collect();
                    }
                }
            }
            _ => {}
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use serde_json::{json, Value};
    use std::time::Duration;

    fn at(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn claude(fx: &Fixture, cwd: &str, sid: &str, ms: u64, lines: &[Value]) -> PathBuf {
        let p = fx.write_jsonl(
            format!(
                "home/.claude/projects/{}/{sid}.jsonl",
                encode_project_name(cwd)
            ),
            lines,
        );
        fx.set_mtime(&p, at(ms));
        p
    }

    fn prompt(text: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": text}})
    }

    fn claude_session(fx: &Fixture, cwd: &str, sid: &str, ms: u64) -> PathBuf {
        claude(
            fx,
            cwd,
            sid,
            ms,
            &[
                json!({"type": "user", "cwd": cwd, "gitBranch": "main",
                       "message": {"role": "user", "content": format!("prompt {sid}")}}),
                json!({"type": "user", "message": {"role": "user",
                       "content": [{"type": "tool_result", "content": "x"}]}}),
                prompt("again"),
            ],
        )
    }

    fn codex_at(fx: &Fixture, rel: &str, cwd: &str, sid: &str, ms: u64) -> PathBuf {
        let p = fx.write_jsonl(
            format!("home/.codex/sessions/{rel}"),
            &[
                json!({"type": "session_meta",
                       "payload": {"id": sid, "cwd": cwd, "git": {"branch": "dev"}}}),
                json!({"type": "event_msg",
                       "payload": {"type": "user_message", "message": format!("codex {sid}")}}),
            ],
        );
        fx.set_mtime(&p, at(ms));
        p
    }

    fn codex_session(fx: &Fixture, cwd: &str, sid: &str, ms: u64) -> PathBuf {
        codex_at(fx, &format!("2026/01/02/rollout-{sid}.jsonl"), cwd, sid, ms)
    }

    fn ids(p: &PastSessionsPage) -> Vec<(&str, &str)> {
        p.sessions
            .iter()
            .map(|s| (s.agent.as_str(), s.id.as_str()))
            .collect()
    }

    /// Every page from the start, `limit` at a time.
    fn all_pages(ctx: &HostCtx, cwd: &str, limit: u32) -> Vec<PastSessionsPage> {
        let mut pages = vec![project_sessions(ctx, cwd, Some(limit), None)];
        while let Some(c) = pages.last().unwrap().next.clone() {
            pages.push(project_sessions(ctx, cwd, Some(limit), Some(&c)));
            assert!(pages.len() < 100, "no end");
        }
        pages
    }

    #[test]
    fn project_sessions_orders_by_mtime_and_pages() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        claude_session(&fx, cwd, "c1", 1_000);
        claude_session(&fx, cwd, "c2", 3_000);
        claude_session(&fx, cwd, "c3", 5_000);
        codex_session(&fx, cwd, "x1", 2_000);
        codex_session(&fx, cwd, "x2", 4_000);
        let ctx = fx.ctx();

        let first = project_sessions(&ctx, cwd, Some(2), None);
        assert_eq!(ids(&first), [("claude", "c3"), ("codex", "x2")]);
        let s = &first.sessions[0];
        assert_eq!(s.title, "prompt c3");
        // The tool result is not a prompt.
        assert_eq!(s.message_count, 2);
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.modified_ms, 5_000);
        let x = &first.sessions[1];
        assert_eq!(
            (x.title.as_str(), x.message_count, x.git_branch.as_deref()),
            ("codex x2", 1, Some("dev"))
        );
        assert_eq!(first.next, Some(x.cursor()));

        let pages = all_pages(&ctx, cwd, 2);
        let got: Vec<_> = pages.iter().flat_map(ids).collect();
        assert_eq!(
            got,
            [
                ("claude", "c3"),
                ("codex", "x2"),
                ("claude", "c2"),
                ("codex", "x1"),
                ("claude", "c1")
            ]
        );
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[2].next, None);
        // A full page that is also the last one has no `next`.
        assert_eq!(project_sessions(&ctx, cwd, Some(5), None).next, None);
        // Default page size holds all five.
        assert_eq!(project_sessions(&ctx, cwd, None, None).sessions.len(), 5);
        // No such Project, and no cwd at all.
        assert!(project_sessions(&ctx, "/work/none", None, None)
            .sessions
            .is_empty());
        assert!(project_sessions(&ctx, "", None, None).sessions.is_empty());
    }

    #[test]
    fn project_sessions_equal_mtimes_and_duplicate_rollouts() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        for id in ["b", "a"] {
            claude_session(&fx, cwd, id, 7_000);
            codex_session(&fx, cwd, id, 7_000);
        }
        // Session "d" has two rollouts: the newer one counts.
        codex_at(&fx, "2026/01/01/rollout-old-d.jsonl", cwd, "d", 6_000);
        let newer = codex_at(&fx, "2026/01/03/rollout-new-d.jsonl", cwd, "d", 6_500);
        fs::write(
            &newer,
            [
                json!({"type": "session_meta", "payload": {"id": "d", "cwd": cwd}}).to_string(),
                json!({"type": "event_msg",
                       "payload": {"type": "user_message", "message": "the newer"}})
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        fx.set_mtime(&newer, at(6_500));
        // Session "e" has two rollouts with the same mtime: the greater path counts.
        codex_at(&fx, "2026/01/01/rollout-e.jsonl", cwd, "e", 5_000);
        codex_at(&fx, "2026/01/02/rollout-e.jsonl", cwd, "e", 5_000);
        let ctx = fx.ctx();
        let want = [
            ("claude", "a"),
            ("claude", "b"),
            ("codex", "a"),
            ("codex", "b"),
            ("codex", "d"),
            ("codex", "e"),
        ];
        let page = project_sessions(&ctx, cwd, None, None);
        assert_eq!(ids(&page), want);
        assert_eq!(page.sessions[4].title, "the newer");
        // One at a time across equal mtimes: nothing skipped, nothing repeated.
        let pages = all_pages(&ctx, cwd, 1);
        let got: Vec<_> = pages.iter().flat_map(ids).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn project_sessions_codex_exact_cwd() {
        let fx = Fixture::new();
        // Both encode to the same Claude directory name.
        assert_eq!(encode_project_name("/p/a-b"), encode_project_name("/p/a/b"));
        codex_session(&fx, "/p/a-b", "dash", 2_000);
        codex_session(&fx, "/p/a/b", "slash", 1_000);
        codex_session(&fx, "/p/a/b/", "trailing", 1_500);
        let ctx = fx.ctx();
        assert_eq!(
            ids(&project_sessions(&ctx, "/p/a/b", None, None)),
            [("codex", "slash")]
        );
        assert_eq!(
            ids(&project_sessions(&ctx, "/p/a-b", None, None)),
            [("codex", "dash")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_sessions_skips_symlinks_and_bad_ids() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        claude_session(&fx, cwd, "good", 1_000);
        let outside = fx.write_jsonl("outside/x.jsonl", &[prompt("secret")]);
        let dir = fx
            .home()
            .join(".claude/projects")
            .join(encode_project_name(cwd));
        std::os::unix::fs::symlink(&outside, dir.join("link.jsonl")).unwrap();
        claude_session(&fx, cwd, "-x", 2_000);
        claude_session(&fx, cwd, "a.b", 2_000);
        codex_session(&fx, cwd, "cgood", 1_000);
        codex_session(&fx, cwd, "-y", 2_000);
        let rollout = fx.write_jsonl(
            "outside/rollout.jsonl",
            &[json!({"type": "session_meta", "payload": {"id": "clink", "cwd": cwd}})],
        );
        std::os::unix::fs::symlink(
            &rollout,
            fx.home()
                .join(".codex/sessions/2026/01/02/rollout-clink.jsonl"),
        )
        .unwrap();
        // A symlinked date directory is not walked either.
        let elsewhere = fx.dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::copy(&rollout, elsewhere.join("rollout-viadir.jsonl")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, fx.home().join(".codex/sessions/2027")).unwrap();
        let page = project_sessions(&fx.ctx(), cwd, None, None);
        assert_eq!(ids(&page), [("claude", "good"), ("codex", "cgood")]);
    }

    #[test]
    fn project_sessions_limit_clamped() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        for i in 0..205u64 {
            claude(&fx, cwd, &format!("s{i:03}"), 1_000 + i, &[prompt("p")]);
        }
        let ctx = fx.ctx();
        let one = project_sessions(&ctx, cwd, Some(0), None);
        assert_eq!(ids(&one), [("claude", "s204")]);
        assert!(one.next.is_some());
        let max = project_sessions(&ctx, cwd, Some(10_000), None);
        assert_eq!(max.sessions.len(), PAST_SESSIONS_PAGE_MAX as usize);
        let rest = project_sessions(&ctx, cwd, Some(10_000), max.next.as_ref());
        assert_eq!(rest.sessions.len(), 5);
        assert_eq!(rest.next, None);
        assert_eq!(
            project_sessions(&ctx, cwd, None, None).sessions.len(),
            PAST_SESSIONS_PAGE_DEFAULT as usize
        );
    }

    #[test]
    fn project_sessions_only_claude_and_codex() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        claude_session(&fx, cwd, "c1", 1_000);
        // An opencode session in the same cwd is not listed.
        let dir = fx.home().join(".local/share/opencode");
        fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT, title TEXT, directory TEXT, model TEXT, \
             version TEXT, time_created INTEGER, time_updated INTEGER, tokens_input INTEGER, \
             tokens_output INTEGER, tokens_reasoning INTEGER, tokens_cache_read INTEGER, \
             tokens_cache_write INTEGER, parent_id TEXT, time_archived INTEGER);
             CREATE TABLE message (session_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('oc1', 't', ?1, NULL, '1.0', 1, 1, 0, 0, 0, 0, 0, NULL, NULL)",
            rusqlite::params![cwd],
        )
        .unwrap();
        // The other agents do know these sessions.
        let all = crate::sessions::get_sessions(&fx.ctx(), encode_project_name(cwd));
        assert!(all.iter().any(|s| s.agent == "opencode"), "{all:?}");
        let page = project_sessions(&fx.ctx(), cwd, None, None);
        assert_eq!(ids(&page), [("claude", "c1")]);
    }

    #[test]
    fn titles_precedence_clipping_and_codex_renames() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        let long = "word ".repeat(100);
        claude(
            &fx,
            cwd,
            "named",
            4_000,
            &[
                prompt("first"),
                json!({"type": "ai-title", "aiTitle": "summary"}),
                json!({"type": "custom-title", "customTitle": "  my\n name  "}),
            ],
        );
        claude(
            &fx,
            cwd,
            "summarised",
            3_000,
            &[
                prompt("first"),
                json!({"type": "ai-title", "aiTitle": "summary"}),
            ],
        );
        claude(&fx, cwd, "long", 2_000, &[prompt(&long)]);
        claude(
            &fx,
            cwd,
            "untitled",
            1_000,
            &[json!({"type": "summary", "gitBranch": "b".repeat(300)})],
        );
        codex_session(&fx, cwd, "renamed", 900);
        codex_session(&fx, cwd, "plain", 800);
        fx.write(
            "home/.codex/session_index.jsonl",
            r#"{"id":"renamed","thread_name":"index name"}"#,
        );
        let ctx = fx.ctx();
        let page = project_sessions(&ctx, cwd, None, None);
        let titles: Vec<_> = page.sessions.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "my name",
                "summary",
                long.trim().chars().take(120).collect::<String>().trim(),
                "",
                "index name",
                "codex plain"
            ]
        );
        assert_eq!(page.sessions[3].message_count, 0);
        assert_eq!(
            page.sessions[3].git_branch.as_deref().map(str::len),
            Some(PAST_SESSION_BRANCH_MAX_CHARS)
        );
        // A rename that lands only in the index (the rollout and its cache unchanged) shows on
        // the next listing.
        fx.write(
            "home/.codex/session_index.jsonl",
            r#"{"id":"renamed","thread_name":"index name"}
{"id":"plain","thread_name":"renamed later"}"#,
        );
        let page = project_sessions(&ctx, cwd, None, None);
        assert_eq!(page.sessions[5].title, "renamed later");
        // A rename is clipped too.
        fx.write(
            "home/.codex/session_index.jsonl",
            format!(r#"{{"id":"plain","thread_name":"{}"}}"#, "n".repeat(500)),
        );
        let page = project_sessions(&ctx, cwd, None, None);
        assert_eq!(
            page.sessions[5].title.chars().count(),
            PAST_SESSION_TITLE_MAX_CHARS
        );
        assert_eq!(clip("  a \n\t b  ", 10), "a b");
        assert_eq!(clip("abcdef", 3), "abc");
    }

    #[cfg(unix)]
    #[test]
    fn rename_index_is_confined() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        codex_session(&fx, cwd, "x1", 1_000);
        let index = fx.home().join(".codex/session_index.jsonl");
        let title = |fx: &Fixture| {
            project_sessions(&fx.ctx(), cwd, None, None).sessions[0]
                .title
                .clone()
        };
        // A symlink to a file outside Codex's storage is not followed.
        let outside = fx.write(
            "outside/index.jsonl",
            r#"{"id":"x1","thread_name":"outside"}"#,
        );
        std::os::unix::fs::symlink(&outside, &index).unwrap();
        assert_eq!(title(&fx), "codex x1");
        // A FIFO is not waited on: the page comes back at once, without renames.
        fs::remove_file(&index).unwrap();
        let c = std::ffi::CString::new(index.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = fx.ctx();
        std::thread::spawn(move || {
            let _ = tx.send(
                project_sessions(&ctx, cwd, None, None).sessions[0]
                    .title
                    .clone(),
            );
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("not held by the FIFO");
        assert_eq!(got, "codex x1");
        // A regular index applies.
        fs::remove_file(&index).unwrap();
        fx.write(
            "home/.codex/session_index.jsonl",
            r#"{"id":"x1","thread_name":"inside"}"#,
        );
        assert_eq!(title(&fx), "inside");
    }

    #[test]
    fn oversized_rename_index_reads_only_its_end() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        codex_session(&fx, cwd, "early", 2_000);
        codex_session(&fx, cwd, "late", 1_000);
        // About 32 MiB: a rename of "early" at the start, filler, a rename of "late" at the end.
        let filler = format!(r#"{{"id":"other","thread_name":"{}"}}"#, "f".repeat(1000)) + "\n";
        let mut body = String::from(r#"{"id":"early","thread_name":"too far back"}"#);
        body.push('\n');
        body.push_str(&filler.repeat((32 << 20) / filler.len()));
        body.push_str(r#"{"id":"late","thread_name":"recent"}"#);
        let index = fx.write("home/.codex/session_index.jsonl", body);
        assert!(fs::metadata(&index).unwrap().len() > 6 * codex::INDEX_TAIL_BYTES);
        let started = std::time::Instant::now();
        let page = project_sessions(&fx.ctx(), cwd, None, None);
        let elapsed = started.elapsed();
        let titles: Vec<_> = page.sessions.iter().map(|s| s.title.as_str()).collect();
        // Only the end is read: the rename at the start is out of the window.
        assert_eq!(titles, ["codex early", "recent"]);
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        // The window itself is bounded.
        let mut f = File::open(&index).unwrap();
        let len = f.metadata().unwrap().len();
        let tail = codex::read_tail(&mut f, len, codex::INDEX_TAIL_BYTES).unwrap();
        assert!(tail.len() as u64 <= codex::INDEX_TAIL_BYTES);
        assert!(tail.starts_with(b"{"));
    }

    #[test]
    fn oversized_session_reads_only_head_and_tail() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        let filler = json!({"type": "assistant", "message": {"content": "x".repeat(1000)}});
        let mut lines = vec![prompt("first prompt")];
        // About 4 MiB in the middle, with prompts that are not read.
        for i in 0..4000 {
            lines.push(if i % 100 == 0 {
                prompt("middle")
            } else {
                filler.clone()
            });
        }
        lines.push(prompt("late"));
        lines.push(json!({"type": "custom-title", "customTitle": "renamed at the end"}));
        let p = claude(&fx, cwd, "big", 1_000, &lines);
        let len = fs::metadata(&p).unwrap().len();
        assert!(len > 4 * HEAD_BYTES, "{len}");

        let (head, tail) = read_windows(&mut File::open(&p).unwrap(), len).unwrap();
        assert!(head.len() as u64 <= HEAD_BYTES && tail.len() as u64 <= TAIL_BYTES);
        // Whole lines only.
        assert!(head.ends_with(b"\n"));
        assert!(lines_of(&tail).all(|l| serde_json::from_slice::<Value>(l).is_ok()));

        let s = &project_sessions(&fx.ctx(), cwd, None, None).sessions[0];
        assert_eq!(s.title, "renamed at the end");
        // A lower bound: the first prompt, the middle ones in the head, and the last one.
        let all = 2 + 40;
        assert!(
            s.message_count >= 2 && s.message_count < all,
            "{}",
            s.message_count
        );

        // A small file is read whole.
        let small = claude(&fx, cwd, "small", 2_000, &[prompt("a"), prompt("b")]);
        let len = fs::metadata(&small).unwrap().len();
        let (head, tail) = read_windows(&mut File::open(&small).unwrap(), len).unwrap();
        assert_eq!(head.len() as u64, len);
        assert!(tail.is_empty());

        // A first line longer than the head: only the tail's whole lines count.
        let huge =
            json!({"type": "user", "message": {"content": "y".repeat(HEAD_BYTES as usize * 2)}});
        let p = claude(&fx, "/work/huge", "h", 1_000, &[huge, prompt("after")]);
        let s = &project_sessions(&fx.ctx(), "/work/huge", None, None).sessions[0];
        assert_eq!((s.title.as_str(), s.message_count), ("after", 1));
        let len = fs::metadata(&p).unwrap().len();
        let (head, tail) = read_windows(&mut File::open(&p).unwrap(), len).unwrap();
        assert!(head.is_empty() && tail.len() as u64 <= TAIL_BYTES);
    }

    fn lines_of(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
        buf.split(|b| *b == b'\n').filter(|l| !l.is_empty())
    }

    #[test]
    fn summaries_follow_file_changes() {
        let fx = Fixture::new();
        let cwd = "/work/alpha";
        let p = claude(&fx, cwd, "s", 1_000, &[prompt("one")]);
        let ctx = fx.ctx();
        let count =
            |ctx: &HostCtx| project_sessions(ctx, cwd, None, None).sessions[0].message_count;
        assert_eq!(count(&ctx), 1);
        fs::write(&p, format!("{}\n{}\n", prompt("one"), prompt("two"))).unwrap();
        fx.set_mtime(&p, at(2_000));
        assert_eq!(count(&ctx), 2);
    }

    #[test]
    fn bounded_cache_evicts() {
        let mut m: HashMap<PathBuf, u32> = HashMap::new();
        for i in 0..10 {
            bounded_insert(&mut m, 3, PathBuf::from(i.to_string()), i);
        }
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(Path::new("9")), Some(&9));
        // Replacing a present key evicts nothing.
        bounded_insert(&mut m, 3, PathBuf::from("9"), 99);
        assert_eq!(m.len(), 3);
    }
}

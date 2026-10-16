//! An agent Terminal's conversation for the Chat View (capability `session.stream`): which
//! session file a launch spec shows, the [`ChatEntry`]s of its lines, and bounded reads of it
//! backwards (a page) and forwards (what was appended).
//!
//! Every read is bounded: a page scans at most [`PAGE_SCAN_MAX`] bytes, an append at most
//! [`FORWARD_SCAN_MAX`], each message stays within [`CHAT_PAGE_MAX_BYTES`] (entries within
//! it less [`MESSAGE_RESERVE`]), and a line longer than [`MAX_LINE`] is skipped without being
//! held whole, also when a read resumes inside it. A page never splits a line:
//! the entries of one session line are in one page or append, or in none.

use crate::claude::encode_project_name;
use crate::ctx::HostCtx;
use crate::last_line::{file_id, open_confined};
use crate::sessions::valid_session_id;
use crate::time::iso_to_unix_ms;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use xshell_protocol::msg::{
    ChatEntry, ChatItem, Speaker, CHAT_PAGE_MAX_BYTES, CHAT_TEXT_MAX_CHARS,
    CHAT_TOOL_INPUT_MAX_CHARS, CHAT_TOOL_NAME_MAX_CHARS, CHAT_TOOL_RESULT_MAX_CHARS,
    CHAT_TOOL_SUMMARY_MAX_CHARS,
};
use xshell_protocol::LaunchSpec;

/// A session line longer than this is skipped.
pub const MAX_LINE: usize = 8 * 1024 * 1024;
/// The most bytes one page request scans. Twice [`MAX_LINE`], so every request moves its
/// cursor back by at least one line, or past part of an oversized one.
pub const PAGE_SCAN_MAX: u64 = 16 * 1024 * 1024;
/// The most bytes one forward read scans; the caller reads on when `more` is set. Twice
/// [`MAX_LINE`] too, so every read moves its cursor.
pub const FORWARD_SCAN_MAX: u64 = 16 * 1024 * 1024;
/// The most items one session line gives.
pub const LINE_MAX_ITEMS: usize = 64;
/// The longest session id a stream follows (longer ones cannot name a file anyway).
pub const SESSION_ID_MAX: usize = 200;
/// Room a page or append message keeps for its own fields (terminal, generation, session,
/// cursor, the `res` envelope): entries are collected within [`CHAT_PAGE_MAX_BYTES`] less
/// this, so the whole message stays within [`CHAT_PAGE_MAX_BYTES`].
pub const MESSAGE_RESERVE: usize = 1024;
/// Bytes checked before a forward cursor to notice a file rewritten in place.
const TAIL_MARK: u64 = 64;
const CHUNK: usize = 256 * 1024;

/// An agent whose session the Chat View reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAgent {
    Claude,
    Codex,
}

/// The agent a spec's conversation is read for: a Claude or Codex agent run directly.
pub fn chat_agent(spec: &LaunchSpec) -> Option<ChatAgent> {
    match spec.direct_agent()? {
        "claude" => Some(ChatAgent::Claude),
        "codex" => Some(ChatAgent::Codex),
        _ => None,
    }
}

/// The session a spec's conversation shows: its session id, when it is a valid one of at
/// most 200 characters.
pub fn stream_session(spec: &LaunchSpec) -> Option<&str> {
    spec.session_id
        .as_deref()
        .filter(|s| s.len() <= SESSION_ID_MAX && valid_session_id(s))
}

/// The session file of a spec, inside its agent's session storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    pub agent: ChatAgent,
    /// `~/.claude/projects` or `~/.codex/sessions`; the file must resolve inside it.
    pub root: PathBuf,
    pub path: PathBuf,
}

/// The session file `spec` runs: only for a Claude or Codex agent run directly, on a valid
/// session id. Claude: `~/.claude/projects/<Project>/<id>.jsonl`; Codex: the rollout ending in
/// `-<id>.jsonl` (found by walking `~/.codex/sessions`). Whether it may be read is up to
/// [`open`].
pub fn session_file(ctx: &HostCtx, spec: &LaunchSpec) -> Option<SessionFile> {
    session_file_within(ctx, spec, usize::MAX)
}

/// [`session_file`], looking at no more than `max_entries` directory entries for a Codex
/// rollout.
pub fn session_file_within(
    ctx: &HostCtx,
    spec: &LaunchSpec,
    max_entries: usize,
) -> Option<SessionFile> {
    let agent = chat_agent(spec)?;
    let sid = stream_session(spec)?;
    let home = ctx.home()?;
    match agent {
        ChatAgent::Claude => {
            let enc = encode_project_name(&spec.cwd);
            if enc.is_empty() {
                return None;
            }
            let root = home.join(".claude").join("projects");
            let path = root.join(enc).join(format!("{sid}.jsonl"));
            Some(SessionFile { agent, root, path })
        }
        ChatAgent::Codex => {
            let root = home.join(".codex").join("sessions");
            let path = crate::codex::rollout_for_within(ctx, sid, max_entries)?;
            Some(SessionFile { agent, root, path })
        }
    }
}

/// `sf` opened for reading if it is a regular file, not a symlink, whose real path stays in
/// its storage (see `last_line::open_confined`).
pub fn open(sf: &SessionFile) -> Option<File> {
    open_confined(&sf.root, &sf.path)
}

/// What identifies an open file on its machine (device and inode, or volume and index).
pub fn identity(f: &File) -> Option<(u64, u64)> {
    file_id(f)
}

// ── Lines to entries ──────────────────────────────────────────────────────

/// `s` cut to `max` characters; whether it was cut.
fn cap(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

/// `s` on one line (whitespace and control characters collapsed to one space, the ends
/// trimmed), at most `max` characters. Reads only as much of `s` as it needs.
fn one_line(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        if space {
            if n + 1 >= max {
                break;
            }
            out.push(' ');
            n += 1;
            space = false;
        }
        if n >= max {
            break;
        }
        out.push(c);
        n += 1;
    }
    out
}

fn text_item(from: Speaker, text: &str) -> Option<ChatItem> {
    if text.trim().is_empty() {
        return None;
    }
    let (text, truncated) = cap(text, CHAT_TEXT_MAX_CHARS);
    Some(match from {
        Speaker::User => ChatItem::User { text, truncated },
        Speaker::Agent => ChatItem::Agent { text, truncated },
    })
}

fn tool_call(call: Option<&str>, name: &str, summary: &str, input: Option<&str>) -> ChatItem {
    let input = input.map(|i| cap(i, CHAT_TOOL_INPUT_MAX_CHARS));
    ChatItem::ToolCall {
        call: call.map(|c| cap(c, CHAT_TOOL_NAME_MAX_CHARS).0),
        name: cap(name, CHAT_TOOL_NAME_MAX_CHARS).0,
        summary: one_line(summary, CHAT_TOOL_SUMMARY_MAX_CHARS),
        truncated: input.as_ref().is_some_and(|i| i.1),
        input: input.map(|i| i.0),
    }
}

fn tool_result(call: Option<&str>, text: &str, error: bool) -> ChatItem {
    let (text, truncated) = cap(text, CHAT_TOOL_RESULT_MAX_CHARS);
    ChatItem::ToolResult {
        call: call.map(|c| cap(c, CHAT_TOOL_NAME_MAX_CHARS).0),
        text,
        error,
        truncated,
    }
}

/// The text between `open` and `close` in `s`.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let from = s.find(open)? + open.len();
    let len = s[from..].find(close)?;
    Some(&s[from..from + len])
}

/// Text Claude Code wraps around a user message: a slash command shows as `/name args`;
/// command output, reminders and notifications it injects are not the user's words.
fn claude_user_text(s: &str) -> Option<String> {
    let t = s.trim();
    if let Some(name) = between(t, "<command-name>", "</command-name>") {
        let name = name.trim();
        let mut out = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/{name}")
        };
        if let Some(args) = between(t, "<command-args>", "</command-args>").map(str::trim) {
            if !args.is_empty() {
                out.push(' ');
                out.push_str(args);
            }
        }
        return Some(out);
    }
    const WRAPPERS: &[&str] = &[
        "<local-command-",
        "<system-reminder>",
        "<task-notification>",
        "<command-message>",
        "<user-prompt-submit-hook>",
    ];
    if t.is_empty() || WRAPPERS.iter().any(|w| t.starts_with(w)) {
        return None;
    }
    Some(t.to_string())
}

/// The first string among the usual "what it acts on" arguments of a tool, else the whole
/// input as compact JSON.
fn tool_summary(input: &Value) -> String {
    const KEYS: &[&str] = &[
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ];
    for k in KEYS {
        match input.get(k) {
            Some(Value::String(s)) if !s.trim().is_empty() => return s.clone(),
            Some(Value::Array(a)) if *k == "command" => {
                if let Some(c) = command_line(a) {
                    return c;
                }
            }
            _ => {}
        }
    }
    match input {
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// An argv as one command line: the script of `sh -c`/`bash -lc`, else the words joined.
fn command_line(argv: &[Value]) -> Option<String> {
    let words: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
    match words[..] {
        [] => None,
        [_, "-c" | "-lc", script] => Some(script.to_string()),
        _ => Some(words.join(" ")),
    }
}

/// The text of a Claude `tool_result` content: a string, or its text parts (images as
/// `[image]`).
fn claude_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn claude_items(json: &Value) -> Vec<ChatItem> {
    let from = match json.get("type").and_then(Value::as_str) {
        Some("user") => Speaker::User,
        Some("assistant") => Speaker::Agent,
        _ => return vec![],
    };
    let flag = |k: &str| json.get(k).and_then(Value::as_bool) == Some(true);
    if flag("isMeta") || flag("isSidechain") {
        return vec![];
    }
    let Some(content) = json.get("message").and_then(|m| m.get("content")) else {
        return vec![];
    };
    let user_text = |s: &str| match from {
        Speaker::User => claude_user_text(s),
        Speaker::Agent => Some(s.to_string()),
    };
    if let Some(s) = content.as_str() {
        return user_text(s)
            .and_then(|t| text_item(from, &t))
            .into_iter()
            .collect();
    }
    let Some(parts) = content.as_array() else {
        return vec![];
    };
    let mut items = Vec::new();
    // Adjacent text (and image) parts make one message.
    let mut text: Vec<String> = Vec::new();
    let flush = |text: &mut Vec<String>, items: &mut Vec<ChatItem>| {
        if !text.is_empty() {
            items.extend(text_item(from, &text.join("\n\n")));
            text.clear();
        }
    };
    for p in parts {
        match p.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = p.get("text").and_then(Value::as_str).and_then(user_text) {
                    text.push(t);
                }
            }
            Some("image") => text.push("[image]".into()),
            Some("tool_use") => {
                flush(&mut text, &mut items);
                let input = p.get("input").unwrap_or(&Value::Null);
                items.push(tool_call(
                    p.get("id").and_then(Value::as_str),
                    p.get("name").and_then(Value::as_str).unwrap_or("tool"),
                    &tool_summary(input),
                    (!input.is_null()).then(|| input.to_string()).as_deref(),
                ));
            }
            Some("tool_result") => {
                flush(&mut text, &mut items);
                items.push(tool_result(
                    p.get("tool_use_id").and_then(Value::as_str),
                    &claude_result_text(p.get("content")),
                    p.get("is_error").and_then(Value::as_bool) == Some(true),
                ));
            }
            // Thinking, and anything newer.
            _ => {}
        }
    }
    flush(&mut text, &mut items);
    items
}

/// The files an `apply_patch` input touches, from its `*** Add/Update/Delete File:` lines.
fn patch_files(patch: &str) -> String {
    patch
        .lines()
        .filter_map(|l| {
            ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                .iter()
                .find_map(|p| l.strip_prefix(p))
        })
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A Codex tool output: plain text, or JSON `{output, metadata: {exit_code}}`, or an object
/// `{content, success}`. A non-zero exit code or `success: false` is an error.
fn codex_output(output: Option<&Value>) -> (String, bool) {
    match output {
        Some(Value::String(s)) => {
            if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(s) {
                if let Some(out) = o.get("output").and_then(Value::as_str) {
                    let code = o
                        .get("metadata")
                        .and_then(|m| m.get("exit_code"))
                        .and_then(Value::as_i64);
                    return (out.to_string(), code.is_some_and(|c| c != 0));
                }
            }
            (s.clone(), false)
        }
        Some(Value::Object(o)) => (
            o.get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            o.get("success").and_then(Value::as_bool) == Some(false),
        ),
        _ => (String::new(), false),
    }
}

fn codex_items(json: &Value) -> Vec<ChatItem> {
    if let Some((from, text)) = crate::codex::event_message(json) {
        // A user message's images (inline `images`, attached files `local_images`) show
        // as one `[image]` each after its text, as Claude's do.
        let p = &json["payload"];
        let images = match from {
            Speaker::User => ["images", "local_images"]
                .iter()
                .filter_map(|k| p.get(k).and_then(Value::as_array))
                .map(Vec::len)
                .sum(),
            Speaker::Agent => 0,
        };
        if images == 0 {
            return text_item(from, text).into_iter().collect();
        }
        let parts: Vec<&str> = (!text.trim().is_empty())
            .then_some(text)
            .into_iter()
            .chain(std::iter::repeat_n("[image]", images))
            .collect();
        return text_item(from, &parts.join("\n\n")).into_iter().collect();
    }
    if json.get("type").and_then(Value::as_str) != Some("response_item") {
        return vec![];
    }
    let Some(p) = json.get("payload") else {
        return vec![];
    };
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    let call = s("call_id");
    let item = match s("type") {
        Some("function_call") => {
            let name = s("name").unwrap_or("tool");
            let raw = p.get("arguments");
            let args = match raw {
                Some(Value::String(a)) => serde_json::from_str(a).unwrap_or(Value::Null),
                Some(v) => v.clone(),
                None => Value::Null,
            };
            let input = match raw {
                Some(Value::String(a)) => Some(a.clone()),
                Some(v) => Some(v.to_string()),
                None => None,
            };
            tool_call(call, name, &tool_summary(&args), input.as_deref())
        }
        Some("local_shell_call") => {
            let action = p.get("action").unwrap_or(&Value::Null);
            let summary = action
                .get("command")
                .and_then(Value::as_array)
                .and_then(|a| command_line(a))
                .unwrap_or_default();
            tool_call(call, "shell", &summary, Some(&action.to_string()))
        }
        Some("custom_tool_call") => {
            let name = s("name").unwrap_or("tool");
            let input = s("input").unwrap_or_default();
            let summary = if name == "apply_patch" {
                patch_files(input)
            } else {
                input.to_string()
            };
            tool_call(call, name, &summary, Some(input))
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let (text, error) = codex_output(p.get("output"));
            tool_result(call, &text, error)
        }
        // Messages repeat the event_msg ones; reasoning is not shown.
        _ => return vec![],
    };
    vec![item]
}

/// The entries of one session line at byte `offset`, read in generation `gen`: `id` is
/// `"<gen>:<offset>"` for the first, `"<gen>:<offset>.<n>"` for the n-th after it.
pub fn items_from_line(agent: ChatAgent, json: &Value, gen: u64, offset: u64) -> Vec<ChatEntry> {
    let mut items = match agent {
        ChatAgent::Claude => claude_items(json),
        ChatAgent::Codex => codex_items(json),
    };
    items.truncate(LINE_MAX_ITEMS);
    let at_ms = json
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(iso_to_unix_ms);
    items
        .into_iter()
        .enumerate()
        .map(|(n, item)| ChatEntry {
            id: match n {
                0 => format!("{gen}:{offset}"),
                n => format!("{gen}:{offset}.{n}"),
            },
            at_ms,
            item,
        })
        .collect()
}

/// An entry's serialized size, with its separator.
fn entry_size(e: &ChatEntry) -> usize {
    serde_json::to_vec(e).map_or(0, |v| v.len()) + 1
}

fn entries_size(es: &[ChatEntry]) -> usize {
    es.iter().map(entry_size).sum()
}

/// The entries of one line that alone exceed `budget`: every entry is cut to an equal share
/// of it, so the line still goes out whole.
fn fit_line(es: &mut Vec<ChatEntry>, budget: usize) {
    if es.is_empty() || entries_size(es) <= budget {
        return;
    }
    let share = budget / es.len();
    // A JSON character takes at most 6 bytes (`\u001f`); room for the id and field names.
    let short = 64;
    let long = share.saturating_sub(6 * 3 * short + 512) / 6;
    let cut = |s: &mut String, max: usize, t: &mut bool| {
        let (c, was) = cap(s, max);
        if was {
            *s = c;
            *t = true;
        }
    };
    for e in es.iter_mut() {
        if entry_size(e) <= share {
            continue;
        }
        match &mut e.item {
            ChatItem::User { text, truncated } | ChatItem::Agent { text, truncated } => {
                cut(text, long, truncated)
            }
            ChatItem::ToolCall {
                call,
                name,
                summary,
                input,
                truncated,
            } => {
                let mut ignored = false;
                if let Some(c) = call {
                    cut(c, short, &mut ignored);
                }
                cut(name, short, &mut ignored);
                cut(summary, short, &mut ignored);
                if let Some(i) = input {
                    cut(i, long, truncated);
                }
            }
            ChatItem::ToolResult {
                call,
                text,
                truncated,
                ..
            } => {
                let mut ignored = false;
                if let Some(c) = call {
                    cut(c, short, &mut ignored);
                }
                cut(text, long, truncated);
            }
        }
    }
    // Only an id far beyond any real offset could still overflow.
    while es.len() > 1 && entries_size(es) > budget {
        es.pop();
    }
}

/// A line of Claude's conversation tree: it has a `uuid` and is not a subagent's. Claude's
/// session file is append-only: a rewind appends the new prompt with `parentUuid` naming the
/// line before the message rewound to, and the abandoned lines stay in the file before it.
struct Link {
    uuid: String,
    /// The `parentUuid` of a prompt (a user message that is not a tool result); `None` for
    /// every other line. `Some(None)`: a prompt with no parent, the start of a conversation.
    prompt: Option<Option<String>>,
}

fn claude_link(json: &Value) -> Option<Link> {
    let uuid = json.get("uuid")?.as_str()?.to_string();
    // Progress records (older Claude versions gave them uuids) are not part of the tree:
    // Claude does not parent the next line on them.
    if json.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || json.get("type").and_then(Value::as_str) == Some("progress")
    {
        return None;
    }
    let content = json.get("message").and_then(|m| m.get("content"));
    let is_prompt = json.get("type").and_then(Value::as_str) == Some("user")
        && json.get("isMeta").and_then(Value::as_bool) != Some(true)
        && match content {
            Some(Value::String(_)) => true,
            Some(Value::Array(parts)) => !parts
                .iter()
                .any(|p| p.get("type").and_then(Value::as_str) == Some("tool_result")),
            _ => false,
        };
    let prompt = is_prompt.then(|| {
        json.get("parentUuid")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    Some(Link { uuid, prompt })
}

/// Where a backwards read stands on Claude's conversation tree, carried from one page to the
/// next with its cursor. A page shows only the branch the conversation is on: the prompt it
/// last took must follow its parent, else the lines in between are an abandoned branch and are
/// skipped back to that parent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Back {
    #[default]
    Free,
    /// A prompt was taken: the next older line of the tree must be this parent (`None`: the
    /// prompt started the conversation).
    Expect(Option<String>),
    /// On an abandoned branch: every line is skipped up to the one with this uuid (`None`: to
    /// the start of the file).
    Skip(Option<String>),
}

impl Back {
    /// The state after line `link` (older than every line read so far), and whether the line
    /// is shown.
    fn step(&self, link: Option<&Link>) -> (Back, bool) {
        let Some(l) = link else {
            return (self.clone(), !matches!(self, Back::Skip(_)));
        };
        match self {
            Back::Expect(p) | Back::Skip(p) if p.as_deref() != Some(l.uuid.as_str()) => {
                (Back::Skip(p.clone()), false)
            }
            _ => match &l.prompt {
                Some(parent) => (Back::Expect(parent.clone()), true),
                None => (Back::Free, true),
            },
        }
    }
}

/// Collects whole lines' entries up to an item count and a byte budget.
struct Collect {
    agent: ChatAgent,
    gen: u64,
    limit: usize,
    budget: usize,
    count: usize,
    bytes: usize,
    lines: Vec<Vec<ChatEntry>>,
}

enum Took {
    /// The line gave no entries.
    Nothing,
    /// The line's entries were taken.
    Taken,
    /// The line's entries do not fit: it is left for the next read.
    Full,
}

impl Collect {
    fn new(agent: ChatAgent, gen: u64, limit: usize, budget: usize) -> Collect {
        Collect {
            agent,
            gen,
            limit: limit.max(1),
            budget,
            count: 0,
            bytes: 0,
            lines: Vec::new(),
        }
    }

    fn take(&mut self, start: u64, json: &Value) -> Took {
        let mut es = items_from_line(self.agent, json, self.gen, start);
        if es.is_empty() {
            return Took::Nothing;
        }
        let size = entries_size(&es);
        if self.count > 0 && (self.count + es.len() > self.limit || self.bytes + size > self.budget)
        {
            return Took::Full;
        }
        if size > self.budget {
            fit_line(&mut es, self.budget);
        }
        self.count += es.len();
        self.bytes += entries_size(&es);
        self.lines.push(es);
        Took::Taken
    }

    fn full(&self) -> bool {
        self.count >= self.limit
    }
}

fn read_at(f: &mut File, at: u64, n: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(at)).ok()?;
    let mut buf = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The end of the complete lines of a file of length `len`: just after its last newline (0
/// without one). A partial last line longer than [`MAX_LINE`] is not looked through: `len`.
pub fn complete_end(f: &mut File, len: u64) -> u64 {
    let mut pos = len;
    while pos > 0 && len - pos <= MAX_LINE as u64 {
        let n = CHUNK.min(pos as usize);
        let Some(buf) = read_at(f, pos - n as u64, n) else {
            return len;
        };
        if buf.len() != n {
            return len;
        }
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            return pos - n as u64 + i as u64 + 1;
        }
        pos -= n as u64;
    }
    if pos == 0 {
        0
    } else {
        len
    }
}

/// Bytes checked before a forward cursor: the up to 64 bytes ending at `at`.
pub fn tail_mark(f: &mut File, at: u64) -> Option<Vec<u8>> {
    let n = TAIL_MARK.min(at);
    let b = read_at(f, at - n, n as usize)?;
    (b.len() as u64 == n).then_some(b)
}

/// The lines before an offset, newest first.
struct RevLines<'a> {
    f: &'a mut File,
    chunk: usize,
    max_line: usize,
    scan_max: u64,
    /// The file offset of `buf[0]`.
    start: u64,
    /// The bytes not yet returned: the end of a line whose start is not read yet.
    buf: Vec<u8>,
    /// The line being read is longer than `max_line`: `buf` holds only its unscanned part.
    oversized: bool,
    scanned: u64,
    done: bool,
}

enum Rev {
    /// A line starting at this offset: its bytes, `None` when it was too long.
    Line(u64, Option<Vec<u8>>),
    /// The start of the file was reached.
    End,
    /// The scan budget is spent (or the file could not be read).
    Stop,
}

impl RevLines<'_> {
    fn next(&mut self) -> Rev {
        loop {
            if let Some(i) = self.buf.iter().rposition(|&b| b == b'\n') {
                let start = self.start + i as u64 + 1;
                let line = &self.buf[i + 1..];
                let line = (!self.oversized && line.len() <= self.max_line).then(|| line.to_vec());
                self.buf.truncate(i);
                self.oversized = false;
                return Rev::Line(start, line);
            }
            if self.start == 0 {
                if self.done {
                    return Rev::End;
                }
                self.done = true;
                let line = std::mem::take(&mut self.buf);
                let line = (!self.oversized && line.len() <= self.max_line).then_some(line);
                return Rev::Line(0, line);
            }
            if self.scanned >= self.scan_max {
                return Rev::Stop;
            }
            let n = self.chunk.min(self.start as usize);
            let from = self.start - n as u64;
            let Some(mut chunk) = read_at(self.f, from, n).filter(|c| c.len() == n) else {
                return Rev::Stop;
            };
            self.scanned += n as u64;
            self.start = from;
            if !self.oversized {
                chunk.extend_from_slice(&self.buf);
            }
            self.buf = chunk;
            if !self.oversized && self.buf.len() > self.max_line && !self.buf.contains(&b'\n') {
                self.oversized = true;
                self.buf.clear();
            }
        }
    }
}

/// Bounds of one read; tests shrink them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    pub chunk: usize,
    pub max_line: usize,
    pub page_scan: u64,
    pub forward_scan: u64,
    pub bytes: usize,
}

pub(crate) const BOUNDS: Bounds = Bounds {
    chunk: CHUNK,
    max_line: MAX_LINE,
    page_scan: PAGE_SCAN_MAX,
    forward_scan: FORWARD_SCAN_MAX,
    bytes: CHAT_PAGE_MAX_BYTES - MESSAGE_RESERVE,
};

/// Whether `at` is inside a line: the byte before it is not a newline. A cursor is a line
/// boundary except after an oversized line was cut short by a scan budget (or behind a
/// partial last line too long to look through). An unreadable byte counts as inside.
fn mid_line(f: &mut File, at: u64) -> bool {
    at > 0 && read_at(f, at - 1, 1).as_deref() != Some(&b"\n"[..])
}

/// What a page read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Oldest first.
    pub items: Vec<ChatEntry>,
    /// Where the next older page ends; `None` at the start of the file.
    pub before: Option<u64>,
    /// Where the read stands on the conversation tree at `before`: the next older page goes
    /// on from it.
    pub back: Back,
    /// The uuid of the newest line of the tree that was read (Claude only), for
    /// [`read_forward`] going on from `upper`.
    pub newest: Option<String>,
}

/// The newest entries before `upper` (a line boundary: [`complete_end`] or an earlier
/// page's cursor), oldest first: at most `limit` of them (more only when one line has more)
/// within the byte budget, whole lines only, and of a Claude session only the branch it is on
/// (see [`Back`]; `back` is [`Page::back`] of the page that handed out `upper`, or
/// [`Back::Free`] at the end of the file).
pub fn page(
    f: &mut File,
    agent: ChatAgent,
    gen: u64,
    upper: u64,
    limit: usize,
    back: Back,
) -> Page {
    page_with(f, agent, gen, upper, limit, back, BOUNDS)
}

pub(crate) fn page_with(
    f: &mut File,
    agent: ChatAgent,
    gen: u64,
    upper: u64,
    limit: usize,
    mut back: Back,
    b: Bounds,
) -> Page {
    let mut c = Collect::new(agent, gen, limit, b.bytes);
    let mut newest: Option<String> = None;
    // The newest line of the tree is found, or a line that could not be read came first (its
    // uuid unknown: `newest` stays `None`).
    let mut newest_done = false;
    // Above a cursor inside a line (an oversized one's), the bytes up to it are only part of
    // that line: skipped like it, never decoded.
    let fragment = mid_line(f, upper);
    let mut rl = RevLines {
        f,
        chunk: b.chunk,
        max_line: b.max_line,
        scan_max: b.page_scan,
        start: upper,
        buf: Vec::new(),
        oversized: fragment,
        scanned: 0,
        done: false,
    };
    // The start of the oldest line consumed (taken or skipped).
    let mut before = upper;
    loop {
        match rl.next() {
            Rev::End => {
                before = 0;
                break;
            }
            Rev::Stop => {
                // Inside an oversized line, the next page goes on from what was scanned.
                if rl.oversized {
                    before = rl.start;
                }
                break;
            }
            // Unreadable or too long to read: not shown. It may be the parent looked for, so
            // the branch is taken to go on here (showing too much, never hiding the branch).
            Rev::Line(start, None) => {
                back = Back::Free;
                newest_done = true;
                before = start;
            }
            // The end of the file, or of what an earlier page read: nothing.
            Rev::Line(start, Some(line)) if line.trim_ascii().is_empty() => before = start,
            Rev::Line(start, Some(line)) => {
                let Ok(json) = serde_json::from_slice::<Value>(&line) else {
                    back = Back::Free;
                    newest_done = true;
                    before = start;
                    continue;
                };
                let link = (agent == ChatAgent::Claude)
                    .then(|| claude_link(&json))
                    .flatten();
                let (next, shown) = back.step(link.as_ref());
                if !shown {
                    back = next;
                    before = start;
                    continue;
                }
                match c.take(start, &json) {
                    Took::Full => break,
                    Took::Nothing | Took::Taken => {
                        if !newest_done {
                            if let Some(l) = link {
                                newest = Some(l.uuid);
                                newest_done = true;
                            }
                        }
                        back = next;
                        before = start;
                        if c.full() {
                            break;
                        }
                    }
                }
            }
        }
    }
    let items = c.lines.into_iter().rev().flatten().collect();
    Page {
        items,
        before: (before > 0).then_some(before),
        back,
        newest,
    }
}

/// What a forward read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub items: Vec<ChatEntry>,
    /// Where the next read starts: after the last line consumed.
    pub next: u64,
    /// Stopped early (count, bytes or scan budget): read again from `next`.
    pub more: bool,
    /// Stopped at a Claude prompt that does not follow the line before it: the conversation
    /// was rewound and the lines read so far may be an abandoned branch. `next` is the
    /// prompt's start; the subscription starts over from the end of the file.
    pub fork: bool,
    /// The uuid of the newest line of the tree before `next` (Claude only), `None` when it is
    /// not known: the `last` of the next read.
    pub last: Option<String>,
}

/// The entries of the complete lines between `from` and `len`, in order, at most `limit`
/// (more only when one line has more) within the byte budget, whole lines only. A partial
/// last line is left for the next read; an oversized one is skipped, also when that leaves
/// `next` inside it. `last` is the uuid of the newest line of a Claude session's tree before
/// `from` ([`Page::newest`] or [`Forward::last`]); a prompt whose parent is not it stops the
/// read ([`Forward::fork`]).
pub fn read_forward(
    f: &mut File,
    agent: ChatAgent,
    gen: u64,
    from: u64,
    len: u64,
    limit: usize,
    last: Option<String>,
) -> Forward {
    read_forward_with(f, agent, gen, from, len, limit, last, BOUNDS)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn read_forward_with(
    f: &mut File,
    agent: ChatAgent,
    gen: u64,
    from: u64,
    len: u64,
    limit: usize,
    mut last: Option<String>,
    b: Bounds,
) -> Forward {
    let mut c = Collect::new(agent, gen, limit, b.bytes);
    let mut next = from;
    // Unconsumed bytes from `next` on (or, while skipping, from inside the long line).
    let mut buf: Vec<u8> = Vec::new();
    let mut buf_pos = from;
    // From a cursor inside a line (an oversized one's), the rest of it is skipped, never
    // decoded.
    let mut skipping = mid_line(f, from);
    let mut read_pos = from;
    let mut scanned = 0u64;
    let done = |c: Collect, next: u64, more: bool, last: Option<String>| Forward {
        items: c.lines.into_iter().flatten().collect(),
        next,
        more,
        fork: false,
        last,
    };
    loop {
        let mut cut = 0;
        while let Some(i) = buf[cut..].iter().position(|&b| b == b'\n') {
            let (s, e) = (cut, cut + i);
            let start = buf_pos + s as u64;
            let end = buf_pos + e as u64 + 1;
            cut = e + 1;
            if skipping {
                skipping = false;
                last = None;
                next = end;
                continue;
            }
            let blank = buf[s..e].trim_ascii().is_empty();
            let json = (e - s <= b.max_line)
                .then(|| serde_json::from_slice::<Value>(&buf[s..e]).ok())
                .flatten();
            match &json {
                Some(json) => {
                    let link = (agent == ChatAgent::Claude)
                        .then(|| claude_link(json))
                        .flatten();
                    if let Some(l) = &link {
                        if let (Some(parent), Some(prev)) = (&l.prompt, &last) {
                            if parent.as_deref() != Some(prev.as_str()) {
                                let mut fwd = done(c, start, true, last);
                                fwd.fork = true;
                                return fwd;
                            }
                        }
                    }
                    match c.take(start, json) {
                        Took::Full => return done(c, next, true, last),
                        Took::Nothing | Took::Taken => {}
                    }
                    if let Some(l) = link {
                        last = Some(l.uuid);
                    }
                }
                None if blank => {}
                // Unreadable or too long to read: the next prompt's parent cannot be checked.
                None => last = None,
            }
            next = end;
            if c.full() {
                return done(c, next, next < len, last);
            }
        }
        buf.drain(..cut);
        buf_pos += cut as u64;
        if buf.len() > b.max_line {
            // A line too long to hold: skip to its end, moving the cursor inside it.
            skipping = true;
            last = None;
            buf_pos += buf.len() as u64;
            buf.clear();
            next = buf_pos;
        }
        if read_pos >= len {
            return done(c, next, false, last);
        }
        if scanned >= b.forward_scan {
            return done(c, next, true, last);
        }
        let n = b.chunk.min((len - read_pos) as usize);
        let Some(chunk) = read_at(f, read_pos, n).filter(|c| !c.is_empty()) else {
            return done(c, next, false, last);
        };
        read_pos += chunk.len() as u64;
        scanned += chunk.len() as u64;
        buf.extend_from_slice(&chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use serde_json::json;
    use std::io::Write;
    use xshell_protocol::msg::{CHAT_PAGE_MAX, CHAT_TOOL_INPUT_MAX_CHARS};

    const CWD: &str = "/work/alpha";

    fn user(text: &str) -> Value {
        json!({"type": "user", "timestamp": "2026-01-01T10:00:00.250Z",
               "message": {"role": "user", "content": text}})
    }

    fn agent(text: &str) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant",
               "content": [{"type": "text", "text": text}]}})
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant",
               "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}})
    }

    fn result(id: &str, content: Value, err: bool) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": err}]}})
    }

    fn claude(v: &Value) -> Vec<ChatItem> {
        items_from_line(ChatAgent::Claude, v, 1, 0)
            .into_iter()
            .map(|e| e.item)
            .collect()
    }

    fn codex(v: &Value) -> Vec<ChatItem> {
        items_from_line(ChatAgent::Codex, v, 1, 0)
            .into_iter()
            .map(|e| e.item)
            .collect()
    }

    fn u(t: &str) -> ChatItem {
        ChatItem::User {
            text: t.into(),
            truncated: false,
        }
    }

    fn a(t: &str) -> ChatItem {
        ChatItem::Agent {
            text: t.into(),
            truncated: false,
        }
    }

    fn call(id: &str, name: &str, summary: &str, input: &str) -> ChatItem {
        ChatItem::ToolCall {
            call: Some(id.into()),
            name: name.into(),
            summary: summary.into(),
            input: Some(input.into()),
            truncated: false,
        }
    }

    fn res(id: &str, text: &str, error: bool) -> ChatItem {
        ChatItem::ToolResult {
            call: Some(id.into()),
            text: text.into(),
            error,
            truncated: false,
        }
    }

    #[test]
    fn claude_items_cover_user_agent_tool_call_result() {
        assert_eq!(claude(&user("fix the build")), vec![u("fix the build")]);
        let e = items_from_line(ChatAgent::Claude, &user("x"), 3, 77);
        assert_eq!(e[0].id, "3:77");
        assert_eq!(e[0].at_ms, Some(1_767_261_600_250));
        // Every text part, joined; one message.
        let multi = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "one"}, {"type": "thinking", "thinking": "hm"},
            {"type": "text", "text": "two"}]}});
        assert_eq!(claude(&multi), vec![a("one\n\ntwo")]);
        // Tool summaries: a Bash command, a Read path, a fallback to the JSON input.
        assert_eq!(
            claude(&tool_use(
                "t1",
                "Bash",
                json!({"command": "npm  test\n", "timeout": 5})
            )),
            vec![call(
                "t1",
                "Bash",
                "npm test",
                r#"{"command":"npm  test\n","timeout":5}"#
            )]
        );
        assert_eq!(
            claude(&tool_use("t2", "Read", json!({"file_path": "/src/a.rs"}))),
            vec![call(
                "t2",
                "Read",
                "/src/a.rs",
                r#"{"file_path":"/src/a.rs"}"#
            )]
        );
        assert_eq!(
            claude(&tool_use("t3", "TodoWrite", json!({"todos": [1]}))),
            vec![call(
                "t3",
                "TodoWrite",
                r#"{"todos":[1]}"#,
                r#"{"todos":[1]}"#
            )]
        );
        // Results: string content, array content, errors.
        assert_eq!(
            claude(&result("t1", json!("3 passed"), false)),
            vec![res("t1", "3 passed", false)]
        );
        assert_eq!(
            claude(&result(
                "t2",
                json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]),
                true
            )),
            vec![res("t2", "a\nb", true)]
        );
        // Several blocks in one entry keep their order; each gets its own id.
        let mixed = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "Let me look."},
            {"type": "tool_use", "id": "t9", "name": "Grep", "input": {"pattern": "fn main"}}]}});
        let es = items_from_line(ChatAgent::Claude, &mixed, 2, 500);
        assert_eq!(
            es.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["2:500", "2:500.1"]
        );
        assert_eq!(es[0].item, a("Let me look."));
        assert!(matches!(&es[1].item, ChatItem::ToolCall { summary, .. } if summary == "fn main"));
    }

    #[test]
    fn claude_skips_thinking_meta_sidechain_and_non_messages() {
        for v in [
            json!({"type": "assistant", "message": {"content": [
                {"type": "thinking", "thinking": "secret plan"}]}}),
            json!({"type": "user", "isMeta": true, "message": {"content": "caveat"}}),
            json!({"type": "assistant", "isSidechain": true,
                   "message": {"content": [{"type": "text", "text": "subagent"}]}}),
            json!({"type": "attachment", "attachment": {"type": "environment"}}),
            json!({"type": "system", "subtype": "turn_duration"}),
            json!({"type": "summary", "summary": "s"}),
            json!({"type": "queue-operation", "content": "done"}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "  \n"}]}}),
            json!({"type": "user"}),
            json!("just a string"),
        ] {
            assert_eq!(claude(&v), vec![], "{v}");
        }
    }

    #[test]
    fn claude_command_wrappers_and_images() {
        let cmd = "<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>PR 12</command-args>";
        assert_eq!(claude(&user(cmd)), vec![u("/review PR 12")]);
        let bare = "<command-name>clear</command-name><command-args></command-args>";
        assert_eq!(claude(&user(bare)), vec![u("/clear")]);
        for skipped in [
            "<local-command-stdout>ok</local-command-stdout>",
            "<local-command-caveat>Caveat</local-command-caveat>",
            "<system-reminder>be good</system-reminder>",
            "<task-notification>done</task-notification>",
        ] {
            assert_eq!(claude(&user(skipped)), vec![], "{skipped}");
        }
        // A reminder part next to the user's own text: only the text.
        let parts = json!({"type": "user", "message": {"content": [
            {"type": "text", "text": "<system-reminder>x</system-reminder>"},
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "data": "AAAA"}}]}});
        assert_eq!(claude(&parts), vec![u("look at this\n\n[image]")]);
        assert_eq!(
            claude(&result(
                "t1",
                json!([{"type": "image", "source": {"data": "AAAA"}}]),
                false
            )),
            vec![res("t1", "[image]", false)]
        );
    }

    fn rollout_item(payload: Value) -> Value {
        json!({"timestamp": "2026-01-01T10:00:01Z", "type": "response_item", "payload": payload})
    }

    #[test]
    fn codex_user_images_marked() {
        // Images in a user message: one marker each (inline, then attached files).
        let img = |text: &str, images: Value, local: Value| {
            json!({"timestamp": "2026-01-01T10:00:01Z", "type": "event_msg",
                   "payload": {"type": "user_message", "message": text,
                               "images": images, "local_images": local}})
        };
        assert_eq!(
            codex(&img(
                "look",
                json!(["data:image/png;base64,AAAA"]),
                json!(["/x.png"])
            )),
            vec![u("look\n\n[image]\n\n[image]")]
        );
        assert_eq!(
            codex(&img("", json!([]), json!(["/x.png"]))),
            vec![u("[image]")]
        );
        assert_eq!(
            codex(&img("plain", json!([]), json!(null))),
            vec![u("plain")]
        );
        assert_eq!(codex(&img(" ", json!(null), json!([]))), vec![]);
    }

    #[test]
    fn codex_items_from_rollout() {
        let ev = |kind: &str, text: &str| {
            json!({"timestamp": "2026-01-01T10:00:01Z", "type": "event_msg",
                   "payload": {"type": kind, "message": text}})
        };
        assert_eq!(codex(&ev("user_message", "fix it")), vec![u("fix it")]);
        assert_eq!(codex(&ev("agent_message", "Fixed.")), vec![a("Fixed.")]);
        let e = items_from_line(ChatAgent::Codex, &ev("agent_message", "x"), 1, 0);
        assert_eq!(e[0].at_ms, Some(1_767_261_601_000));
        assert_eq!(codex(&ev("token_count", "")), vec![]);
        // Shell: `bash -lc` shows its script; exec_command its cmd.
        let shell = rollout_item(
            json!({"type": "function_call", "name": "shell", "call_id": "c1",
            "arguments": r#"{"command":["bash","-lc","cargo test"],"timeout_ms":1000}"#}),
        );
        assert_eq!(
            codex(&shell),
            vec![call(
                "c1",
                "shell",
                "cargo test",
                r#"{"command":["bash","-lc","cargo test"],"timeout_ms":1000}"#
            )]
        );
        let exec = rollout_item(json!({"type": "function_call", "name": "exec_command",
            "call_id": "c2", "arguments": r#"{"cmd":"ls -la"}"#}));
        assert!(
            matches!(&codex(&exec)[0], ChatItem::ToolCall { summary, .. } if summary == "ls -la")
        );
        let local = rollout_item(json!({"type": "local_shell_call", "call_id": "c3",
            "action": {"type": "exec", "command": ["git", "status"]}}));
        assert!(matches!(&codex(&local)[0],
            ChatItem::ToolCall { name, summary, .. } if name == "shell" && summary == "git status"));
        // apply_patch lists the files it touches.
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: b.md\n+hi\n*** End Patch";
        let custom = rollout_item(json!({"type": "custom_tool_call", "name": "apply_patch",
            "call_id": "c4", "input": patch}));
        assert_eq!(
            codex(&custom),
            vec![call("c4", "apply_patch", "src/a.rs, b.md", patch)]
        );
        // Outputs: JSON-wrapped with an exit code, plain, and {content, success}.
        let wrapped = rollout_item(json!({"type": "function_call_output", "call_id": "c1",
            "output": r#"{"output":"error: 1 failed","metadata":{"exit_code":101}}"#}));
        assert_eq!(codex(&wrapped), vec![res("c1", "error: 1 failed", true)]);
        let ok = rollout_item(json!({"type": "function_call_output", "call_id": "c2",
            "output": r#"{"output":"fine","metadata":{"exit_code":0}}"#}));
        assert_eq!(codex(&ok), vec![res("c2", "fine", false)]);
        let plain = rollout_item(json!({"type": "custom_tool_call_output", "call_id": "c4",
            "output": "Success. Updated the following files"}));
        assert_eq!(
            codex(&plain),
            vec![res("c4", "Success. Updated the following files", false)]
        );
        let obj = rollout_item(json!({"type": "function_call_output", "call_id": "c5",
            "output": {"content": "denied", "success": false}}));
        assert_eq!(codex(&obj), vec![res("c5", "denied", true)]);
        // Messages repeat event_msg; reasoning and context are not shown.
        for v in [
            rollout_item(json!({"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "dup"}]})),
            rollout_item(json!({"type": "reasoning", "summary": []})),
            json!({"type": "turn_context", "payload": {"model": "gpt"}}),
            json!({"type": "session_meta", "payload": {"id": "x"}}),
        ] {
            assert_eq!(codex(&v), vec![], "{v}");
        }
    }

    #[test]
    fn caps_cut_on_char_boundary() {
        let long = "é".repeat(40 * 1024);
        let [ChatItem::User { text, truncated }] = &claude(&user(&long))[..] else {
            panic!()
        };
        assert!(*truncated);
        assert_eq!(text.chars().count(), CHAT_TEXT_MAX_CHARS);
        let exact = "é".repeat(CHAT_TEXT_MAX_CHARS);
        assert_eq!(claude(&agent(&exact)), vec![a(&exact)]);
        // Input, summary, name and call id, and a result.
        let huge = "x".repeat(3 * 1024 * 1024);
        let id = "i".repeat(500);
        let name = "n".repeat(500);
        let [ChatItem::ToolCall {
            call,
            name: n,
            summary,
            input,
            truncated,
        }] = &claude(&tool_use(&id, &name, json!({"command": huge})))[..]
        else {
            panic!()
        };
        assert!(*truncated);
        assert_eq!(
            input.as_ref().unwrap().chars().count(),
            CHAT_TOOL_INPUT_MAX_CHARS
        );
        assert_eq!(summary.chars().count(), CHAT_TOOL_SUMMARY_MAX_CHARS);
        assert_eq!(n.chars().count(), CHAT_TOOL_NAME_MAX_CHARS);
        assert_eq!(
            call.as_ref().unwrap().chars().count(),
            CHAT_TOOL_NAME_MAX_CHARS
        );
        let [ChatItem::ToolResult {
            text, truncated, ..
        }] = &claude(&result("t", json!("€".repeat(5000)), false))[..]
        else {
            panic!()
        };
        assert!(*truncated);
        assert_eq!(text.chars().count(), CHAT_TOOL_RESULT_MAX_CHARS);
        // One-line summaries collapse whitespace and stop at the cap without a trailing
        // space.
        assert_eq!(one_line("  a \n\t b  ", 10), "a b");
        assert_eq!(one_line("ab cd", 3), "ab");
        assert_eq!(one_line("abcdef", 3), "abc");
    }

    /// A session file of `lines` (one JSON value each), opened.
    fn file_of(fx: &Fixture, name: &str, lines: &[Value]) -> (PathBuf, File) {
        let p = fx.write_jsonl(format!("s/{name}.jsonl"), lines);
        let f = File::open(&p).unwrap();
        (p, f)
    }

    fn len(f: &File) -> u64 {
        f.metadata().unwrap().len()
    }

    fn texts(es: &[ChatEntry]) -> Vec<String> {
        es.iter()
            .map(|e| match &e.item {
                ChatItem::User { text, .. } | ChatItem::Agent { text, .. } => text.clone(),
                ChatItem::ToolCall { summary, .. } => format!("call {summary}"),
                ChatItem::ToolResult { text, .. } => format!("result {text}"),
            })
            .collect()
    }

    #[test]
    fn page_from_end_then_older_to_start() {
        let fx = Fixture::new();
        let lines: Vec<Value> = (0..120)
            .flat_map(|i| [agent(&format!("m{i}")), json!({"type": "progress"})])
            .collect();
        let (_, mut f) = file_of(&fx, "a", &lines);
        let n = len(&f);
        let end = complete_end(&mut f, n);
        assert_eq!(end, len(&f));
        let mut seen = Vec::new();
        let mut upper = end;
        let mut sizes = Vec::new();
        loop {
            let Page {
                items: es, before, ..
            } = page(&mut f, ChatAgent::Claude, 1, upper, 50, Back::Free);
            sizes.push(es.len());
            let mut t = texts(&es);
            t.extend(seen);
            seen = t;
            match before {
                Some(b) => {
                    assert!(b < upper);
                    upper = b;
                }
                None => break,
            }
        }
        assert_eq!(sizes, [50, 50, 20]);
        let want: Vec<String> = (0..120).map(|i| format!("m{i}")).collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn page_byte_budget_and_scan_cap() {
        let fx = Fixture::new();
        // Results at their 4 Ki-character cap: a page of 200 would be ~800 KiB.
        let big = "r".repeat(10_000);
        let lines: Vec<Value> = (0..150)
            .map(|i| result(&format!("t{i}"), json!(format!("{i} {big}")), false))
            .collect();
        let (_, mut f) = file_of(&fx, "a", &lines);
        let end = len(&f);
        let Page {
            items: es, before, ..
        } = page(
            &mut f,
            ChatAgent::Claude,
            1,
            end,
            CHAT_PAGE_MAX as usize,
            Back::Free,
        );
        assert!(es.len() < 150 && es.len() > 10, "{}", es.len());
        assert!(entries_size(&es) <= BOUNDS.bytes);
        let mut got = es.len();
        let mut upper = before.unwrap();
        loop {
            let Page {
                items: es,
                before: b,
                ..
            } = page(
                &mut f,
                ChatAgent::Claude,
                1,
                upper,
                CHAT_PAGE_MAX as usize,
                Back::Free,
            );
            assert!(entries_size(&es) <= BOUNDS.bytes);
            got += es.len();
            match b {
                Some(b) => upper = b,
                None => break,
            }
        }
        assert_eq!(got, 150);
        // The scan cap: with a small one, a page over lines that give nothing stops early
        // and the next page goes on from where it stopped.
        let mut lines: Vec<Value> = vec![agent("first")];
        lines.extend((0..200).map(|_| json!({"type": "progress", "data": "x".repeat(100)})));
        let (_, mut f) = file_of(&fx, "b", &lines);
        let b = Bounds {
            page_scan: 4 * 1024,
            max_line: 1024,
            chunk: 512,
            ..BOUNDS
        };
        let mut upper = len(&f);
        let mut rounds = 0;
        let found = loop {
            rounds += 1;
            let Page {
                items: es, before, ..
            } = page_with(&mut f, ChatAgent::Claude, 1, upper, 50, Back::Free, b);
            if !es.is_empty() {
                break texts(&es);
            }
            upper = before.expect("more to scan");
        };
        assert_eq!(found, ["first"]);
        assert!(rounds > 2, "{rounds}");
    }

    #[test]
    fn page_keeps_lines_whole() {
        let fx = Fixture::new();
        let two = |i: usize| {
            json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": format!("t{i}")},
                {"type": "tool_use", "id": format!("c{i}"), "name": "Bash",
                 "input": {"command": format!("cmd{i}")}}]}})
        };
        let lines: Vec<Value> = (0..5).map(two).collect();
        let (_, mut f) = file_of(&fx, "a", &lines);
        // Count boundary: 3 does not fit two lines of two; the older line waits.
        let n = len(&f);
        let Page {
            items: es, before, ..
        } = page(&mut f, ChatAgent::Claude, 1, n, 3, Back::Free);
        assert_eq!(texts(&es), ["t4", "call cmd4"]);
        let Page { items: es2, .. } =
            page(&mut f, ChatAgent::Claude, 1, before.unwrap(), 4, Back::Free);
        assert_eq!(texts(&es2), ["t2", "call cmd2", "t3", "call cmd3"]);
        // A single line with more items than the limit goes out whole.
        let n = len(&f);
        let Page { items: es, .. } = page(&mut f, ChatAgent::Claude, 1, n, 1, Back::Free);
        assert_eq!(es.len(), 2);
        // Byte boundary: a budget for one line and a bit.
        let one = entries_size(&items_from_line(
            ChatAgent::Claude,
            &two(4),
            1,
            len(&f) - 200,
        ));
        let b = Bounds {
            bytes: one + one / 2,
            ..BOUNDS
        };
        let n = len(&f);
        let Page {
            items: es, before, ..
        } = page_with(&mut f, ChatAgent::Claude, 1, n, 50, Back::Free, b);
        assert_eq!(texts(&es), ["t4", "call cmd4"]);
        let n = len(&f);
        let fwd = read_forward_with(&mut f, ChatAgent::Claude, 1, 0, n, 50, None, b);
        assert_eq!(texts(&fwd.items), ["t0", "call cmd0"]);
        assert!(fwd.more);
        assert!(before.is_some());
        // One line over an empty page's budget: cut, still whole, within the budget.
        let huge = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "\u{1}".repeat(30_000)},
            {"type": "tool_use", "id": "z", "name": "Bash", "input": {"command": "\u{1}".repeat(2000)}},
            {"type": "text", "text": "€".repeat(30_000)}]}});
        let (_, mut f) = file_of(&fx, "b", &[huge]);
        let b = Bounds {
            bytes: 64 * 1024,
            ..BOUNDS
        };
        let n = len(&f);
        let Page {
            items: es, before, ..
        } = page_with(&mut f, ChatAgent::Claude, 1, n, 50, Back::Free, b);
        assert_eq!(es.len(), 3);
        assert!(entries_size(&es) <= 64 * 1024, "{}", entries_size(&es));
        assert!(matches!(
            &es[0].item,
            ChatItem::Agent {
                truncated: true,
                ..
            }
        ));
        assert_eq!(before, None);
        let n = len(&f);
        let fwd = read_forward_with(&mut f, ChatAgent::Claude, 1, 0, n, 50, None, b);
        assert_eq!(fwd.items, es);
    }

    #[test]
    fn oversized_line_is_skipped() {
        let fx = Fixture::new();
        let big = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "x".repeat(MAX_LINE + 1024 * 1024)}]}});
        let lines = [agent("before"), big, agent("after")];
        let (_, mut f) = file_of(&fx, "a", &lines);
        let end = len(&f);
        let Page {
            items: es, before, ..
        } = page(&mut f, ChatAgent::Claude, 1, end, 50, Back::Free);
        assert_eq!(texts(&es), ["before", "after"]);
        assert_eq!(before, None);
        let fwd = read_forward(&mut f, ChatAgent::Claude, 1, 0, end, 50, None);
        assert_eq!(texts(&fwd.items), ["before", "after"]);
        assert_eq!((fwd.next, fwd.more), (end, false));
        // With a smaller scan budget the long line takes several reads, each moving on.
        let b = Bounds {
            max_line: 1024 * 1024,
            forward_scan: 2 * 1024 * 1024 + 1,
            ..BOUNDS
        };
        let (mut at, mut got, mut reads) = (0, Vec::new(), 0);
        loop {
            let fwd = read_forward_with(&mut f, ChatAgent::Claude, 1, at, end, 50, None, b);
            assert!(fwd.next > at || !fwd.more);
            got.extend(texts(&fwd.items));
            at = fwd.next;
            reads += 1;
            if !fwd.more {
                break;
            }
        }
        assert_eq!(got, ["before", "after"]);
        assert!(reads > 3, "{reads}");
        // An oversized line still being written: the cursor moves inside it, and what
        // follows its end is read.
        let p = fx.dir.path().join("s/b.jsonl");
        let mut w = File::create(&p).unwrap();
        writeln!(w, "{}", agent("one")).unwrap();
        write!(
            w,
            "{{\"type\":\"assistant\",\"x\":\"{}",
            "y".repeat(MAX_LINE + 10)
        )
        .unwrap();
        let mut f = File::open(&p).unwrap();
        let n = len(&f);
        let fwd = read_forward(&mut f, ChatAgent::Claude, 1, 0, n, 50, None);
        assert_eq!(texts(&fwd.items), ["one"]);
        assert!(fwd.next > MAX_LINE as u64 && fwd.next <= len(&f));
        writeln!(w, "\"}}").unwrap();
        writeln!(w, "{}", agent("two")).unwrap();
        let n = len(&f);
        let fwd = read_forward(&mut f, ChatAgent::Claude, 1, fwd.next, n, 50, None);
        assert_eq!(texts(&fwd.items), ["two"]);
        // The page's end ignores the long partial line it cannot look through.
        let n = len(&f);
        assert_eq!(complete_end(&mut f, n), n);
    }

    /// Small bounds so a test's oversized lines stay a few MiB.
    const SMALL: Bounds = Bounds {
        chunk: 64 * 1024,
        max_line: 1024 * 1024,
        page_scan: 2 * 1024 * 1024,
        forward_scan: 2 * 1024 * 1024,
        bytes: BOUNDS.bytes,
    };

    #[test]
    fn oversized_line_continuations_are_never_decoded() {
        let fx = Fixture::new();
        // Forward: leading whitespace makes the line's tail a valid record of its own.
        let p = fx.dir.path().join("fwd.jsonl");
        let mut w = File::create(&p).unwrap();
        writeln!(w, "{}", agent("one")).unwrap();
        // Long enough that the first read stops inside it, short enough that what is left
        // after its last skip is under `max_line`.
        writeln!(
            w,
            "{}{}",
            " ".repeat(2 * 1024 * 1024 + 20 * 1024),
            user("hidden")
        )
        .unwrap();
        writeln!(w, "{}", agent("two")).unwrap();
        let mut f = File::open(&p).unwrap();
        let end = len(&f);
        let (mut at, mut got, mut reads) = (0, Vec::new(), 0);
        loop {
            let fwd = read_forward_with(&mut f, ChatAgent::Claude, 1, at, end, 50, None, SMALL);
            got.extend(texts(&fwd.items));
            at = fwd.next;
            reads += 1;
            if !fwd.more {
                break;
            }
        }
        assert_eq!(got, ["one", "two"]);
        assert!(reads > 1, "the long line must span reads: {reads}");
        // Backward: trailing whitespace makes the line's head a valid record of its own.
        let p = fx.dir.path().join("back.jsonl");
        let mut w = File::create(&p).unwrap();
        writeln!(w, "{}", agent("one")).unwrap();
        writeln!(w, "{}{}", user("hidden"), " ".repeat(5 * 1024 * 1024 / 2)).unwrap();
        writeln!(w, "{}", agent("two")).unwrap();
        let mut f = File::open(&p).unwrap();
        let (mut upper, mut got, mut pages) = (len(&f), Vec::new(), 0);
        loop {
            let Page {
                items: es, before, ..
            } = page_with(&mut f, ChatAgent::Claude, 1, upper, 50, Back::Free, SMALL);
            let mut t = texts(&es);
            t.extend(got);
            got = t;
            pages += 1;
            match before {
                Some(b) => upper = b,
                None => break,
            }
        }
        assert_eq!(got, ["one", "two"]);
        assert!(pages > 1, "the long line must span pages: {pages}");
        // A cursor behind a too-long partial last line (complete_end gives `len`): the rest
        // of that line, once written, is skipped too.
        let p = fx.dir.path().join("tail.jsonl");
        let mut w = File::create(&p).unwrap();
        write!(w, "{}", " ".repeat(2 * 1024 * 1024)).unwrap();
        let mut f = File::open(&p).unwrap();
        let cursor = len(&f);
        writeln!(w, "{}", user("hidden")).unwrap();
        writeln!(w, "{}", agent("after")).unwrap();
        let n = len(&f);
        let fwd = read_forward_with(&mut f, ChatAgent::Claude, 1, cursor, n, 50, None, SMALL);
        assert_eq!(texts(&fwd.items), ["after"]);
    }

    #[test]
    fn read_forward_leaves_partial_line() {
        let fx = Fixture::new();
        let p = fx.dir.path().join("p.jsonl");
        let mut w = File::create(&p).unwrap();
        writeln!(w, "{}", user("asked")).unwrap();
        let half = agent("answer").to_string();
        write!(w, "{}", &half[..10]).unwrap();
        let mut f = File::open(&p).unwrap();
        let n = len(&f);
        let end = complete_end(&mut f, n);
        assert_eq!(end, (user("asked").to_string().len() + 1) as u64);
        let n = len(&f);
        let fwd = read_forward(&mut f, ChatAgent::Claude, 1, 0, n, 50, None);
        assert_eq!(texts(&fwd.items), ["asked"]);
        assert_eq!((fwd.next, fwd.more), (end, false));
        // Read again before it is finished: nothing.
        let n = len(&f);
        let again = read_forward(&mut f, ChatAgent::Claude, 1, fwd.next, n, 50, None);
        assert!(again.items.is_empty());
        assert_eq!(again.next, end);
        writeln!(w, "{}", &half[10..]).unwrap();
        let n = len(&f);
        let done = read_forward(&mut f, ChatAgent::Claude, 1, again.next, n, 50, None);
        assert_eq!(texts(&done.items), ["answer"]);
        assert_eq!(done.next, len(&f));
        let n = len(&f);
        let none = read_forward(&mut f, ChatAgent::Claude, 1, done.next, n, 50, None);
        assert!(none.items.is_empty());
        // No newline at all yet: the end of complete lines is 0.
        let p2 = fx.write("q.jsonl", "{\"type\":");
        let mut f2 = File::open(p2).unwrap();
        assert_eq!(complete_end(&mut f2, 8), 0);
        // The tail mark is the bytes before the cursor.
        assert_eq!(
            tail_mark(&mut f, 3).unwrap(),
            &user("asked").to_string().as_bytes()[..3]
        );
        assert_eq!(tail_mark(&mut f, 0).unwrap(), b"");
        assert_eq!(tail_mark(&mut f, end).unwrap().last(), Some(&b'\n'));
    }

    #[test]
    fn ids_stable_across_page_and_forward() {
        let fx = Fixture::new();
        let lines: Vec<Value> = (0..30)
            .map(|i| {
                if i % 3 == 0 {
                    tool_use(&format!("t{i}"), "Bash", json!({"command": "ls"}))
                } else {
                    agent(&format!("m{i}"))
                }
            })
            .collect();
        let (_, mut f) = file_of(&fx, "a", &lines);
        let end = len(&f);
        let mut paged = Vec::new();
        let mut upper = end;
        loop {
            let Page {
                items: es,
                before: b,
                ..
            } = page(&mut f, ChatAgent::Claude, 4, upper, 7, Back::Free);
            let mut es = es;
            es.extend(paged);
            paged = es;
            match b {
                Some(b) => upper = b,
                None => break,
            }
        }
        let mut forward = Vec::new();
        let mut at = 0;
        loop {
            let fwd = read_forward(&mut f, ChatAgent::Claude, 4, at, end, 5, None);
            forward.extend(fwd.items);
            at = fwd.next;
            if !fwd.more {
                break;
            }
        }
        assert_eq!(paged, forward);
        assert_eq!(paged.len(), 30);
        let ids: std::collections::HashSet<_> = paged.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), 30);
        assert!(paged.iter().all(|e| e.id.starts_with("4:")));
    }

    fn claude_spec(sid: &str) -> LaunchSpec {
        LaunchSpec {
            agent: Some("claude".into()),
            session_id: Some(sid.into()),
            cwd: CWD.into(),
            ..Default::default()
        }
    }

    #[test]
    fn session_file_for_specs() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let sf = session_file(&ctx, &claude_spec("s1")).unwrap();
        assert_eq!(sf.agent, ChatAgent::Claude);
        assert_eq!(sf.root, fx.home().join(".claude").join("projects"));
        assert_eq!(
            sf.path,
            sf.root.join(encode_project_name(CWD)).join("s1.jsonl")
        );
        assert_eq!(open(&sf).map(|_| ()), None, "missing");
        let codex = LaunchSpec {
            agent: Some("codex".into()),
            ..claude_spec("abc")
        };
        assert_eq!(session_file(&ctx, &codex), None, "no rollout yet");
        let p = fx.write_jsonl(
            "home/.codex/sessions/2026/01/01/rollout-2026-01-01T10-00-00-abc.jsonl",
            &[json!({})],
        );
        let sf = session_file(&ctx, &codex).unwrap();
        assert_eq!((sf.agent, sf.path), (ChatAgent::Codex, p));
        // Bounded discovery gives up early.
        assert_eq!(session_file_within(&ctx, &codex, 2), None);
        assert!(session_file_within(&ctx, &codex, 100).is_some());
        for s in [
            claude_spec("../s1"),
            claude_spec(&"a".repeat(201)),
            LaunchSpec {
                agent: Some("cursor".into()),
                ..claude_spec("s1")
            },
            LaunchSpec {
                shell_mode: Some("raw".into()),
                ..claude_spec("s1")
            },
            LaunchSpec {
                launch_prefix: Some(vec!["env".into()]),
                ..claude_spec("s1")
            },
            LaunchSpec {
                session_id: None,
                ..claude_spec("s1")
            },
        ] {
            assert_eq!(session_file(&ctx, &s), None, "{s:?}");
        }
        assert_eq!(chat_agent(&claude_spec("s")), Some(ChatAgent::Claude));
        assert_eq!(
            stream_session(&claude_spec(&"a".repeat(200))).map(str::len),
            Some(200)
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_file_confinement() {
        use std::os::unix::fs::symlink;
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let outside = fx.write_jsonl("elsewhere/s1.jsonl", &[agent("secret")]);
        let outside_dir = outside.parent().unwrap().to_path_buf();
        let proj = fx
            .home()
            .join(".claude/projects")
            .join(encode_project_name(CWD));
        std::fs::create_dir_all(&proj).unwrap();
        // A symlinked session file.
        symlink(&outside, proj.join("s1.jsonl")).unwrap();
        assert!(open(&session_file(&ctx, &claude_spec("s1")).unwrap()).is_none());
        // A FIFO.
        let fifo = proj.join("f.jsonl");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(open(&session_file(&ctx, &claude_spec("f")).unwrap()).is_none());
        // A symlinked Project directory.
        std::fs::remove_dir_all(&proj).unwrap();
        std::fs::write(
            outside_dir.join("s2.jsonl"),
            format!("{}\n", agent("secret")),
        )
        .unwrap();
        symlink(&outside_dir, &proj).unwrap();
        assert!(open(&session_file(&ctx, &claude_spec("s2")).unwrap()).is_none());
        // A symlinked Codex date directory: the walk does not enter it; named directly the
        // open refuses it.
        let day = fx.home().join(".codex/sessions/2026/01");
        std::fs::create_dir_all(&day).unwrap();
        let name = "rollout-2026-01-01T10-00-00-c1.jsonl";
        std::fs::write(outside_dir.join(name), "{}\n").unwrap();
        symlink(&outside_dir, day.join("02")).unwrap();
        let codex = LaunchSpec {
            agent: Some("codex".into()),
            ..claude_spec("c1")
        };
        assert_eq!(session_file(&ctx, &codex), None);
        let through = SessionFile {
            agent: ChatAgent::Codex,
            root: fx.home().join(".codex/sessions"),
            path: day.join("02").join(name),
        };
        assert!(open(&through).is_none());
        // A swap between the checks and the open (the `BEFORE_OPEN` seam).
        std::fs::remove_file(&proj).unwrap();
        let real = fx.write_jsonl(
            format!(
                "home/.claude/projects/{}/s3.jsonl",
                encode_project_name(CWD)
            ),
            &[agent("inside")],
        );
        let sf = session_file(&ctx, &claude_spec("s3")).unwrap();
        assert!(open(&sf).is_some());
        let o = outside.clone();
        crate::last_line::BEFORE_OPEN.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |p: &std::path::Path| {
                std::fs::remove_file(p).unwrap();
                symlink(&o, p).unwrap();
            }))
        });
        assert!(open(&sf).is_none());
        crate::last_line::BEFORE_OPEN.with(|h| *h.borrow_mut() = None);
        assert!(real.is_symlink());
    }

    // ── Rewinds: only the branch the conversation is on ──

    /// `v` as a line of Claude's tree.
    fn node(mut v: Value, uuid: &str, parent: Option<&str>) -> Value {
        v["uuid"] = json!(uuid);
        v["parentUuid"] = json!(parent);
        v
    }

    fn turn_end(uuid: &str, parent: &str) -> Value {
        node(
            json!({"type": "system", "subtype": "turn_duration"}),
            uuid,
            Some(parent),
        )
    }

    fn interrupted(uuid: &str, parent: &str) -> Value {
        let v = json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "text", "text": "[Request interrupted by user]"}]}});
        node(v, uuid, Some(parent))
    }

    fn snapshot_line() -> Value {
        json!({"type": "file-history-snapshot", "messageId": "m"})
    }

    /// A turn, then one that was interrupted and rewound: the prompt `C` has the parent of `B`
    /// (the shape of a real rewind: the abandoned lines stay before it).
    fn rewound() -> Vec<Value> {
        vec![
            node(user("A"), "s0", None),
            node(agent("a"), "a0", Some("s0")),
            turn_end("t0", "a0"),
            snapshot_line(),
            node(user("B"), "p1", Some("t0")),
            node(agent("b"), "a1", Some("p1")),
            interrupted("i1", "a1"),
            snapshot_line(),
            node(user("C"), "p2", Some("t0")),
            node(agent("c"), "a2", Some("p2")),
        ]
    }

    /// Every page from the end of `f`, carrying [`Page::back`] (or not), oldest first.
    fn all_pages(f: &mut File, limit: usize, carry: bool, b: Bounds) -> Vec<String> {
        let n = len(f);
        let mut upper = complete_end(f, n);
        let mut back = Back::Free;
        let mut seen: Vec<String> = Vec::new();
        loop {
            let pg = page_with(f, ChatAgent::Claude, 1, upper, limit, back.clone(), b);
            let mut t = texts(&pg.items);
            t.extend(seen);
            seen = t;
            match pg.before {
                Some(at) => {
                    assert!(at < upper);
                    upper = at;
                }
                None => return seen,
            }
            back = if carry { pg.back } else { Back::Free };
        }
    }

    #[test]
    fn a_rewind_shows_only_the_new_branch() {
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &rewound());
        let n = len(&f);
        let end = complete_end(&mut f, n);
        let pg = page(&mut f, ChatAgent::Claude, 1, end, 50, Back::Free);
        assert_eq!(texts(&pg.items), ["A", "a", "C", "c"]);
        assert_eq!(pg.before, None);
        assert_eq!(pg.newest.as_deref(), Some("a2"));
    }

    #[test]
    fn rewinds_to_the_same_message_twice_show_the_newest_branch() {
        let mut lines = rewound();
        lines.push(interrupted("i2", "a2"));
        lines.push(node(user("D"), "p3", Some("t0")));
        lines.push(node(agent("d"), "a3", Some("p3")));
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        assert_eq!(all_pages(&mut f, 50, true, BOUNDS), ["A", "a", "D", "d"]);
    }

    #[test]
    fn a_rewind_to_the_first_prompt_hides_everything_before_it() {
        let mut lines = rewound();
        lines.push(node(user("Z"), "z0", None));
        lines.push(node(agent("z"), "z1", Some("z0")));
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        assert_eq!(all_pages(&mut f, 50, true, BOUNDS), ["Z", "z"]);
    }

    #[test]
    fn parallel_tool_calls_sharing_a_parent_are_not_a_rewind() {
        // Claude writes each tool_use of one message as its own line; a result can share its
        // parent with the next call.
        let lines = vec![
            node(user("go"), "s0", None),
            node(
                tool_use("t1", "Read", json!({"file_path": "/x"})),
                "c1",
                Some("s0"),
            ),
            node(
                tool_use("t2", "Read", json!({"file_path": "/y"})),
                "c2",
                Some("c1"),
            ),
            node(result("t1", json!("one"), false), "r1", Some("c1")),
            node(result("t2", json!("two"), false), "r2", Some("c2")),
            node(agent("done"), "a1", Some("r2")),
            turn_end("e1", "a1"),
            node(user("next"), "s1", Some("e1")),
        ];
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        let want = [
            "go",
            "call /x",
            "call /y",
            "result one",
            "result two",
            "done",
            "next",
        ];
        assert_eq!(all_pages(&mut f, 50, true, BOUNDS), want);
        let n = len(&f);
        let fwd = read_forward(&mut f, ChatAgent::Claude, 1, 0, n, 50, None);
        assert!(!fwd.fork);
        assert_eq!(texts(&fwd.items), want);
        assert_eq!(fwd.last.as_deref(), Some("s1"));
    }

    #[test]
    fn a_page_ending_at_the_new_prompt_hands_the_skip_to_the_next_page() {
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &rewound());
        // Two items: `c` and `C`, then the page is full.
        let n = len(&f);
        let end = complete_end(&mut f, n);
        let pg = page(&mut f, ChatAgent::Claude, 1, end, 2, Back::Free);
        assert_eq!(texts(&pg.items), ["C", "c"]);
        assert_eq!(pg.back, Back::Expect(Some("t0".into())));
        assert_eq!(all_pages(&mut f, 2, true, BOUNDS), ["A", "a", "C", "c"]);
        // Without the state the next page would show the abandoned branch.
        let shown = all_pages(&mut f, 2, false, BOUNDS);
        assert!(shown.contains(&"B".to_string()), "{shown:?}");
    }

    #[test]
    fn a_skip_longer_than_the_scan_budget_goes_on_in_the_next_page() {
        let mut lines = vec![
            node(user("A"), "s0", None),
            node(agent("a"), "a0", Some("s0")),
            node(user("B"), "p1", Some("a0")),
        ];
        let mut parent = "p1".to_string();
        for i in 0..200 {
            let id = format!("x{i}");
            lines.push(node(
                agent(&format!("abandoned {i} {}", "y".repeat(100))),
                &id,
                Some(&parent),
            ));
            parent = id;
        }
        lines.push(node(user("C"), "p2", Some("a0")));
        let tiny = Bounds {
            chunk: 1024,
            max_line: 4096,
            page_scan: 8192,
            forward_scan: 8192,
            bytes: BOUNDS.bytes,
        };
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        assert!(len(&f) > 4 * tiny.page_scan);
        assert_eq!(all_pages(&mut f, 50, true, tiny), ["A", "a", "C"]);
    }

    #[test]
    fn a_forward_read_stops_at_a_rewound_prompt() {
        let lines = rewound();
        let fx = Fixture::new();
        let (p, mut f) = file_of(&fx, "r", &lines[..3]);
        let n = len(&f);
        let end = complete_end(&mut f, n);
        let pg = page(&mut f, ChatAgent::Claude, 1, end, 50, Back::Free);
        assert_eq!(pg.newest.as_deref(), Some("t0"));
        let mut w = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        for l in &lines[3..7] {
            writeln!(w, "{l}").unwrap();
        }
        let n = len(&f);
        let fwd = read_forward(&mut f, ChatAgent::Claude, 1, end, n, 50, pg.newest);
        assert!(!fwd.fork);
        assert_eq!(
            texts(&fwd.items),
            ["B", "b", "[Request interrupted by user]"]
        );
        assert_eq!(fwd.last.as_deref(), Some("i1"));
        for l in &lines[7..] {
            writeln!(w, "{l}").unwrap();
        }
        let fork_at = n + format!("{}\n", lines[7]).len() as u64;
        let m = len(&f);
        let fwd2 = read_forward(
            &mut f,
            ChatAgent::Claude,
            1,
            fwd.next,
            m,
            50,
            fwd.last.clone(),
        );
        assert!(fwd2.fork);
        assert_eq!(fwd2.next, fork_at);
        assert!(fwd2.items.is_empty());
        // The parent unknown (a line that could not be read): no rewind is assumed.
        let blind = read_forward(&mut f, ChatAgent::Claude, 1, fwd.next, m, 50, None);
        assert!(!blind.fork);
        assert_eq!(texts(&blind.items), ["C", "c"]);
    }

    #[test]
    fn codex_lines_are_never_a_rewind() {
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &rewound());
        let n = len(&f);
        let fwd = read_forward(&mut f, ChatAgent::Codex, 1, 0, n, 50, Some("zz".into()));
        assert!(!fwd.fork);
        assert_eq!(fwd.last.as_deref(), Some("zz"));
    }

    #[test]
    fn progress_records_with_a_uuid_are_not_the_parent_a_prompt_follows() {
        let lines = vec![
            node(user("A"), "s0", None),
            node(agent("a"), "a0", Some("s0")),
            node(json!({"type": "progress", "data": {}}), "g0", Some("a0")),
            node(user("B"), "p1", Some("a0")),
        ];
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        let n = len(&f);
        let fwd = read_forward(&mut f, ChatAgent::Claude, 1, 0, n, 50, None);
        assert!(!fwd.fork);
        assert_eq!(texts(&fwd.items), ["A", "a", "B"]);
        assert_eq!(all_pages(&mut f, 50, true, BOUNDS), ["A", "a", "B"]);
    }

    #[test]
    fn an_unreadable_parent_never_hides_the_conversation() {
        let tiny = Bounds {
            chunk: 1024,
            max_line: 4096,
            page_scan: 64 * 1024,
            forward_scan: 64 * 1024,
            bytes: BOUNDS.bytes,
        };
        let huge = node(
            result("t1", json!("z".repeat(8000)), false),
            "r1",
            Some("c1"),
        );
        let lines = vec![
            node(user("A"), "s0", None),
            node(
                tool_use("t1", "Read", json!({"file_path": "/x"})),
                "c1",
                Some("s0"),
            ),
            huge,
            node(user("B"), "p1", Some("r1")),
        ];
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        assert_eq!(all_pages(&mut f, 50, true, tiny), ["A", "call /x", "B"]);
        let n = len(&f);
        let fwd = read_forward_with(&mut f, ChatAgent::Claude, 1, 0, n, 50, None, tiny);
        assert!(!fwd.fork);
        assert_eq!(texts(&fwd.items), ["A", "call /x", "B"]);
    }

    #[test]
    fn a_prompt_left_for_the_next_read_by_the_byte_budget_is_not_a_rewind() {
        let lines = vec![
            node(agent(&"a".repeat(3000)), "a0", None),
            node(user(&"B".repeat(3000)), "p1", Some("a0")),
        ];
        let small = Bounds {
            bytes: 4000,
            ..BOUNDS
        };
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        let n = len(&f);
        let one = read_forward_with(&mut f, ChatAgent::Claude, 1, 0, n, 50, None, small);
        assert!(one.more && !one.fork);
        assert_eq!(one.items.len(), 1);
        assert_eq!(one.last.as_deref(), Some("a0"));
        let two = read_forward_with(
            &mut f,
            ChatAgent::Claude,
            1,
            one.next,
            n,
            50,
            one.last,
            small,
        );
        assert!(!two.fork);
        assert_eq!(two.items.len(), 1);
    }

    #[test]
    fn an_unreadable_newest_line_leaves_the_newest_uuid_unknown() {
        let tiny = Bounds {
            chunk: 1024,
            max_line: 4096,
            page_scan: 64 * 1024,
            forward_scan: 64 * 1024,
            bytes: BOUNDS.bytes,
        };
        let huge = node(
            result("t1", json!("z".repeat(8000)), false),
            "r1",
            Some("c1"),
        );
        {
            let last = huge;
            let lines = vec![
                node(user("A"), "s0", None),
                node(
                    tool_use("t1", "Read", json!({"file_path": "/x"})),
                    "c1",
                    Some("s0"),
                ),
                last,
            ];
            let fx = Fixture::new();
            let (_, mut f) = file_of(&fx, "r", &lines);
            let n = len(&f);
            let end = complete_end(&mut f, n);
            let pg = page_with(&mut f, ChatAgent::Claude, 1, end, 50, Back::Free, tiny);
            assert_eq!(pg.newest, None);
        }
        // A malformed line: no uuid either.
        let fx = Fixture::new();
        let p = fx.write_jsonl("s/m.jsonl", &[node(user("A"), "s0", None)]);
        let mut w = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(w, "{{broken").unwrap();
        let mut f = File::open(&p).unwrap();
        let n = len(&f);
        let pg = page(&mut f, ChatAgent::Claude, 1, n, 50, Back::Free);
        assert_eq!(pg.newest, None);
        assert_eq!(texts(&pg.items), ["A"]);
    }

    #[test]
    fn a_parent_prompt_left_by_the_byte_budget_keeps_the_state() {
        // `C` follows prompt `B` directly (B's turn produced nothing), and `B` does not fit.
        let lines = vec![
            node(user("A"), "s0", None),
            node(user(&"B".repeat(3000)), "p1", Some("s0")),
            node(user(&"C".repeat(3000)), "p2", Some("p1")),
        ];
        let small = Bounds {
            bytes: 4000,
            ..BOUNDS
        };
        let fx = Fixture::new();
        let (_, mut f) = file_of(&fx, "r", &lines);
        let n = len(&f);
        let end = complete_end(&mut f, n);
        let pg = page_with(&mut f, ChatAgent::Claude, 1, end, 50, Back::Free, small);
        assert_eq!(pg.items.len(), 1);
        assert_eq!(pg.back, Back::Expect(Some("p1".into())));
        let at = pg.before.unwrap();
        let pg2 = page_with(&mut f, ChatAgent::Claude, 1, at, 50, pg.back, small);
        let t = texts(&pg2.items);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[0], "A");
        assert!(t[1].starts_with('B'));
        assert_eq!(pg2.before, None);
    }
}

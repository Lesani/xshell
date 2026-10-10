#![cfg(unix)]
//! Session streams (capability `session.stream`): a connection subscribes to an agent
//! Terminal's conversation and gets its newest page, then `session.append`s, against an
//! in-process server whose fake agents only sleep. The tests write the session files.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::claude::encode_project_name;
use xshell_core::launch::LaunchSpec;
use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
use xshell_protocol::msg::{
    decode_server, encode_msg, AgentStatus, ChatEntry, ChatItem, ClientMsg, Hello, ServerMsg,
    SessionPage, CHAT_PAGE_MAX_BYTES, NOT_SUBSCRIBED, NO_SESSION_STREAM, SESSION_CHANGED,
};
use xshelld::server::{Config, Role, ServerHandle, TestHook, TestPoint};

const SID: &str = "11111111-2222-3333-4444-555555555555";
const SID_B: &str = "bbbbbbbb-2222-3333-4444-555555555555";
const POLL: Duration = Duration::from_millis(50);

/// Fake `claude`, `codex` and `cursor-agent`, first in `PATH` once per test binary: each
/// logs its `XSHELL_*` environment and pid in its working directory, then sleeps.
fn fake_agents() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let bin = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stream-agents");
        fs::create_dir_all(&bin).unwrap();
        for name in ["claude", "codex", "cursor-agent"] {
            let tmp = bin.join(format!(".{name}.{}", std::process::id()));
            fs::write(
                &tmp,
                "#!/bin/sh\ntrap '' HUP\nenv | grep '^XSHELL_TERMINAL_ID' >> env.log\n\
                 echo $$ >> pids.log\nexec sleep 1000\n",
            )
            .unwrap();
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
            fs::rename(&tmp, bin.join(name)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", path);
    });
}

struct Env {
    h: TestHome,
    srv: ServerHandle,
    /// A Desktop on the socket: opens Terminals, links sessions.
    desk: Client,
    cwd: PathBuf,
    _reaper: FakeReaper,
}

fn env() -> Env {
    env_with(|_| {})
}

fn env_with(tweak: impl FnOnce(&mut Config)) -> Env {
    fake_agents();
    let h = TestHome::new();
    let cwd = h.project("app");
    let srv = start(&h, |c| {
        c.session_poll = POLL;
        tweak(c);
    });
    let mut desk = Client::connect(&srv.socket);
    desk.hello(range(1, 1));
    Env {
        _reaper: FakeReaper(cwd.join("pids.log")),
        h,
        srv,
        desk,
        cwd,
    }
}

fn spec(cwd: &Path, agent: &str, session: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        agent: Some(agent.into()),
        shell_mode: Some("claude".into()),
        session_id: session.map(str::to_string),
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

impl Env {
    fn open_spec(&mut self, s: LaunchSpec) -> Uuid {
        let t = Uuid::new_v4();
        self.desk.open(t, s);
        t
    }

    fn open(&mut self, agent: &str, session: Option<&str>) -> Uuid {
        let s = spec(&self.cwd, agent, session);
        self.open_spec(s)
    }

    fn claude_file(&self, sid: &str) -> PathBuf {
        self.h
            .home()
            .join(".claude/projects")
            .join(encode_project_name(&self.cwd.to_string_lossy()))
            .join(format!("{sid}.jsonl"))
    }

    fn codex_file(&self, sid: &str) -> PathBuf {
        self.h
            .home()
            .join(".codex/sessions/2026/10/10")
            .join(format!("rollout-2026-10-10T10-00-00-{sid}.jsonl"))
    }

    fn mobile(&self) -> Client {
        Client::in_process(&self.srv, Role::Mobile)
    }

    fn link(&mut self, t: Uuid, sid: &str) {
        self.desk
            .request(&ClientMsg::TermUpdate {
                terminal: t,
                session_id: Some(sid.into()),
                meta: None,
            })
            .unwrap();
    }

    /// An agent report for `t`'s newest run, as its hook would send it.
    fn report(&mut self, t: Uuid, status: AgentStatus) {
        let deadline = Instant::now() + T;
        let run = loop {
            let log = fs::read_to_string(self.cwd.join("env.log")).unwrap_or_default();
            let found = log.lines().rev().find_map(|l| {
                let v = l.strip_prefix("XSHELL_TERMINAL_ID=")?;
                let (id, run) = v.split_once('.')?;
                (id == t.to_string()).then(|| run.parse::<u64>().ok())?
            });
            if let Some(r) = found {
                break r;
            }
            assert!(Instant::now() < deadline, "no run logged for {t}");
            std::thread::sleep(Duration::from_millis(20));
        };
        self.desk
            .request(&ClientMsg::TermEvent {
                terminal: t,
                run,
                status,
            })
            .unwrap();
    }
}

fn append(p: &Path, v: &Value) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .unwrap();
    writeln!(f, "{v}").unwrap();
}

fn user(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": text}})
}

fn agent(text: &str) -> Value {
    json!({"type": "assistant", "message": {"role": "assistant",
           "content": [{"type": "text", "text": text}]}})
}

fn tool_use(id: &str, cmd: &str) -> Value {
    json!({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": cmd}}]}})
}

fn tool_result(id: &str, out: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "content": out}]}})
}

fn codex_ev(kind: &str, text: &str) -> Value {
    json!({"timestamp": "2026-10-10T10:00:01Z", "type": "event_msg",
           "payload": {"type": kind, "message": text}})
}

fn text(e: &ChatEntry) -> String {
    match &e.item {
        ChatItem::User { text, .. } => format!("u:{text}"),
        ChatItem::Agent { text, .. } => format!("a:{text}"),
        ChatItem::ToolCall { summary, .. } => format!("call:{summary}"),
        ChatItem::ToolResult { text, error, .. } => format!("res:{text}:{error}"),
    }
}

fn texts(es: &[ChatEntry]) -> Vec<String> {
    es.iter().map(text).collect()
}

fn subscribe(c: &mut Client, t: Uuid) -> Result<SessionPage, String> {
    subscribe_limit(c, t, None)
}

fn subscribe_limit(c: &mut Client, t: Uuid, limit: Option<u32>) -> Result<SessionPage, String> {
    c.request(&ClientMsg::SessionSubscribe { terminal: t, limit })
        .map(|v| serde_json::from_value(v).expect("a page"))
}

fn older(c: &mut Client, t: Uuid, gen: u64, before: u64) -> Result<SessionPage, String> {
    c.request(&ClientMsg::SessionPage {
        terminal: t,
        gen,
        before,
        limit: Some(50),
    })
    .map(|v| serde_json::from_value(v).expect("a page"))
}

/// One `session.append` for `t`.
#[derive(Debug, Clone)]
struct Append {
    gen: u64,
    reset: bool,
    session: Option<String>,
    items: Vec<ChatEntry>,
    before: Option<u64>,
}

fn try_append(c: &mut Client, t: Uuid, within: Duration) -> Option<Append> {
    match c.try_msg(
        within,
        |m| matches!(m, ServerMsg::SessionAppend { terminal, .. } if *terminal == t),
    )? {
        ServerMsg::SessionAppend {
            gen,
            reset,
            session,
            items,
            before,
            ..
        } => Some(Append {
            gen,
            reset,
            session,
            items,
            before,
        }),
        _ => unreachable!(),
    }
}

fn next_append(c: &mut Client, t: Uuid) -> Append {
    try_append(c, t, T).unwrap_or_else(|| panic!("no session.append; log {:?}", c.summary()))
}

/// What a Chat View shows: appends applied to the page until it shows `want`. Returns the
/// appends seen.
fn wait_view(c: &mut Client, t: Uuid, page: &[ChatEntry], want: &[&str]) -> Vec<Append> {
    let mut view: Vec<ChatEntry> = page.to_vec();
    let mut seen = Vec::new();
    let deadline = Instant::now() + T;
    while texts(&view) != want {
        let left = deadline.saturating_duration_since(Instant::now());
        let a = try_append(c, t, left)
            .unwrap_or_else(|| panic!("view {:?} never became {want:?}", texts(&view)));
        if a.reset {
            view = a.items.clone();
        } else {
            view.extend(a.items.clone());
        }
        seen.push(a);
    }
    seen
}

/// Every append for `t` that arrives within `d`.
fn quiet_for(c: &mut Client, t: Uuid, d: Duration) -> Vec<Append> {
    let deadline = Instant::now() + d;
    let mut got = Vec::new();
    while let Some(a) = try_append(c, t, deadline.saturating_duration_since(Instant::now())) {
        got.push(a);
    }
    got
}

#[test]
fn subscribe_returns_initial_page_then_appends() {
    let mut e = env();
    let f = e.claude_file(SID);
    append(&f, &user("fix the build"));
    append(&f, &json!({"type": "progress", "data": "x"}));
    append(&f, &agent("Looking."));
    append(&f, &tool_use("t0", "ls"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    assert_eq!(page.session.as_deref(), Some(SID));
    assert_eq!(
        texts(&page.items),
        ["u:fix the build", "a:Looking.", "call:ls"]
    );
    assert_eq!(page.before, None);
    assert!(page
        .items
        .iter()
        .all(|i| i.id.starts_with(&format!("{}:", page.gen))));
    // No hook fires: polling alone delivers, in order, exactly once.
    append(&f, &user("and the tests"));
    append(&f, &tool_use("t1", "npm test"));
    append(&f, &tool_result("t1", "3 passed"));
    append(&f, &agent("All green."));
    let seen = wait_view(
        &mut m,
        t,
        &page.items,
        &[
            "u:fix the build",
            "a:Looking.",
            "call:ls",
            "u:and the tests",
            "call:npm test",
            "res:3 passed:false",
            "a:All green.",
        ],
    );
    assert!(
        seen.iter().all(|a| !a.reset && a.gen == page.gen),
        "{seen:?}"
    );
    assert!(quiet_for(&mut m, t, POLL * 6).is_empty());
}

#[test]
fn appends_during_a_turn_without_hooks() {
    let mut e = env();
    let f = e.claude_file(SID);
    append(&f, &user("go"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    // A turn writes line after line; no hook fires until its end.
    let mut want = vec!["u:go".to_string()];
    for i in 0..5 {
        let started = Instant::now();
        append(&f, &agent(&format!("step {i}")));
        want.push(format!("a:step {i}"));
        let a = next_append(&mut m, t);
        assert_eq!(texts(&a.items), [want.last().unwrap().clone()]);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    assert_eq!(page.items.len(), 1);
}

#[test]
fn codex_subscribe_and_append() {
    let mut e = env();
    let sid = "0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee";
    let f = e.codex_file(sid);
    append(&f, &json!({"type": "session_meta", "payload": {"id": sid}}));
    append(&f, &codex_ev("user_message", "build it"));
    let t = e.open("codex", Some(sid));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    assert_eq!(texts(&page.items), ["u:build it"]);
    append(
        &f,
        &json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
            "call_id": "c1", "arguments": r#"{"command":["bash","-lc","cargo build"]}"#}}),
    );
    append(
        &f,
        &json!({"type": "response_item", "payload": {"type": "function_call_output",
            "call_id": "c1", "output": r#"{"output":"ok","metadata":{"exit_code":0}}"#}}),
    );
    append(&f, &codex_ev("agent_message", "Built."));
    wait_view(
        &mut m,
        t,
        &page.items,
        &["u:build it", "call:cargo build", "res:ok:false", "a:Built."],
    );
}

#[test]
fn hook_wakes_immediately() {
    let mut e = env_with(|c| c.session_poll = Duration::from_secs(3600));
    let f = e.claude_file(SID);
    append(&f, &user("hi"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    subscribe(&mut m, t).unwrap();
    append(&f, &agent("answer"));
    // Not polled within the hour.
    assert!(try_append(&mut m, t, Duration::from_millis(400)).is_none());
    let started = Instant::now();
    e.report(t, AgentStatus::Finished);
    let a = next_append(&mut m, t);
    assert_eq!(texts(&a.items), ["a:answer"]);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn page_older_walks_back_to_start() {
    let mut e = env();
    let f = e.claude_file(SID);
    for i in 0..120 {
        append(&f, &agent(&format!("m{i}")));
    }
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    assert_eq!(page.items.len(), 50);
    let mut all = page.items.clone();
    let mut before = page.before;
    let mut sizes = vec![50];
    while let Some(b) = before {
        let p = older(&mut m, t, page.gen, b).unwrap();
        sizes.push(p.items.len());
        let mut v = p.items;
        v.extend(all);
        all = v;
        before = p.before;
    }
    assert_eq!(sizes, [50, 50, 20]);
    let want: Vec<String> = (0..120).map(|i| format!("a:m{i}")).collect();
    assert_eq!(texts(&all), want);
    let ids: std::collections::HashSet<_> = all.iter().map(|e| e.id.clone()).collect();
    assert_eq!(ids.len(), 120);
}

#[test]
fn page_with_stale_session_errs() {
    let mut e = env();
    let t = e.open("claude", Some(SID));
    for i in 0..60 {
        append(&e.claude_file(SID), &agent(&format!("a{i}")));
    }
    append(&e.claude_file(SID_B), &agent("b"));
    let mut m = e.mobile();
    assert_eq!(older(&mut m, t, 1, 0).unwrap_err(), NOT_SUBSCRIBED);
    let page = subscribe(&mut m, t).unwrap();
    let before = page.before.expect("older entries");
    e.link(t, SID_B);
    let a = next_append(&mut m, t);
    assert!(a.reset);
    assert_eq!(a.session.as_deref(), Some(SID_B));
    assert_eq!(texts(&a.items), ["a:b"]);
    assert!(a.gen > page.gen);
    assert_eq!(
        older(&mut m, t, page.gen, before).unwrap_err(),
        SESSION_CHANGED
    );
    // A cursor past what was read is refused.
    let a_end = fs::metadata(e.claude_file(SID_B)).unwrap().len();
    assert!(older(&mut m, t, a.gen, a_end + 100).is_err());
}

/// Where a held read says it is held, and what releases it.
type Held = (Sender<()>, Receiver<()>);

/// Holds the next session read of the worker until released.
#[derive(Clone, Default)]
struct Hold(Arc<Mutex<Option<Held>>>);

impl Hold {
    fn hook(&self) -> TestHook {
        let me = self.clone();
        TestHook(Arc::new(move |_, p| {
            if p == TestPoint::SessionRead {
                let armed = me.0.lock().unwrap().take();
                if let Some((held, release)) = armed {
                    held.send(()).unwrap();
                    let _ = release.recv_timeout(T);
                }
            }
            false
        }))
    }

    /// Hold the next read: returns (held, release).
    fn arm(&self) -> (Receiver<()>, Sender<()>) {
        let (held_tx, held_rx) = channel();
        let (release_tx, release_rx) = channel();
        *self.0.lock().unwrap() = Some((held_tx, release_rx));
        (held_rx, release_tx)
    }
}

#[test]
fn relink_resets() {
    let hold = Hold::default();
    let hook = hold.hook();
    let mut e = env_with(|c| c.test_hook = Some(hook));
    append(&e.claude_file(SID), &agent("a1"));
    append(&e.claude_file(SID_B), &agent("b1"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    assert_eq!(texts(&page.items), ["a:a1"]);
    // A read of session A's new line is held while the Desktop links session B.
    let (held, release) = hold.arm();
    append(&e.claude_file(SID), &agent("a2"));
    held.recv_timeout(T).expect("the read of A");
    e.link(t, SID_B);
    release.send(()).unwrap();
    let a = next_append(&mut m, t);
    assert!(a.reset, "{a:?}");
    assert_eq!(a.session.as_deref(), Some(SID_B));
    assert_eq!(texts(&a.items), ["a:b1"]);
    // Nothing of A after the relink, ever.
    for later in quiet_for(&mut m, t, POLL * 6) {
        assert!(!texts(&later.items).contains(&"a:a2".to_string()));
    }
    append(&e.claude_file(SID_B), &agent("b2"));
    let a2 = next_append(&mut m, t);
    assert_eq!(
        (texts(&a2.items), a2.gen, a2.reset),
        (vec!["a:b2".into()], a.gen, false)
    );
}

#[test]
fn line_appended_while_subscribing_arrives_once() {
    let hold = Hold::default();
    let hook = hold.hook();
    let mut e = env_with(|c| c.test_hook = Some(hook));
    let f = e.claude_file(SID);
    append(&f, &agent("one"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let (held, release) = hold.arm();
    let id = m.request_id();
    m.send(
        &ClientMsg::SessionSubscribe {
            terminal: t,
            limit: None,
        },
        Some(id),
    );
    held.recv_timeout(T).expect("the subscribe's read");
    append(&f, &agent("two"));
    release.send(()).unwrap();
    let page: SessionPage = serde_json::from_value(m.wait_res(id).unwrap()).unwrap();
    assert_eq!(texts(&page.items), ["a:one"]);
    let a = next_append(&mut m, t);
    assert_eq!(texts(&a.items), ["a:two"]);
    assert!(quiet_for(&mut m, t, POLL * 6).is_empty());
}

#[test]
fn unlinked_codex_then_linked() {
    let mut e = env();
    let t = e.open("codex", None);
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    assert_eq!(page.session, None);
    assert!(page.items.is_empty() && page.before.is_none());
    let sid = "0199aaaa-bbbb-7ccc-8ddd-ffffffffffff";
    append(&e.codex_file(sid), &codex_ev("user_message", "hello codex"));
    e.link(t, sid);
    let a = next_append(&mut m, t);
    assert!(a.reset);
    assert_eq!(a.session.as_deref(), Some(sid));
    assert_eq!(texts(&a.items), ["u:hello codex"]);
}

#[test]
fn codex_rollout_appearing_later_resets() {
    let mut e = env();
    let sid = "0199aaaa-bbbb-7ccc-8ddd-000000000001";
    let t = e.open("codex", Some(sid));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    assert_eq!(page.session.as_deref(), Some(sid));
    assert!(page.items.is_empty());
    append(&e.codex_file(sid), &codex_ev("user_message", "late"));
    let a = next_append(&mut m, t);
    assert!(a.reset);
    assert_eq!(texts(&a.items), ["u:late"]);
}

#[test]
fn replaced_or_truncated_file_resets() {
    let mut e = env_with(|c| c.session_poll = Duration::from_secs(3600));
    let f = e.claude_file(SID);
    append(&f, &agent("first version"));
    append(&f, &agent("more"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    // Replaced (a new file renamed over it).
    let tmp = f.with_extension("tmp");
    append(&tmp, &agent("replaced"));
    fs::rename(&tmp, &f).unwrap();
    e.report(t, AgentStatus::Working);
    let a = next_append(&mut m, t);
    assert!(a.reset && a.gen > page.gen);
    assert_eq!(texts(&a.items), ["a:replaced"]);
    // Truncated in place to something shorter.
    fs::write(&f, format!("{}\n", agent("x"))).unwrap();
    e.report(t, AgentStatus::Finished);
    let b = next_append(&mut m, t);
    assert!(b.reset && b.gen > a.gen);
    assert_eq!(texts(&b.items), ["a:x"]);
    // Truncated and grown past the cursor before the next read: same file, other bytes.
    fs::write(
        &f,
        format!("{}\n{}\n", agent("rewritten one"), agent("rewritten two")),
    )
    .unwrap();
    e.report(t, AgentStatus::Working);
    let c = next_append(&mut m, t);
    assert!(c.reset && c.gen > b.gen, "{c:?}");
    assert_eq!(texts(&c.items), ["a:rewritten one", "a:rewritten two"]);
}

#[test]
fn mobile_refused_for_shell_prefix_and_other_agents() {
    let mut e = env();
    let shell = e.open_spec(sh_spec(&e.cwd.clone()));
    let prefixed = e.open_spec(LaunchSpec {
        launch_prefix: Some(vec!["env".into()]),
        ..spec(&e.cwd, "claude", Some(SID))
    });
    let cursor = e.open("cursor", None);
    let mut m = e.mobile();
    for t in [shell, prefixed] {
        let err = subscribe(&mut m, t).unwrap_err();
        assert!(err.starts_with("forbidden for mobile"), "{err}");
    }
    assert_eq!(subscribe(&mut m, cursor).unwrap_err(), NO_SESSION_STREAM);
    let err = subscribe(&mut m, Uuid::new_v4()).unwrap_err();
    assert!(err.starts_with("unknown terminal"), "{err}");
    // A Desktop may subscribe too, to the same direct agents: a shell or a wrapped agent
    // has no session stream.
    assert_eq!(
        subscribe(&mut e.desk, shell).unwrap_err(),
        NO_SESSION_STREAM
    );
    assert_eq!(
        subscribe(&mut e.desk, prefixed).unwrap_err(),
        NO_SESSION_STREAM
    );
    let plain = e.open("claude", Some(SID));
    assert!(subscribe(&mut e.desk, plain).is_ok());
    assert_eq!(e.srv.session_subs(), 1);
}

#[test]
fn confinement_never_leaks_outside_storage() {
    use std::os::unix::fs::symlink;
    let mut e = env();
    let outside = e.h.root().join("outside.jsonl");
    append(&outside, &agent("secret"));
    let f = e.claude_file(SID);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    symlink(&outside, &f).unwrap();
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    assert!(page.items.is_empty());
    // A real file, then swapped for a symlink to the outside file after subscribing.
    fs::remove_file(&f).unwrap();
    let t2 = e.open("claude", Some(SID_B));
    let f2 = e.claude_file(SID_B);
    append(&f2, &agent("inside"));
    let page2 = subscribe(&mut m, t2).unwrap();
    assert_eq!(texts(&page2.items), ["a:inside"]);
    for _ in 0..20 {
        append(&outside, &agent("secret"));
    }
    fs::remove_file(&f2).unwrap();
    symlink(&outside, &f2).unwrap();
    let mut got = quiet_for(&mut m, t2, POLL * 10);
    got.extend(quiet_for(&mut m, t, POLL));
    for a in got {
        assert!(
            !texts(&a.items).iter().any(|s| s.contains("secret")),
            "{a:?}"
        );
    }
}

#[test]
fn uuid_reused_as_shell_ends_stream() {
    let mut e = env();
    let f = e.claude_file(SID);
    append(&f, &agent("one"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    subscribe(&mut m, t).unwrap();
    assert_eq!(e.srv.session_subs(), 1);
    // Closed (ended and removed) and reopened under the same UUID as a shell.
    e.desk
        .request(&ClientMsg::TermClose { terminal: t })
        .unwrap();
    e.desk
        .terminals_where(|l| l.iter().all(|i| i.terminal != t));
    e.desk.open(t, sh_spec(&e.cwd));
    append(&f, &agent("after"));
    let deadline = Instant::now() + T;
    while e.srv.session_subs() != 0 {
        assert!(Instant::now() < deadline, "still subscribed");
        std::thread::sleep(POLL);
    }
    assert!(quiet_for(&mut m, t, POLL * 4).is_empty());
}

#[test]
fn unsubscribe_and_disconnect_release() {
    let mut e = env();
    let f = e.claude_file(SID);
    append(&f, &agent("one"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    subscribe(&mut m, t).unwrap();
    // A re-subscribe replaces the subscription.
    subscribe(&mut m, t).unwrap();
    assert_eq!(e.srv.session_subs(), 1);
    assert_eq!(
        m.request(&ClientMsg::SessionUnsubscribe { terminal: t }),
        Ok(Value::Null)
    );
    assert_eq!(e.srv.session_subs(), 0);
    append(&f, &agent("two"));
    assert!(quiet_for(&mut m, t, POLL * 6).is_empty());
    // Unsubscribing again is fine.
    assert!(m
        .request(&ClientMsg::SessionUnsubscribe { terminal: t })
        .is_ok());
    let mut m2 = e.mobile();
    subscribe(&mut m2, t).unwrap();
    assert_eq!(e.srv.session_subs(), 1);
    m2.shutdown();
    let deadline = Instant::now() + T;
    while e.srv.session_subs() != 0 {
        assert!(Instant::now() < deadline, "not released on disconnect");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn subscription_cap() {
    let mut e = env();
    let ts: Vec<Uuid> = (0..9).map(|_| e.open("claude", Some(SID))).collect();
    let mut m = e.mobile();
    for t in &ts[..8] {
        subscribe(&mut m, *t).unwrap();
    }
    assert_eq!(
        subscribe(&mut m, ts[8]).unwrap_err(),
        "too many session subscriptions"
    );
    // Another connection has its own.
    let mut m2 = e.mobile();
    subscribe(&mut m2, ts[8]).unwrap();
    // Re-subscribing to one already held is not a new one.
    subscribe(&mut m, ts[0]).unwrap();
    assert_eq!(e.srv.session_subs(), 9);
}

#[test]
fn page_flood_is_bounded_and_others_stay_prompt() {
    let mut e = env();
    // A long history of lines that give no entries: every page scans its whole budget.
    let f = e.claude_file(SID);
    {
        let mut w = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open({
                fs::create_dir_all(f.parent().unwrap()).unwrap();
                &f
            })
            .unwrap();
        append(&f, &agent("start"));
        let filler = format!(
            "{}\n",
            json!({"type": "progress", "data": "x".repeat(1000)})
        );
        for _ in 0..20_000 {
            w.write_all(filler.as_bytes()).unwrap();
        }
    }
    let flooded = e.open("claude", Some(SID));
    let quiet = e.open("claude", Some(SID_B));
    let fb = e.claude_file(SID_B);
    append(&fb, &agent("b0"));
    let mut a = e.mobile();
    let page = subscribe(&mut a, flooded).unwrap();
    let before = page.before.expect("older");
    let mut b = e.mobile();
    subscribe(&mut b, quiet).unwrap();
    let ids: Vec<u64> = (0..40)
        .map(|_| {
            let id = a.request_id();
            a.send(
                &ClientMsg::SessionPage {
                    terminal: flooded,
                    gen: page.gen,
                    before,
                    limit: None,
                },
                Some(id),
            );
            id
        })
        .collect();
    let started = Instant::now();
    append(&fb, &agent("b1"));
    let got = next_append(&mut b, quiet);
    assert_eq!(texts(&got.items), ["a:b1"]);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let refused = ids
        .iter()
        .filter(|id| {
            a.wait_res(**id)
                .is_err_and(|e| e == "too many session requests")
        })
        .count();
    assert!(refused > 0 && refused < 40, "{refused}");
}

#[test]
fn huge_entries_stay_within_the_message_budget() {
    let mut e = env();
    let f = e.claude_file(SID);
    append(&f, &agent("start"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    subscribe(&mut m, t).unwrap();
    // A multi-MB command, and one line with many large blocks.
    append(&f, &tool_use("big", &"y".repeat(3 * 1024 * 1024)));
    let blocks: Vec<Value> = (0..40)
        .map(|i| json!({"type": "text", "text": format!("{i}{}", "\u{1}".repeat(30_000))}))
        .collect();
    append(
        &f,
        &json!({"type": "assistant", "message": {"role": "assistant", "content": blocks}}),
    );
    let mut n = 0;
    while n < 2 {
        let a = next_append(&mut m, t);
        n += a.items.len();
        let size = append_size(t, &a);
        assert!(size <= CHAT_PAGE_MAX_BYTES, "{size}");
        for it in &a.items {
            if let ChatItem::ToolCall {
                summary, truncated, ..
            } = &it.item
            {
                assert!(*truncated && summary.len() <= 200);
            }
        }
    }
}

/// The frame size of `a` as the Daemon sent it (re-encoding gives the same bytes).
fn append_size(t: Uuid, a: &Append) -> usize {
    let msg = ServerMsg::SessionAppend {
        terminal: t,
        gen: a.gen,
        reset: a.reset,
        session: a.session.clone(),
        items: a.items.clone(),
        before: a.before,
    };
    encode_msg(&msg, None).unwrap().len()
}

/// Send `msg` and return its `res` and that frame's size.
fn sized_request(c: &mut Client, msg: &ClientMsg) -> (Result<Value, String>, usize) {
    let id = c.request_id();
    c.send(msg, Some(id));
    let m = c.expect_msg("res", |m| matches!(m, ServerMsg::Res(r) if r.id == id));
    let size = encode_msg(&m, None).unwrap().len();
    let ServerMsg::Res(r) = m else { unreachable!() };
    (r.outcome.into_result(), size)
}

#[test]
fn page_and_append_messages_fill_up_to_the_budget() {
    let mut e = env();
    let f = e.claude_file(SID);
    // Results at their cap of characters that JSON spends 6 bytes on: ~24 KiB each, so a
    // page stops at the byte budget long before its count.
    let res = |i: usize| tool_result(&format!("t{i}"), &format!("{i}{}", "\u{1}".repeat(5000)));
    for i in 0..60 {
        append(&f, &res(i));
    }
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let near = CHAT_PAGE_MAX_BYTES - 32 * 1024;
    let (r, size) = sized_request(
        &mut m,
        &ClientMsg::SessionSubscribe {
            terminal: t,
            limit: Some(200),
        },
    );
    let page: SessionPage = serde_json::from_value(r.unwrap()).unwrap();
    assert!(size <= CHAT_PAGE_MAX_BYTES && size > near, "{size}");
    assert!(page.items.len() > 1 && page.items.len() < 60);
    let (r, size) = sized_request(
        &mut m,
        &ClientMsg::SessionPage {
            terminal: t,
            gen: page.gen,
            before: page.before.unwrap(),
            limit: Some(200),
        },
    );
    let older: SessionPage = serde_json::from_value(r.unwrap()).unwrap();
    assert!(size <= CHAT_PAGE_MAX_BYTES && size > near, "{size}");
    assert!(older.items.len() > 1);
    // A burst of appended lines: split into appends that each stay within the budget.
    // Written at once, so one read finds them all.
    let burst: String = (60..100).map(|i| format!("{}\n", res(i))).collect();
    fs::OpenOptions::new()
        .append(true)
        .open(&f)
        .unwrap()
        .write_all(burst.as_bytes())
        .unwrap();
    let (mut n, mut largest) = (0, 0);
    while n < 40 {
        let a = next_append(&mut m, t);
        assert!(!a.reset);
        n += a.items.len();
        let size = append_size(t, &a);
        assert!(size <= CHAT_PAGE_MAX_BYTES, "{size}");
        largest = largest.max(size);
    }
    assert_eq!(n, 40);
    assert!(largest > near, "{largest}");
}

#[test]
fn older_page_refused_after_rewrite_in_place() {
    let mut e = env_with(|c| c.session_poll = Duration::from_secs(3600));
    let f = e.claude_file(SID);
    for i in 0..60 {
        append(&f, &agent(&format!("old {i}")));
    }
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    let before = page.before.expect("older entries");
    // The same file (same inode) rewritten with other history, longer than before, and no
    // read of it since.
    let rewritten: String = (0..80)
        .map(|i| format!("{}\n", agent(&format!("new history {i}"))))
        .collect();
    fs::write(&f, rewritten).unwrap();
    assert_eq!(
        older(&mut m, t, page.gen, before).unwrap_err(),
        SESSION_CHANGED
    );
    // The refusal scheduled the reset: it comes without a poll or a hook.
    let a = next_append(&mut m, t);
    assert!(a.reset && a.gen > page.gen, "{a:?}");
    assert_eq!(text(a.items.last().unwrap()), "a:new history 79");
    assert!(texts(&a.items)
        .iter()
        .all(|s| s.starts_with("a:new history")));
}

#[test]
fn queued_requests_bounded_over_all_connections() {
    let hold = Hold::default();
    let hook = hold.hook();
    let mut e = env_with(|c| {
        c.test_hook = Some(hook);
        c.max_session_queue = 4;
    });
    append(&e.claude_file(SID), &agent("one"));
    let t = e.open("claude", Some(SID));
    // The worker is busy with one connection's subscribe…
    let mut a = e.mobile();
    let (held, release) = hold.arm();
    let sub_id = a.request_id();
    a.send(
        &ClientMsg::SessionSubscribe {
            terminal: t,
            limit: None,
        },
        Some(sub_id),
    );
    held.recv_timeout(T).expect("the subscribe's read");
    // …while three others queue three requests each: four fit, over all of them.
    let mut cs: Vec<Client> = (0..3).map(|_| e.mobile()).collect();
    let mut sent: Vec<(usize, u64)> = Vec::new();
    for (i, c) in cs.iter_mut().enumerate() {
        for _ in 0..3 {
            let id = c.request_id();
            c.send(&ClientMsg::SessionUnsubscribe { terminal: t }, Some(id));
            sent.push((i, id));
        }
    }
    let refused = |r: &Option<ServerMsg>| {
        matches!(r, Some(ServerMsg::Res(r))
            if r.outcome.clone().into_result() == Err("too many session requests".into()))
    };
    let mut accepted = Vec::new();
    for (i, id) in &sent {
        let r = cs[*i].try_msg(
            Duration::from_millis(300),
            |m| matches!(m, ServerMsg::Res(r) if r.id == *id),
        );
        if refused(&r) {
            continue;
        }
        assert!(r.is_none(), "answered while the worker is busy: {r:?}");
        accepted.push((*i, *id));
    }
    assert_eq!(accepted.len(), 4, "{accepted:?}");
    // A connection that goes away frees its places; cleanup needs none.
    let gone = accepted[0].0;
    let freed = accepted.iter().filter(|(i, _)| *i == gone).count();
    cs[gone].shutdown();
    let deadline = Instant::now() + T;
    while e.srv.connections() > 4 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut late = e.mobile();
    for _ in 0..freed {
        let id = late.request_id();
        late.send(&ClientMsg::SessionUnsubscribe { terminal: t }, Some(id));
        accepted.push((usize::MAX, id));
    }
    let id = late.request_id();
    late.send(&ClientMsg::SessionUnsubscribe { terminal: t }, Some(id));
    assert_eq!(late.wait_res(id).unwrap_err(), "too many session requests");
    release.send(()).unwrap();
    assert!(a.wait_res(sub_id).is_ok());
    for (i, id) in accepted {
        if i == gone {
            continue;
        }
        let c = if i == usize::MAX {
            &mut late
        } else {
            &mut cs[i]
        };
        assert_eq!(c.wait_res(id), Ok(Value::Null));
    }
}

#[test]
fn backpressure_defers_not_drops() {
    let mut e = env_with(|c| {
        c.conn_total_cap = 2 * 1024 * 1024;
        c.write_stall_timeout = Duration::from_secs(30);
    });
    let f = e.claude_file(SID);
    append(&f, &agent("start"));
    let t = e.open("claude", Some(SID));
    // A Mobile that subscribes, then stops reading.
    let s = e.srv.connect_in_process(Role::Mobile).unwrap();
    let mut w = s.try_clone().unwrap();
    let hello = ClientMsg::Hello(Hello {
        protocol: range(1, 1),
        version: "t".into(),
        capabilities: vec![],
    });
    w.write_all(&encode_msg(&hello, None).unwrap()).unwrap();
    let sub = ClientMsg::SessionSubscribe {
        terminal: t,
        limit: None,
    };
    w.write_all(&encode_msg(&sub, Some(1)).unwrap()).unwrap();
    let mut r = BufReader::new(s);
    r.get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let next_msg = |r: &mut BufReader<_>| loop {
        if let Some(Frame::Json(j)) = read_frame(r, MAX_FRAME_LEN).expect("connected") {
            return decode_server(&j).unwrap();
        }
    };
    // The subscription stands, with the history so far, before the backlog is written.
    let mut got: Vec<String> = loop {
        if let ServerMsg::Res(res) = next_msg(&mut r) {
            assert_eq!(res.id, 1);
            let p: SessionPage =
                serde_json::from_value(res.outcome.into_result().unwrap()).unwrap();
            break p.items.iter().map(text).collect();
        }
    };
    assert_eq!(got, ["a:start"]);
    // 4 MiB of messages while it does not read.
    let lines = 2000;
    for i in 0..lines {
        append(&f, &agent(&format!("{i} {}", "z".repeat(2000))));
    }
    std::thread::sleep(Duration::from_millis(1500));
    // Now it reads everything: in order, nothing lost, still connected.
    let deadline = Instant::now() + Duration::from_secs(20);
    while got.len() < lines + 1 {
        assert!(Instant::now() < deadline, "only {} entries", got.len());
        if let ServerMsg::SessionAppend { reset, items, .. } = next_msg(&mut r) {
            assert!(!reset);
            got.extend(items.iter().map(text));
        }
    }
    let want: Vec<String> = std::iter::once("a:start".to_string())
        .chain((0..lines).map(|i| format!("a:{i} {}", "z".repeat(2000))))
        .collect();
    assert_eq!(got.len(), want.len());
    assert!(got == want, "out of order or duplicated");
    assert_eq!(e.srv.connections(), 2);
}

#[test]
fn hello_advertises_session_stream() {
    let e = env();
    let mut c = Client::connect(&e.srv.socket);
    let (h, _) = c.hello(range(1, 1));
    assert!(h.capabilities.iter().any(|c| c == "session.stream"));
    let e2 = env_with(|c| c.hide_capabilities = vec!["session.stream".into()]);
    let mut c = Client::connect(&e2.srv.socket);
    let (h, _) = c.hello(range(1, 1));
    assert!(!h.capabilities.iter().any(|c| c == "session.stream"));
    assert!(h.capabilities.iter().any(|c| c == "term"));
}

#[test]
fn relaunch_keeps_subscription() {
    let mut e = env();
    let f = e.claude_file(SID);
    append(&f, &agent("before"));
    let t = e.open("claude", Some(SID));
    let mut m = e.mobile();
    let page = subscribe(&mut m, t).unwrap();
    let pids = || {
        fs::read_to_string(e.cwd.join("pids.log"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    let n = pids();
    e.desk
        .request(&ClientMsg::TermRelaunch {
            terminal: t,
            skip_permissions: true,
        })
        .unwrap();
    let deadline = Instant::now() + T;
    while pids() <= n {
        assert!(Instant::now() < deadline, "not relaunched");
        std::thread::sleep(Duration::from_millis(20));
    }
    append(&f, &agent("after"));
    let a = next_append(&mut m, t);
    assert_eq!(
        (texts(&a.items), a.reset, a.gen),
        (vec!["a:after".into()], false, page.gen)
    );
}

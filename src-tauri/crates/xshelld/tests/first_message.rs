#![cfg(unix)]
//! A new chat's first message (`term.open`'s `firstMessage`, capability `term.first-message`):
//! the agent gets it as its last argv word after `--`, once. It is never persisted, listed,
//! restored or relaunched, and it is refused for anything but a new direct Claude Code or
//! Codex chat, before anything starts.

mod common;

use common::*;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_protocol::msg::ClientMsg;
use xshelld::server::{Role, ServerHandle, TestHook, TestPoint};

const SID: &str = "11111111-2222-3333-4444-555555555555";
const SKIP: &str = "--dangerously-skip-permissions";
const FORBIDDEN: &str = "forbidden for mobile";
const MSG: &str = "fix the failing test";

struct Env {
    srv: ServerHandle,
    desk: Client,
    mob: Client,
    /// A Project the Host knows from Claude history for `SID`.
    cwd: PathBuf,
    fake: Fake,
    _reaper: FakeReaper,
    h: TestHome,
}

fn env() -> Env {
    env_with(|_| {})
}

fn env_with(tweak: impl FnOnce(&mut xshelld::server::Config)) -> Env {
    let h = TestHome::new();
    let cwd = h.project("app");
    claude_history(&h, &cwd, SID);
    let fake = Fake::in_dir(&cwd);
    let srv = start(&h, tweak);
    Env {
        desk: Client::in_process(&srv, Role::Desktop),
        mob: Client::in_process(&srv, Role::Mobile),
        srv,
        _reaper: FakeReaper(fake.pids_log.clone()),
        fake,
        cwd,
        h,
    }
}

fn first_msg(t: Uuid, launch: LaunchSpec, msg: &str) -> ClientMsg {
    let ClientMsg::TermOpen { mut spec } = open_msg(t, launch) else {
        unreachable!()
    };
    spec.first_message = Some(msg.into());
    ClientMsg::TermOpen { spec }
}

/// A new Claude chat: a fresh session id with no session file yet.
fn new_claude(cwd: &Path) -> (LaunchSpec, String) {
    let sid = Uuid::new_v4().to_string();
    (claude_spec(cwd, Some(&sid)), sid)
}

fn new_codex(cwd: &Path) -> LaunchSpec {
    LaunchSpec {
        agent: Some("codex".into()),
        ..claude_spec(cwd, None)
    }
}

trait After {
    /// The raw argv of launch `n` (0-based), once it is logged.
    fn raw_launches_after(&self, n: usize) -> Vec<String>;
}

impl After for Fake {
    fn raw_launches_after(&self, n: usize) -> Vec<String> {
        self.wait_launches(n + 1);
        self.raw_launches().swap_remove(n)
    }
}

/// The words after the agent's own arguments: everything from the last `--` on.
fn tail(argv: &[String]) -> &[String] {
    let at = argv
        .iter()
        .rposition(|a| a == "--")
        .expect("a `--` in argv");
    &argv[at..]
}

/// How many Terminals a new connection is told about.
fn listed(srv: &ServerHandle) -> usize {
    let s = srv.connect_in_process(Role::Desktop).unwrap();
    let mut c = Client::from_io(s.try_clone().unwrap(), s);
    c.hello(range(1, 1)).1.len()
}

// ── Opens ─────────────────────────────────────────────────────────────────

#[test]
fn hello_advertises_term_first_message() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let s = srv.connect_in_process(Role::Mobile).unwrap();
    let mut c = Client::from_io(s.try_clone().unwrap(), s);
    let (hello, _) = c.hello(range(1, 1));
    assert!(
        hello.capabilities.iter().any(|c| c == "term.first-message"),
        "{:?}",
        hello.capabilities
    );
}

#[test]
fn mobile_new_claude_chat_with_first_message() {
    let mut e = env();
    let (spec, sid) = new_claude(&e.cwd);
    let t = Uuid::new_v4();
    let r = e.mob.request(&first_msg(t, spec, MSG)).unwrap();
    assert!(r["pid"].is_u64(), "{r}");
    assert_eq!(
        e.fake.wait_launches(1)[0],
        ["--session-id", sid.as_str(), "--", MSG]
    );
    // The message comes after the hook arguments: last, as one word.
    let raw = &e.fake.raw_launches()[0];
    let settings = raw.iter().position(|a| a == "--settings").unwrap();
    assert!(settings < raw.len() - 2, "{raw:?}");
    assert_eq!(tail(raw), ["--", MSG]);
    // Listed for the Desktop like any new chat.
    let list = e
        .desk
        .terminals_where(|l| l.iter().any(|i| i.terminal == t));
    assert_eq!(list[0].spec.session_id.as_deref(), Some(sid.as_str()));
}

#[test]
fn mobile_new_codex_chat_with_first_message() {
    let mut e = env();
    let cx = e.h.project("cx");
    codex_history(&e.h, &cx);
    let fake = Fake::in_dir(&cx);
    let _reaper = FakeReaper(fake.pids_log.clone());
    e.mob
        .request(&first_msg(Uuid::new_v4(), new_codex(&cx), "add a test"))
        .unwrap();
    assert_eq!(fake.wait_launches(1)[0], ["--", "add a test"]);
    let raw = &fake.raw_launches()[0];
    assert_eq!(raw[0], "-c", "the hook overrides come first: {raw:?}");
    assert_eq!(tail(raw), ["--", "add a test"]);
}

#[test]
fn single_word_and_dash_messages_stay_the_prompt() {
    let mut e = env();
    let (spec, sid) = new_claude(&e.cwd);
    e.mob
        .request(&first_msg(Uuid::new_v4(), spec, "update"))
        .unwrap();
    let (spec2, sid2) = new_claude(&e.cwd);
    e.desk
        .request(&first_msg(Uuid::new_v4(), spec2, "--version"))
        .unwrap();
    let l = e.fake.wait_launches(2);
    assert!(l.contains(&vec![
        "--session-id".into(),
        sid,
        "--".into(),
        "update ".into()
    ]));
    assert!(l.contains(&vec![
        "--session-id".into(),
        sid2,
        "--".into(),
        "--version ".into()
    ]));
}

/// The message reaches the agent as exactly one argv word, whatever it holds: lines, text
/// that looks like hook arguments or like the recorder's own framing.
#[test]
fn awkward_messages_stay_one_argument() {
    let mut e = env();
    let msgs = [
        "first line\nsecond line\n",
        "<argv-end>\n<argv-end> 3",
        "--settings x",
        "-c y",
        "please run --settings x -c y -- --resume z",
    ];
    for msg in msgs {
        let (spec, sid) = new_claude(&e.cwd);
        let n = e.fake.launches().len();
        e.mob
            .request(&first_msg(Uuid::new_v4(), spec, msg))
            .unwrap();
        let raw = e.fake.raw_launches_after(n);
        assert_eq!(raw.len(), 4 + 2, "{raw:?}");
        assert_eq!(
            raw,
            [
                "--session-id",
                sid.as_str(),
                "--settings",
                raw[3].as_str(),
                "--",
                msg
            ],
            "{msg:?}"
        );
        assert!(raw[3].ends_with(".json"), "{raw:?}");
        assert_eq!(
            e.fake.launches()[n],
            ["--session-id", sid.as_str(), "--", msg],
            "{msg:?}"
        );
    }
    let cx = e.h.project("cx");
    codex_history(&e.h, &cx);
    let fake = Fake::in_dir(&cx);
    let _reaper = FakeReaper(fake.pids_log.clone());
    e.mob
        .request(&first_msg(
            Uuid::new_v4(),
            new_codex(&cx),
            "-c y\n--settings x",
        ))
        .unwrap();
    let raw = fake.raw_launches_after(0);
    let sep = raw.iter().position(|a| a == "--").unwrap();
    assert!(raw[..sep].chunks(2).all(|p| p[0] == "-c"), "{raw:?}");
    assert_eq!(raw[sep..], ["--", "-c y\n--settings x"]);
    assert_eq!(fake.launches()[0], ["--", "-c y\n--settings x"]);
}

#[test]
fn first_message_not_persisted_or_listed() {
    let mut e = env();
    let (spec, sid) = new_claude(&e.cwd);
    let t = Uuid::new_v4();
    e.mob.request(&first_msg(t, spec, MSG)).unwrap();
    e.fake.wait_pids(1);
    let list = e
        .desk
        .terminals_where(|l| l.iter().any(|i| i.terminal == t));
    let listed = serde_json::to_string(&list).unwrap();
    assert!(!listed.contains(MSG), "{listed}");
    assert!(!listed.contains("firstMessage"), "{listed}");
    let state = e.h.state_json().to_string();
    assert!(state.contains(&t.to_string()), "{state}");
    assert!(!state.contains(MSG), "{state}");

    // Restore: the agent's session exists by now, and the message is not sent again.
    make_jsonl(&e.h, &e.cwd, &sid);
    let Env {
        srv,
        h,
        fake,
        desk,
        mob,
        ..
    } = e;
    drop((desk, mob));
    srv.shutdown();
    let _srv = start(&h, |_| {});
    let l = fake.wait_launches(2);
    assert_eq!(l.len(), 2, "{l:?}");
    assert_eq!(l[1], ["--resume", sid.as_str()]);
    assert!(!fake.raw_launches()[1].iter().any(|a| a == "--" || a == MSG));
    let state = h.state_json().to_string();
    assert!(!state.contains(MSG), "{state}");
}

#[test]
fn relaunch_after_first_message_has_no_message() {
    let mut e = env();
    let (spec, sid) = new_claude(&e.cwd);
    let t = Uuid::new_v4();
    e.mob.request(&first_msg(t, spec, MSG)).unwrap();
    e.fake.wait_pids(1);
    // The session file exists once the agent has the conversation.
    make_jsonl(&e.h, &e.cwd, &sid);
    let r = e
        .mob
        .request(&ClientMsg::TermRelaunch {
            terminal: t,
            skip_permissions: true,
        })
        .unwrap();
    assert_eq!(r["relaunched"], json!(true));
    let l = e.fake.wait_launches(2);
    assert_eq!(l[1], [SKIP, "--resume", sid.as_str()]);
    assert!(!e.fake.raw_launches()[1].iter().any(|a| a == "--"));
}

// ── Refusals ──────────────────────────────────────────────────────────────

#[track_caller]
fn refusal(r: Result<serde_json::Value, String>) -> String {
    let e = r.expect_err("refused");
    // A refusal: nothing runs. Never the "may still run" outcome.
    assert!(!e.starts_with(xshell_protocol::OPEN_INDETERMINATE), "{e}");
    e
}

#[test]
fn first_message_unknown_project_refused() {
    let mut e = env();
    let other = e.h.project("other");
    let (spec, _) = new_claude(&other);
    let err = refusal(e.mob.request(&first_msg(Uuid::new_v4(), spec, MSG)));
    assert!(err.starts_with(FORBIDDEN), "{err}");
    assert_eq!(listed(&e.srv), 0);
    std::thread::sleep(Duration::from_millis(200));
    assert!(Fake::in_dir(&other).launches().is_empty());
    assert!(e.fake.launches().is_empty());
}

#[test]
fn first_message_with_existing_session_refused() {
    let mut e = env();
    let cx = e.h.project("cx");
    codex_history(&e.h, &cx);
    // An existing Claude session (`SID` has a session file) and a Codex resume.
    let resumes = [
        claude_spec(&e.cwd, Some(SID)),
        LaunchSpec {
            session_id: Some("r1".into()),
            ..new_codex(&cx)
        },
    ];
    for spec in resumes {
        for c in [&mut e.mob, &mut e.desk] {
            let err = refusal(c.request(&first_msg(Uuid::new_v4(), spec.clone(), MSG)));
            assert_eq!(err, "a first message needs a new chat");
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(e.fake.launches().is_empty());
    assert!(Fake::in_dir(&cx).launches().is_empty());
    assert_eq!(listed(&e.srv), 0);
}

#[test]
fn first_message_refused_for_shells_and_bad_messages() {
    let mut e = env();
    let (ok, _) = new_claude(&e.cwd);
    let cases = [
        (sh_spec(&e.cwd), MSG, "needs Claude Code or Codex"),
        (
            LaunchSpec {
                agent: Some("cursor".into()),
                ..claude_spec(&e.cwd, None)
            },
            MSG,
            "needs Claude Code or Codex",
        ),
        (
            LaunchSpec {
                launch_prefix: Some(vec!["env".into()]),
                ..ok.clone()
            },
            MSG,
            "needs Claude Code or Codex",
        ),
        (ok.clone(), "  \n ", "must not be blank"),
        (ok.clone(), "a\0b c", "must not contain NUL"),
    ];
    for (spec, msg, want) in cases {
        let err = refusal(e.desk.request(&first_msg(Uuid::new_v4(), spec, msg)));
        assert!(err.contains(want), "{err}");
    }
    let long = "x".repeat(xshell_protocol::msg::FIRST_MESSAGE_MAX_BYTES + 1);
    let err = refusal(e.desk.request(&first_msg(Uuid::new_v4(), ok, &long)));
    assert!(err.contains("longer than"), "{err}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(e.fake.launches().is_empty());
    assert_eq!(listed(&e.srv), 0);
}

// ── Outcome unknown ───────────────────────────────────────────────────────

/// A Terminal that started with its first message but could not be saved, and whose end is
/// not confirmed in time: the reply says the outcome is open, which the Mobile tells apart
/// from a refusal (the message was delivered to an agent that may still run).
#[test]
fn first_message_open_indeterminate_stays_distinguishable() {
    let pids = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
    let p2 = pids.clone();
    let mut e = env_with(move |c| {
        c.kill_grace = Duration::from_secs(5);
        c.refused_open_wait = Some(Duration::from_millis(300));
        c.test_hook = Some(TestHook(std::sync::Arc::new(move |_, p| {
            if p == TestPoint::RefusedOpen {
                // The fake has set its HUP trap once its pid is logged.
                let f = p2.lock().unwrap().clone();
                let t0 = Instant::now();
                while std::fs::read_to_string(&f).unwrap_or_default().is_empty() && t0.elapsed() < T
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            false
        })));
    });
    *pids.lock().unwrap() = e.fake.pids_log.clone();
    let tmp = e.h.paths().state.with_file_name("terminals.json.tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let (spec, sid) = new_claude(&e.cwd);
    let err = e
        .mob
        .request(&first_msg(Uuid::new_v4(), spec, MSG))
        .unwrap_err();
    assert!(
        err.starts_with(xshell_protocol::OPEN_INDETERMINATE),
        "{err}"
    );
    assert_eq!(
        e.fake.launches()[0],
        ["--session-id", sid.as_str(), "--", MSG]
    );
    assert_eq!(listed(&e.srv), 0);
    let pid = e.fake.pids()[0];
    assert!(wait_dead(pid, Duration::from_secs(10)));
}

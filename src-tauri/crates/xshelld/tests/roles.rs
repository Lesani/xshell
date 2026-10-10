#![cfg(unix)]
//! Connection roles: what a Mobile may do (ADR-0004). Each refusal is checked against a
//! Desktop connection to the same server doing the same thing. Mobile connections are served
//! in process (`ServerHandle::connect_in_process`) until the Relay carries them.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;
use xshell_core::claude::encode_project_name;
use xshell_core::launch::LaunchSpec;
use xshell_protocol::frame::Frame;
use xshell_protocol::msg::{
    ClientMsg, Hello, OpenSpec, PastSessionsPage, ServerMsg, PROMPT_ANSWERED, SUBMIT_NOT_READY,
};
use xshelld::server::{ExitReason, Role, ServerHandle};

const SID: &str = "11111111-2222-3333-4444-555555555555";
const SKIP: &str = "--dangerously-skip-permissions";
const FORBIDDEN: &str = "forbidden for mobile";

struct Env {
    desk: Client,
    mob: Client,
    srv: ServerHandle,
    /// A Project the Host knows from Claude history for `SID`.
    cwd: PathBuf,
    fake: Fake,
    _reaper: FakeReaper,
    h: TestHome,
}

fn env() -> Env {
    let h = TestHome::new();
    let cwd = h.project("app");
    claude_history(&h, &cwd, SID);
    let fake = Fake::in_dir(&cwd);
    let srv = start(&h, |_| {});
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

impl Env {
    fn claude(&self) -> LaunchSpec {
        claude_spec(&self.cwd, Some(SID))
    }

    fn shell(&self) -> LaunchSpec {
        sh_spec(&self.cwd)
    }

    /// A Terminal the Desktop opened with `title` in its meta.
    fn desk_titled(&mut self, spec: LaunchSpec, title: &str) -> Uuid {
        open_titled(&mut self.desk, spec, title)
    }

    /// A Terminal the Desktop opened.
    fn desk_open(&mut self, spec: LaunchSpec) -> Uuid {
        let t = Uuid::new_v4();
        self.desk.open(t, spec);
        t
    }
}

#[track_caller]
fn refused(r: Result<Value, String>) {
    let e = r.expect_err("refused for a Mobile");
    assert!(e.starts_with(FORBIDDEN), "{e}");
}

fn try_open(c: &mut Client, spec: LaunchSpec) -> Result<Value, String> {
    c.request(&open_msg(Uuid::new_v4(), spec))
}

/// Every message about one Terminal, each with an id.
fn per_terminal(t: Uuid) -> Vec<ClientMsg> {
    vec![
        ClientMsg::TermAttach { terminal: t },
        ClientMsg::TermInput {
            terminal: t,
            data: "echo hi\n".into(),
        },
        ClientMsg::TermResize {
            terminal: t,
            cols: 100,
            rows: 30,
        },
        ClientMsg::TermDetach { terminal: t },
        ClientMsg::TermSubmit {
            terminal: t,
            text: "reply".into(),
        },
        ClientMsg::TermRelaunch {
            terminal: t,
            skip_permissions: true,
        },
        ClientMsg::TermClose { terminal: t },
    ]
}

/// The Terminals a new connection is told about.
fn listed(srv: &ServerHandle) -> Vec<Uuid> {
    let s = srv.connect_in_process(Role::Desktop).unwrap();
    let mut c = Client::from_io(s.try_clone().unwrap(), s);
    let list = c.hello(range(1, 1)).1;
    list.iter().map(|t| t.terminal).collect()
}

// ── Refusals ──────────────────────────────────────────────────────────────

#[test]
fn mobile_refuses_raw_shell_open_desktop_opens() {
    let mut e = env();
    let t = Uuid::new_v4();
    refused(e.mob.request(&open_msg(t, e.shell())));
    assert!(listed(&e.srv).is_empty());
    e.desk.open(t, e.shell());
    e.desk.attach(t);
    e.desk.marker(t, "deskok");
}

#[test]
fn mobile_refuses_open_variants() {
    let mut e = env();
    let other = e.h.project("other");
    let ok = e.claude();
    let variants = [
        (
            "shell_command",
            LaunchSpec {
                shell_command: Some("/bin/sh".into()),
                ..ok.clone()
            },
        ),
        (
            "shell_id",
            LaunchSpec {
                shell_id: Some("bash".into()),
                ..ok.clone()
            },
        ),
        (
            "launch_prefix",
            LaunchSpec {
                launch_prefix: Some(vec!["env".into()]),
                ..ok.clone()
            },
        ),
        (
            "agent cursor",
            LaunchSpec {
                agent: Some("cursor".into()),
                ..ok.clone()
            },
        ),
        (
            "agent none",
            LaunchSpec {
                agent: None,
                ..ok.clone()
            },
        ),
        ("unknown project", claude_spec(&other, Some(SID))),
        (
            "empty cwd",
            LaunchSpec {
                cwd: String::new(),
                ..ok.clone()
            },
        ),
        (
            "option session id",
            LaunchSpec {
                session_id: Some("-cx".into()),
                ..ok.clone()
            },
        ),
        (
            "shell_mode weird",
            LaunchSpec {
                shell_mode: Some("weird".into()),
                ..ok.clone()
            },
        ),
    ];
    for (what, spec) in variants {
        let e2 = try_open(&mut e.mob, spec.clone()).expect_err(what);
        assert!(e2.starts_with(FORBIDDEN), "{what}: {e2}");
        let r = try_open(&mut e.desk, spec);
        assert!(r.as_ref().is_ok_and(|v| v["pid"].is_u64()), "{what}: {r:?}");
    }
    // No Mobile launch reached the agent.
    assert_eq!(e.fake.wait_launches(7).len(), 7);
}

#[test]
fn mobile_refuses_file_git_and_probe_calls() {
    let mut e = env();
    if !git_fixture(&e.cwd) {
        eprintln!("git not available; skipping");
        return;
    }
    let cwd = e.cwd.to_string_lossy().into_owned();
    let calls = [
        ("read_text_file", json!({ "path": e.cwd.join("a.txt") })),
        ("list_dir", json!({ "path": cwd })),
        ("search_dir", json!({ "root": cwd, "query": "a" })),
        ("get_git_status", json!({ "cwd": cwd })),
        ("get_git_log", json!({ "cwd": cwd })),
        (
            "git_diff",
            json!({ "cwd": cwd, "path": "a.txt", "mode": "unstaged" }),
        ),
        ("git_stage", json!({ "cwd": cwd, "paths": ["a.txt"] })),
        ("git_unstage", json!({ "cwd": cwd, "paths": ["a.txt"] })),
        ("list_git_branches", json!({ "cwd": cwd })),
        ("git_checkout", json!({ "cwd": cwd, "branch": "other" })),
        (
            "git_discard",
            json!({ "cwd": cwd, "path": "b.txt", "mode": "untracked" }),
        ),
        ("get_project_skills", json!({ "projectPath": cwd })),
        ("get_project_memories", json!({ "projectPath": cwd })),
        ("get_codex_context", json!({ "projectPath": cwd })),
        ("get_cursor_context", json!({ "projectPath": cwd })),
        ("get_opencode_context", json!({ "projectPath": cwd })),
        ("get_antigravity_context", json!({ "projectPath": cwd })),
        ("get_username", json!({})),
        ("get_home_dir", json!({})),
        ("detect_agent_binary", json!({ "binary": "agy" })),
    ];
    for (m, p) in calls {
        let r = e.mob.call(m, p.clone());
        assert_eq!(r, Err(format!("{FORBIDDEN}: call {m}")));
        if m == "git_discard" {
            assert!(
                e.cwd.join("b.txt").exists(),
                "a refused discard changed nothing"
            );
        }
        let d = e.desk.call(m, p);
        assert!(d.is_ok(), "{m}: {d:?}");
    }
    assert!(!e.cwd.join("b.txt").exists());
}

#[test]
fn mobile_unknown_call_forbidden() {
    let mut e = env();
    assert_eq!(
        e.mob.call("no_such_method", json!({})),
        Err(format!("{FORBIDDEN}: call no_such_method"))
    );
    let d = e.desk.call("no_such_method", json!({})).unwrap_err();
    assert!(d.contains("unknown method"), "{d}");
}

#[test]
fn mobile_refuses_term_update_desktop_updates() {
    let mut e = env();
    let t = e.desk_open(e.claude());
    let upd = ClientMsg::TermUpdate {
        terminal: t,
        session_id: Some("-cx".into()),
        meta: None,
    };
    refused(e.mob.request(&upd));
    assert_eq!(e.desk.request(&upd), Ok(Value::Null));
}

#[test]
fn mobile_refuses_daemon_upgrade_desktop_upgrades() {
    let mut e = env();
    refused(e.mob.request(&ClientMsg::DaemonUpgrade));
    assert_eq!(e.srv.wait_timeout(Duration::from_millis(300)), None);
    // Still serving: a refused upgrade froze nothing.
    e.desk_open(e.shell());
    assert_eq!(e.desk.request(&ClientMsg::DaemonUpgrade), Ok(Value::Null));
    assert_eq!(e.srv.wait_timeout(T), Some(ExitReason::Upgrade));
}

#[test]
fn mobile_refuses_term_event_desktop_reports() {
    let mut e = env();
    let t = e.desk_open(e.claude());
    let ev = |run| ClientMsg::TermEvent {
        terminal: t,
        run,
        status: xshell_protocol::msg::AgentStatus::NeedsYou,
        session_id: None,
    };
    // Hooks report from the Host itself; a phone never does.
    refused(e.mob.request(&ev(0)));
    // A Desktop connection gets past the role check (the run decides).
    let d = e.desk.request(&ev(0)).unwrap_err();
    assert_eq!(d, "stale run");
}

#[test]
fn mobile_refuses_ops_on_shell_terminal() {
    let mut e = env();
    // A raw shell, and an agent under a launch prefix that is really a shell.
    let prefixed = LaunchSpec {
        launch_prefix: Some(
            ["bash", "-c", "exec sh -i", "--"]
                .map(String::from)
                .to_vec(),
        ),
        ..e.claude()
    };
    for spec in [e.shell(), prefixed] {
        let t = e.desk_open(spec);
        for m in per_terminal(t) {
            refused(e.mob.request(&m));
        }
        assert!(listed(&e.srv).contains(&t), "not closed");
        // The Desktop still has it, and no Mobile input reached it.
        e.desk.attach(t);
        e.desk.marker(t, "deskok");
        let out = String::from_utf8_lossy(&e.desk.out[&t]).into_owned();
        assert!(!out.contains("hi\r\n"), "{out}");
    }
    // An agent wrapped in a shell. Its fake agent only sleeps, so no marker: the Desktop's
    // attach replays the agent's startup line.
    let t = e.desk_open(wrapped(&e));
    for m in per_terminal(t) {
        refused(e.mob.request(&m));
    }
    assert!(listed(&e.srv).contains(&t), "not closed");
    e.desk.attach(t);
    e.desk.output_until(t, "args");
    assert_eq!(e.srv.attached(), 3);
}

#[test]
fn mobile_refuses_relaunch_of_shell_and_wrapped_agent() {
    let mut e = env();
    let relaunch = |t| ClientMsg::TermRelaunch {
        terminal: t,
        skip_permissions: true,
    };
    let shell = e.desk_open(e.shell());
    refused(e.mob.request(&relaunch(shell)));
    let wrapped = e.desk_open(LaunchSpec {
        shell_command: Some("/bin/sh".into()),
        shell_id: Some("bash".into()),
        ..e.claude()
    });
    refused(e.mob.request(&relaunch(wrapped)));
    let r = e.desk.request(&relaunch(wrapped)).unwrap();
    assert_eq!(r["relaunched"], json!(true));

    // An agent whose session id a Desktop changed into an option: reachable, not relaunchable.
    let t = e.desk_open(e.claude());
    let upd = ClientMsg::TermUpdate {
        terminal: t,
        session_id: Some("-cx".into()),
        meta: None,
    };
    assert_eq!(e.desk.request(&upd), Ok(Value::Null));
    e.mob.attach(t);
    refused(e.mob.request(&relaunch(t)));
}

#[test]
fn mobile_input_without_id_to_shell_dropped() {
    let mut e = env();
    let t = e.desk_open(e.shell());
    e.desk.attach(t);
    e.mob.input(t, "echo mo\"\"bile\n");
    // Ordered after the input on the Mobile's connection: by now it was handled.
    e.mob
        .call("get_all_recent_sessions", json!({ "limit": 1 }))
        .unwrap();
    e.desk.marker(t, "deskok");
    let out = String::from_utf8_lossy(&e.desk.out[&t]).into_owned();
    assert!(!out.contains("mobile"), "{out}");
    // No reply and no error for it: the Mobile's connection carries on.
    assert!(!e
        .mob
        .log
        .iter()
        .any(|m| matches!(m, Ev::Msg(ServerMsg::Error { .. }))));
    assert!(!e.mob.is_eof());
}

// ── Allowed ───────────────────────────────────────────────────────────────

#[test]
fn mobile_opens_claude_in_known_project() {
    let mut e = env();
    let t = Uuid::new_v4();
    let spec = LaunchSpec {
        skip_permissions: Some(true),
        ..e.claude()
    };
    assert!(e.mob.open(t, spec)["pid"].is_u64());
    let argv = &e.fake.wait_launches(1)[0];
    assert_eq!(argv, &[SKIP, "--resume", SID]);
}

#[test]
fn mobile_opens_codex_in_known_project() {
    let mut e = env();
    let cx = e.h.project("cx");
    codex_history(&e.h, &cx);
    let fake = Fake::in_dir(&cx);
    let _reaper = FakeReaper(fake.pids_log.clone());
    let spec = LaunchSpec {
        agent: Some("codex".into()),
        session_id: Some("r1".into()),
        ..claude_spec(&cx, None)
    };
    e.mob.open(Uuid::new_v4(), spec);
    assert_eq!(fake.wait_launches(1)[0], ["resume", "r1"]);
}

#[test]
fn mobile_opens_claude_in_project_known_only_from_cursor() {
    let mut e = env();
    let cur = e.h.project("cur");
    cursor_history(&e.h, &cur);
    let fake = Fake::in_dir(&cur);
    let _reaper = FakeReaper(fake.pids_log.clone());
    e.mob.open(Uuid::new_v4(), claude_spec(&cur, None));
    assert_eq!(fake.wait_launches(1).len(), 1);
}

#[test]
fn mobile_attach_input_resize_detach_close_agent() {
    let mut e = env();
    let t = Uuid::new_v4();
    e.mob.open(t, e.claude());
    e.mob.attach(t);
    e.mob.output_until(t, "args");
    let input = ClientMsg::TermInput {
        terminal: t,
        data: "typed\r".into(),
    };
    assert_eq!(e.mob.request(&input), Ok(Value::Null));
    e.mob.output_until(t, "typed");
    e.mob.resize(t, 100, 30);
    assert_eq!(
        e.mob.request(&ClientMsg::TermDetach { terminal: t }),
        Ok(Value::Null)
    );
    assert_eq!(e.srv.attached(), 0);
    assert_eq!(
        e.mob.request(&ClientMsg::TermClose { terminal: t }),
        Ok(Value::Null)
    );
    e.mob.terminals_where(|l| l.is_empty());
}

/// A Mobile may reply to the agent Terminals it sees: past the role checks, the Daemon
/// judges the agent's screen (this fake shows no composer).
#[test]
fn mobile_may_submit() {
    let mut e = env();
    let t = e.desk_open(e.claude());
    let submit = ClientMsg::TermSubmit {
        terminal: t,
        text: "reply".into(),
    };
    assert_eq!(e.mob.request(&submit), Err(SUBMIT_NOT_READY.into()));
    assert_eq!(e.desk.request(&submit), Err(SUBMIT_NOT_READY.into()));
}

#[test]
fn mobile_attaches_to_desktop_opened_agent() {
    let mut e = env();
    let t = e.desk_open(e.claude());
    e.mob.attach(t);
    e.mob.output_until(t, "args");
}

#[test]
fn mobile_relaunches_agent() {
    let mut e = env();
    let t = Uuid::new_v4();
    e.mob.open(t, e.claude());
    e.fake.wait_pids(1);
    let r = e
        .mob
        .request(&ClientMsg::TermRelaunch {
            terminal: t,
            skip_permissions: true,
        })
        .unwrap();
    assert_eq!(r["relaunched"], json!(true));
    let l = e.fake.wait_launches(2);
    assert_eq!(l[1], [SKIP, "--resume", SID]);
}

#[test]
fn mobile_session_and_stats_calls_ok() {
    let mut e = env();
    let cwd = e.cwd.to_string_lossy().into_owned();
    let enc = encode_project_name(&cwd);
    let calls = [
        ("list_claude_projects", json!({})),
        ("get_sessions", json!({ "encodedName": enc })),
        ("get_all_recent_sessions", json!({ "limit": 5 })),
        (
            "get_session_messages",
            json!({ "encodedName": enc, "sessionId": SID, "limit": 5 }),
        ),
        ("list_project_session_ids", json!({ "cwd": cwd })),
        (
            "detect_session_branch",
            json!({ "cwd": cwd, "currentSessionId": SID, "knownSessionIds": [SID] }),
        ),
        ("list_codex_projects", json!({})),
        ("list_cursor_projects", json!({})),
        ("list_opencode_projects", json!({})),
        ("list_antigravity_projects", json!({})),
        ("probe_statusline_setup", json!({})),
        ("get_global_rate_limits", json!({})),
        ("get_claude_cost_summary", json!({})),
        ("get_codex_usage", json!({})),
        (
            "save_dropped_file",
            json!({ "bytesBase64": "aGk=", "name": "x.png" }),
        ),
    ];
    for (m, p) in calls {
        let r = e.mob.call(m, p.clone());
        assert!(r.is_ok(), "{m}: {r:?}");
        assert!(e.desk.call(m, p).is_ok(), "{m}");
    }
    let projects = e.mob.call("list_claude_projects", json!({})).unwrap();
    assert_eq!(projects[0]["path"], json!(cwd));
    let ids = e
        .mob
        .call("list_project_session_ids", json!({ "cwd": cwd }));
    assert_eq!(ids, Ok(json!([SID])));
}

#[test]
fn mobile_save_dropped_file_ok() {
    let mut e = env();
    let p = e
        .mob
        .call(
            "save_dropped_file",
            json!({ "bytesBase64": "aGk=", "name": "shot.png" }),
        )
        .unwrap();
    let p = PathBuf::from(p.as_str().unwrap());
    assert!(
        p.starts_with(e.h.paths().tmp.join("xshell-clipboard")),
        "{p:?}"
    );
    assert_eq!(fs::read(&p).unwrap(), b"hi");
    let evil = json!({ "bytesBase64": "aGk=", "name": "../../evil" });
    refused(e.mob.call("save_dropped_file", evil.clone()));
    // A Desktop's name is sanitised as before.
    let d = e.desk.call("save_dropped_file", evil).unwrap();
    assert!(Path::new(d.as_str().unwrap()).starts_with(e.h.paths().tmp.join("xshell-clipboard")));
}

#[test]
fn mobile_session_reads_stay_in_session_storage() {
    let mut e = env();
    let projects = e.h.home().join(".claude/projects");
    let outside = e.h.root().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let line = json!({ "type": "user", "message": { "role": "user", "content": "secret" } });
    fs::write(outside.join("x.jsonl"), format!("{line}\n")).unwrap();
    std::os::unix::fs::symlink(&outside, projects.join("escape")).unwrap();
    let enc = encode_project_name(&e.cwd.to_string_lossy());

    let abs = outside.to_string_lossy().into_owned();
    let cases = [
        (abs.as_str(), "x"),
        ("../../../outside", "x"),
        (enc.as_str(), "../../../../outside/x"),
        ("escape", "x"),
    ];
    for (name, sid) in cases {
        let p = json!({ "encodedName": name, "sessionId": sid, "limit": 5 });
        refused(e.mob.call("get_session_messages", p.clone()));
        // A Desktop reads it as it always did.
        let d = e.desk.call("get_session_messages", p).unwrap();
        assert_eq!(d[0]["text"], json!("secret"), "{name} {sid}");
    }
    for name in [abs.as_str(), "..", "escape"] {
        let p = json!({ "encodedName": name });
        refused(e.mob.call("get_sessions", p.clone()));
        assert!(e.desk.call("get_sessions", p).is_ok());
    }
    let outside_cwd = json!({ "cwd": abs });
    refused(e.mob.call("list_project_session_ids", outside_cwd.clone()));
    assert!(e.desk.call("list_project_session_ids", outside_cwd).is_ok());
}

#[test]
fn mobile_bulk_session_reads_refuse_symlinked_files() {
    let mut e = env();
    let outside = e.h.root().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let line =
        json!({ "type": "user", "cwd": e.cwd, "message": { "role": "user", "content": "secret" } });
    fs::write(outside.join("x.jsonl"), format!("{line}\n")).unwrap();
    let cwd = e.cwd.to_string_lossy().into_owned();
    let enc = encode_project_name(&cwd);
    let link =
        e.h.home()
            .join(".claude/projects")
            .join(&enc)
            .join("leak.jsonl");
    std::os::unix::fs::symlink(outside.join("x.jsonl"), &link).unwrap();

    let calls = [
        ("get_sessions", json!({ "encodedName": enc })),
        ("get_all_recent_sessions", json!({ "limit": 50 })),
        (
            "detect_session_branch",
            json!({ "cwd": cwd, "currentSessionId": SID, "knownSessionIds": [SID] }),
        ),
    ];
    for (m, p) in &calls {
        refused(e.mob.call(m, p.clone()));
    }
    // A Desktop reads the linked file as it always did.
    let d = e.desk.call("get_sessions", calls[0].1.clone()).unwrap();
    assert!(
        d.as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == json!("leak")),
        "{d}"
    );
    assert!(e
        .desk
        .call("get_all_recent_sessions", calls[1].1.clone())
        .is_ok());

    fs::remove_file(&link).unwrap();
    for (m, p) in calls {
        assert!(e.mob.call(m, p).is_ok(), "{m}");
    }
}

#[test]
fn mobile_escaping_parent_with_missing_leaf_refused() {
    let mut e = env();
    let outside = e.h.root().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, e.h.home().join(".claude/projects/escape")).unwrap();
    let p = json!({ "encodedName": "escape", "sessionId": "missing", "limit": 5 });
    refused(e.mob.call("get_session_messages", p.clone()));
    assert_eq!(e.desk.call("get_session_messages", p), Ok(json!([])));
}

#[test]
fn mobile_lists_project_sessions() {
    let mut e = env();
    codex_history(&e.h, &e.cwd);
    let cwd = e.cwd.to_string_lossy().into_owned();
    let got = e
        .mob
        .call("get_project_sessions", json!({ "cwd": cwd }))
        .unwrap();
    let page: PastSessionsPage = serde_json::from_value(got.clone()).unwrap();
    let mut listed: Vec<_> = page
        .sessions
        .iter()
        .map(|s| (s.agent.as_str(), s.id.as_str()))
        .collect();
    listed.sort();
    assert_eq!(listed, [("claude", SID), ("codex", "r1")]);
    assert_eq!(page.next, None);
    // Paged: one at a time, the cursor carried back as is.
    let first = e
        .mob
        .call("get_project_sessions", json!({ "cwd": cwd, "limit": 1 }))
        .unwrap();
    let rest = e
        .mob
        .call(
            "get_project_sessions",
            json!({ "cwd": cwd, "limit": 1, "before": first["next"] }),
        )
        .unwrap();
    assert_eq!(
        [first["sessions"][0].clone(), rest["sessions"][0].clone()].to_vec(),
        got["sessions"].as_array().unwrap().clone()
    );
    assert_eq!(rest["next"], Value::Null);
    // Outside a known Project: refused for a Mobile, an empty page for a Desktop.
    let outside = json!({ "cwd": e.h.root().join("outside") });
    refused(e.mob.call("get_project_sessions", outside.clone()));
    refused(e.mob.call("get_project_sessions", json!({})));
    let d = e.desk.call("get_project_sessions", outside).unwrap();
    assert_eq!(d, json!({ "sessions": [], "next": null }));
}

#[test]
fn mobile_project_sessions_check_does_not_hold_up_the_connection() {
    let mut e = env();
    // A FIFO among the Claude Projects: reading the history to find the Project blocks on it
    // until a writer comes.
    let held = e.h.home().join(".claude/projects/held");
    fs::create_dir_all(&held).unwrap();
    let fifo = held.join("f.jsonl");
    let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);

    let id = e.mob.request_id();
    let msg = ClientMsg::Call {
        method: "get_project_sessions".into(),
        params: json!({ "cwd": e.cwd }),
    };
    e.mob.send(&msg, Some(id));
    // An unrelated Terminal request on the same connection is answered meanwhile.
    let r = e.mob.request(&ClientMsg::TermResize {
        terminal: Uuid::new_v4(),
        cols: 80,
        rows: 24,
    });
    assert!(r.unwrap_err().starts_with("unknown terminal"));
    let answered = |m: &ServerMsg| matches!(m, ServerMsg::Res(r) if r.id == id);
    assert!(e
        .mob
        .try_msg(Duration::from_millis(300), answered)
        .is_none());
    // Release the read: the call then answers.
    drop(fs::OpenOptions::new().write(true).open(&fifo).unwrap());
    let page = e.mob.wait_res(id).unwrap();
    assert_eq!(page["sessions"][0]["id"], json!(SID));
}

// ── Visibility ────────────────────────────────────────────────────────────

/// An agent run inside a wrapping shell.
fn wrapped(e: &Env) -> LaunchSpec {
    LaunchSpec {
        shell_command: Some("/bin/sh".into()),
        shell_id: Some("bash".into()),
        ..e.claude()
    }
}

/// Every kind of Terminal a Mobile is never told about: a raw shell, a wrapped agent and an
/// agent under a launch prefix.
fn hidden_specs(e: &Env) -> Vec<LaunchSpec> {
    let prefixed = LaunchSpec {
        launch_prefix: Some(
            ["bash", "-c", "exec sh -i", "--"]
                .map(String::from)
                .to_vec(),
        ),
        ..e.claude()
    };
    vec![e.shell(), wrapped(e), prefixed]
}

/// Open `spec` from `c` with `title` in its meta.
fn open_titled(c: &mut Client, spec: LaunchSpec, title: &str) -> Uuid {
    let t = Uuid::new_v4();
    let mut meta = serde_json::Map::new();
    meta.insert("title".into(), json!(title));
    let msg = ClientMsg::TermOpen {
        spec: OpenSpec {
            terminal: t,
            launch: spec,
            cols: 80,
            rows: 24,
            meta,
            first_message: None,
        },
    };
    let r = c
        .request(&msg)
        .unwrap_or_else(|e| panic!("term.open failed: {e}"));
    assert!(r["pid"].is_u64(), "{r}");
    t
}

/// The Desktop opens every hidden kind (titled `HIDDEN-<n>`), then one direct agent.
fn open_hidden_and_agent(e: &mut Env) -> (Vec<Uuid>, Uuid) {
    let hidden = hidden_specs(e)
        .into_iter()
        .enumerate()
        .map(|(n, s)| e.desk_titled(s, &format!("HIDDEN-{n}")))
        .collect();
    let a = e.desk_titled(e.claude(), "agent");
    (hidden, a)
}

/// The Terminals a new connection of `role` is told about in its hello.
fn hello_list(srv: &ServerHandle, role: Role) -> Vec<Uuid> {
    let s = srv.connect_in_process(role).unwrap();
    let mut c = Client::from_io(s.try_clone().unwrap(), s);
    c.hello(range(1, 1)).1.iter().map(|t| t.terminal).collect()
}

/// Nothing `c` received names a Terminal in `hidden`: no list entry, no exit, no output, and
/// no hidden title anywhere.
#[track_caller]
fn never_saw(c: &Client, hidden: &[Uuid]) {
    for ev in &c.log {
        match ev {
            Ev::Msg(ServerMsg::Terminals { list }) => {
                for i in list {
                    assert!(!hidden.contains(&i.terminal), "listed {}", i.terminal);
                }
            }
            Ev::Msg(ServerMsg::TermExit { terminal, .. }) => {
                assert!(!hidden.contains(terminal), "told {terminal} exited");
            }
            Ev::Out(t, _) => assert!(!hidden.contains(t), "output of {t}"),
            _ => {}
        }
    }
    let leaked: Vec<_> = c
        .summary()
        .into_iter()
        .filter(|l| l.contains("HIDDEN-"))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
}

fn set_title(c: &mut Client, t: Uuid, title: &str) {
    let mut meta = serde_json::Map::new();
    meta.insert("title".into(), json!(title));
    let upd = ClientMsg::TermUpdate {
        terminal: t,
        session_id: None,
        meta: Some(meta),
    };
    assert_eq!(c.request(&upd), Ok(Value::Null));
}

fn titled(list: &[xshell_protocol::msg::TerminalInfo], t: Uuid, title: &str) -> bool {
    list.iter()
        .any(|i| i.terminal == t && i.meta.get("title") == Some(&json!(title)))
}

#[test]
fn mobile_list_holds_only_direct_agents() {
    let mut e = env();
    let (hidden, a) = open_hidden_and_agent(&mut e);
    assert_eq!(hello_list(&e.srv, Role::Mobile), vec![a]);
    let mut all = hello_list(&e.srv, Role::Desktop);
    all.sort();
    let mut want: Vec<_> = hidden.iter().copied().chain([a]).collect();
    want.sort();
    assert_eq!(all, want);
    // Connected before the opens: every broadcast it got was filtered.
    let l = e.mob.terminals_where(|l| l.iter().any(|t| t.terminal == a));
    assert_eq!(l.iter().map(|t| t.terminal).collect::<Vec<_>>(), vec![a]);
    never_saw(&e.mob, &hidden);
}

#[test]
fn mobile_list_follows_changes_without_hidden_terminals() {
    let mut e = env();
    let (hidden, a) = open_hidden_and_agent(&mut e);
    set_title(&mut e.desk, hidden[0], "HIDDEN-updated");
    set_title(&mut e.desk, a, "seen");
    e.desk
        .terminals_where(|l| titled(l, hidden[0], "HIDDEN-updated") && titled(l, a, "seen"));
    // Ordered after the hidden update under the registry lock.
    let l = e.mob.terminals_where(|l| titled(l, a, "seen"));
    assert_eq!(l.len(), 1);
    never_saw(&e.mob, &hidden);
}

#[test]
fn mobile_gets_no_exit_of_hidden_terminals() {
    let mut e = env();
    let a = e.desk_titled(e.claude(), "agent");
    let closed = e.desk_titled(e.shell(), "HIDDEN-closed");
    let exits = e.desk_titled(e.shell(), "HIDDEN-exits");
    let exit_of = |t: Uuid| move |m: &ServerMsg| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == t);
    // Closed by the Desktop.
    assert_eq!(
        e.desk.request(&ClientMsg::TermClose { terminal: closed }),
        Ok(Value::Null)
    );
    e.desk
        .expect_msg("exit of the closed shell", exit_of(closed));
    // Ends by itself and stays listed as exited.
    e.desk.attach(exits);
    e.desk.input(exits, "exit\n");
    e.desk.expect_msg("exit of the shell", exit_of(exits));
    e.desk.terminals_where(|l| {
        l.iter()
            .any(|i| i.terminal == exits && i.exit_code.is_some())
    });
    // A direct agent's exit still reaches the Mobile, after the shells' exits.
    assert_eq!(
        e.desk.request(&ClientMsg::TermClose { terminal: a }),
        Ok(Value::Null)
    );
    e.mob.expect_msg("exit of the agent", exit_of(a));
    never_saw(&e.mob, &[closed, exits]);
}

#[test]
fn mobile_list_hides_wrapped_agent_through_relaunch() {
    let mut e = env();
    let a = e.desk_titled(e.claude(), "agent");
    let w = e.desk_titled(wrapped(&e), "HIDDEN-wrapped");
    let old = e
        .desk
        .terminals_where(|l| l.iter().any(|i| i.terminal == w && i.pid.is_some()))
        .into_iter()
        .find(|i| i.terminal == w)
        .unwrap()
        .pid;
    let r = e
        .desk
        .request(&ClientMsg::TermRelaunch {
            terminal: w,
            skip_permissions: true,
        })
        .unwrap();
    assert_eq!(r["relaunched"], json!(true));
    e.desk.terminals_where(|l| {
        l.iter()
            .any(|i| i.terminal == w && i.pid.is_some() && i.pid != old)
    });
    // Ordered after the Relaunch's list on the Mobile's connection.
    set_title(&mut e.desk, a, "after relaunch");
    e.mob.terminals_where(|l| titled(l, a, "after relaunch"));
    never_saw(&e.mob, &[w]);
}

#[test]
fn mobile_hello_after_restart_lists_only_direct_agents() {
    let mut e = env();
    let s = e.desk_open(e.shell());
    let a = e.desk_open(e.claude());
    e.fake.wait_pids(1);
    e.srv.stopper().shutdown();
    assert!(e.srv.wait_timeout(T).is_some());
    e.srv = start(&e.h, |_| {});
    assert_eq!(hello_list(&e.srv, Role::Mobile), vec![a]);
    let mut all = hello_list(&e.srv, Role::Desktop);
    all.sort();
    let mut want = vec![s, a];
    want.sort();
    assert_eq!(all, want);
}

// ── The role cannot change ────────────────────────────────────────────────

#[test]
fn mobile_second_hello_keeps_role() {
    let mut e = env();
    let r = e.mob.request(&ClientMsg::Hello(Hello {
        protocol: range(1, 1),
        version: "t".into(),
        capabilities: vec![],
    }));
    assert_eq!(r, Err("already said hello".into()));
    let shell = e.shell();
    refused(try_open(&mut e.mob, shell));
}

#[test]
fn mobile_hello_role_field_ignored() {
    let e = env();
    let s = e.srv.connect_in_process(Role::Mobile).unwrap();
    let mut c = Client::from_io(s.try_clone().unwrap(), s);
    let hello = json!({
        "t": "hello",
        "protocol": { "min": 1, "max": 1 },
        "version": "t",
        "role": "desktop",
    });
    c.send_frame(&Frame::Json(hello.to_string().into_bytes()));
    c.terminals();
    refused(try_open(&mut c, e.shell()));
}

#[test]
fn roles_are_per_connection() {
    let mut e = env();
    let mut mob2 = Client::in_process(&e.srv, Role::Mobile);
    // The socket carries Desktops.
    let mut sock = Client::connect(&e.h.paths().socket);
    sock.hello(range(1, 1));
    let t = e.desk_open(e.shell());
    for m in [&mut e.mob, &mut mob2] {
        refused(m.request(&ClientMsg::TermAttach { terminal: t }));
    }
    sock.attach(t);
    sock.marker(t, "sockok");
    refused(try_open(&mut mob2, e.shell()));
    e.desk.open(Uuid::new_v4(), e.shell());
}

// ── Permission Prompts ────────────────────────────────────────────────────

fn answer_msg(t: Uuid) -> ClientMsg {
    ClientMsg::TermAnswer {
        terminal: t,
        prompt: 1,
        option: 0,
    }
}

#[test]
fn mobile_may_answer_agent_terminals() {
    let mut e = env();
    for spec in [
        e.claude(),
        LaunchSpec {
            agent: Some("codex".into()),
            session_id: None,
            ..e.claude()
        },
    ] {
        let t = e.desk_open(spec);
        // It reaches the Terminal: no prompt is listed, so the answer is stale, not refused.
        for c in [&mut e.mob, &mut e.desk] {
            assert_eq!(c.request(&answer_msg(t)), Err(PROMPT_ANSWERED.into()));
        }
    }
    let gone = Uuid::new_v4();
    assert_eq!(
        e.mob.request(&answer_msg(gone)),
        Err(format!("unknown terminal {gone}"))
    );
}

#[test]
fn mobile_answer_refused_for_hidden_terminal() {
    let mut e = env();
    for spec in hidden_specs(&e) {
        let t = e.desk_open(spec);
        refused(e.mob.request(&answer_msg(t)));
        // A Desktop's answer reaches it (and finds no prompt).
        assert_eq!(e.desk.request(&answer_msg(t)), Err(PROMPT_ANSWERED.into()));
    }
}

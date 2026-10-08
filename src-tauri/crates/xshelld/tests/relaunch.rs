#![cfg(unix)]
//! `term.relaunch`: a Terminal restarted in place with `skipPermissions` changed. The fake
//! agents (see `shared_fake_agents`) ignore SIGHUP, so every Relaunch here also goes through
//! the SIGKILL escalation. Races are ordered with test hooks, never with sleeps.

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_core::protocol::msg::{ClientMsg, ServerMsg, TerminalInfo};
use xshell_core::terminal::replay::RESET;
use xshelld::server::{Config, Server, ServerHandle, StartError, TestHook, TestPoint};

const SID: &str = "11111111-2222-3333-4444-555555555555";
const SKIP: &str = "--dangerously-skip-permissions";

struct Env {
    c: Client,
    srv: ServerHandle,
    cwd: PathBuf,
    fake: Fake,
    _reaper: FakeReaper,
    h: TestHome,
}

fn env(tweak: impl FnOnce(&mut Config)) -> Env {
    let h = TestHome::new();
    let cwd = h.project("app");
    let fake = Fake::in_dir(&cwd);
    let srv = start(&h, tweak);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    Env {
        c,
        srv,
        _reaper: FakeReaper(fake.pids_log.clone()),
        fake,
        cwd,
        h,
    }
}

impl Env {
    /// Open an agent Terminal on a session that already has a transcript, so it resumes.
    fn open(&mut self, spec: LaunchSpec) -> (Uuid, i32) {
        make_jsonl(&self.h, &self.cwd, SID);
        let t = Uuid::new_v4();
        let pid = ok_pid(&self.c.open(t, spec));
        assert_eq!(self.fake.wait_pids(1).last(), Some(&pid));
        (t, pid)
    }

    fn claude(&mut self) -> (Uuid, i32) {
        let spec = claude_spec(&self.cwd, Some(SID));
        self.open(spec)
    }

    fn client(&self) -> Client {
        let mut c = Client::connect(&self.h.paths().socket);
        c.hello(range(1, 1));
        c
    }

    /// The list a new connection gets.
    fn list(&self) -> Vec<TerminalInfo> {
        let mut c = Client::connect(&self.h.paths().socket);
        c.hello(range(1, 1)).1
    }

    fn state_spec(&self) -> Value {
        self.h.state_json()["terminals"][0]["spec"].clone()
    }
}

fn relaunch_msg(t: Uuid, skip: bool) -> ClientMsg {
    ClientMsg::TermRelaunch {
        terminal: t,
        skip_permissions: skip,
    }
}

fn relaunch(c: &mut Client, t: Uuid, skip: bool) -> Result<Value, String> {
    c.request(&relaunch_msg(t, skip))
}

fn exits(c: &Client, t: Uuid) -> usize {
    c.log
        .iter()
        .filter(|e| matches!(e, Ev::Msg(ServerMsg::TermExit { terminal, .. }) if *terminal == t))
        .count()
}

fn only(list: &[TerminalInfo], t: Uuid) -> &TerminalInfo {
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].terminal, t);
    &list[0]
}

/// Holds the thread that reaches a matching [`TestPoint`] until released.
struct Gate {
    reached: Receiver<Uuid>,
    release: Sender<()>,
}

impl Gate {
    fn hook(matches: fn(TestPoint) -> bool) -> (Gate, TestHook) {
        let (reached_tx, reached) = channel();
        let (release, release_rx) = channel::<()>();
        let (reached_tx, release_rx) = (Mutex::new(reached_tx), Mutex::new(release_rx));
        let hook = TestHook(Arc::new(move |t, p| {
            if matches(p) {
                let _ = reached_tx.lock().unwrap().send(t);
                // Bounded, so a failed test never strands the worker.
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30));
            }
            false
        }));
        (Gate { reached, release }, hook)
    }

    fn wait(&self) -> Uuid {
        self.reached.recv_timeout(T).expect("hook point reached")
    }

    fn open(&self) {
        let _ = self.release.send(());
    }
}

fn waited(p: TestPoint) -> bool {
    matches!(p, TestPoint::Waited { exited: true })
}

#[test]
fn relaunch_same_uuid_new_pid_with_flag() {
    let mut e = env(|_| {});
    let (t, old) = e.claude();
    let ok = relaunch(&mut e.c, t, true).unwrap();
    assert_eq!(ok["relaunched"], Value::Bool(true));
    let new = ok_pid(&ok);
    assert_ne!(new, old);
    let launches = e.fake.wait_launches(2);
    assert_eq!(launches[1], vec![SKIP, "--resume", SID]);
    assert!(wait_dead(old, T), "the old agent survived the relaunch");
    assert!(alive(new));
    let list =
        e.c.terminals_where(|l| l.first().and_then(|i| i.pid) == Some(new as u32));
    let info = only(&list, t);
    assert_eq!(info.spec.skip_permissions, Some(true));
    assert_eq!(info.exit_code, None);
    assert_eq!(e.state_spec()["skipPermissions"], Value::Bool(true));

    // And back: the flag is gone from the next launch.
    let ok = relaunch(&mut e.c, t, false).unwrap();
    assert_eq!(e.fake.wait_launches(3)[2], vec!["--resume", SID]);
    assert!(wait_dead(new, T));
    assert!(alive(ok_pid(&ok)));
    assert_eq!(e.state_spec()["skipPermissions"], Value::Bool(false));
}

#[test]
fn relaunch_codex_puts_flag_after_resume() {
    let mut e = env(|_| {});
    let spec = LaunchSpec {
        agent: Some("codex".into()),
        ..claude_spec(&e.cwd, Some("thread-1"))
    };
    let (t, _) = e.open(spec);
    relaunch(&mut e.c, t, true).unwrap();
    assert_eq!(
        e.fake.wait_launches(2)[1],
        vec![
            "resume",
            "--dangerously-bypass-approvals-and-sandbox",
            "thread-1"
        ]
    );
}

#[test]
fn relaunch_no_exit_attachment_continues() {
    let mut e = env(|_| {});
    let (t, old) = e.claude();
    e.c.attach(t);
    e.c.output_until(t, &format!("pid {old} "));
    let new = ok_pid(&relaunch(&mut e.c, t, true).unwrap());
    // Without attaching again: a reset, then the new process.
    let got = e.c.output_until(t, &format!("pid {new} "));
    let reset = find(&got, RESET).expect("a reset before the new output");
    assert!(find(&got[reset..], format!("pid {new} ").as_bytes()).is_some());
    assert_eq!(exits(&e.c, t), 0, "{:?}", e.c.summary());
}

#[test]
fn relaunch_second_connection_mirrors() {
    let mut e = env(|_| {});
    let (t, old) = e.claude();
    let mut b = e.client();
    b.attach(t);
    b.output_until(t, &format!("pid {old} "));
    let new = ok_pid(&relaunch(&mut e.c, t, true).unwrap());
    let list = b.terminals_where(|l| l.first().and_then(|i| i.pid) == Some(new as u32));
    assert_eq!(only(&list, t).spec.skip_permissions, Some(true));
    let got = b.output_until(t, &format!("pid {new} "));
    assert!(find(&got, RESET).is_some());
    assert_eq!(exits(&b, t), 0, "{:?}", b.summary());
}

#[test]
fn relaunch_keeps_size() {
    let mut e = env(|_| {});
    let (t, _) = e.claude();
    e.c.attach(t);
    e.c.resize(t, 100, 30);
    let new = ok_pid(&relaunch(&mut e.c, t, true).unwrap());
    // The fake prints its size at start.
    e.c.output_until(t, &format!("pid {new} size 30 100 "));
    // The size owner carried over too: it is still the only connection tracked.
    assert_eq!(e.srv.size_tracked(), 1);
}

#[test]
fn relaunch_fresh_replay() {
    let mut e = env(|_| {});
    let (t, old) = e.claude();
    e.c.attach(t);
    e.c.output_until(t, &format!("pid {old} "));
    let new = ok_pid(&relaunch(&mut e.c, t, true).unwrap());
    e.c.output_until(t, &format!("pid {new} "));
    let mut b = e.client();
    b.attach(t);
    b.output_until(t, &format!("pid {new} "));
    let replay = b.out[&t].clone();
    assert!(replay.starts_with(RESET));
    assert!(
        find(&replay, format!("pid {old} ").as_bytes()).is_none(),
        "replay still shows the old process: {:?}",
        String::from_utf8_lossy(&replay)
    );
}

#[test]
fn relaunch_same_value_noop() {
    let mut e = env(|_| {});
    let (t, pid) = e.claude();
    let ok = relaunch(&mut e.c, t, false).unwrap();
    assert_eq!(ok["relaunched"], Value::Bool(false));
    assert_eq!(ok_pid(&ok), pid);
    assert!(alive(pid));
    assert_eq!(e.fake.launches().len(), 1);
}

#[test]
fn relaunch_refusals_leave_process() {
    let mut e = env(|_| {});
    let (claude, pid) = e.claude();

    let raw = Uuid::new_v4();
    e.c.open(raw, sh_spec(&e.cwd));
    let cursor = Uuid::new_v4();
    let spec = LaunchSpec {
        agent: Some("cursor".into()),
        ..claude_spec(&e.cwd, Some(SID))
    };
    e.c.open(cursor, spec);
    let fresh = Uuid::new_v4();
    e.c.open(fresh, claude_spec(&e.cwd, None));
    let ended = Uuid::new_v4();
    let quits = e.h.script("quits", "exit 3");
    e.c.open(ended, raw_spec(&e.cwd, &quits));
    e.c.expect_msg(
        "term.exit",
        |m| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == ended),
    );
    let unknown = Uuid::new_v4();

    let cases = [
        (
            raw,
            "a raw shell has no permission prompts to skip".to_string(),
        ),
        (
            cursor,
            "cursor-agent has no flag to skip permission prompts".into(),
        ),
        (fresh, "no session to resume".into()),
        (ended, "terminal has exited".into()),
        (unknown, format!("unknown terminal {unknown}")),
    ];
    for (t, err) in cases {
        assert_eq!(relaunch(&mut e.c, t, true), Err(err));
    }
    assert!(alive(pid));
    // claude, cursor-agent and the fresh claude: one launch each, none relaunched.
    assert_eq!(e.fake.launches().len(), 3);
    let list = e.c.terminals_where(|l| l.len() == 5);
    assert!(list.iter().all(|i| i.spec.skip_permissions.is_none()));
    assert!(list.iter().any(|i| i.terminal == claude));
}

/// A second request while one runs is refused whatever its value, and the first completes.
#[test]
fn concurrent_relaunch_rejected() {
    let (gate, hook) = Gate::hook(|p| p == TestPoint::Signalled);
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, _) = e.claude();
    let id = e.c.request_id();
    e.c.send(&relaunch_msg(t, true), Some(id));
    gate.wait();
    for skip in [true, false] {
        assert_eq!(
            relaunch(&mut e.c, t, skip),
            Err("terminal is already relaunching".into())
        );
    }
    gate.open();
    assert_eq!(e.c.wait_res(id).unwrap()["relaunched"], Value::Bool(true));
    assert_eq!(e.fake.wait_launches(2).len(), 2);
}

#[test]
fn close_during_relaunch() {
    let (gate, hook) = Gate::hook(waited);
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, old) = e.claude();
    let id = e.c.request_id();
    e.c.send(&relaunch_msg(t, true), Some(id));
    gate.wait();
    // The old process has ended; closing now removes the Terminal for good.
    assert!(!alive(old));
    e.c.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    e.c.terminals_where(|l| l.is_empty());
    gate.open();
    assert_eq!(
        e.c.wait_res(id),
        Err("terminal was closed during the relaunch".into())
    );
    assert_eq!(e.fake.launches().len(), 1);
    assert!(e.h.state_ids().is_empty());
    let list = e.list();
    assert!(list.is_empty(), "{list:?}");
}

/// The update lands between the old process's end and the replacement's start: the
/// replacement resumes the session the record names now.
#[test]
fn relaunch_uses_record_updated_meanwhile() {
    let (gate, hook) = Gate::hook(waited);
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, _) = e.claude();
    let id = e.c.request_id();
    e.c.send(&relaunch_msg(t, true), Some(id));
    gate.wait();
    let other = "99999999-8888-7777-6666-555555555555";
    e.c.request(&ClientMsg::TermUpdate {
        terminal: t,
        session_id: Some(other.into()),
        meta: None,
    })
    .unwrap();
    gate.open();
    e.c.wait_res(id).unwrap();
    assert_eq!(
        e.fake.wait_launches(2)[1],
        vec![SKIP, "--session-id", other]
    );
}

/// A failure after the old process ended publishes its exit once and keeps the spec.
#[test]
fn relaunch_rollback_when_update_removes_session() {
    let (gate, hook) = Gate::hook(waited);
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, _) = e.claude();
    let id = e.c.request_id();
    e.c.send(&relaunch_msg(t, true), Some(id));
    gate.wait();
    e.c.request(&ClientMsg::TermUpdate {
        terminal: t,
        session_id: Some(String::new()),
        meta: None,
    })
    .unwrap();
    gate.open();
    assert_eq!(e.c.wait_res(id), Err("no session to resume".into()));
    assert_eq!(exits(&e.c, t), 1);
    let list =
        e.c.terminals_where(|l| l.first().is_some_and(|i| i.exit_code.is_some()));
    assert_eq!(only(&list, t).spec.skip_permissions, None);
    assert_eq!(e.fake.launches().len(), 1);
    assert_eq!(e.state_spec().get("skipPermissions"), Some(&Value::Null));
}

#[test]
fn relaunch_spawn_failure_keeps_terminal() {
    let mut e = env(|_| {});
    let (t, old) = e.claude();
    let _pids = PidReaper(vec![old]);
    e.c.attach(t);
    e.c.output_until(t, &format!("pid {old} "));
    fs::remove_dir_all(&e.cwd).unwrap();
    let id = e.c.request_id();
    e.c.send(&relaunch_msg(t, true), Some(id));
    let err = e.c.wait_res(id).unwrap_err();
    assert!(
        err.starts_with("restart failed: working directory does not exist"),
        "{err}"
    );
    assert!(!alive(old));
    // term.exit, then the list with the Terminal ended and its spec unchanged, then res.
    let at = |pred: &dyn Fn(&ServerMsg) -> bool| {
        e.c.log
            .iter()
            .position(|ev| matches!(ev, Ev::Msg(m) if pred(m)))
            .unwrap_or_else(|| panic!("{:?}", e.c.summary()))
    };
    let exit = at(&|m| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == t));
    let ended = at(&|m| {
        matches!(m, ServerMsg::Terminals { list }
            if list.len() == 1 && list[0].exit_code.is_some() && list[0].spec.skip_permissions.is_none())
    });
    let res = at(&|m| matches!(m, ServerMsg::Res(r) if r.id == id));
    assert!(exit < ended && ended < res, "{:?}", e.c.summary());
    assert_eq!(exits(&e.c, t), 1);

    // Still attachable like any ended Terminal: reply, the old replay, then term.exit.
    let mut b = e.client();
    let r = b.attach(t);
    assert!(r["exitCode"].is_i64(), "{r}");
    b.output_until(t, &format!("pid {old} "));
    b.expect_msg(
        "term.exit",
        |m| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == t),
    );
}

/// The replacement starts but one of its threads does not: it is ended, and its exit must
/// neither be published nor remove the original Terminal, which stays listed as ended.
#[test]
fn relaunch_partial_start_failure_keeps_entry() {
    let starts = Mutex::new(0);
    let (exited_tx, exited_rx) = channel::<Option<u32>>();
    let exited_tx = Mutex::new(exited_tx);
    let hook = TestHook(Arc::new(move |_, p| match p {
        TestPoint::StartInput => {
            let mut n = starts.lock().unwrap();
            *n += 1;
            *n == 2
        }
        TestPoint::ExitHandled { pid } => {
            let _ = exited_tx.lock().unwrap().send(pid);
            false
        }
        _ => false,
    }));
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, old) = e.claude();
    e.c.attach(t);
    e.c.output_until(t, &format!("pid {old} "));
    let err = relaunch(&mut e.c, t, true).unwrap_err();
    assert_eq!(
        err,
        "restart failed: failed to start terminal threads: refused by a test hook"
    );
    // Both exits handled: the old process's (held back), then the failed replacement's.
    let old_pid = Some(old as u32);
    let mut seen = vec![];
    while !(seen.contains(&old_pid) && seen.iter().any(|p| p.is_some() && *p != old_pid)) {
        seen.push(exited_rx.recv_timeout(T).expect("both exits handled"));
    }
    assert_eq!(exits(&e.c, t), 1, "{:?}", e.c.summary());
    let list = e.list();
    let info = only(&list, t);
    assert!(info.exit_code.is_some());
    assert_eq!(info.pid, Some(old as u32));
    assert_eq!(e.h.state_ids(), vec![t]);
    // The failed replacement was confirmed gone, so nothing is left to end on a restart.
    assert_eq!(e.h.state_json()["terminals"][0]["leader"], Value::Null);
    let mut b = e.client();
    b.attach(t);
    b.output_until(t, &format!("pid {old} "));
    b.expect_msg(
        "term.exit",
        |m| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == t),
    );
}

/// The process exits just as the wait gives up: the timeout path publishes that exit, once.
#[test]
fn relaunch_timeout_at_exit_publishes_once() {
    let hook = TestHook(Arc::new(|_, p| waited(p)));
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, old) = e.claude();
    assert_eq!(
        relaunch(&mut e.c, t, true),
        Err("previous process did not exit".into())
    );
    assert!(!alive(old));
    assert_eq!(exits(&e.c, t), 1, "{:?}", e.c.summary());
    let list =
        e.c.terminals_where(|l| l.first().is_some_and(|i| i.exit_code.is_some()));
    assert_eq!(only(&list, t).spec.skip_permissions, None);
    assert_eq!(e.fake.launches().len(), 1);
    assert_eq!(e.state_spec().get("skipPermissions"), Some(&Value::Null));
}

#[test]
fn relaunch_worker_start_failure_rolls_back() {
    let refused = Arc::new(Mutex::new(true));
    let r = refused.clone();
    let hook = TestHook(Arc::new(move |_, p| {
        p == TestPoint::StartWorker && *r.lock().unwrap()
    }));
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, pid) = e.claude();
    assert_eq!(
        relaunch(&mut e.c, t, true),
        Err("cannot start the relaunch: refused by a test hook".into())
    );
    assert!(alive(pid));
    assert_eq!(e.state_spec().get("skipPermissions"), Some(&Value::Null));
    // The reservation was released.
    *refused.lock().unwrap() = false;
    relaunch(&mut e.c, t, true).unwrap();
    assert_eq!(e.fake.wait_launches(2)[1], vec![SKIP, "--resume", SID]);
}

/// A detach or disconnect that lands while the Relaunch runs leaves nothing behind on the
/// replacement: no subscriber, no remembered size.
#[test]
fn detach_and_disconnect_during_relaunch_leave_nothing() {
    let (gate, hook) = Gate::hook(waited);
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, _) = e.claude();
    e.c.attach(t);
    let mut b = e.client();
    b.attach(t);
    b.resize(t, 90, 20);
    let mut gone = e.client();
    gone.attach(t);
    gone.resize(t, 70, 22);
    assert_eq!((e.srv.attached(), e.srv.size_tracked()), (3, 2));

    let id = e.c.request_id();
    e.c.send(&relaunch_msg(t, true), Some(id));
    gate.wait();
    b.request(&ClientMsg::TermDetach { terminal: t }).unwrap();
    gone.shutdown();
    drop(gone);
    let deadline = std::time::Instant::now() + T;
    while e.srv.attached() != 1 || e.srv.size_tracked() != 0 {
        assert!(std::time::Instant::now() < deadline, "cleanup did not run");
        std::thread::sleep(Duration::from_millis(10));
    }
    gate.open();
    let new = ok_pid(&e.c.wait_res(id).unwrap());
    e.c.output_until(t, &format!("pid {new} "));
    assert_eq!((e.srv.attached(), e.srv.size_tracked()), (1, 0));
}

/// The state file names the requested value as soon as the Relaunch is accepted, so a
/// shutdown (or crash) before it completes restores the Terminal with it.
#[test]
fn relaunch_interrupted_restores_with_flag() {
    let (gate, hook) = Gate::hook(|p| p == TestPoint::Signalled);
    let mut e = env(|c| c.test_hook = Some(hook));
    let (t, _) = e.claude();
    let id = e.c.request_id();
    e.c.send(&relaunch_msg(t, true), Some(id));
    gate.wait();
    assert_eq!(e.state_spec()["skipPermissions"], Value::Bool(true));
    let Env {
        c,
        srv,
        fake,
        h,
        _reaper,
        ..
    } = e;
    drop(c);
    srv.shutdown();
    gate.open();

    // A child forked by another test in this process holds the lock fd until it execs.
    let deadline = std::time::Instant::now() + T;
    let _srv = loop {
        match Server::start(config(&h)) {
            Err(StartError::AlreadyRunning) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            r => break r.expect("server starts"),
        }
    };
    let mut c = Client::connect(&h.paths().socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(only(&list, t).spec.skip_permissions, Some(true));
    assert_eq!(fake.wait_launches(2)[1], vec![SKIP, "--resume", SID]);
}

/// The Daemon crashes right after starting the replacement, before it is listed: the state
/// file already names it, so the restart ends it and exactly one agent runs.
#[test]
fn crash_after_replacement_spawn_ends_it_on_restore() {
    let h = TestHome::new();
    let f = fake_claude(&h);
    let _reaper = FakeReaper(f.pids_log.clone());
    let cwd = h.project("app");
    let t = Uuid::new_v4();
    let path = f.path_env();
    let crash_file = f.pids_log.to_string_lossy().into_owned();

    let mut s1 = ServeProc::start(
        &h,
        &[
            ("PATH", &path),
            ("XSHELLD_TEST_CRASH_AFTER_REPLACEMENT", &crash_file),
        ],
    );
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    c.open(t, claude_spec(&cwd, Some(SID)));
    let old = f.wait_pids(1)[0];
    make_jsonl(&h, &cwd, SID);
    let id = c.request_id();
    c.send(&relaunch_msg(t, true), Some(id));
    assert!(s1.wait_exit(T).is_some(), "serve did not crash");
    let replacement = f.wait_pids(2)[1];
    assert!(!alive(old));
    // The master closed with the crash; the HUP-ignoring replacement survived it.
    assert!(
        alive(replacement),
        "the replacement should survive the crash"
    );

    let _s2 = ServeProc::start(&h, &[("PATH", &path)]);
    let mut c = Client::connect(&h.paths().socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(only(&list, t).spec.skip_permissions, Some(true));
    assert!(
        wait_dead(replacement, T),
        "the leftover replacement was not ended"
    );
    let pids = f.wait_pids(3);
    assert_eq!(pids.iter().filter(|&&p| alive(p)).count(), 1, "{pids:?}");
    assert!(alive(pids[2]));
    assert_eq!(f.wait_launches(3)[2], vec![SKIP, "--resume", SID]);
}

/// Through the real binary: relaunch, crash, restart. Both the relaunched agent and the one
/// the restart restores run with the flag and resume the session.
#[test]
fn relaunch_persisted_then_restored() {
    let h = TestHome::new();
    let f = fake_claude(&h);
    let _reaper = FakeReaper(f.pids_log.clone());
    let cwd = h.project("app");
    let t = Uuid::new_v4();

    let mut s1 = ServeProc::start(&h, &[("PATH", &f.path_env())]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    c.open(t, claude_spec(&cwd, Some(SID)));
    f.wait_pids(1);
    make_jsonl(&h, &cwd, SID);
    relaunch(&mut c, t, true).unwrap();
    assert_eq!(f.wait_launches(2)[1], vec![SKIP, "--resume", SID]);
    drop(c);
    unsafe { libc::kill(s1.pid(), libc::SIGKILL) };
    assert!(s1.wait_exit(T).is_some());

    let _s2 = ServeProc::start(&h, &[("PATH", &f.path_env())]);
    let mut c = Client::connect(&h.paths().socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(only(&list, t).spec.skip_permissions, Some(true));
    assert_eq!(f.wait_launches(3)[2], vec![SKIP, "--resume", SID]);
}

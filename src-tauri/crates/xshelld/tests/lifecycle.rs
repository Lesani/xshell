#![cfg(unix)]
//! Daemon lifecycle in-process: idle exit, the single-instance lock, upgrade and restore.

mod common;

use common::*;
use serde_json::json;
use std::fs;
use std::time::Duration;
use uuid::Uuid;
use xshell_core::terminal::state::{save_atomic, PersistedTerminal};
use xshell_protocol::msg::ClientMsg;
use xshelld::server::{ExitReason, Server, StartError};

#[test]
fn idle_exit_when_empty() {
    let h = TestHome::new();
    let srv = start(&h, |c| c.idle_timeout = Duration::from_millis(300));
    let sock = srv.socket.clone();
    assert_eq!(
        srv.wait_timeout(Duration::from_secs(3)),
        Some(ExitReason::Idle)
    );
    assert!(!sock.exists());
    assert!(!h.paths().pid.exists());
}

#[test]
fn no_idle_exit_with_connection() {
    let h = TestHome::new();
    let srv = start(&h, |c| c.idle_timeout = Duration::from_millis(300));
    let mut a = Client::connect(&srv.socket);
    a.hello(range(1, 1));
    assert_eq!(srv.wait_timeout(Duration::from_millis(1500)), None);
    a.shutdown();
    drop(a);
    assert_eq!(
        srv.wait_timeout(Duration::from_secs(2)),
        Some(ExitReason::Idle)
    );
}

#[test]
fn no_idle_exit_with_terminal() {
    let h = TestHome::new();
    let srv = start(&h, |c| c.idle_timeout = Duration::from_millis(300));
    let mut a = Client::connect(&srv.socket);
    a.hello(range(1, 1));
    a.open(Uuid::new_v4(), sh_spec(&h.project("p")));
    a.shutdown();
    drop(a);
    assert_eq!(srv.wait_timeout(Duration::from_millis(1500)), None);
}

#[test]
fn single_instance_lock() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    assert!(matches!(
        Server::start(config(&h)),
        Err(StartError::AlreadyRunning)
    ));
    let mut a = Client::connect(&srv.socket);
    a.hello(range(1, 1));
    assert_eq!(get_home(&mut a), json!(h.home().to_string_lossy()));
}

#[test]
fn upgrade_persists_kills_and_exits() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = Client::connect(&srv.socket);
    a.hello(range(1, 1));
    let t = Uuid::new_v4();
    let pid = ok_pid(&a.open(t, sh_spec(&h.project("p"))));
    assert_eq!(a.request(&ClientMsg::DaemonUpgrade), Ok(json!(null)));
    a.expect_eof();
    assert_eq!(srv.wait(), ExitReason::Upgrade);
    assert!(wait_dead(pid, T));
    assert_eq!(h.state_ids(), vec![t]);
}

#[test]
fn restart_restores_raw_shell_same_uuid() {
    let h = TestHome::new();
    let cwd = h.project("restore me");
    let t = Uuid::new_v4();
    let pid1 = {
        let srv = start(&h, |_| {});
        let mut a = Client::connect(&srv.socket);
        a.hello(range(1, 1));
        let pid = ok_pid(&a.open(t, sh_spec(&cwd)));
        a.resize(t, 120, 40);
        a.request(&ClientMsg::DaemonUpgrade).unwrap();
        assert_eq!(srv.wait(), ExitReason::Upgrade);
        pid
    };
    // The upgrade's final persist recorded the size.
    assert_eq!(h.state_json()["terminals"][0]["cols"], json!(120));
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].terminal, t);
    assert_ne!(list[0].pid, Some(pid1 as u32));
    c.attach(t);
    c.marker(t, "back");
    c.expect_size(t, 40, 120);
    c.input(t, "pwd\n");
    c.output_until(t, &format!("{}\r\n", cwd.to_string_lossy()));
}

#[test]
fn failed_relaunch_dropped() {
    let h = TestHome::new();
    let good = Uuid::new_v4();
    let bad = Uuid::new_v4();
    let entry = |id: Uuid, cwd: &str| PersistedTerminal {
        terminal: id,
        spec: sh_spec(std::path::Path::new(cwd)),
        meta: Default::default(),
        cols: 80,
        rows: 24,
        created_at_ms: 1,
        leader: None,
    };
    let cwd = h.project("p");
    save_atomic(
        &h.paths().state,
        &[
            entry(good, &cwd.to_string_lossy()),
            entry(bad, "/nonexistent/x"),
        ],
    )
    .unwrap();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(
        list.iter().map(|i| i.terminal).collect::<Vec<_>>(),
        vec![good]
    );
    assert_eq!(h.state_ids(), vec![good]);
}

#[test]
fn corrupt_state_starts_empty() {
    let h = TestHome::new();
    fs::create_dir_all(h.paths().state.parent().unwrap()).unwrap();
    fs::write(h.paths().state, "{nope").unwrap();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    assert!(c.hello(range(1, 1)).1.is_empty());
    let aside = fs::read_dir(h.paths().state.parent().unwrap())
        .unwrap()
        .flatten()
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("terminals.json.corrupt-")
        });
    assert!(aside);
}

/// Shutdown (the SIGTERM path) ends Terminals but keeps them in the state file and does not
/// tell Desktops they are gone.
#[test]
fn shutdown_keeps_state() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = Client::connect(&srv.socket);
    a.hello(range(1, 1));
    let t = Uuid::new_v4();
    let pid = ok_pid(&a.open(t, sh_spec(&h.project("p"))));
    a.terminals_where(|l| l.len() == 1);
    assert_eq!(srv.shutdown(), ExitReason::Shutdown);
    a.expect_eof();
    assert!(wait_dead(pid, T));
    assert_eq!(h.state_ids(), vec![t]);
    assert!(a
        .try_msg(Duration::ZERO, |m| matches!(
            m,
            xshell_protocol::msg::ServerMsg::TermExit { .. }
        ))
        .is_none());
}

fn persisted(id: Uuid, cwd: &std::path::Path, title: &str) -> PersistedTerminal {
    let mut meta = serde_json::Map::new();
    meta.insert("title".into(), json!(title));
    PersistedTerminal {
        terminal: id,
        spec: sh_spec(cwd),
        meta,
        cols: 80,
        rows: 24,
        created_at_ms: 1,
        leader: None,
    }
}

/// Records saved under earlier (or larger) limits are restored, not dropped for their size.
#[test]
fn restore_ignores_budgets() {
    let h = TestHome::new();
    let cwd = h.project("p");
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    save_atomic(
        &h.paths().state,
        &[
            persisted(a, &cwd, &"x".repeat(5000)),
            persisted(b, &cwd, &"y".repeat(5000)),
        ],
    )
    .unwrap();
    let srv = start(&h, |c| {
        c.max_terminal_bytes = 1000;
        c.max_list_bytes = 2000;
    });
    let mut c = Client::connect(&srv.socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(list.len(), 2);
    assert!(list
        .iter()
        .all(|i| i.exit_code.is_none() && i.pid.is_some()));
    let mut ids = h.state_ids();
    ids.sort();
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(ids, want);
}

fn unkillable(_: &xshell_core::terminal::state::Leader, _: Duration) -> xshelld::server::Cleanup {
    xshelld::server::Cleanup::Unresolved
}

/// When leftovers of the previous run cannot be ended, the Terminal is not relaunched: it is
/// listed as exited (-1) with its record and leader kept, until `term.close` removes it.
#[test]
fn unresolved_cleanup_does_not_relaunch() {
    let h = TestHome::new();
    let cwd = h.project("p");
    let t = Uuid::new_v4();
    let mut p = persisted(t, &cwd, "kept");
    let leader = xshell_core::terminal::state::Leader {
        pid: 999_999,
        start_time: Some(42),
        groups: vec![],
    };
    p.leader = Some(leader.clone());
    save_atomic(&h.paths().state, &[p]).unwrap();
    let srv = start(&h, |c| c.cleanup_override = Some(unkillable));
    let mut c = Client::connect(&srv.socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].terminal, t);
    assert_eq!(list[0].exit_code, Some(-1));
    assert_eq!(list[0].pid, None);
    assert_eq!(list[0].meta.get("title"), Some(&json!("kept")));
    let state = h.state_json();
    assert_eq!(state["terminals"][0]["leader"]["pid"], json!(999_999));
    // Attaching shows it ended; input is refused.
    let id = c.request_id();
    c.send(&ClientMsg::TermAttach { terminal: t }, Some(id));
    assert_eq!(
        c.wait_res(id).unwrap(),
        json!({"exitCode": -1, "cols": 80, "rows": 24})
    );
    let err = c
        .request(&ClientMsg::TermInput {
            terminal: t,
            data: "x".into(),
        })
        .unwrap_err();
    assert_eq!(err, "terminal has exited");
    c.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    c.terminals_where(|l| l.is_empty());
    assert!(h.state_ids().is_empty());
}

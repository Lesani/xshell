#![cfg(unix)]
//! Restart and restore through the real binary: resume flags, PATH, crash leftovers and the
//! SIGTERM path. A fake `claude` records its argv and pid.

mod common;

use common::*;
use std::time::Duration;
use uuid::Uuid;
use xshell_core::protocol::msg::ClientMsg;

fn serve(h: &TestHome, f: &Fake) -> ServeProc {
    ServeProc::start(h, &[("PATH", &f.path_env())])
}

fn client(h: &TestHome) -> (Client, Vec<xshell_core::protocol::msg::TerminalInfo>) {
    let mut c = Client::connect(&h.paths().socket);
    let (_, list) = c.hello(range(1, 1));
    (c, list)
}

/// SIGKILL `serve` (a crash: no cleanup at all) and reap it.
fn crash(mut s: ServeProc) {
    unsafe { libc::kill(s.pid(), libc::SIGKILL) };
    assert!(s.wait_exit(T).is_some());
}

#[test]
fn restart_restores_with_resume_flags() {
    let h = TestHome::new();
    let f = fake_claude(&h);
    let _reaper = FakeReaper(f.pids_log.clone());
    let cwd = h.project("app");
    let sid = "11111111-2222-3333-4444-555555555555";
    let t = Uuid::new_v4();

    let s1 = serve(&h, &f);
    let (mut c, _) = client(&h);
    c.open(t, claude_spec(&cwd, Some(sid)));
    assert_eq!(f.wait_launches(1)[0], vec!["--session-id", sid]);
    f.wait_pids(1);
    make_jsonl(&h, &cwd, sid);
    drop(c);
    crash(s1);

    let _s2 = serve(&h, &f);
    let (_c, list) = client(&h);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].terminal, t);
    let launches = f.wait_launches(2);
    assert_eq!(launches[1], vec!["--resume", sid]);
}

#[test]
fn term_update_session_id_used_on_restore() {
    let h = TestHome::new();
    let f = fake_claude(&h);
    let _reaper = FakeReaper(f.pids_log.clone());
    let cwd = h.project("app");
    let sid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let t = Uuid::new_v4();

    let s1 = serve(&h, &f);
    let (mut c, _) = client(&h);
    c.open(t, claude_spec(&cwd, None));
    assert_eq!(f.wait_launches(1)[0], Vec::<String>::new());
    f.wait_pids(1);
    c.request(&ClientMsg::TermUpdate {
        terminal: t,
        session_id: Some(sid.into()),
        meta: None,
    })
    .unwrap();
    make_jsonl(&h, &cwd, sid);
    drop(c);
    crash(s1);

    let _s2 = serve(&h, &f);
    client(&h);
    assert_eq!(f.wait_launches(2)[1], vec!["--resume", sid]);
}

/// After a crash the HUP-ignoring agent survives; the restarted Daemon ends it before
/// relaunching, so exactly one instance runs.
#[test]
fn crash_leftovers_are_ended_before_relaunch() {
    let h = TestHome::new();
    let f = fake_claude(&h);
    let _reaper = FakeReaper(f.pids_log.clone());
    let cwd = h.project("app");
    let t = Uuid::new_v4();

    let s1 = serve(&h, &f);
    let (mut c, _) = client(&h);
    c.open(t, claude_spec(&cwd, Some("s")));
    let first = f.wait_pids(1)[0];
    drop(c);
    crash(s1);
    // The master closed, the agent ignored the hangup: it is still running.
    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(first), "the fake agent should survive the crash");

    let _s2 = serve(&h, &f);
    client(&h);
    assert!(wait_dead(first, T), "leftover agent was not ended");
    let pids = f.wait_pids(2);
    assert_eq!(pids.len(), 2);
    assert_eq!(pids.iter().filter(|&&p| alive(p)).count(), 1);
    assert!(alive(pids[1]));
}

/// SIGTERM ends the Terminals (even HUP-ignoring ones) but keeps the state for a restore.
#[test]
fn sigterm_ends_terminals_and_keeps_state() {
    let h = TestHome::new();
    let f = fake_claude(&h);
    let _reaper = FakeReaper(f.pids_log.clone());
    let cwd = h.project("app");
    let t = Uuid::new_v4();

    let mut s1 = serve(&h, &f);
    let (mut c, _) = client(&h);
    c.open(t, claude_spec(&cwd, Some("s")));
    let agent = f.wait_pids(1)[0];
    unsafe { libc::kill(s1.pid(), libc::SIGTERM) };
    let st = s1
        .wait_exit(Duration::from_secs(10))
        .expect("serve exits on SIGTERM");
    assert!(st.success(), "{st}");
    c.expect_eof();
    assert!(wait_dead(agent, T));
    assert_eq!(h.state_ids(), vec![t]);
    assert!(!h.paths().socket.exists());
}

/// A leftover that forks a HUP-immune child while handling each hangup: cleanup must rescan
/// the session and end the child born during its own grace period too.
#[cfg(target_os = "linux")]
#[test]
fn crash_leftover_forking_on_hup_is_ended() {
    let h = TestHome::new();
    let kids = h.root().join("kids.log");
    let trap = format!(
        "trap 'sh -c \"trap \\\"\\\" HUP; echo \\$\\$ >> {}; exec sleep 1000\" &' HUP",
        kids.display()
    );
    let f = fake_claude_with(&h, &trap, "while :; do sleep 0.1; done");
    let _reaper = FakeReaper(f.pids_log.clone());
    let _kid_reaper = FakeReaper(kids.clone());
    let cwd = h.project("app");
    let t = Uuid::new_v4();

    let s1 = serve(&h, &f);
    let (mut c, _) = client(&h);
    c.open(t, claude_spec(&cwd, Some("s")));
    let first = f.wait_pids(1)[0];
    drop(c);
    crash(s1);
    let read_kids = || -> Vec<i32> {
        std::fs::read_to_string(&kids)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    };
    // Whatever the crash's hangup forked has had time to log itself.
    std::thread::sleep(Duration::from_millis(500));
    let before = read_kids().len();

    let _s2 = serve(&h, &f);
    client(&h);
    assert!(wait_dead(first, T), "leftover agent was not ended");
    let pids = f.wait_pids(2);
    // Cleanup's own SIGHUP made the leftover fork a child after the first session scan.
    let spawned = read_kids();
    eprintln!(
        "children: {before} after the crash, {} in total",
        spawned.len()
    );
    assert!(
        spawned.len() > before,
        "no child was forked during cleanup: {spawned:?}"
    );
    for k in spawned {
        assert!(!alive(k), "child {k} forked during cleanup survived");
    }
    assert!(alive(pids[1]), "the relaunched agent is not running");
}

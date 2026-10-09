#![cfg(unix)]
//! `serve --gui-bound` as a real process (ADR-0005): it ends with its parent, the xshell app,
//! however the parent ends, and ends its Terminals with it while keeping their state.

mod common;

use common::*;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_protocol::msg::ClientMsg;

/// An agent that ends on SIGHUP.
fn soft_agent(h: &TestHome) -> Fake {
    fake_claude_with(h, ":", "exec sleep 1000")
}

fn wait_gone(p: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while p.exists() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Start a GUI-bound Daemon with `fake` on its PATH and open one agent Terminal in it.
fn with_terminal(h: &TestHome, fake: &Fake, env: &[(&str, &str)]) -> (GuiParent, Uuid, i32) {
    let path = fake.path_env();
    let mut all = vec![("PATH", path.as_str())];
    all.extend_from_slice(env);
    let parent = GuiParent::start(h, &all);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    let id = Uuid::new_v4();
    c.open(id, claude_spec(&h.project("p"), None));
    let pid = *fake.wait_pids(1).last().unwrap();
    (parent, id, pid)
}

#[test]
fn refuses_foreign_parent_pid() {
    let h = TestHome::new();
    // A process that is not our parent.
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let st = bin_cmd(&h)
        .args(["serve", "--gui-bound", "--parent-pid"])
        .arg(other.id().to_string())
        .stdin(Stdio::null())
        .status()
        .unwrap();
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(st.code(), Some(1));
    assert!(!h.paths().socket.exists());
    assert!(!h.paths().pid.exists());
    assert!(log_lines(&h, "is not running") >= 1);
}

#[test]
fn ends_when_parent_killed() {
    let h = TestHome::new();
    let fake = soft_agent(&h);
    let _reap = FakeReaper(fake.pids_log.clone());
    let (mut parent, id, agent) = with_terminal(&h, &fake, &[]);
    let daemon = parent.daemon_pid();
    parent.kill();
    assert!(
        wait_dead(daemon, Duration::from_secs(3)),
        "the Daemon outlived its parent"
    );
    assert!(!alive(agent), "the agent outlived the Daemon");
    assert!(wait_gone(&h.paths().socket, Duration::from_secs(1)));
    assert!(!h.paths().pid.exists());
    // Ended, not closed: the next start restores it.
    assert_eq!(h.state_ids(), vec![id]);
}

#[test]
fn sigterm_shutdown_is_bounded() {
    let h = TestHome::new();
    // Ignores SIGHUP: only the SIGKILL after the grace period ends it.
    let fake = fake_claude(&h);
    let _reap = FakeReaper(fake.pids_log.clone());
    let (parent, id, agent) = with_terminal(&h, &fake, &[]);
    let daemon = parent.daemon_pid();
    let t0 = Instant::now();
    unsafe { libc::kill(daemon, libc::SIGTERM) };
    assert!(
        wait_dead(daemon, Duration::from_secs(8)),
        "no exit within 8 s"
    );
    assert!(t0.elapsed() < Duration::from_secs(8));
    assert!(!alive(agent));
    assert_eq!(h.state_ids(), vec![id]);
    drop(parent);
}

/// The agent ends on the hangup, but a helper in its process group ignores it and holds no
/// PTY descriptor: the Terminal exits at once, and the Daemon still SIGKILLs the helper
/// before it exits.
#[test]
fn shutdown_kills_hangup_ignoring_group_members() {
    let h = TestHome::new();
    let helper_pid = h.root().join("helper.pid");
    let fake = fake_claude_with(
        &h,
        ":",
        &format!(
            "sh -c 'echo $$ > {}; trap \"\" HUP; exec sleep 1000 </dev/null >/dev/null 2>&1' &\n\
             exec sleep 1000",
            helper_pid.display()
        ),
    );
    let _reap = FakeReaper(fake.pids_log.clone());
    let (mut parent, _id, agent) = with_terminal(&h, &fake, &[]);
    let deadline = Instant::now() + T;
    let helper: i32 = loop {
        if let Some(p) = fs::read_to_string(&helper_pid)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break p;
        }
        assert!(Instant::now() < deadline, "no helper");
        std::thread::sleep(Duration::from_millis(20));
    };
    let _reap_helper = PidReaper(vec![helper]);
    // Give the helper time to reach `sleep` with its trap in place.
    std::thread::sleep(Duration::from_millis(300));
    let daemon = parent.daemon_pid();
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(5)));
    assert!(!alive(agent));
    assert!(
        wait_dead(helper, Duration::from_millis(500)),
        "the helper survived"
    );
}

#[test]
fn no_idle_exit() {
    let h = TestHome::new();
    let parent = GuiParent::start(&h, &[("XSHELLD_IDLE_TIMEOUT_MS", "50")]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    c.shutdown();
    drop(c);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        alive(parent.daemon_pid()),
        "a GUI-bound Daemon exited for idleness"
    );
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
}

#[test]
fn writes_mode_marker() {
    let h = TestHome::new();
    let _parent = GuiParent::start(&h, &[]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    let mode = h.paths().mode;
    assert_eq!(fs::read_to_string(&mode).unwrap().trim(), "gui-bound");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&mode).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn restores_terminals_on_next_gui_bound_start() {
    let h = TestHome::new();
    let fake = soft_agent(&h);
    let _reap = FakeReaper(fake.pids_log.clone());
    let (mut parent, id, _) = with_terminal(&h, &fake, &[]);
    let daemon = parent.daemon_pid();
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(3)));
    let path = fake.path_env();
    let _again = GuiParent::start(&h, &[("PATH", path.as_str())]);
    let mut c = Client::connect(&h.paths().socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(
        list.iter().map(|t| t.terminal).collect::<Vec<_>>(),
        vec![id]
    );
    assert_eq!(fake.wait_pids(2).len(), 2, "the agent was not relaunched");
}

#[test]
fn loses_lock_to_running_daemon_exit_3() {
    let h = TestHome::new();
    let _parent = GuiParent::start(&h, &[]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    // A second GUI-bound start, its parent this test.
    let st = bin_cmd(&h)
        .args(["serve", "--gui-bound", "--parent-pid"])
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(3));
    // The first one still serves.
    let mut c2 = Client::connect(&h.paths().socket);
    c2.hello(range(1, 1));
}

#[test]
fn refuses_daemon_upgrade() {
    let h = TestHome::new();
    let parent = GuiParent::start(&h, &[]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    let e = c.request(&ClientMsg::DaemonUpgrade).unwrap_err();
    assert!(e.contains("update xshell"), "{e}");
    assert!(!e.contains("Daemon"), "{e}");
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(parent.daemon_pid()));
}

/// The parent watch is armed before the login shell runs: a parent death while it stalls
/// ends the start at once, the shell with it.
#[test]
fn parent_death_during_login_shell_aborts_start() {
    let h = TestHome::new();
    let shell_pid = h.root().join("shell.pid");
    let shell = h.script(
        "slow-shell",
        &format!("echo $$ > {}\nexec sleep 30", shell_pid.display()),
    );
    let mut parent = GuiParent::start(
        &h,
        &[
            ("XSHELLD_LOGIN_ENV", "1"),
            ("SHELL", shell.to_str().unwrap()),
        ],
    );
    let daemon = parent.daemon_pid();
    let deadline = Instant::now() + T;
    while !shell_pid.exists() {
        assert!(Instant::now() < deadline, "the login shell never ran");
        std::thread::sleep(Duration::from_millis(20));
    }
    let shell: i32 = fs::read_to_string(&shell_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _reap = PidReaper(vec![shell]);
    let t0 = Instant::now();
    parent.kill();
    assert!(
        wait_dead(daemon, Duration::from_secs(2)),
        "the start went on"
    );
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(
        wait_dead(shell, Duration::from_secs(1)),
        "the login shell survived"
    );
    assert!(!h.paths().socket.exists());
    assert_eq!(log_lines(&h, "serving"), 0);
}

/// A parent death during restore abandons the start: the Terminals restored so far end, and
/// the state file keeps every Terminal.
#[test]
fn parent_death_during_restore_aborts_start() {
    let h = TestHome::new();
    let fake = soft_agent(&h);
    let _reap = FakeReaper(fake.pids_log.clone());
    let path = fake.path_env();
    // Two Terminals in the state file.
    let mut parent = GuiParent::start(&h, &[("PATH", path.as_str())]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    let ids = [Uuid::new_v4(), Uuid::new_v4()];
    for id in ids {
        c.open(id, claude_spec(&h.project("p"), None));
    }
    fake.wait_pids(2);
    let daemon = parent.daemon_pid();
    drop(c);
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(3)));
    let mut saved = h.state_ids();
    saved.sort();
    assert_eq!(saved.len(), 2);

    // Restore stalls 1.5 s before each Terminal.
    let mut parent = GuiParent::start(
        &h,
        &[
            ("PATH", path.as_str()),
            ("XSHELLD_TEST_RESTORE_DELAY_MS", "1500"),
        ],
    );
    let daemon = parent.daemon_pid();
    let restored = *fake.wait_pids(3).last().unwrap();
    parent.kill();
    assert!(
        wait_dead(daemon, Duration::from_secs(4)),
        "the start went on"
    );
    assert!(!alive(restored), "a restored agent outlived the Daemon");
    assert_eq!(fake.pids().len(), 3, "the second Terminal was relaunched");
    let mut now = h.state_ids();
    now.sort();
    assert_eq!(now, saved);
    assert!(!h.paths().socket.exists());
    assert_eq!(log_lines(&h, "serving"), 1);
}

/// PATH entries set only in interactive rc files reach agents of a GUI-bound Daemon.
#[test]
fn interactive_login_path_reaches_agents() {
    let h = TestHome::new();
    let fake = soft_agent(&h);
    let _reap = FakeReaper(fake.pids_log.clone());
    // `fake.bin` is on PATH only for an interactive shell.
    let shell = h.script(
        "rc-shell",
        &format!(
            "case \" $* \" in *\" -i \"*) PATH=\"{}:$PATH\";; esac\n\
             for last; do :; done\nexec /bin/sh -c \"$last\"",
            fake.bin.display()
        ),
    );
    let _parent = GuiParent::start(
        &h,
        &[
            ("XSHELLD_LOGIN_ENV", "1"),
            ("SHELL", shell.to_str().unwrap()),
        ],
    );
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    c.open(Uuid::new_v4(), claude_spec(&h.project("p"), None));
    assert_eq!(fake.wait_pids(1).len(), 1, "the agent was not found");
}

/// An agent that ends on SIGHUP with a helper in its group that ignores it and holds no PTY
/// descriptor; the helper's pid is written to `helper_pid`.
fn agent_with_helper(h: &TestHome, helper_pid: &Path) -> Fake {
    fake_claude_with(
        h,
        ":",
        &format!(
            "sh -c 'echo $$ > {}; trap \"\" HUP; exec sleep 1000 </dev/null >/dev/null 2>&1' &\n\
             exec sleep 1000",
            helper_pid.display()
        ),
    )
}

fn read_pid(p: &Path) -> i32 {
    let deadline = Instant::now() + T;
    loop {
        if let Some(n) = fs::read_to_string(p)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return n;
        }
        assert!(Instant::now() < deadline, "no pid in {}", p.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A Tab closed just before quitting: its Terminal has left the list, but its group's
/// SIGKILL is still due. The Daemon sends it before it exits.
#[test]
fn quit_kills_helpers_of_a_terminal_closed_just_before() {
    let h = TestHome::new();
    let helper_pid = h.root().join("helper.pid");
    let fake = agent_with_helper(&h, &helper_pid);
    let _reap = FakeReaper(fake.pids_log.clone());
    let mut parent = GuiParent::start(&h, &[("PATH", fake.path_env().as_str())]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    let id = Uuid::new_v4();
    c.open(id, claude_spec(&h.project("p"), None));
    let helper = read_pid(&helper_pid);
    let _reap_helper = PidReaper(vec![helper]);
    std::thread::sleep(Duration::from_millis(300));
    c.request(&ClientMsg::TermClose { terminal: id }).unwrap();
    c.terminals_where(|l| l.is_empty());
    // Well inside the 2 s grace period.
    assert!(alive(helper));
    let daemon = parent.daemon_pid();
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(5)));
    assert!(
        wait_dead(helper, Duration::from_millis(500)),
        "the helper survived"
    );
}

/// A Terminal restored early whose agent exits while restore is stalled: the aborted start
/// still keeps every Terminal in the state file.
#[test]
fn aborted_restore_keeps_terminals_whose_agent_exited() {
    let h = TestHome::new();
    let exit_now = h.root().join("exit-now");
    let fake = fake_claude_with(
        &h,
        ":",
        &format!("[ -e {} ] && exit 0\nexec sleep 1000", exit_now.display()),
    );
    let _reap = FakeReaper(fake.pids_log.clone());
    let path = fake.path_env();
    let mut parent = GuiParent::start(&h, &[("PATH", path.as_str())]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    for _ in 0..2 {
        c.open(Uuid::new_v4(), claude_spec(&h.project("p"), None));
    }
    fake.wait_pids(2);
    let daemon = parent.daemon_pid();
    drop(c);
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(3)));
    let mut saved = h.state_ids();
    saved.sort();
    assert_eq!(saved.len(), 2);

    // Relaunched agents now exit at once.
    fs::write(&exit_now, "").unwrap();
    let mut parent = GuiParent::start(
        &h,
        &[
            ("PATH", path.as_str()),
            ("XSHELLD_TEST_RESTORE_DELAY_MS", "1500"),
        ],
    );
    let daemon = parent.daemon_pid();
    fake.wait_pids(3);
    // Let the first restored agent's exit be handled while restore stalls.
    std::thread::sleep(Duration::from_millis(500));
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(4)));
    let mut now = h.state_ids();
    now.sort();
    assert_eq!(now, saved);
}

/// An rc file that leaves a background job holding stdout: a parent death ends the probe and
/// the job at once.
#[test]
fn parent_death_ends_a_stdout_holding_probe_helper() {
    let h = TestHome::new();
    let helper_pid = h.root().join("helper.pid");
    let shell = h.script(
        "holding-shell",
        &format!("sleep 30 &\necho $! > {}\nexit 0", helper_pid.display()),
    );
    let mut parent = GuiParent::start(
        &h,
        &[
            ("XSHELLD_LOGIN_ENV", "1"),
            ("SHELL", shell.to_str().unwrap()),
        ],
    );
    let daemon = parent.daemon_pid();
    let helper = read_pid(&helper_pid);
    let _reap = PidReaper(vec![helper]);
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    parent.kill();
    assert!(
        wait_dead(daemon, Duration::from_secs(2)),
        "the start went on"
    );
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(
        wait_dead(helper, Duration::from_secs(1)),
        "the probe's job survived"
    );
    // Only the interactive probe ran: none after the cancellation.
    assert_eq!(
        fs::read_to_string(&helper_pid).unwrap().trim(),
        helper.to_string()
    );
    assert_eq!(log_lines(&h, "serving"), 0);
}

/// Make the mode marker unwritable: a non-empty directory in its place.
fn block_marker(h: &TestHome) {
    fs::create_dir_all(h.paths().mode.join("x")).unwrap();
}

#[test]
fn gui_bound_start_fails_without_mode_marker() {
    let h = TestHome::new();
    block_marker(&h);
    let parent = GuiParent::start(&h, &[]);
    let daemon = parent.daemon_pid();
    assert!(wait_dead(daemon, Duration::from_secs(3)), "it served");
    assert!(!h.paths().socket.exists());
    assert!(!h.paths().pid.exists());
    assert_eq!(log_lines(&h, "serving"), 0);
    assert!(log_lines(&h, "cannot write") >= 1);
}

#[test]
fn persistent_start_fails_without_mode_marker() {
    let h = TestHome::new();
    block_marker(&h);
    match xshelld::server::Server::start(config(&h)) {
        Err(xshelld::server::StartError::Io(e)) => {
            assert!(e.to_string().contains("cannot write"), "{e}")
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("it served"),
    }
    assert!(!h.paths().socket.exists());
    assert!(!h.paths().pid.exists());
    // The lock is released: a later start with a writable marker works.
    fs::remove_dir_all(h.paths().mode).unwrap();
    let srv = xshelld::server::Server::start(config(&h)).expect("starts");
    drop(srv);
}

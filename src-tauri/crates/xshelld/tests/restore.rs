#![cfg(unix)]
//! Restart and restore through the real binary: resume flags, PATH, crash leftovers and the
//! SIGTERM path. A fake `claude` records its argv and pid.

mod common;

use common::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::claude::encode_project_name;
use xshell_core::launch::LaunchSpec;
use xshell_core::protocol::msg::ClientMsg;

struct Fake {
    bin: PathBuf,
    argv_log: PathBuf,
    pids_log: PathBuf,
}

/// `fake-bin/claude`: ignores SIGHUP like a stubborn agent, logs argv (one block per launch,
/// ended by `--`) and then its pid, then sleeps. A logged pid means the trap is in place.
fn fake_claude(h: &TestHome) -> Fake {
    let bin = h.root().join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let argv_log = h.root().join("argv.log");
    let pids_log = h.root().join("pids.log");
    let p = bin.join("claude");
    fs::write(
        &p,
        format!(
            "#!/bin/sh\ntrap '' HUP\nprintf '%s\\n' \"$@\" -- >> '{}'\necho $$ >> '{}'\nexec sleep 1000\n",
            argv_log.display(),
            pids_log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    Fake {
        bin,
        argv_log,
        pids_log,
    }
}

impl Fake {
    fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// argv blocks, one per launch.
    fn launches(&self) -> Vec<Vec<String>> {
        let s = fs::read_to_string(&self.argv_log).unwrap_or_default();
        let mut out = vec![];
        let mut cur = vec![];
        for l in s.lines() {
            if l == "--" {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.push(l.to_string());
            }
        }
        out
    }

    fn wait_launches(&self, n: usize) -> Vec<Vec<String>> {
        let deadline = Instant::now() + T;
        loop {
            let l = self.launches();
            if l.len() >= n || Instant::now() >= deadline {
                return l;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_pids(&self, n: usize) -> Vec<i32> {
        let deadline = Instant::now() + T;
        loop {
            let p = self.pids();
            if p.len() >= n || Instant::now() >= deadline {
                assert!(p.len() >= n, "only {} fake agent(s) started", p.len());
                return p;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pids(&self) -> Vec<i32> {
        fs::read_to_string(&self.pids_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }
}

/// Kills every fake agent ever launched, whatever happens in the test.
struct FakeReaper(PathBuf);

impl Drop for FakeReaper {
    fn drop(&mut self) {
        for l in fs::read_to_string(&self.0).unwrap_or_default().lines() {
            if let Ok(p) = l.trim().parse::<i32>() {
                if p > 1 {
                    unsafe { libc::kill(p, libc::SIGKILL) };
                }
            }
        }
    }
}

fn claude_spec(cwd: &Path, session: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        agent: Some("claude".into()),
        shell_mode: Some("claude".into()),
        session_id: session.map(str::to_string),
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

fn make_jsonl(h: &TestHome, cwd: &Path, sid: &str) {
    let dir = h
        .home()
        .join(".claude/projects")
        .join(encode_project_name(&cwd.to_string_lossy()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{sid}.jsonl")), "{}\n").unwrap();
}

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

#![cfg(unix)]
//! The binary's command line: `--version`, `connect` auto-start and bridging, `serve` lock.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;
use uuid::Uuid;
use xshell_protocol::msg::ClientMsg;

#[test]
fn version_is_machine_readable() {
    let out = Command::new(bin()).arg("--version").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(s.lines().count(), 1);
    let v: Value = serde_json::from_str(&s).unwrap();
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        other => other,
    };
    assert_eq!(
        v,
        json!({"name": "xshelld", "version": env!("CARGO_PKG_VERSION"),
               "protocol": {"min": 1, "max": 1},
               "os": os, "arch": std::env::consts::ARCH})
    );
}

#[test]
fn bad_usage_exits_2() {
    let out = Command::new(bin()).arg("frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn connect_autostarts_and_bridges() {
    let h = TestHome::new();
    // A fresh Host: nothing under the home yet.
    assert!(!h.home().join(".xshell").exists());
    let _guard = DaemonGuard::new(&h);
    let mut p = ConnectProc::start(&h);
    p.client.hello(range(1, 1));
    assert_eq!(get_home(&mut p.client), json!(h.home().to_string_lossy()));
    let sock_dir = h.run().join("xshell");
    assert!(sock_dir.join("daemon.sock").exists());
    assert_eq!(
        fs::metadata(&sock_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(h.home().join(".xshell/log/xshelld.log").exists());
    assert!(p.finish().success());
}

#[test]
fn second_connect_reuses_daemon() {
    let h = TestHome::new();
    let _guard = DaemonGuard::new(&h);
    let t = Uuid::new_v4();
    let mut p1 = ConnectProc::start(&h);
    p1.client.hello(range(1, 1));
    p1.client.open(t, sh_spec(&h.project("p")));
    let daemon = _guard.pid();
    assert!(p1.finish().success());

    let mut p2 = ConnectProc::start(&h);
    let (_, list) = p2.client.hello(range(1, 1));
    assert_eq!(list.iter().map(|i| i.terminal).collect::<Vec<_>>(), vec![t]);
    assert_eq!(_guard.pid(), daemon);
    p2.client
        .request(&ClientMsg::TermClose { terminal: t })
        .unwrap();
    p2.client.terminals_where(|l| l.is_empty());
}

#[test]
fn concurrent_connects_start_one_daemon() {
    let h = TestHome::new();
    let _guard = DaemonGuard::new(&h);
    let mut ps: Vec<ConnectProc> = (0..4).map(|_| ConnectProc::start(&h)).collect();
    for p in &mut ps {
        p.client.hello(range(1, 1));
    }
    let t = Uuid::new_v4();
    ps[0].client.open(t, sh_spec(&h.project("p")));
    for p in &mut ps[1..] {
        p.client
            .terminals_where(|l| l.iter().any(|i| i.terminal == t));
    }
    // One Daemon: one serving process, the rest lost the lock race (exit 3) or never ran.
    let log = fs::read_to_string(h.home().join(".xshell/log/xshelld.log")).unwrap();
    assert_eq!(log.matches(" serving ").count(), 1, "{log}");
    ps[0]
        .client
        .request(&ClientMsg::TermClose { terminal: t })
        .unwrap();
}

#[test]
fn serve_already_running_exit_code() {
    let h = TestHome::new();
    let _s1 = ServeProc::start(&h, &[]);
    connect_socket(&h.paths().socket);
    let mut s2 = ServeProc::start(&h, &[]);
    let st = s2
        .wait_exit(Duration::from_secs(2))
        .expect("second serve exits");
    assert_eq!(st.code(), Some(3));
}

#[test]
fn connect_reports_unstartable_daemon() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores the permission this test relies on
    }
    let h = TestHome::new();
    // `serve` cannot create its socket directory under a read-only one.
    let ro = h.root().join("ro");
    fs::create_dir(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o500)).unwrap();
    let out = bin_cmd(&h)
        .arg("connect")
        .env("XSHELLD_SOCKET", ro.join("sub/d.sock"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("xshelld serve exited with"), "{err}");
}

/// A `serve` that `connect` spawned and that lost the lock race exits only after `connect`
/// has bridged to the winner; `connect` must still reap it while the bridge lives.
#[cfg(target_os = "linux")]
#[test]
fn connect_reaps_a_losing_serve() {
    use std::os::unix::net::UnixListener;
    let h = TestHome::new();
    let paths = h.paths();
    let sock_dir = paths.socket.parent().unwrap().to_path_buf();
    fs::create_dir_all(&sock_dir).unwrap();
    fs::set_permissions(&sock_dir, fs::Permissions::from_mode(0o700)).unwrap();
    // We hold the lock, so the spawned `serve` loses and waits for `release`.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock)
        .unwrap();
    lock.try_lock().unwrap();
    let release = h.root().join("release");
    let mut connect = bin_cmd(&h)
        .arg("connect")
        .env("XSHELLD_TEST_HOLD_LOSER", &release)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let cpid = connect.id() as i32;
    let stat = |p: i32| -> Option<(String, i32)> {
        let s = fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
        let f: Vec<&str> = s[s.rfind(')')? + 1..].split_whitespace().collect();
        Some((f[0].to_string(), f[1].parse().ok()?))
    };
    let children = || -> Vec<i32> {
        fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .filter(|&p| stat(p).is_some_and(|(_, pp)| pp == cpid))
            .collect()
    };
    let deadline = std::time::Instant::now() + T;
    let loser = loop {
        if let Some(&p) = children().first() {
            break p;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "connect spawned no serve"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    // Play the winner: `connect` bridges to us while the loser is still running.
    let listener = UnixListener::bind(&paths.socket).unwrap();
    let (_bridged, _) = listener.accept().unwrap();
    assert!(
        stat(loser).is_some_and(|(st, _)| st != "Z"),
        "loser exited too early"
    );
    fs::write(&release, "").unwrap();
    let deadline = std::time::Instant::now() + T;
    while stat(loser).is_some_and(|(_, pp)| pp == cpid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the losing serve {loser} was never reaped: {:?}",
            stat(loser)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(connect.try_wait().unwrap().is_none(), "the bridge ended");
    let _ = connect.kill();
    let _ = connect.wait();
}

/// After a GUI-bound Daemon ran here, `connect` starts none: a missing Daemon means xshell
/// is closed on this machine.
#[test]
fn connect_never_starts_gui_bound_daemon() {
    let h = TestHome::new();
    let mut parent = GuiParent::start(&h, &[]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    drop(c);
    let daemon = parent.daemon_pid();
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(3)));
    let serving = log_lines(&h, "serving");

    let out = bin_cmd(&h)
        .arg("connect")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(xshell_protocol::NOT_RUNNING_EXIT));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("xshell is not running"), "{err}");
    assert!(!err.contains("No such file") && !err.contains("Connection refused"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!h.paths().socket.exists());
    assert!(!h.paths().pid.exists());
    assert_eq!(log_lines(&h, "serving"), serving, "a Daemon was started");
}

#[test]
fn connect_bridges_to_running_gui_bound_daemon() {
    let h = TestHome::new();
    let _parent = GuiParent::start(&h, &[]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    let t = Uuid::new_v4();
    c.open(t, sh_spec(&h.project("p")));
    let mut p = ConnectProc::start(&h);
    let (_, list) = p.client.hello(range(1, 1));
    assert_eq!(list.iter().map(|i| i.terminal).collect::<Vec<_>>(), vec![t]);
    assert!(p.finish().success());
}

/// A Persistent `serve` takes the machine back from xshell: afterwards `connect` starts a
/// Daemon again when none runs.
#[test]
fn persistent_serve_reenables_autostart() {
    let h = TestHome::new();
    let mut parent = GuiParent::start(&h, &[]);
    Client::connect(&h.paths().socket).hello(range(1, 1));
    let daemon = parent.daemon_pid();
    parent.kill();
    assert!(wait_dead(daemon, Duration::from_secs(3)));
    assert_eq!(
        fs::read_to_string(h.paths().mode).unwrap().trim(),
        "gui-bound"
    );

    let mut serve = ServeProc::start(&h, &[]);
    Client::connect(&h.paths().socket).hello(range(1, 1));
    assert_eq!(
        fs::read_to_string(h.paths().mode).unwrap().trim(),
        "persistent"
    );
    unsafe { libc::kill(serve.pid(), libc::SIGTERM) };
    assert!(serve.wait_exit(T).is_some());

    let _guard = DaemonGuard::new(&h);
    let mut p = ConnectProc::start(&h);
    p.client.hello(range(1, 1));
    assert!(p.finish().success());
}

/// After the app hands its Terminals to a Persistent Daemon (`serve --interactive-env`),
/// `connect` starts a Daemon again when that one is gone.
#[test]
fn connect_autostarts_after_persistent_handover() {
    let h = TestHome::new();
    let mut parent = GuiParent::start(&h, &[]);
    Client::connect(&h.paths().socket).hello(range(1, 1));
    let daemon = parent.daemon_pid();
    unsafe { libc::kill(daemon, libc::SIGTERM) };
    assert!(wait_dead(daemon, T));
    parent.kill();

    let mut serve = ServeProc {
        child: bin_cmd(&h)
            .args(["serve", "--interactive-env"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    };
    Client::connect(&h.paths().socket).hello(range(1, 1));
    assert_eq!(
        fs::read_to_string(h.paths().mode).unwrap().trim(),
        "persistent"
    );
    unsafe { libc::kill(serve.pid(), libc::SIGKILL) };
    assert!(serve.wait_exit(T).is_some());

    let _guard = DaemonGuard::new(&h);
    let mut p = ConnectProc::start(&h);
    p.client.hello(range(1, 1));
    assert!(p.finish().success());
}

// ---- `xshelld pair` -------------------------------------------------------------------------

mod pair {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::Child;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Instant;
    use xshell_hostlink::ring::{
        DesktopRing, DesktopRingConfig, PairingEvent, PairingFlow, PairingObserver, RingObserver,
        RingView,
    };
    use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions};
    use xshell_protocol::ring::relay::RingTimeouts;
    use xshell_protocol::ring::{Role, RosterChain, SignKey};

    struct Quiet;
    impl RingObserver for Quiet {
        fn changed(&self, _: &RingView) {}
    }

    #[derive(Default)]
    struct Events(std::sync::Mutex<Vec<PairingEvent>>);
    impl PairingObserver for Events {
        fn pairing(&self, _: PairingFlow, e: &PairingEvent) {
            self.0.lock().unwrap().push(e.clone());
        }
    }
    impl Events {
        fn outcome(&self) -> PairingEvent {
            let deadline = Instant::now() + T;
            loop {
                if let Some(e) = self
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|e| **e != PairingEvent::Waiting)
                {
                    return e.clone();
                }
                assert!(Instant::now() < deadline, "no pairing outcome");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn timeouts() -> RingTimeouts {
        RingTimeouts {
            connect: Duration::from_secs(5),
            request: Duration::from_secs(2),
            ..RingTimeouts::default()
        }
    }

    fn desktop(dir: &std::path::Path, url: &str) -> Arc<DesktopRing> {
        let mut cfg = DesktopRingConfig::new(dir.join("ring"), "desk".into());
        cfg.default_relay_url = url.into();
        cfg.hosted_relay_url = url.into();
        cfg.backoff_unit = Duration::from_millis(10);
        cfg.timeouts = timeouts();
        let r = DesktopRing::open(cfg, Arc::new(Quiet));
        r.enable(None, false).unwrap();
        let deadline = Instant::now() + T;
        while r.view().connection != "connected" {
            assert!(Instant::now() < deadline, "the desktop connected");
            std::thread::sleep(Duration::from_millis(10));
        }
        r
    }

    fn relay() -> TestRelay {
        TestRelay::start_with(TestRelayOptions {
            auth_timeout: Duration::from_millis(500),
            ..TestRelayOptions::default()
        })
    }

    /// `xshelld pair` with its stdout read line by line.
    fn pair(h: &TestHome, url: &str, env: &[(&str, &str)]) -> (Child, mpsc::Receiver<String>) {
        let mut c = bin_cmd(h);
        c.args(["pair", "--relay", url, "--name", "box"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines() {
                let Ok(l) = l else { return };
                if tx.send(l).is_err() {
                    return;
                }
            }
        });
        (child, rx)
    }

    fn code(rx: &mpsc::Receiver<String>) -> String {
        loop {
            let l = rx.recv_timeout(T).expect("the code is shown");
            if let Some(c) = l.strip_prefix("Pairing code: ") {
                return c.to_string();
            }
        }
    }

    fn rest(rx: &mpsc::Receiver<String>) -> Vec<String> {
        rx.iter().collect()
    }

    fn finish(mut child: Child, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(s) = child.try_wait().unwrap() {
                return s.code();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stored(h: &TestHome) -> Option<RosterChain> {
        let p = h.paths().ring_dir.join("roster.json");
        let v: Value = serde_json::from_slice(&fs::read(p).ok()?).ok()?;
        let tokens: Vec<String> = serde_json::from_value(v["rosters"].clone()).unwrap();
        RosterChain::from_tokens(&tokens).ok()
    }

    fn daemon_key(c: &RosterChain) -> Option<SignKey> {
        c.head()
            .roster()
            .members
            .iter()
            .find(|m| m.role == Role::Daemon && m.name == "box")
            .map(|m| m.sign_key)
    }

    #[test]
    fn pair_prints_code_and_joins() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let ring = desktop(t.path(), &r.url());
        let h = TestHome::new();
        let (child, rx) = pair(&h, &r.url(), &[]);
        let code = code(&rx);
        assert_eq!(code.len(), 19, "{code}");
        // Typed on the Desktop.
        std::thread::sleep(Duration::from_millis(200));
        let ev = Arc::new(Events::default());
        ring.pair_computer(&code, ev.clone()).unwrap();
        assert_eq!(
            ev.outcome(),
            PairingEvent::Paired {
                name: "box".into(),
                role: Role::Daemon
            }
        );
        assert_eq!(finish(child, T), Some(0));
        let lines = rest(&rx);
        assert!(lines.iter().any(|l| l == "Paired as box"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("xshelld serve")),
            "{lines:?}"
        );
        let chain = stored(&h).expect("the chain is stored");
        assert_eq!(chain.head(), ring.chain().unwrap().head());
        assert!(daemon_key(&chain).is_some());
        // Its keys are its own and private.
        let keys = h.paths().ring_dir.join("keys.json");
        assert_eq!(
            fs::metadata(keys).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Pairing again is refused without --force.
        let out = bin_cmd(&h)
            .args(["pair", "--relay", &r.url()])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("--force"));
        ring.quit();
    }

    #[test]
    fn pair_hands_chain_to_running_serve() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let ring = desktop(t.path(), &r.url());
        let h = TestHome::new();
        let srv = start(&h, |c| {
            c.ring_backoff_unit = Duration::from_millis(10);
            c.ring_timeouts = timeouts();
        });
        let (child, rx) = pair(&h, &r.url(), &[]);
        let code = code(&rx);
        std::thread::sleep(Duration::from_millis(200));
        ring.pair_computer(&code, Arc::new(Events::default()))
            .unwrap();
        assert_eq!(finish(child, T), Some(0));
        let lines = rest(&rx);
        assert!(
            !lines.iter().any(|l| l.contains("not running")),
            "{lines:?}"
        );
        // The running serve took the chain and is online on the Relay.
        let mut c = Client::in_process(&srv, xshelld::server::Role::Desktop);
        let id = c.request(&ClientMsg::RingIdentity).unwrap();
        let head = ring.chain().unwrap();
        assert_eq!(id["ring"]["version"], json!(head.head().version()));
        let key = daemon_key(&head).unwrap();
        assert_eq!(id["signKey"], json!(key.to_b64()));
        let deadline = Instant::now() + T;
        while !r.presence(head.ring_id(), &key).is_some_and(|p| p.online) {
            assert!(Instant::now() < deadline, "the daemon is online");
            std::thread::sleep(Duration::from_millis(10));
        }
        ring.quit();
    }

    #[test]
    fn pair_wrong_code_adds_nothing() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let ring = desktop(t.path(), &r.url());
        let before = ring.chain().unwrap();
        let h = TestHome::new();
        let (child, rx) = pair(&h, &r.url(), &[("XSHELLD_PAIR_TTL_MS", "1500")]);
        let code = code(&rx);
        let mut wrong: Vec<char> = code.chars().collect();
        wrong[0] = if wrong[0] == '0' { '1' } else { '0' };
        let ev = Arc::new(Events::default());
        ring.pair_computer(&wrong.iter().collect::<String>(), ev.clone())
            .unwrap();
        assert_eq!(
            ev.outcome(),
            PairingEvent::Failed {
                code: "not_found".into()
            }
        );
        assert_eq!(finish(child, T), Some(5));
        assert!(rest(&rx)
            .iter()
            .any(|l| l == "This code expired. Run xshelld pair again to get a new one."));
        assert_eq!(ring.chain().unwrap(), before);
        assert!(stored(&h).is_none());
        ring.quit();
    }

    #[test]
    fn pair_expires() {
        let r = relay();
        let h = TestHome::new();
        let (child, rx) = pair(&h, &r.url(), &[("XSHELLD_PAIR_TTL_MS", "500")]);
        code(&rx);
        assert_eq!(finish(child, T), Some(5));
        assert!(stored(&h).is_none());
    }
}

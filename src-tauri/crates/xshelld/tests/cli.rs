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

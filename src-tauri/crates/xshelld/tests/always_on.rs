#![cfg(unix)]
//! The always-on Daemon (ADR-0017): `xshelld service`, the stable launcher, one endpoint for
//! every client of a home (A2), no recursive start from the profile block (A3), and the
//! Desktop's registration starting the normal `serve` first.
//!
//! Every test forces the `profile` mechanism (or `none`) on a scratch HOME: nothing here
//! touches the real user's crontab, systemd units or shell profile (A1).

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `xshelld` for `h` with the forced mechanism and no idle exit.
fn svc(h: &TestHome) -> Command {
    let mut c = bin_cmd(h);
    c.env_remove("XSHELLD_IDLE_TIMEOUT_MS")
        .env("XSHELL_SERVICE_MECHANISM", "profile")
        .env("XSHELL_SERVICE_POLL_MS", "100")
        .env_remove("XSHELL_SERVICE_GUARD")
        .env("SHELL", "/bin/sh");
    c
}

fn json_out(c: &mut Command) -> Value {
    let out = c.stdin(Stdio::null()).output().unwrap();
    assert!(
        out.status.success(),
        "{:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).unwrap();
    serde_json::from_str(s.trim()).unwrap_or_else(|e| panic!("{e}: {s}"))
}

/// The processes (pid, argv) whose command line mentions `needle` and `word`.
fn procs(needle: &Path, word: &str) -> Vec<(i32, Vec<String>)> {
    let needle = needle.to_string_lossy().into_owned();
    let mut v = Vec::new();
    for e in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(raw) = fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        if argv.iter().any(|a| a.contains(&needle)) && argv.iter().any(|a| a == word) && alive(pid)
        {
            v.push((pid, argv));
        }
    }
    v
}

fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    f()
}

/// Ends every process of this home (by pid) when dropped.
struct Reap(PathBuf);

impl Drop for Reap {
    fn drop(&mut self) {
        for word in ["serve", "service"] {
            for (pid, _) in procs(&self.0, word) {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                if !wait_dead(pid, T) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }
}

fn pidfile_pid(h: &TestHome) -> Option<i32> {
    fs::read_to_string(h.paths().pid).ok()?.trim().parse().ok()
}

#[test]
fn service_install_profile_fresh_home() {
    let h = TestHome::new();
    let home = h.home();
    let first = json_out(svc(&h).args(["service", "install", "--json"]));
    assert_eq!(first["mechanism"], "profile");
    assert_eq!(first["changed"], true);
    assert_eq!(first["installed"], serde_json::json!(["profile"]));
    assert_eq!(first["linger"], Value::Null);
    let launcher = home.join(".xshell/service/xshelld");
    assert_eq!(first["launcher"], launcher.to_string_lossy().as_ref());
    let again = json_out(svc(&h).args(["service", "install", "--json"]));
    assert_eq!(again["changed"], false, "{again}");
    // One block, in a .profile created for it.
    let profile = fs::read_to_string(home.join(".profile")).unwrap();
    assert_eq!(profile.matches(">>> xshell daemon >>>").count(), 1);
    assert!(profile.contains("XSHELL_SERVICE_GUARD"));
    for p in [launcher.clone(), launcher.parent().unwrap().to_path_buf()] {
        let m = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o700, "{}", p.display());
    }
    // Its own build was installed, so the launcher has something to run.
    let installed = fs::read_dir(home.join(".xshell/server")).unwrap().count();
    assert!(installed >= 1);
    let st = json_out(svc(&h).args(["service", "status", "--json"]));
    assert_eq!(st["mechanism"], "profile");
    assert_eq!(st["running"], false);
    assert!(st.get("changed").is_none());
    let text = svc(&h).args(["service", "status"]).output().unwrap();
    assert!(String::from_utf8_lossy(&text.stdout).contains("shell profile"));
    // Uninstall: the block and the launcher are gone; nothing else was touched.
    let out = svc(&h).args(["service", "uninstall"]).output().unwrap();
    assert!(out.status.success());
    assert!(!fs::read_to_string(home.join(".profile"))
        .unwrap()
        .contains("xshell daemon"));
    assert!(!launcher.exists());
    assert!(!home.join(".config/systemd").exists());
    // `none`: nothing is written at all.
    let n = json_out(
        svc(&h)
            .env("XSHELL_SERVICE_MECHANISM", "none")
            .args(["service", "install", "--json"]),
    );
    assert_eq!(n["mechanism"], "none");
    assert!(!fs::read_to_string(home.join(".profile"))
        .unwrap()
        .contains("xshell daemon"));
}

#[test]
fn service_ensure_starts_one_daemon() {
    let h = TestHome::new();
    let _reap = Reap(h.home());
    json_out(svc(&h).args(["service", "install", "--json"]));
    // Two ensures at once, then more while it runs: exactly one Daemon.
    let a = svc(&h).args(["service", "ensure"]).spawn().unwrap();
    let b = svc(&h).args(["service", "ensure"]).spawn().unwrap();
    for mut c in [a, b] {
        assert!(c.wait().unwrap().success());
    }
    assert!(wait_until(Duration::from_secs(10), || {
        std::os::unix::net::UnixStream::connect(h.paths().socket).is_ok()
    }));
    for _ in 0..3 {
        assert!(svc(&h)
            .args(["service", "ensure"])
            .status()
            .unwrap()
            .success());
    }
    std::thread::sleep(Duration::from_millis(800));
    let serves = procs(&h.home(), "serve");
    assert_eq!(serves.len(), 1, "{serves:?}");
    assert_eq!(Some(serves[0].0), pidfile_pid(&h));
    // It runs from the installed build, not the test binary.
    assert!(serves[0].1[0].contains("/.xshell/server/"), "{serves:?}");
    assert!(
        procs(&h.home(), "service").is_empty(),
        "a waiting service run is left"
    );
    let st = json_out(svc(&h).args(["service", "status", "--json"]));
    assert_eq!(st["running"], true);
}

/// `service run` waits while another Daemon holds this home, then becomes `serve` itself
/// (same pid).
#[test]
fn service_run_waits_for_lock_then_serves() {
    let h = TestHome::new();
    let _reap = Reap(h.home());
    json_out(svc(&h).args(["service", "install", "--json"]));
    let mut other = ServeProc::start(&h, &[("XSHELLD_IDLE_TIMEOUT_MS", "600000")]);
    connect_socket(&h.paths().socket);
    let mut run = svc(&h)
        .args(["service", "run", "--home"])
        .arg(h.home())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(700));
    assert!(run.try_wait().unwrap().is_none(), "run gave up");
    assert_eq!(pidfile_pid(&h), Some(other.pid()), "run took over early");
    assert!(
        procs(&h.home(), "serve").is_empty(),
        "run started a serve early"
    );
    unsafe { libc::kill(other.pid(), libc::SIGTERM) };
    assert!(other.wait_exit(T).is_some());
    assert!(wait_until(Duration::from_secs(10), || pidfile_pid(&h)
        == Some(run.id() as i32)
        && std::os::unix::net::UnixStream::connect(h.paths().socket)
            .is_ok()));
    let argv = fs::read(format!("/proc/{}/cmdline", run.id())).unwrap();
    let argv = String::from_utf8_lossy(&argv).replace('\0', " ");
    assert!(argv.contains(" serve --interactive-env"), "{argv}");
    unsafe { libc::kill(run.id() as i32, libc::SIGTERM) };
    let _ = run.wait();
}

/// A5: after waiting, `service run` starts what is selected *then*, never its own path.
#[test]
fn service_run_execs_selected_build() {
    let h = TestHome::new();
    let _reap = Reap(h.home());
    json_out(svc(&h).args(["service", "install", "--json"]));
    let mut other = ServeProc::start(&h, &[("XSHELLD_IDLE_TIMEOUT_MS", "600000")]);
    connect_socket(&h.paths().socket);
    let mut run = svc(&h)
        .args(["service", "run", "--home"])
        .arg(h.home())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // While it waits: a new build is selected and the launcher is rewritten.
    let root = h.home().join(".xshell/server");
    let dir = root.join("9.9.9+fake");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let marker = h.root().join("selected-ran");
    let fake = dir.join("xshelld");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\nexec '{}' \"$@\"\n",
            marker.display(),
            bin()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    xshell_core::server::write_selected(&h.home(), "9.9.9+fake").unwrap();
    let launcher = h.home().join(".xshell/service/xshelld");
    let replaced = fs::read_to_string(&launcher)
        .unwrap()
        .replace("# xshell:", "# xshell (replaced):");
    fs::write(&launcher, replaced).unwrap();
    unsafe { libc::kill(other.pid(), libc::SIGTERM) };
    assert!(other.wait_exit(T).is_some());
    assert!(wait_until(Duration::from_secs(10), || {
        std::os::unix::net::UnixStream::connect(h.paths().socket).is_ok()
            && pidfile_pid(&h) == Some(run.id() as i32)
    }));
    assert_eq!(
        fs::read_to_string(&marker).unwrap().trim(),
        format!("serve --interactive-env --home {}", h.home().display())
    );
    unsafe { libc::kill(run.id() as i32, libc::SIGTERM) };
    let _ = run.wait();
}

/// A2: one Daemon per home, whatever each process's XDG_RUNTIME_DIR says.
#[test]
fn endpoint_is_found_with_and_without_xdg_runtime_dir() {
    for daemon_has_xdg in [false, true] {
        let h = TestHome::new();
        let _reap = Reap(h.home());
        let mut serve = bin_cmd(&h);
        serve
            .arg("serve")
            .arg("--home")
            .arg(h.home())
            .env("XSHELLD_IDLE_TIMEOUT_MS", "600000")
            .stdin(Stdio::null());
        if !daemon_has_xdg {
            serve.env_remove("XDG_RUNTIME_DIR");
        }
        let mut serve = serve.spawn().unwrap();
        let sock = if daemon_has_xdg {
            h.paths().socket
        } else {
            h.home().join(".xshell/run/daemon.sock")
        };
        connect_socket(&sock);
        let record = fs::read_to_string(h.home().join(".xshell/run/daemon.endpoint")).unwrap();
        assert_eq!(
            record,
            format!("{}\n{}\n", serve.id(), sock.display()),
            "the record names this Daemon"
        );
        // The client runs with the other environment.
        let mut c = bin_cmd(&h);
        c.arg("connect");
        if daemon_has_xdg {
            c.env_remove("XDG_RUNTIME_DIR");
        }
        let mut p = ConnectProc::spawn(c);
        p.client.hello(range(1, 1));
        assert!(p.finish().success());
        let serves = procs(&h.home(), "serve");
        assert_eq!(serves.len(), 1, "a second Daemon was started: {serves:?}");
        assert_eq!(serves[0].0, serve.id() as i32);
        // `service ensure` with the other environment sees it alive too.
        let mut e = svc(&h);
        e.args(["service", "ensure"]);
        if daemon_has_xdg {
            e.env_remove("XDG_RUNTIME_DIR");
        }
        assert!(e.status().unwrap().success());
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(procs(&h.home(), "serve").len(), 1);
        unsafe { libc::kill(serve.id() as i32, libc::SIGTERM) };
        let _ = serve.wait();
        // A clean exit removes the record.
        assert!(!h.home().join(".xshell/run/daemon.endpoint").exists());
    }
}

/// A3: with real login preparation and the profile block installed, starting the Daemon
/// starts exactly one, and leaves no waiting `service run` behind.
#[test]
fn profile_block_never_starts_a_second_daemon() {
    let h = TestHome::new();
    let _reap = Reap(h.home());
    json_out(svc(&h).args(["service", "install", "--json"]));
    // `sh -l` reads ~/.profile, which now holds the block.
    assert!(fs::read_to_string(h.home().join(".profile"))
        .unwrap()
        .contains("service ensure"));
    let ok = svc(&h)
        .env_remove("XSHELLD_LOGIN_ENV")
        .args(["service", "ensure"])
        .status()
        .unwrap();
    assert!(ok.success());
    assert!(wait_until(Duration::from_secs(20), || {
        std::os::unix::net::UnixStream::connect(h.paths().socket).is_ok()
    }));
    // The interactive login shell ran (and with it the block); give anything it started
    // time to show up.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(procs(&h.home(), "serve").len(), 1);
    assert!(procs(&h.home(), "service").is_empty());
    assert_eq!(log_lines(&h, "started by the profile service"), 1);
    assert_eq!(log_lines(&h, "another xshelld serve holds the lock"), 0);
}

/// The Desktop's registration starts the normal `serve`, never the helper child.
#[test]
fn serve_first_registration_starts_serve_not_helper() {
    isolate_home();
    let t = tempfile::Builder::new()
        .prefix("sf")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().canonicalize().unwrap();
    let run = root.join("run");
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    let _reap = Reap(root.clone());
    let socket = run.join("xshell/daemon.sock");
    fs::create_dir(root.join("desktop")).unwrap();
    fs::set_permissions(root.join("desktop"), fs::Permissions::from_mode(0o700)).unwrap();
    let local = xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin: bin().into(),
        home: root.clone(),
        socket: socket.clone(),
        env: vec![
            ("HOME".into(), root.clone().into()),
            ("XDG_RUNTIME_DIR".into(), run.clone().into()),
            ("XSHELLD_LOGIN_ENV".into(), "0".into()),
        ],
        version: env!("CARGO_PKG_VERSION").into(),
        build: xshelld::build_id::BUILD.clone(),
    })
    .unwrap();
    local.set_persistent(true);
    local.set_serve_first(true);
    let mut ran = false;
    local
        .with_registration_endpoint(&root.join("desktop"), |_client| {
            ran = true;
            Ok(())
        })
        .unwrap();
    assert!(ran);
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
    let pid = fs::read_to_string(run.join("xshell/daemon.pid")).unwrap();
    let argv = fs::read(format!("/proc/{}/cmdline", pid.trim())).unwrap();
    let argv = String::from_utf8_lossy(&argv).replace('\0', " ");
    assert!(argv.contains(" serve --interactive-env"), "{argv}");
    assert!(procs(&root, "companion-registration").is_empty());
    local.hang_up();
}

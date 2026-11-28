#![cfg(windows)]
//! A GUI-bound Windows Daemon takes an update over its link like an always-on Linux one
//! (rjw.9 D2/D3): it offers `daemon.stage` and `daemon.update-latest` when its Desktop
//! restarts it into the selected build (`--supports-selected-restart`), installs the build
//! where that restart reads it (`%USERPROFILE%\.xshell\server\<dir>\xshelld.exe`), with the
//! same checks (SHA-256, size, build stamp, the installed file's own identity), and leaves
//! no part file behind. The "other build" is this test's `xshelld.exe` with another stamp or
//! version patched in (equal-length edits), so it really runs.
//!
//! Real processes only where a build runs for its identity; the Daemon itself runs in this
//! test process on a unique pipe. No test reaches the real release server: update-latest
//! asks a loopback stub, and every other Daemon gets a closed local URL.

#[path = "common/get_stub.rs"]
#[allow(dead_code)]
mod get_stub;
mod win;

use base64::Engine;
use get_stub::{closed_get_url, GetStub};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;
use win::*;
use xshell_protocol::build::{install_dir_name, scan_stamp, BuildId};
use xshell_protocol::cap::{DAEMON_STAGE, DAEMON_UPDATE_LATEST, DAEMON_UPGRADE_SELECTED};
use xshell_protocol::msg::{
    ClientMsg, UPDATE_LATEST_BAD_BUILD, UPDATE_LATEST_NO_BUILD, UPDATE_LATEST_UNREACHABLE,
};
use xshelld::server::{Config, ExitReason, GuiBound, Server, ServerHandle};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TRIPLE: &str = "x86_64-pc-windows-msvc";

/// A GUI-bound Daemon of this test process for `h`, with selected restart when `restart`,
/// asking `get_base` for the latest release.
fn start(h: &TestHome, restart: bool, get_base: String) -> ServerHandle {
    let paths = xshelld::paths::resolve(&h.home(), None, Some(&h.pipe));
    let mut cfg = Config::new(h.home(), paths);
    cfg.gui_bound = Some(GuiBound {
        parent_pid: std::process::id(),
    });
    cfg.supports_selected_restart = restart;
    cfg.get_base = get_base;
    Server::start(cfg).unwrap()
}

fn root(h: &TestHome) -> PathBuf {
    xshell_core::server::server_root(&h.home())
}

fn selected(h: &TestHome) -> Option<String> {
    fs::read_to_string(root(h).join("selected"))
        .ok()
        .map(|t| t.trim_end().to_string())
}

/// Every part file left in any install directory.
fn part_files(h: &TestHome) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in fs::read_dir(root(h)).into_iter().flatten().flatten() {
        for f in fs::read_dir(d.path()).into_iter().flatten().flatten() {
            if f.file_name()
                .to_string_lossy()
                .starts_with(".xshelld.stage-")
            {
                out.push(f.path());
            }
        }
    }
    out
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// This test's `xshelld.exe` with its build stamp's time moved by `delta` and another
/// commit, and every literal of this version replaced by `version` (same length).
fn patched(delta: i64, version: &str) -> (Vec<u8>, BuildId) {
    assert_eq!(version.len(), VERSION.len());
    let mut bytes = fs::read(bin()).unwrap();
    let original = scan_stamp(&bytes).unwrap().unwrap();
    let stamp = format!(
        "@@XSHELL-BUILD-V1:{}:{}@@",
        original.display(),
        original.time
    );
    let time = (original.time as i64 + delta).to_string();
    assert_eq!(time.len(), original.time.to_string().len());
    let mut commit = original.display().into_bytes();
    commit[0] = if commit[0] == b'0' { b'1' } else { b'0' };
    let replacement = format!(
        "@@XSHELL-BUILD-V1:{}:{time}@@",
        String::from_utf8(commit).unwrap()
    );
    let replace = |bytes: &mut Vec<u8>, from: &[u8], to: &[u8]| {
        let mut n = 0;
        let mut i = 0;
        while i + from.len() <= bytes.len() {
            if bytes[i..].starts_with(from) {
                bytes[i..i + from.len()].copy_from_slice(to);
                n += 1;
                i += from.len();
            } else {
                i += 1;
            }
        }
        n
    };
    assert!(replace(&mut bytes, stamp.as_bytes(), replacement.as_bytes()) > 0);
    if version != VERSION {
        assert!(replace(&mut bytes, VERSION.as_bytes(), version.as_bytes()) > 0);
    }
    let build = scan_stamp(&bytes).unwrap().unwrap();
    assert!(!build.same(&original));
    (bytes, build)
}

/// A version newer than this one, of the same length (`1.x` becomes `2.x`).
fn newer_version() -> String {
    assert!(
        VERSION.starts_with("1."),
        "the fixture assumes a 1.x version"
    );
    format!("2{}", &VERSION[1..])
}

fn hello_caps(h: &TestHome) -> Vec<String> {
    let mut c = Client::connect(&h.pipe);
    c.hello().0.capabilities
}

/// Stage `bytes` as `version`+`build` over `c`, chunk by chunk.
fn stage(c: &mut Client, version: &str, build: &BuildId, bytes: &[u8]) -> Result<String, String> {
    let begun = c.request(&ClientMsg::DaemonStageBegin {
        version: version.into(),
        build: build.clone(),
        sha256: sha(bytes),
        length: bytes.len() as u64,
    })?;
    if begun["installed"] == json!(true) {
        return Ok(install_dir_name(version, Some(build)));
    }
    let id: Uuid = serde_json::from_value(begun["stage"].clone()).unwrap();
    let chunk = xshell_protocol::msg::STAGE_MAX_CHUNK as usize;
    for (n, part) in bytes.chunks(chunk).enumerate() {
        c.request(&ClientMsg::DaemonStageChunk {
            stage: id,
            offset: (n * chunk) as u64,
            data: base64::engine::general_purpose::STANDARD.encode(part),
        })?;
    }
    let done = c.request(&ClientMsg::DaemonStageCommit { stage: id })?;
    Ok(done["dir"].as_str().unwrap().to_string())
}

fn assert_prefix(e: &str, prefix: &str) {
    assert!(
        e.starts_with(prefix),
        "{e:?} does not start with {prefix:?}"
    );
}

/// D2: only a GUI-bound Daemon whose Desktop restarts it into the selected build offers the
/// update capabilities; without selected restart staging is refused before anything is
/// written. `daemon.update-info` names the Windows target either way.
#[test]
fn update_capabilities_follow_selected_restart() {
    let with = TestHome::new_private_home();
    let srv = start(&with, true, closed_get_url());
    let caps = hello_caps(&with);
    for cap in [DAEMON_STAGE, DAEMON_UPDATE_LATEST, DAEMON_UPGRADE_SELECTED] {
        assert!(caps.iter().any(|c| c == cap), "{cap}: {caps:?}");
    }
    let mut c = Client::connect(&with.pipe);
    c.hello();
    let info = c.request(&ClientMsg::DaemonUpdateInfo).unwrap();
    assert_eq!(info["target"], json!(TRIPLE));
    srv.shutdown();

    let without = TestHome::new_private_home();
    let srv = start(&without, false, closed_get_url());
    let caps = hello_caps(&without);
    for cap in [DAEMON_STAGE, DAEMON_UPDATE_LATEST, DAEMON_UPGRADE_SELECTED] {
        assert!(!caps.iter().any(|c| c == cap), "{cap}: {caps:?}");
    }
    let mut c = Client::connect(&without.pipe);
    c.hello();
    let (bytes, build) = patched(1, VERSION);
    let e = stage(&mut c, VERSION, &build, &bytes).unwrap_err();
    assert!(e.contains("cannot be updated over its link"), "{e}");
    let e = c.request(&ClientMsg::DaemonUpdateLatest).unwrap_err();
    assert!(e.contains("cannot be updated over its link"), "{e}");
    assert!(!root(&without).exists() || part_files(&without).is_empty());
    srv.shutdown();
}

/// D3 and D1: a staged build lands where the selected restart reads it, private, runnable
/// and of the announced identity, with nothing selected; staging it again is answered
/// "installed" at once. `daemon.upgrade-selected` then selects it and the Daemon exits for
/// its Desktop to start it (`windows_spawn_target` now names the staged file).
#[test]
fn staged_build_is_installed_where_the_selected_restart_reads() {
    let h = TestHome::new_private_home();
    let srv = start(&h, true, closed_get_url());
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let (bytes, build) = patched(1, VERSION);
    let dir = stage(&mut c, VERSION, &build, &bytes).unwrap();
    assert_eq!(dir, install_dir_name(VERSION, Some(&build)));
    let file = root(&h).join(&dir).join("xshelld.exe");
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(
        xshell_core::server::windows_install(&h.home(), &dir).unwrap(),
        file
    );
    let sddl = xshell_core::private_fs::security_sddl(&file).unwrap();
    assert!(!sddl.contains(";;;WD)"), "{sddl}");
    assert!(part_files(&h).is_empty(), "{:?}", part_files(&h));
    assert_eq!(selected(&h), None, "staging selects nothing");
    assert!(!srv.stage_active());
    // Again: already installed, nothing sent.
    let again = c
        .request(&ClientMsg::DaemonStageBegin {
            version: VERSION.into(),
            build: build.clone(),
            sha256: sha(&bytes),
            length: bytes.len() as u64,
        })
        .unwrap();
    assert_eq!(again["installed"], json!(true));
    assert!(again.get("stage").is_none_or(|s| s.is_null()), "{again}");
    // The existing restart path takes it from here.
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build: build.clone(),
    })
    .unwrap();
    assert_eq!(srv.wait_timeout(T), Some(ExitReason::Upgrade));
    assert_eq!(selected(&h).as_deref(), Some(dir.as_str()));
    assert_eq!(
        xshell_core::server::windows_spawn_target(&h.home(), Path::new(bin())).unwrap(),
        file
    );
}

/// D3: bytes that fail a check (SHA-256, the build stamp against the announcement, the
/// installed file's own identity) publish nothing and leave no part file.
#[test]
fn staged_build_that_fails_a_check_installs_nothing() {
    let h = TestHome::new_private_home();
    let srv = start(&h, true, closed_get_url());
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let (bytes, build) = patched(1, VERSION);
    let dir = install_dir_name(VERSION, Some(&build));
    let file = root(&h).join(&dir).join("xshelld.exe");

    // A corrupted byte: the SHA-256 differs at commit.
    let begun = c
        .request(&ClientMsg::DaemonStageBegin {
            version: VERSION.into(),
            build: build.clone(),
            sha256: sha(&bytes),
            length: bytes.len() as u64,
        })
        .unwrap();
    let id: Uuid = serde_json::from_value(begun["stage"].clone()).unwrap();
    let mut bad = bytes.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xff;
    let chunk = xshell_protocol::msg::STAGE_MAX_CHUNK as usize;
    for (n, part) in bad.chunks(chunk).enumerate() {
        c.request(&ClientMsg::DaemonStageChunk {
            stage: id,
            offset: (n * chunk) as u64,
            data: base64::engine::general_purpose::STANDARD.encode(part),
        })
        .unwrap();
    }
    assert!(c
        .request(&ClientMsg::DaemonStageCommit { stage: id })
        .is_err());
    assert!(!file.exists());
    assert!(wait_until(T, || part_files(&h).is_empty()));

    // Another build announced than the bytes carry.
    let other = BuildId {
        commit: if build.commit == "abcdefabcdef" {
            "fedcbafedcba"
        } else {
            "abcdefabcdef"
        }
        .into(),
        ..build.clone()
    };
    let e = stage(&mut c, VERSION, &other, &bytes).unwrap_err();
    assert!(e.contains("does not match"), "{e}");
    assert!(!root(&h)
        .join(install_dir_name(VERSION, Some(&other)))
        .join("xshelld.exe")
        .exists());

    // Stamped bytes that are not a Windows program: refused at the identity check.
    let fake = format!(
        "@@XSHELL-BUILD-V1:{}:{}@@ not a program",
        build.display(),
        build.time
    )
    .into_bytes();
    assert!(stage(&mut c, VERSION, &build, &fake).is_err());
    assert!(!file.exists());
    assert!(wait_until(T, || part_files(&h).is_empty()));
    assert_eq!(selected(&h), None);
    assert!(!srv.stage_active());
    srv.shutdown();
}

/// D3: `daemon.update-latest` downloads `xshelld-x86_64-pc-windows-msvc.exe` and its
/// `.sha256` from the release server and installs it like a stage; nothing is selected.
#[test]
fn update_latest_installs_the_windows_release() {
    let stub = GetStub::start();
    let next = newer_version();
    let (bytes, build) = patched(0, &next);
    stub.release(&next, TRIPLE, &bytes, None);
    let h = TestHome::new_private_home();
    let srv = start(&h, true, stub.url());
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let r = c.request(&ClientMsg::DaemonUpdateLatest).unwrap();
    let dir = install_dir_name(&next, Some(&build));
    assert_eq!(
        r,
        json!({"current": false, "version": next, "build": build, "dir": dir})
    );
    let file = root(&h).join(&dir).join("xshelld.exe");
    assert_eq!(fs::read(&file).unwrap(), bytes);
    xshell_core::server::windows_install(&h.home(), &dir).unwrap();
    assert!(part_files(&h).is_empty());
    assert_eq!(selected(&h), None);
    assert!(!srv.stage_active());
    let asset = GetStub::asset(&next, TRIPLE);
    assert!(
        asset.ends_with("xshelld-x86_64-pc-windows-msvc.exe"),
        "{asset}"
    );
    assert_eq!(
        stub.hits(),
        vec!["/latest".to_string(), format!("{asset}.sha256"), asset]
    );
    // Asked again: the same bytes are there already.
    let r = c.request(&ClientMsg::DaemonUpdateLatest).unwrap();
    assert_eq!(r["dir"], json!(dir));
    // Not newer: current, nothing downloaded.
    stub.body("/latest", format!("{VERSION}\n"));
    let r = c.request(&ClientMsg::DaemonUpdateLatest).unwrap();
    assert_eq!(r, json!({"current": true, "version": VERSION}));
    srv.shutdown();
}

/// D3 failure paths: a wrong checksum, a missing asset and an unreachable server each
/// answer with their prefix, install nothing and leave no directory or part file behind.
#[test]
fn update_latest_failures_install_nothing() {
    let stub = GetStub::start();
    let next = newer_version();
    let (bytes, build) = patched(0, &next);
    let dir = install_dir_name(&next, Some(&build));
    let h = TestHome::new_private_home();
    let srv = start(&h, true, stub.url());
    let mut c = Client::connect(&h.pipe);
    c.hello();

    stub.release(&next, TRIPLE, &bytes, Some(&"0".repeat(64)));
    let e = c.request(&ClientMsg::DaemonUpdateLatest).unwrap_err();
    assert_prefix(&e, UPDATE_LATEST_BAD_BUILD);
    assert!(!root(&h).join(&dir).exists(), "the created directory goes");
    assert!(part_files(&h).is_empty());

    let asset = GetStub::asset(&next, TRIPLE);
    stub.remove(&asset);
    stub.remove(&format!("{asset}.sha256"));
    let e = c.request(&ClientMsg::DaemonUpdateLatest).unwrap_err();
    assert_prefix(&e, UPDATE_LATEST_NO_BUILD);
    assert!(!root(&h).join(&dir).exists());
    assert_eq!(selected(&h), None);
    assert!(!srv.stage_active());
    srv.shutdown();

    let h = TestHome::new_private_home();
    let srv = start(&h, true, closed_get_url());
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let e = c.request(&ClientMsg::DaemonUpdateLatest).unwrap_err();
    assert_prefix(&e, UPDATE_LATEST_UNREACHABLE);
    assert!(!srv.stage_active());
    srv.shutdown();
}

/// D4: the owning Desktop starts the selected build after a Ring update; when that build
/// cannot start (here it exits at once, `XSHELLD_TEST_SERVE_FAIL_BUILD`) the Desktop starts
/// its own build instead, so the machine is back on a working Daemon (of the old version)
/// with nobody at it. A `selected` record that cannot be used at all does the same.
#[test]
fn selected_build_that_cannot_start_falls_back_to_the_apps_own() {
    use xshell_hostlink::{CancelToken, Dialer, LocalDaemon, LocalDaemonConfig, LocalDialer};
    let h = TestHome::new_private_home();
    let (bytes, build) = patched(1, VERSION);
    let dir = install_dir_name(VERSION, Some(&build));
    xshelld::stage::stage(
        &h.home(),
        &dir,
        &sha(&bytes),
        bytes.len() as u64,
        &mut base64::engine::general_purpose::STANDARD
            .encode(&bytes)
            .as_bytes(),
    )
    .unwrap();
    xshell_core::server::write_windows_selected(&h.home(), &dir).unwrap();
    let staged = root(&h).join(&dir).join("xshelld.exe");
    assert_eq!(
        xshell_core::server::windows_spawn_target(&h.home(), Path::new(bin())).unwrap(),
        staged,
        "the selected build is the one the Desktop would start"
    );
    let own = scan_stamp(&fs::read(bin()).unwrap()).unwrap().unwrap();
    let mut env = h.daemon_env();
    env.push((
        "XSHELLD_TEST_SERVE_FAIL_BUILD".into(),
        build.display().into(),
    ));
    let start = |env: Vec<(std::ffi::OsString, std::ffi::OsString)>| {
        std::sync::Arc::new(
            LocalDaemon::new(LocalDaemonConfig {
                bin: bin().into(),
                env,
                home: h.home(),
                socket: h.pipe.clone(),
                version: VERSION.into(),
                build: None,
            })
            .unwrap(),
        )
    };
    let daemon = start(env);
    let cancel = CancelToken::new();
    let dialed = LocalDialer {
        daemon: daemon.clone(),
    }
    .dial(&cancel)
    .unwrap_or_else(|e| panic!("{e:?}"));
    let _conn = dialed.conn;
    let mut c = Client::from_io(dialed.io.read, dialed.io.write);
    let (hello, _) = c.hello();
    assert!(
        hello.build.as_ref().is_some_and(|b| b.same(&own)),
        "the app's own build serves: {:?}",
        hello.build
    );
    assert!(daemon.start_error().is_none());
    // The selected record is left for the next update to replace.
    assert_eq!(selected(&h).as_deref(), Some(dir.as_str()));
    drop(c);
    daemon.terminate(std::time::Duration::from_secs(15));

    // A record that cannot be used at all: the app's own build too.
    fs::write(root(&h).join("selected"), "../unsafe\n").unwrap();
    assert!(xshell_core::server::windows_spawn_target(&h.home(), Path::new(bin())).is_err());
    let daemon = start(h.daemon_env());
    let dialed = LocalDialer {
        daemon: daemon.clone(),
    }
    .dial(&cancel)
    .unwrap_or_else(|e| panic!("{e:?}"));
    let _conn = dialed.conn;
    let mut c = Client::from_io(dialed.io.read, dialed.io.write);
    let (hello, _) = c.hello();
    assert!(hello.build.as_ref().is_some_and(|b| b.same(&own)));
    drop(c);
    daemon.terminate(std::time::Duration::from_secs(15));
}

/// A home whose `selected` record names another build of this version (this test's
/// `xshelld.exe`, restamped), and the build the Desktop then dials, started by a
/// [`xshell_hostlink::LocalDaemon`] whose Daemons fail as `mode` says
/// (`XSHELLD_TEST_SERVE_FAIL_MODE`; `None`: they do not fail). `seam` adjusts the
/// `LocalDaemon` before it dials (given the staged file). Returns the build that answered,
/// the selected build, and how long the dial took.
fn dial_after_selected(
    mode: Option<&str>,
    seam: impl FnOnce(&xshell_hostlink::LocalDaemon, &Path),
) -> (Option<BuildId>, BuildId, std::time::Duration) {
    use xshell_hostlink::{CancelToken, Dialer, LocalDaemon, LocalDaemonConfig, LocalDialer};
    let h = TestHome::new_private_home();
    let (bytes, build) = patched(1, VERSION);
    let dir = install_dir_name(VERSION, Some(&build));
    xshelld::stage::stage(
        &h.home(),
        &dir,
        &sha(&bytes),
        bytes.len() as u64,
        &mut base64::engine::general_purpose::STANDARD
            .encode(&bytes)
            .as_bytes(),
    )
    .unwrap();
    xshell_core::server::write_windows_selected(&h.home(), &dir).unwrap();
    let staged = root(&h).join(&dir).join("xshelld.exe");
    let mut env = h.daemon_env();
    if let Some(mode) = mode {
        env.push((
            "XSHELLD_TEST_SERVE_FAIL_BUILD".into(),
            build.display().into(),
        ));
        env.push(("XSHELLD_TEST_SERVE_FAIL_MODE".into(), mode.into()));
    }
    let daemon = std::sync::Arc::new(
        LocalDaemon::new(LocalDaemonConfig {
            bin: bin().into(),
            env,
            home: h.home(),
            socket: h.pipe.clone(),
            version: VERSION.into(),
            build: None,
        })
        .unwrap(),
    );
    seam(&daemon, &staged);
    let began = std::time::Instant::now();
    let dialed = LocalDialer {
        daemon: daemon.clone(),
    }
    .dial(&CancelToken::new())
    .unwrap_or_else(|e| panic!("{mode:?}: {e:?}"));
    let took = began.elapsed();
    let _conn = dialed.conn;
    let mut c = Client::from_io(dialed.io.read, dialed.io.write);
    let (hello, _) = c.hello();
    assert!(daemon.start_error().is_none(), "{mode:?}");
    drop(c);
    daemon.terminate(std::time::Duration::from_secs(15));
    (hello.build, build, took)
}

fn own_build() -> BuildId {
    scan_stamp(&fs::read(bin()).unwrap()).unwrap().unwrap()
}

/// A healthy selected build passes the readiness check and is the one that serves.
#[test]
fn healthy_selected_build_serves() {
    let (served, selected, _) = dial_after_selected(None, |_, _| {});
    assert!(served.is_some_and(|b| b.same(&selected)));
}

/// D4 (Sol P2-1): a selected build that binds its pipe and then exits before it answers
/// falls back to the app's own build.
#[test]
fn selected_build_that_binds_then_exits_falls_back() {
    let (served, _, _) = dial_after_selected(Some("exit-after-bind"), |_, _| {});
    assert!(served.is_some_and(|b| b.same(&own_build())));
}

/// D4 (Sol P2-1): a selected build that binds its pipe and then hangs falls back once the
/// readiness wait is over.
#[test]
fn selected_build_that_binds_then_stalls_falls_back() {
    let (served, _, took) = dial_after_selected(Some("stall-after-bind"), |d, _| {
        d.set_selected_ready_wait_for_tests(std::time::Duration::from_secs(3))
    });
    assert!(served.is_some_and(|b| b.same(&own_build())));
    assert!(took >= std::time::Duration::from_secs(3), "{took:?}");
}

/// D4 (Sol P2-2): a selected build whose process cannot be started at all (refused at the
/// point the Desktop starts it) falls back to the app's own build.
#[test]
fn selected_build_whose_start_is_refused_falls_back() {
    let (served, _, _) = dial_after_selected(None, |d, staged| {
        d.refuse_spawn_for_tests(Some(staged.to_path_buf()))
    });
    assert!(served.is_some_and(|b| b.same(&own_build())));
}

/// D4: a selected build that never listens falls back after the 15 s start wait.
#[test]
fn selected_build_that_never_listens_falls_back_after_the_spawn_wait() {
    let (served, _, took) = dial_after_selected(Some("stall"), |_, _| {});
    assert!(served.is_some_and(|b| b.same(&own_build())));
    assert!(took >= xshell_hostlink::local::SPAWN_WAIT, "{took:?}");
}

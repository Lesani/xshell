#![cfg(target_os = "linux")]
//! Updates over the link (S4; ADR-0019): staging a build (`daemon.stage-*`), the in-place
//! re-exec of `daemon.upgrade-selected` with its trial, and the rollbacks (amendments
//! C1–C8). Real `xshelld` processes where a re-exec is involved; the "other build" is this
//! test binary with another commit stamped into it, so it is a different build that really
//! runs.
mod common;
use base64::Engine;
use common::get_stub::{GetStub, Route};
use common::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_protocol::build::{install_dir_name, BuildId};
use xshell_protocol::msg::ClientMsg;
use xshelld::server::{Role, ServerHandle};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const WAIT: Duration = Duration::from_secs(30);

fn own_build() -> BuildId {
    xshell_protocol::build::parse_stamp(env!("XSHELL_BUILDINFO_STAMP"))
        .unwrap()
        .expect("tests run from a git checkout: the build is stamped")
}

fn own_dir() -> String {
    install_dir_name(VERSION, Some(&own_build()))
}

/// This test's `xshelld` without debug information (a debug build is larger than the 64 MiB
/// a stage takes; a release build is about 20 MiB). One file per build of the test binary,
/// shared by test processes and reused across runs.
fn stripped() -> &'static Path {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let m = fs::metadata(bin()).unwrap();
        let mtime = m
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let out = PathBuf::from(format!("/tmp/xd-stripped-{}-{mtime}", m.len()));
        if out.is_file() {
            return out;
        }
        let tmp = PathBuf::from(format!("{}.{}.tmp", out.display(), std::process::id()));
        let st = Command::new("strip")
            .arg("-o")
            .arg(&tmp)
            .arg(bin())
            .status()
            .expect("strip (binutils) is required");
        assert!(st.success());
        fs::rename(&tmp, &out).unwrap();
        out
    })
}

/// This test's `xshelld` with `commit` stamped in instead of its own: another build of the
/// same version that runs exactly like this one.
fn patched_bytes(commit: &str) -> (Vec<u8>, BuildId) {
    let mut bytes = fs::read(stripped()).unwrap();
    let own = own_build();
    assert_ne!(own.commit, commit);
    let marker = b"@@XSHELL-BUILD-V1:";
    let mut n = 0;
    let mut i = 0;
    while let Some(at) = bytes[i..].windows(marker.len()).position(|w| w == marker) {
        let start = i + at + marker.len();
        assert_eq!(&bytes[start..start + 12], own.commit.as_bytes());
        bytes[start..start + 12].copy_from_slice(commit.as_bytes());
        n += 1;
        i = start + 12;
    }
    assert!(n > 0, "no build stamp in {}", stripped().display());
    let build = BuildId {
        commit: commit.into(),
        ..own
    };
    assert_eq!(
        xshell_protocol::build::scan_stamp(&bytes),
        Ok(Some(build.clone()))
    );
    (bytes, build)
}

/// A shell script that carries `build`'s stamp and answers `--version` as that build;
/// anything else runs `body`.
fn stamped_script(build: &BuildId, version_ok: bool, body: &str) -> Vec<u8> {
    let v = json!({"name":"xshelld","version":VERSION,"protocol":{"min":1,"max":1},"os":"Linux","arch":"x86_64","build":build});
    let answer = if version_ok {
        format!("echo '{v}'; exit 0")
    } else {
        "exit 1".to_string()
    };
    format!(
        "#!/bin/sh\n# @@XSHELL-BUILD-V1:{}:{}@@\ncase \"$1\" in --version) {answer};; esac\n{body}\n",
        build.display(),
        build.time
    )
    .into_bytes()
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn root(h: &TestHome) -> PathBuf {
    xshell_core::server::server_root(&h.home())
}

fn selected(h: &TestHome) -> Option<String> {
    fs::read_to_string(root(h).join("selected"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn part_files(h: &TestHome) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in fs::read_dir(root(h)).into_iter().flatten().flatten() {
        for f in fs::read_dir(d.path()).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().ends_with(".part") {
                out.push(f.path());
            }
        }
    }
    out
}

fn begin(c: &mut Client, build: &BuildId, bytes: &[u8]) -> Result<Value, String> {
    c.request(&ClientMsg::DaemonStageBegin {
        version: VERSION.into(),
        build: build.clone(),
        sha256: sha(bytes),
        length: bytes.len() as u64,
    })
}

fn chunk(c: &mut Client, stage: Uuid, offset: u64, data: &[u8]) -> Result<Value, String> {
    c.request(&ClientMsg::DaemonStageChunk {
        stage,
        offset,
        data: b64(data),
    })
}

fn stage_id(v: &Value) -> Uuid {
    v["stage"].as_str().unwrap().parse().unwrap()
}

/// Send `bytes` as build `build` in 256 KiB pieces and commit: the install directory.
fn stage(c: &mut Client, build: &BuildId, bytes: &[u8]) -> Result<String, String> {
    let b = begin(c, build, bytes)?;
    if b["installed"] == json!(true) {
        return Ok(install_dir_name(VERSION, Some(build)));
    }
    let id = stage_id(&b);
    for (i, piece) in bytes.chunks(256 * 1024).enumerate() {
        let r = chunk(c, id, (i * 256 * 1024) as u64, piece)?;
        assert_eq!(r["received"], json!((i * 256 * 1024 + piece.len()) as u64));
    }
    c.request(&ClientMsg::DaemonStageCommit { stage: id })
        .map(|v| v["dir"].as_str().unwrap().to_string())
}

// ---- staging (in-process Daemon) ----

#[test]
fn update_info_reports_target_and_limits() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    let (hello, _) = c.hello(range(1, 1));
    assert!(hello.capabilities.iter().any(|c| c == "daemon.stage"));
    assert!(hello
        .capabilities
        .iter()
        .any(|c| c == "daemon.upgrade-selected"));
    let info = c.request(&ClientMsg::DaemonUpdateInfo).unwrap();
    assert_eq!(
        info["target"],
        json!(xshell_protocol::build::host_triple("Linux", std::env::consts::ARCH).unwrap())
    );
    assert_eq!(info["maxBytes"], json!(64 * 1024 * 1024));
    assert_eq!(info["maxChunk"], json!(1024 * 1024));
    assert!(info.get("lastFailure").is_none());
}

/// Round 6 (D1): a Daemon that cannot be updated over its link (GUI-bound, as every
/// Windows Daemon) still says which target it is, so a Desktop learns the Ring Host's OS;
/// staging stays refused.
#[test]
fn gui_bound_daemon_reports_its_target_but_refuses_staging() {
    let h = TestHome::new();
    let srv = start(&h, |cfg| {
        cfg.gui_bound = Some(xshelld::server::GuiBound {
            parent_pid: std::process::id(),
        });
    });
    let mut c = Client::connect(&srv.socket);
    let (hello, _) = c.hello(range(1, 1));
    assert!(!hello.capabilities.iter().any(|c| c == "daemon.stage"));
    assert!(!hello
        .capabilities
        .iter()
        .any(|c| c == "daemon.update-latest"));
    let refused = c.request(&ClientMsg::DaemonUpdateLatest).unwrap_err();
    assert!(
        refused.contains("cannot be updated over its link"),
        "{refused}"
    );
    let info = c.request(&ClientMsg::DaemonUpdateInfo).unwrap();
    assert_eq!(
        info["target"],
        json!(xshell_protocol::build::host_triple("Linux", std::env::consts::ARCH).unwrap())
    );
    let (_, build) = patched_bytes("0123456789ab");
    let refused = c
        .request(&ClientMsg::DaemonStageBegin {
            version: VERSION.into(),
            build,
            sha256: "0".repeat(64),
            length: 1,
        })
        .unwrap_err();
    assert!(
        refused.contains("cannot be updated over its link"),
        "{refused}"
    );
}

#[test]
fn stage_sequential_chunks_publish_verified_build() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = stage(&mut c, &build, &bytes).unwrap();
    assert_eq!(dir, install_dir_name(VERSION, Some(&build)));
    let file = root(&h).join(&dir).join("xshelld");
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(root(&h).join(&dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(part_files(&h).is_empty());
    assert_eq!(selected(&h), None, "staging never selects");
    assert!(!srv.stage_active());
}

#[test]
fn stage_identical_install_skips_upload() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    stage(&mut c, &build, &bytes).unwrap();
    let again = begin(&mut c, &build, &bytes).unwrap();
    assert_eq!(again, json!({"installed": true}));
    assert!(!srv.stage_active());
}

#[test]
fn stage_wrong_offset_keeps_progress() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    chunk(&mut c, id, 0, &bytes[..1000]).unwrap();
    let e = chunk(&mut c, id, 5, &bytes[5..1000]).unwrap_err();
    assert!(e.contains("wrong offset 5: 1000 bytes received"), "{e}");
    assert!(srv.stage_active(), "the stage is kept");
    let mut off = 1000;
    for piece in bytes[1000..].chunks(512 * 1024) {
        chunk(&mut c, id, off as u64, piece).unwrap();
        off += piece.len();
    }
    let r = c
        .request(&ClientMsg::DaemonStageCommit { stage: id })
        .unwrap();
    assert_eq!(r["dir"], json!(install_dir_name(VERSION, Some(&build))));
}

#[test]
fn stage_corrupted_chunk_fails_commit_and_removes_part() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let mut bad = bytes.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 1;
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    for (i, piece) in bad.chunks(1024 * 1024).enumerate() {
        chunk(&mut c, id, (i * 1024 * 1024) as u64, piece).unwrap();
    }
    assert_eq!(part_files(&h).len(), 1);
    let e = c
        .request(&ClientMsg::DaemonStageCommit { stage: id })
        .unwrap_err();
    assert!(e.contains("SHA-256 mismatch"), "{e}");
    let dir = root(&h).join(install_dir_name(VERSION, Some(&build)));
    assert!(!dir.join("xshelld").exists());
    wait_until("part file removed", || part_files(&h).is_empty());
    // Invalid base64 aborts the stage at once.
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    let e = c
        .request(&ClientMsg::DaemonStageChunk {
            stage: id,
            offset: 0,
            data: "not base64!".into(),
        })
        .unwrap_err();
    assert!(e.contains("invalid update chunk"), "{e}");
    assert!(!srv.stage_active());
    wait_until("part file removed", || part_files(&h).is_empty());
    let e = chunk(&mut c, id, 0, &bytes[..10]).unwrap_err();
    assert!(e.contains("unknown update stage"), "{e}");
}

#[test]
fn stage_wrong_sha_refused() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = stage_id(
        &c.request(&ClientMsg::DaemonStageBegin {
            version: VERSION.into(),
            build: build.clone(),
            sha256: "00".repeat(32),
            length: bytes.len() as u64,
        })
        .unwrap(),
    );
    for (i, piece) in bytes.chunks(1024 * 1024).enumerate() {
        chunk(&mut c, id, (i * 1024 * 1024) as u64, piece).unwrap();
    }
    assert!(c
        .request(&ClientMsg::DaemonStageCommit { stage: id })
        .is_err());
    assert!(!root(&h)
        .join(install_dir_name(VERSION, Some(&build)))
        .join("xshelld")
        .exists());
    for bad in ["zz".repeat(32), "abc".into()] {
        assert!(c
            .request(&ClientMsg::DaemonStageBegin {
                version: VERSION.into(),
                build: build.clone(),
                sha256: bad,
                length: 1,
            })
            .is_err());
    }
}

#[test]
fn stage_limits() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    for length in [0, 64 * 1024 * 1024 + 1] {
        let e = c
            .request(&ClientMsg::DaemonStageBegin {
                version: VERSION.into(),
                build: build.clone(),
                sha256: sha(&bytes),
                length,
            })
            .unwrap_err();
        assert!(e.contains("invalid staging length"), "{length}: {e}");
    }
    // An older version is refused.
    let e = c
        .request(&ClientMsg::DaemonStageBegin {
            version: "0.0.1".into(),
            build: build.clone(),
            sha256: sha(&bytes),
            length: bytes.len() as u64,
        })
        .unwrap_err();
    assert!(e.contains("older"), "{e}");
    // A chunk over 1 MiB aborts.
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    let e = chunk(&mut c, id, 0, &bytes[..1024 * 1024 + 1]).unwrap_err();
    assert!(e.contains("too large") || e.contains("chunk size"), "{e}");
    assert!(!srv.stage_active());
    // C7: past the announced length is refused before anything is written.
    let small = &bytes[..100];
    let id = stage_id(&begin(&mut c, &build, small).unwrap());
    chunk(&mut c, id, 0, &small[..60]).unwrap();
    let e = chunk(&mut c, id, 60, &bytes[60..141]).unwrap_err();
    assert!(e.contains("more bytes than announced"), "{e}");
    assert!(!srv.stage_active());
    wait_until("part file removed", || part_files(&h).is_empty());
}

#[test]
fn stage_stamp_or_dir_mismatch_refused() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, _) = patched_bytes("0123456789ab");
    let other = BuildId {
        commit: "aaaaaaaaaaaa".into(),
        ..own_build()
    };
    let e = stage(&mut c, &other, &bytes).unwrap_err();
    assert!(e.contains("does not match"), "{e}");
    assert!(!root(&h)
        .join(install_dir_name(VERSION, Some(&other)))
        .join("xshelld")
        .exists());
    let script = b"#!/bin/sh\necho no stamp\n".to_vec();
    let e = stage(&mut c, &other, &script).unwrap_err();
    assert!(
        e.contains("stamp") || e.contains("not an xshell build"),
        "{e}"
    );
    wait_until("part files removed", || part_files(&h).is_empty());
}

#[test]
fn stage_binary_that_does_not_run_refused() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let build = BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    };
    let e = stage(&mut c, &build, &stamped_script(&build, false, "exit 1")).unwrap_err();
    assert!(e.contains("--version failed"), "{e}");
    assert!(!root(&h)
        .join(install_dir_name(VERSION, Some(&build)))
        .join("xshelld")
        .exists());
    // One that reports another build is refused too.
    let other = BuildId {
        commit: "aaaaaaaaaaaa".into(),
        ..own_build()
    };
    let mut lying = stamped_script(&build, true, "exit 1");
    let text = String::from_utf8(lying.clone()).unwrap();
    lying = text
        .replacen(
            "\"commit\":\"0123456789ab\"",
            "\"commit\":\"aaaaaaaaaaaa\"",
            1,
        )
        .into_bytes();
    let _ = other;
    let e = stage(&mut c, &build, &lying).unwrap_err();
    assert!(e.contains("another version or build"), "{e}");
    wait_until("part files removed", || part_files(&h).is_empty());
}

#[test]
fn second_connection_stage_is_busy() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let mut b = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let first = stage_id(&begin(&mut a, &build, &bytes).unwrap());
    let e = begin(&mut b, &build, &bytes).unwrap_err();
    assert_eq!(e, xshell_protocol::msg::STAGE_BUSY);
    // b cannot touch a's stage either.
    assert!(chunk(&mut b, first, 0, &bytes[..10]).is_err());
    // The same connection may start over.
    let second = stage_id(&begin(&mut a, &build, &bytes).unwrap());
    assert_ne!(first, second);
    assert!(chunk(&mut a, first, 0, &bytes[..10]).is_err());
    wait_until("one part file", || part_files(&h).len() == 1);
}

#[test]
fn idle_stage_expires() {
    let h = TestHome::new();
    let srv = start(&h, |c| c.stage_idle = Duration::from_millis(300));
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    chunk(&mut c, id, 0, &bytes[..100]).unwrap();
    assert_eq!(part_files(&h).len(), 1);
    wait_until("idle stage removed", || {
        !srv.stage_active() && part_files(&h).is_empty()
    });
    assert!(chunk(&mut c, id, 100, &bytes[100..200]).is_err());
}

#[test]
fn interrupted_staging_removed_on_disconnect() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    chunk(&mut c, id, 0, &bytes[..4096]).unwrap();
    assert_eq!(part_files(&h).len(), 1);
    c.shutdown();
    drop(c);
    wait_until("stage removed with its connection", || {
        !srv.stage_active() && part_files(&h).is_empty()
    });
    // Another Desktop may send now.
    let mut d = Client::in_process_within(&srv, Role::Desktop, WAIT);
    stage(&mut d, &build, &bytes).unwrap();
}

/// C8: the requester is checked again right before the staged build runs; one that left
/// in the meantime publishes nothing.
#[test]
fn stage_requester_gone_before_validation_publishes_nothing() {
    let h = TestHome::new();
    let paused = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let go = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (p2, g2) = (paused.clone(), go.clone());
    let srv = start(&h, move |cfg| {
        cfg.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
            if point == xshelld::server::TestPoint::StageBeforeVerify {
                *p2.0.lock().unwrap() = true;
                p2.1.notify_all();
                let mut g = g2.0.lock().unwrap();
                while !*g {
                    g = g2.1.wait(g).unwrap();
                }
            }
            false
        })));
    });
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    for (i, piece) in bytes.chunks(1024 * 1024).enumerate() {
        chunk(&mut c, id, (i * 1024 * 1024) as u64, piece).unwrap();
    }
    let rid = c.request_id();
    c.send(&ClientMsg::DaemonStageCommit { stage: id }, Some(rid));
    {
        let mut p = paused.0.lock().unwrap();
        while !*p {
            p = paused.1.wait(p).unwrap();
        }
    }
    c.shutdown();
    drop(c);
    wait_until("connection gone", || srv.connections() == 0);
    *go.0.lock().unwrap() = true;
    go.1.notify_all();
    wait_until("part file removed", || part_files(&h).is_empty());
    assert!(!root(&h)
        .join(install_dir_name(VERSION, Some(&build)))
        .join("xshelld")
        .exists());
}

#[test]
fn mobile_cannot_stage_or_select() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut m = Client::in_process_within(&srv, Role::Mobile, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = Uuid::new_v4();
    for msg in [
        ClientMsg::DaemonUpdateInfo,
        ClientMsg::DaemonStageBegin {
            version: VERSION.into(),
            build: build.clone(),
            sha256: sha(&bytes),
            length: bytes.len() as u64,
        },
        ClientMsg::DaemonStageChunk {
            stage: id,
            offset: 0,
            data: b64(&bytes[..10]),
        },
        ClientMsg::DaemonStageCommit { stage: id },
        ClientMsg::DaemonStageAbort { stage: id },
        ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build: build.clone(),
        },
        ClientMsg::DaemonUpgrade,
        ClientMsg::DaemonUpdateLatest,
    ] {
        let e = m.request(&msg).unwrap_err();
        assert!(e.starts_with("forbidden for mobile"), "{msg:?}: {e}");
    }
    assert!(!srv.stage_active());
    assert!(part_files(&h).is_empty());
    assert!(!root(&h).exists() || fs::read_dir(root(&h)).unwrap().next().is_none());
}

/// A Daemon told not to offer `daemon.stage` (an older build, as far as a Desktop can
/// tell) refuses the staging messages; it still says which target it is (round 6, D1).
#[test]
fn daemon_without_capability_refuses_staging() {
    let h = TestHome::new();
    let srv = start(&h, |c| c.hide_capabilities = vec!["daemon.stage".into()]);
    let mut c = Client::connect(&srv.socket);
    let (hello, _) = c.hello(range(1, 1));
    assert!(!hello.capabilities.iter().any(|c| c == "daemon.stage"));
    assert!(!hello
        .capabilities
        .iter()
        .any(|c| c == "daemon.update-latest"));
    let (bytes, build) = patched_bytes("0123456789ab");
    assert!(begin(&mut c, &build, &bytes).is_err());
    assert!(c.request(&ClientMsg::DaemonUpdateLatest).is_err());
    let info = c.request(&ClientMsg::DaemonUpdateInfo).unwrap();
    assert!(info["target"].as_str().is_some_and(|t| !t.is_empty()));
    // Hiding only `daemon.update-latest` refuses it and leaves staging offered. The stub
    // shows the refusal comes before any request to the release server.
    let stub = GetStub::start();
    stub.release(NEXT, triple(), b"x", None);
    let base = stub.url();
    let h = TestHome::new();
    let srv = start(&h, move |c| {
        c.hide_capabilities = vec!["daemon.update-latest".into()];
        c.get_base = base;
    });
    let mut c = Client::connect(&srv.socket);
    let (hello, _) = c.hello(range(1, 1));
    assert!(hello.capabilities.iter().any(|c| c == "daemon.stage"));
    assert!(!hello
        .capabilities
        .iter()
        .any(|c| c == "daemon.update-latest"));
    let e = c.request(&ClientMsg::DaemonUpdateLatest).unwrap_err();
    assert!(e.contains("cannot update itself"), "{e}");
    assert!(stub.hits().is_empty(), "{:?}", stub.hits());
}

// ---- updating from the release server (`daemon.update-latest`, in-process Daemon) ----

/// A version newer than this build, for scripts (their length does not matter).
const NEXT: &str = "99.0.0";

fn triple() -> &'static str {
    xshell_protocol::build::host_triple("Linux", std::env::consts::ARCH).unwrap()
}

/// [`stamped_script`] that answers `--version` as `version` (with `build`).
fn stamped_script_v(build: &BuildId, version: &str, body: &str) -> Vec<u8> {
    let v = json!({"name":"xshelld","version":version,"protocol":{"min":1,"max":1},"os":"Linux","arch":"x86_64","build":build});
    format!(
        "#!/bin/sh\n# @@XSHELL-BUILD-V1:{}:{}@@\ncase \"$1\" in --version) echo '{v}'; exit 0;; esac\n{body}\n",
        build.display(),
        build.time
    )
    .into_bytes()
}

fn release_build() -> BuildId {
    BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    }
}

/// An in-process Daemon that asks `stub`.
fn start_with_stub(h: &TestHome, stub: &GetStub) -> ServerHandle {
    let base = stub.url();
    start(h, move |c| c.get_base = base)
}

fn update_latest(c: &mut Client) -> Result<Value, String> {
    c.request(&ClientMsg::DaemonUpdateLatest)
}

fn assert_prefix(e: &str, prefix: &str) {
    assert!(
        e.starts_with(prefix),
        "{e:?} does not start with {prefix:?}"
    );
}

fn nothing_installed(h: &TestHome, build: &BuildId) {
    let dir = install_dir_name(NEXT, Some(build));
    assert!(!root(h).join(dir).join("xshelld").exists());
    wait_until("part files removed", || part_files(h).is_empty());
    assert_eq!(selected(h), None);
}

#[test]
fn update_latest_installs_newer_release() {
    let stub = GetStub::start();
    let build = release_build();
    let script = stamped_script_v(&build, NEXT, "exit 3");
    stub.release(NEXT, triple(), &script, None);
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (hello, _) = Client::connect(&srv.socket).hello(range(1, 1));
    assert!(hello
        .capabilities
        .iter()
        .any(|c| c == "daemon.update-latest"));
    let r = update_latest(&mut c).unwrap();
    let dir = install_dir_name(NEXT, Some(&build));
    assert_eq!(
        r,
        json!({"current": false, "version": NEXT, "build": build, "dir": dir})
    );
    let file = root(&h).join(&dir).join("xshelld");
    assert_eq!(fs::read(&file).unwrap(), script);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(part_files(&h).is_empty());
    assert_eq!(selected(&h), None, "nothing is selected");
    assert!(!srv.stage_active(), "the slot is free");
    let asset = GetStub::asset(NEXT, triple());
    assert_eq!(
        stub.hits(),
        vec!["/latest".to_string(), format!("{asset}.sha256"), asset]
    );
}

#[test]
fn update_latest_current_when_not_newer() {
    let stub = GetStub::start();
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let own_plus = format!("{VERSION}+0123456789ab");
    for latest in [VERSION, "0.0.1", own_plus.as_str()] {
        stub.body(
            "/latest",
            format!(
                "{latest}
"
            ),
        );
        let r = update_latest(&mut c).unwrap();
        assert_eq!(r, json!({"current": true, "version": VERSION}), "{latest}");
    }
    assert!(
        stub.hits().iter().all(|h| h == "/latest"),
        "nothing is downloaded: {:?}",
        stub.hits()
    );
    assert!(!srv.stage_active());
}

#[test]
fn update_latest_wrong_checksum_installs_nothing() {
    let stub = GetStub::start();
    let build = release_build();
    let script = stamped_script_v(&build, NEXT, "exit 3");
    stub.release(NEXT, triple(), &script, Some(&"00".repeat(32)));
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    nothing_installed(&h, &build);
    assert!(!srv.stage_active(), "the slot is free again");
    // A checksum file without a digest is the same failure.
    stub.body(
        &format!("{}.sha256", GetStub::asset(NEXT, triple())),
        "not a digest
",
    );
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    // The right one then installs.
    stub.release(NEXT, triple(), &script, None);
    update_latest(&mut c).unwrap();
}

#[test]
fn update_latest_version_mismatch_refused() {
    let stub = GetStub::start();
    let build = release_build();
    // Stamped and announced as NEXT, but it reports this build's version.
    stub.release(
        NEXT,
        triple(),
        &stamped_script_v(&build, VERSION, "exit 3"),
        None,
    );
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    assert!(e.contains("another version or build"), "{e}");
    nothing_installed(&h, &build);
    // One that does not run at all is the same failure.
    stub.release(
        NEXT,
        triple(),
        &stamped_script(&build, false, "exit 1"),
        None,
    );
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    nothing_installed(&h, &build);
    assert!(!srv.stage_active());
}

#[test]
fn update_latest_unstamped_refused() {
    let stub = GetStub::start();
    stub.release(NEXT, triple(), b"#!/bin/sh\necho no stamp\n", None);
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    assert!(e.contains("stamp"), "{e}");
    assert!(part_files(&h).is_empty());
    assert!(!root(&h).exists() || fs::read_dir(root(&h)).unwrap().next().is_none());
    assert!(!srv.stage_active());
}

#[test]
fn update_latest_unreachable_and_server_error() {
    use xshell_protocol::msg::UPDATE_LATEST_UNREACHABLE;
    // Nothing listens.
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, UPDATE_LATEST_UNREACHABLE);
    assert!(!srv.stage_active());
    // A server error, no version, or garbage.
    let stub = GetStub::start();
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    stub.set("/latest", Route::Status(500));
    assert_prefix(
        &update_latest(&mut c).unwrap_err(),
        UPDATE_LATEST_UNREACHABLE,
    );
    stub.remove("/latest");
    assert_prefix(
        &update_latest(&mut c).unwrap_err(),
        UPDATE_LATEST_UNREACHABLE,
    );
    stub.body("/latest", "<html>hello</html>");
    assert_prefix(
        &update_latest(&mut c).unwrap_err(),
        UPDATE_LATEST_UNREACHABLE,
    );
    stub.body("/latest", "1".repeat(65));
    assert_prefix(
        &update_latest(&mut c).unwrap_err(),
        UPDATE_LATEST_UNREACHABLE,
    );
    // A server error on the build itself.
    let build = release_build();
    stub.release(
        NEXT,
        triple(),
        &stamped_script_v(&build, NEXT, "exit 3"),
        None,
    );
    stub.set(&GetStub::asset(NEXT, triple()), Route::Status(503));
    assert_prefix(
        &update_latest(&mut c).unwrap_err(),
        UPDATE_LATEST_UNREACHABLE,
    );
    nothing_installed(&h, &build);
    assert!(!srv.stage_active());
}

#[test]
fn update_latest_missing_asset_is_no_build() {
    use xshell_protocol::msg::UPDATE_LATEST_NO_BUILD;
    let stub = GetStub::start();
    let build = release_build();
    let script = stamped_script_v(&build, NEXT, "exit 3");
    stub.release(NEXT, triple(), &script, None);
    let asset = GetStub::asset(NEXT, triple());
    stub.remove(&asset);
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    assert_prefix(&update_latest(&mut c).unwrap_err(), UPDATE_LATEST_NO_BUILD);
    // Without a checksum file too.
    stub.body(&asset, script);
    stub.remove(&format!("{asset}.sha256"));
    assert_prefix(&update_latest(&mut c).unwrap_err(), UPDATE_LATEST_NO_BUILD);
    nothing_installed(&h, &build);
    assert!(!srv.stage_active());
}

#[test]
fn update_latest_oversize_refused() {
    let stub = GetStub::start();
    let build = release_build();
    let mut big = stamped_script_v(&build, NEXT, "exit 3");
    big.resize(64 * 1024 * 1024 + 1, b'#');
    stub.release(NEXT, triple(), &big, None);
    drop(big);
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    assert!(e.contains("64 MiB"), "{e}");
    nothing_installed(&h, &build);
    assert!(!srv.stage_active());
}

#[test]
fn update_latest_redirects_bounded() {
    let stub = GetStub::start();
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    stub.body(
        "/v",
        format!(
            "{VERSION}
"
        ),
    );
    // `/latest` → /r1 → … → /r<n-1> → /v: n redirects.
    let chain = |n: usize| {
        for i in 0..n {
            let from = if i == 0 {
                "/latest".to_string()
            } else {
                format!("/r{i}")
            };
            let to = if i + 1 == n {
                "/v".to_string()
            } else {
                format!("/r{}", i + 1)
            };
            stub.set(&from, Route::Redirect(to));
        }
    };
    chain(5);
    assert_eq!(
        update_latest(&mut c).unwrap(),
        json!({"current": true, "version": VERSION})
    );
    chain(6);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_UNREACHABLE);
    assert!(e.contains("redirect"), "{e}");
}

/// One update at a time: a fetch holds the staging slot, so a stage begin or a second fetch
/// (from another Desktop) is busy while it runs, and a fetch is busy while a stage is open
/// (an update over the link in progress).
#[test]
fn update_latest_and_stage_are_exclusive() {
    use xshell_protocol::msg::STAGE_BUSY;
    let stub = GetStub::start();
    let build = release_build();
    stub.release(
        NEXT,
        triple(),
        &stamped_script_v(&build, NEXT, "exit 3"),
        None,
    );
    let asset = GetStub::asset(NEXT, triple());
    stub.gate(&asset);
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut a = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let mut b = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let rid = a.request_id();
    a.send(&ClientMsg::DaemonUpdateLatest, Some(rid));
    stub.wait_hits(&asset, 1, WAIT);
    let (bytes, other) = patched_bytes("aaaaaaaaaaaa");
    assert_eq!(begin(&mut b, &other, &bytes).unwrap_err(), STAGE_BUSY);
    assert_eq!(update_latest(&mut b).unwrap_err(), STAGE_BUSY);
    // The requester's own second ask too.
    assert_eq!(update_latest(&mut a).unwrap_err(), STAGE_BUSY);
    assert!(srv.stage_active());
    stub.open(&asset);
    let r = a.wait_res(rid).unwrap();
    assert_eq!(r["current"], json!(false));
    assert!(!srv.stage_active());
    // A stage that is open makes a fetch busy, from any connection.
    let id = stage_id(&begin(&mut b, &other, &bytes).unwrap());
    assert_eq!(update_latest(&mut a).unwrap_err(), STAGE_BUSY);
    assert_eq!(update_latest(&mut b).unwrap_err(), STAGE_BUSY);
    assert_eq!(stub.count(&asset), 1, "no second download");
    c_abort(&mut b, id);
    update_latest(&mut a).unwrap();
}

fn c_abort(c: &mut Client, stage: Uuid) {
    c.request(&ClientMsg::DaemonStageAbort { stage }).unwrap();
}

#[test]
fn update_latest_does_not_block_reader() {
    let stub = GetStub::start();
    stub.release(
        NEXT,
        triple(),
        &stamped_script_v(&release_build(), NEXT, "exit 3"),
        None,
    );
    stub.gate("/latest");
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let rid = c.request_id();
    c.send(&ClientMsg::DaemonUpdateLatest, Some(rid));
    stub.wait_hits("/latest", 1, WAIT);
    let t = Instant::now();
    let info = c.request(&ClientMsg::DaemonUpdateInfo).unwrap();
    assert!(info["target"].is_string());
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "answered in {:?}",
        t.elapsed()
    );
    stub.open("/latest");
    assert_eq!(c.wait_res(rid).unwrap()["current"], json!(false));
}

/// C8 for a fetch: a requester that left before the build is checked publishes nothing.
#[test]
fn update_latest_requester_gone_publishes_nothing() {
    let stub = GetStub::start();
    let build = release_build();
    stub.release(
        NEXT,
        triple(),
        &stamped_script_v(&build, NEXT, "exit 3"),
        None,
    );
    let h = TestHome::new();
    let paused = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let go = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (p2, g2) = (paused.clone(), go.clone());
    let base = stub.url();
    let srv = start(&h, move |cfg| {
        cfg.get_base = base;
        cfg.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
            if point == xshelld::server::TestPoint::StageBeforeVerify {
                *p2.0.lock().unwrap() = true;
                p2.1.notify_all();
                let mut g = g2.0.lock().unwrap();
                while !*g {
                    g = g2.1.wait(g).unwrap();
                }
            }
            false
        })));
    });
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let rid = c.request_id();
    c.send(&ClientMsg::DaemonUpdateLatest, Some(rid));
    {
        let mut p = paused.0.lock().unwrap();
        while !*p {
            p = paused.1.wait(p).unwrap();
        }
    }
    c.shutdown();
    drop(c);
    wait_until("connection gone", || srv.connections() == 0);
    // The disconnect kept the reservation: nobody else may start meanwhile.
    assert!(srv.stage_active());
    *go.0.lock().unwrap() = true;
    go.1.notify_all();
    wait_until("slot freed", || !srv.stage_active());
    nothing_installed(&h, &build);
}

/// Amendment 6: an install directory of the release that holds other bytes is never
/// overwritten and never reported as installed.
#[test]
fn update_latest_existing_other_bytes_refused() {
    let stub = GetStub::start();
    let build = release_build();
    let there = stamped_script_v(&build, NEXT, "exit 1");
    let served = stamped_script_v(&build, NEXT, "exit 2");
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    // Put `there` in place over the link (a build of version NEXT).
    let b = c
        .request(&ClientMsg::DaemonStageBegin {
            version: NEXT.into(),
            build: build.clone(),
            sha256: sha(&there),
            length: there.len() as u64,
        })
        .unwrap();
    let id = stage_id(&b);
    chunk(&mut c, id, 0, &there).unwrap();
    let dir = c
        .request(&ClientMsg::DaemonStageCommit { stage: id })
        .unwrap()["dir"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(dir, install_dir_name(NEXT, Some(&build)));
    stub.release(NEXT, triple(), &served, None);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    assert_eq!(
        fs::read(root(&h).join(&dir).join("xshelld")).unwrap(),
        there
    );
    // Also when it is the selected build.
    fs::write(
        root(&h).join("selected"),
        format!(
            "{dir}
"
        ),
    )
    .unwrap();
    fs::set_permissions(root(&h).join("selected"), fs::Permissions::from_mode(0o600)).unwrap();
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    assert_eq!(
        fs::read(root(&h).join(&dir).join("xshelld")).unwrap(),
        there
    );
    assert!(part_files(&h).is_empty());
    assert!(!srv.stage_active());
}

/// Amendment 6: the same bytes in place are reused without a second write, and only after
/// they still report the release's identity.
#[test]
fn update_latest_existing_same_bytes_reused() {
    use std::os::unix::fs::MetadataExt;
    let stub = GetStub::start();
    let build = release_build();
    let script = stamped_script_v(&build, NEXT, "exit 3");
    stub.release(NEXT, triple(), &script, None);
    let h = TestHome::new();
    let srv = start_with_stub(&h, &stub);
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let first = update_latest(&mut c).unwrap();
    let file = root(&h)
        .join(install_dir_name(NEXT, Some(&build)))
        .join("xshelld");
    let before = fs::metadata(&file).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(update_latest(&mut c).unwrap(), first);
    let after = fs::metadata(&file).unwrap();
    assert_eq!(
        (before.ino(), before.mtime_nsec(), before.mtime()),
        (after.ino(), after.mtime_nsec(), after.mtime()),
        "no second write"
    );
    // Bytes in place that match the checksum but do not report the release are refused:
    // the reuse is verified, not assumed.
    let lying = stamped_script_v(&build, VERSION, "exit 3");
    let dir = install_dir_name(NEXT, Some(&build));
    fs::write(&file, &lying).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
    stub.release(NEXT, triple(), &lying, None);
    let e = update_latest(&mut c).unwrap_err();
    assert_prefix(&e, xshell_protocol::msg::UPDATE_LATEST_BAD_BUILD);
    assert!(e.contains("another version or build"), "{e}");
    assert_eq!(fs::read(root(&h).join(dir).join("xshelld")).unwrap(), lying);
    assert!(!srv.stage_active());
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let until = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < until, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- the in-place update (real processes) ----

/// A Daemon process of one home, started from the installed copy of this build, the way the
/// launcher would start it.
struct Machine {
    h: TestHome,
    child: Option<Child>,
    env: Vec<(String, String)>,
}

impl Machine {
    fn new(env: &[(&str, &str)]) -> Machine {
        let h = TestHome::new();
        let mut env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        for (k, v) in [
            ("XSHELLD_TEST_TRIAL_SETTLE_MS", "800"),
            ("XSHELLD_TEST_TRIAL_READY_MS", "20000"),
            ("XSHELLD_TEST_TRIAL_RING_MS", "3000"),
            ("XSHELLD_TEST_TRIAL_TERM_MS", "1000"),
        ] {
            if !env.iter().any(|(e, _)| e == k) {
                env.push((k.into(), v.into()));
            }
        }
        xshell_hostlink::install::install_local(stripped(), &h.home(), &own_dir()).unwrap();
        Machine {
            h,
            child: None,
            env,
        }
    }

    fn installed(&self, dir: &str) -> PathBuf {
        root(&self.h).join(dir).join("xshelld")
    }

    fn command(&self, exe: &Path) -> Command {
        let mut c = Command::new(exe);
        c.env("HOME", self.h.home())
            .env("XDG_RUNTIME_DIR", self.h.run())
            .env("XSHELLD_LOGIN_ENV", "0")
            .env("XSHELL_GET_URL", closed_get_url())
            .env_remove("XSHELLD_IDLE_TIMEOUT_MS")
            .env_remove("XSHELLD_HOME")
            .env_remove("XSHELLD_SOCKET")
            .env_remove("XSHELL_SERVICE_GUARD")
            .env("XSHELL_SERVICE_MECHANISM", "none")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    fn serve(&mut self) -> i32 {
        let mut c = self.command(&self.installed(&own_dir()));
        c.arg("serve").arg("--interactive-env");
        let child = c.spawn().unwrap();
        let pid = child.id() as i32;
        self.child = Some(child);
        pid
    }

    fn socket(&self) -> PathBuf {
        let paths = self.h.paths();
        xshell_hostlink::local::local_live_socket(&self.h.home(), &paths.socket)
    }

    /// A client of whatever build serves now, once it says `want` (an install directory).
    fn client_of(
        &self,
        want: &str,
    ) -> (
        Client,
        xshell_protocol::msg::Hello,
        Vec<xshell_protocol::msg::TerminalInfo>,
    ) {
        let until = Instant::now() + WAIT;
        loop {
            if let Ok(s) = std::os::unix::net::UnixStream::connect(self.socket()) {
                let r = s.try_clone().unwrap();
                let mut c = Client::from_io(r, s);
                // A Daemon on its way out (an update's re-exec) still accepts and says hello,
                // then refuses with `shutting_down` and closes: retry, as the Desktop's
                // supervisor does, until the successor serves.
                if let Some((hello, list)) = try_hello(&mut c) {
                    if install_dir_name(&hello.version, hello.build.as_ref()) == want {
                        return (c, hello, list);
                    }
                }
            }
            assert!(
                Instant::now() < until,
                "{want} never served:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pidfile(&self) -> Option<i32> {
        fs::read_to_string(self.h.paths().pid)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn exe_of(pid: i32) -> PathBuf {
        fs::read_link(format!("/proc/{pid}/exe")).unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.h.paths().log).unwrap_or_default()
    }

    fn trial_open(&self) -> bool {
        root(&self.h).join("trial").exists()
    }

    fn wait_trial_closed(&self) {
        let until = Instant::now() + WAIT;
        while self.trial_open() || xshell_hostlink::trial::held(&self.h.home()) {
            assert!(Instant::now() < until, "trial still open:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn failure(&self) -> Option<xshell_protocol::msg::UpdateFailure> {
        xshell_hostlink::trial::failure(&self.h.home(), xshell_hostlink::trial::now_ms())
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // The Daemon of this home (whichever process holds it now), then our child.
        for _ in 0..3 {
            if let Some(pid) = self.pidfile() {
                if alive(pid) {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    if !wait_dead(pid, Duration::from_secs(5)) {
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Open a shell Terminal that prints a marker; returns its UUID and pid.
fn open_shell(c: &mut Client, m: &Machine) -> (Uuid, u32) {
    let t = Uuid::new_v4();
    let r = c.open(t, sh_spec(&m.h.project("p")));
    (t, r["pid"].as_u64().unwrap() as u32)
}

/// The update an older Desktop's Daemon gets: staged over its link, then selected.
#[test]
fn local_socket_update_reexecs_same_pid_and_restores_terminals() {
    let mut m = Machine::new(&[]);
    let pid = m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, old_terminal_pid) = open_shell(&mut c, &m);
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = stage(&mut c, &build, &bytes).unwrap();
    assert_eq!(
        c.request(&ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build: build.clone(),
        }),
        Ok(json!(null))
    );
    let (mut c, hello, list) = m.client_of(&dir);
    assert_eq!(hello.build.as_ref(), Some(&build));
    // Same process, now running the new build from its install directory.
    assert!(alive(pid));
    assert_eq!(m.pidfile(), Some(pid));
    assert_eq!(Machine::exe_of(pid), m.installed(&dir));
    // The Terminal came back under its UUID, in a new process.
    let restored = list.iter().find(|i| i.terminal == t).expect("restored");
    assert_ne!(restored.pid, Some(old_terminal_pid));
    c.attach(t);
    c.marker(t, "after-update");
    m.wait_trial_closed();
    assert_eq!(selected(&m.h).as_deref(), Some(dir.as_str()));
    assert!(m.failure().is_none());
    assert!(m.log().contains("accepted"), "{}", m.log());
    // Still the same process after the trial.
    assert!(alive(pid));
    assert_eq!(Machine::exe_of(pid), m.installed(&dir));
}

/// A plain `daemon.upgrade` (the Local Host's and an SSH Desktop's path) re-executes into
/// what `selected` names, with the same trial.
#[test]
fn local_socket_upgrade_reexecs_into_selected() {
    let mut m = Machine::new(&[]);
    let pid = m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, _) = open_shell(&mut c, &m);
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = install_dir_name(VERSION, Some(&build));
    xshell_hostlink::install::install_local_bytes(&bytes, &m.h.home(), &dir).unwrap();
    xshell_core::server::write_selected(&m.h.home(), &dir).unwrap();
    assert_eq!(c.request(&ClientMsg::DaemonUpgrade), Ok(json!(null)));
    let (_c, _, list) = m.client_of(&dir);
    assert_eq!(Machine::exe_of(pid), m.installed(&dir));
    assert!(list.iter().any(|i| i.terminal == t));
    m.wait_trial_closed();
    assert!(m.failure().is_none());
}

/// The new build passes `--version` but cannot serve: the watchdog brings back the previous
/// build with the Terminals, `selected` names it again, and the failure is reported.
#[test]
fn reexec_failure_falls_back_and_reports() {
    let mut m = Machine::new(&[]);
    let pid = m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, _) = open_shell(&mut c, &m);
    let build = BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    };
    let dir = stage(&mut c, &build, &stamped_script(&build, true, "exit 1")).unwrap();
    assert_eq!(
        c.request(&ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build: build.clone(),
        }),
        Ok(json!(null))
    );
    // Not before the trial ended: the old process still answers until it re-executes.
    m.wait_trial_closed();
    let (mut c, _, list) = m.client_of(&own_dir());
    assert!(list.iter().any(|i| i.terminal == t), "{list:?}");
    assert_eq!(selected(&m.h).as_deref(), Some(own_dir().as_str()));
    let f = m.failure().expect("failure recorded");
    assert_eq!(f.target, dir);
    assert_eq!(f.previous.as_deref(), Some(own_dir().as_str()));
    assert!(f.reason.contains("stopped while starting"), "{}", f.reason);
    // The script exited: our child is gone; the watchdog started the previous build.
    let st = m.child.as_mut().unwrap().wait().unwrap();
    assert!(!st.success());
    let now = m.pidfile().unwrap();
    assert_ne!(now, pid);
    assert_eq!(Machine::exe_of(now), m.installed(&own_dir()));
    let info = c.request(&ClientMsg::DaemonUpdateInfo);
    let info = info.unwrap_or_else(|e| panic!("{e}\n{}", m.log()));
    assert_eq!(info["lastFailure"]["target"], json!(dir));
}

/// C1: the new build fails to restore one Terminal: rolled back, and every Terminal is
/// there under the old build.
#[test]
fn successor_failing_a_restore_rolls_back_with_all_terminals() {
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = install_dir_name(VERSION, Some(&build));
    let mut m = Machine::new(&[("XSHELLD_TEST_FAIL_RESTORE", &dir)]);
    m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t1, _) = open_shell(&mut c, &m);
    let (t2, _) = open_shell(&mut c, &m);
    stage(&mut c, &build, &bytes).unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    m.wait_trial_closed();
    let f = m.failure().expect("rolled back");
    assert!(f.reason.contains("did not start again"), "{}", f.reason);
    let (_c, _, list) = m.client_of(&own_dir());
    for t in [t1, t2] {
        assert!(
            list.iter().any(|i| i.terminal == t),
            "{t} missing: {list:?}"
        );
    }
    assert_eq!(selected(&m.h).as_deref(), Some(own_dir().as_str()));
}

/// C2: a build that starts but never serves is not accepted.
#[test]
fn successor_that_never_serves_rolls_back() {
    let mut m = Machine::new(&[("XSHELLD_TEST_TRIAL_READY_MS", "1500")]);
    m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, _) = open_shell(&mut c, &m);
    let build = BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    };
    stage(
        &mut c,
        &build,
        &stamped_script(&build, true, "exec sleep 600"),
    )
    .unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    m.wait_trial_closed();
    let f = m.failure().expect("rolled back");
    assert!(f.reason.contains("did not start in time"), "{}", f.reason);
    let (_c, _, list) = m.client_of(&own_dir());
    assert!(list.iter().any(|i| i.terminal == t));
}

/// C2: a build that comes up and crashes before the settle time is over is not accepted.
#[test]
fn successor_crashing_after_start_rolls_back() {
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = install_dir_name(VERSION, Some(&build));
    let crash = format!("{dir}:300");
    let mut m = Machine::new(&[
        ("XSHELLD_TEST_TRIAL_CRASH", &crash),
        ("XSHELLD_TEST_TRIAL_SETTLE_MS", "4000"),
    ]);
    m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, _) = open_shell(&mut c, &m);
    stage(&mut c, &build, &bytes).unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    // It does come up first.
    drop(m.client_of(&dir));
    m.wait_trial_closed();
    let f = m.failure().expect("rolled back");
    assert!(
        f.reason.contains("stopped after it started"),
        "{}",
        f.reason
    );
    let (_c, _, list) = m.client_of(&own_dir());
    assert!(list.iter().any(|i| i.terminal == t));
}

/// C3: a build that ignores SIGTERM and keeps the serve lock is killed through the pidfd;
/// the previous build starts only once the lock is free.
#[test]
fn successor_ignoring_sigterm_is_killed_and_rolled_back() {
    if Command::new("python3")
        .arg("-c")
        .arg("1")
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        panic!("python3 is required for this test");
    }
    let mut m = Machine::new(&[("XSHELLD_TEST_TRIAL_READY_MS", "1500")]);
    m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, _) = open_shell(&mut c, &m);
    let lock = m.h.paths().lock;
    let build = BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    };
    let body = format!(
        "exec python3 -c 'import fcntl,signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); f=open(\"{}\",\"a\"); fcntl.flock(f, fcntl.LOCK_EX); time.sleep(600)'",
        lock.display()
    );
    stage(&mut c, &build, &stamped_script(&build, true, &body)).unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    m.wait_trial_closed();
    let st = m.child.as_mut().unwrap().wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(st.signal(), Some(libc::SIGKILL), "{st:?}");
    assert!(m.log().contains("ignored SIGTERM"), "{}", m.log());
    let (_c, _, list) = m.client_of(&own_dir());
    assert!(list.iter().any(|i| i.terminal == t));
}

/// C4: a service manager restarting the launcher during a trial (the new build exits at
/// once) starts no second Daemon: the launcher takes the previous build, waits while the
/// watchdog rolls back, and then finds that Daemon running.
#[test]
fn reexec_failure_under_restart_loop_rolls_back() {
    let m = Machine::new(&[]);
    let out = m
        .command(&m.installed(&own_dir()))
        .args(["service", "install", "--mechanism", "none"])
        .status()
        .unwrap();
    assert!(out.success());
    let launcher = m.h.home().join(".xshell/service/xshelld");
    // `while :` stands in for systemd's Restart=always.
    let mut looped = m.command(Path::new("/bin/sh"));
    looped.arg("-c").arg(format!(
        "while :; do sh '{}' service run; sleep 0.2; done",
        launcher.display()
    ));
    let mut supervisor = looped.spawn().unwrap();
    struct Kill(i32);
    impl Drop for Kill {
        fn drop(&mut self) {
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
    let _k = Kill(supervisor.id() as i32);
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, _) = open_shell(&mut c, &m);
    let build = BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    };
    let dir = stage(&mut c, &build, &stamped_script(&build, true, "exit 1")).unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    m.wait_trial_closed();
    assert_eq!(m.failure().map(|f| f.target), Some(dir));
    let (_c, _, list) = m.client_of(&own_dir());
    assert!(list.iter().any(|i| i.terminal == t));
    assert_eq!(selected(&m.h).as_deref(), Some(own_dir().as_str()));
    // Exactly one Daemon: the pidfile's, and it is the previous build.
    let pid = m.pidfile().unwrap();
    assert_eq!(Machine::exe_of(pid), m.installed(&own_dir()));
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(m.pidfile(), Some(pid), "a second Daemon took over");
    let _ = supervisor.kill();
    let _ = supervisor.wait();
}

/// C4/C5: a trial record nobody owns (power loss after `selected` was written): the new
/// build, started by anything, rolls back and executes the previous build in its place.
#[test]
fn unowned_trial_rolls_back_at_next_start() {
    let mut m = Machine::new(&[]);
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = install_dir_name(VERSION, Some(&build));
    xshell_hostlink::install::install_local_bytes(&bytes, &m.h.home(), &dir).unwrap();
    let t = xshell_hostlink::trial::Trial {
        target: dir.clone(),
        previous: Some(own_dir()),
        previous_exe: m.installed(&own_dir()),
        ring: false,
        at_ms: 1,
        pid: 1,
    };
    xshell_hostlink::trial::write(&m.h.home(), &t).unwrap();
    xshell_hostlink::trial::select(&m.h.home(), &dir).unwrap();
    let mut cmd = m.command(&m.installed(&dir));
    cmd.arg("serve").arg("--interactive-env");
    let child = cmd.spawn().unwrap();
    let pid = child.id() as i32;
    m.child = Some(child);
    let _ = m.client_of(&own_dir());
    assert_eq!(Machine::exe_of(pid), m.installed(&own_dir()));
    assert_eq!(selected(&m.h).as_deref(), Some(own_dir().as_str()));
    assert!(!m.trial_open());
    assert_eq!(m.failure().map(|f| f.target), Some(dir));
}

/// `daemon.upgrade-selected` refuses what it must: not installed, an identity that does not
/// match, the running build.
#[test]
fn upgrade_selected_refusals_touch_nothing() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (_, build) = patched_bytes("0123456789ab");
    let e = c
        .request(&ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build: build.clone(),
        })
        .unwrap_err();
    assert!(e.contains("not installed"), "{e}");
    // Installed under its name but reporting another build.
    let other = BuildId {
        commit: "aaaaaaaaaaaa".into(),
        ..own_build()
    };
    let dir = install_dir_name(VERSION, Some(&build));
    xshell_hostlink::install::install_local_bytes(
        &stamped_script(&other, true, "exit 1"),
        &h.home(),
        &dir,
    )
    .unwrap();
    let e = c
        .request(&ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build: build.clone(),
        })
        .unwrap_err();
    assert!(e.contains("identity differs"), "{e}");
    assert_eq!(selected(&h), None);
    assert!(!root(&h).join("trial").exists());
    // Nothing froze: update requests are still answered.
    assert!(c.request(&ClientMsg::DaemonUpdateInfo).is_ok());
}

/// A stop asked for during the trial (SIGTERM, a service stop) is honoured: the update is
/// rolled back so the next start runs the previous build, and nothing is started now.
#[test]
fn stop_during_trial_rolls_back_and_starts_nothing() {
    let (bytes, build) = patched_bytes("0123456789ab");
    let mut m = Machine::new(&[("XSHELLD_TEST_TRIAL_SETTLE_MS", "5000")]);
    let pid = m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let dir = stage(&mut c, &build, &bytes).unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    drop(m.client_of(&dir));
    assert!(m.trial_open());
    unsafe { libc::kill(pid, libc::SIGTERM) };
    assert!(m.child.as_mut().unwrap().wait().unwrap().success());
    m.wait_trial_closed();
    let f = m.failure().expect("rolled back");
    assert!(
        f.reason.contains("stopped during the update"),
        "{}",
        f.reason
    );
    assert_eq!(selected(&m.h).as_deref(), Some(own_dir().as_str()));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!xshell_hostlink::trial::daemon_alive(&m.h.home()));
}

// ---- review findings (S4 diff review P1-1 … P2-10) ----

/// A hook that pauses at `at` until released; `paused()` waits until it got there.
#[derive(Clone)]
struct Pause {
    at: xshelld::server::TestPoint,
    state: Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
}

impl Pause {
    fn new(at: xshelld::server::TestPoint) -> Pause {
        Pause {
            at,
            state: Arc::new((Mutex::new((false, false)), std::sync::Condvar::new())),
        }
    }
    fn hook(&self) -> xshelld::server::TestHook {
        let me = self.clone();
        xshelld::server::TestHook(Arc::new(move |_, point| {
            if point == me.at {
                let mut g = me.state.0.lock().unwrap();
                g.0 = true;
                me.state.1.notify_all();
                while !g.1 {
                    g = me.state.1.wait(g).unwrap();
                }
            }
            false
        }))
    }
    fn paused(&self) {
        let mut g = self.state.0.lock().unwrap();
        while !g.0 {
            g = self.state.1.wait(g).unwrap();
        }
    }
    fn release(&self) {
        self.state.0.lock().unwrap().1 = true;
        self.state.1.notify_all();
    }
}

/// P1-6: the requester leaves while validation runs: nothing is published.
#[test]
fn stage_requester_gone_before_publication_publishes_nothing() {
    let h = TestHome::new();
    let pause = Pause::new(xshelld::server::TestPoint::StageBeforePublish);
    let hook = pause.hook();
    let srv = start(&h, move |cfg| cfg.test_hook = Some(hook));
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    for (i, piece) in bytes.chunks(1024 * 1024).enumerate() {
        chunk(&mut c, id, (i * 1024 * 1024) as u64, piece).unwrap();
    }
    let rid = c.request_id();
    c.send(&ClientMsg::DaemonStageCommit { stage: id }, Some(rid));
    pause.paused();
    c.shutdown();
    drop(c);
    wait_until("connection gone", || srv.connections() == 0);
    pause.release();
    wait_until("stage over", || !srv.stage_active());
    assert!(part_files(&h).is_empty());
    assert!(!root(&h)
        .join(install_dir_name(VERSION, Some(&build)))
        .join("xshelld")
        .exists());
}

/// P1-6: the running build's and the selected build's directories are never staged into.
#[test]
fn stage_never_replaces_running_or_selected_build() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let other = b"not the running build".to_vec();
    assert_eq!(
        begin(&mut c, &own_build(), &other).unwrap(),
        json!({"installed": true})
    );
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = install_dir_name(VERSION, Some(&build));
    xshell_hostlink::install::install_local_bytes(&bytes, &h.home(), &dir).unwrap();
    xshell_core::server::write_selected(&h.home(), &dir).unwrap();
    assert_eq!(
        begin(&mut c, &build, &other).unwrap(),
        json!({"installed": true})
    );
    assert_eq!(
        fs::read(root(&h).join(&dir).join("xshelld")).unwrap(),
        bytes
    );
    assert!(!srv.stage_active());
}

/// P2-8 / C7: a write that fails as on a full disk stops the stage; the part file goes and
/// the running build and `selected` are untouched.
#[test]
fn stage_write_failure_leaves_selection_and_running_build() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (old, old_build) = patched_bytes("aaaaaaaaaaaa");
    let old_dir = install_dir_name(VERSION, Some(&old_build));
    xshell_hostlink::install::install_local_bytes(&old, &h.home(), &old_dir).unwrap();
    xshell_core::server::write_selected(&h.home(), &old_dir).unwrap();
    let (bytes, build) = patched_bytes("0123456789ab");
    let id = stage_id(&begin(&mut c, &build, &bytes).unwrap());
    chunk(&mut c, id, 0, &bytes[..1000]).unwrap();
    xshelld_staging_fail_writes(&h, true);
    let e = chunk(&mut c, id, 1000, &bytes[1000..2000]).unwrap_err();
    xshelld_staging_fail_writes(&h, false);
    assert!(e.contains("No space left"), "{e}");
    wait_until("stage removed", || {
        !srv.stage_active() && part_files(&h).is_empty()
    });
    assert_eq!(selected(&h).as_deref(), Some(old_dir.as_str()));
    assert_eq!(
        fs::read(root(&h).join(&old_dir).join("xshelld")).unwrap(),
        old
    );
    assert!(!root(&h)
        .join(install_dir_name(VERSION, Some(&build)))
        .join("xshelld")
        .exists());
    assert!(
        c.request(&ClientMsg::DaemonUpdateInfo).is_ok(),
        "still serving"
    );
}

fn xshelld_staging_fail_writes(h: &TestHome, on: bool) {
    let mut f = xshelld::server::STAGE_FAIL_WRITES.lock().unwrap();
    f.retain(|p| p != &h.home());
    if on {
        f.push(h.home());
    }
}

/// P2-10: the single stage stays reserved until its commit has fully finished.
#[test]
fn stage_reservation_held_until_commit_finished() {
    let h = TestHome::new();
    let pause = Pause::new(xshelld::server::TestPoint::StageBeforeVerify);
    let hook = pause.hook();
    let srv = start(&h, move |cfg| cfg.test_hook = Some(hook));
    let mut a = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let mut b = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let (other, other_build) = patched_bytes("aaaaaaaaaaaa");
    let id = stage_id(&begin(&mut a, &build, &bytes).unwrap());
    for (i, piece) in bytes.chunks(1024 * 1024).enumerate() {
        chunk(&mut a, id, (i * 1024 * 1024) as u64, piece).unwrap();
    }
    let rid = a.request_id();
    a.send(&ClientMsg::DaemonStageCommit { stage: id }, Some(rid));
    pause.paused();
    assert_eq!(
        begin(&mut b, &other_build, &other).unwrap_err(),
        xshell_protocol::msg::STAGE_BUSY
    );
    // Not even the committing connection starts another one meanwhile.
    assert!(begin(&mut a, &other_build, &other).is_err());
    pause.release();
    assert_eq!(
        a.wait_res(rid).unwrap()["dir"],
        json!(install_dir_name(VERSION, Some(&build)))
    );
    wait_until("released", || !srv.stage_active());
    assert!(begin(&mut b, &other_build, &other).is_ok());
}

/// P1-4: `selected` changed but its directory sync failed: the previous selection is put
/// back durably and the trial record removed; the Daemon keeps serving.
#[test]
fn select_sync_failure_restores_previous_selection() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    stage(&mut c, &build, &bytes).unwrap();
    xshell_hostlink::trial::FAIL_SELECT_SYNC
        .lock()
        .unwrap()
        .push((h.home(), 1));
    let e = c
        .request(&ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build,
        })
        .unwrap_err();
    assert!(e.contains("cannot prepare the update"), "{e}");
    assert_eq!(selected(&h).as_deref(), Some(own_dir().as_str()));
    assert!(!root(&h).join("trial").exists());
    assert!(
        c.request(&ClientMsg::DaemonUpdateInfo).is_ok(),
        "still serving"
    );
}

/// P1-4: when even the restore cannot be confirmed, the trial record stays, and the next
/// start rolls back to the previous build.
#[test]
fn select_sync_failure_keeps_recovery_record() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = stage(&mut c, &build, &bytes).unwrap();
    xshell_hostlink::trial::FAIL_SELECT_SYNC
        .lock()
        .unwrap()
        .push((h.home(), 2));
    assert!(c
        .request(&ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build,
        })
        .is_err());
    // The restore was written but not confirmed durable either: the record stays.
    assert!(root(&h).join("trial").exists(), "recovery record kept");
    assert_ne!(selected(&h), None);
    let _ = dir;
    drop(c);
    srv.shutdown();
    assert_eq!(xshell_hostlink::trial::settle(&h.home()).unwrap(), None);
    assert_eq!(selected(&h).as_deref(), Some(own_dir().as_str()));
    assert!(!root(&h).join("trial").exists());
}

/// P2-9: a trial that cannot be prepared refuses `daemon.upgrade`; the Daemon keeps
/// serving and nothing is frozen.
#[test]
fn plain_upgrade_refused_when_trial_cannot_be_prepared() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process_within(&srv, Role::Desktop, WAIT);
    let (t, _) = (Uuid::new_v4(), ());
    c.open(t, sh_spec(&h.project("p")));
    let (bytes, build) = patched_bytes("0123456789ab");
    let dir = install_dir_name(VERSION, Some(&build));
    xshell_hostlink::install::install_local_bytes(&bytes, &h.home(), &dir).unwrap();
    xshell_core::server::write_selected(&h.home(), &dir).unwrap();
    let held = xshell_hostlink::trial::try_lock(&h.home())
        .unwrap()
        .unwrap();
    let e = c.request(&ClientMsg::DaemonUpgrade).unwrap_err();
    assert!(e.contains("already running"), "{e}");
    drop(held);
    assert!(c.request(&ClientMsg::DaemonUpdateInfo).is_ok());
    assert!(
        srv.wait_timeout(Duration::from_millis(300)).is_none(),
        "still serving"
    );
}

/// P2-9: a plain `daemon.upgrade` that selects no other build restarts this build in
/// place: same process, Terminals restored.
#[test]
fn plain_upgrade_restarts_in_place() {
    let mut m = Machine::new(&[]);
    let pid = m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, old) = open_shell(&mut c, &m);
    assert_eq!(c.request(&ClientMsg::DaemonUpgrade), Ok(json!(null)));
    let until = Instant::now() + WAIT;
    loop {
        let (_c, _, list) = m.client_of(&own_dir());
        if list.iter().any(|i| i.terminal == t && i.pid != Some(old)) {
            break;
        }
        assert!(Instant::now() < until, "not restarted");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(m.child.as_mut().unwrap().try_wait().unwrap().is_none());
    assert_eq!(m.pidfile(), Some(pid));
    assert!(!m.trial_open());
}

/// P1-1: a `serve` started directly while a trial runs waits; it neither starts beside the
/// trial nor touches it, and gives up once the build on trial serves.
#[test]
fn direct_serve_waits_while_a_trial_runs() {
    let (bytes, build) = patched_bytes("0123456789ab");
    let mut m = Machine::new(&[("XSHELLD_TEST_TRIAL_SETTLE_MS", "3000")]);
    m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let dir = stage(&mut c, &build, &bytes).unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    drop(m.client_of(&dir));
    assert!(m.trial_open());
    let mut racer = m.command(&m.installed(&own_dir()));
    racer.arg("serve").arg("--interactive-env");
    let mut racer = racer.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(m.trial_open(), "the trial must still run");
    assert!(
        racer.try_wait().unwrap().is_none(),
        "a serve beside a running trial must wait"
    );
    m.wait_trial_closed();
    let st = wait_child(&mut racer, WAIT).expect("the racer gives up");
    assert_eq!(st.code(), Some(3));
    assert!(m.failure().is_none(), "the trial was not disturbed");
    assert_eq!(selected(&m.h).as_deref(), Some(dir.as_str()));
}

fn wait_child(c: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let until = Instant::now() + within;
    loop {
        if let Some(s) = c.try_wait().unwrap() {
            return Some(s);
        }
        if Instant::now() >= until {
            let _ = c.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// P1-1: the build on trial dies but something it started still holds the serve lock: the
/// watchdog does not roll back (Terminal records untouched) until the lock is free.
#[test]
fn rollback_waits_until_a_dead_successors_lock_is_free() {
    let mut m = Machine::new(&[("XSHELLD_TEST_TRIAL_READY_MS", "1500")]);
    m.serve();
    let (mut c, _, _) = m.client_of(&own_dir());
    let (t, _) = open_shell(&mut c, &m);
    let lock = m.h.paths().lock;
    let build = BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    };
    // The successor forks a child that keeps the serve lock for 4 s after it is gone.
    let body = format!(
        "exec python3 -c 'import fcntl,os,time,signal; f=open(\"{}\",\"a\"); fcntl.flock(f, fcntl.LOCK_EX)\nif os.fork()==0:\n  os.setsid(); signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(4); os._exit(0)\ntime.sleep(600)'",
        lock.display()
    );
    stage(&mut c, &build, &stamped_script(&build, true, &body)).unwrap();
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    // Ready never comes: after 1.5 s the watchdog stops the successor; its child keeps
    // the lock for a while longer.
    let st = wait_child(m.child.as_mut().unwrap(), WAIT).expect("successor stopped");
    assert!(!st.success());
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        m.trial_open(),
        "rolled back while the serve lock was still held"
    );
    assert!(m.failure().is_none());
    m.wait_trial_closed();
    assert!(m.failure().is_some());
    let (_c, _, list) = m.client_of(&own_dir());
    assert!(list.iter().any(|i| i.terminal == t));
}

/// P1-5: during a trial, unreadable or corrupt Terminal records fail the trial; the
/// previous build comes back with every Terminal.
#[test]
fn unreadable_or_corrupt_state_during_trial_rolls_back() {
    for how in ["unreadable", "corrupt"] {
        let (bytes, build) = patched_bytes("0123456789ab");
        let dir = install_dir_name(VERSION, Some(&build));
        let hook = format!("{dir}:{how}");
        let mut m = Machine::new(&[("XSHELLD_TEST_CORRUPT_STATE", &hook)]);
        m.serve();
        let (mut c, _, _) = m.client_of(&own_dir());
        let (t1, _) = open_shell(&mut c, &m);
        let (t2, _) = open_shell(&mut c, &m);
        stage(&mut c, &build, &bytes).unwrap();
        c.request(&ClientMsg::DaemonUpgradeSelected {
            version: VERSION.into(),
            build,
        })
        .unwrap();
        m.wait_trial_closed();
        let f = m
            .failure()
            .unwrap_or_else(|| panic!("{how}: not rolled back"));
        assert!(
            f.reason.contains("did not start again"),
            "{how}: {}",
            f.reason
        );
        let (_c, _, list) = m.client_of(&own_dir());
        for t in [t1, t2] {
            assert!(list.iter().any(|i| i.terminal == t), "{how}: {t} missing");
        }
    }
}

/// P1-2: installing over a running Daemon only installs; an installer stopped before the
/// switch leaves `selected` alone, so the next start serves the old build. An installer
/// run during a trial does not change `selected` either.
#[test]
fn self_install_never_selects_beside_a_running_daemon_or_trial() {
    let mut m = Machine::new(&[("XSHELLD_TEST_TRIAL_SETTLE_MS", "3000")]);
    xshell_hostlink::trial::select(&m.h.home(), &own_dir()).unwrap();
    let pid = m.serve();
    let _ = m.client_of(&own_dir());
    let (bytes, build) = patched_bytes("0123456789ab");
    let new = m.h.root().join("new-xshelld");
    fs::write(&new, &bytes).unwrap();
    fs::set_permissions(&new, fs::Permissions::from_mode(0o700)).unwrap();
    let out = m
        .command(&new)
        .args(["self-install", "--select"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success());
    let dir = install_dir_name(VERSION, Some(&build));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), dir);
    assert!(m.installed(&dir).is_file(), "the bytes are installed");
    assert_eq!(selected(&m.h).as_deref(), Some(own_dir().as_str()));
    // Interrupted here; the machine restarts.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = m.child.as_mut().unwrap().wait();
    m.serve();
    let _ = m.client_of(&own_dir());
    // Now a trial runs; another installer leaves the selection to it.
    let (mut c, _, _) = m.client_of(&own_dir());
    c.request(&ClientMsg::DaemonUpgradeSelected {
        version: VERSION.into(),
        build,
    })
    .unwrap();
    drop(m.client_of(&dir));
    assert!(m.trial_open());
    let (third, _) = patched_bytes("bbbbbbbbbbbb");
    let third_path = m.h.root().join("third-xshelld");
    fs::write(&third_path, &third).unwrap();
    fs::set_permissions(&third_path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(m
        .command(&third_path)
        .args(["self-install", "--select"])
        .status()
        .unwrap()
        .success());
    assert_eq!(selected(&m.h).as_deref(), Some(dir.as_str()));
    m.wait_trial_closed();
    assert_eq!(selected(&m.h).as_deref(), Some(dir.as_str()));
}

/// `hello`, or `None` when the Daemon refuses (`shutting_down`) or closes instead of
/// sending its Terminal list.
fn try_hello(
    c: &mut Client,
) -> Option<(
    xshell_protocol::msg::Hello,
    Vec<xshell_protocol::msg::TerminalInfo>,
)> {
    use xshell_protocol::msg::{Hello, ServerMsg};
    c.send(
        &ClientMsg::Hello(Hello {
            protocol: range(1, 1),
            version: "test".into(),
            capabilities: vec!["call".into(), "term".into()],
            build: None,
        }),
        None,
    );
    let Some(ServerMsg::Hello(h)) = c.try_msg(WAIT, |m| matches!(m, ServerMsg::Hello(_))) else {
        return None;
    };
    match c.try_msg(WAIT, |m| {
        matches!(m, ServerMsg::Terminals { .. } | ServerMsg::Error { .. })
    })? {
        ServerMsg::Terminals { list } => Some((h, list)),
        ServerMsg::Error { code, .. } => {
            assert_eq!(code, "shutting_down", "unexpected refusal");
            None
        }
        _ => None,
    }
}

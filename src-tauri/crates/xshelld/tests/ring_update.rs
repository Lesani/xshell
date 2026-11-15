#![cfg(target_os = "linux")]
//! Updates over the link (S4; ADR-0019): staging a build (`daemon.stage-*`), the in-place
//! re-exec of `daemon.upgrade-selected` with its trial, and the rollbacks (amendments
//! C1–C8). Real `xshelld` processes where a re-exec is involved; the "other build" is this
//! test binary with another commit stamped into it, so it is a different build that really
//! runs.
mod common;
use base64::Engine;
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
use xshelld::server::Role;

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

#[test]
fn stage_sequential_chunks_publish_verified_build() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut a = Client::in_process(&srv, Role::Desktop);
    let mut b = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut d = Client::in_process(&srv, Role::Desktop);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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
    let mut m = Client::in_process(&srv, Role::Mobile);
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
    ] {
        let e = m.request(&msg).unwrap_err();
        assert!(e.starts_with("forbidden for mobile"), "{msg:?}: {e}");
    }
    assert!(!srv.stage_active());
    assert!(part_files(&h).is_empty());
    assert!(!root(&h).exists() || fs::read_dir(root(&h)).unwrap().next().is_none());
}

/// A Daemon told not to offer `daemon.stage` (an older build, as far as a Desktop can
/// tell) refuses the messages.
#[test]
fn daemon_without_capability_refuses_staging() {
    let h = TestHome::new();
    let srv = start(&h, |c| c.hide_capabilities = vec!["daemon.stage".into()]);
    let mut c = Client::connect(&srv.socket);
    let (hello, _) = c.hello(range(1, 1));
    assert!(!hello.capabilities.iter().any(|c| c == "daemon.stage"));
    let (bytes, build) = patched_bytes("0123456789ab");
    assert!(begin(&mut c, &build, &bytes).is_err());
    assert!(c.request(&ClientMsg::DaemonUpdateInfo).is_err());
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
                let (hello, list) = c.hello(range(1, 1));
                if install_dir_name(&hello.version, hello.build.as_ref()) == want {
                    return (c, hello, list);
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
    let mut c = Client::in_process(&srv, Role::Desktop);
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

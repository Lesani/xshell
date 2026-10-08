//! Managed install against a temp "remote home": probe, upload the workspace-built binary,
//! connect to it.
#![cfg(unix)]

mod common;
mod desktop;

use common::{bin, DaemonGuard, TestHome};
use desktop::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use xshell_hostlink::{BinarySource, CancelToken, FileSource};

struct Counting(FileSource, Arc<AtomicUsize>);

impl BinarySource for Counting {
    fn fetch(&self, t: &str, v: &str, c: &CancelToken) -> Result<Vec<u8>, String> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.fetch(t, v, c)
    }
}

struct Ix {
    d: Desk,
    fetches: Arc<AtomicUsize>,
    _guard: DaemonGuard,
    home: TestHome,
}

impl Drop for Ix {
    fn drop(&mut self) {
        self.d.m.shutdown();
    }
}

fn installed(home: &TestHome) -> std::path::PathBuf {
    home.home()
        .join(".xshell/server")
        .join(VERSION)
        .join("xshelld")
}

fn start(seed: impl FnOnce(&TestHome)) -> Ix {
    let home = TestHome::new();
    seed(&home);
    let guard = DaemonGuard::new(&home);
    let fetches = Arc::new(AtomicUsize::new(0));
    let rec = Recorder::new();
    let cfg = manager_config(
        local_factory(&home),
        Arc::new(Counting(FileSource(bin().into()), fetches.clone())),
        rec.clone(),
    );
    let d = Desk::new(cfg, rec, host_config(None));
    Ix {
        d,
        fetches,
        _guard: guard,
        home,
    }
}

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn auto_install_then_connect() {
    let ix = start(|h| assert!(!h.home().join(".xshell").exists()));
    let s = ix.d.wait_usable();
    assert_eq!(s.daemon_version.as_deref(), Some(VERSION));
    assert_eq!(
        s.os.as_deref(),
        Some(if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        })
    );
    let p = installed(&ix.home);
    assert_eq!(mode(&p), 0o700);
    assert_eq!(mode(p.parent().unwrap()), 0o700);
    assert_eq!(ix.fetches.load(Ordering::SeqCst), 1);
    // The phases were published on the way.
    let phases: Vec<_> =
        ix.d.rec
            .statuses
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.phase)
            .collect();
    use xshell_hostlink::Phase;
    assert!(phases.contains(&Some(Phase::Probing)), "{phases:?}");
    assert!(phases.contains(&Some(Phase::Installing)), "{phases:?}");
    let home = call(&ix.d.host(), "get_home_dir", json!({})).unwrap();
    assert_eq!(home, json!(ix.home.home().to_string_lossy()));
}

#[test]
fn reconnect_does_not_reupload() {
    let ix = start(|_| {});
    ix.d.wait_usable();
    let n = ix.d.rec.count();
    let ssh = ix.d.host().child_pid().unwrap() as i32;
    unsafe { libc::kill(-ssh, libc::SIGKILL) };
    let (i, _) = ix.d.rec.wait_status_from(n, "reconnecting", |s| {
        s.status == xshell_hostlink::StatusKind::Reconnecting
    });
    ix.d.rec.wait_status_from(i, "connected again", usable);
    assert_eq!(ix.fetches.load(Ordering::SeqCst), 1);
}

#[test]
fn corrupt_install_replaced() {
    let ix = start(|h| {
        let p = installed(h);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    });
    ix.d.wait_usable();
    assert_eq!(ix.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::metadata(installed(&ix.home)).unwrap().len(),
        std::fs::metadata(bin()).unwrap().len()
    );
}

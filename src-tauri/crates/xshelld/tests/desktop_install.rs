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
use xshell_hostlink::{BinarySource, CancelToken, Fetched, FileSource, Wanted};

struct Counting(FileSource, Arc<AtomicUsize>);

impl BinarySource for Counting {
    fn fetch(&self, w: &Wanted, c: &CancelToken) -> Result<Fetched, String> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.fetch(w, c)
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
    start_with(seed, None)
}

fn start_with(
    seed: impl FnOnce(&TestHome),
    desktop_build: Option<xshell_protocol::build::BuildId>,
) -> Ix {
    let home = TestHome::new();
    seed(&home);
    let guard = DaemonGuard::new(&home);
    let fetches = Arc::new(AtomicUsize::new(0));
    let rec = Recorder::new();
    let mut cfg = manager_config(
        local_factory(&home),
        Arc::new(Counting(FileSource(bin().into()), fetches.clone())),
        rec.clone(),
    );
    cfg.desktop_build = desktop_build;
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

/// With the Desktop's build known (#49), the install goes to `<version>+<build>`, the
/// Daemon reports that build, and nothing is selected.
#[test]
fn keyed_by_build() {
    let Some(ours) = xshelld::build_id::BUILD.clone() else {
        return;
    };
    let ix = start_with(|_| {}, Some(ours.clone()));
    let s = ix.d.wait_usable();
    assert_eq!(s.status, xshell_hostlink::StatusKind::Connected);
    assert_eq!(s.daemon_build.as_ref(), Some(&ours));
    let dir = xshell_protocol::build::install_dir_name(VERSION, Some(&ours));
    let p = ix
        .home
        .home()
        .join(".xshell/server")
        .join(dir)
        .join("xshelld");
    assert_eq!(mode(&p), 0o700);
    assert!(!installed(&ix.home).exists());
    assert!(!ix.home.home().join(".xshell/server/selected").exists());
    assert_eq!(ix.fetches.load(Ordering::SeqCst), 1);
}

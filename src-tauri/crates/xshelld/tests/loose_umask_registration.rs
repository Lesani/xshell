#![cfg(unix)]
//! Local Computer registration on a fresh home under the Ubuntu/Fedora default umask
//! (0002), with the actual registration child. Its own test binary: the umask is
//! process-wide and must not leak into other tests.
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig, RingObserver, RingView};

struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o777
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    ring: Arc<DesktopRing>,
    daemon: xshell_hostlink::LocalDaemon,
}

/// `shared`: how `~/.xshell` exists before the Desktop starts (`None`: fresh home).
fn fixture(shared: Option<u32>) -> Fixture {
    let root = tempfile::Builder::new().prefix("xu").tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    if let Some(shared) = shared {
        std::fs::DirBuilder::new()
            .mode(shared)
            .create(home.join(".xshell"))
            .unwrap();
        std::fs::set_permissions(
            home.join(".xshell"),
            std::fs::Permissions::from_mode(shared),
        )
        .unwrap();
        // The installer's binary directory, made with the user's umask: 0775.
        std::fs::create_dir(home.join(".xshell/bin")).unwrap();
        assert_eq!(mode(&home.join(".xshell/bin")), 0o775);
    }
    // The app data directory as the platform creates it under this umask: 0775.
    let app = home.join(".local/share/com.xshell.app");
    std::fs::create_dir_all(&app).unwrap();
    assert_eq!(mode(&app), 0o775);
    let mut cfg = DesktopRingConfig::new(app.join("ring"), "local computer".into());
    cfg.owner_retry = Duration::from_millis(20);
    let ring = DesktopRing::open(cfg, Arc::new(Quiet));
    let daemon = xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin: env!("CARGO_BIN_EXE_xshelld").into(),
        home: home.clone(),
        socket: home.join(".xshell/run/daemon.sock"),
        env: vec![],
        version: "1.5.0".into(),
        build: xshelld::build_id::BUILD.clone(),
    })
    .unwrap();
    Fixture {
        _root: root,
        home,
        ring,
        daemon,
    }
}

/// The Desktop's startup order: the reservation, then registration retried until the
/// owner election is done. Returns the last registration error on the deadline.
fn start(f: &Fixture) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(10);
    let mut reserved = false;
    loop {
        if !reserved {
            reserved = f.ring.reserve_local_companion(&f.home)?;
        }
        match f.ring.register_local_companion(&f.daemon) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) if Instant::now() >= until => return Err(error),
            Err(_) => {}
        }
        if Instant::now() >= until {
            return Err("owner election deadline".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn loose_umask_registration_succeeds_and_refuses_writable_shared_root_unchanged() {
    unsafe { libc::umask(0o002) };

    // Fresh home: everything xshell creates on the Store/IPC path is 0700.
    let f = fixture(None);
    start(&f).unwrap();
    let ring_dir = f.home.join(".xshell/daemon/ring");
    assert!(ring_dir.join("companion.json").exists());
    for dir in [
        f.home.join(".xshell"),
        f.home.join(".xshell/daemon"),
        ring_dir.clone(),
        f.home.join(".local/share/com.xshell.app/ring"),
    ] {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    f.ring.quit();

    // An installer made `~/.xshell` 0700 and `~/.xshell/bin` 0775 (its umask): the
    // group-writable install directory below the private root is not on the Store/IPC
    // path and is left alone.
    let f = fixture(Some(0o700));
    start(&f).unwrap();
    assert!(f.home.join(".xshell/daemon/ring/companion.json").exists());
    assert_eq!(mode(&f.home.join(".xshell/bin")), 0o775);
    f.ring.quit();

    // Read-only exposure is tightened.
    let f = fixture(Some(0o755));
    start(&f).unwrap();
    assert_eq!(mode(&f.home.join(".xshell")), 0o700);
    assert!(f.home.join(".xshell/daemon/ring/companion.json").exists());
    f.ring.quit();

    // A group-writable `~/.xshell` (an installer created it under umask 0002): other users
    // may have had rename authority, so it is refused unchanged, with an error that names
    // it and the fix.
    let f = fixture(Some(0o775));
    let shared = f.home.join(".xshell");
    for _ in 0..2 {
        let error = start(&f).unwrap_err();
        assert!(
            error.contains(&shared.display().to_string())
                && error.contains("writable by other users"),
            "{error}"
        );
    }
    assert_eq!(mode(&shared), 0o775);
    assert!(!f.home.join(".xshell/daemon").exists());
    f.ring.quit();
}

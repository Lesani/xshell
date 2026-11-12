#![cfg(unix)]
//! Actual registration child + actual CLI. No roster/auth-row seeding or confirmation.
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use xshell_hostlink::ring::administration::{
    AdministrationClient, AdministrationRequest as R, AdministrationStatus as S,
};
use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig, RingObserver, RingView};
use xshell_hostlink::{BinarySource, Manager, ManagerConfig, Observer, TransportFactory};
use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};

struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}
impl Observer for Quiet {
    fn status(&self, _: &xshell_hostlink::HostStatus) {}
    fn terminals(&self, _: &str, _: &[xshell_protocol::msg::TerminalInfo]) {}
}
struct NoNetwork;
impl TransportFactory for NoNetwork {
    fn for_host(&self, _: &xshell_hostlink::HostConfig) -> Box<dyn xshell_hostlink::Transport> {
        panic!("startup must not dial a Host");
    }
}
impl BinarySource for NoNetwork {
    fn fetch(
        &self,
        _: &xshell_hostlink::Wanted,
        _: &xshell_hostlink::CancelToken,
    ) -> Result<xshell_hostlink::Fetched, String> {
        panic!("startup must not install a Daemon");
    }
}
fn administration_deadline() -> Instant {
    Instant::now() + Duration::from_secs(8)
}
fn bin() -> PathBuf {
    env!("CARGO_BIN_EXE_xshelld").into()
}
struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    desktop: PathBuf,
    socket: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new().prefix("xc").tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let desktop = home.join("desktop");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&desktop)
            .unwrap();
        let socket = home.join("run/daemon.sock");
        Self {
            _root: root,
            home,
            desktop,
            socket,
        }
    }
    fn daemon_dir(&self) -> PathBuf {
        self.home.join(".xshell/daemon/ring")
    }
    fn ring(&self) -> Arc<DesktopRing> {
        let mut cfg = DesktopRingConfig::new(self.desktop.clone(), "local computer".into());
        cfg.owner_retry = Duration::from_millis(20);
        DesktopRing::open(cfg, Arc::new(Quiet))
    }
    fn daemon(
        &self,
        binary: PathBuf,
        build: Option<xshell_protocol::build::BuildId>,
    ) -> xshell_hostlink::LocalDaemon {
        xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
            bin: binary,
            home: self.home.clone(),
            socket: self.socket.clone(),
            env: vec![],
            version: "1.5.0".into(),
            build,
        })
        .unwrap()
    }
    fn manager(&self, ring: &DesktopRing) -> Arc<Manager> {
        let manager = Arc::new(Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(NoNetwork),
            Arc::new(NoNetwork),
            Arc::new(Quiet),
        )));
        ring.attach_manager(&manager).unwrap();
        wait(|| AdministrationClient::connect(&self.desktop, administration_deadline()).is_ok());
        manager
    }
    fn pair(&self, ttl: u64) -> (OwnedChild, mpsc::Receiver<String>) {
        self.pair_on_relay(ttl, Some("ws://127.0.0.1:9"))
    }
    fn pair_on_relay(&self, ttl: u64, relay: Option<&str>) -> (OwnedChild, mpsc::Receiver<String>) {
        let mut command = Command::new(bin());
        command.arg("pair");
        if let Some(relay) = relay {
            command.args(["--relay", relay]);
        }
        let mut child = command
            .arg("--home")
            .arg(&self.home)
            .arg("--socket")
            .arg(&self.socket)
            .env("XSHELLD_PAIR_TTL_MS", ttl.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let _ = tx.send(line.unwrap());
            }
        });
        (OwnedChild(child), rx)
    }
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(8);
    while !ready() {
        assert!(Instant::now() < until, "startup deadline");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn endpoint(f: &Fixture) -> xshell_hostlink::ring::companion_ipc::CompanionClient {
    xshell_hostlink::ring::companion_ipc::CompanionClient::new(
        f.daemon_dir().join("companion.sock"),
    )
}
fn assert_no_membership(f: &Fixture) {
    assert!(!f.daemon_dir().join("roster.json").exists());
    let store = xshell_hostlink::ring::store::Store::new(f.desktop.clone());
    assert!(store.load().unwrap().0.is_none());
}
fn expired_pending(f: &Fixture) {
    // Even a future fallback regression must not contact the deployed default Relay.
    // The owned listener also proves that a pending registered CLI makes no Relay dial.
    let relay_sentinel = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    relay_sentinel.set_nonblocking(true).unwrap();
    let relay = format!("ws://{}", relay_sentinel.local_addr().unwrap());
    let (mut child, rx) = f.pair_on_relay(450, Some(&relay));
    let line = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(line.starts_with("Computer pairing pending."), "{line}");
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "must stay pending, not exit success/error immediately"
    );
    wait(|| child.0.try_wait().unwrap().is_some());
    assert_eq!(child.0.wait().unwrap().code(), Some(5));
    assert!(
        matches!(relay_sentinel.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "registered pending CLI must not dial the Relay"
    );
    let rest: Vec<_> = rx.iter().collect();
    assert!(!rest
        .iter()
        .any(|line| line.starts_with("Pairing code:") || line.starts_with("Paired as")));
    assert!(rest
        .iter()
        .any(|line| line == "Computer pairing request expired. No pairing code was generated."));
    assert_no_membership(f);
}
#[test]
fn fresh_unpaired_startup_registers_then_gui_close_actual_cli_stays_computer_pending() {
    let f = Fixture::new();
    let ring = f.ring();
    let daemon = f.daemon(bin(), xshelld::build_id::BUILD.clone());
    let profile = ring.register_local_companion(&daemon).unwrap().unwrap();
    assert_eq!(Path::new(&profile.daemon_store), f.daemon_dir());
    assert!(f.daemon_dir().join("companion.json").exists());
    assert!(!f.daemon_dir().join("companion-pending.json").exists());
    assert!(
        !f.daemon_dir().join("companion.sock").exists(),
        "registration child drained"
    );
    assert!(!f.socket.exists(), "no normal serve");
    assert_no_membership(&f);
    let keys = std::fs::read(f.daemon_dir().join("keys.json")).unwrap();
    let desktop = std::fs::read(f.desktop.join("ring.json")).ok();
    let follower = f.ring();
    assert!(!follower.reserve_local_companion(&f.home).unwrap());
    assert!(follower
        .register_local_companion(&daemon)
        .unwrap()
        .is_none());
    follower.quit();
    // Backend setup lifecycle used by actual Tauri setup, not a rendered/native GUI test.
    ring.quit();
    drop(ring);
    expired_pending(&f);
    let (mut cancel, rx) = f.pair(5_000);
    assert!(rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .starts_with("Computer pairing pending."));
    unsafe {
        libc::kill(cancel.0.id() as i32, libc::SIGINT);
    }
    wait(|| cancel.0.try_wait().unwrap().is_some());
    assert!(!cancel.0.wait().unwrap().success());
    assert_eq!(
        std::fs::read(f.daemon_dir().join("keys.json")).unwrap(),
        keys
    );
    assert_eq!(std::fs::read(f.desktop.join("ring.json")).ok(), desktop);
    assert!(!f.socket.exists(), "CLI must not autostart GUI or serve");
}
#[test]
fn unavailable_or_mismatched_helper_leaves_durable_deny_only_pending_after_gui_close() {
    for mismatch in [false, true] {
        let f = Fixture::new();
        let ring = f.ring();
        // A nonexistent binary tests unavailable; /bin/false reports no matching build.
        let daemon = f.daemon(
            if mismatch {
                "/bin/false".into()
            } else {
                f.home.join("missing-xshelld")
            },
            xshelld::build_id::BUILD.clone(),
        );
        assert!(ring.register_local_companion(&daemon).is_err());
        assert!(f.daemon_dir().join("companion-pending.json").exists());
        assert!(!f.daemon_dir().join("companion.json").exists());
        assert!(!f.daemon_dir().join("keys.json").exists());
        ring.quit();
        expired_pending(&f);
        assert!(!f.daemon_dir().join("keys.json").exists());
    }
}
#[test]
fn actual_helper_failure_cancellation_and_live_server_collision_do_not_steal_endpoint() {
    let f = Fixture::new();
    let ring = f.ring();
    ring.reserve_local_companion(&f.home).unwrap();
    let mut child = OwnedChild(
        Command::new(bin())
            .args(["companion-registration", "--home"])
            .arg(&f.home)
            .arg("--socket")
            .arg(&f.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    writeln!(
        child.0.stdin.as_mut().unwrap(),
        "{}",
        serde_json::to_string(&f.desktop).unwrap()
    )
    .unwrap();
    wait(|| f.daemon_dir().join("companion.sock").exists());
    assert!(endpoint(&f).request(CompanionRequest::Descriptor).is_ok());
    let refuse = endpoint(&f)
        .request(CompanionRequest::CompletionStatus)
        .unwrap_err();
    assert!(refuse.contains("registration helper refuses"));
    let paths = xshelld::paths::resolve(&f.home, None, Some(&f.socket));
    let config = xshelld::server::Config::new(f.home.clone(), paths.clone());
    assert!(matches!(
        xshelld::server::Server::start(config),
        Err(xshelld::server::StartError::AlreadyRunning)
    ));
    child.0.stdin.take();
    wait(|| child.0.try_wait().unwrap().is_some());
    assert!(child.0.wait().unwrap().success());
    assert!(!f.daemon_dir().join("companion.sock").exists());
    assert!(f.daemon_dir().join("companion-pending.json").exists());
    // Normal serve can own its existing endpoint; a second child must refuse the lock.
    let server =
        xshelld::server::Server::start(xshelld::server::Config::new(f.home.clone(), paths))
            .unwrap();
    let mut collision = OwnedChild(
        Command::new(bin())
            .args(["companion-registration", "--home"])
            .arg(&f.home)
            .arg("--socket")
            .arg(&f.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    writeln!(
        collision.0.stdin.as_mut().unwrap(),
        "{}",
        serde_json::to_string(&f.desktop).unwrap()
    )
    .unwrap();
    wait(|| collision.0.try_wait().unwrap().is_some());
    assert!(!collision.0.wait().unwrap().success());
    let profile = ring
        .register_local_companion(&f.daemon(bin(), xshelld::build_id::BUILD.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(
        endpoint(&f).request(CompanionRequest::Descriptor).map(
            |r| matches!(r, CompanionResponse::Identity { profile: Some(p), .. } if p == profile)
        ),
        Ok(true)
    );
    server.shutdown();
    ring.quit();
}
#[test]
fn protected_administration_before_membership_epoch_cancel_follower_and_owner_death() {
    let f = Fixture::new();
    let ring = f.ring();
    let manager = f.manager(&ring);
    let mut cli = AdministrationClient::connect(&f.desktop, administration_deadline()).unwrap();
    let operation = uuid::Uuid::new_v4();
    assert_eq!(
        cli.request(R::Cancel { operation }, administration_deadline())
            .unwrap(),
        S::Cancelled
    );
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    assert_eq!(
        cli.request(
            R::Start {
                operation,
                expires_at
            },
            administration_deadline()
        )
        .unwrap(),
        S::Cancelled
    );
    let mut follower =
        AdministrationClient::connect(&f.desktop, administration_deadline()).unwrap();
    assert_eq!(
        follower
            .request(R::Status { operation }, administration_deadline())
            .unwrap(),
        S::Unknown
    );
    let next = uuid::Uuid::new_v4();
    assert_eq!(
        follower
            .request(
                R::Start {
                    operation: next,
                    expires_at
                },
                administration_deadline()
            )
            .unwrap(),
        S::ComputerPending
    );
    assert_eq!(
        follower
            .request(R::RequestOwnerFocus {}, administration_deadline())
            .unwrap(),
        S::OwnerFocusRequested
    );
    wait(|| ring.take_administration_focus_request());
    assert_eq!(
        follower
            .request(R::InvalidateOrigin {}, administration_deadline())
            .unwrap(),
        S::OriginInvalidated
    );
    assert_eq!(
        follower
            .request(
                R::Start {
                    operation: next,
                    expires_at
                },
                administration_deadline()
            )
            .unwrap(),
        S::OriginInvalidated
    );
    ring.quit();
    assert!(cli
        .request(R::Status { operation }, administration_deadline())
        .is_err());
    manager.shutdown();
    // New owner has a new epoch and no reconstructed pending operations.
    let new_ring = f.ring();
    let new_manager = f.manager(&new_ring);
    let mut new_cli = AdministrationClient::connect(&f.desktop, administration_deadline()).unwrap();
    assert_eq!(
        new_cli
            .request(R::Status { operation }, administration_deadline())
            .unwrap(),
        S::Unknown
    );
    new_ring.quit();
    new_manager.shutdown();
    assert_no_membership(&f);
}
#[test]
fn malformed_pending_cli_is_fail_closed_without_code_or_keys() {
    let f = Fixture::new();
    let ring = f.ring();
    ring.reserve_local_companion(&f.home).unwrap();
    std::fs::write(f.daemon_dir().join("companion-pending.json"), b"malformed").unwrap();
    ring.quit();
    let (mut cli, rx) = f.pair(100);
    wait(|| cli.0.try_wait().unwrap().is_some());
    assert_eq!(cli.0.wait().unwrap().code(), Some(1));
    assert!(rx.iter().collect::<Vec<_>>().is_empty());
    assert!(!f.daemon_dir().join("keys.json").exists());
}

#[test]
fn actual_child_death_after_registration_commit_before_denial_clear_is_idempotent() {
    let f = Fixture::new();
    let ring = f.ring();
    ring.reserve_local_companion(&f.home).unwrap();
    let cut = f.home.join("after-save");
    std::thread::scope(|scope| {
        // Declared inside the scope: panic cleanup kills the child before scope join.
        let mut child = OwnedChild(
            Command::new(bin())
                .args(["companion-registration", "--home"])
                .arg(&f.home)
                .arg("--socket")
                .arg(&f.socket)
                .env("XSHELLD_TEST_REGISTRATION_AFTER_SAVE", &cut)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        writeln!(
            child.0.stdin.as_mut().unwrap(),
            "{}",
            serde_json::to_string(&f.desktop).unwrap()
        )
        .unwrap();
        wait(|| f.daemon_dir().join("companion.sock").exists());
        let worker = scope.spawn(|| ring.register_companion(&endpoint(&f)));
        wait(|| cut.exists());
        assert!(f.daemon_dir().join("companion.json").exists());
        assert!(f.daemon_dir().join("companion-pending.json").exists());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(
            worker.join().unwrap().is_err(),
            "no private ack escaped the child"
        );
    });
    let policy = std::fs::read(f.daemon_dir().join("companion.json")).unwrap();
    let keys = std::fs::read(f.daemon_dir().join("keys.json")).unwrap();
    let profile = ring
        .register_local_companion(&f.daemon(bin(), xshelld::build_id::BUILD.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(f.daemon_dir().join("companion.json")).unwrap(),
        policy
    );
    assert_eq!(
        std::fs::read(f.daemon_dir().join("keys.json")).unwrap(),
        keys
    );
    assert_eq!(profile.desktop_store, f.desktop.to_str().unwrap());
    assert!(!f.daemon_dir().join("companion-pending.json").exists());
    ring.quit();
    expired_pending(&f);
}

fn send_wire(stream: &mut std::os::unix::net::UnixStream, value: serde_json::Value) {
    let bytes = serde_json::to_vec(&value).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&0u64.to_be_bytes()).unwrap();
    stream.write_all(&bytes).unwrap();
}
fn read_wire(
    stream: &mut std::os::unix::net::UnixStream,
) -> Result<serde_json::Value, std::io::Error> {
    use std::io::Read;
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let size = u32::from_be_bytes(header) as usize;
    assert!(size < 4096);
    let mut key = [0; 8];
    stream.read_exact(&mut key)?;
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn wire_hello(f: &Fixture) -> (std::os::unix::net::UnixStream, serde_json::Value) {
    use sha2::{Digest, Sha256};
    let mut stream = xshell_core::local_ipc::protected_connect(
        &f.desktop.join("host-broker.sock"),
        Duration::from_secs(2),
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let profile = format!(
        "{:x}",
        Sha256::digest(f.desktop.as_os_str().as_encoded_bytes())
    );
    send_wire(
        &mut stream,
        serde_json::json!({"type":"hello", "version":1, "profile_id":profile}),
    );
    let welcome = read_wire(&mut stream).unwrap();
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(read_wire(&mut stream).unwrap()["type"], "snapshot");
    (stream, welcome["owner_epoch"].clone())
}
#[test]
fn protected_wire_forged_confirm_cli_flag_and_stale_epoch_release_no_proofs() {
    let f = Fixture::new();
    let ring = f.ring();
    let manager = f.manager(&ring);
    for request in [
        serde_json::json!({"action":"confirm"}),
        serde_json::json!({"action":"sign"}),
        serde_json::json!({"action":"request-owner-focus", "gui":true}),
    ] {
        let (mut stream, epoch) = wire_hello(&f);
        send_wire(
            &mut stream,
            serde_json::json!({"type":"administration", "owner_epoch":epoch, "request_id":1, "request":request}),
        );
        assert!(read_wire(&mut stream).is_err());
    }
    let (mut wrong_epoch, _) = wire_hello(&f);
    send_wire(
        &mut wrong_epoch,
        serde_json::json!({"type":"administration", "owner_epoch":uuid::Uuid::new_v4(), "request_id":1,
        "request":{"action":"start", "operation":uuid::Uuid::new_v4(), "expires_at":1}}),
    );
    assert!(read_wire(&mut wrong_epoch).is_err());
    let out = Command::new(bin())
        .args(["pair", "--confirm"])
        .arg("--home")
        .arg(&f.home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(!f.daemon_dir().join("keys.json").exists());
    assert!(!f.desktop.join("ring.json").exists());
    ring.quit();
    manager.shutdown();
}

#[test]
fn unsafe_local_store_cannot_become_headless_permission_after_reservation_refusal() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), f.home.join(".xshell")).unwrap();
    let ring = f.ring();
    assert!(ring.reserve_local_companion(&f.home).is_err());
    ring.quit();
    let (mut cli, rx) = f.pair(100);
    wait(|| cli.0.try_wait().unwrap().is_some());
    assert_eq!(cli.0.wait().unwrap().code(), Some(1));
    assert!(rx.iter().collect::<Vec<_>>().is_empty());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn conflicting_desktop_cannot_replace_registered_association_or_retained_keys() {
    let f = Fixture::new();
    let ring = f.ring();
    ring.register_local_companion(&f.daemon(bin(), xshelld::build_id::BUILD.clone()))
        .unwrap()
        .unwrap();
    let policy = std::fs::read(f.daemon_dir().join("companion.json")).unwrap();
    let keys = std::fs::read(f.daemon_dir().join("keys.json")).unwrap();
    let other = f.home.join("other-desktop");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&other)
        .unwrap();
    assert!(xshell_core::companion_reservation::reserve(&f.home, &other).is_err());
    assert_eq!(
        std::fs::read(f.daemon_dir().join("companion.json")).unwrap(),
        policy
    );
    assert_eq!(
        std::fs::read(f.daemon_dir().join("keys.json")).unwrap(),
        keys
    );
    assert!(!f.daemon_dir().join("companion-pending.json").exists());
    ring.quit();
}

#[test]
fn desktop_shutdown_reaps_its_inflight_registration_child_without_clearing_denial() {
    let f = Fixture::new();
    let ring = f.ring();
    let cut = f.home.join("shutdown-after-save");
    let daemon = xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin: bin(),
        home: f.home.clone(),
        socket: f.socket.clone(),
        env: vec![(
            "XSHELLD_TEST_REGISTRATION_AFTER_SAVE".into(),
            cut.clone().into_os_string(),
        )],
        version: "1.5.0".into(),
        build: xshelld::build_id::BUILD.clone(),
    })
    .unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| ring.register_local_companion(&daemon));
        wait(|| cut.exists());
        daemon.hang_up();
        assert!(worker.join().unwrap().is_err());
    });
    assert!(f.daemon_dir().join("companion-pending.json").exists());
    assert!(
        endpoint(&f).request(CompanionRequest::Descriptor).is_err(),
        "no hidden registration service after shutdown"
    );
    assert!(!f.socket.exists());
    ring.quit();
    expired_pending(&f);
}

fn fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
}
#[test]
fn no_writer_fifo_metadata_actual_cli_and_helper_fail_promptly_without_effects() {
    for name in ["companion-pending.json", "companion.json"] {
        let f = Fixture::new();
        let ring = f.ring();
        ring.reserve_local_companion(&f.home).unwrap();
        let path = f.daemon_dir().join(name);
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        fifo(&path);
        let (mut cli, rx) = f.pair(100);
        let until = Instant::now() + Duration::from_millis(700);
        while cli.0.try_wait().unwrap().is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let prompt_cli = cli.0.try_wait().unwrap().is_some();
        if !prompt_cli {
            cli.0.kill().unwrap();
        }
        let code = cli.0.wait().unwrap();
        assert!(rx.iter().collect::<Vec<_>>().is_empty());
        let mut helper = OwnedChild(
            Command::new(bin())
                .args(["companion-registration", "--home"])
                .arg(&f.home)
                .arg("--socket")
                .arg(&f.socket)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        writeln!(
            helper.0.stdin.as_mut().unwrap(),
            "{}",
            serde_json::to_string(&f.desktop).unwrap()
        )
        .unwrap();
        let until = Instant::now() + Duration::from_millis(700);
        while helper.0.try_wait().unwrap().is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let prompt_helper = helper.0.try_wait().unwrap().is_some();
        if !prompt_helper {
            helper.0.kill().unwrap();
        }
        assert!(!helper.0.wait().unwrap().success());
        assert!(
            prompt_cli && prompt_helper,
            "FIFO {name} must not hold open/read or child shutdown"
        );
        assert_eq!(code.code(), Some(1));
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_file());
        assert!(!f.daemon_dir().join("keys.json").exists());
        assert!(!f.daemon_dir().join("companion.json").is_file());
        assert_no_membership(&f);
        ring.quit();
    }
}

fn stalled_administration_cli(stall_hello: bool) {
    let f = Fixture::new();
    let ring = f.ring();
    ring.reserve_local_companion(&f.home).unwrap();
    ring.quit();
    let listener =
        xshell_core::local_ipc::ProtectedListener::bind(f.desktop.join("host-broker.sock"))
            .unwrap();
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            if let Ok(stream) = listener.accept() {
                break stream;
            }
            assert!(Instant::now() < until, "actual CLI did not connect");
            std::thread::sleep(Duration::from_millis(5));
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(700)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_millis(700)))
            .unwrap();
        let hello = read_wire(&mut stream).unwrap();
        if stall_hello {
            std::thread::sleep(Duration::from_millis(800));
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(100));
        send_wire(
            &mut stream,
            serde_json::json!({"type":"welcome", "version":1, "profile_id":hello["profile_id"],
            "owner_epoch":uuid::Uuid::new_v4(), "client_id":uuid::Uuid::new_v4()}),
        );
        let start = read_wire(&mut stream).unwrap();
        assert_eq!(start["request"]["action"], "start");
        std::thread::sleep(Duration::from_millis(350));
        // A late result must not release a new focus request after the original CLI TTL.
        let reply = serde_json::to_vec(&serde_json::json!({"type":"reply", "request_id":start["request_id"], "result":{"Ok":"computer-pending"}})).unwrap();
        let sent = stream
            .write_all(&(reply.len() as u32).to_be_bytes())
            .and_then(|()| stream.write_all(&0u64.to_be_bytes()))
            .and_then(|()| stream.write_all(&reply));
        if sent.is_err() {
            return Vec::new();
        }
        read_wire(&mut stream).into_iter().collect::<Vec<_>>()
    });
    let began = Instant::now();
    let (mut cli, rx) = f.pair(200);
    wait(|| cli.0.try_wait().unwrap().is_some());
    let elapsed = began.elapsed();
    assert_eq!(cli.0.wait().unwrap().code(), Some(5));
    let later = server.join().unwrap();
    assert!(
        later
            .iter()
            .all(|message| message["request"]["action"] != "request-owner-focus"),
        "late Start reply must not cause post-expiry focus: {later:?}"
    );
    assert!(
        elapsed < Duration::from_millis(600),
        "original 200ms CLI TTL extended by fresh IO budgets: {elapsed:?}"
    );
    assert!(!rx.iter().any(|line| line.starts_with("Pairing code:")));
    assert!(!f.daemon_dir().join("keys.json").exists());
}
#[test]
fn actual_cli_hello_stall_does_not_extend_original_deadline() {
    stalled_administration_cli(true);
}
#[test]
fn actual_cli_start_response_stall_does_not_release_focus_or_renew_io_deadline() {
    stalled_administration_cli(false);
}

#[test]
fn outstanding_actual_legacy_offer_cannot_erase_failed_startup_denial() {
    let relay = workerd::Relay::start();
    let f = Fixture::new();
    let (mut incumbent, rx) = f.pair_on_relay(30_000, Some(&relay.url()));
    let line = rx.recv_timeout(Duration::from_secs(8)).unwrap();
    assert!(line.starts_with("Pairing code:")); // never include the code in artifacts
    let keys = std::fs::read(f.daemon_dir().join("keys.json")).unwrap();
    let ring = f.ring();
    assert!(ring
        .register_local_companion(&f.daemon(
            f.home.join("missing-helper"),
            xshelld::build_id::BUILD.clone()
        ))
        .is_err());
    let denied = f.daemon_dir().join("companion-pending.json").exists();
    ring.quit();
    incumbent.0.kill().unwrap();
    incumbent.0.wait().unwrap();
    assert!(
        denied,
        "active legacy admission cannot prevent persistent denial"
    );
    expired_pending(&f);
    assert_eq!(
        std::fs::read(f.daemon_dir().join("keys.json")).unwrap(),
        keys
    );
    assert!(!f.daemon_dir().join("companion.json").exists());
}

#[test]
fn actual_legacy_guest_proof_escape_leaves_sticky_denial_across_restart_and_helper_retry() {
    use xshell_protocol::ring::pairing::{JoinRequest, Joined, PairCode, PairRefusal};
    use xshell_protocol::ring::relay::pair::{
        host_pairing, HostRequest, PairOptions, PairPipe, PairingHost,
    };
    use xshell_protocol::ring::{DeviceKeys, RingId, Role};
    struct RefuseAfterProof<'a> {
        f: &'a Fixture,
        ring: &'a DesktopRing,
        saw_proof: std::sync::atomic::AtomicBool,
    }
    impl PairingHost for RefuseAfterProof<'_> {
        fn consume(&self) -> Result<(), PairRefusal> {
            Ok(())
        }
        fn add(&self, _: &JoinRequest) -> Result<Joined, PairRefusal> {
            // host_pairing calls add only after validating the guest's signed Noise m3.
            self.saw_proof
                .store(true, std::sync::atomic::Ordering::Release);
            assert!(self
                .ring
                .register_local_companion(&self.f.daemon(
                    self.f.home.join("missing-helper"),
                    xshelld::build_id::BUILD.clone()
                ))
                .is_err());
            Err(PairRefusal::PublishFailed) // No enrollment/genesis/publish is performed.
        }
    }
    let relay = workerd::Relay::start();
    let f = Fixture::new();
    let (mut incumbent, rx) = f.pair_on_relay(30_000, Some(&relay.url()));
    let line = rx.recv_timeout(Duration::from_secs(8)).unwrap();
    let code = PairCode::parse(line.strip_prefix("Pairing code: ").unwrap()).unwrap();
    let keys = std::fs::read(f.daemon_dir().join("keys.json")).unwrap();
    let ring = f.ring();
    let secret = code.secret();
    let mut pipe = PairPipe::open(&relay.url(), &secret.slot(), &PairOptions::default()).unwrap();
    pipe.wait_peer(Instant::now() + Duration::from_secs(5), None)
        .unwrap();
    let host = DeviceKeys::generate().unwrap();
    let approver = RefuseAfterProof {
        f: &f,
        ring: &ring,
        saw_proof: std::sync::atomic::AtomicBool::new(false),
    };
    assert!(host_pairing(
        pipe,
        &HostRequest {
            keys: &host,
            secret: &secret,
            ring_id: &RingId::derive(&host.sign_key()),
            name: "legacy refusal control",
            role: Role::Daemon
        },
        &approver,
        None
    )
    .is_err());
    assert!(approver
        .saw_proof
        .load(std::sync::atomic::Ordering::Acquire));
    wait(|| incumbent.0.try_wait().unwrap().is_some());
    assert_eq!(incumbent.0.wait().unwrap().code(), Some(1));
    let mut stderr = String::new();
    std::io::Read::read_to_string(incumbent.0.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("remote outcome is uncertain"));
    let denial = std::fs::read(f.daemon_dir().join("companion-pending.json")).unwrap();
    ring.quit();
    let restarted = f.ring();
    let error = restarted
        .register_local_companion(&f.daemon(bin(), xshelld::build_id::BUILD.clone()))
        .unwrap_err();
    assert!(error.contains("remote outcome is uncertain"));
    assert_eq!(
        std::fs::read(f.daemon_dir().join("companion-pending.json")).unwrap(),
        denial
    );
    assert_eq!(
        std::fs::read(f.daemon_dir().join("keys.json")).unwrap(),
        keys
    );
    assert!(!f.daemon_dir().join("companion.json").exists());
    assert_no_membership(&f);
    restarted.quit();
    expired_pending(&f);
}

#[test]
fn actual_cli_cancel_cleanup_is_limited_to_explicit_100ms_allowance() {
    let f = Fixture::new();
    let ring = f.ring();
    ring.reserve_local_companion(&f.home).unwrap();
    ring.quit();
    let listener =
        xshell_core::local_ipc::ProtectedListener::bind(f.desktop.join("host-broker.sock"))
            .unwrap();
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            if let Ok(stream) = listener.accept() {
                break stream;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let hello = read_wire(&mut stream).unwrap();
        send_wire(
            &mut stream,
            serde_json::json!({"type":"welcome", "version":1, "profile_id":hello["profile_id"],
            "owner_epoch":uuid::Uuid::new_v4(), "client_id":uuid::Uuid::new_v4()}),
        );
        loop {
            let request = read_wire(&mut stream).unwrap();
            let action = request["request"]["action"].as_str().unwrap();
            if action == "cancel" {
                std::thread::sleep(Duration::from_millis(800));
                return;
            }
            let status = match action {
                "start" | "status" => "computer-pending",
                "request-owner-focus" => "owner-focus-requested",
                _ => panic!("unexpected administration action"),
            };
            send_wire(
                &mut stream,
                serde_json::json!({"type":"reply", "request_id":request["request_id"], "result":{"Ok":status}}),
            );
        }
    });
    let began = Instant::now();
    let (mut cli, rx) = f.pair(200);
    wait(|| cli.0.try_wait().unwrap().is_some());
    let elapsed = began.elapsed();
    assert_eq!(cli.0.wait().unwrap().code(), Some(5));
    server.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(600),
        "200ms TTL plus explicit 100ms cleanup exceeded margin: {elapsed:?}"
    );
    assert!(!rx.iter().any(|line| line.starts_with("Pairing code:")));
}

#[test]
fn actual_registered_cli_expires_while_classification_lock_is_still_held() {
    let f = Fixture::new();
    let ring = f.ring();
    ring.reserve_local_companion(&f.home).unwrap();
    ring.quit();
    let admin = xshell_core::local_ipc::ProtectedListener::bind(f.desktop.join("host-broker.sock"))
        .unwrap();
    let relay = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    relay.set_nonblocking(true).unwrap();
    let relay_url = format!("ws://{}", relay.local_addr().unwrap());
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.daemon_dir().join("ring.lock"))
        .unwrap();
    lock.lock().unwrap();
    let began = Instant::now();
    let (mut cli, rx) = f.pair_on_relay(200, Some(&relay_url));
    let test_until = began + Duration::from_millis(700);
    while cli.0.try_wait().unwrap().is_none() && Instant::now() < test_until {
        std::thread::sleep(Duration::from_millis(10));
    }
    let exited_before_unlock = cli.0.try_wait().unwrap().is_some();
    // Bounded pre-fix RED cleanup: kill/reap the child without releasing its awaited lock.
    if !exited_before_unlock {
        cli.0.kill().unwrap();
    }
    let status = cli.0.wait().unwrap();
    let lines: Vec<_> = rx.iter().collect();
    assert!(
        matches!(admin.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "no administration connection/Start/focus may escape blocked classification"
    );
    assert!(
        matches!(relay.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "classification must not fall back to a Relay dial"
    );
    assert!(!f.daemon_dir().join("keys.json").exists());
    assert!(!f.daemon_dir().join("roster.json").exists());
    assert!(f.daemon_dir().join("companion-pending.json").exists());
    assert!(!lines
        .iter()
        .any(|line| line.starts_with("Pairing code:") || line.starts_with("Paired as")));
    drop(lock);
    assert!(
        exited_before_unlock,
        "original 200ms deadline must expire before classification lock release"
    );
    assert_eq!(status.code(), Some(5));
    assert!(lines
        .iter()
        .any(|line| line == "Computer pairing request expired. No pairing code was generated."));
}

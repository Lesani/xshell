#![cfg(unix)]
//! Actual Unix CLI + unchanged production workerd. Computer confirmations use private
//! Rust backend leases, NOT native GUI/window-consent evidence. No v2 roster seeding.
//! Run with TMPDIR on SSD or RAM, not an HDD: under default test parallelism the fsync-heavy
//! stores on an HDD push Device-code readiness past the short test TTLs (2-4 s).
mod common;
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use xshell_hostlink::ring::adoption::{CreateDecision, JoinOffer, JoinStatus};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::store::{AdoptionPhase, Store};
use xshell_hostlink::ring::{
    AdoptionTestHook, AdoptionTestPoint, DesktopRing, DesktopRingConfig, RingObserver, RingView,
};
use xshell_hostlink::{BinarySource, Manager, ManagerConfig, Observer, TransportFactory};
use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::RosterChain;
use xshelld::server::{Config, Server, ServerHandle};
struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}
impl Observer for Quiet {
    fn status(&self, _: &xshell_hostlink::HostStatus) {}
    fn terminals(&self, _: &str, _: &[xshell_protocol::msg::TerminalInfo]) {}
}
struct NoFallback;
impl TransportFactory for NoFallback {
    fn for_host(&self, _: &xshell_hostlink::HostConfig) -> Box<dyn xshell_hostlink::Transport> {
        panic!("Pair must not fall back to SSH/Direct");
    }
}
impl BinarySource for NoFallback {
    fn fetch(
        &self,
        _: &xshell_hostlink::Wanted,
        _: &xshell_hostlink::CancelToken,
    ) -> Result<xshell_hostlink::Fetched, String> {
        panic!("Pair must not install/start another Daemon");
    }
}
struct Fixture {
    home: tempfile::TempDir,
    paths: xshelld::paths::Paths,
    desktop: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        common::isolate_home();
        let home = tempfile::Builder::new().prefix("b1-").tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let paths = xshelld::paths::resolve(&root, None, Some(&root.join("run/daemon.sock")));
        Self {
            desktop: root.join("desktop"),
            home,
            paths,
        }
    }
    fn server(&self) -> ServerHandle {
        Server::start(Config::new(self.home.path().into(), self.paths.clone())).unwrap()
    }
    fn ring(&self, relay: &str) -> Arc<DesktopRing> {
        self.ring_with_hook(relay, None)
    }
    fn ring_with_hook(&self, relay: &str, hook: Option<AdoptionTestHook>) -> Arc<DesktopRing> {
        let mut cfg = DesktopRingConfig::new(self.desktop.clone(), "Computer".into());
        cfg.default_relay_url = relay.into();
        cfg.owner_retry = Duration::from_millis(20);
        cfg.adoption_test_hook = hook;
        DesktopRing::open(cfg, Arc::new(Quiet))
    }
    fn manager(&self, ring: &DesktopRing) -> Arc<Manager> {
        let manager = Arc::new(Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(NoFallback),
            Arc::new(NoFallback),
            Arc::new(Quiet),
        )));
        ring.attach_manager(&manager).unwrap();
        manager
    }
    fn pair(&self, relay: &str, ttl_ms: u64) -> PairChild {
        self.pair_with_env(relay, ttl_ms, &[])
    }
    fn pair_with_env(
        &self,
        relay: &str,
        ttl_ms: u64,
        env: &[(&str, &std::path::Path)],
    ) -> PairChild {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xshelld"));
        command
            .arg("pair")
            .args(["--relay", relay])
            .arg("--home")
            .arg(self.home.path())
            .arg("--socket")
            .arg(&self.paths.socket)
            .env("XSHELLD_PAIR_TTL_MS", ttl_ms.to_string());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let _ = tx.send(line.unwrap());
            }
        });
        // Drain stderr so the CLI never blocks on a full pipe, and keep it for failure reports.
        let err = child.stderr.take().unwrap();
        let stderr = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = stderr.clone();
        let stderr_reader = std::thread::spawn(move || {
            let mut buf = String::new();
            for line in BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                buf.push_str(&line);
                buf.push('\n');
                *sink.lock().unwrap() = buf.clone();
            }
        });
        PairChild {
            child,
            lines,
            words: 0,
            stderr,
            stderr_reader: Some(stderr_reader),
            started: Instant::now(),
        }
    }
    fn companion(&self) -> CompanionClient {
        CompanionClient::new(self.paths.ring_dir.join("companion.sock"))
    }
    fn chain(&self) -> RosterChain {
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.paths.ring_dir.join("roster.json")).unwrap(),
        )
        .unwrap();
        RosterChain::from_tokens(
            &serde_json::from_value::<Vec<String>>(json["rosters"].clone()).unwrap(),
        )
        .unwrap()
    }
}
struct PairChild {
    child: Child,
    lines: mpsc::Receiver<String>,
    words: usize,
    stderr: Arc<std::sync::Mutex<String>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    started: Instant,
}
impl PairChild {
    fn code(&mut self) -> DeviceCode {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let line = match self
                .lines
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(line) => line,
                Err(e) => self.no_code(e),
            };
            if let Some(words) = line.strip_prefix("Device code: ") {
                self.words += 1;
                return DeviceCode::parse(words).expect("eight words");
            }
        }
    }
    /// The CLI gave no Device code: report how it ended (exit status, elapsed, stderr).
    fn no_code(&mut self, e: mpsc::RecvTimeoutError) -> ! {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = None;
        while Instant::now() < deadline {
            status = self.child.try_wait().unwrap();
            if status.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if status.is_some() {
            if let Some(r) = self.stderr_reader.take() {
                let _ = r.join();
            }
        }
        panic!(
            "CLI gave no Device code ({e:?}) after {:?}; exit status: {}; stderr:\n{}",
            self.started.elapsed(),
            status.map_or("still running".into(), |s| format!("{s:?}")),
            self.stderr.lock().unwrap()
        );
    }
    fn stderr(&mut self) -> String {
        if let Some(r) = self.stderr_reader.take() {
            let _ = r.join();
        }
        self.stderr.lock().unwrap().clone()
    }
    fn finish(&mut self) -> i32 {
        wait(|| self.child.try_wait().unwrap().is_some());
        let code = self.child.wait().unwrap().code().unwrap();
        for line in self.lines.iter() {
            if line.starts_with("Device code: ") {
                self.words += 1;
            }
        }
        code
    }
    fn cancel(&self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) },
            0
        );
    }
}
impl Drop for PairChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn wait(mut test: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !test() {
        assert!(Instant::now() < until, "bounded Pair driver wait");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn create(ring: &DesktopRing, client: &CompanionClient) {
    ring.create_computer(client, |p, lease| {
        Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
    })
    .unwrap();
}
fn controller(ring: &DesktopRing) -> Arc<JoinOffer> {
    let mut result = None;
    wait(|| {
        result = ring
            .administration_joins()
            .into_iter()
            .next()
            .map(|(_, c)| c);
        result.is_some()
    });
    result.unwrap()
}
fn confirm_backend(join: &JoinOffer) {
    let (preview, lease) = join.await_consent(Duration::from_secs(8)).unwrap();
    join.confirm_join(lease.confirm(&preview.proposal_hash).unwrap())
        .unwrap();
}
fn assert_no_words_on_disk(f: &Fixture, code: &DeviceCode) {
    let words = code.format();
    for path in [
        f.desktop.join("ring.json"),
        f.paths.ring_dir.join("companion.json"),
        f.paths.ring_dir.join("standalone-adoption.json"),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            assert!(
                !String::from_utf8_lossy(&bytes).contains(words.as_str()),
                "credential persisted"
            );
        }
    }
}
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn actual_cli_registered_same_offer_prints_once_and_survives_own_membership_transition_backend_consent(
) {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let _bd = b.server();
    let br = b.ring(&relay.url());
    let profile = br.register_companion(&b.companion()).unwrap();
    let _manager = b.manager(&br);
    let mut child = b.pair(&relay.url(), 30_000);
    let code = child.code();
    let join = controller(&br);
    assert!(join.is_ready());
    assert!(join.code.format() == code.format());
    let wire_operation = join.operation.clone();
    let expiry = join.expires_at;
    let pending = Store::new(b.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .pending_join
        .unwrap();
    assert_eq!(pending.intent.operation, wire_operation);
    assert_eq!(pending.intent.expires_at, expiry);
    assert_eq!(pending.intent.target, profile.target());
    assert!(br.chain().is_none());
    std::thread::scope(|scope| {
        let approve = scope.spawn(|| ar.approve_device_code(&code));
        confirm_backend(&join);
        assert!(approve.join().unwrap().is_ok());
    });
    assert_eq!(child.finish(), 0);
    assert_eq!(child.words, 1);
    let stored = Store::new(b.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    let journal = stored.journal.unwrap();
    assert_eq!(journal.intent.operation, wire_operation);
    assert_eq!(journal.intent.expires_at, expiry);
    assert_eq!(journal.phase, AdoptionPhase::Installed);
    assert!(journal.desktop_installed && journal.daemon_installed);
    assert_eq!(stored.membership.unwrap().chain, b.chain());
    assert_no_words_on_disk(&b, &code);
    assert!(ar.approve_device_code(&code).is_err());
    br.quit();
    ar.quit();
}
#[test]
fn actual_cli_stopped_owner_manual_open_uses_original_deadline_without_autostart() {
    let relay = workerd::Relay::start();
    let b = Fixture::new();
    let _bd = b.server();
    let initial = b.ring(&relay.url());
    initial.register_companion(&b.companion()).unwrap();
    initial.quit();
    drop(initial);
    let before = Instant::now();
    let mut child = b.pair(&relay.url(), 4_000);
    let line = child.lines.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(line.starts_with("Computer pairing pending."));
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.lines.try_recv().is_err());
    assert!(child.child.try_wait().unwrap().is_none());
    let br = b.ring(&relay.url());
    let _manager = b.manager(&br);
    let code = child.code();
    let join = controller(&br);
    assert!(join.code.format() == code.format());
    assert!(join.expires_at <= unix_now() + 4);
    assert_eq!(child.finish(), 5);
    assert_eq!(child.words, 1);
    assert!(before.elapsed() < Duration::from_secs(5));
    wait(|| matches!(join.join_status(), JoinStatus::Failed { .. }));
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert_no_words_on_disk(&b, &code);
    br.quit();
}
#[test]
fn actual_cli_standalone_without_serve_installs_exact_daemon() {
    standalone_install(false, false, false);
}
#[test]
fn actual_cli_standalone_with_serve_installs_exact_daemon_and_running_runtime() {
    standalone_install(true, false, false);
}
#[test]
fn actual_cli_same_home_different_socket_uses_actual_owner_and_installs_running_runtime() {
    standalone_install(true, true, false);
}
#[test]
fn actual_cli_standalone_existing_upgraded_state_with_shared_0755_without_serve_retains_both_keys_and_exact_receipt(
) {
    standalone_install(false, false, true);
}
fn standalone_install(running: bool, different_socket: bool, existing: bool) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    {
        let mut b = Fixture::new();
        let retained = existing.then(|| {
            let shared = b.home.path().join(".xshell");
            std::fs::create_dir(&shared).unwrap();
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(shared.join("existing-install"), b"opaque installed bytes").unwrap();
            let before = std::fs::metadata(&shared).unwrap();
            let daemon = b.server();
            let (_, _, identity) = common::ring::identity(&daemon);
            assert!(!identity.is_null());
            let keys = std::fs::read(b.paths.ring_dir.join("keys.json")).unwrap();
            daemon.shutdown();
            assert!(b.paths.mode.exists());
            // Stale launch mode is not registration or live ownership.
            xshelld::paths::write_mode(&b.paths.mode, xshelld::paths::Mode::GuiBound).unwrap();
            (keys, before.dev(), before.ino())
        });
        let daemon = running.then(|| b.server());
        if different_socket {
            let original_lock = b.paths.lock.clone();
            b.paths = xshelld::paths::resolve(
                b.home.path(),
                None,
                Some(&b.home.path().join("other/daemon.sock")),
            );
            assert_ne!(original_lock, b.paths.lock);
        }
        let mut child = b.pair(&relay.url(), 30_000);
        let code = child.code();
        assert!(ar.approve_device_code(&code).is_ok());
        assert_eq!(child.finish(), 0);
        assert_eq!(child.words, 1);
        if let Some((keys, dev, ino)) = retained {
            let shared = b.home.path().join(".xshell");
            let after = std::fs::metadata(&shared).unwrap();
            assert_eq!((after.dev(), after.ino()), (dev, ino));
            assert_eq!(after.mode() & 0o777, 0o755);
            assert_eq!(
                std::fs::read(shared.join("existing-install")).unwrap(),
                b"opaque installed bytes"
            );
            assert_eq!(
                std::fs::read(b.paths.ring_dir.join("keys.json")).unwrap(),
                keys
            );
            assert!(!b.paths.socket.exists());
            assert!(!b.paths.pid.exists());
        }
        let receipt: serde_json::Value = serde_json::from_slice(
            &std::fs::read(b.paths.ring_dir.join("standalone-adoption.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["installed"], true);
        let chain = b.chain();
        assert_eq!(
            receipt["accepted"],
            serde_json::json!(chain
                .versions()
                .iter()
                .map(|v| v.token())
                .collect::<Vec<_>>())
        );
        assert!(matches!(
            chain
                .head()
                .roster()
                .membership()
                .unwrap()
                .enrollments
                .last()
                .unwrap()
                .kind,
            xshell_protocol::ring::membership::EnrollmentKind::Daemon
        ));
        assert!(!b.paths.ring_dir.join("companion.json").exists());
        assert!(!b.desktop.exists());
        assert_no_words_on_disk(&b, &code);
        if let Some(daemon) = &daemon {
            let (_, _, identity) = common::ring::identity(daemon);
            assert_eq!(identity["ring"]["version"], chain.head().version());
            assert!(b
                .companion()
                .request(CompanionRequest::InstallStandaloneAccepted {
                    operation: xshell_protocol::ring::b64::encode(&[99; 32]),
                    rosters: chain.versions().iter().map(|v| v.token().into()).collect()
                })
                .is_err());
            assert_eq!(b.chain(), chain);
        }
        assert!(ar.approve_device_code(&code).is_err());
    }
    ar.quit();
}
#[test]
fn actual_cli_cancel_after_ready_and_owner_loss_never_restart_or_succeed() {
    let relay = workerd::Relay::start();
    for owner_loss in [false, true] {
        let b = Fixture::new();
        let _bd = b.server();
        let br = b.ring(&relay.url());
        br.register_companion(&b.companion()).unwrap();
        let _manager = b.manager(&br);
        let mut child = b.pair(&relay.url(), 10_000);
        let code = child.code();
        let join = controller(&br);
        if owner_loss {
            br.quit();
        } else {
            child.cancel();
        }
        assert_ne!(child.finish(), 0);
        assert_eq!(child.words, 1);
        wait(|| matches!(join.join_status(), JoinStatus::Failed { .. }));
        assert!(!b.paths.ring_dir.join("roster.json").exists());
        assert_no_words_on_disk(&b, &code);
        br.quit();
    }
}
#[test]
fn protected_standalone_handoff_without_exact_journal_and_wire_origin_claims_refuse() {
    let b = Fixture::new();
    let _bd = b.server();
    assert!(b
        .companion()
        .request(CompanionRequest::InstallStandaloneAccepted {
            operation: xshell_protocol::ring::b64::encode(&[3; 32]),
            rosters: vec![]
        })
        .is_err());
    for value in [
        serde_json::json!({"action":"confirm","gui":true}),
        serde_json::json!({"action":"sign"}),
        serde_json::json!({"action":"request-owner-focus","native":true}),
    ] {
        assert!(serde_json::from_value::<
            xshell_hostlink::ring::administration::AdministrationRequest,
        >(value)
        .is_err());
    }
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert!(matches!(
        b.companion().request(CompanionRequest::Descriptor).unwrap(),
        CompanionResponse::Identity { profile: None, .. }
    ));
}
#[test]
fn actual_cli_standalone_expiry_and_ctrl_c_do_not_save_membership_or_start_serve() {
    let relay = workerd::Relay::start();
    for (cancel, existing) in [(false, false), (true, false), (false, true), (true, true)] {
        let b = Fixture::new();
        if existing {
            let daemon = b.server();
            common::ring::identity(&daemon);
            daemon.shutdown();
        }
        let mut child = b.pair(&relay.url(), if cancel { 10_000 } else { 2_000 });
        let code = child.code();
        if cancel {
            child.cancel();
        }
        assert_eq!(child.finish(), if cancel { 1 } else { 5 });
        assert_eq!(child.words, 1);
        assert!(!b.paths.socket.exists());
        assert!(!b.paths.ring_dir.join("roster.json").exists());
        assert_no_words_on_disk(&b, &code);
        let stderr = child.stderr();
        assert!(!stderr.contains(code.format().as_str()));
        b.server().shutdown(); // expiry/Ctrl-C released ownership after offer cleanup
    }
}
#[test]
fn actual_cli_durable_desktop_failure_is_reconciliation_not_success_backend_consent() {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let _bd = b.server();
    let desktop = b.desktop.clone();
    let br = b.ring_with_hook(
        &relay.url(),
        Some(AdoptionTestHook(Arc::new(move |point| {
            if point == AdoptionTestPoint::CompanionInstalledBeforeDesktop {
                std::fs::rename(
                    desktop.join("ring.json"),
                    desktop.join("receipt-before-failure.json"),
                )
                .unwrap();
                std::fs::create_dir(desktop.join("ring.json")).unwrap();
            }
        }))),
    );
    br.register_companion(&b.companion()).unwrap();
    let _manager = b.manager(&br);
    let mut child = b.pair(&relay.url(), 30_000);
    let code = child.code();
    let join = controller(&br);
    std::thread::scope(|scope| {
        let approve = scope.spawn(|| ar.approve_device_code(&code));
        confirm_backend(&join);
        assert!(approve.join().unwrap().is_ok());
    });
    assert_ne!(child.finish(), 0);
    assert_eq!(child.words, 1);
    assert!(matches!(
        join.join_status(),
        JoinStatus::ReconciliationRequired { .. }
    ));
    assert!(b.paths.ring_dir.join("roster.json").exists());
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(b.desktop.join("receipt-before-failure.json")).unwrap(),
    )
    .unwrap();
    assert!(!saved.to_string().contains(code.format().as_str()));
    br.quit();
    ar.quit();
}
#[test]
fn registration_child_cannot_claim_live_driver_readiness_and_manual_daemon_start_keeps_original_offer(
) {
    let relay = workerd::Relay::start();
    let b = Fixture::new();
    let br = b.ring(&relay.url());
    let local = xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin: env!("CARGO_BIN_EXE_xshelld").into(),
        home: b.home.path().into(),
        socket: b.paths.socket.clone(),
        env: vec![],
        version: "1.5.0".into(),
        build: xshelld::build_id::BUILD.clone(),
    })
    .unwrap();
    local.with_registration_endpoint(&b.desktop, |client| {
        let profile = br.register_companion(client)?;
        assert!(matches!(client.request(CompanionRequest::Descriptor)?, CompanionResponse::Identity { profile: Some(p), .. } if p == profile));
        assert!(client.request(CompanionRequest::PairDriverReady).is_err());
        assert!(client.request(CompanionRequest::StandaloneCommandReady).is_err()); assert!(!b.paths.ring_dir.join("roster.json").exists()); assert!(!b.paths.ring_dir.join("standalone-adoption.json").exists()); Ok(())
    }).unwrap();
    let _manager = b.manager(&br);
    let mut child = b.pair(&relay.url(), 4_000);
    assert!(child
        .lines
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .starts_with("Computer pairing pending."));
    std::thread::sleep(Duration::from_millis(250));
    assert!(child.lines.try_recv().is_err());
    assert!(!b.paths.socket.exists());
    assert!(Store::new(b.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .pending_join
        .is_none());
    let _daemon = b.server();
    let profile = br.register_companion(&b.companion()).unwrap();
    assert!(
        matches!(b.companion().request(CompanionRequest::PairDriverReady).unwrap(), CompanionResponse::Descriptor { profile: Some(p) } if p == profile)
    );
    let code = child.code();
    let join = controller(&br);
    assert!(join.code.format() == code.format());
    assert!(join.expires_at <= unix_now() + 4);
    child.cancel();
    assert_eq!(child.finish(), 1);
    assert_eq!(child.words, 1);
    br.quit();
}
#[test]
fn actual_cli_wrong_relay_does_not_consume_or_reissue_its_original_offer() {
    let relay = workerd::Relay::start();
    let wrong = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&wrong.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let mut child = b.pair(&relay.url(), 2_000);
    let code = child.code();
    let head = ar.chain().unwrap();
    assert!(ar.approve_device_code(&code).is_err());
    assert_eq!(child.finish(), 5);
    assert_eq!(child.words, 1);
    assert_eq!(ar.chain().unwrap(), head);
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    ar.quit();
}

#[test]
fn actual_cli_standalone_durable_failure_cannot_be_disk_only_success() {
    standalone_install_failure(false, false);
}
#[test]
fn actual_cli_standalone_lost_running_owner_cannot_be_disk_only_success() {
    standalone_install_failure(true, false);
}
#[test]
fn actual_cli_replaced_running_owner_refuses_pinned_install_then_exact_recovery_succeeds() {
    standalone_install_failure(true, true);
}
fn standalone_install_failure(owner_loss: bool, replace: bool) {
    {
        let relay = workerd::Relay::start();
        let a = Fixture::new();
        let _ad = a.server();
        let b = Fixture::new();
        let mut daemon = owner_loss.then(|| b.server());
        let (reached, receive) = mpsc::channel();
        let (release, proceed) = mpsc::channel();
        let proceed = std::sync::Mutex::new(proceed);
        let ar = a.ring_with_hook(
            &relay.url(),
            Some(AdoptionTestHook(Arc::new(move |point| {
                if point == AdoptionTestPoint::ApprovalBeforePublish {
                    reached.send(()).unwrap();
                    proceed
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(8))
                        .unwrap();
                }
            }))),
        );
        create(&ar, &a.companion());
        let mut child = b.pair(&relay.url(), 30_000);
        let code = child.code();
        std::thread::scope(|scope| {
            let approval = scope.spawn(|| ar.approve_device_code(&code));
            receive.recv_timeout(Duration::from_secs(8)).unwrap();
            if owner_loss {
                daemon.take().unwrap().shutdown();
                if replace {
                    daemon = Some(b.server());
                } // not this driver's pinned route
            } else {
                std::fs::create_dir(b.paths.ring_dir.join("roster.json")).unwrap();
            }
            release.send(()).unwrap();
            assert!(approval.join().unwrap().is_ok());
        });
        assert_ne!(child.finish(), 0);
        assert_eq!(child.words, 1);
        let journal: serde_json::Value = serde_json::from_slice(
            &std::fs::read(b.paths.ring_dir.join("standalone-adoption.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(journal["installed"], false);
        assert!(!journal["accepted"].as_array().unwrap().is_empty());
        if owner_loss {
            if !replace {
                daemon = Some(b.server());
            }
            let status = daemon
                .as_ref()
                .unwrap()
                .reconcile_standalone_adoption()
                .unwrap();
            assert_eq!(
                status.state,
                xshelld::server::adoption::StandaloneRecoveryState::Installed
            );
            let recovered: serde_json::Value = serde_json::from_slice(
                &std::fs::read(b.paths.ring_dir.join("standalone-adoption.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(recovered["accepted"], journal["accepted"]);
            assert_eq!(recovered["installed"], true);
        }
        assert_no_words_on_disk(&b, &code);
        ar.quit();
    }
}

#[test]
fn actual_cli_running_owner_membership_with_failed_disk_refuses_before_second_words_or_proofs() {
    runtime_admission(false);
}
#[test]
fn actual_cli_same_home_different_socket_cannot_bypass_actual_runtime_admission() {
    runtime_admission(true);
}
fn runtime_admission(different_socket: bool) {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let mut b = Fixture::new();
    let mut config = Config::new(b.home.path().into(), b.paths.clone());
    config.test_hook = Some(xshelld::server::TestHook(Arc::new(|_, point| {
        point == xshelld::server::TestPoint::BeforeRingRosterWrite
    })));
    let bd = Server::start(config).unwrap();
    let mut first = b.pair(&relay.url(), 30_000);
    let code = first.code();
    assert!(ar.approve_device_code(&code).is_ok());
    assert_ne!(first.finish(), 0);
    assert_eq!(first.words, 1);
    let (_, _, identity) = common::ring::identity(&bd);
    assert!(!identity["ring"].is_null());
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    // Deliberate missing-journal fault isolates the independently live runtime fence.
    // The membership came from the actual CLI/public Relay, not a seeded v2 roster.
    let journal = b.paths.ring_dir.join("standalone-adoption.json");
    let retained = b.paths.ring_dir.join("retained-obligation.json");
    std::fs::rename(&journal, &retained).unwrap();
    let keys = std::fs::read(b.paths.ring_dir.join("keys.json")).unwrap();
    if different_socket {
        // The caller chooses an unrelated private socket directory for the SAME Store.
        // The actual runtime still holds its original socket-parent lock.
        let actual_lock = b.paths.lock.clone();
        b.paths = xshelld::paths::resolve(
            b.home.path(),
            None,
            Some(&b.home.path().join("other/daemon.sock")),
        );
        assert_ne!(b.paths.lock, actual_lock);
    }
    let mut second = b.pair(&relay.url(), 2_000);
    assert_eq!(second.finish(), 1);
    assert_eq!(second.words, 0);
    assert!(!journal.exists());
    assert!(
        std::fs::read(b.paths.ring_dir.join("keys.json")).unwrap() == keys,
        "retained key material changed"
    );
    assert!(b
        .companion()
        .request(CompanionRequest::StandaloneCommandReady)
        .is_err());
    std::fs::rename(&retained, &journal).unwrap();
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}

#[test]
fn actual_cli_unavailable_running_owner_cannot_fall_back_to_disk_or_open_a_slot() {
    use std::os::unix::fs::OpenOptionsExt;
    let relay = workerd::Relay::start();
    let b = Fixture::new();
    xshelld::paths::ensure_private_dir(&b.paths.ring_dir).unwrap();
    let owner = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(b.paths.ring_dir.join("runtime-owner.lock"))
        .unwrap();
    owner.lock().unwrap();
    let mut child = b.pair(&relay.url(), 2_000);
    assert_eq!(child.finish(), 1);
    assert_eq!(child.words, 0);
    assert!(!b.paths.ring_dir.join("keys.json").exists());
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert!(!b.paths.ring_dir.join("standalone-adoption.json").exists());
}

#[test]
fn actual_cli_stalled_origin_cannot_extend_owner_monotonic_deadline_or_reopen_words() {
    let relay = workerd::Relay::start();
    let b = Fixture::new();
    let _bd = b.server();
    let br = b.ring(&relay.url());
    br.register_companion(&b.companion()).unwrap();
    let _manager = b.manager(&br);
    let began = Instant::now();
    let mut child = b.pair(&relay.url(), 2_000);
    let code = child.code();
    let join = controller(&br);
    assert_eq!(
        unsafe { libc::kill(child.child.id() as i32, libc::SIGSTOP) },
        0
    );
    // The origin keeps its connection, but cannot poll/cancel/disconnect while stopped.
    wait(|| matches!(join.join_status(), JoinStatus::Failed { .. }));
    assert!(began.elapsed() < Duration::from_secs(3));
    assert!(child.child.try_wait().unwrap().is_none());
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert_eq!(
        unsafe { libc::kill(child.child.id() as i32, libc::SIGCONT) },
        0
    );
    assert_eq!(child.finish(), 5);
    assert_eq!(child.words, 1);
    assert_no_words_on_disk(&b, &code);
    br.quit();
}

#[test]
fn protected_preparation_rejects_wrong_operation_proposal_and_foreign_key_without_mutation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
    use xshell_protocol::ring::pairing::adoption::JoinTarget;
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let (reached, receive) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    let proceed = std::sync::Mutex::new(proceed);
    let first = AtomicBool::new(true);
    let mut config = Config::new(b.home.path().into(), b.paths.clone());
    config.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
        if point == xshelld::server::TestPoint::StandaloneCommandBeforePrepare
            && first.swap(false, Ordering::AcqRel)
        {
            reached.send(()).unwrap();
            proceed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        false
    })));
    let _bd = Server::start(config).unwrap();
    let mut child = b.pair(&relay.url(), 30_000);
    let code = child.code();
    let mut failures = Vec::new();
    std::thread::scope(|scope| {
        let approval = scope.spawn(|| ar.approve_device_code(&code));
        receive.recv_timeout(Duration::from_secs(8)).unwrap();
        let outgoing = Store::new(a.desktop.clone())
            .load_profile()
            .unwrap()
            .0
            .unwrap()
            .outgoing
            .unwrap();
        let intent = outgoing.intent;
        let proposal = CompanionProposal {
            operation: intent.operation.clone(),
            predecessor: outgoing.predecessor,
            proposal: outgoing.proposal,
            transcript: ConsentTranscript::Pairing {
                final_noise_hash: outgoing.final_noise_hash.unwrap(),
            },
        };
        let keys = std::fs::read(b.paths.ring_dir.join("keys.json")).unwrap();
        let mut wrong_operation = intent.clone();
        wrong_operation.operation = xshell_protocol::ring::b64::encode(&[42; 32]);
        let mut wrong_envelope = proposal.clone();
        wrong_envelope.operation = wrong_operation.operation.clone();
        let mut wrong_proposal = proposal.clone();
        wrong_proposal.proposal.relay_url = "ws://127.0.0.1:9".into();
        let mut foreign = intent.clone();
        let other = xshell_protocol::ring::DeviceKeys::generate().unwrap();
        if let JoinTarget::StandaloneDaemon { daemon } = &mut foreign.target {
            daemon.sign_key = other.sign_key();
            daemon.noise_key = other.noise_key();
        } else {
            panic!("real CLI was not standalone");
        }
        for (intent, proposal) in [
            (wrong_operation, wrong_envelope),
            (intent.clone(), wrong_proposal),
            (foreign, proposal),
        ] {
            failures.push(
                b.companion()
                    .request(CompanionRequest::PrepareStandaloneCommand {
                        intent,
                        proposal: Box::new(proposal),
                    })
                    .is_err(),
            );
        }
        let unchanged = !b.paths.ring_dir.join("standalone-adoption.json").exists()
            && !b.paths.ring_dir.join("roster.json").exists()
            && std::fs::read(b.paths.ring_dir.join("keys.json")).unwrap() == keys;
        release.send(()).unwrap();
        let approved = approval.join().unwrap();
        // Always drain the genuine exchange before asserting the negative matrix.
        let exit = child.finish();
        assert!(
            failures.iter().all(|refused| *refused),
            "protected preparation accepted a changed operation/proposal/key"
        );
        assert!(unchanged, "rejected preparation mutated protected stores");
        assert!(approved.is_ok());
        assert_eq!(exit, 0);
    });
    assert_eq!(child.words, 1);
    assert_no_words_on_disk(&b, &code);
    let journal_path = b.paths.ring_dir.join("standalone-adoption.json");
    let installed = std::fs::read(&journal_path).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&installed).unwrap();
    let intent: xshell_protocol::ring::pairing::adoption::AdoptionIntent =
        serde_json::from_value(json["intent"].clone()).unwrap();
    let proposal: CompanionProposal = serde_json::from_value(json["proposal"].clone()).unwrap();
    assert!(b
        .companion()
        .request(CompanionRequest::PrepareStandaloneCommand {
            intent: intent.clone(),
            proposal: Box::new(proposal.clone())
        })
        .is_err());
    let chain = b.chain();
    let mut prefix: Vec<String> = chain.versions().iter().map(|r| r.token().into()).collect();
    prefix.pop();
    let operation = json["intent"]["operation"].as_str().unwrap().to_owned();
    assert!(b
        .companion()
        .request(CompanionRequest::InstallStandaloneAccepted {
            operation,
            rosters: prefix
        })
        .is_err());
    assert_eq!(std::fs::read(journal_path).unwrap(), installed);
    assert_eq!(b.chain(), chain);
    // The actual registration-only child must refuse the new preparation/handoff
    // requests too; none may expand Descriptor/Register into a signing/runtime service.
    let c = Fixture::new();
    let cr = c.ring(&relay.url());
    let local = xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin: env!("CARGO_BIN_EXE_xshelld").into(),
        home: c.home.path().into(),
        socket: c.paths.socket.clone(),
        env: vec![],
        version: "1.5.0".into(),
        build: xshelld::build_id::BUILD.clone(),
    })
    .unwrap();
    local
        .with_registration_endpoint(&c.desktop, |client| {
            cr.register_companion(client)?;
            let keys = std::fs::read(c.paths.ring_dir.join("keys.json")).unwrap();
            assert!(client
                .request(CompanionRequest::StandaloneCommandReady)
                .is_err());
            assert!(client
                .request(CompanionRequest::PrepareStandaloneCommand {
                    intent: intent.clone(),
                    proposal: Box::new(proposal.clone())
                })
                .is_err());
            assert!(client
                .request(CompanionRequest::InstallStandaloneAccepted {
                    operation: intent.operation.clone(),
                    rosters: chain.versions().iter().map(|r| r.token().into()).collect()
                })
                .is_err());
            assert!(client
                .request(CompanionRequest::Possession {
                    operation: intent.operation.clone(),
                    handshake_hash: match &proposal.transcript {
                        ConsentTranscript::Pairing { final_noise_hash } => final_noise_hash.clone(),
                        _ => unreachable!(),
                    }
                })
                .is_err());
            assert!(
                std::fs::read(c.paths.ring_dir.join("keys.json")).unwrap() == keys,
                "retained key material changed"
            );
            assert!(!c.paths.ring_dir.join("standalone-adoption.json").exists());
            assert!(!c.paths.ring_dir.join("roster.json").exists());
            let policy: serde_json::Value = serde_json::from_slice(
                &std::fs::read(c.paths.ring_dir.join("companion.json")).unwrap(),
            )
            .unwrap();
            assert!(policy["obligation"].is_null());
            let mut forged = serde_json::to_value(CompanionRequest::PrepareStandaloneCommand {
                intent: intent.clone(),
                proposal: Box::new(proposal.clone()),
            })
            .unwrap();
            forged["origin"] = serde_json::json!("standalone-command");
            assert!(serde_json::from_value::<CompanionRequest>(forged).is_err());
            Ok(())
        })
        .unwrap();
    cr.quit();
    ar.quit();
}

#[test]
fn lost_preparation_reply_preserves_uncertainty_without_consent_restart_or_new_offer() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let proofs = Arc::new(AtomicUsize::new(0));
    let observed = proofs.clone();
    let ar = a.ring_with_hook(
        &relay.url(),
        Some(AdoptionTestHook(Arc::new(move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived {
                observed.fetch_add(1, Ordering::AcqRel);
            }
        }))),
    );
    create(&ar, &a.companion());
    let before = ar.chain().unwrap();
    let b = Fixture::new();
    let (reached, receive) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    let proceed = std::sync::Mutex::new(proceed);
    let first = AtomicBool::new(true);
    let mut config = Config::new(b.home.path().into(), b.paths.clone());
    config.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
        if point == xshelld::server::TestPoint::StandaloneCommandPreparedBeforeReply
            && first.swap(false, Ordering::AcqRel)
        {
            reached.send(()).unwrap();
            proceed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        false
    })));
    let bd = Server::start(config).unwrap();
    let mut child = b.pair(&relay.url(), 4_000);
    let code = child.code();
    let journal_path = b.paths.ring_dir.join("standalone-adoption.json");
    let retained = std::thread::scope(|scope| {
        let approval = scope.spawn(|| ar.approve_device_code(&code));
        receive.recv_timeout(Duration::from_secs(8)).unwrap();
        let retained = std::fs::read(&journal_path).unwrap();
        // No positive preparation ACK can arrive while the actual handler is paused.
        // The ORIGINAL command deadline expires; the client closes its request socket.
        assert_eq!(child.finish(), 1);
        assert_eq!(child.words, 1);
        release.send(()).unwrap();
        assert!(approval.join().unwrap().is_err());
        retained
    });
    assert_eq!(
        proofs.load(Ordering::Acquire),
        0,
        "no enrollment consent reached the real approver"
    );
    assert_eq!(ar.chain().unwrap(), before);
    let json: serde_json::Value = serde_json::from_slice(&retained).unwrap();
    assert_eq!(json["consentMayHaveLeft"], true);
    assert_eq!(json["installed"], false);
    assert!(json["accepted"].as_array().unwrap().is_empty());
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert_eq!(std::fs::read(&journal_path).unwrap(), retained);
    assert!(b
        .companion()
        .request(CompanionRequest::StandaloneCommandReady)
        .is_err());
    bd.shutdown();
    let restarted = b.server();
    assert_eq!(
        restarted
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        xshelld::server::adoption::StandaloneRecoveryState::ReconciliationRequired
    );
    let mut later = b.pair(&relay.url(), 2_000);
    assert_eq!(later.finish(), 1);
    assert_eq!(later.words, 0);
    assert_eq!(std::fs::read(journal_path).unwrap(), retained);
    assert_eq!(proofs.load(Ordering::Acquire), 0);
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}

#[path = "common/admission_contention.rs"]
mod admission_contention;

#[test]
fn offline_pair_lease_before_keys_excludes_start_and_second_pair_then_crash_releases() {
    let relay = workerd::Relay::start();
    let b = Fixture::new();
    let cut = b.home.path().join("before-keys");
    let mut child = b.pair_with_env(
        &relay.url(),
        30_000,
        &[("XSHELLD_TEST_OFFLINE_OWNER_BEFORE_KEYS", &cut)],
    );
    wait(|| cut.exists());
    assert!(!b.paths.ring_dir.join("keys.json").exists());
    let other = xshelld::paths::resolve(
        b.home.path(),
        None,
        Some(&b.home.path().join("other/daemon.sock")),
    );
    assert!(matches!(
        Server::start(Config::new(b.home.path().into(), other)),
        Err(xshelld::server::StartError::AlreadyRunning)
    ));
    let mut second = b.pair(&relay.url(), 2_000);
    assert_eq!(second.finish(), 1);
    assert_eq!(second.words, 0);
    assert!(!b.paths.ring_dir.join("keys.json").exists());
    child.child.kill().unwrap();
    assert!(!child.child.wait().unwrap().success()); // exact owned process natively reaped
    let successor = b.server();
    assert!(!b.paths.ring_dir.join("keys.json").exists());
    successor.shutdown();
}

#[test]
fn offline_pair_before_accepted_commit_excludes_start_and_second_pair_then_releases_on_success() {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let cut = b.home.path().join("before-install");
    let mut child = b.pair_with_env(
        &relay.url(),
        30_000,
        &[("XSHELLD_TEST_OFFLINE_OWNER_BEFORE_INSTALL", &cut)],
    );
    let code = child.code();
    ar.approve_device_code(&code).unwrap();
    wait(|| cut.exists());
    assert!(matches!(
        Server::start(Config::new(b.home.path().into(), b.paths.clone())),
        Err(xshelld::server::StartError::AlreadyRunning)
    ));
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    let mut second = b.pair(&relay.url(), 2_000);
    assert_eq!(second.finish(), 1);
    assert_eq!(second.words, 0);
    std::fs::remove_file(cut).unwrap();
    assert_eq!(child.finish(), 0);
    assert_eq!(child.words, 1);
    let successor = b.server();
    let (_, _, identity) = common::ring::identity(&successor);
    assert_eq!(identity["ring"]["version"], b.chain().head().version());
    successor.shutdown();
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}

#[test]
fn offline_pair_process_death_after_acceptance_keeps_exact_obligation_for_explicit_recovery() {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let cut = b.home.path().join("accepted-before-install");
    let mut child = b.pair_with_env(
        &relay.url(),
        30_000,
        &[("XSHELLD_TEST_OFFLINE_OWNER_BEFORE_INSTALL", &cut)],
    );
    let code = child.code();
    ar.approve_device_code(&code).unwrap();
    wait(|| cut.exists());
    let path = b.paths.ring_dir.join("standalone-adoption.json");
    let before: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(before["installed"], false);
    assert!(!before["accepted"].as_array().unwrap().is_empty());
    child.child.kill().unwrap();
    assert!(!child.child.wait().unwrap().success());
    let successor = b.server();
    assert_eq!(
        successor
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        xshelld::server::adoption::StandaloneRecoveryState::AcceptedInstalling
    );
    assert_eq!(
        successor.reconcile_standalone_adoption().unwrap().state,
        xshelld::server::adoption::StandaloneRecoveryState::Installed
    );
    let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(after["accepted"], before["accepted"]);
    assert_eq!(after["intent"], before["intent"]);
    assert_eq!(after["installed"], true);
    successor.shutdown();
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}

#[test]
fn in_process_adoption_shutdown_cancels_network_but_retains_store_through_inline_words_callback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let relay = workerd::Relay::start();
    let b = Fixture::new();
    let daemon = b.server();
    let stopper = daemon.stopper();
    let cancel = Arc::new(AtomicBool::new(false));
    let (at, reached) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let url = relay.url();
    std::thread::scope(|scope| {
        let daemon_ref = &daemon;
        let cancel_ref = &cancel;
        let url_ref = &url;
        let guest = scope.spawn(move || {
            daemon_ref.adopt_standalone(
                url_ref,
                Duration::from_secs(30),
                cancel_ref,
                |_| {
                    at.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(8)).unwrap();
                },
                |_, _| panic!("cancelled pre-proof invocation cannot request consent"),
            )
        });
        reached.recv_timeout(Duration::from_secs(8)).unwrap();
        stopper.shutdown();
        assert!(cancel.load(Ordering::Acquire));
        assert!(matches!(
            Server::start(Config::new(b.home.path().into(), b.paths.clone())),
            Err(xshelld::server::StartError::AlreadyRunning)
        ));
        assert!(daemon
            .adopt_standalone(
                &url,
                Duration::from_secs(30),
                &Arc::new(AtomicBool::new(false)),
                |_| panic!("late admission cannot emit words"),
                |_, _| panic!("late consent")
            )
            .is_err());
        release.send(()).unwrap();
        assert!(guest.join().unwrap().is_err());
    });
    let successor = b.server();
    successor.shutdown();
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert!(!b.paths.ring_dir.join("standalone-adoption.json").exists());
}

#[test]
fn offline_pair_output_failure_releases_ownership_without_starting_serve() {
    let relay = workerd::Relay::start();
    let b = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
        .arg("pair")
        .args(["--relay", &relay.url()])
        .arg("--home")
        .arg(b.home.path())
        .arg("--socket")
        .arg(&b.paths.socket)
        .env("XSHELLD_PAIR_TTL_MS", "5000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    wait(|| child.try_wait().unwrap().is_some());
    assert!(!child.wait().unwrap().success());
    assert!(!b.paths.socket.exists());
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert!(!b.paths.ring_dir.join("standalone-adoption.json").exists());
    b.server().shutdown();
}

#[test]
fn offline_pair_final_receipt_failure_is_nonzero_and_exact_accepted_recovery_remains() {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let cut = b.home.path().join("installed-before-receipt");
    let mut child = b.pair_with_env(
        &relay.url(),
        30_000,
        &[("XSHELLD_TEST_OFFLINE_OWNER_BEFORE_RECEIPT", &cut)],
    );
    let code = child.code();
    ar.approve_device_code(&code).unwrap();
    wait(|| cut.exists());
    let journal = b.paths.ring_dir.join("standalone-adoption.json");
    let retained = b.paths.ring_dir.join("accepted-retained");
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    assert_eq!(original["installed"], false);
    let accepted = b.chain();
    std::fs::rename(&journal, &retained).unwrap();
    std::fs::create_dir(&journal).unwrap();
    std::fs::remove_file(cut).unwrap();
    assert_ne!(child.finish(), 0);
    assert_eq!(child.words, 1);
    assert_eq!(b.chain(), accepted);
    std::fs::remove_dir(&journal).unwrap();
    std::fs::rename(&retained, &journal).unwrap();
    let successor = b.server();
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use xshell_protocol::frame::{read_frame, Frame, FrameError, MAX_FRAME_LEN};
    use xshell_protocol::msg::{decode_server, encode_msg, ClientMsg, Hello, Outcome, ServerMsg};

    // Keep decoding and failures local: common::Client can dump retained identity payloads.
    struct IdentityRead<'a> {
        stream: &'a mut UnixStream,
        deadline: Instant,
    }
    impl Read for IdentityRead<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.stream.set_read_timeout(Some(remaining))?;
            self.stream.read(bytes)
        }
    }
    fn identity_message(stream: &mut UnixStream) -> ServerMsg {
        let mut reader = IdentityRead {
            stream,
            deadline: Instant::now() + common::T,
        };
        let bytes = match read_frame(&mut reader, MAX_FRAME_LEN) {
            Ok(Some(Frame::Json(bytes))) => bytes,
            Ok(None) => panic!("control identity EOF"),
            Ok(Some(_)) => panic!("control identity unexpected frame"),
            Err(FrameError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                panic!("control identity timeout")
            }
            Err(_) => panic!("control identity frame read failed"),
        };
        decode_server(&bytes).unwrap_or_else(|_| panic!("control identity decode failed"))
    }
    fn identity_send(stream: &mut UnixStream, message: &ClientMsg, id: Option<u64>) {
        let frame =
            encode_msg(message, id).unwrap_or_else(|_| panic!("control identity encode failed"));
        stream
            .write_all(&frame)
            .and_then(|_| stream.flush())
            .unwrap_or_else(|_| panic!("control identity write failed"));
    }
    let open_identity = |socket: &std::path::Path| {
        let deadline = Instant::now() + common::T;
        let mut stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() >= deadline => panic!("control identity connect failed"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        identity_send(
            &mut stream,
            &ClientMsg::Hello(Hello {
                protocol: common::range(1, 1),
                version: "test".into(),
                capabilities: vec!["call".into(), "term".into()],
                build: None,
            }),
            None,
        );
        assert!(
            matches!(identity_message(&mut stream), ServerMsg::Hello(_)),
            "control identity expected hello"
        );
        assert!(
            matches!(identity_message(&mut stream), ServerMsg::Terminals { .. }),
            "control identity expected terminals"
        );
        let mut request_id = 1000_u64;
        move || {
            request_id += 1;
            identity_send(&mut stream, &ClientMsg::RingIdentity, Some(request_id));
            match identity_message(&mut stream) {
                ServerMsg::Res(response) if response.id == request_id => match response.outcome {
                    Outcome::Ok { ok }
                        if ok.is_object()
                            && ok["ring"].is_object()
                            && ok["ring"]["ringId"].is_string()
                            && ok["ring"]["version"].is_u64()
                            && ok["ring"]["state"].is_string() =>
                    {
                        ok
                    }
                    Outcome::Ok { .. } => panic!("control identity invalid result"),
                    Outcome::Err { .. } => panic!("control identity response error"),
                },
                _ => panic!("control identity unexpected response"),
            }
        }
    };

    let mut observe = open_identity(&successor.socket);
    wait(|| {
        let identity = observe();
        !identity["ring"].is_null()
            && identity["ring"]["ringId"] == accepted.ring_id().as_str()
            && identity["ring"]["version"] == accepted.head().version()
            && identity["ring"]["state"] == "connected"
    });
    drop(observe);
    assert_eq!(
        successor.reconcile_standalone_adoption().unwrap().state,
        xshelld::server::adoption::StandaloneRecoveryState::Installed
    );
    let recovered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    assert_eq!(recovered["accepted"], original["accepted"]);
    assert_eq!(recovered["installed"], true);
    successor.shutdown();
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}

#![cfg(unix)]
//! Actual Unix CLI + unchanged production workerd. Computer confirmations use private
//! Rust backend leases, NOT native GUI/window-consent evidence. No v2 roster seeding.
//! Run with TMPDIR on SSD or RAM, not an HDD: under default test parallelism the fsync-heavy
//! stores on an HDD push Device-code readiness past the short test TTLs (2-4 s).
mod common;
#[path = "desktop/fault_proxy.rs"]
mod fault_proxy;
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
    fn server_with_hook(&self, hook: xshelld::server::TestHook) -> ServerHandle {
        let mut config = Config::new(self.home.path().into(), self.paths.clone());
        config.test_hook = Some(hook);
        Server::start(config).unwrap()
    }
    /// A Daemon whose automatic standalone step is deferred while `defer` is set (as if a
    /// pair command still owned the obligation), so a test can look at the obligation as
    /// the command left it before the Daemon finishes it.
    fn server_deferring(&self, defer: &Arc<std::sync::atomic::AtomicBool>) -> ServerHandle {
        let defer = defer.clone();
        self.server_with_hook(xshelld::server::TestHook(Arc::new(move |_, point| {
            point == xshelld::server::TestPoint::StandaloneMaintenanceActs
                && defer.load(std::sync::atomic::Ordering::Acquire)
        })))
    }
    fn journal(&self) -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(self.paths.ring_dir.join("standalone-adoption.json")).unwrap(),
        )
        .unwrap()
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
            seen: Vec::new(),
            words: 0,
            stderr,
            stderr_reader: Some(stderr_reader),
            started: Instant::now(),
        }
    }
    fn companion(&self) -> CompanionClient {
        CompanionClient::for_store(&self.paths.ring_dir).unwrap()
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
    /// Every stdout line read so far (by `code` and `finish`).
    seen: Vec<String>,
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
            self.seen.push(line.clone());
            if let Ok(code) = DeviceCode::parse(&line) {
                self.words += 1;
                return code;
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
            if DeviceCode::parse(&line).is_ok() {
                self.words += 1;
            }
            self.seen.push(line);
        }
        code
    }
    fn cancel(&self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) },
            0
        );
    }
    /// Reads stdout until `line` arrives (kept in `seen`), at most `within`.
    fn wait_line(&mut self, line: &str, within: Duration) {
        let until = Instant::now() + within;
        while !self.seen.iter().any(|l| l == line) {
            let next = self
                .lines
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| panic!("no {line:?} ({e:?}); seen {:?}", self.seen));
            if DeviceCode::parse(&next).is_ok() {
                self.words += 1;
            }
            self.seen.push(next);
        }
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
/// There is no confirmation on the joining machine any more: the Join continues by itself
/// once the approver's proposal verifies. Waits until it has left `Waiting`.
fn confirm_backend(join: &JoinOffer) {
    let until = std::time::Instant::now() + Duration::from_secs(8);
    while matches!(join.join_status(), JoinStatus::Waiting) {
        assert!(
            std::time::Instant::now() < until,
            "Join never continued by itself"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
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
    assert_eq!(line, "Waiting for another device to add this machine...");
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
        // The approved copy (cli.pair.*), the words on a line of their own.
        assert_eq!(
            child.seen,
            [
                code.format().as_str(),
                format!("xshell {} on this machine", env!("CARGO_PKG_VERSION")).as_str(),
                "Type these eight words on a machine or phone already in your Ring. On a machine, open Settings > Ring > Add a machine.",
                "These words work once, for 10 minutes, while this command is running.",
                "Waiting for another device to add this machine...",
                "Added this machine to your Ring. Your Ring has 2 machines.",
            ]
        );
        // Again on a machine already in the Ring: no words, success.
        let mut again = b.pair(&relay.url(), 30_000);
        assert_eq!(again.finish(), 0);
        assert_eq!(again.words, 0);
        assert_eq!(
            again.seen,
            ["This machine is already in your Ring. Your Ring has 2 machines."]
        );
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
    assert!(matches!(join.join_status(), JoinStatus::Finishing { .. }));
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
    assert!(
        child.lines.recv_timeout(Duration::from_secs(3)).unwrap()
            == "Waiting for another device to add this machine..."
    );
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
        // The Daemon now finishes the obligation as soon as the command exits: defer that
        // until the test has looked at what the command left and run explicit recovery.
        let defer = Arc::new(std::sync::atomic::AtomicBool::new(true));
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
                    daemon = Some(b.server_deferring(&defer));
                } // not this driver's pinned route
            } else {
                std::fs::create_dir(b.paths.ring_dir.join("roster.json")).unwrap();
            }
            release.send(()).unwrap();
            assert!(approval.join().unwrap().is_ok());
        });
        assert_eq!(child.finish(), xshelld::pair::UNFINISHED_EXIT);
        assert_eq!(child.words, 1);
        let journal: serde_json::Value = serde_json::from_slice(
            &std::fs::read(b.paths.ring_dir.join("standalone-adoption.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(journal["installed"], false);
        assert!(!journal["accepted"].as_array().unwrap().is_empty());
        if owner_loss {
            if !replace {
                daemon = Some(b.server_deferring(&defer));
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
        // The obligation as the command left it is the subject: the automatic step (which
        // may abandon it once its 4 s code expired) is deferred on both Daemons.
        if point == xshelld::server::TestPoint::StandaloneMaintenanceActs {
            return true;
        }
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
    let restarted = b.server_deferring(&Arc::new(AtomicBool::new(true)));
    assert_eq!(
        restarted
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        xshelld::server::adoption::StandaloneRecoveryState::ReconciliationRequired
    );
    // #110: a later pair waits briefly for the running Daemon (deferred here), then says
    // the earlier attempt is still unresolved (its code has expired): exit 6, no offer.
    let wait_1s = ms("1000");
    let mut later = b.pair_with_env(&relay.url(), 2_000, &earlier_wait(&wait_1s));
    assert_eq!(later.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert_eq!(later.words, 0);
    let stderr = later.stderr();
    assert_eq!(stderr.trim_end(), UNFINISHED_RUNNING_LINE, "{stderr}");
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
    let stderr = second.stderr();
    assert_eq!(stderr.trim_end(), BUSY_LINE, "{stderr}");
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
    // #110: the earlier attempt is unresolved and no Daemon runs to finish it (the first
    // command still owns it): exit 6 with that copy, not a raw refusal.
    let wait_1s = ms("1000");
    let mut second = b.pair_with_env(&relay.url(), 2_000, &earlier_wait(&wait_1s));
    assert_eq!(second.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert_eq!(second.words, 0);
    let stderr = second.stderr();
    assert_eq!(stderr.trim_end(), EARLIER_STOPPED_LINE, "{stderr}");
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
    // Explicit recovery is the subject; the automatic step (covered by
    // actual_cli_killed_after_consent_offline_then_daemon_start_installs_within_seconds) is
    // deferred so it cannot race it.
    let successor = b.server_deferring(&Arc::new(std::sync::atomic::AtomicBool::new(true)));
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
    assert_eq!(child.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert_eq!(child.words, 1);
    assert_eq!(b.chain(), accepted);
    std::fs::remove_dir(&journal).unwrap();
    std::fs::rename(&retained, &journal).unwrap();
    // Explicit recovery is the subject: defer the automatic step so it cannot race it.
    let successor = b.server_deferring(&Arc::new(std::sync::atomic::AtomicBool::new(true)));
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

/// Pauses a Join at [`AdoptionTestPoint::JoinBeforeConsent`] (the approver's proposal is
/// verified, nothing is signed yet) until released.
#[derive(Clone, Default)]
struct ConsentGate(Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>);
impl ConsentGate {
    fn hook(&self) -> AdoptionTestHook {
        let gate = self.clone();
        AdoptionTestHook(Arc::new(move |point| {
            if point == AdoptionTestPoint::JoinBeforeConsent {
                let (lock, cv) = &*gate.0;
                let mut st = lock.lock().unwrap();
                st.0 = true;
                cv.notify_all();
                while !st.1 {
                    st = cv.wait(st).unwrap();
                }
            }
        }))
    }
    fn reached(&self) {
        let (lock, cv) = &*self.0;
        let mut st = lock.lock().unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        while !st.0 {
            assert!(Instant::now() < until, "the proposal never arrived");
            st = cv.wait_timeout(st, Duration::from_millis(50)).unwrap().0;
        }
    }
    fn release(&self) {
        let (lock, cv) = &*self.0;
        lock.lock().unwrap().1 = true;
        cv.notify_all();
    }
}

/// What ends the code's display while the approver's proposal arrives (B7).
#[derive(Clone, Copy, Debug)]
enum CodeGone {
    /// The code screen closes (`ring_join_cancel`).
    Cancelled,
    /// The `xshelld pair` command that shows the code exits.
    PairExited,
    /// Another xshell window takes the Ring over (this one closed).
    OtherWindow,
    /// The code expires.
    Expired,
}

/// B7: the approver's proposal arrives just as the code stops being shown. The joining
/// machine releases no proof (its journal never reaches "consent may have left", its
/// Daemon holds none) and is not enrolled; the Ring is unchanged.
fn no_proofs_after_the_code_is_gone(gone: CodeGone) {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    wait(|| ar.view().connection == "connected");
    let b = Fixture::new();
    let _bd = b.server();
    let gate = ConsentGate::default();
    let br = {
        let mut cfg = DesktopRingConfig::new(b.desktop.clone(), "Computer".into());
        cfg.default_relay_url = relay.url();
        cfg.owner_retry = Duration::from_millis(20);
        cfg.adoption_test_hook = Some(gate.hook());
        if matches!(gone, CodeGone::Expired) {
            cfg.pair_ttl = Duration::from_secs(3);
        }
        DesktopRing::open(cfg, Arc::new(Quiet))
    };
    br.register_companion(&b.companion()).unwrap();
    let _manager = b.manager(&br);
    let mut child = None;
    let (offer, join, code) = if matches!(gone, CodeGone::PairExited) {
        let mut c = b.pair(&relay.url(), 30_000);
        let code = c.code();
        child = Some(c);
        (None, Some(controller(&br)), code)
    } else {
        let offer = br.begin_join(&b.companion()).unwrap();
        let code = offer.code.clone();
        (Some(offer), None, code)
    };
    let expires_at = offer
        .as_ref()
        .map_or_else(|| join.as_ref().unwrap().expires_at, |o| o.expires_at);
    let mut other = None;
    std::thread::scope(|scope| {
        let approval = scope.spawn(|| ar.approve_device_code(&code));
        gate.reached();
        match gone {
            CodeGone::Cancelled => offer.as_ref().unwrap().cancel_join(),
            CodeGone::PairExited => {
                let c = child.as_mut().unwrap();
                c.cancel();
                assert_ne!(c.finish(), 0);
                // The Desktop dropped the command's operation.
                wait(|| br.administration_joins().is_empty());
            }
            CodeGone::OtherWindow => {
                br.quit();
                let next = b.ring(&relay.url());
                wait(|| next.pairing_ipc_owner());
                other = Some(next);
            }
            CodeGone::Expired => {
                let until = Instant::now() + Duration::from_secs(10);
                while unix_now() < expires_at {
                    assert!(Instant::now() < until);
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        gate.release();
        assert!(approval.join().unwrap().is_err(), "nothing to publish");
        match offer {
            Some(offer) => assert!(offer.wait().is_err()),
            None => wait(|| {
                matches!(
                    join.as_ref().unwrap().join_status(),
                    JoinStatus::Failed { .. } | JoinStatus::Finishing { .. }
                )
            }),
        }
    });
    let stored = Store::new(b.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert!(stored.membership.is_none(), "{gone:?}: not enrolled");
    let journal = stored.journal.unwrap();
    assert!(
        matches!(
            journal.phase,
            AdoptionPhase::Cancelled | AdoptionPhase::ExpiredBeforeProof
        ),
        "{gone:?}: no proof may have left, got {:?}",
        journal.phase
    );
    assert!(journal.signed_candidate.is_none() && journal.accepted.is_empty());
    let counterpart: serde_json::Value =
        serde_json::from_slice(&std::fs::read(b.paths.ring_dir.join("companion.json")).unwrap())
            .unwrap();
    assert!(
        counterpart["obligation"]["proofs"].is_null(),
        "{gone:?}: the Daemon released no proof"
    );
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    let ring = ar.chain().unwrap();
    assert_eq!(ring.head().version(), 1, "{gone:?}: the Ring is unchanged");
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), ring.ring_id()),
        1
    );
    assert_no_words_on_disk(&b, &code);
    drop(join);
    if let Some(other) = other {
        other.quit();
    }
    br.quit();
    ar.quit();
}
#[test]
fn no_proofs_when_the_code_screen_closes_as_the_proposal_arrives() {
    no_proofs_after_the_code_is_gone(CodeGone::Cancelled);
}
#[test]
fn no_proofs_when_the_pair_command_exits_as_the_proposal_arrives() {
    no_proofs_after_the_code_is_gone(CodeGone::PairExited);
}
#[test]
fn no_proofs_when_another_window_takes_over_as_the_proposal_arrives() {
    no_proofs_after_the_code_is_gone(CodeGone::OtherWindow);
}
#[test]
fn no_proofs_when_the_code_expires_as_the_proposal_arrives() {
    no_proofs_after_the_code_is_gone(CodeGone::Expired);
}

// #109: an interrupted standalone join finishes at once. Every Relay, Ring and `--relay` URL
// below is the fault proxy's (or the local workerd's); never a hosted Relay.

const RECONNECTING_LINE: &str = "Connection to the service was lost; this command is still trying. Do not enter the words again or start over.";
const UNFINISHED_RUNNING_LINE: &str = "Pairing isn't confirmed; this machine keeps checking and joins when it reaches the service. No action needed; watch for it in Settings > Ring on another device.";
const UNFINISHED_STOPPED_LINE: &str = "Pairing isn't confirmed; xshell finishes it when it next runs in the background here (xshelld serve or a terminal connects). Do not run xshelld pair again first.";
const UNFINISHED_CANCELLED_LINE: &str = "Stopping does not undo approval; this machine may still join your Ring. If you don't want it to, remove it in Settings > Ring on another device.";

fn wait_for(what: &str, within: Duration, mut test: impl FnMut() -> bool) {
    let until = Instant::now() + within;
    while !test() {
        assert!(Instant::now() < until, "{what} within {within:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// The obligation is installed: receipt written and `roster.json` lists this Daemon.
fn standalone_installed(b: &Fixture) -> bool {
    b.paths.ring_dir.join("roster.json").is_file()
        && std::fs::read(b.paths.ring_dir.join("standalone-adoption.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|j| j["installed"] == true)
}
/// A Daemon fixture's standalone status, through its handle.
fn status_installed(daemon: &ServerHandle) -> bool {
    daemon
        .standalone_adoption_status()
        .ok()
        .flatten()
        .is_some_and(|s| s.state == xshelld::server::adoption::StandaloneRecoveryState::Installed)
}
/// An approver (a created Ring) whose test points run `on_point`.
fn approver(
    a: &Fixture,
    relay: &str,
    on_point: impl Fn(AdoptionTestPoint) + Send + Sync + 'static,
) -> Arc<DesktopRing> {
    let ring = a.ring_with_hook(relay, Some(AdoptionTestHook(Arc::new(on_point))));
    create(&ring, &a.companion());
    ring
}

fn relay_drop_after_consent(running: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let url = proxy.url();
    let a = Fixture::new();
    let _ad = a.server();
    let b = Fixture::new();
    let journal_path = b.paths.ring_dir.join("standalone-adoption.json");
    let proofs = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
    let ar = {
        let (proxy, proofs, captured, path) = (
            proxy.clone(),
            proofs.clone(),
            captured.clone(),
            journal_path.clone(),
        );
        approver(&a, &url, move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived
                && proofs.fetch_add(1, Ordering::SeqCst) == 0
            {
                *captured.lock().unwrap() =
                    Some(serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap());
                // The pairing pipe and proposal reader die; the approver's Ring
                // connection (its publication) stays.
                proxy.cut_non_ring();
            }
        })
    };
    let _bd = running.then(|| b.server());
    let mut child = b.pair(&url, 120_000);
    let code = child.code();
    let started = Instant::now();
    let approved = ar.approve_device_code(&code);
    let exit = child.finish();
    let elapsed = started.elapsed();
    assert_eq!(exit, 0, "running {running}; stderr:\n{}", child.stderr());
    assert!(approved.is_ok(), "{approved:?}");
    assert!(elapsed < Duration::from_secs(15), "took {elapsed:?}");
    assert_eq!(child.words, 1);
    assert_eq!(
        child
            .seen
            .iter()
            .filter(|l| *l == RECONNECTING_LINE)
            .count(),
        1,
        "{:?}",
        child.seen
    );
    assert_eq!(
        child.seen.last().unwrap(),
        "Added this machine to your Ring. Your Ring has 2 machines."
    );
    assert_eq!(proofs.load(Ordering::SeqCst), 1, "signed and sent once");
    let before = captured.lock().unwrap().clone().unwrap();
    let after = b.journal();
    assert_eq!(after["installed"], true);
    assert_eq!(
        after["intent"], before["intent"],
        "no new offer or lifetime"
    );
    assert_eq!(after["intent"]["expiresAt"], before["intent"]["expiresAt"]);
    assert_eq!(after["proposal"], before["proposal"], "no new proposal");
    let chain = b.chain();
    assert_eq!(
        after["accepted"],
        serde_json::json!(chain
            .versions()
            .iter()
            .map(|v| v.token())
            .collect::<Vec<_>>())
    );
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}
#[test]
fn actual_cli_relay_drop_after_consent_installs_without_waiting_for_expiry() {
    relay_drop_after_consent(true);
}
#[test]
fn actual_cli_relay_drop_after_consent_offline_installs_without_waiting_for_expiry() {
    relay_drop_after_consent(false);
}

fn killed_after_consent(running: bool) {
    use std::sync::atomic::{AtomicI32, Ordering};
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let b = Fixture::new();
    let pid = Arc::new(AtomicI32::new(0));
    let killed = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let ar = {
        let (pid, killed) = (pid.clone(), killed.clone());
        approver(&a, &relay.url(), move |point| {
            let mut killed = killed.lock().unwrap();
            if point == AdoptionTestPoint::ApprovalProofsReceived && killed.is_none() {
                assert_eq!(
                    unsafe { libc::kill(pid.load(Ordering::SeqCst), libc::SIGKILL) },
                    0
                );
                *killed = Some(Instant::now());
            }
        })
    };
    let mut daemon = running.then(|| b.server());
    let mut child = b.pair(&relay.url(), 300_000);
    pid.store(child.child.id() as i32, Ordering::SeqCst);
    let code = child.code();
    let _ = ar.approve_device_code(&code);
    assert!(!child.child.wait().unwrap().success());
    let killed = killed.lock().unwrap().unwrap();
    let expires_at = b.journal()["intent"]["expiresAt"].as_u64().unwrap();
    let from = if running {
        killed
    } else {
        // The Offline route: the obligation waits for the Daemon's next start.
        assert!(!standalone_installed(&b));
        daemon = Some(b.server());
        Instant::now()
    };
    let daemon = daemon.unwrap();
    wait_for(
        "the Daemon installs the obligation",
        Duration::from_secs(10),
        || status_installed(&daemon) && standalone_installed(&b),
    );
    assert!(from.elapsed() < Duration::from_secs(10));
    assert!(
        unix_now() + 200 < expires_at,
        "well before the code expires"
    );
    let (_, _, identity) = common::ring::identity(&daemon);
    assert_eq!(identity["ring"]["version"], b.chain().head().version());
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}
#[test]
fn actual_cli_killed_after_consent_running_daemon_reconciles_within_seconds() {
    killed_after_consent(true);
}
#[test]
fn actual_cli_killed_after_consent_offline_then_daemon_start_installs_within_seconds() {
    killed_after_consent(false);
}

/// The approver loses the pipe at the proofs and waits 3 s before it publishes, so the
/// command spends about 3 s in its retry (answered `not_member`).
fn slow_publication_after_cut(
    a: &Fixture,
    proxy: &Arc<fault_proxy::FaultProxy>,
) -> Arc<DesktopRing> {
    let proxy = proxy.clone();
    approver(a, &proxy.url(), move |point| match point {
        AdoptionTestPoint::ApprovalProofsReceived => proxy.cut_non_ring(),
        AdoptionTestPoint::ApprovalBeforePublish => std::thread::sleep(Duration::from_secs(3)),
        _ => {}
    })
}
fn counting_maintenance(acts: &Arc<std::sync::atomic::AtomicUsize>) -> xshelld::server::TestHook {
    let acts = acts.clone();
    xshelld::server::TestHook(Arc::new(move |_, point| {
        if point == xshelld::server::TestPoint::StandaloneMaintenanceActs {
            acts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        false
    }))
}

#[test]
fn live_pair_command_is_never_raced_by_daemon() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let a = Fixture::new();
    let _ad = a.server();
    let ar = slow_publication_after_cut(&a, &proxy);
    let b = Fixture::new();
    let acts = Arc::new(AtomicUsize::new(0));
    let _bd = b.server_with_hook(counting_maintenance(&acts));
    let mut child = b.pair(&proxy.url(), 120_000);
    let code = child.code();
    std::thread::scope(|scope| {
        let approval = scope.spawn(|| ar.approve_device_code(&code));
        while child.child.try_wait().unwrap().is_none() {
            assert_eq!(
                acts.load(Ordering::SeqCst),
                0,
                "the Daemon acted while the command lived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(approval.join().unwrap().is_ok());
    });
    assert_eq!(child.finish(), 0, "stderr:\n{}", child.stderr());
    assert!(child.seen.iter().any(|l| l == RECONNECTING_LINE));
    assert!(standalone_installed(&b));
    // Installed: idle maintenance returns before it probes, so it never acts later either.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(acts.load(Ordering::SeqCst), 0);
    ar.quit();
}
#[test]
fn live_in_process_adoption_is_never_raced_by_daemon() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let a = Fixture::new();
    let _ad = a.server();
    let ar = slow_publication_after_cut(&a, &proxy);
    let b = Fixture::new();
    let acts = Arc::new(AtomicUsize::new(0));
    let bd = b.server_with_hook(counting_maintenance(&acts));
    let (tx, rx) = mpsc::channel();
    let url = proxy.url();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            let result = bd.adopt_standalone(
                &url,
                Duration::from_secs(120),
                &Arc::new(AtomicBool::new(false)),
                |code| tx.send(code.clone()).unwrap(),
                |p, lease| Ok(Some(lease.confirm(&p.proposal_hash)?)),
            );
            done.store(true, Ordering::SeqCst);
            result
        });
        let code = rx.recv_timeout(Duration::from_secs(8)).unwrap();
        let ar = &ar;
        let approval = scope.spawn(move || ar.approve_device_code(&code));
        while !done.load(Ordering::SeqCst) {
            assert_eq!(
                acts.load(Ordering::SeqCst),
                0,
                "raced the in-process command"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(guest.join().unwrap().is_ok());
        assert!(approval.join().unwrap().is_ok());
    });
    assert!(standalone_installed(&b));
    assert!(status_installed(&bd));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(acts.load(Ordering::SeqCst), 0);
    ar.quit();
}

fn unfinished_and_ctrl_c(running: bool) {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let a = Fixture::new();
    let _ad = a.server();
    // At the proofs: the pipe dies and the service becomes unreachable for new
    // connections; the approver still publishes on its open Ring connection.
    let ar = {
        let proxy = proxy.clone();
        approver(&a, &proxy.url(), move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived {
                proxy.set_down(true);
                proxy.cut_non_ring();
            }
        })
    };
    for cancel in [false, true] {
        let b = Fixture::new();
        let mut daemon = running.then(|| b.server());
        let ttl = if cancel { 60_000 } else { 6_000 };
        let mut child = b.pair(&proxy.url(), ttl);
        let code = child.code();
        assert!(ar.approve_device_code(&code).is_ok());
        child.wait_line(RECONNECTING_LINE, Duration::from_secs(5));
        if cancel {
            child.cancel();
        }
        assert_eq!(child.finish(), xshelld::pair::UNFINISHED_EXIT);
        let stderr = child.stderr();
        let copy = if cancel {
            UNFINISHED_CANCELLED_LINE
        } else if running {
            UNFINISHED_RUNNING_LINE
        } else {
            UNFINISHED_STOPPED_LINE
        };
        let lines: Vec<&str> = stderr.lines().collect();
        assert!(lines.contains(&copy), "cancel {cancel}: {stderr}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("xshelld: reconciliation_required:")),
            "{stderr}"
        );
        assert!(
            !stderr.contains("These words were cancelled"),
            "after consent: {stderr}"
        );
        assert!(!standalone_installed(&b));
        proxy.set_down(false);
        if !running {
            daemon = Some(b.server());
        }
        let daemon = daemon.unwrap();
        wait_for(
            "the Daemon finishes the approved Join",
            Duration::from_secs(30),
            || status_installed(&daemon) && standalone_installed(&b),
        );
        assert_no_words_on_disk(&b, &code);
    }
    ar.quit();
}
#[test]
fn actual_cli_unfinished_prints_daemon_finishes_and_ctrl_c_after_consent() {
    unfinished_and_ctrl_c(true);
}
#[test]
fn actual_cli_unfinished_offline_prints_next_start_and_ctrl_c_after_consent() {
    unfinished_and_ctrl_c(false);
}

/// S2: every Relay refusal before expiry is retried, including `removed` and
/// `bad_signature`; the command finishes once the accepted chain lists it.
#[test]
fn actual_cli_relay_refusals_after_consent_are_retried_until_listed() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let a = Fixture::new();
    let _ad = a.server();
    let armed = Arc::new(std::sync::Mutex::new(None::<fault_proxy::Fault>));
    let ar = {
        let (proxy, armed) = (proxy.clone(), armed.clone());
        approver(&a, &proxy.url(), move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived {
                if let Some(fault) = armed.lock().unwrap().take() {
                    proxy.set_fault(Some(fault));
                    proxy.cut_non_ring();
                }
            }
        })
    };
    for code in ["removed", "bad_signature", "not_member"] {
        let b = Fixture::new();
        let bd = b.server();
        let (key, _, _) = common::ring::identity(&bd);
        let key = key.to_string();
        *armed.lock().unwrap() = Some(fault_proxy::Fault::RefuseAuth {
            keys: vec![key.clone()],
            code,
            detail: None,
        });
        let mut child = b.pair(&proxy.url(), 120_000);
        let words = child.code();
        std::thread::scope(|scope| {
            let approval = scope.spawn(|| ar.approve_device_code(&words));
            wait_for("two refused observations", Duration::from_secs(20), || {
                proxy.auths(&key).iter().filter(|s| s.refused).count() >= 2
            });
            assert!(
                child.child.try_wait().unwrap().is_none(),
                "{code} ended the command"
            );
            proxy.set_fault(None);
            assert!(approval.join().unwrap().is_ok());
        });
        assert_eq!(child.finish(), 0, "{code}: {}", child.stderr());
        assert!(child.seen.iter().any(|l| l == RECONNECTING_LINE));
        assert!(standalone_installed(&b), "{code}");
        drop(bd);
    }
    ar.quit();
}

/// S3: an observation answered after Ctrl-C is not used: the held authentication is let
/// through only after the command was cancelled, and the answer would list this machine.
#[test]
fn actual_cli_ctrl_c_during_observation_never_uses_its_late_answer() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let a = Fixture::new();
    let _ad = a.server();
    let ar = {
        let proxy = proxy.clone();
        approver(&a, &proxy.url(), move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived {
                // Every new authentication (the command's observation) is held.
                proxy.set_fault(Some(fault_proxy::Fault::HoldFrom("auth")));
                proxy.cut_non_ring();
            }
        })
    };
    let b = Fixture::new();
    let mut child = b.pair(&proxy.url(), 60_000);
    let code = child.code();
    assert!(ar.approve_device_code(&code).is_ok(), "published");
    child.wait_line(RECONNECTING_LINE, Duration::from_secs(5));
    wait_for("an observation in flight", Duration::from_secs(5), || {
        proxy.holding()
    });
    child.cancel();
    std::thread::sleep(Duration::from_millis(300));
    // The held observation now completes and would list this machine.
    proxy.release();
    let exit = child.finish();
    let stderr = child.stderr();
    assert_eq!(exit, xshelld::pair::UNFINISHED_EXIT, "{stderr}");
    assert!(
        stderr.lines().any(|l| l == UNFINISHED_CANCELLED_LINE),
        "{stderr}"
    );
    // The Offline command used nothing it observed late.
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert_eq!(b.journal()["installed"], false);
    let daemon = b.server();
    wait_for(
        "the Daemon finishes the approved Join",
        Duration::from_secs(15),
        || status_installed(&daemon) && standalone_installed(&b),
    );
    ar.quit();
}

/// S3: a successful observation that returns after the command's deadline is not used.
/// The observation succeeds (the approver published) and is held at a test cut until the
/// code has expired, then returned.
#[test]
fn actual_cli_deadline_during_observation_never_uses_its_late_answer() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let a = Fixture::new();
    let _ad = a.server();
    let ar = {
        let proxy = proxy.clone();
        approver(&a, &proxy.url(), move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived {
                proxy.cut_non_ring();
            }
        })
    };
    let b = Fixture::new();
    let cut = b.home.path().join("after-observation");
    let ttl = 6_000;
    let mut child = b.pair_with_env(
        &proxy.url(),
        ttl,
        &[("XSHELLD_TEST_AFTER_OBSERVATION", &cut)],
    );
    let code = child.code();
    let started = Instant::now();
    assert!(ar.approve_device_code(&code).is_ok(), "published");
    wait_for(
        "a successful observation",
        Duration::from_millis(ttl),
        || cut.exists(),
    );
    let expires_at = b.journal()["intent"]["expiresAt"].as_u64().unwrap();
    while unix_now() <= expires_at || started.elapsed() < Duration::from_millis(ttl + 500) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.child.try_wait().unwrap().is_none());
    std::fs::remove_file(&cut).unwrap();
    let exit = child.finish();
    let stderr = child.stderr();
    assert_eq!(exit, xshelld::pair::UNFINISHED_EXIT, "{stderr}");
    assert!(
        stderr.lines().any(|l| l == UNFINISHED_STOPPED_LINE),
        "{stderr}"
    );
    assert!(!child
        .seen
        .iter()
        .any(|l| l.starts_with("Added this machine")));
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    assert_eq!(b.journal()["installed"], false);
    let daemon = b.server();
    wait_for(
        "the Daemon finishes the approved Join",
        Duration::from_secs(15),
        || status_installed(&daemon) && standalone_installed(&b),
    );
    ar.quit();
}

/// Ctrl-C while the Store lock for the observation pin is busy ends the command within one
/// attempt's bound, with the cancelled copy.
#[test]
fn actual_cli_ctrl_c_while_store_lock_busy_after_consent_ends_promptly() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let a = Fixture::new();
    let _ad = a.server();
    let b = Fixture::new();
    let held = Arc::new(std::sync::Mutex::new(None::<std::fs::File>));
    let ar = {
        let (proxy, held, ring_lock) = (
            proxy.clone(),
            held.clone(),
            b.paths.ring_dir.join("ring.lock"),
        );
        approver(&a, &proxy.url(), move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&ring_lock)
                    .unwrap();
                file.try_lock().unwrap();
                *held.lock().unwrap() = Some(file);
                proxy.cut_non_ring();
            }
        })
    };
    let mut child = b.pair(&proxy.url(), 120_000);
    let code = child.code();
    assert!(ar.approve_device_code(&code).is_ok());
    child.wait_line(RECONNECTING_LINE, Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(500));
    assert!(held.lock().unwrap().is_some());
    let cancelled = Instant::now();
    child.cancel();
    let exit = child.finish();
    let stderr = child.stderr();
    assert!(
        cancelled.elapsed() < Duration::from_secs(2),
        "{:?}",
        cancelled.elapsed()
    );
    assert_eq!(exit, xshelld::pair::UNFINISHED_EXIT, "{stderr}");
    assert!(
        stderr.lines().any(|l| l == UNFINISHED_CANCELLED_LINE),
        "{stderr}"
    );
    drop(held.lock().unwrap().take());
    ar.quit();
}

/// Every journal state from the start until `watching` is cleared: (accepted length,
/// installed).
fn watch_journal(
    b: &Fixture,
    watching: &Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<Vec<(usize, bool)>> {
    use std::sync::atomic::Ordering;
    let (watching, path) = (
        watching.clone(),
        b.paths.ring_dir.join("standalone-adoption.json"),
    );
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        while watching.load(Ordering::SeqCst) {
            if let Some(j) = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            {
                seen.push((
                    j["accepted"].as_array().map_or(0, |a| a.len()),
                    j["installed"] == true,
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        seen
    })
}
fn assert_never_regressed(seen: &[(usize, bool)]) {
    assert!(
        seen.windows(2)
            .all(|w| w[1].0 >= w[0].0 && (w[1].1 || !w[0].1)),
        "the journal regressed: {seen:?}"
    );
}
/// The receipt names exactly the durable installation.
fn assert_receipt_is_installation(b: &Fixture) -> serde_json::Value {
    let end = b.journal();
    assert_eq!(end["installed"], true);
    let chain = b.chain();
    assert_eq!(
        end["accepted"],
        serde_json::json!(chain
            .versions()
            .iter()
            .map(|v| v.token())
            .collect::<Vec<_>>())
    );
    end
}

/// S1: "offer lock free" proves only that the command is gone. A running owner's admitted
/// `InstallStandaloneAccepted` handler may still overlap with maintenance; the journal and
/// runtime fences keep the result exact. `newer`: while the handler is paused another
/// machine joins, so maintenance observes a strictly newer accepted prefix than the one
/// the paused handler carries.
fn handoff_overlapping_maintenance(newer: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let (reached, at_handler) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    let (ended_tx, ended) = mpsc::channel::<bool>();
    let ended_tx = std::sync::Mutex::new(ended_tx);
    let first = AtomicBool::new(true);
    let bd = b.server_with_hook(xshelld::server::TestHook(Arc::new(move |_, point| {
        match point {
            xshelld::server::TestPoint::StandaloneHandoffInstalled
                if first.swap(false, Ordering::SeqCst) =>
            {
                reached.send(()).unwrap();
                let _ = released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30));
            }
            xshelld::server::TestPoint::StandaloneHandoffEnded { installed } => {
                let _ = ended_tx.lock().unwrap().send(installed);
            }
            _ => {}
        }
        false
    })));
    let mut child = b.pair(&relay.url(), 120_000);
    let code = child.code();
    assert!(ar.approve_device_code(&code).is_ok());
    at_handler.recv_timeout(Duration::from_secs(10)).unwrap();
    let handed = b.journal();
    assert_eq!(handed["installed"], false);
    let handed_len = handed["accepted"].as_array().unwrap().len();
    assert!(handed_len > 0);
    let watching = Arc::new(AtomicBool::new(true));
    let watcher = watch_journal(&b, &watching);
    if newer {
        let c = Fixture::new();
        let mut joiner = c.pair(&relay.url(), 30_000);
        let words = joiner.code();
        assert!(ar.approve_device_code(&words).is_ok());
        assert_eq!(joiner.finish(), 0);
    }
    // The command dies while its handoff is paused inside the Daemon.
    child.child.kill().unwrap();
    assert!(!child.child.wait().unwrap().success());
    wait_for(
        "maintenance installs while the handoff is paused",
        Duration::from_secs(15),
        || {
            standalone_installed(&b)
                && (!newer || b.journal()["accepted"].as_array().unwrap().len() > handed_len)
        },
    );
    assert!(ended.try_recv().is_err(), "the handoff is still paused");
    release.send(()).unwrap();
    // Synchronised on the handler's actual end, not a sleep.
    let handler_installed = ended.recv_timeout(Duration::from_secs(10)).unwrap();
    watching.store(false, Ordering::SeqCst);
    assert_never_regressed(&watcher.join().unwrap());
    let end = assert_receipt_is_installation(&b);
    assert_eq!(end["intent"], handed["intent"]);
    let len = end["accepted"].as_array().unwrap().len();
    if newer {
        assert!(len > handed_len, "the newer prefix is kept");
    } else {
        assert!(
            handler_installed,
            "same prefix: the handoff receipt is consistent"
        );
        assert_eq!(len, handed_len);
    }
    assert!(status_installed(&bd));
    let (_, _, identity) = common::ring::identity(&bd);
    assert_eq!(identity["ring"]["version"], b.chain().head().version());
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}
#[test]
fn running_owner_handoff_overlapping_maintenance_ends_installed_without_regression() {
    handoff_overlapping_maintenance(false);
}
#[test]
fn running_owner_handoff_overlapping_maintenance_keeps_a_newer_accepted_prefix() {
    handoff_overlapping_maintenance(true);
}

/// A running owner's handoff whose install fails gives no success receipt: the command
/// exits unfinished, and maintenance ends the obligation installed.
#[test]
fn running_owner_failed_handoff_gives_no_receipt_and_maintenance_installs() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = a.ring(&relay.url());
    create(&ar, &a.companion());
    let b = Fixture::new();
    let armed = Arc::new(AtomicBool::new(true));
    let (ended_tx, ended) = mpsc::channel::<bool>();
    let ended_tx = std::sync::Mutex::new(ended_tx);
    let hook_armed = armed.clone();
    let bd = b.server_with_hook(xshelld::server::TestHook(Arc::new(move |_, point| {
        match point {
            // The handoff's roster write fails; maintenance's own (on its thread) does not.
            xshelld::server::TestPoint::BeforeRingRosterWrite => {
                hook_armed.load(Ordering::SeqCst)
                    && std::thread::current().name() != Some("ring-maintenance")
            }
            xshelld::server::TestPoint::StandaloneHandoffEnded { installed } => {
                hook_armed.store(false, Ordering::SeqCst);
                let _ = ended_tx.lock().unwrap().send(installed);
                false
            }
            _ => false,
        }
    })));
    let watching = Arc::new(AtomicBool::new(true));
    let watcher = watch_journal(&b, &watching);
    let mut child = b.pair(&relay.url(), 120_000);
    let code = child.code();
    assert!(ar.approve_device_code(&code).is_ok());
    assert!(
        !ended.recv_timeout(Duration::from_secs(10)).unwrap(),
        "the failed handoff replied without a receipt"
    );
    assert_eq!(child.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert!(child.stderr().lines().any(|l| l == UNFINISHED_RUNNING_LINE));
    assert!(!child
        .seen
        .iter()
        .any(|l| l.starts_with("Added this machine")));
    wait_for(
        "maintenance installs the retained obligation",
        Duration::from_secs(15),
        || status_installed(&bd) && standalone_installed(&b),
    );
    watching.store(false, Ordering::SeqCst);
    assert_never_regressed(&watcher.join().unwrap());
    assert_receipt_is_installation(&b);
    assert_no_words_on_disk(&b, &code);
    ar.quit();
}

// ---------- #110 B1: a second `xshelld pair` while an earlier attempt is unresolved ----------

const EARLIER_CHECKING_LINE: &str =
    "An earlier pairing of this machine is still being finished. Checking now; no new words yet.";
const EARLIER_STOPPED_LINE: &str = "Pairing needs xshell running in the background here: start xshelld serve. Then check Settings > Ring on another device; no new words needed yet.";
const ALREADY_RUNNING_LINE: &str = "Another xshelld pair command is showing words on this machine. Use those words, or stop that command first.";
const BUSY_LINE: &str =
    "xshell on this machine is starting or stopping and hasn't answered. Try again in a moment.";
const DONE_TWO: &str = "Added this machine to your Ring. Your Ring has 2 machines.";

/// The `cli.pair.earlier.running` line with `minutes`.
fn earlier_running_line(minutes: u64) -> String {
    format!("Pairing isn't confirmed; the machine keeps checking and joins when it reaches the service. If not joined in about {minutes} min, run xshelld pair for new words.")
}
fn assert_no_raw_code(stderr: &str) {
    for code in [
        "reconciliation_required",
        "legacy_offer_active",
        "running owner unavailable",
        "xshelld:",
    ] {
        assert!(!stderr.contains(code), "raw {code:?} in:\n{stderr}");
    }
}
/// An approver that SIGKILLs the pair command `pid` the first time it receives proofs,
/// right after `at_proofs` (proxy faults, say); `before_publish` runs at the first
/// `ApprovalBeforePublish`.
fn killing_approver(
    a: &Fixture,
    url: &str,
    pid: &Arc<std::sync::atomic::AtomicI32>,
    at_proofs: impl Fn() + Send + Sync + 'static,
    before_publish: impl Fn() + Send + Sync + 'static,
) -> Arc<DesktopRing> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let pid = pid.clone();
    let (killed, held) = (AtomicBool::new(false), AtomicBool::new(false));
    approver(a, url, move |point| match point {
        AdoptionTestPoint::ApprovalProofsReceived if !killed.swap(true, Ordering::SeqCst) => {
            assert_eq!(
                unsafe { libc::kill(pid.load(Ordering::SeqCst), libc::SIGKILL) },
                0
            );
            at_proofs();
        }
        AdoptionTestPoint::ApprovalBeforePublish if !held.swap(true, Ordering::SeqCst) => {
            before_publish()
        }
        _ => {}
    })
}
/// The first `xshelld pair` on `b`, SIGKILLed after this machine's consent left (by a
/// [`killing_approver`] sharing `pid`). The approval runs on its own thread (returned).
fn first_pair_killed(
    b: &Fixture,
    ar: &Arc<DesktopRing>,
    url: &str,
    ttl_ms: u64,
    pid: &Arc<std::sync::atomic::AtomicI32>,
) -> std::thread::JoinHandle<Result<(), String>> {
    let mut child = b.pair(url, ttl_ms);
    pid.store(child.child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    let code = child.code();
    let ar = ar.clone();
    let approval = std::thread::spawn(move || {
        ar.approve_device_code(&code)
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    assert!(!child.child.wait().unwrap().success());
    approval
}
impl PairChild {
    /// The next Device code within `within` (stdout lines are kept in `seen`).
    fn code_within(&mut self, within: Duration) -> DeviceCode {
        let until = Instant::now() + within;
        loop {
            let line = match self
                .lines
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(line) => line,
                Err(e) => self.no_code(e),
            };
            self.seen.push(line.clone());
            if let Ok(code) = DeviceCode::parse(&line) {
                self.words += 1;
                return code;
            }
        }
    }
}
fn ms(value: &str) -> PathBuf {
    PathBuf::from(value)
}
/// The pair env (`XSHELLD_PAIR_EARLIER_WAIT_MS`) as the fixture takes it.
fn earlier_wait(ms_value: &PathBuf) -> [(&'static str, &std::path::Path); 1] {
    [("XSHELLD_PAIR_EARLIER_WAIT_MS", ms_value.as_path())]
}
/// Members of `chain` whose key is `key`.
fn entries(chain: &RosterChain, key: &xshell_protocol::ring::SignKey) -> usize {
    chain
        .head()
        .roster()
        .members
        .iter()
        .filter(|m| m.sign_key == *key)
        .count()
}

/// Fault: the joiner's Relay is unreachable from the approver's proofs on (the approver
/// still publishes on its open Ring connection).
fn outage_at_proofs(proxy: &Arc<fault_proxy::FaultProxy>) -> impl Fn() + Send + Sync + 'static {
    let proxy = proxy.clone();
    move || {
        proxy.set_down(true);
        proxy.cut_non_ring();
    }
}

#[test]
fn actual_cli_second_pair_while_daemon_finishes_reports_added() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let url = proxy.url();
    let a = Fixture::new();
    let _ad = a.server();
    let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ar = killing_approver(&a, &url, &pid, outage_at_proofs(&proxy), || {});
    let b = Fixture::new();
    let bd = b.server();
    let approval = first_pair_killed(&b, &ar, &url, 300_000, &pid);
    assert!(approval.join().unwrap().is_ok());
    assert_eq!(b.journal()["installed"], false);
    let mut second = b.pair(&url, 300_000);
    second.wait_line(EARLIER_CHECKING_LINE, Duration::from_secs(5));
    std::thread::sleep(Duration::from_secs(1));
    proxy.set_down(false);
    assert_eq!(second.finish(), 0, "stderr:\n{}", second.stderr());
    assert_eq!(second.words, 0, "no new offer");
    assert_eq!(second.seen.last().unwrap(), DONE_TWO);
    assert!(standalone_installed(&b));
    let key = common::ring::identity(&bd).0;
    assert_eq!(entries(&b.chain(), &key), 1, "one Roster entry");
    assert_no_raw_code(&second.stderr());
    ar.quit();
}

#[test]
fn actual_cli_second_pair_unresolved_times_out_with_earlier_copy() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let url = proxy.url();
    let a = Fixture::new();
    let _ad = a.server();
    let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ar = killing_approver(&a, &url, &pid, outage_at_proofs(&proxy), || {});
    let b = Fixture::new();
    let _bd = b.server();
    let approval = first_pair_killed(&b, &ar, &url, 300_000, &pid);
    assert!(approval.join().unwrap().is_ok());
    let wait_2s = ms("2000");
    let started = Instant::now();
    let mut second = b.pair_with_env(&url, 300_000, &earlier_wait(&wait_2s));
    assert_eq!(second.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert!(started.elapsed() < Duration::from_secs(6));
    let stderr = second.stderr();
    assert_eq!(second.words, 0);
    assert!(second.seen.iter().any(|l| l == EARLIER_CHECKING_LINE));
    // About 5 minutes of the 300 s code are left.
    assert_eq!(stderr.trim_end(), earlier_running_line(5), "{stderr}");
    assert_no_raw_code(&stderr);
    assert_eq!(
        b.journal()["installed"],
        false,
        "nothing installed or set aside"
    );
    ar.quit();
}

/// S7: once the earlier attempt's code has expired and the service is still unreachable,
/// the command promises no time: slice A's `cli.pair.unfinished.running` text.
#[test]
fn actual_cli_second_pair_outage_past_expiry_makes_no_promise() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let url = proxy.url();
    let a = Fixture::new();
    let _ad = a.server();
    let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ar = killing_approver(&a, &url, &pid, outage_at_proofs(&proxy), || {});
    let b = Fixture::new();
    let _bd = b.server();
    let approval = first_pair_killed(&b, &ar, &url, 4_000, &pid);
    let _ = approval.join().unwrap();
    let expires_at = b.journal()["intent"]["expiresAt"].as_u64().unwrap();
    wait_for("the code expires", Duration::from_secs(10), || {
        unix_now() > expires_at
    });
    let wait_1s = ms("1000");
    let mut second = b.pair_with_env(&url, 300_000, &earlier_wait(&wait_1s));
    assert_eq!(second.finish(), xshelld::pair::UNFINISHED_EXIT);
    let stderr = second.stderr();
    assert_eq!(stderr.trim_end(), UNFINISHED_RUNNING_LINE, "{stderr}");
    assert_eq!(second.words, 0);
    assert_no_raw_code(&stderr);
    assert!(
        b.paths.ring_dir.join("standalone-adoption.json").exists(),
        "kept past expiry while the service is unreachable"
    );
    ar.quit();
}

/// No Daemon runs: the second command says so at once. It never takes the Store ownership
/// lease to find that out, so a `serve` starting meanwhile still starts (S1). A listener
/// that does not answer is "starting or stopping".
#[test]
fn actual_cli_second_pair_offline_reports_stopped_at_once() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let url = proxy.url();
    let a = Fixture::new();
    let _ad = a.server();
    let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ar = killing_approver(&a, &url, &pid, outage_at_proofs(&proxy), || {});
    let b = Fixture::new();
    let approval = first_pair_killed(&b, &ar, &url, 300_000, &pid);
    assert!(approval.join().unwrap().is_ok());
    assert_eq!(b.journal()["installed"], false);
    let started = Instant::now();
    let mut second = b.pair(&url, 300_000);
    assert_eq!(second.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    let stderr = second.stderr();
    assert_eq!(stderr.trim_end(), EARLIER_STOPPED_LINE, "{stderr}");
    assert!(second.seen.is_empty(), "{:?}", second.seen);
    assert_eq!(second.words, 0);
    // A serve started while a second command runs is never turned away by it.
    let wait_300ms = ms("300");
    for _ in 0..5 {
        let mut again = b.pair_with_env(&url, 300_000, &earlier_wait(&wait_300ms));
        let daemon = b.server();
        let code = again.finish();
        assert!(
            [xshelld::pair::UNFINISHED_EXIT, 1].contains(&code),
            "{code}: {}",
            again.stderr()
        );
        assert_eq!(again.words, 0);
        daemon.shutdown();
    }
    // Something listens on the endpoint but never answers.
    let endpoint = b.paths.ring_dir.join("companion.sock");
    let _ = std::fs::remove_file(&endpoint);
    let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    listener.set_nonblocking(true).unwrap();
    // Accepts and holds connections for 4 s without ever answering.
    let held = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(4);
        let mut conns = Vec::new();
        while Instant::now() < until {
            if let Ok((conn, _)) = listener.accept() {
                conns.push(conn);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        conns.len()
    });
    let mut busy = b.pair(&url, 300_000);
    assert_eq!(busy.finish(), 1);
    let stderr = busy.stderr();
    assert_eq!(stderr.trim_end(), BUSY_LINE, "{stderr}");
    assert_eq!(busy.words, 0);
    assert!(held.join().unwrap() >= 1, "the command asked the endpoint");
    std::fs::remove_file(&endpoint).unwrap();
    ar.quit();
}

/// S8: the second command starts while the unresolved journal exists, shows that it is
/// checking, and gets fresh words once the Daemon sets the never-published attempt aside.
#[test]
fn actual_cli_second_pair_after_abandon_starts_fresh_offer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
    // The first approval never publishes: it holds at the proofs until its window closes
    // (as if the approving Desktop quit there), before any publication was uncertain.
    let release = Arc::new(AtomicBool::new(false));
    let ar = {
        let release = release.clone();
        killing_approver(
            &a,
            &relay.url(),
            &pid,
            move || {
                let until = Instant::now() + Duration::from_secs(30);
                while !release.load(Ordering::SeqCst) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
            },
            || {},
        )
    };
    let b = Fixture::new();
    let bd = b.server();
    let approval = first_pair_killed(&b, &ar, &relay.url(), 3_000, &pid);
    assert_eq!(b.journal()["installed"], false);
    let mut second = b.pair(&relay.url(), 300_000);
    second.wait_line(EARLIER_CHECKING_LINE, Duration::from_secs(2));
    let quit = {
        let ar = ar.clone();
        std::thread::spawn(move || ar.quit())
    };
    std::thread::sleep(Duration::from_millis(200));
    release.store(true, Ordering::SeqCst);
    quit.join().unwrap();
    assert!(approval.join().unwrap().is_err(), "never published");
    let next = a.ring(&relay.url());
    wait_for("the approver window again", Duration::from_secs(10), || {
        next.pairing_ipc_owner()
    });
    let code = second.code_within(Duration::from_secs(14));
    let approved = next.approve_device_code(&code);
    assert!(approved.is_ok(), "{approved:?}");
    assert_eq!(second.finish(), 0, "stderr:\n{}", second.stderr());
    assert_eq!(second.words, 1);
    assert_eq!(second.seen.last().unwrap(), DONE_TWO);
    let key = common::ring::identity(&bd).0;
    assert_eq!(entries(&b.chain(), &key), 1);
    assert!(std::fs::read_dir(&b.paths.ring_dir).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("standalone-adoption.json.abandoned-")));
    next.quit();
}

#[test]
fn actual_cli_second_pair_while_first_running_says_already_running() {
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let ar = approver(&a, &relay.url(), |_| {});
    let b = Fixture::new();
    let _bd = b.server();
    let mut first = b.pair(&relay.url(), 60_000);
    let code = first.code();
    let mut second = b.pair(&relay.url(), 60_000);
    assert_eq!(second.finish(), 1);
    let stderr = second.stderr();
    assert_eq!(stderr.trim_end(), ALREADY_RUNNING_LINE, "{stderr}");
    assert_eq!(second.words, 0);
    assert!(ar.approve_device_code(&code).is_ok());
    assert_eq!(first.finish(), 0, "stderr:\n{}", first.stderr());
    assert_eq!(first.seen.last().unwrap(), DONE_TWO);
    ar.quit();
}

/// S8: Ctrl-C during the wait ends it with the earlier-running copy; the owning Daemon
/// exiting during the wait is reported as stopped.
#[test]
fn actual_cli_second_pair_ctrl_c_and_owner_exit_during_wait() {
    let relay = workerd::Relay::start();
    let proxy = Arc::new(fault_proxy::FaultProxy::start(&relay.url()));
    let url = proxy.url();
    let a = Fixture::new();
    let _ad = a.server();
    let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ar = killing_approver(&a, &url, &pid, outage_at_proofs(&proxy), || {});
    let b = Fixture::new();
    let bd = b.server();
    let approval = first_pair_killed(&b, &ar, &url, 300_000, &pid);
    assert!(approval.join().unwrap().is_ok());
    let mut second = b.pair(&url, 300_000);
    second.wait_line(EARLIER_CHECKING_LINE, Duration::from_secs(5));
    let at = Instant::now();
    second.cancel();
    assert_eq!(second.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
    let stderr = second.stderr();
    assert_eq!(stderr.trim_end(), earlier_running_line(5), "{stderr}");
    let mut third = b.pair(&url, 300_000);
    third.wait_line(EARLIER_CHECKING_LINE, Duration::from_secs(5));
    let at = Instant::now();
    bd.shutdown();
    assert_eq!(third.finish(), xshelld::pair::UNFINISHED_EXIT);
    assert!(at.elapsed() < Duration::from_secs(5), "{:?}", at.elapsed());
    let stderr = third.stderr();
    assert_eq!(stderr.trim_end(), EARLIER_STOPPED_LINE, "{stderr}");
    assert_eq!(third.words, 0);
    ar.quit();
}

/// S2: the Daemon has installed the chain but not yet written its receipt, and then fails
/// to: the waiting command never reports "Added" without the receipt.
#[test]
fn actual_cli_second_pair_needs_the_installation_receipt() {
    use std::os::unix::fs::PermissionsExt;
    let relay = workerd::Relay::start();
    let a = Fixture::new();
    let _ad = a.server();
    let pid = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let ar = killing_approver(&a, &relay.url(), &pid, || {}, || {});
    let b = Fixture::new();
    let (reached_tx, reached) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    let first = std::sync::atomic::AtomicBool::new(true);
    let reached_tx = std::sync::Mutex::new(reached_tx);
    let _bd = b.server_with_hook(xshelld::server::TestHook(Arc::new(move |_, point| {
        if point == xshelld::server::TestPoint::StandaloneInstalledBeforeJournal
            && first.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = reached_tx.lock().unwrap().send(());
            let _ = released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30));
        }
        false
    })));
    let approval = first_pair_killed(&b, &ar, &relay.url(), 300_000, &pid);
    assert!(approval.join().unwrap().is_ok());
    reached.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(b.paths.ring_dir.join("roster.json").is_file(), "installed");
    assert_eq!(b.journal()["installed"], false, "no receipt yet");
    let wait_5s = ms("5000");
    let mut second = b.pair_with_env(&relay.url(), 300_000, &earlier_wait(&wait_5s));
    second.wait_line(EARLIER_CHECKING_LINE, Duration::from_secs(5));
    std::thread::sleep(Duration::from_secs(1));
    // The receipt cannot be written: finalization fails.
    let dir = b.paths.ring_dir.clone();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    release.send(()).unwrap();
    let code = second.finish();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let stderr = second.stderr();
    assert_eq!(code, xshelld::pair::UNFINISHED_EXIT, "{stderr}");
    assert!(
        !second.seen.iter().any(|l| l.starts_with("Added")),
        "{:?}",
        second.seen
    );
    assert_eq!(stderr.trim_end(), earlier_running_line(5), "{stderr}");
    // The Daemon writes the receipt on a later step.
    wait_for("the receipt", Duration::from_secs(40), || {
        standalone_installed(&b)
    });
    ar.quit();
}

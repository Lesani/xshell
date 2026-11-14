#![cfg(windows)]
//! #87 Windows slice: Desktop-first and Host-first Computer adoption against the real
//! `xshelld.exe serve --gui-bound` the hostlink `LocalDaemon` starts, the real
//! `xshelld.exe pair` CLI, and the unchanged production Relay on a Windows-local loopback
//! workerd. Computer confirmations use private Rust backend leases, NOT native GUI/window
//! consent evidence (that gate is separate).
#[path = "desktop/windows_fixture.rs"]
#[allow(dead_code)]
mod fixture;
mod win;
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use fixture::{wait, Events, NoSsh, Quiet, WAIT};
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use win::TestHome;
use workerd::Relay;
use xshell_core::companion_reservation;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision, JoinOffer, JoinStatus};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::hosts::relay_host_id;
use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig};
use xshell_hostlink::{LocalDaemon, LocalDaemonConfig, Manager, ManagerConfig};
use xshell_protocol::ring::companion::CompanionProfile;
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::membership::EnrollmentKind;
use xshell_protocol::ring::RosterChain;

/// One Windows computer: a Desktop backend (DesktopRing + Manager, elected owner) and the
/// real GUI-bound Daemon its `LocalDaemon` starts (in the Desktop's kill-on-close job).
struct WinComputer {
    ring: Arc<DesktopRing>,
    manager: Arc<Manager>,
    daemon: Arc<LocalDaemon>,
    home: TestHome,
    desktop: PathBuf,
}
impl WinComputer {
    fn new(relay: &Relay) -> Self {
        let home = TestHome::new();
        let daemon = Arc::new(
            LocalDaemon::new(LocalDaemonConfig {
                bin: win::bin().into(),
                env: home.daemon_env(),
                home: home.home(),
                socket: home.pipe.clone(),
                version: "1.5.0".into(),
                build: None,
            })
            .unwrap(),
        );
        let desktop = home.root().join("desktop");
        let (ring, manager) = open_backend(&desktop, relay);
        Self {
            ring,
            manager,
            daemon,
            home,
            desktop,
        }
    }
    fn canonical_home(&self) -> PathBuf {
        self.home.home().canonicalize().unwrap()
    }
    fn daemon_dir(&self) -> PathBuf {
        self.canonical_home()
            .join(".xshell")
            .join("daemon")
            .join("ring")
    }
    /// The Tauri startup path: durable deny-only reservation, the Daemon the Desktop
    /// starts on first dial, then in-process registration through its live pipe.
    fn register(&self) -> CompanionProfile {
        assert!(self
            .ring
            .reserve_local_companion(&self.canonical_home())
            .unwrap());
        self.daemon.ensure_spawned().unwrap();
        self.ring
            .register_local_companion(&self.daemon)
            .unwrap()
            .expect("elected owner registers")
    }
    fn companion(&self, profile: &CompanionProfile) -> CompanionClient {
        CompanionClient::for_profile(profile)
    }
    fn create(&self, profile: &CompanionProfile) -> RosterChain {
        match self
            .ring
            .create_computer(&self.companion(profile), |p, lease| {
                Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
            })
            .unwrap()
        {
            AdoptionOutcome::Installed { chain, .. } => chain,
            _ => panic!("Create not installed"),
        }
    }
    fn pair(&self, relay: &str, ttl_ms: u64) -> PairChild {
        PairChild::spawn(self.pair_command(relay, ttl_ms))
    }
    fn pair_command(&self, relay: &str, ttl_ms: u64) -> Command {
        let mut command = Command::new(win::bin());
        command
            .arg("pair")
            .args(["--relay", relay])
            .env("XSHELLD_HOME", self.home.home())
            .env("XSHELLD_SOCKET", &self.home.pipe)
            .env("XSHELLD_PAIR_TTL_MS", ttl_ms.to_string());
        command
    }
}
impl Drop for WinComputer {
    fn drop(&mut self) {
        self.ring.quit();
        self.manager.shutdown();
        self.daemon.terminate(Duration::from_secs(10));
    }
}

fn open_backend(desktop: &Path, relay: &Relay) -> (Arc<DesktopRing>, Arc<Manager>) {
    let mut cfg = DesktopRingConfig::new(desktop.to_path_buf(), "computer".into());
    cfg.default_relay_url = relay.url();
    cfg.owner_retry = Duration::from_millis(20);
    cfg.backoff_unit = Duration::from_millis(10);
    let ring = DesktopRing::open(cfg, Arc::new(Quiet));
    let events = Arc::new(Events::default());
    let mut mcfg = ManagerConfig::new("1.5.0", Arc::new(NoSsh), Arc::new(NoSsh), events);
    mcfg.backoff_unit = Duration::from_millis(20);
    let manager = Arc::new(Manager::new(mcfg));
    ring.attach_manager(&manager).unwrap();
    wait("elected Desktop owner", || ring.pairing_ipc_owner());
    (ring, manager)
}

struct PairChild {
    child: Child,
    lines: mpsc::Receiver<String>,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    readers: Vec<std::thread::JoinHandle<()>>,
}
impl PairChild {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        let (o, e) = (stdout.clone(), stderr.clone());
        let readers = vec![
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    o.lock().unwrap().push(line.clone());
                    let _ = tx.send(line);
                }
            }),
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines() {
                    let Ok(line) = line else { break };
                    let mut s = e.lock().unwrap();
                    s.push_str(&line);
                    s.push('\n');
                }
            }),
        ];
        Self {
            child,
            lines,
            stdout,
            stderr,
            readers,
        }
    }
    fn code(&mut self) -> DeviceCode {
        let until = Instant::now() + Duration::from_secs(12);
        loop {
            match self
                .lines
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(line) => {
                    if let Some(words) = line.strip_prefix("Device code: ") {
                        return DeviceCode::parse(words).expect("eight words");
                    }
                }
                Err(e) => panic!(
                    "CLI gave no Device code ({e:?}); status {:?}; stderr:\n{}",
                    self.child.try_wait(),
                    self.stderr.lock().unwrap()
                ),
            }
        }
    }
    fn finish(&mut self) -> i32 {
        let until = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < until, "Pair CLI did not exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        status.code().unwrap()
    }
    fn stdout(&self) -> Vec<String> {
        self.stdout.lock().unwrap().clone()
    }
    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
    fn words_printed(&self) -> usize {
        self.stdout()
            .iter()
            .filter(|l| l.starts_with("Device code: "))
            .count()
    }
}
impl Drop for PairChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn controller(ring: &DesktopRing) -> Arc<JoinOffer> {
    let until = Instant::now() + WAIT;
    loop {
        if let Some((_, c)) = ring.administration_joins().into_iter().next() {
            return c;
        }
        assert!(
            Instant::now() < until,
            "timed out: CLI offer dispatched to the owner"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn confirm_backend(join: &JoinOffer) {
    let (preview, lease) = join.await_consent(WAIT).unwrap();
    join.confirm_join(lease.confirm(&preview.proposal_hash).unwrap())
        .unwrap();
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Both the Desktop and the Daemon identity of `profile` are one Computer enrollment.
fn assert_computer_enrolled(chain: &RosterChain, profile: &CompanionProfile) {
    let membership = chain.head().roster().membership().unwrap();
    assert!(
        membership.enrollments.iter().any(|unit| {
            unit.kind == EnrollmentKind::Computer
                && unit.members.contains(&profile.desktop.sign_key)
                && unit.members.contains(&profile.daemon.sign_key)
        }),
        "both identities enrolled as one Computer"
    );
}

/// A live process of this user that serves no pipe (a stand-in for a replaced or squatted
/// PID record). Ends when dropped.
struct Bystander(Child);
impl Bystander {
    fn start() -> Self {
        Self(
            Command::new("cmd.exe")
                .args(["/d", "/c", "pause"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
}
impl Drop for Bystander {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Replace the PID record as the Daemon writes it (private descriptor, write-through).
fn write_pid_record(home: &Path, pid: u32) {
    let path = xshell_hostlink::local::local_pid_record_path(home);
    let tmp = path.with_extension("test.tmp");
    let _ = std::fs::remove_file(&tmp);
    use std::io::Write;
    let mut f = xshell_core::private_fs::create_new(&tmp).unwrap();
    writeln!(f, "{pid}").unwrap();
    f.sync_all().unwrap();
    drop(f);
    xshell_core::private_fs::replace_write_through(&tmp, &path).unwrap();
}

// ── Desktop-first ─────────────────────────────────────────────────────────

#[test]
fn desktop_first_registers_with_real_gui_bound_daemon_creates_and_second_computer_joins() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let a_profile = a.register();
    // Registration cleared only its exact denial and wrote the policy durably.
    assert!(!a.daemon_dir().join(companion_reservation::FILE).exists());
    assert!(a.daemon_dir().join("companion.json").exists());
    assert_eq!(Path::new(&a_profile.daemon_store), a.daemon_dir().as_path());
    let created = a.create(&a_profile);
    assert_computer_enrolled(&created, &a_profile);
    assert!(a.daemon_dir().join("roster.json").exists());

    let b = WinComputer::new(&relay);
    let b_profile = b.register();
    let offer = b.ring.begin_join(&b.companion(&b_profile)).unwrap();
    let code = offer.code.clone();
    let joined = std::thread::scope(|scope| {
        let approval = scope.spawn(|| a.ring.approve_device_code(&code));
        let (p, lease) = offer.await_consent(WAIT).unwrap();
        offer
            .confirm_join(lease.confirm(&p.proposal_hash).unwrap())
            .unwrap();
        let result = offer.wait().unwrap();
        approval.join().unwrap().unwrap();
        match result {
            AdoptionOutcome::Installed { chain, .. } => chain,
            _ => panic!("Join not installed"),
        }
    });
    assert_computer_enrolled(&joined, &b_profile);
    assert_computer_enrolled(&joined, &a_profile);
    // The new Computer's Host is visible to the first one through the Relay, never SSH.
    let id = relay_host_id(&b_profile.daemon.sign_key);
    wait("joined Host usable through the Relay", || {
        a.manager.host(&id).is_some_and(|h| h.status().usable())
    });
}

#[test]
fn reservation_refuses_other_stores_and_survives_desktop_shutdown() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let home = a.canonical_home();
    assert!(a.ring.reserve_local_companion(&home).unwrap());
    let marker = std::fs::read(a.daemon_dir().join(companion_reservation::FILE)).unwrap();
    // A second Desktop Store cannot claim this Daemon Store.
    let other = a.home.root().join("other-desktop");
    xshell_core::private_fs::ensure_dir(&other).unwrap();
    let other = other.canonicalize().unwrap();
    assert!(companion_reservation::reserve(&home, &other).is_err());
    // A Register from another Desktop Store is refused by the live Daemon.
    a.daemon.ensure_spawned().unwrap();
    let (intruder, _intruder_manager) = open_backend(&other, &relay);
    let client = CompanionClient::for_store(&a.daemon_dir()).unwrap();
    wait("Daemon companion endpoint", || {
        client
            .request(xshell_protocol::ring::companion::CompanionRequest::Descriptor)
            .is_ok()
    });
    let refused = intruder.register_companion(&client).unwrap_err();
    assert!(refused.contains("conflicting"), "{refused}");
    assert!(!a.daemon_dir().join("companion.json").exists());
    intruder.quit();
    // Desktop shutdown (quit + Daemon end) never clears the durable denial.
    a.ring.quit();
    a.daemon.terminate(Duration::from_secs(10));
    assert_eq!(
        std::fs::read(a.daemon_dir().join(companion_reservation::FILE)).unwrap(),
        marker
    );
    // A restarted Desktop with the same Store registers and clears exactly that denial.
    let daemon = Arc::new(
        LocalDaemon::new(LocalDaemonConfig {
            bin: win::bin().into(),
            env: a.home.daemon_env(),
            home: a.home.home(),
            socket: a.home.pipe.clone(),
            version: "1.5.0".into(),
            build: None,
        })
        .unwrap(),
    );
    let (ring, _manager) = open_backend(&a.desktop, &relay);
    daemon.ensure_spawned().unwrap();
    let profile = ring.register_local_companion(&daemon).unwrap().unwrap();
    assert_eq!(
        Path::new(&profile.desktop_store),
        a.desktop.canonicalize().unwrap()
    );
    assert!(!a.daemon_dir().join(companion_reservation::FILE).exists());
    ring.quit();
    daemon.terminate(Duration::from_secs(10));
}

/// BINDING AMENDMENT 1 §5: two processes with different Desktop Stores race for one Daemon
/// Store behind a barrier; exactly one claims it, the durable marker names it, and the
/// loser's Register is refused by the live Daemon.
#[test]
fn barrier_controlled_two_process_reservation_race_has_one_winner() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let home = a.canonical_home();
    let race = a.home.root().join("race");
    std::fs::create_dir(&race).unwrap();
    let stores: Vec<PathBuf> = ["one", "two"]
        .iter()
        .map(|n| {
            let p = a.home.root().join(format!("desktop-{n}"));
            xshell_core::private_fs::ensure_dir(&p).unwrap();
            p.canonicalize().unwrap()
        })
        .collect();
    let mut children: Vec<Child> = stores
        .iter()
        .enumerate()
        .map(|(i, store)| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "helper_reservation_race", "--nocapture"])
                .env("XSHELL_X87_RACE", i.to_string())
                .env("XSHELL_X87_RACE_HOME", &home)
                .env("XSHELL_X87_RACE_STORE", store)
                .env("XSHELL_X87_RACE_DIR", &race)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    wait("both racers armed", || {
        (0..2).all(|i| race.join(format!("ready-{i}")).exists())
    });
    std::fs::write(race.join("go"), b"go").unwrap();
    wait("both racers finished", || {
        (0..2).all(|i| race.join(format!("result-{i}")).exists())
    });
    for child in &mut children {
        let _ = child.wait();
    }
    let results: Vec<String> = (0..2)
        .map(|i| std::fs::read_to_string(race.join(format!("result-{i}"))).unwrap())
        .collect();
    let winners: Vec<usize> = (0..2).filter(|i| results[*i] == "ok").collect();
    assert_eq!(winners.len(), 1, "exactly one claim: {results:?}");
    let (winner, loser) = (winners[0], 1 - winners[0]);
    let marker = companion_reservation::read_reservation(&a.daemon_dir())
        .unwrap()
        .unwrap();
    assert_eq!(marker.desktop_store, stores[winner].to_str().unwrap());
    a.daemon.ensure_spawned().unwrap();
    let client = CompanionClient::for_store(&a.daemon_dir()).unwrap();
    wait("Daemon companion endpoint", || {
        client
            .request(xshell_protocol::ring::companion::CompanionRequest::Descriptor)
            .is_ok()
    });
    let (losing, _m1) = open_backend(&stores[loser], &relay);
    assert!(losing.register_companion(&client).is_err());
    losing.quit();
    let (winning, _m2) = open_backend(&stores[winner], &relay);
    winning.register_companion(&client).unwrap();
    winning.quit();
}

/// Not a test unless `XSHELL_X87_RACE` is set: one racer of the reservation race.
#[test]
fn helper_reservation_race() {
    let Ok(index) = std::env::var("XSHELL_X87_RACE") else {
        return;
    };
    let var = |k: &str| PathBuf::from(std::env::var_os(k).unwrap());
    let dir = var("XSHELL_X87_RACE_DIR");
    std::fs::write(dir.join(format!("ready-{index}")), b"").unwrap();
    while !dir.join("go").exists() {
        std::hint::spin_loop();
    }
    let result = match companion_reservation::reserve(
        &var("XSHELL_X87_RACE_HOME"),
        &var("XSHELL_X87_RACE_STORE"),
    ) {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("refused: {e}"),
    };
    std::fs::write(dir.join(format!("result-{index}")), result).unwrap();
}

// ── Endpoint identity (Decision 4, Amendment 1 §3) ────────────────────────

#[test]
fn adopted_daemon_registers_through_its_private_pid_record() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    // Another Desktop process started this Daemon; this one only adopts it.
    let external = win::Daemon::start(&a.home);
    let record = xshell_hostlink::local::local_pid_record_path(&a.home.home());
    wait("Daemon PID record", || {
        xshell_hostlink::local::read_pid_record(&a.home.home()).ok() == Some(external.pid())
    });
    assert!(record.exists());
    assert!(a.daemon.gui_pid().is_none());
    let profile = a.ring.register_local_companion(&a.daemon).unwrap().unwrap();
    assert_eq!(Path::new(&profile.daemon_store), a.daemon_dir().as_path());
    assert!(a.daemon.gui_pid().is_none(), "registration never spawns");
}

#[test]
fn stale_and_replaced_pid_records_refuse_before_any_request() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let external = win::Daemon::start(&a.home);
    wait("Daemon PID record", || {
        xshell_hostlink::local::read_pid_record(&a.home.home()).ok() == Some(external.pid())
    });
    let client = CompanionClient::for_store(&a.daemon_dir()).unwrap();
    wait("Daemon companion endpoint", || {
        client
            .request(xshell_protocol::ring::companion::CompanionRequest::Descriptor)
            .is_ok()
    });
    let policy = a.daemon_dir().join("companion.json");
    // Replaced: the record names another live process, not the pipe's server.
    let bystander = Bystander::start();
    write_pid_record(&a.home.home(), bystander.pid());
    let started = Instant::now();
    let e = a.ring.register_local_companion(&a.daemon).unwrap_err();
    assert!(e.contains("not the expected Daemon"), "{e}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "refused at once"
    );
    assert!(!policy.exists());
    // Stale: the record names an exited process.
    let gone = {
        let b = Bystander::start();
        let pid = b.pid();
        drop(b);
        pid
    };
    write_pid_record(&a.home.home(), gone);
    assert!(xshell_core::local_ipc::ServerProcess::open(gone).is_err());
    let e = a.ring.register_local_companion(&a.daemon).unwrap_err();
    assert!(
        e.contains("stale Daemon PID record") || e.contains("pending"),
        "{e}"
    );
    assert!(!policy.exists());
    // Malformed: never parsed into a PID.
    std::fs::write(
        xshell_hostlink::local::local_pid_record_path(&a.home.home()),
        b"12 34\n",
    )
    .unwrap();
    assert!(xshell_hostlink::local::read_pid_record(&a.home.home()).is_err());
    // Restored: the real Daemon's own record registers.
    write_pid_record(&a.home.home(), external.pid());
    a.ring.register_local_companion(&a.daemon).unwrap().unwrap();
    assert!(policy.exists());
}

#[test]
fn squatter_pipe_is_refused_before_register() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let home = a.canonical_home();
    // A same-user process serving the companion pipe before any Daemon owns it.
    let dir = companion_reservation::reserve(&home, &{
        xshell_core::private_fs::ensure_dir(&a.desktop).unwrap();
        a.desktop.canonicalize().unwrap()
    })
    .unwrap();
    let endpoint = xshell_core::local_ipc::companion_endpoint(
        &xshell_core::local_ipc::canonical_store(&dir).unwrap(),
    )
    .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let _squatter = xshell_core::local_ipc::serve(endpoint, move |_: serde_json::Value, _| {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        serde_json::json!({"type": "descriptor", "profile": null})
    })
    .unwrap();
    // The record names another live process (as a squatter would need).
    std::fs::create_dir_all(home.join(".xshell").join("run")).unwrap();
    let bystander = Bystander::start();
    write_pid_record(&home, bystander.pid());
    let e = a.ring.register_local_companion(&a.daemon).unwrap_err();
    assert!(e.contains("not the expected Daemon"), "{e}");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no request reached the squatter"
    );
    assert!(!dir.join("companion.json").exists());
}

// ── Host-first: the real `xshelld.exe pair` ────────────────────────────────

/// Owner A has a Ring; B's Desktop registered its Daemon. The Pair CLI on B captures its
/// deadline on the shared Windows monotonic clock in its own process; B's Desktop owner (a
/// separate process: this test) receives it, presents the offer, confirms on the backend.
#[test]
fn host_first_actual_cli_pair_through_registered_desktop_backend_consent() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let a_profile = a.register();
    a.create(&a_profile);
    let b = WinComputer::new(&relay);
    let b_profile = b.register();
    let mut child = b.pair(&relay.url(), 30_000);
    let code = child.code();
    let join = controller(&b.ring);
    assert!(join.code.format() == code.format());
    assert!(join.expires_at <= unix_now() + 30);
    assert!(b.ring.chain().is_none());
    std::thread::scope(|scope| {
        let approve = scope.spawn(|| a.ring.approve_device_code(&code));
        confirm_backend(&join);
        assert!(approve.join().unwrap().is_ok());
    });
    assert_eq!(child.finish(), 0, "stderr:\n{}", child.stderr());
    assert!(child
        .stdout()
        .iter()
        .any(|l| l == "Paired this Computer; both local stores are installed."));
    assert_eq!(child.words_printed(), 1);
    assert_computer_enrolled(&b.ring.chain().unwrap(), &b_profile);
    assert!(b.daemon_dir().join("roster.json").exists());
    assert!(a.ring.approve_device_code(&code).is_err(), "one use");
}

#[test]
fn host_first_decline_exits_1_and_expiry_exits_5_without_proofs() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let a_profile = a.register();
    a.create(&a_profile);
    let b = WinComputer::new(&relay);
    b.register();
    // Decline on B's backend after the verified preview.
    let mut child = b.pair(&relay.url(), 30_000);
    let code = child.code();
    let join = controller(&b.ring);
    std::thread::scope(|scope| {
        let approve = scope.spawn(|| a.ring.approve_device_code(&code));
        join.await_consent(WAIT).unwrap();
        join.decline_join().unwrap();
        assert!(approve.join().unwrap().is_err());
    });
    assert_eq!(child.finish(), 1, "stderr:\n{}", child.stderr());
    assert!(!b.daemon_dir().join("roster.json").exists());
    assert!(b.ring.chain().is_none());
    wait("declined offer retired", || {
        b.ring.administration_joins().is_empty()
    });
    // Expiry: nobody approves within the original budget.
    let started = Instant::now();
    let mut child = b.pair(&relay.url(), 4_000);
    let code = child.code();
    let join = controller(&b.ring);
    assert!(join.expires_at <= unix_now() + 4);
    assert_eq!(child.finish(), 5, "stderr:\n{}", child.stderr());
    assert!(started.elapsed() < Duration::from_secs(8));
    wait("expired offer failed", || {
        matches!(join.join_status(), JoinStatus::Failed { .. })
    });
    assert!(a.ring.approve_device_code(&code).is_err());
    assert!(!b.daemon_dir().join("roster.json").exists());
}

/// Delayed dispatch keeps the CLI's original deadline (never renewed by the late owner);
/// an already expired deadline is never dispatched at all.
#[test]
fn host_first_late_owner_keeps_original_deadline_and_expired_offer_is_not_dispatched() {
    let relay = Relay::start();
    let b = WinComputer::new(&relay);
    b.register();
    b.ring.quit();
    b.manager.shutdown();
    // Delayed: the owner appears 1.5 s into a 4 s budget.
    let started = Instant::now();
    let mut child = b.pair(&relay.url(), 4_000);
    let first = child.lines.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(first.starts_with("Computer pairing pending."), "{first}");
    std::thread::sleep(Duration::from_millis(1_500));
    let (ring, _manager) = open_backend(&b.desktop, &relay);
    let code = child.code();
    let join = controller(&ring);
    assert!(join.code.format() == code.format());
    assert!(join.expires_at <= unix_now() + 3);
    assert_eq!(child.finish(), 5, "stderr:\n{}", child.stderr());
    assert!(started.elapsed() < Duration::from_secs(6));
    ring.quit();
    // Expired: the owner appears only after a 1.5 s budget ended.
    let mut child = b.pair(&relay.url(), 1_500);
    std::thread::sleep(Duration::from_millis(2_000));
    let (ring, _manager) = open_backend(&b.desktop, &relay);
    assert_eq!(child.finish(), 5, "stderr:\n{}", child.stderr());
    std::thread::sleep(Duration::from_millis(300));
    assert!(ring.administration_joins().is_empty());
    assert_eq!(child.words_printed(), 0);
    ring.quit();
}

#[test]
fn unregistered_windows_pair_is_refused_without_store_files_or_fallback() {
    let relay = Relay::start();
    let fresh = TestHome::new();
    let mut command = Command::new(win::bin());
    command
        .arg("pair")
        .args(["--relay", &relay.url()])
        .env("XSHELLD_HOME", fresh.home())
        .env("XSHELLD_SOCKET", &fresh.pipe);
    let mut child = PairChild::spawn(command);
    assert_eq!(child.finish(), 1);
    assert!(
        child
            .stderr()
            .contains("protected_counterpart_required: no registered xshell Desktop; open xshell"),
        "{}",
        child.stderr()
    );
    assert_eq!(child.words_printed(), 0);
    assert!(!fresh.home().join(".xshell").exists(), "no Store files");
    // An existing but unregistered Store: still refused, nothing written.
    let ring = fresh.home().join(".xshell").join("daemon").join("ring");
    for dir in [
        fresh.home().join(".xshell"),
        fresh.home().join(".xshell").join("daemon"),
        ring.clone(),
    ] {
        xshell_core::private_fs::ensure_dir(&dir).unwrap();
    }
    let mut command = Command::new(win::bin());
    command
        .arg("pair")
        .args(["--relay", &relay.url()])
        .env("XSHELLD_HOME", fresh.home())
        .env("XSHELLD_SOCKET", &fresh.pipe);
    let mut child = PairChild::spawn(command);
    assert_eq!(child.finish(), 1);
    assert!(child.stderr().contains("no registered xshell Desktop"));
    assert_eq!(std::fs::read_dir(&ring).unwrap().count(), 0);
}

/// BINDING AMENDMENT 1 §2 through the real entry points: a broad-ACL Store directory or
/// reservation file refuses Pair and Desktop registration with DACL and content unchanged.
#[test]
fn broad_acl_store_and_reservation_refuse_pair_and_registration_unchanged() {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let home = a.canonical_home();
    assert!(a.ring.reserve_local_companion(&home).unwrap());
    let sid = xshell_core::private_fs::user_sid_string().unwrap();
    let marker = a.daemon_dir().join(companion_reservation::FILE);
    let json = std::fs::read(&marker).unwrap();
    std::fs::remove_file(&marker).unwrap();
    {
        use std::io::Write;
        xshell_core::private_fs::create_file_with_sddl(
            &marker,
            &format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FR;;;WD)"),
        )
        .unwrap()
        .write_all(&json)
        .unwrap();
    }
    let acl = xshell_core::private_fs::security_sddl(&marker).unwrap();
    let mut child = a.pair(&relay.url(), 4_000);
    assert_eq!(child.finish(), 1, "stdout {:?}", child.stdout());
    assert!(child.stderr().contains("protected_counterpart_required"));
    a.daemon.ensure_spawned().unwrap();
    assert!(a.ring.register_local_companion(&a.daemon).is_err());
    assert_eq!(
        xshell_core::private_fs::security_sddl(&marker).unwrap(),
        acl
    );
    assert_eq!(std::fs::read(&marker).unwrap(), json);
    a.daemon.terminate(Duration::from_secs(10));

    // A broad Store directory: refused by Pair before any normalization.
    let b = WinComputer::new(&relay);
    let daemon = b.canonical_home().join(".xshell").join("daemon");
    xshell_core::private_fs::ensure_dir(&b.canonical_home().join(".xshell")).unwrap();
    xshell_core::private_fs::create_dir_with_sddl(
        &daemon,
        &format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FR;;;WD)"),
    )
    .unwrap();
    xshell_core::private_fs::ensure_dir(&daemon.join("ring")).unwrap();
    let before = xshell_core::private_fs::security_sddl(&daemon).unwrap();
    let mut child = b.pair(&relay.url(), 4_000);
    assert_eq!(child.finish(), 1);
    assert!(
        child.stderr().contains("unsafe local Store"),
        "{}",
        child.stderr()
    );
    assert!(b.ring.reserve_local_companion(&b.canonical_home()).is_err());
    assert_eq!(
        xshell_core::private_fs::security_sddl(&daemon).unwrap(),
        before
    );
}

// ── Attached console: real Ctrl+C / Ctrl+Break (Amendment 1 §4) ───────────

/// Not a test unless `XSHELL_X87_CONSOLE` is set: a console host for `xshelld pair` (this
/// helper runs in its own new console). After the first Device code it raises a real
/// console control event and records the CLI's exit code and output.
#[test]
fn helper_console_pair() {
    let Ok(mode) = std::env::var("XSHELL_X87_CONSOLE") else {
        return;
    };
    use windows_sys::Win32::System::Console::{
        GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };
    let var = |k: &str| std::env::var_os(k).unwrap();
    let out = PathBuf::from(var("XSHELL_X87_CONSOLE_OUT"));
    let mut command = Command::new(win::bin());
    command
        .arg("pair")
        .args(["--relay", var("XSHELL_X87_RELAY").to_str().unwrap()])
        .env("XSHELLD_HOME", var("XSHELLD_HOME"))
        .env("XSHELLD_SOCKET", var("XSHELLD_SOCKET"))
        .env("XSHELLD_PAIR_TTL_MS", "30000");
    if mode == "break" {
        // CREATE_NEW_PROCESS_GROUP: Ctrl+Break targets exactly the CLI's group.
        command.creation_flags(0x0000_0200);
    }
    let mut child = PairChild::spawn(command);
    let code = child.code();
    std::fs::write(out.with_extension("code"), code.format().as_str()).unwrap();
    // Raise only after the parent saw the offer presented to the owner.
    let go = out.with_extension("go");
    let until = Instant::now() + Duration::from_secs(20);
    while !go.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ok = unsafe {
        if mode == "break" {
            GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.child.id())
        } else {
            // Ignore it here (set after spawning: the ignore flag is inherited).
            SetConsoleCtrlHandler(None, 1);
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)
        }
    };
    let exit = child.finish();
    std::fs::write(
        &out,
        format!(
            "raised={ok}\nexit={exit}\nstdout={:?}\nstderr={}",
            child.stdout(),
            child.stderr()
        ),
    )
    .unwrap();
}

fn console_ctrl_cancels_offer(mode: &str) {
    let relay = Relay::start();
    let a = WinComputer::new(&relay);
    let a_profile = a.register();
    a.create(&a_profile);
    let b = WinComputer::new(&relay);
    b.register();
    let out = b.home.root().join(format!("console-{mode}.txt"));
    // CREATE_NEW_CONSOLE: the helper (and the CLI it starts) get a real attached console.
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "helper_console_pair", "--nocapture"])
        .env("XSHELL_X87_CONSOLE", mode)
        .env("XSHELL_X87_CONSOLE_OUT", &out)
        .env("XSHELL_X87_RELAY", relay.url())
        .env("XSHELLD_HOME", b.home.home())
        .env("XSHELLD_SOCKET", &b.home.pipe)
        .creation_flags(0x0000_0010)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait("console CLI printed its code", || {
        out.with_extension("code").exists()
    });
    let code =
        DeviceCode::parse(&std::fs::read_to_string(out.with_extension("code")).unwrap()).unwrap();
    let join = controller(&b.ring);
    assert!(join.code.format() == code.format());
    std::fs::write(out.with_extension("go"), b"").unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    while !out.exists() {
        assert!(Instant::now() < until, "console helper did not report");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = helper.wait();
    let report = std::fs::read_to_string(&out).unwrap();
    assert!(report.contains("raised=1"), "{report}");
    assert!(report.contains("exit=1"), "{report}");
    assert!(report.contains("Pairing cancelled"), "{report}");
    // The offer is invalidated: no approval can use it, no proof was released.
    wait("cancelled offer retired", || {
        matches!(join.join_status(), JoinStatus::Failed { .. })
            || b.ring.administration_joins().is_empty()
    });
    assert!(a.ring.approve_device_code(&code).is_err());
    assert!(!b.daemon_dir().join("roster.json").exists());
    assert!(b.ring.chain().is_none());
}

#[test]
fn attached_console_ctrl_c_cancels_the_offer_without_proof_release() {
    console_ctrl_cancels_offer("c");
}

#[test]
fn attached_console_ctrl_break_cancels_the_offer_without_proof_release() {
    console_ctrl_cancels_offer("break");
}

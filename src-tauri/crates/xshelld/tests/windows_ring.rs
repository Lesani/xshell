#![cfg(windows)]
//! Ring just works on Windows (Stage 2, W3/W8): the machine under test is a real
//! `xshelld.exe` started GUI-bound by hostlink's `LocalDaemon` (in its Job, on its own pipe)
//! plus a `DesktopRing` that registers with it over the named pipe the way the app's
//! registration loop does: no startup reservation, no registration child. The other
//! machines are in-process GUI-bound `fixture::Computer`s, all against the production Relay
//! under workerd (`WORKERD`, `XSHELL_ADOPTION_RELAY_DIR`). Run with `--test-threads=1`.
#[path = "desktop/windows_fixture.rs"]
#[allow(dead_code)]
mod fixture;
mod win;
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use fixture::{wait, Computer, Events, NoSsh, Quiet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use win::TestHome;
use workerd::Relay;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision, MembershipState};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::store::Store;
use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint, DesktopRing, DesktopRingConfig};
use xshell_hostlink::{CancelToken, LocalDaemon, LocalDaemonConfig, Manager, ManagerConfig};
use xshell_protocol::ring::companion::CompanionProfile;
use xshell_protocol::ring::{RosterChain, SignKey};

/// The Windows machine under test: its Desktop Store, its `LocalDaemon` (the real
/// `xshelld.exe`, GUI-bound in this process's Job) and its Desktop Ring.
struct WinMachine {
    home: TestHome,
    daemon: Arc<LocalDaemon>,
    ring: Arc<DesktopRing>,
    manager: Arc<Manager>,
}

impl WinMachine {
    fn new(relay: &Relay) -> Self {
        Self::on(TestHome::new(), relay, None)
    }

    fn on(home: TestHome, relay: &Relay, hook: Option<AdoptionTestHook>) -> Self {
        let daemon = Arc::new(
            LocalDaemon::new(LocalDaemonConfig {
                bin: win::bin().into(),
                env: home.daemon_env(),
                home: home.home(),
                socket: home.pipe.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                build: None,
            })
            .unwrap(),
        );
        daemon.set_persistent(false);
        let ring = open_ring(&home, relay, hook);
        let events = Arc::new(Events::default());
        let mut cfg = ManagerConfig::new("1.5.0", Arc::new(NoSsh), Arc::new(NoSsh), events);
        cfg.backoff_unit = Duration::from_millis(20);
        let manager = Arc::new(Manager::new(cfg));
        ring.attach_manager(&manager).unwrap();
        let m = Self {
            home,
            daemon,
            ring,
            manager,
        };
        m.start_daemon();
        m
    }

    /// What the Local Host's dialer does: connect, starting the GUI-bound Daemon when
    /// nothing listens.
    fn start_daemon(&self) {
        drop(self.daemon.connect_or_start(&CancelToken::new()).unwrap());
        assert!(
            self.daemon.gui_pid().is_some(),
            "this process owns the Daemon"
        );
    }

    fn ring_dir(&self) -> std::path::PathBuf {
        self.home.paths().ring_dir
    }

    /// The app's registration loop: wait-only over the live Daemon's pipe.
    fn register(&self) -> CompanionProfile {
        self.ring
            .register_local_companion(&self.daemon)
            .unwrap()
            .expect("the elected owner registers")
    }

    /// The client `ring_create_computer` / `ring_join_start` use.
    fn client(&self) -> CompanionClient {
        self.ring.local_companion_client().unwrap()
    }

    fn create(&self) -> RosterChain {
        self.register();
        match self
            .ring
            .create_computer(&self.client(), |p, lease| {
                Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
            })
            .unwrap()
        {
            AdoptionOutcome::Installed { chain, .. } => chain,
            _ => panic!("Create not installed"),
        }
    }

    fn desktop_key(&self) -> SignKey {
        Store::new(self.home.root().join("desktop"))
            .load_profile()
            .unwrap()
            .0
            .unwrap()
            .keys
            .sign_key()
    }

    /// The Daemon's durable chain (`roster.json`), if it holds one.
    fn daemon_chain(&self) -> Option<RosterChain> {
        let bytes = std::fs::read(self.ring_dir().join("roster.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let tokens: Vec<String> = serde_json::from_value(v["rosters"].clone()).unwrap();
        Some(RosterChain::from_tokens(&tokens).unwrap())
    }

    fn wait_daemon_set_aside(&self) {
        let dir = self.ring_dir();
        wait("the Daemon set its membership aside", || {
            !dir.join("roster.json").exists() && files(&dir, "roster.json.removed-") > 0
        });
    }

    /// The Daemon process dies (as if killed): the dialer starts it again in our Job.
    fn kill_daemon(&self) {
        let pid = self.daemon.gui_pid().expect("our Daemon runs");
        win::kill_pid(pid);
        assert!(win::wait_dead(pid, Duration::from_secs(10)));
        let until = Instant::now() + fixture::WAIT;
        loop {
            match self.daemon.connect_or_start(&CancelToken::new()) {
                Ok(s) => {
                    drop(s);
                    break;
                }
                Err(e) => assert!(Instant::now() < until, "Daemon not restarted: {e:?}"),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_ne!(self.daemon.gui_pid(), Some(pid));
    }
}

impl Drop for WinMachine {
    fn drop(&mut self) {
        self.ring.quit();
        self.manager.shutdown();
        self.daemon.hang_up();
        self.daemon.reap(Instant::now() + Duration::from_secs(10));
    }
}

fn open_ring(home: &TestHome, relay: &Relay, hook: Option<AdoptionTestHook>) -> Arc<DesktopRing> {
    let mut cfg = DesktopRingConfig::new(home.root().join("desktop"), "windows machine".into());
    cfg.default_relay_url = relay.url();
    cfg.owner_retry = Duration::from_millis(20);
    cfg.backoff_unit = Duration::from_millis(10);
    cfg.adoption_test_hook = hook;
    DesktopRing::open(cfg, Arc::new(Quiet))
}

fn files(dir: &std::path::Path, prefix: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
                .count()
        })
        .unwrap_or(0)
}

/// `guest` joins with words `approver` types (no confirmation on the guest).
fn join_computer(guest: &Computer, approver: &DesktopRing) -> RosterChain {
    let companion = guest.companion();
    guest.ring.register_companion(&companion).unwrap();
    let offer = guest.ring.begin_join(&companion).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let approval = scope.spawn(|| approver.approve_device_code(&code));
        let outcome = offer.wait().unwrap();
        approval.join().unwrap().unwrap();
        match outcome {
            AdoptionOutcome::Installed { chain, .. } => chain,
            _ => panic!("Join not installed"),
        }
    })
}

/// The Windows machine joins with words `approver` types.
fn join_windows(guest: &WinMachine, approver: &Computer) -> RosterChain {
    guest.register();
    let offer = guest.ring.begin_join(&guest.client()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let approval = scope.spawn(|| approver.ring.approve_device_code(&code));
        let outcome = offer.wait().unwrap();
        approval.join().unwrap().unwrap();
        match outcome {
            AdoptionOutcome::Installed { chain, .. } => chain,
            _ => panic!("Join not installed"),
        }
    })
}

#[test]
fn registers_over_pipe_without_reservation() {
    let relay = Relay::start();
    let w = WinMachine::new(&relay);
    assert!(w.ring.companion_registration_needed());
    let profile = w.register();
    assert!(!w.ring.companion_registration_needed());
    // No startup reservation anywhere: no marker in the Daemon store, nothing under the
    // home's legacy reservation path either.
    assert!(!w.ring_dir().join("companion-pending.json").exists());
    assert_eq!(files(&w.home.home(), "companion-pending"), 0);
    // The profile names the Daemon's counterpart pipe for its canonical store.
    let store = xshell_core::local_ipc::canonical_store(&w.ring_dir()).unwrap();
    let pipe = xshell_core::local_ipc::companion_endpoint(&store).unwrap();
    assert_eq!(std::path::Path::new(&profile.endpoint), pipe.as_path());
    assert!(
        profile
            .endpoint
            .starts_with(r"\\.\pipe\xshell-companion-v1-"),
        "{}",
        profile.endpoint
    );
    assert_eq!(std::path::Path::new(&profile.daemon_store), store.as_path());
    // The client the Create/Join commands use reaches the same Daemon.
    assert_eq!(w.client().endpoint(), pipe.as_path());
    // Registering again is idempotent.
    assert_eq!(w.register().daemon.sign_key, profile.daemon.sign_key);
}

/// Round 4: on Windows too, a Join whose Relay cannot be reached reports that error, not
/// "cancelled" (the GUI showed "cancelled when you switched away" within 2 s).
#[test]
fn join_start_with_an_unreachable_relay_reports_its_error_not_a_cancel() {
    let relay = Relay::start();
    let w = WinMachine::new(&relay);
    w.register();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cancel = Arc::new(AtomicBool::new(false));
    let error = w
        .ring
        .begin_join_cancellable(
            &w.client(),
            cancel.clone(),
            Some(&format!("ws://127.0.0.1:{port}")),
        )
        .err()
        .expect("an unreachable Relay never shows words");
    assert!(!error.starts_with("cancelled:"), "{error}");
    assert!(!cancel.load(Ordering::Acquire));
    // A start that is not cancelled shows its words and keeps them.
    let offer = w
        .ring
        .begin_join_cancellable(&w.client(), Arc::new(AtomicBool::new(false)), None)
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !matches!(
            offer.join_status(),
            xshell_hostlink::ring::adoption::JoinStatus::Failed { .. }
        ),
        "{:?}",
        offer.join_status()
    );
    offer.cancel_join();
}

#[test]
fn create_ring_from_windows_desktop() {
    let relay = Relay::start();
    let w = WinMachine::new(&relay);
    let chain = w.create();
    let head = chain.head();
    assert_eq!(w.ring.membership_state().unwrap(), MembershipState::Member);
    // Both of this machine's stores are enrolled: Desktop and Daemon.
    assert_eq!(head.roster().members.len(), 2);
    assert!(chain.authorized_member(&w.desktop_key()).is_some());
    assert_eq!(w.daemon_chain().unwrap().head(), head);
    // Another computer joins (approved here) and reaches this Daemon through the Relay.
    let b = Computer::new(&relay);
    join_computer(&b, &w.ring);
    let daemon_key = w.register().daemon.sign_key;
    assert!(b.relay_host(daemon_key).status().usable());
}

#[test]
fn join_without_confirmation() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let w = WinMachine::new(&relay);
    let chain = join_windows(&w, &a);
    assert_eq!(w.ring.membership_state().unwrap(), MembershipState::Member);
    assert_eq!(w.ring.chain().unwrap().head(), chain.head());
    assert_eq!(w.daemon_chain().unwrap().head(), chain.head());
    assert!(chain.authorized_member(&w.desktop_key()).is_some());
}

#[test]
fn approve_typed_words_from_windows() {
    let relay = Relay::start();
    let w = WinMachine::new(&relay);
    w.create();
    let b = Computer::new(&relay);
    let chain = join_computer(&b, &w.ring);
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::Member);
    assert_eq!(chain.head().version(), 2);
    wait("the Windows Desktop sees the new head", || {
        w.ring
            .chain()
            .is_some_and(|c| c.head().version() == chain.head().version())
    });
}

#[test]
fn remove_other_member_from_windows() {
    let relay = Relay::start();
    let w = WinMachine::new(&relay);
    w.create();
    let b = Computer::new(&relay);
    join_computer(&b, &w.ring);
    let b_key = Store::new(b.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .keys
        .sign_key();
    let chain = w.ring.remove_member(&b_key).unwrap();
    assert!(chain.authorized_member(&b_key).is_none());
    wait("b detected its removal", || {
        b.ring.membership_state().unwrap() == MembershipState::Removed
    });
}

#[test]
fn remove_self_rotates_and_reregisters() {
    let relay = Relay::start();
    let w = WinMachine::new(&relay);
    let first = w.create();
    let b = Computer::new(&relay);
    join_computer(&b, &w.ring);
    let before = w.desktop_key();
    let daemon_keys_before = std::fs::read(w.ring_dir().join("keys.json")).unwrap();
    let chain = w.ring.remove_member(&before).unwrap();
    assert!(chain.authorized_member(&before).is_none());
    // The Desktop: in no Ring, a new identity.
    assert_eq!(w.ring.membership_state().unwrap(), MembershipState::None);
    assert!(w.ring.chain().is_none());
    assert_ne!(w.desktop_key(), before);
    // The Daemon: membership and identity set aside too.
    w.wait_daemon_set_aside();
    assert_ne!(
        std::fs::read(w.ring_dir().join("keys.json")).unwrap_or_default(),
        daemon_keys_before
    );
    // The same process registers its new identity again (B1) and creates a new Ring.
    wait("registration needed after the set-aside", || {
        w.ring.companion_registration_needed()
    });
    w.register();
    assert!(!w.ring.companion_registration_needed());
    let second = w.create();
    assert_ne!(second.ring_id(), first.ring_id());
}

#[test]
fn self_detected_removal_while_gui_bound() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let w = WinMachine::new(&relay);
    join_windows(&w, &a);
    wait("the Windows Desktop is connected", || {
        w.ring.view().connection == "connected"
    });
    let old = w.desktop_key();
    a.ring.remove_member(&old).unwrap();
    // Both sides learn it by themselves from the signed history.
    let until = Instant::now() + Duration::from_secs(30);
    while w.ring.membership_state().unwrap() != MembershipState::Removed || w.ring.chain().is_some()
    {
        assert!(
            Instant::now() < until,
            "the Desktop never detected its removal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let notice = w.ring.notices().unwrap().0.expect("removal notice");
    assert_eq!(&notice.ring_id, a.ring.chain().unwrap().ring_id());
    w.wait_daemon_set_aside();
    // And joins again in the same process.
    wait("registration needed after the set-aside", || {
        w.ring.companion_registration_needed()
    });
    let chain = join_windows(&w, &a);
    assert!(chain.authorized_member(&old).is_none());
    assert!(w.ring.notices().unwrap().0.is_none(), "the notice clears");
}

#[test]
fn daemon_killed_restarts_and_keeps_registration() {
    let relay = Relay::start();
    let w = WinMachine::new(&relay);
    let chain = w.create();
    let profile = w.register();
    w.kill_daemon();
    // The stored registration survives the restart: nothing to register again, and the
    // restarted Daemon answers with the same profile and chain.
    wait("the restarted Daemon answers on its pipe", || {
        w.client()
            .request(xshell_protocol::ring::companion::CompanionRequest::Descriptor)
            .is_ok()
    });
    assert!(!w.ring.companion_registration_needed());
    assert_eq!(w.register().daemon.sign_key, profile.daemon.sign_key);
    assert_eq!(w.daemon_chain().unwrap().head(), chain.head());
    assert_eq!(w.ring.membership_state().unwrap(), MembershipState::Member);
}

#[test]
fn crash_after_accepted_reconciles() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    // The joining Desktop "dies" after the accepted chain is journalled, before the Daemon
    // installs it (the panic unwinds the join like a process death would end it).
    let cut = Arc::new(AtomicBool::new(true));
    let hook = {
        let cut = cut.clone();
        AdoptionTestHook(Arc::new(move |point| {
            if point == AdoptionTestPoint::AcceptedBeforeCompanionInstall
                && cut.swap(false, Ordering::SeqCst)
            {
                panic!("simulated Desktop death after acceptance");
            }
        }))
    };
    let mut w = WinMachine::on(TestHome::new(), &relay, Some(hook));
    w.register();
    let offer = w.ring.begin_join(&w.client()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let approval = scope.spawn(|| a.ring.approve_device_code(&code));
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| offer.wait()));
        assert!(
            !matches!(ended, Ok(Ok(AdoptionOutcome::Installed { .. }))),
            "the cut did not happen"
        );
        approval.join().unwrap().unwrap();
    });
    assert!(!cut.load(Ordering::SeqCst));
    assert!(w.daemon_chain().is_none(), "the Daemon never installed it");
    // Desktop and Daemon both go; the next start finishes the Join by itself.
    w.ring.quit();
    w.kill_daemon();
    w.ring = open_ring(&w.home, &relay, None);
    w.ring.attach_manager(&w.manager).unwrap();
    w.ring.start_reconciler();
    let until = Instant::now() + Duration::from_secs(60);
    while w.ring.membership_state().unwrap() != MembershipState::Member {
        assert!(Instant::now() < until, "the Join was never finished");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The reopened Desktop's in-memory chain follows the store a moment later.
    wait("the finished Join's chain is loaded", || {
        w.ring.chain().is_some()
    });
    let chain = w.ring.chain().unwrap();
    wait("the Daemon holds the finished chain", || {
        w.daemon_chain().is_some_and(|d| d.head() == chain.head())
    });
    assert!(chain.authorized_member(&w.desktop_key()).is_some());
}

#[test]
fn stale_x87_reservation_is_set_aside() {
    let relay = Relay::start();
    let home = TestHome::new();
    // An unmerged x87 build left its startup reservation in the Daemon store.
    let ring_dir = home.paths().ring_dir;
    xshell_core::private_fs::ensure_dir(ring_dir.parent().unwrap().parent().unwrap()).unwrap();
    xshell_core::private_fs::ensure_dir(ring_dir.parent().unwrap()).unwrap();
    xshell_core::private_fs::ensure_dir(&ring_dir).unwrap();
    {
        use std::io::Write;
        let mut f =
            xshell_core::private_fs::create_new(&ring_dir.join("companion-pending.json")).unwrap();
        f.write_all(br#"{"desktopStore":"C:\\old","legacyOutcomeUncertain":false}"#)
            .unwrap();
    }
    let w = WinMachine::on(home, &relay, None);
    wait("the stale reservation was renamed aside", || {
        !w.ring_dir().join("companion-pending.json").exists()
            && files(&w.ring_dir(), "companion-pending.json.v1-") == 1
    });
    // It no longer blocks registration or Create.
    w.register();
    w.create();
    assert_eq!(w.ring.membership_state().unwrap(), MembershipState::Member);
}

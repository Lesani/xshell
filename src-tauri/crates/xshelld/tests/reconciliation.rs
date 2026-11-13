#![cfg(unix)]
//! Runtime5 owned-controller SIGKILL evidence against public production workerd setup.
mod common;
#[allow(dead_code)]
#[path = "desktop/relay_fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "desktop/workerd.rs"]
mod workerd;
use fixture::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use xshell_hostlink::ring::adoption::{
    AdoptionRecoveryState, CreateDecision, PublicationRecoveryState,
};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::store::{AdoptionPhase, Store};
use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint, DesktopRing, DesktopRingConfig};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::relay::{
    ByeReason, CloseReason, RingClient, RingClientConfig, RingEvents,
};
use xshell_protocol::ring::{RosterChain, SignKey, SignedRoster};

// Executed only by this file's owned parent tests. No process in this helper owns the
// Daemon/workerd: SIGKILL kills exactly the Desktop controller, not a process group.
#[test]
fn owned_controller_child() {
    let Some(root) = std::env::var_os("XSHELL_R5_CHILD_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let cut = std::env::var("XSHELL_R5_CUT").unwrap();
    let mode = std::env::var("XSHELL_R5_MODE").unwrap();
    let marker = root.join("controller-cut");
    let mut cfg = DesktopRingConfig::new(root.join("desktop"), "same name".into());
    cfg.default_relay_url = std::env::var("XSHELL_R5_RELAY").unwrap();
    cfg.adoption_test_hook = Some(AdoptionTestHook(Arc::new(move |p| {
        let hit = match cut.as_str() {
            "accepted-before-journal" => p == AdoptionTestPoint::AcceptedBeforeJournal,
            "between-stores" => p == AdoptionTestPoint::CompanionInstalledBeforeDesktop,
            "before-publication" => p == AdoptionTestPoint::ApprovalBeforePublish,
            _ => panic!("unknown cut"),
        };
        if hit {
            std::fs::write(&marker, format!("{p:?}")).unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    })));
    let ring = DesktopRing::open(cfg, Arc::new(Quiet));
    let companion = CompanionClient::new(std::env::var_os("XSHELL_R5_ENDPOINT").unwrap().into());
    let confirm = |p: &xshell_hostlink::ring::adoption::ConsentPreview,
                   l: &xshell_hostlink::ring::adoption::CreateConfirmationLease| {
        Ok(CreateDecision::Confirm(l.confirm(&p.proposal_hash)?))
    };
    match mode.as_str() {
        "create" => {
            ring.create_computer(&companion, confirm).unwrap();
        }
        "join" => {
            let offer = ring.begin_join(&companion).unwrap();
            // One credential to the invoking parent pipe only; never written to an artifact.
            println!("WORDS:{}", offer.code.format().as_str());
            std::io::stdout().flush().unwrap();
            let (preview, lease) = offer.await_consent(WAIT).unwrap();
            offer
                .confirm_join(lease.confirm(&preview.proposal_hash).unwrap())
                .unwrap();
            offer.wait().unwrap();
        }
        "upgrade" => {
            wait("child existing owner connected", || {
                ring.view().connection == "connected"
            });
            ring.upgrade_retained_ring(&companion, confirm).unwrap();
        }
        "finish" => {
            wait("child retained owner connected", || {
                ring.view().connection == "connected"
            });
            ring.finish_retained_computer(&companion, confirm).unwrap();
        }
        "approve" => {
            wait("child existing approver connected", || {
                ring.view().connection == "connected"
            });
            let mut words = String::new();
            std::io::stdin().read_line(&mut words).unwrap();
            let code = DeviceCode::parse(words.trim()).unwrap();
            ring.approve_device_code(&code).unwrap();
        }
        _ => panic!("unknown child mode"),
    }
    panic!("controller returned without reaching requested cut");
}
struct OwnedController(Child);
impl Drop for OwnedController {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn spawn_controller(
    c: &Computer,
    relay: &workerd::Relay,
    mode: &str,
    cut: &str,
) -> OwnedController {
    c.ring.quit();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "owned_controller_child", "--nocapture"])
        .env("XSHELL_R5_CHILD_ROOT", c.home.root())
        .env("XSHELL_R5_RELAY", relay.url())
        .env("XSHELL_R5_ENDPOINT", c.companion().endpoint())
        .env("XSHELL_R5_MODE", mode)
        .env("XSHELL_R5_CUT", cut)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    OwnedController(child)
}
fn kill_at_cut(child: &mut OwnedController, c: &Computer) {
    wait("real controller reached exact cut", || {
        c.home.root().join("controller-cut").is_file()
    });
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "controller must still be alive at cut"
    );
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL));
}
fn restart_controller(c: &mut Computer, relay: &workerd::Relay) {
    let mut cfg = DesktopRingConfig::new(c.home.root().join("desktop"), "same name".into());
    cfg.default_relay_url = relay.url();
    cfg.backoff_unit = Duration::from_millis(10);
    c.ring = DesktopRing::open(cfg, Arc::new(Quiet));
    assert!(c.ring.pairing_ipc_owner());
}
fn stored(c: &Computer) -> xshell_hostlink::ring::store::ProfileState {
    Store::new(c.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
}
fn process_recovery(mode: &str, cut: &str, descendant: bool, revoke: bool) {
    let relay = workerd::Relay::start();
    let a = Computer::new(&relay);
    if mode == "join" {
        a.create();
    }
    let mut b = Computer::new(&relay);
    let profile = b.ring.register_companion(&b.companion()).unwrap();
    let keys = stored(&b).keys;
    let daemon_keys = std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap();
    let terminal = uuid::Uuid::new_v4();
    let project = b.home.project("r5-preserved-shell");
    let mut local = common::Client::connect(&b.daemon.as_ref().unwrap().socket);
    local.hello(common::range(1, 1));
    let opened = local.open(terminal, common::sh_spec(&project));
    let pid = opened["pid"].as_u64().unwrap() as i32;
    local.attach(terminal);
    let mut child = spawn_controller(&b, &relay, mode, cut);
    let accepted = if mode == "join" {
        let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
        let code = loop {
            let mut line = String::new();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "child exited before words"
            );
            if let Some(words) = line.trim().strip_prefix("WORDS:") {
                break DeviceCode::parse(words).unwrap();
            }
        };
        a.ring.approve_device_code(&code).unwrap().chain
    } else {
        // The exact candidate is durable before publication; the marker proves positive
        // readback returned, not merely that the candidate was signed.
        wait("Create accepted readback", || {
            b.home.root().join("controller-cut").exists()
        });
        let j = stored(&b).journal.unwrap();
        RosterChain::from_chain(vec![SignedRoster::parse(
            j.signed_candidate.as_ref().unwrap(),
        )
        .unwrap()])
        .unwrap()
    };
    kill_at_cut(&mut child, &b);
    let before = stored(&b);
    let j = before.journal.unwrap();
    assert!(before.membership.is_none());
    assert!(!j.desktop_installed && !j.daemon_installed);
    assert_eq!(j.intent.target, profile.target());
    if cut == "accepted-before-journal" {
        assert!(j.accepted.is_empty());
        assert_eq!(
            j.phase,
            if mode == "join" {
                AdoptionPhase::ConsentMayHaveLeft
            } else {
                AdoptionPhase::PublicationUncertain
            }
        );
        if mode == "join" {
            assert!(
                j.signed_candidate.is_none(),
                "guest recovery must use predecessor, not an ACK hash"
            );
        }
    } else {
        assert_eq!(j.phase, AdoptionPhase::Accepted);
        assert!(b.home.paths().ring_dir.join("roster.json").is_file());
    }
    let operation = j.intent.operation.clone();
    let expiry = j.intent.expires_at;
    let mut expected = accepted.clone();
    if descendant {
        let c = Computer::new(&relay);
        expected = c.join(&a); // independent authorized successor, not controller-state injection
        assert_eq!(expected.head().version(), 3);
    }
    if revoke {
        expected = remove_computer(&a, &accepted, profile.desktop.sign_key);
        wait("counterpart independently received removal", || {
            std::fs::read_to_string(b.home.paths().ring_dir.join("roster.json"))
                .unwrap()
                .contains(expected.head().token())
        });
    }
    restart_controller(&mut b, &relay);
    let state = b
        .ring
        .reconcile_adoption(&b.companion(), &operation)
        .unwrap()
        .local
        .unwrap();
    if revoke {
        assert_eq!(state.state, AdoptionRecoveryState::Terminal);
        assert!(!state.desktop_installed);
        assert!(
            stored(&b).membership.is_none(),
            "revocation must not revive Desktop"
        );
        assert!(b.ring.begin_join(&b.companion()).is_err());
    } else {
        assert_eq!(state.state, AdoptionRecoveryState::Installed);
        assert!(state.desktop_installed && state.daemon_installed);
        assert_eq!(stored(&b).membership.unwrap().chain.head(), expected.head());
        wait("recovered normal owner connected", || {
            b.ring.view().connection == "connected"
        });
        let again = b
            .ring
            .reconcile_adoption(&b.companion(), &operation)
            .unwrap();
        assert_eq!(again.local.unwrap().state, AdoptionRecoveryState::Installed);
    }
    let after = stored(&b);
    assert_eq!(after.keys.sign_key(), keys.sign_key());
    assert_eq!(after.keys.noise_key(), keys.noise_key());
    assert_eq!(after.journal.unwrap().intent.expires_at, expiry);
    assert_eq!(
        std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap(),
        daemon_keys
    );
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), expected.ring_id()),
        expected.head().version()
    );
    assert!(common::alive(pid));
    local.input(terminal, "printf 'R5-SAME-TERMINAL\\n'\n");
    local.output_until(terminal, "R5-SAME-TERMINAL");
}
#[test]
fn accepted_before_controller_journal_create_process_death_recovers_exact_genesis() {
    process_recovery("create", "accepted-before-journal", false, false);
}
#[test]
fn between_store_writes_create_process_death_retries_only_missing_installation() {
    process_recovery("create", "between-stores", false, false);
}
#[test]
fn accepted_before_controller_journal_join_process_death_uses_predecessor_and_current_descendant() {
    process_recovery("join", "accepted-before-journal", true, false);
}
#[test]
fn between_store_writes_join_process_death_then_revocation_never_revives() {
    process_recovery("join", "between-stores", false, true);
}

struct ProtocolEvents;
impl RingEvents for ProtocolEvents {
    fn envelope(&self, _: SignKey, _: Vec<u8>) {}
    fn closed(&self, _: CloseReason) {}
    fn presence(&self, _: SignKey, _: xshell_protocol::ring::relay::MemberPresence) {}
    fn roster(&self, _: &SignedRoster) {}
    fn roster_rejected(&self, _: xshell_protocol::ring::RosterError) {}
}
fn remove_computer(a: &Computer, chain: &RosterChain, key: SignKey) -> RosterChain {
    // Public v2 lifecycle UI is later. This independent mutation uses the actual retained
    // approver signer after its owner is stopped: no injected rows/competing client.
    a.ring.quit();
    a.manager.shutdown();
    let keys = stored(a).keys;
    let client = RingClient::connect(
        RingClientConfig::new(chain.clone(), keys.clone()),
        Arc::new(ProtocolEvents),
    )
    .unwrap();
    let mut roster = chain.head().roster().clone();
    roster.version += 1;
    roster.prev = Some(chain.head().hash());
    let mut membership = roster.membership().unwrap();
    let unit = membership.unit_for(&key).unwrap().clone();
    membership.enrollments.retain(|e| e.id != unit.id);
    membership.retired.push(unit.id.clone());
    membership.change = xshell_protocol::ring::membership::RosterChange::Remove {
        enrollment_id: unit.id,
    };
    roster
        .members
        .retain(|m| !unit.members.contains(&m.sign_key));
    roster.set_membership(&membership);
    let signed = roster.sign(keys.as_ref()).unwrap();
    let accepted = chain.extended(std::slice::from_ref(&signed)).unwrap().0;
    client.publish_roster(&signed).unwrap();
    client.bye(ByeReason::quit()).unwrap();
    accepted
}

#[test]
fn upgrade_process_death_after_wire_ack_before_controller_journal_recovers_publication_only() {
    let relay = workerd::Relay::start();
    let mut b = Computer::new(&relay);
    // Genuine public retained v1 setup, deliberately no inferred counterpart grouping.
    b.ring.enable(None, false).unwrap();
    wait("legacy owner connected", || {
        b.ring.view().connection == "connected"
    });
    b.ring.register_companion(&b.companion()).unwrap();
    let mut child = spawn_controller(&b, &relay, "upgrade", "accepted-before-journal");
    kill_at_cut(&mut child, &b);
    let j = stored(&b).outgoing.unwrap();
    assert_eq!(
        j.phase,
        xshell_hostlink::ring::store::OutgoingPhase::PublicationUncertain
    );
    assert!(j.accepted.is_empty());
    let candidate = j.signed_candidate.clone().unwrap();
    restart_controller(&mut b, &relay);
    wait("restarted normal owner connected", || {
        b.ring.view().connection == "connected"
    });
    let recovered = b
        .ring
        .reconcile_adoption(&b.companion(), &j.intent.operation)
        .unwrap();
    assert_eq!(
        recovered.publication.unwrap().state,
        PublicationRecoveryState::Published
    );
    let p = stored(&b);
    assert_eq!(p.outgoing.unwrap().accepted[1], candidate);
    assert_eq!(p.membership.unwrap().chain.head().version(), 2);
    assert!(
        !b.home.paths().ring_dir.join("roster.json").exists(),
        "Upgrade does not enroll unpaired counterpart"
    );
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &relay.target(),
            b.ring.chain().unwrap().ring_id()
        ),
        2
    );
}

#[test]
fn approver_process_death_after_acceptance_recovers_standalone_lost_controller_ack() {
    use std::sync::{atomic::AtomicBool, mpsc};
    use xshelld::server::adoption::StandaloneRecoveryState;
    let relay = workerd::Relay::start();
    let mut a = Computer::new(&relay);
    a.create();
    let mut d = Computer::new(&relay); // deliberately unregistered: no companion registration
    let (tx, rx) = mpsc::channel();
    let mut controller = spawn_controller(&a, &relay, "approve", "accepted-before-journal");
    let url = relay.url();
    std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            d.daemon.as_ref().unwrap().adopt_standalone(
                &url,
                WAIT,
                &Arc::new(AtomicBool::new(false)),
                |code| tx.send(code.clone()).unwrap(),
                |p, lease| Ok(Some(lease.confirm(&p.proposal_hash)?)),
            )
        });
        let code = rx.recv_timeout(WAIT).unwrap();
        writeln!(
            controller.0.stdin.take().unwrap(),
            "{}",
            code.format().as_str()
        )
        .unwrap();
        kill_at_cut(&mut controller, &a);
        let error = guest.join().unwrap().unwrap_err();
        assert!(error.starts_with("reconciliation_required:"), "{error}");
    });
    let before: serde_json::Value = serde_json::from_slice(
        &std::fs::read(d.home.paths().ring_dir.join("standalone-adoption.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(before["accepted"], serde_json::json!([]));
    assert_eq!(before["installed"], false);
    assert!(!d.home.paths().ring_dir.join("roster.json").exists());
    let outgoing = stored(&a).outgoing.unwrap();
    assert!(outgoing.accepted.is_empty());
    restart_controller(&mut a, &relay);
    wait("restarted existing approver connected", || {
        a.ring.view().connection == "connected"
    });
    let status = a
        .ring
        .reconcile_adoption(&a.companion(), &outgoing.intent.operation)
        .unwrap();
    assert_eq!(
        status.publication.unwrap().state,
        PublicationRecoveryState::Published
    );
    let daemon_keys = std::fs::read(d.home.paths().ring_dir.join("keys.json")).unwrap();
    d.daemon.take().unwrap().shutdown();
    let mut cfg = xshelld::server::Config::new(d.home.home(), d.home.paths());
    cfg.ring_backoff_unit = Duration::from_millis(10);
    d.daemon = Some(xshelld::server::Server::start(cfg).unwrap());
    assert_eq!(
        d.daemon
            .as_ref()
            .unwrap()
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        StandaloneRecoveryState::ReconciliationRequired
    );
    let recovered = d
        .daemon
        .as_ref()
        .unwrap()
        .reconcile_standalone_adoption()
        .unwrap();
    assert_eq!(recovered.state, StandaloneRecoveryState::Installed);
    assert_eq!(
        recovered.operation,
        before["intent"]["operation"].as_str().unwrap()
    );
    assert_eq!(
        recovered.expires_at,
        before["intent"]["expiresAt"].as_u64().unwrap()
    );
    wait_daemon_online(&d);
    let again = d
        .daemon
        .as_ref()
        .unwrap()
        .reconcile_standalone_adoption()
        .unwrap();
    assert_eq!(again.state, StandaloneRecoveryState::Installed);
    let after: serde_json::Value = serde_json::from_slice(
        &std::fs::read(d.home.paths().ring_dir.join("standalone-adoption.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(after["proposal"], before["proposal"]);
    assert_eq!(after["intent"], before["intent"]);
    assert_eq!(after["installed"], true);
    assert_eq!(
        std::fs::read(d.home.paths().ring_dir.join("keys.json")).unwrap(),
        daemon_keys
    );
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &relay.target(),
            a.ring.chain().unwrap().ring_id()
        ),
        2
    );
    assert!(!d.home.paths().ring_dir.join("companion.json").exists());
}

fn wait_daemon_online(d: &Computer) {
    let local = std::sync::Mutex::new(common::Client::connect(&d.daemon.as_ref().unwrap().socket));
    local.lock().unwrap().hello(common::range(1, 1));
    wait("Daemon normal owner connected", || {
        local
            .lock()
            .unwrap()
            .request(&xshell_protocol::msg::ClientMsg::RingIdentity)
            .unwrap()["ring"]["state"]
            == "connected"
    });
}

#[test]
fn killed_before_publication_and_expired_stays_uncertain_without_absence_inference() {
    use std::sync::{atomic::AtomicBool, mpsc};
    use xshelld::server::adoption::StandaloneRecoveryState;
    let relay = workerd::Relay::start();
    let mut a = Computer::new(&relay);
    let original = a.create();
    let d = Computer::new(&relay);
    let (tx, rx) = mpsc::channel();
    let mut controller = spawn_controller(&a, &relay, "approve", "before-publication");
    let url = relay.url();
    std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            d.daemon.as_ref().unwrap().adopt_standalone(
                &url,
                Duration::from_secs(3),
                &Arc::new(AtomicBool::new(false)),
                |code| tx.send(code.clone()).unwrap(),
                |p, lease| Ok(Some(lease.confirm(&p.proposal_hash)?)),
            )
        });
        let code = rx.recv_timeout(WAIT).unwrap();
        writeln!(
            controller.0.stdin.take().unwrap(),
            "{}",
            code.format().as_str()
        )
        .unwrap();
        kill_at_cut(&mut controller, &a);
        assert!(guest
            .join()
            .unwrap()
            .unwrap_err()
            .starts_with("reconciliation_required:"));
    });
    let outgoing = stored(&a).outgoing.unwrap();
    assert!(outgoing.signed_candidate.is_some() && outgoing.accepted.is_empty());
    restart_controller(&mut a, &relay);
    wait("uncertain approver normal owner connected", || {
        a.ring.view().connection == "connected"
    });
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        < outgoing.intent.expires_at
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(a
        .ring
        .reconcile_adoption(&a.companion(), &outgoing.intent.operation)
        .is_err());
    assert_eq!(
        a.ring.adoption_status().unwrap().publication.unwrap().state,
        PublicationRecoveryState::ReconciliationRequired
    );
    let after = stored(&a).outgoing.unwrap();
    assert_eq!(
        after, outgoing,
        "expiry/readback at predecessor does not prove nonpublication or permit replay"
    );
    assert!(
        d.daemon
            .as_ref()
            .unwrap()
            .reconcile_standalone_adoption()
            .is_err(),
        "authentication refusal is not absence"
    );
    assert_eq!(
        d.daemon
            .as_ref()
            .unwrap()
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        StandaloneRecoveryState::ReconciliationRequired
    );
    assert!(
        d.ring.begin_join(&d.companion()).is_err(),
        "shared unresolved obligation still fences replacement"
    );
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), original.ring_id()),
        1
    );
}

#[test]
fn standalone_actual_store_write_failure_retries_same_accepted_bytes_and_receipt() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use xshelld::server::adoption::StandaloneRecoveryState;
    use xshelld::server::{TestHook, TestPoint};
    let relay = workerd::Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let fail = Arc::new(AtomicBool::new(true));
    let hit = Arc::new(AtomicBool::new(false));
    let d = Computer::with_config(&relay, |cfg| {
        let fail = fail.clone();
        let hit = hit.clone();
        cfg.test_hook = Some(TestHook(Arc::new(move |_, p| {
            if p == TestPoint::BeforeRingRosterWrite && fail.load(Ordering::Acquire) {
                hit.store(true, Ordering::Release);
                true
            } else {
                false
            }
        })));
    });
    let (tx, rx) = mpsc::channel();
    let url = relay.url();
    std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            d.daemon.as_ref().unwrap().adopt_standalone(
                &url,
                WAIT,
                &Arc::new(AtomicBool::new(false)),
                |code| tx.send(code.clone()).unwrap(),
                |p, l| Ok(Some(l.confirm(&p.proposal_hash)?)),
            )
        });
        let code = rx.recv_timeout(WAIT).unwrap();
        a.ring.approve_device_code(&code).unwrap();
        assert!(guest
            .join()
            .unwrap()
            .unwrap_err()
            .starts_with("reconciliation_required:"));
    });
    assert!(hit.load(Ordering::Acquire), "real Saved.write_error branch");
    let before = std::fs::read(d.home.paths().ring_dir.join("standalone-adoption.json")).unwrap();
    assert_eq!(
        d.daemon
            .as_ref()
            .unwrap()
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        StandaloneRecoveryState::AcceptedInstalling
    );
    // The failed invocation has released its offer lease. Actual startup denial can
    // now coexist with the recorded accepted obligation without authorizing a new one.
    let directory = d.home.paths().ring_dir;
    let keys_before = std::fs::read(directory.join("keys.json")).unwrap();
    assert_eq!(
        xshell_core::companion_reservation::reserve(&d.home.home(), &d.home.root().join("desktop"))
            .unwrap(),
        directory
    );
    let pending = directory.join(xshell_core::companion_reservation::FILE);
    let pending_before = std::fs::read(&pending).unwrap();
    let reservation = xshell_core::companion_reservation::read_reservation(&directory)
        .unwrap()
        .unwrap();
    assert!(!reservation.legacy_outcome_uncertain, "offer lease ended");
    assert!(!directory.join("companion.json").exists());
    let accepted_before: serde_json::Value = serde_json::from_slice(&before).unwrap();
    let mut host = common::Client::connect(&d.daemon.as_ref().unwrap().socket);
    host.hello(common::range(1, 1));
    let rosters = serde_json::from_value(accepted_before["accepted"].clone()).unwrap();
    assert!(
        host.request(&xshell_protocol::msg::ClientMsg::RingJoin {
            rosters,
            expect: None,
        })
        .is_err(),
        "pending denial still fences ordinary Host installation"
    );
    assert!(d
        .daemon
        .as_ref()
        .unwrap()
        .adopt_standalone(
            &url,
            WAIT,
            &Arc::new(AtomicBool::new(false)),
            |_| panic!("pending denial must not emit a new offer"),
            |_, _| panic!("pending denial must not request consent"),
        )
        .is_err());
    assert!(
        d.ring.begin_join(&d.companion()).is_err(),
        "unresolved standalone obligation must still fence registration"
    );
    assert_eq!(std::fs::read(&pending).unwrap(), pending_before);
    fail.store(false, Ordering::Release);
    wait_daemon_online(&d);
    let recovered = d
        .daemon
        .as_ref()
        .unwrap()
        .reconcile_standalone_adoption()
        .unwrap();
    assert_eq!(recovered.state, StandaloneRecoveryState::Installed);
    let after: serde_json::Value = serde_json::from_slice(
        &std::fs::read(d.home.paths().ring_dir.join("standalone-adoption.json")).unwrap(),
    )
    .unwrap();
    let before: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(after["accepted"], before["accepted"]);
    assert_eq!(after["intent"], before["intent"]);
    assert_eq!(after["installed"], true);
    let mut expected = before.clone();
    expected["installed"] = serde_json::json!(true);
    assert_eq!(
        after, expected,
        "only the durable installation receipt changes"
    );
    let roster_after = std::fs::read(directory.join("roster.json")).unwrap();
    let roster: serde_json::Value = serde_json::from_slice(&roster_after).unwrap();
    assert_eq!(roster["rosters"], before["accepted"]);
    assert_eq!(
        std::fs::read(directory.join("keys.json")).unwrap(),
        keys_before
    );
    assert_eq!(std::fs::read(&pending).unwrap(), pending_before);
    assert!(
        !directory.join("companion.json").exists(),
        "denial is not registration"
    );
    let journal_after = std::fs::read(directory.join("standalone-adoption.json")).unwrap();
    assert_eq!(
        d.daemon
            .as_ref()
            .unwrap()
            .reconcile_standalone_adoption()
            .unwrap()
            .state,
        StandaloneRecoveryState::Installed
    );
    assert_eq!(
        std::fs::read(directory.join("standalone-adoption.json")).unwrap(),
        journal_after
    );
    assert_eq!(
        std::fs::read(directory.join("roster.json")).unwrap(),
        roster_after
    );
    assert_eq!(std::fs::read(&pending).unwrap(), pending_before);
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &relay.target(),
            a.ring.chain().unwrap().ring_id()
        ),
        2
    );
}

#[test]
fn declined_and_expired_before_proof_are_not_publication_uncertainty() {
    let relay = workerd::Relay::start();
    let declined = Computer::new(&relay);
    let outcome = declined
        .ring
        .create_computer(&declined.companion(), |_, _| Ok(CreateDecision::Decline))
        .unwrap();
    let xshell_hostlink::ring::adoption::AdoptionOutcome::Declined { operation } = outcome else {
        panic!("not declined")
    };
    let before = std::fs::read(declined.home.root().join("desktop/ring.json")).unwrap();
    assert_eq!(
        declined
            .ring
            .reconcile_adoption(&declined.companion(), &operation)
            .unwrap()
            .local
            .unwrap()
            .state,
        AdoptionRecoveryState::DeclinedBeforeProof
    );
    assert_eq!(
        std::fs::read(declined.home.root().join("desktop/ring.json")).unwrap(),
        before,
        "read-only preproof status must not regenerate or rewrite"
    );
    let expired = Computer::with_ring_config(&relay, |cfg| cfg.pair_ttl = Duration::from_secs(1));
    assert!(expired
        .ring
        .create_computer(&expired.companion(), |p, lease| {
            while std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                < p.expires_at
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(lease.confirm(&p.proposal_hash).is_err());
            Err("explicit expired preproof callback".into())
        })
        .is_err());
    assert_eq!(
        expired.ring.adoption_status().unwrap().local.unwrap().state,
        AdoptionRecoveryState::ExpiredBeforeProof
    );
    assert!(stored(&expired).membership.is_none());
    assert_eq!(
        stored(&expired).journal.unwrap().phase,
        AdoptionPhase::ExpiredBeforeProof
    );
}

#[test]
fn retained_local_complete_process_death_between_stores_preserves_singleton_history_and_profile() {
    let relay = workerd::Relay::start();
    let mut b = Computer::new(&relay);
    b.ring.enable(None, false).unwrap();
    wait("legacy Desktop online", || {
        b.ring.view().connection == "connected"
    });
    b.ring
        .upgrade_retained_ring(&b.companion(), |p, l| {
            Ok(CreateDecision::Confirm(l.confirm(&p.proposal_hash)?))
        })
        .unwrap();
    let prior = stored(&b);
    let original = prior.membership.unwrap();
    let desktop_key = prior.keys.sign_key();
    let daemon_keys = std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap();
    let mut controller = spawn_controller(&b, &relay, "finish", "between-stores");
    kill_at_cut(&mut controller, &b);
    let journal = stored(&b).journal.unwrap();
    assert_eq!(journal.phase, AdoptionPhase::Accepted);
    assert!(!journal.desktop_installed && !journal.daemon_installed);
    let accepted = RosterChain::from_tokens(&journal.accepted).unwrap();
    assert_eq!(accepted.head().version(), 3);
    restart_controller(&mut b, &relay);
    wait("restarted completion owner online", || {
        b.ring.view().connection == "connected"
    });
    let result = b
        .ring
        .reconcile_adoption(&b.companion(), &journal.intent.operation)
        .unwrap();
    assert_eq!(
        result.local.unwrap().state,
        AdoptionRecoveryState::Installed
    );
    let after = stored(&b);
    let member = after.membership.unwrap();
    assert_eq!(member.chain.head(), accepted.head());
    assert_eq!(member.chain.get(1), original.chain.get(1));
    assert_eq!(member.chain.get(2), original.chain.get(2));
    assert_eq!(member.host_members, original.host_members);
    assert_eq!(member.farewells.len(), original.farewells.len());
    assert_eq!(member.held_relays, original.held_relays);
    assert_eq!(after.companion, prior.companion);
    assert_eq!(after.keys.sign_key(), desktop_key);
    assert_eq!(
        std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap(),
        daemon_keys
    );
    assert_eq!(
        after.journal.unwrap().intent.expires_at,
        journal.intent.expires_at
    );
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), accepted.ring_id()),
        3
    );
}

#[test]
fn current_preproposal_reservation_is_not_hidden_by_an_older_closed_journal() {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    b.ring
        .create_computer(&b.companion(), |_, _| Ok(CreateDecision::Decline))
        .unwrap();
    let old = stored(&b).journal.unwrap();
    let offer = b.ring.begin_join(&b.companion()).unwrap();
    let current = b.ring.adoption_status().unwrap().local.unwrap();
    assert_eq!(current.operation, offer.operation);
    assert_ne!(current.operation, old.intent.operation);
    assert_eq!(current.state, AdoptionRecoveryState::WaitingBeforeProposal);
    assert_eq!(current.expires_at, offer.expires_at);
    assert!(!current.desktop_installed && !current.daemon_installed);
    drop(offer);
}

#[test]
fn concurrent_standalone_recoveries_never_overwrite_a_newer_accepted_journal_prefix() {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Mutex,
    };
    use xshelld::server::{TestHook, TestPoint};
    let relay = workerd::Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let fail = Arc::new(AtomicBool::new(true));
    let pause = Arc::new(AtomicBool::new(false));
    let installs = Arc::new(AtomicUsize::new(0));
    let (at_install, installed) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Arc::new(Mutex::new(released));
    let d = Computer::with_config(&relay, |cfg| {
        let fail = fail.clone();
        let pause = pause.clone();
        let installs = installs.clone();
        cfg.test_hook = Some(TestHook(Arc::new(move |_, p| {
            if p == TestPoint::BeforeRingRosterWrite && fail.load(Ordering::Acquire) {
                return true;
            }
            if p == TestPoint::StandaloneInstalledBeforeJournal
                && pause.load(Ordering::Acquire)
                && installs.fetch_add(1, Ordering::SeqCst) == 0
            {
                at_install.send(()).unwrap();
                released.lock().unwrap().recv_timeout(WAIT).unwrap();
            }
            false
        })));
    });
    let url = relay.url();
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            d.daemon.as_ref().unwrap().adopt_standalone(
                &url,
                WAIT,
                &Arc::new(AtomicBool::new(false)),
                |code| tx.send(code.clone()).unwrap(),
                |p, l| Ok(Some(l.confirm(&p.proposal_hash)?)),
            )
        });
        a.ring
            .approve_device_code(&rx.recv_timeout(WAIT).unwrap())
            .unwrap();
        assert!(guest
            .join()
            .unwrap()
            .unwrap_err()
            .contains("reconciliation_required"));
    });
    let file = d.home.paths().ring_dir.join("standalone-adoption.json");
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(original["installed"], false);
    assert_eq!(original["accepted"].as_array().unwrap().len(), 2);
    fail.store(false, Ordering::Release);
    wait_daemon_online(&d);
    pause.store(true, Ordering::Release);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| d.daemon.as_ref().unwrap().reconcile_standalone_adoption());
        installed.recv_timeout(WAIT).unwrap();
        let c = Computer::new(&relay);
        let descendant = c.join(&a); // actual independently authorized public enrollment
        assert_eq!(descendant.head().version(), 3);
        let second = d
            .daemon
            .as_ref()
            .unwrap()
            .reconcile_standalone_adoption()
            .unwrap();
        assert_eq!(
            second.state,
            xshelld::server::adoption::StandaloneRecoveryState::Installed
        );
        let newer: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(newer["accepted"].as_array().unwrap().len(), 3);
        assert_eq!(newer["accepted"][2], descendant.head().token());
        let roster = std::fs::read(d.home.paths().ring_dir.join("roster.json")).unwrap();
        release.send(()).unwrap();
        let first_result = first.join().unwrap();
        let final_record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            final_record["accepted"], newer["accepted"],
            "stale final receipt must never shrink recorded history"
        );
        assert_eq!(final_record["installed"], true);
        assert_eq!(final_record["intent"], original["intent"]);
        assert_eq!(
            std::fs::read(d.home.paths().ring_dir.join("roster.json")).unwrap(),
            roster
        );
        assert!(
            first_result
                .unwrap_err()
                .contains("journal changed before final receipt"),
            "stale invocation must refuse its final write"
        );
    });
}

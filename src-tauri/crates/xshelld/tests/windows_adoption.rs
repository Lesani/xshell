#![cfg(windows)]
//! Native GUI-bound counterpart transactions against the unchanged production Relay.
#[path = "desktop/windows_fixture.rs"]
#[allow(dead_code)]
mod fixture;
mod win;
#[path = "desktop/workerd.rs"]
mod workerd;
use fixture::{CancelJoinOnUnwind, Computer, WAIT};
use std::sync::Arc;
use std::time::Duration;
use workerd::Relay;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::store::{AdoptionPhase, Store};
use xshell_protocol::ring::membership::{ConsentFlow, RosterChange};
use xshell_protocol::ring::relay::contract::relay_version;
use xshelld::server::{Config, Server};

struct PublicComputer {
    inner: Computer,
    desktop: std::path::PathBuf,
    daemon_dir: std::path::PathBuf,
    ring: Arc<xshell_hostlink::ring::DesktopRing>,
}
impl PublicComputer {
    fn new(relay: &Relay) -> Self {
        Self::with_options(relay, None, None)
    }
    fn with_options(
        relay: &Relay,
        hook: Option<xshelld::server::TestHook>,
        ttl: Option<std::time::Duration>,
    ) -> Self {
        let inner = Computer::with_options(
            relay,
            |cfg| cfg.test_hook = hook,
            |_| Arc::new(fixture::NoSsh),
            |cfg| {
                if let Some(ttl) = ttl {
                    cfg.pair_ttl = ttl;
                }
            },
        );
        let desktop = inner.home.root().join("desktop");
        let daemon_dir = inner.home.paths().ring_dir;
        let ring = inner.ring.clone();
        Self {
            inner,
            desktop,
            daemon_dir,
            ring,
        }
    }
    fn companion(&self) -> CompanionClient {
        self.inner.companion()
    }
}

#[test]
fn public_create_uses_two_private_stores_only_after_backend_confirmation() {
    let mut relay = Relay::start();
    let computer = PublicComputer::new(&relay);
    let companion = computer.companion();
    let profile = computer.ring.register_companion(&companion).unwrap();
    assert!(computer.ring.chain().is_none());
    assert!(!computer.daemon_dir.join("roster.json").exists());
    let keys_before = std::fs::read(computer.daemon_dir.join("keys.json")).unwrap();
    let outcome = computer
        .ring
        .create_computer(&companion, |preview, lease| {
            assert!(computer.ring.chain().is_none());
            assert!(!computer.daemon_dir.join("roster.json").exists());
            let journal = Store::new(computer.desktop.clone())
                .load_profile()
                .unwrap()
                .0
                .unwrap()
                .journal
                .unwrap();
            assert_eq!(journal.phase, AdoptionPhase::Prepared);
            assert!(journal.signed_candidate.is_none());
            assert!(
                computer.ring.register_companion(&companion).is_err(),
                "one admitted owner operation"
            );
            assert_eq!(preview.context.kind, ConsentFlow::LocalGenesis);
            assert_eq!(relay_version(&relay.target(), &preview.ring_id), 0);
            Ok(CreateDecision::Confirm(
                lease.confirm(&preview.proposal_hash)?,
            ))
        })
        .unwrap();
    let AdoptionOutcome::Installed { chain, .. } = outcome else {
        panic!("Create not installed");
    };
    let head = chain.head();
    let membership = head.roster().membership().unwrap();
    assert_eq!(membership.enrollments.len(), 1);
    assert_eq!(head.roster().members.len(), 2);
    let RosterChange::Genesis { proofs } = &membership.change else {
        panic!("not genesis");
    };
    assert_eq!(proofs.len(), 2);
    assert!(proofs
        .iter()
        .all(|p| p.context.kind == ConsentFlow::LocalGenesis));
    assert!(proofs.iter().any(|p| p.key == profile.desktop.sign_key));
    assert!(proofs.iter().any(|p| p.key == profile.daemon.sign_key));
    assert!(
        std::fs::read(computer.daemon_dir.join("keys.json")).unwrap() == keys_before,
        "Daemon keys changed"
    );
    let saved = Store::new(computer.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert_eq!(saved.keys.sign_key(), profile.desktop.sign_key);
    let journal = saved.journal.unwrap();
    assert_eq!(journal.phase, AdoptionPhase::Installed);
    assert!(journal.desktop_installed && journal.daemon_installed);
    assert_eq!(journal.signed_candidate.as_deref(), Some(head.token()));
    assert_eq!(journal.accepted[0], head.token());
    let daemon: serde_json::Value =
        serde_json::from_slice(&std::fs::read(computer.daemon_dir.join("roster.json")).unwrap())
            .unwrap();
    assert_eq!(daemon["rosters"][0], head.token());
    assert_eq!(membership.enrollments[0].id, journal.intent.operation);
    let desktop_bytes = std::fs::read(computer.desktop.join("ring.json")).unwrap();
    let desktop_json: serde_json::Value = serde_json::from_slice(&desktop_bytes).unwrap();
    let daemon_json: serde_json::Value = serde_json::from_slice(&keys_before).unwrap();
    assert!(
        desktop_json["signSeed"] != daemon_json["signSeed"],
        "private signing seeds must remain separate"
    );
    assert!(
        desktop_json["noiseSeed"] != daemon_json["noiseSeed"],
        "private Noise seeds must remain separate"
    );
    assert!(!String::from_utf8_lossy(&desktop_bytes)
        .contains(daemon_json["signSeed"].as_str().unwrap()));
    let daemon_journal = std::fs::read(computer.daemon_dir.join("companion.json")).unwrap();
    assert!(!String::from_utf8_lossy(&daemon_journal)
        .contains(desktop_json["signSeed"].as_str().unwrap()));
    let wait_online = || {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let view = computer.ring.view();
            // The Computer is one grouped row (its Desktop and Daemon).
            if view.connection == "connected"
                && view.members.len() == 1
                && view.members[0].kind
                    == Some(xshell_protocol::ring::membership::EnrollmentKind::Computer)
                && view.members[0].daemon_key == Some(profile.daemon.sign_key)
                && view.members[0].presence.kind == "online"
                // The Daemon itself, as the Relay reports it (not this app's own presence).
                && view.members[0]
                    .daemon_presence
                    .as_ref()
                    .is_some_and(|p| p.kind == "online")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < until,
                "normal Connector did not become usable: {} {:?}",
                view.connection,
                view.connection_error
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    wait_online();
    assert!(computer
        .ring
        .create_computer(&companion, |_, _| panic!(
            "installed membership cannot silently create again"
        ))
        .is_err());
    assert_eq!(relay_version(&relay.target(), chain.ring_id()), 1);
    relay.restart();
    assert_eq!(relay_version(&relay.target(), chain.ring_id()), 1);
    assert_eq!(computer.ring.chain().unwrap().head().hash(), head.hash());
    wait_online();
}

#[test]
fn decline_creates_no_enrollment_proofs_candidate_or_relay_membership() {
    let relay = Relay::start();
    let computer = PublicComputer::new(&relay);
    let companion = computer.companion();
    let profile = computer.ring.register_companion(&companion).unwrap();
    let result = computer
        .ring
        .create_computer(&companion, |_, _| Ok(CreateDecision::Decline))
        .unwrap();
    assert!(matches!(result, AdoptionOutcome::Declined { .. }));
    let journal = Store::new(computer.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .journal
        .unwrap();
    // Stage 1 records the precise pre-proof reason.
    assert_eq!(journal.phase, AdoptionPhase::DeclinedBeforeProof);
    assert!(journal.signed_candidate.is_none() && journal.accepted.is_empty());
    assert!(computer.ring.chain().is_none());
    assert!(!computer.daemon_dir.join("roster.json").exists());
    let policy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(computer.daemon_dir.join("companion.json")).unwrap())
            .unwrap();
    assert_eq!(policy["obligation"]["phase"], "cancelled");
    assert!(policy["obligation"]["proofs"].is_null());
    assert_eq!(
        relay_version(
            &relay.target(),
            &xshell_protocol::ring::RingId::derive(&profile.desktop.sign_key)
        ),
        0
    );
}

#[test]
fn cancelled_wrong_hash_and_stale_leases_release_no_enrollment_proofs() {
    let relay = Relay::start();
    let computer = PublicComputer::new(&relay);
    let client = computer.companion();
    computer.ring.register_companion(&client).unwrap();
    assert!(computer
        .ring
        .create_computer(&client, |preview, lease| {
            let proof = lease.confirm(&preview.proposal_hash)?;
            lease.cancel();
            Ok(CreateDecision::Confirm(proof))
        })
        .is_err());
    assert!(computer
        .ring
        .create_computer(&client, |_, lease| Ok(CreateDecision::Confirm(
            lease.confirm("wrong-hash")?
        )))
        .is_err());
    let mut stale = None;
    computer
        .ring
        .create_computer(&client, |preview, lease| {
            stale = Some(lease.confirm(&preview.proposal_hash)?);
            Ok(CreateDecision::Decline)
        })
        .unwrap();
    assert!(computer
        .ring
        .create_computer(&client, |_, _| Ok(CreateDecision::Confirm(
            stale.take().unwrap()
        )))
        .is_err());
    let policy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(computer.daemon_dir.join("companion.json")).unwrap())
            .unwrap();
    assert!(policy["obligation"]["proofs"].is_null());
    assert!(!computer.daemon_dir.join("roster.json").exists());
    assert!(computer.ring.chain().is_none());
    let stored = Store::new(computer.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert_eq!(stored.journal.unwrap().phase, AdoptionPhase::Cancelled);
    assert_eq!(
        relay_version(
            &relay.target(),
            &xshell_protocol::ring::RingId::derive(&stored.keys.sign_key())
        ),
        0
    );
}

fn runtime2_create(computer: &PublicComputer) {
    let client = computer.companion();
    computer.ring.register_companion(&client).unwrap();
    assert!(matches!(
        computer
            .ring
            .create_computer(&client, |preview, lease| Ok(CreateDecision::Confirm(
                lease.confirm(&preview.proposal_hash)?
            )))
            .unwrap(),
        AdoptionOutcome::Installed { .. }
    ));
}

fn runtime2_join(
    approver: &PublicComputer,
    guest: &PublicComputer,
) -> xshell_protocol::ring::RosterChain {
    let client = guest.companion();
    guest.ring.register_companion(&client).unwrap();
    let retained = Store::new(guest.desktop.clone()).retained_keys().unwrap();
    let daemon_keys = std::fs::read(guest.daemon_dir.join("keys.json")).unwrap();
    let offer = guest.ring.begin_join(&client).unwrap();
    assert_eq!(offer.code.format().split('-').count(), 8);
    let words = offer.code.format();
    let disk = std::fs::read(guest.desktop.join("ring.json")).unwrap();
    assert!(!String::from_utf8_lossy(&disk).contains(words.as_str()));
    assert!(!format!("{:?}", offer.join_status()).contains(words.as_str()));
    let secret = xshell_protocol::ring::b64::encode(offer.code.secret().as_bytes());
    assert!(!String::from_utf8_lossy(&disk).contains(&secret));
    let code = offer.code.clone();
    let accepted = std::thread::scope(|scope| {
        let offer = offer;
        let approval = scope.spawn(|| approver.ring.approve_device_code(&code));
        let mut cancel_before_join = CancelJoinOnUnwind(Some(&offer));
        // No confirmation on the joining machine: it signs and installs by itself.
        cancel_before_join.0 = None;
        drop(cancel_before_join);
        let outcome = offer.wait().unwrap();
        let published = approval.join().unwrap().unwrap();
        let AdoptionOutcome::Installed { chain, .. } = outcome else {
            panic!("Join not installed");
        };
        assert_eq!(published.chain.head().hash(), chain.head().hash());
        chain
    });
    assert!(
        approver.ring.approve_device_code(&code).is_err(),
        "the spent eight-word credential must not enroll twice"
    );
    let after = Store::new(guest.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert_eq!(after.keys.sign_key(), retained.sign_key());
    assert_eq!(after.keys.noise_key(), retained.noise_key());
    assert!(
        std::fs::read(guest.daemon_dir.join("keys.json")).unwrap() == daemon_keys,
        "Daemon retained keys changed during Join"
    );
    accepted
}

#[test]
fn public_join_and_approve_then_adopted_desktop_approves_third_with_original_offline() {
    let relay = Relay::start();
    let original = PublicComputer::new(&relay);
    runtime2_create(&original);
    let adopted = PublicComputer::new(&relay);
    let second = runtime2_join(&original, &adopted);
    assert_eq!(second.head().version(), 2);
    assert_eq!(
        second
            .head()
            .roster()
            .membership()
            .unwrap()
            .enrollments
            .len(),
        2
    );
    original.ring.quit();
    original.inner.daemon.as_ref().unwrap().stopper().shutdown();
    assert!(original
        .inner
        .daemon
        .as_ref()
        .unwrap()
        .wait_timeout(std::time::Duration::from_secs(2))
        .is_some());
    let third = PublicComputer::new(&relay);
    let third_chain = runtime2_join(&adopted, &third);
    let own = Store::new(adopted.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert_eq!(third_chain.head().roster().signed_by, own.keys.sign_key());
    assert_eq!(third_chain.head().version(), 3);
    let membership = third_chain.head().roster().membership().unwrap();
    assert_eq!(membership.enrollments.len(), 3);
    assert_eq!(third_chain.head().roster().members.len(), 6);
    assert_eq!(
        membership
            .enrollments
            .iter()
            .map(|unit| &unit.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    assert_eq!(relay_version(&relay.target(), third_chain.ring_id()), 3);
    let third_saved = Store::new(third.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    let journal = third_saved.journal.unwrap();
    assert_eq!(journal.phase, AdoptionPhase::Installed);
    assert!(journal.desktop_installed && journal.daemon_installed);
    let durable: serde_json::Value =
        serde_json::from_slice(&std::fs::read(third.daemon_dir.join("roster.json")).unwrap())
            .unwrap();
    assert_eq!(durable["rosters"][2], third_chain.head().token());
}

fn runtime2_assert_no_guest_proofs(guest: &PublicComputer) {
    let stored = Store::new(guest.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert!(stored.membership.is_none() && stored.pending_join.is_none());
    // Stage 1: a Join that ends before consent may leave no journal at all; one that has
    // one ended before any proof.
    if let Some(journal) = stored.journal {
        assert!(
            matches!(
                journal.phase,
                AdoptionPhase::Cancelled
                    | AdoptionPhase::DeclinedBeforeProof
                    | AdoptionPhase::ExpiredBeforeProof
            ),
            "{:?}",
            journal.phase
        );
        assert!(journal.signed_candidate.is_none() && journal.accepted.is_empty());
    }
    let counterpart: serde_json::Value =
        serde_json::from_slice(&std::fs::read(guest.daemon_dir.join("companion.json")).unwrap())
            .unwrap();
    assert!(counterpart["obligation"]["proofs"].is_null());
    assert!(!guest.daemon_dir.join("roster.json").exists());
}

#[test]
fn quitting_join_owner_cancels_the_pending_driver_without_waiting_for_a_peer() {
    let relay = Relay::start();
    let guest = PublicComputer::new(&relay);
    let offer = guest.ring.begin_join(&guest.companion()).unwrap();
    guest.ring.quit();
    assert!(offer.wait().is_err());
    let profile = Store::new(guest.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert!(profile.pending_join.is_none() && profile.membership.is_none());
    assert!(guest.ring.begin_join(&guest.companion()).is_err());
}
/// Format 1 is not supported any more (Stage 1, D2a): the retained-completion entry points
/// of the old historical-Daemon flow are refused over the real Windows pipe, as on Unix
/// (`ring_just_works::format1_entry_points_refused`). The six completion tests of that flow
/// were removed with it.
#[test]
fn format1_completion_entry_points_refused_over_the_pipe() {
    let relay = Relay::start();
    let computer = PublicComputer::new(&relay);
    runtime2_create(&computer);
    let client = computer.companion();
    let target = Store::new(computer.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .companion
        .unwrap()
        .target();
    let intent = xshell_protocol::ring::pairing::adoption::AdoptionIntent {
        operation: xshell_protocol::ring::b64::encode(&[5; 32]),
        expires_at: 4_000_000_000,
        target,
    };
    for request in [
        xshell_protocol::ring::companion::CompanionRequest::CompletionStart {
            intent: intent.clone(),
        },
        xshell_protocol::ring::companion::CompanionRequest::RetainedCompletionStart {
            intent,
            predecessor: vec![],
        },
    ] {
        let error = client.request(request).unwrap_err();
        assert!(error.contains("unsupported: format 1"), "{error}");
    }
}

#[test]
fn external_ring_lock_holder() {
    let Some(path) = std::env::var_os("X87_WINDOWS_RING_LOCK") else {
        return;
    };
    let ready = std::env::var_os("X87_WINDOWS_LOCK_READY").unwrap();
    let release = std::env::var_os("X87_WINDOWS_LOCK_RELEASE").unwrap();
    let file = xshell_core::private_fs::open_or_create(std::path::Path::new(&path)).unwrap();
    file.lock().unwrap();
    std::fs::write(&ready, "locked").unwrap();
    assert!(win::wait_until(WAIT, || std::path::Path::new(&release).exists()));
}
#[test]
fn external_file_lock_spends_original_budget_and_shutdown_drains_without_late_worker() {
    let relay = Relay::start();
    let mut computer = Computer::new(&relay);
    let client = computer.companion();
    let ready = computer.home.root().join("ready");
    let release = computer.home.root().join("release");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "external_ring_lock_holder", "--nocapture"])
        .env(
            "X87_WINDOWS_RING_LOCK",
            computer.home.paths().ring_dir.join("ring.lock"),
        )
        .env("X87_WINDOWS_LOCK_READY", &ready)
        .env("X87_WINDOWS_LOCK_RELEASE", &release)
        .spawn()
        .unwrap();
    struct ChildGuard<'a>(&'a mut std::process::Child);
    impl Drop for ChildGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let guard = ChildGuard(&mut child);
    assert!(win::wait_until(WAIT, || ready.exists()));
    let start = std::time::Instant::now();
    assert!(client
        .request(xshell_protocol::ring::companion::CompanionRequest::Descriptor)
        .is_err());
    assert!(start.elapsed() >= Duration::from_secs(4) && start.elapsed() < Duration::from_secs(7));
    std::thread::scope(|scope| {
        let request = scope.spawn(|| {
            client.request(xshell_protocol::ring::companion::CompanionRequest::Descriptor)
        });
        std::thread::sleep(Duration::from_millis(100));
        let start = std::time::Instant::now();
        computer.daemon.take().unwrap().shutdown();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(request.join().unwrap().is_err());
    });
    std::fs::write(&release, "release").unwrap();
    assert!(guard.0.wait().unwrap().success());
    assert!(
        !computer.home.paths().ring_dir.join("keys.json").exists(),
        "timed-out worker must never acquire the store later"
    );
}
#[test]
fn unresolved_standalone_journal_blocks_registration_even_after_expiry_and_restart() {
    let relay = Relay::start();
    let mut computer = Computer::new(&relay);
    let client = computer.companion();
    // Corrupt retained uncertainty is never absence; this is a synthetic historical record.
    std::fs::write(
        computer
            .home
            .paths()
            .ring_dir
            .join("standalone-adoption.json"),
        b"{unreadable",
    )
    .unwrap();
    assert!(computer
        .ring
        .register_companion(&client)
        .unwrap_err()
        .contains("reconciliation_required"));
    computer.daemon.take().unwrap().shutdown();
    let mut cfg = Config::new(computer.home.home(), computer.home.paths());
    cfg.gui_bound = Some(xshelld::server::GuiBound {
        parent_pid: std::process::id(),
    });
    computer.daemon = Some(Server::start(cfg).unwrap());
    assert!(computer
        .ring
        .register_companion(&client)
        .unwrap_err()
        .contains("reconciliation_required"));
    assert!(!computer
        .home
        .paths()
        .ring_dir
        .join("companion.json")
        .exists());
}

#[test]
fn real_pipe_refuses_cross_operation_and_cross_domain_consent_replay_without_store_changes() {
    use xshell_protocol::ring::companion::CompanionRequest;
    let relay = Relay::start();
    let computer = Computer::new(&relay);
    let chain = computer.create();
    let client = computer.companion();
    let profile = Store::new(computer.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .companion
        .unwrap();
    let RosterChange::Genesis { proofs } = chain.head().roster().membership().unwrap().change
    else {
        panic!("expected real public genesis");
    };
    let desktop_proof = proofs
        .into_iter()
        .find(|p| p.key == profile.desktop.sign_key)
        .unwrap();
    let dir = computer.home.paths().ring_dir;
    let policy_before = std::fs::read(dir.join("companion.json")).unwrap();
    let keys_before = std::fs::read(dir.join("keys.json")).unwrap();
    let chain_before = std::fs::read(dir.join("roster.json")).unwrap();
    let refused = client
        .request(CompanionRequest::Register {
            profile,
            proof: desktop_proof.signature,
        })
        .unwrap_err();
    assert!(
        refused.contains("registration possession proof refused"),
        "consent is not registration possession: {refused}"
    );
    assert!(
        client
            .request(CompanionRequest::Consent {
                operation: xshell_protocol::ring::relay::wire::new_nonce().unwrap(),
                desktop_proof,
            })
            .is_err(),
        "a valid consent proof must not authorize another operation"
    );
    assert_eq!(
        std::fs::read(dir.join("companion.json")).unwrap(),
        policy_before
    );
    assert_eq!(std::fs::read(dir.join("keys.json")).unwrap(), keys_before);
    assert_eq!(
        std::fs::read(dir.join("roster.json")).unwrap(),
        chain_before
    );
}

#[test]
fn shared_join_helper_preview_panic_cancels_before_approval_join() {
    let relay = Relay::start();
    let host = PublicComputer::new(&relay);
    runtime2_create(&host);
    let guest = PublicComputer::with_options(&relay, None, Some(Duration::from_secs(30)));
    let (preview, preview_seen) = std::sync::mpsc::sync_channel(1);
    let (finished, helper_finished) = std::sync::mpsc::sync_channel(1);
    let rescued = std::thread::scope(|scope| {
        let guest_ring = guest.ring.clone();
        let rescue = scope.spawn(move || {
            if preview_seen.recv().is_err() {
                return false;
            }
            if helper_finished
                .recv_timeout(Duration::from_secs(2))
                .is_err()
            {
                // Bounded RED cleanup of this fixture only; not a successful cancellation.
                guest_ring.quit();
                return true;
            }
            false
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guest
                .inner
                .join_with_verified_preview(&host.inner, move || {
                    preview.send(()).unwrap();
                    panic!("deliberate shared native Join helper assertion failure");
                });
        }));
        let _ = finished.send(());
        let rescued = rescue.join().unwrap();
        let panic = result.expect_err("shared helper must reach deliberate assertion");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"deliberate shared native Join helper assertion failure")
        );
        rescued
    });
    runtime2_assert_no_guest_proofs(&guest);
    assert_eq!(host.ring.chain().unwrap().head().version(), 1);
    assert!(
        !rescued,
        "shared helper stranded scoped approval until rescue cancellation"
    );
}

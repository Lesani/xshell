#![cfg(windows)]
//! Native GUI-bound counterpart transactions against the unchanged production Relay.
#[path = "desktop/windows_fixture.rs"]
#[allow(dead_code)]
mod fixture;
mod win;
#[path = "desktop/workerd.rs"]
mod workerd;
use fixture::{CancelJoinOnUnwind, Computer, Quiet, WAIT};
use std::sync::Arc;
use std::time::Duration;
use workerd::Relay;
use xshell_hostlink::ring::adoption::CompletionOutcome;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::store::{AdoptionPhase, Store};
use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig};
use xshell_hostlink::Manager;
use xshell_protocol::ring::membership::EnrollmentKind;
use xshell_protocol::ring::membership::{ConsentFlow, RosterChange};
use xshell_protocol::ring::relay::contract::relay_version;
use xshelld::server::{Config, Server, ServerHandle};

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
    assert_eq!(journal.phase, AdoptionPhase::Cancelled);
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
        let (preview, lease) = offer
            .await_consent(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(preview.context.kind, ConsentFlow::Pairing);
        assert!(guest.ring.chain().is_none());
        assert!(!guest.daemon_dir.join("roster.json").exists());
        offer
            .confirm_join(lease.confirm(&preview.proposal_hash).unwrap())
            .unwrap();
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
    let journal = stored.journal.unwrap();
    assert_eq!(journal.phase, AdoptionPhase::Cancelled);
    assert!(journal.signed_candidate.is_none() && journal.accepted.is_empty());
    let counterpart: serde_json::Value =
        serde_json::from_slice(&std::fs::read(guest.daemon_dir.join("companion.json")).unwrap())
            .unwrap();
    assert!(counterpart["obligation"]["proofs"].is_null());
    assert!(!guest.daemon_dir.join("roster.json").exists());
}

#[test]
fn public_join_decline_releases_no_enrollment_proofs_or_publication() {
    let relay = Relay::start();
    let host = PublicComputer::new(&relay);
    runtime2_create(&host);
    let guest = PublicComputer::new(&relay);
    let offer = guest.ring.begin_join(&guest.companion()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let offer = offer;
        let approval = scope.spawn(|| host.ring.approve_device_code(&code));
        let mut cancel_before_join = CancelJoinOnUnwind(Some(&offer));
        offer
            .await_consent(std::time::Duration::from_secs(10))
            .unwrap();
        offer.decline_join().unwrap();
        cancel_before_join.0 = None;
        drop(cancel_before_join);
        assert!(matches!(
            offer.wait().unwrap(),
            AdoptionOutcome::Declined { .. }
        ));
        assert!(approval.join().unwrap().is_err());
    });
    runtime2_assert_no_guest_proofs(&guest);
    let stored = Store::new(host.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    let outgoing = stored.outgoing.unwrap();
    assert_eq!(
        outgoing.phase,
        xshell_hostlink::ring::store::OutgoingPhase::Cancelled
    );
    assert!(outgoing.signed_candidate.is_none() && outgoing.accepted.is_empty());
    let serialized = serde_json::to_string(&outgoing).unwrap();
    assert!(!serialized.contains("desktopInstalled") && !serialized.contains("daemonInstalled"));
    assert_eq!(
        relay_version(&relay.target(), stored.membership.unwrap().chain.ring_id()),
        1
    );
}

#[test]
fn public_join_expiry_is_terminal_without_regeneration_or_enrollment_proofs() {
    let relay = Relay::start();
    let host = PublicComputer::new(&relay);
    runtime2_create(&host);
    // Native fixture setup gets 30s to reach verified preview (the Unix fixture uses 3s).
    // Once begun, this operation's captured absolute expiry is never renewed.
    let guest = PublicComputer::with_options(&relay, None, Some(Duration::from_secs(30)));
    let offer = guest.ring.begin_join(&guest.companion()).unwrap();
    let original_expiry = offer.expires_at;
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let offer = offer;
        let approval = scope.spawn(|| host.ring.approve_device_code(&code));
        let mut cancel_on_unwind = CancelJoinOnUnwind(Some(&offer));
        let (preview, lease) = offer
            .await_consent(WAIT)
            .expect("expiry test must reach verified pre-proof preview");
        assert_eq!(preview.operation, offer.operation);
        let wall_now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert!(
            wall_now() < original_expiry,
            "early setup expiry is not this test's proof"
        );
        let prepared = Store::new(guest.desktop.clone())
            .load_profile()
            .unwrap()
            .0
            .unwrap();
        assert!(prepared.membership.is_none());
        let journal = prepared.journal.unwrap();
        assert_eq!(journal.phase, AdoptionPhase::Prepared);
        assert!(journal.signed_candidate.is_none() && journal.accepted.is_empty());
        let counterpart: serde_json::Value = serde_json::from_slice(
            &std::fs::read(guest.daemon_dir.join("companion.json")).unwrap(),
        )
        .unwrap();
        assert!(counterpart["obligation"]["proofs"].is_null());
        assert!(!guest.daemon_dir.join("roster.json").exists());
        while wall_now() <= original_expiry {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(offer.expires_at, original_expiry);
        assert!(lease.confirm(&preview.proposal_hash).is_err());
        cancel_on_unwind.0 = None;
        drop(cancel_on_unwind);
        assert!(
            offer.wait().is_err(),
            "expired operation must fail without cancellation authority"
        );
        assert!(approval.join().unwrap().is_err());
    });
    runtime2_assert_no_guest_proofs(&guest);
    assert_eq!(host.ring.chain().unwrap().head().version(), 1);
}
#[test]
fn verified_join_preview_panic_cancels_before_scope_joins_approval() {
    let relay = Relay::start();
    let host = PublicComputer::new(&relay);
    runtime2_create(&host);
    let guest = PublicComputer::with_options(&relay, None, Some(Duration::from_secs(30)));
    let offer = guest.ring.begin_join(&guest.companion()).unwrap();
    let code = offer.code.clone();
    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        std::thread::scope(|scope| {
            let offer = offer;
            let _approval = scope.spawn(|| host.ring.approve_device_code(&code));
            let _cancel_before_join = CancelJoinOnUnwind(Some(&offer));
            offer
                .await_consent(WAIT)
                .expect("cleanup control must reach verified preview");
            panic!("deliberate native fixture assertion failure");
        });
    }));
    let panic = result.expect_err("deliberate preview failure must unwind");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"deliberate native fixture assertion failure"),
        "an early setup failure is not the cleanup control"
    );
    assert!(start.elapsed() < WAIT, "panic cleanup stranded approval");
    runtime2_assert_no_guest_proofs(&guest);
    assert_eq!(host.ring.chain().unwrap().head().version(), 1);
}

#[test]
fn public_join_refuses_wrong_origin_lease_before_proof_release() {
    let relay = Relay::start();
    let host = PublicComputer::new(&relay);
    runtime2_create(&host);
    let unrelated = PublicComputer::new(&relay);
    let mut wrong = None;
    unrelated
        .ring
        .create_computer(&unrelated.companion(), |preview, lease| {
            wrong = Some(lease.confirm(&preview.proposal_hash)?);
            Ok(CreateDecision::Decline)
        })
        .unwrap();
    let guest = PublicComputer::new(&relay);
    let offer = guest.ring.begin_join(&guest.companion()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let offer = offer;
        let approval = scope.spawn(|| host.ring.approve_device_code(&code));
        let mut cancel_before_join = CancelJoinOnUnwind(Some(&offer));
        let (preview, lease) = offer
            .await_consent(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(offer.confirm_join(wrong.take().unwrap()).is_err());
        assert!(lease.confirm("wrong-proposal-hash").is_err());
        let counterpart: serde_json::Value = serde_json::from_slice(
            &std::fs::read(guest.daemon_dir.join("companion.json")).unwrap(),
        )
        .unwrap();
        assert!(counterpart["obligation"]["proofs"].is_null());
        assert_eq!(preview.context.kind, ConsentFlow::Pairing);
        offer.decline_join().unwrap();
        cancel_before_join.0 = None;
        drop(cancel_before_join);
        offer.wait().unwrap();
        assert!(approval.join().unwrap().is_err());
    });
    runtime2_assert_no_guest_proofs(&guest);
}

#[test]
fn dropping_public_join_origin_cancels_and_drains_the_driver_without_proof_release() {
    let relay = Relay::start();
    let host = PublicComputer::new(&relay);
    runtime2_create(&host);
    let guest = PublicComputer::new(&relay);
    let offer = guest.ring.begin_join(&guest.companion()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let offer = offer;
        let approval = scope.spawn(|| host.ring.approve_device_code(&code));
        let mut cancel_before_join = CancelJoinOnUnwind(Some(&offer));
        offer
            .await_consent(std::time::Duration::from_secs(10))
            .unwrap();
        let before = std::time::Instant::now();
        cancel_before_join.0 = None;
        drop(cancel_before_join);
        drop(offer);
        assert!(
            before.elapsed() < std::time::Duration::from_secs(2),
            "verified human wait must be cancellable, not a detached driver"
        );
        assert!(approval.join().unwrap().is_err());
    });
    runtime2_assert_no_guest_proofs(&guest);
    let replacement = guest.ring.begin_join(&guest.companion()).unwrap();
    drop(replacement);
    assert_eq!(host.ring.chain().unwrap().head().version(), 1);
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

// This fixture recreates historical standalone membership entirely from fresh VM-owned
// keys and a real public Noise pairing exchange. It is not a Windows standalone command,
// and none of the current Computer Create/Join success tests use seeded authority.
fn historical_daemon(a: &Computer, home: &win::TestHome, relay: &Relay) {
    use xshell_protocol::ring::pairing::adoption::{
        AdoptionIntent, AdoptionSigners, JoinTarget, PublicIdentity,
    };
    use xshell_protocol::ring::relay::pair::{adopt_as_guest, AdoptionGuestRequest, PairOptions};
    use xshell_protocol::ring::{b64, DeviceKeys};
    let keys = Arc::new(DeviceKeys::generate().unwrap());
    let code = xshell_protocol::ring::device_code::DeviceCode::generate().unwrap();
    let intent = AdoptionIntent {
        operation: xshell_protocol::ring::relay::wire::new_nonce().unwrap(),
        expires_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30,
        target: JoinTarget::StandaloneDaemon {
            daemon: PublicIdentity {
                sign_key: keys.sign_key(),
                noise_key: keys.noise_key(),
                name: xshell_protocol::ring::member_name(
                    &std::env::var("COMPUTERNAME").unwrap_or_default(),
                    "this computer",
                ),
            },
        },
    };
    let chain = std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            adopt_as_guest(
                &AdoptionGuestRequest {
                    keys: keys.clone(),
                    signers: AdoptionSigners {
                        principal: keys.as_ref(),
                        companion: None,
                    },
                    secret: &code.secret(),
                    intent: &intent,
                    relay_url: &relay.url(),
                    approver: None,
                    until: std::time::Instant::now() + std::time::Duration::from_secs(30),
                },
                &PairOptions::default(),
                None,
                |_| Ok(true),
            )
        });
        a.ring.approve_device_code(&code).unwrap();
        guest.join().unwrap().unwrap().0
    });
    let dir = home.paths().ring_dir;
    xshell_core::private_fs::ensure_dir(&dir).unwrap();
    let (sign, noise) = keys.seeds();
    use std::io::Write;
    for (name, value) in [
        (
            "keys.json",
            serde_json::json!({"v":1,"signSeed":b64::encode(&*sign),"noiseSeed":b64::encode(&*noise)}),
        ),
        (
            "roster.json",
            serde_json::json!({"v":1,"rosters":chain.versions().iter().map(|r|r.token()).collect::<Vec<_>>()}),
        ),
    ] {
        let mut file = xshell_core::private_fs::create_new(&dir.join(name)).unwrap();
        file.write_all(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        file.sync_all().unwrap();
    }
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + WAIT;
    while !condition() {
        assert!(std::time::Instant::now() < until, "runtime3 wait timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn deliver_current_result(
    approval: &xshell_hostlink::ring::adoption::CompletionApproval<'_>,
    manager: &Manager,
    id: &str,
) {
    // Completing the singleton replaces its enrollment ID. The old Host generation
    // can still look usable just before its final invalidation. Only replay this typed,
    // exact accepted result on the replacement; never retry proposal/proofs/publication.
    wait_for(|| {
        let Some(host) = manager.access(id).filter(|h| h.status().usable()) else {
            return false;
        };
        match approval.deliver_result(&host) {
            Ok(()) => true,
            Err(error)
                if error.contains("broker connection changed")
                    || error.contains("broker connection is not usable")
                    || error.contains("broker connection ended")
                    || error.contains("is offline") =>
            {
                eprintln!("result generation replacement: {error}");
                false
            }
            Err(error) => panic!("accepted result delivery failed: {error}"),
        }
    });
}
struct Retained {
    b: Arc<DesktopRing>,
    d: ServerHandle,
    a: Computer,
    home: win::TestHome,
    relay: Relay,
    id: String,
}
impl Retained {
    fn new(ttl: Duration) -> Self {
        Self::with_hook(ttl, None)
    }
    fn with_hook(ttl: Duration, hook: Option<xshelld::server::TestHook>) -> Self {
        Self::with_hooks(ttl, hook, None)
    }
    fn with_hooks(
        ttl: Duration,
        hook: Option<xshelld::server::TestHook>,
        desktop_hook: Option<xshell_hostlink::ring::AdoptionTestHook>,
    ) -> Self {
        let relay = Relay::start();
        let a = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = desktop_hook);
        a.create();
        let home = win::TestHome::new();
        let mut config = Config::new(home.home(), home.paths());
        config.test_hook = hook;
        historical_daemon(&a, &home, &relay);
        config.gui_bound = Some(xshelld::server::GuiBound {
            parent_pid: std::process::id(),
        });
        config.job_launcher = Some(win::bin().into());
        let d = Server::start(config).unwrap();
        let chain = xshell_protocol::ring::RosterChain::from_tokens(
            &serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(home.paths().ring_dir.join("roster.json")).unwrap(),
            )
            .unwrap()["rosters"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let singleton = chain
            .head()
            .roster()
            .membership()
            .unwrap()
            .enrollments
            .into_iter()
            .find(|e| e.kind == EnrollmentKind::Daemon)
            .unwrap();
        let id = xshell_hostlink::ring::hosts::relay_host_id(&singleton.members[0]);
        wait_for(|| a.manager.access(&id).is_some_and(|h| h.status().usable()));
        let mut cfg = DesktopRingConfig::new(home.root().join("desktop"), "B".into());
        cfg.default_relay_url = relay.url();
        cfg.pair_ttl = ttl;
        let b = DesktopRing::open(cfg, Arc::new(Quiet));
        Self {
            b,
            d,
            a,
            home,
            relay,
            id,
        }
    }
    fn client(&self) -> CompanionClient {
        let client = CompanionClient::for_store(&self.home.paths().ring_dir).unwrap();
        assert_eq!(client.endpoint(), self.d.companion_endpoint().unwrap());
        client
    }
    fn begin(&self) {
        self.b.begin_companion_completion(&self.client()).unwrap();
    }
    fn host(&self) -> xshell_hostlink::HostAccess {
        wait_for(|| {
            self.a
                .manager
                .access(&self.id)
                .is_some_and(|h| h.status().usable())
        });
        self.a.manager.access(&self.id).unwrap()
    }
    fn no_local_proofs(&self) {
        let policy: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.home.paths().ring_dir.join("companion.json")).unwrap(),
        )
        .unwrap();
        assert!(policy["obligation"]["proofs"].is_null());
        assert!(self.b.chain().is_none());
    }
    fn confirm(&self) {
        assert!(matches!(
            self.b
                .confirm_companion_completion(&self.client(), |p, lease| Ok(
                    CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?)
                ))
                .unwrap(),
            CompletionOutcome::Consented { .. }
        ));
    }
}
impl Drop for Retained {
    fn drop(&mut self) {
        self.b.quit();
    }
}

#[test]
fn completion_decline_and_wrong_lease_never_release_local_proofs_or_reuse_confirmation() {
    let r = Retained::new(WAIT);
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    assert!(matches!(
        r.b.confirm_companion_completion(&r.client(), |_, _| Ok(CreateDecision::Decline))
            .unwrap(),
        CompletionOutcome::Declined { .. }
    ));
    r.no_local_proofs();
    drop(approval);
    let first = Store::new(r.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .journal
        .unwrap()
        .intent
        .operation;
    let fresh = r.b.begin_companion_completion(&r.client()).unwrap();
    assert_ne!(first, fresh);
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let other = Computer::new(&r.relay);
    let mut wrong = None;
    other
        .ring
        .create_computer(&other.companion(), |p, lease| {
            wrong = Some(lease.confirm(&p.proposal_hash)?);
            Ok(CreateDecision::Decline)
        })
        .unwrap();
    assert!(r
        .b
        .confirm_companion_completion(&r.client(), |_, _| Ok(CreateDecision::Confirm(
            wrong.take().unwrap()
        )))
        .is_err());
    r.no_local_proofs();
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &r.relay.target(),
            r.a.ring.chain().unwrap().ring_id()
        ),
        2
    );
}
#[test]
fn expired_completion_requires_fresh_local_intent_without_another_code() {
    let r = Retained::new(Duration::from_secs(2));
    r.begin();
    let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(r
        .b
        .confirm_companion_completion(&r.client(), |_, _| panic!(
            "expired proposal cannot show live confirmation"
        ))
        .is_err());
    assert!(approval.publish().is_err());
    r.no_local_proofs();
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
    r.b.begin_companion_completion(&r.client()).unwrap();
}
#[test]
fn independent_changed_head_refuses_obsolete_completion_without_local_proofs() {
    let r = Retained::new(WAIT);
    let c = Computer::new(&r.relay);
    c.join(&r.a);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 3)
    });
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let e = Computer::new(&r.relay);
    e.join(&c);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 4)
    });
    let error =
        r.b.confirm_companion_completion(&r.client(), |_, _| {
            panic!("obsolete head cannot release a confirmation")
        })
        .err()
        .unwrap();
    assert!(
        error.contains("membership") || error.contains("retained"),
        "{error}"
    );
    r.no_local_proofs();
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
    // Return/reproposal is a fresh operation, never a rebased old consent.
    r.begin();
    let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    assert!(approval.publish().unwrap().is_none());
    r.confirm();
    approval.publish().unwrap().unwrap();
    deliver_current_result(&approval, &r.a.manager, &r.id);
    assert!(matches!(
        r.b.install_companion_completion(&r.client()).unwrap(),
        CompletionOutcome::Installed { .. }
    ));
}
#[test]
fn callback_successor_prevents_desktop_completion_proof_release() {
    let r = Retained::new(WAIT);
    let c = Computer::new(&r.relay);
    c.join(&r.a);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 3)
    });
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let e = Computer::new(&r.relay);
    let result = r.b.confirm_companion_completion(&r.client(), |p, lease| {
        e.join(&c);
        wait_for(|| {
            let disk: serde_json::Value = serde_json::from_slice(
                &std::fs::read(r.home.paths().ring_dir.join("roster.json")).unwrap(),
            )
            .unwrap();
            disk["rosters"].as_array().unwrap().len() == 4
        });
        Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
    });
    assert!(
        result.is_err(),
        "changed head inside callback must not return Consented"
    );
    r.no_local_proofs();
    let journal = Store::new(r.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .journal
        .unwrap();
    assert_eq!(
        journal.phase,
        AdoptionPhase::Prepared,
        "no MayHaveLeft before the fresh-head gate"
    );
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
}
#[test]
fn daemon_consent_independently_rejects_prepared_predecessor_after_callback_successor() {
    use xshell_protocol::ring::companion::CompanionRequest;
    use xshell_protocol::ring::membership::{consent_message, ConsentProof};
    use xshell_protocol::ring::{Signature, Signer};
    let r = Retained::new(WAIT);
    let c = Computer::new(&r.relay);
    c.join(&r.a);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 3)
    });
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let e = Computer::new(&r.relay);
    r.b.confirm_companion_completion(&r.client(), |p, lease| {
        let _confirmation = lease.confirm(&p.proposal_hash)?;
        e.join(&c);
        wait_for(|| {
            let disk: serde_json::Value = serde_json::from_slice(
                &std::fs::read(r.home.paths().ring_dir.join("roster.json")).unwrap(),
            )
            .unwrap();
            disk["rosters"].as_array().unwrap().len() == 4
        });
        // Explicitly test D's independent proof boundary with B's actual local signer,
        // not forged signatures or a caller-supplied boolean confirmation.
        let keys = Store::new(r.home.root().join("desktop"))
            .retained_keys()
            .unwrap();
        let message = consent_message(&p.proposal, p.expires_at, &p.context).unwrap();
        let proof = ConsentProof {
            key: keys.sign_key(),
            expires_at: p.expires_at,
            context: p.context.clone(),
            signature: Signature::from_bytes(keys.sign(&message).unwrap()),
        };
        let reply = r.client().request(CompanionRequest::Consent {
            operation: p.operation.clone(),
            desktop_proof: proof,
        });
        assert!(
            reply.is_err(),
            "D must refuse its proof even when live and durable agree at a newer head"
        );
        Ok(CreateDecision::Decline)
    })
    .unwrap();
    r.no_local_proofs();
    drop(approval);
}
#[test]
fn retained_historical_daemon_completes_via_authorized_relay_and_real_counterpart() {
    let f = Retained::new(WAIT);
    let before = std::fs::read(f.home.paths().ring_dir.join("keys.json")).unwrap();
    f.begin();
    let mut approval = f.a.ring.prepare_companion_completion(f.host()).unwrap();
    assert!(approval.publish().unwrap().is_none());
    f.confirm();
    let accepted = approval.publish().unwrap().unwrap().chain;
    deliver_current_result(&approval, &f.a.manager, &f.id);
    assert!(matches!(
        f.b.install_companion_completion(&f.client()).unwrap(),
        xshell_hostlink::ring::adoption::CompletionOutcome::Installed { .. }
    ));
    assert_eq!(accepted.head().version(), 3);
    assert_eq!(
        std::fs::read(f.home.paths().ring_dir.join("keys.json")).unwrap(),
        before
    );
    let journal = Store::new(f.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .journal
        .unwrap();
    assert_eq!(journal.phase, AdoptionPhase::Installed);
    assert!(journal.desktop_installed && journal.daemon_installed);
    assert!(approval.publish().is_err(), "publication must not replay");
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

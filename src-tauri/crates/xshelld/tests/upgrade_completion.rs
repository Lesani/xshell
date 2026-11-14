#![cfg(unix)]
//! Runtime4: retained v1 is established by public legacy enable/Host join, never seeded v2 rows.
mod common;
#[path = "desktop/relay_fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "desktop/workerd.rs"]
mod workerd;
use fixture::*;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision};
use xshell_hostlink::ring::store::{AdoptionPhase, Store};
use xshell_hostlink::ring::LocalIdentity;
use xshell_protocol::msg::ClientMsg;
use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
use xshell_protocol::ring::membership::{ConsentFlow, EnrollmentKind, RosterChange};
use xshell_protocol::ring::RosterChain;

fn confirm(
    p: &xshell_hostlink::ring::adoption::ConsentPreview,
    lease: &xshell_hostlink::ring::adoption::CreateConfirmationLease,
) -> Result<CreateDecision, String> {
    Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
}
fn installed(outcome: AdoptionOutcome) -> RosterChain {
    match outcome {
        AdoptionOutcome::Installed { chain, .. } => chain,
        _ => panic!("not installed"),
    }
}
fn legacy(b: &Computer, both: bool) -> RosterChain {
    let d = b.daemon.as_ref().unwrap();
    let identity = match b.companion().request(CompanionRequest::Descriptor).unwrap() {
        CompanionResponse::Identity { identity, .. } => identity,
        _ => panic!("no public identity"),
    };
    b.ring
        .enable(
            both.then_some(LocalIdentity {
                name: identity.name.clone(),
                sign_key: identity.sign_key,
                noise_key: identity.noise_key,
            }),
            false,
        )
        .unwrap();
    if both {
        // Record a real public legacy Host mapping without opening SSH or copying keys.
        b.ring
            .ensure_host_daemons(
                &[(
                    xshell_hostlink::ring::HostRef {
                        id: "retained-host-mapping".into(),
                        name: identity.name.clone(),
                        target: "synthetic-retained-target".into(),
                    },
                    LocalIdentity {
                        name: identity.name,
                        sign_key: identity.sign_key,
                        noise_key: identity.noise_key,
                    },
                )],
                &|_| true,
                None,
            )
            .unwrap();
    }
    let original = b.ring.chain().unwrap();
    if both {
        let mut c = common::Client::connect(&d.socket);
        c.hello(common::range(1, 1));
        c.request(&ClientMsg::RingJoin {
            rosters: original
                .versions()
                .iter()
                .map(|r| r.token().into())
                .collect(),
            expect: None,
        })
        .unwrap();
    }
    wait("legacy Desktop connected to actual workerd", || {
        b.ring.view().connection == "connected"
    });
    original
}
fn wait_daemon_head(b: &Computer, chain: &RosterChain) {
    wait("Daemon independently receives exact accepted head", || {
        let path = b.home.paths().ring_dir.join("roster.json");
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|v| v.to_string().contains(chain.head().token()))
    });
}
#[test]
fn retained_desktop_is_not_permission_for_create_or_foreign_join() {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    let original = legacy(&b, false);
    assert!(b
        .ring
        .create_computer(&b.companion(), |_, _| panic!("not Create"))
        .err()
        .expect("Create refused")
        .contains("leave_required"));
    assert!(b.ring.begin_join(&b.companion()).is_err());
    assert_eq!(b.ring.chain().unwrap().head(), original.head());
}
fn retained_success(both: bool) {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    let original = legacy(&b, both);
    let store = Store::new(b.home.root().join("desktop"));
    let old = store.load().unwrap().0.unwrap();
    let desktop_file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(b.home.root().join("desktop/ring.json")).unwrap())
            .unwrap();
    let dkeys = std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap();
    let terminal = uuid::Uuid::new_v4();
    let project = b.home.project("runtime4-retained-shell");
    let mut local = common::Client::connect(&b.daemon.as_ref().unwrap().socket);
    local.hello(common::range(1, 1));
    let opened = local.open(terminal, common::sh_spec(&project));
    let pid = opened["pid"].as_u64().unwrap() as i32;
    local.attach(terminal);
    local.input(terminal, "printf 'RUNTIME4-BEFORE\\n'\n");
    local.output_until(terminal, "RUNTIME4-BEFORE");
    let upgraded = installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), |p, lease| {
                assert_eq!(p.context.kind, ConsentFlow::LocalUpgrade);
                assert!(p
                    .proposal
                    .membership()
                    .unwrap()
                    .enrollments
                    .iter()
                    .all(|e| e.kind != EnrollmentKind::Computer));
                confirm(p, lease)
            })
            .unwrap(),
    );
    assert_eq!(upgraded.ring_id(), original.ring_id());
    assert_eq!(
        upgraded.get(original.head().version()),
        Some(original.head())
    );
    assert_eq!(
        upgraded.head().roster().members,
        original.head().roster().members
    );
    if both {
        wait_daemon_head(&b, &upgraded);
    }
    let singleton_ids: Vec<_> = upgraded
        .head()
        .roster()
        .membership()
        .unwrap()
        .enrollments
        .iter()
        .map(|u| u.id.clone())
        .collect();
    let accepted = installed(
        b.ring
            .finish_retained_computer(&b.companion(), |p, lease| {
                assert_eq!(p.context.kind, ConsentFlow::LegacyCompletion);
                let policy: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(b.home.paths().ring_dir.join("companion.json")).unwrap(),
                )
                .unwrap();
                assert!(policy["obligation"]["proofs"].is_null());
                if !both {
                    assert!(
                        !b.home.paths().ring_dir.join("roster.json").exists(),
                        "preparation must not install supplied trust"
                    );
                }
                confirm(p, lease)
            })
            .unwrap(),
    );
    let m = accepted.head().roster().membership().unwrap();
    assert_eq!(accepted.ring_id(), original.ring_id());
    assert_eq!(
        accepted.get(upgraded.head().version()),
        Some(upgraded.head())
    );
    assert_eq!(m.enrollments.len(), 1);
    assert_eq!(m.enrollments[0].kind, EnrollmentKind::Computer);
    assert!(!singleton_ids.contains(&m.enrollments[0].id));
    assert_eq!(m.retired, singleton_ids);
    let RosterChange::CompleteComputer {
        proofs, replaces, ..
    } = m.change
    else {
        panic!("not CompleteComputer")
    };
    assert_eq!(replaces, m.retired);
    assert_eq!(proofs.len(), 2);
    assert!(proofs
        .iter()
        .all(|p| p.context.kind == ConsentFlow::LegacyCompletion));
    let old_unit = upgraded
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit_for(&old.keys.sign_key())
        .unwrap()
        .clone();
    let mut cleanup = xshell_protocol::ring::relay::DepartureClient::connect(
        upgraded.clone(),
        old.keys.clone(),
        old_unit,
        None,
        WAIT,
    )
    .unwrap();
    assert!(matches!(
        cleanup.complete().unwrap(),
        xshell_protocol::ring::relay::DepartureStatus::Absent(_)
    ));
    assert_eq!(
        cleanup.chain().head(),
        accepted.head(),
        "old singleton cleanup cannot remove the replacement"
    );
    let after = store.load().unwrap().0.unwrap();
    assert_eq!(after.keys.sign_key(), old.keys.sign_key());
    assert_eq!(after.keys.noise_key(), old.keys.noise_key());
    let after_file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(b.home.root().join("desktop/ring.json")).unwrap())
            .unwrap();
    assert!(desktop_file["signSeed"] == after_file["signSeed"]);
    assert!(desktop_file["noiseSeed"] == after_file["noiseSeed"]);
    assert_eq!(after.host_members, old.host_members);
    if both {
        assert!(!after.host_members.is_empty());
    }
    assert_eq!(after.farewells, old.farewells);
    assert_eq!(
        std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap(),
        dkeys
    );
    let journal = store.load_profile().unwrap().0.unwrap().journal.unwrap();
    assert_eq!(journal.phase, AdoptionPhase::Installed);
    assert!(journal.desktop_installed && journal.daemon_installed);
    assert_eq!(journal.accepted.last().unwrap(), accepted.head().token());
    assert!(common::alive(pid));
    let mut verify = common::Client::connect(&b.daemon.as_ref().unwrap().socket);
    let (_, terminals) = verify.hello(common::range(1, 1));
    assert_eq!(
        terminals
            .iter()
            .find(|t| t.terminal == terminal)
            .unwrap()
            .pid,
        Some(pid as u32)
    );
    verify.attach(terminal);
    verify.output_until(terminal, "RUNTIME4-BEFORE");
    verify.input(terminal, "printf 'RUNTIME4-AFTER\\n'\n");
    verify.output_until(terminal, "RUNTIME4-AFTER");
}
#[test]
fn retained_desktop_only_public_upgrade_then_own_local_completion() {
    retained_success(false);
}
#[test]
fn retained_both_singletons_public_upgrade_then_own_local_completion() {
    retained_success(true);
}

fn no_proofs(b: &Computer) {
    let policy: serde_json::Value = serde_json::from_slice(
        &std::fs::read(b.home.paths().ring_dir.join("companion.json")).unwrap(),
    )
    .unwrap();
    assert!(policy["obligation"]["proofs"].is_null());
}
#[test]
fn retained_completion_decline_wrong_hash_and_cancel_never_group() {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    legacy(&b, false);
    let chain = installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), confirm)
            .unwrap(),
    );
    assert!(matches!(
        b.ring
            .finish_retained_computer(&b.companion(), |_, _| Ok(CreateDecision::Decline))
            .unwrap(),
        AdoptionOutcome::Declined { .. }
    ));
    no_proofs(&b);
    assert!(b
        .ring
        .finish_retained_computer(&b.companion(), |_, lease| {
            assert!(lease.confirm("wrong exact proposal hash").is_err());
            lease.cancel();
            assert!(lease.confirm("wrong exact proposal hash").is_err());
            Err("cancelled callback".into())
        })
        .is_err());
    no_proofs(&b);
    assert_eq!(b.ring.chain().unwrap().head(), chain.head());
    assert!(!b.home.paths().ring_dir.join("roster.json").exists());
    installed(
        b.ring
            .finish_retained_computer(&b.companion(), confirm)
            .unwrap(),
    );
}
#[test]
fn upgrade_decline_is_not_grouping_or_permission_to_create() {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    let old = legacy(&b, false);
    assert!(matches!(
        b.ring
            .upgrade_retained_ring(&b.companion(), |_, _| Ok(CreateDecision::Decline))
            .unwrap(),
        AdoptionOutcome::Declined { .. }
    ));
    assert_eq!(b.ring.chain().unwrap().head(), old.head());
    assert!(!b.home.paths().ring_dir.join("roster.json").exists());
    installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), confirm)
            .unwrap(),
    );
    let code = xshell_protocol::ring::device_code::DeviceCode::generate().unwrap();
    assert!(b
        .ring
        .approve_device_code(&code)
        .expect_err("migration singleton cannot approve pairing")
        .contains("Computer"));
}
#[test]
fn foreign_retained_daemon_is_not_replaced_by_local_desktop_completion() {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    let a = Computer::new(&relay);
    let identity = match b.companion().request(CompanionRequest::Descriptor).unwrap() {
        CompanionResponse::Identity { identity, .. } => identity,
        _ => panic!("identity"),
    };
    a.ring
        .enable(
            Some(LocalIdentity {
                name: identity.name,
                sign_key: identity.sign_key,
                noise_key: identity.noise_key,
            }),
            false,
        )
        .unwrap();
    let foreign = a.ring.chain().unwrap();
    let mut c = common::Client::connect(&b.daemon.as_ref().unwrap().socket);
    c.hello(common::range(1, 1));
    c.request(&ClientMsg::RingJoin {
        rosters: foreign
            .versions()
            .iter()
            .map(|r| r.token().into())
            .collect(),
        expect: None,
    })
    .unwrap();
    let bytes = std::fs::read(b.home.paths().ring_dir.join("roster.json")).unwrap();
    legacy(&b, false);
    installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), confirm)
            .unwrap(),
    );
    assert!(b
        .ring
        .finish_retained_computer(&b.companion(), |_, _| panic!(
            "foreign trust cannot reach consent"
        ))
        .is_err());
    assert_eq!(
        std::fs::read(b.home.paths().ring_dir.join("roster.json")).unwrap(),
        bytes
    );
    no_proofs(&b);
}

fn publication_deadline_after_locked_journal(upgrade: bool) {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use xshell_hostlink::ring::{
        AdoptionTestHook, AdoptionTestPoint, DesktopRing, DesktopRingConfig,
    };
    let relay = workerd::Relay::start();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let point = if upgrade {
        AdoptionTestPoint::UpgradeBeforeUncertainWrite
    } else {
        AdoptionTestPoint::CompletionBeforeUncertainWrite
    };
    let b = Computer::with_ring_config(&relay, |cfg| {
        cfg.pair_ttl = Duration::from_secs(3);
        cfg.adoption_test_hook = Some(AdoptionTestHook(Arc::new(move |at| {
            if at == point {
                ready_tx.send(()).unwrap();
                resume_rx.lock().unwrap().recv().unwrap();
            }
            if at == AdoptionTestPoint::RetainedPublicationAttempt {
                count.fetch_add(1, Ordering::SeqCst);
            }
        })));
    });
    let original = legacy(&b, false);
    let predecessor = if upgrade {
        original
    } else {
        installed(
            b.ring
                .upgrade_retained_ring(&b.companion(), confirm)
                .unwrap(),
        )
    };
    let baseline_attempts = attempts.load(Ordering::SeqCst);
    let store = Store::new(b.home.root().join("desktop"));
    let error = std::thread::scope(|scope| {
        let operation = scope.spawn(|| {
            let client = b.companion();
            if upgrade {
                b.ring.upgrade_retained_ring(&client, confirm)
            } else {
                b.ring.finish_retained_computer(&client, confirm)
            }
        });
        ready_rx.recv_timeout(WAIT).unwrap();
        let stored = store.load_profile().unwrap().0.unwrap();
        let expires = if upgrade {
            stored.outgoing.unwrap().intent.expires_at
        } else {
            stored.journal.unwrap().intent.expires_at
        };
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(b.home.root().join("desktop/ring.lock"))
            .unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        resume_tx.send(()).unwrap();
        while SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            <= expires
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
        operation
            .join()
            .unwrap()
            .err()
            .expect("fixed expiry after journal wait must refuse transmission")
    });
    assert!(error.starts_with("reconciliation_required:"), "{error}");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        baseline_attempts,
        "no late publication attempt, not just Worker rejection"
    );
    let stored = store.load_profile().unwrap().0.unwrap();
    if upgrade {
        assert_eq!(
            stored.outgoing.unwrap().phase,
            xshell_hostlink::ring::store::OutgoingPhase::PublicationUncertain
        );
    } else {
        assert_eq!(
            stored.journal.unwrap().phase,
            AdoptionPhase::PublicationUncertain
        );
    }
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &relay.target(),
            predecessor.ring_id()
        ),
        predecessor.head().version()
    );
    assert!(b
        .ring
        .upgrade_retained_ring(&b.companion(), |_, _| panic!(
            "uncertainty is not fresh authority"
        ))
        .is_err());
    assert!(b
        .ring
        .finish_retained_computer(&b.companion(), |_, _| panic!(
            "uncertainty is not fresh authority"
        ))
        .is_err());
    assert!(b.ring.begin_join(&b.companion()).is_err());
    assert!(b
        .ring
        .create_computer(&b.companion(), |_, _| panic!("retained is not Create"))
        .is_err());
    b.ring.quit();
    let mut cfg = DesktopRingConfig::new(b.home.root().join("desktop"), "same name".into());
    cfg.default_relay_url = relay.url();
    let restarted = DesktopRing::open(cfg, Arc::new(Quiet));
    assert!(restarted
        .upgrade_retained_ring(&b.companion(), |_, _| panic!("restart cannot replay"))
        .is_err());
    assert!(restarted
        .finish_retained_computer(&b.companion(), |_, _| panic!("restart cannot replay"))
        .is_err());
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &relay.target(),
            predecessor.ring_id()
        ),
        predecessor.head().version()
    );
    restarted.quit();
}
#[test]
fn upgrade_post_journal_wait_rechecks_fixed_expiry_and_never_replays_after_restart() {
    publication_deadline_after_locked_journal(true);
}
#[test]
fn completion_post_journal_wait_rechecks_fixed_expiry_and_never_replays_after_restart() {
    publication_deadline_after_locked_journal(false);
}

#[test]
fn protected_unpaired_daemon_refuses_unrelated_supplied_preparation_trust() {
    let relay = workerd::Relay::start();
    let a = Computer::new(&relay);
    legacy(&a, false);
    let foreign = installed(
        a.ring
            .upgrade_retained_ring(&a.companion(), confirm)
            .unwrap(),
    );
    let b = Computer::new(&relay);
    let profile = b.ring.register_companion(&b.companion()).unwrap();
    let operation = xshell_protocol::ring::relay::wire::new_nonce().unwrap();
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    let result = b
        .companion()
        .request(CompanionRequest::RetainedCompletionStart {
            intent: xshell_protocol::ring::pairing::adoption::AdoptionIntent {
                operation,
                expires_at,
                target: profile.target(),
            },
            predecessor: foreign
                .versions()
                .iter()
                .map(|r| r.token().into())
                .collect(),
        });
    assert!(result.is_err());
    assert!(!b.home.paths().ring_dir.join("roster.json").exists());
    assert!(b.ring.chain().is_none());
    no_proofs(&b);
}

#[test]
fn retained_local_completion_retries_only_exact_installation_after_either_store_failure() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use xshell_hostlink::ring::{
        AdoptionTestHook, AdoptionTestPoint, DesktopRing, DesktopRingConfig,
    };
    use xshelld::server::{TestHook, TestPoint};
    for fail_desktop in [false, true] {
        let relay = workerd::Relay::start();
        let armed = Arc::new(AtomicBool::new(false));
        let hit = Arc::new(AtomicBool::new(false));
        let a = armed.clone();
        let h = hit.clone();
        let b = Computer::with_config(&relay, |cfg| {
            cfg.test_hook = Some(TestHook(Arc::new(move |_, at| {
                if !fail_desktop
                    && at == TestPoint::BeforeRingRosterWrite
                    && a.load(Ordering::SeqCst)
                {
                    h.store(true, Ordering::SeqCst);
                    return true;
                }
                false
            })))
        });
        legacy(&b, false);
        installed(
            b.ring
                .upgrade_retained_ring(&b.companion(), confirm)
                .unwrap(),
        );
        // Reopen only the elected Desktop controller to install a deterministic failure
        // seam. No identities, membership or accepted rows are constructed/replaced.
        b.ring.quit();
        let desktop = b.home.root().join("desktop");
        let mut cfg = DesktopRingConfig::new(desktop.clone(), "same name".into());
        cfg.default_relay_url = relay.url();
        let file = desktop.join("ring.json");
        let path = file.clone();
        let a = armed.clone();
        let injected = Arc::new(Mutex::new(false));
        let once = injected.clone();
        cfg.adoption_test_hook = Some(AdoptionTestHook(Arc::new(move |at| {
            if at == AdoptionTestPoint::AcceptedBeforeCompanionInstall && !*once.lock().unwrap() {
                *once.lock().unwrap() = true;
                a.store(true, Ordering::SeqCst);
                if fail_desktop {
                    std::fs::rename(&path, path.with_extension("fault-backup")).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
            }
        })));
        let ring = DesktopRing::open(cfg, Arc::new(Quiet));
        wait("reopened retained Desktop connected", || {
            ring.view().connection == "connected"
        });
        let error = ring
            .finish_retained_computer(&b.companion(), confirm)
            .err()
            .expect("missing durable receipt cannot report Installed");
        assert!(error.starts_with("reconciliation_required:"), "{error}");
        armed.store(false, Ordering::SeqCst);
        if fail_desktop {
            std::fs::remove_dir(&file).unwrap();
            std::fs::rename(file.with_extension("fault-backup"), &file).unwrap();
        } else {
            assert!(
                hit.load(Ordering::SeqCst),
                "actual Daemon Saved.write_error path"
            );
        }
        let store = Store::new(desktop);
        let before = store.load_profile().unwrap().0.unwrap().journal.unwrap();
        assert_eq!(before.phase, AdoptionPhase::Accepted);
        let signed = before.signed_candidate.clone().unwrap();
        assert!(matches!(
            ring.install_companion_completion(&b.companion()).unwrap(),
            xshell_hostlink::ring::adoption::CompletionOutcome::Installed { .. }
        ));
        let after = store.load_profile().unwrap().0.unwrap().journal.unwrap();
        assert_eq!(after.signed_candidate, Some(signed.clone()));
        assert_eq!(after.accepted, before.accepted);
        assert_eq!(after.phase, AdoptionPhase::Installed);
        assert!(after.desktop_installed && after.daemon_installed);
        let chain = RosterChain::from_tokens(&after.accepted).unwrap();
        assert_eq!(
            xshell_protocol::ring::relay::contract::relay_version(&relay.target(), chain.ring_id()),
            chain.head().version(),
            "installation never republishes"
        );
        ring.quit();
    }
}

struct ProtocolEvents;
impl xshell_protocol::ring::relay::RingEvents for ProtocolEvents {
    fn envelope(&self, _: xshell_protocol::ring::SignKey, _: Vec<u8>) {}
    fn closed(&self, _: xshell_protocol::ring::relay::CloseReason) {}
    fn presence(
        &self,
        _: xshell_protocol::ring::SignKey,
        _: xshell_protocol::ring::relay::MemberPresence,
    ) {
    }
    fn roster(&self, _: &xshell_protocol::ring::SignedRoster) {}
    fn roster_rejected(&self, e: xshell_protocol::ring::RosterError) {
        panic!("negative-control successor rejected: {e}");
    }
}
fn legacy_mobile(b: &Computer) -> std::sync::Arc<xshell_protocol::ring::DeviceKeys> {
    use std::sync::Arc;
    use std::time::Duration;
    use xshell_protocol::ring::pairing::PairingOffer;
    use xshell_protocol::ring::relay::pair::{pair_as_guest, GuestRequest, PairOptions};
    use xshell_protocol::ring::{DeviceKeys, Role};
    // This independent negative-control signer is enrolled by the genuine public v1
    // phone pair, not by constructing authorized rows or a competing Desktop socket.
    let mobile = Arc::new(DeviceKeys::generate().unwrap());
    let offer = b.ring.pair_phone(Arc::new(Quiet)).unwrap();
    let offer = PairingOffer::parse(&offer.payload).unwrap();
    pair_as_guest(
        &GuestRequest {
            relay_url: &offer.relay_url,
            secret: &offer.secret,
            pin: Some(offer.noise_key),
            ring_id: Some(&offer.ring_id),
            keys: mobile.clone(),
            role: Role::Mobile,
            name: "synthetic-race-control",
            wait: Duration::ZERO,
        },
        &PairOptions::default(),
        None,
    )
    .unwrap();
    mobile
}
#[test]
fn local_completion_rechecks_both_current_heads_after_confirmation_wait() {
    use std::sync::Arc;
    use xshell_protocol::ring::relay::{RingClient, RingClientConfig};
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    legacy(&b, true);
    let mobile = legacy_mobile(&b);
    let upgraded = installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), confirm)
            .unwrap(),
    );
    wait_daemon_head(&b, &upgraded);
    let client = RingClient::connect(
        RingClientConfig::new(upgraded.clone(), mobile.clone()),
        Arc::new(ProtocolEvents),
    )
    .unwrap();
    let error = b
        .ring
        .finish_retained_computer(&b.companion(), |p, lease| {
            let mut successor = upgraded.head().roster().clone();
            successor.version += 1;
            successor.prev = Some(upgraded.head().hash());
            successor.signed_by = mobile.sign_key();
            let mut membership = successor.membership().unwrap();
            let old_mobile = membership.unit_for(&mobile.sign_key()).unwrap().clone();
            membership.enrollments.retain(|u| u.id != old_mobile.id);
            membership.retired.push(old_mobile.id.clone());
            membership.change = RosterChange::Remove {
                enrollment_id: old_mobile.id,
            };
            successor
                .members
                .retain(|m| m.sign_key != mobile.sign_key());
            successor.set_membership(&membership);
            let signed = successor.sign(mobile.as_ref()).unwrap();
            xshell_protocol::ring::verify_successor(upgraded.head(), &signed).unwrap();
            client.publish_roster(&signed).unwrap();
            let advanced = upgraded.extended(&[signed]).unwrap().0;
            wait_daemon_head(&b, &advanced);
            assert!(
                b.companion()
                    .request(CompanionRequest::CheckPrepared {
                        operation: p.operation.clone()
                    })
                    .is_err(),
                "Daemon independently refuses prepared stale head before proof release"
            );
            confirm(p, lease)
        })
        .err()
        .expect("changed head must not release proof or rebase");
    assert!(error.contains("changed"), "{error}");
    no_proofs(&b);
    let stored = Store::new(b.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert_eq!(stored.journal.unwrap().phase, AdoptionPhase::Cancelled);
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), upgraded.ring_id()),
        upgraded.head().version() + 1
    );
}

fn mobile_preservation(both: bool) {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    legacy(&b, both);
    let mobile = legacy_mobile(&b);
    let retained = b.ring.chain().unwrap();
    let upgraded = installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), confirm)
            .unwrap(),
    );
    assert_eq!(
        upgraded.head().roster().members,
        retained.head().roster().members
    );
    let membership = upgraded.head().roster().membership().unwrap();
    assert_eq!(membership.enrollments.len(), if both { 3 } else { 2 });
    assert!(membership.enrollments.iter().all(|u| u.members.len() == 1));
    let unproven = membership.unit_for(&mobile.sign_key()).unwrap().clone();
    assert_eq!(unproven.kind, EnrollmentKind::Mobile);
    let replaces: Vec<_> = membership
        .enrollments
        .iter()
        .filter(|u| u.id != unproven.id)
        .map(|u| u.id.clone())
        .collect();
    if both {
        wait_daemon_head(&b, &upgraded);
    }
    let mut confirmed = false;
    let completed = installed(
        b.ring
            .finish_retained_computer(&b.companion(), |p, lease| {
                assert!(!confirmed);
                confirmed = true;
                no_proofs(&b);
                assert_eq!(b.ring.chain().unwrap().head(), upgraded.head());
                if both {
                    wait_daemon_head(&b, &upgraded);
                }
                if !both {
                    assert!(
                        !b.home.paths().ring_dir.join("roster.json").exists(),
                        "Start/preparation must not install supplied trust"
                    );
                }
                assert_eq!(p.context.kind, ConsentFlow::LegacyCompletion);
                confirm(p, lease)
            })
            .unwrap(),
    );
    assert!(confirmed, "completion needs explicit local confirmation");
    let membership = completed.head().roster().membership().unwrap();
    assert_eq!(membership.unit_for(&mobile.sign_key()), Some(&unproven));
    for member in &upgraded.head().roster().members {
        assert_eq!(completed.head().member(&member.sign_key), Some(member));
    }
    assert_eq!(membership.retired, replaces);
    assert_eq!(membership.enrollments.len(), 2);
    let computer = membership
        .enrollments
        .iter()
        .find(|u| u.kind == EnrollmentKind::Computer)
        .unwrap();
    assert!(!membership.retired.contains(&computer.id));
    assert!(!membership.retired.contains(&unproven.id));
}
#[test]
fn public_upgrade_and_completion_leave_unproven_mobile_as_unchanged_singleton() {
    mobile_preservation(true);
}
#[test]
fn desktop_only_upgrade_and_completion_preserve_unrelated_mobile_without_start_install() {
    mobile_preservation(false);
}

#[test]
fn matching_retained_removal_heads_refuse_finish_before_confirmation_or_proofs() {
    use std::sync::Arc;
    use xshell_protocol::ring::relay::{RingClient, RingClientConfig};
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    legacy(&b, true);
    let mobile = legacy_mobile(&b);
    let upgraded = installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), confirm)
            .unwrap(),
    );
    wait_daemon_head(&b, &upgraded);
    let profile = b.ring.register_companion(&b.companion()).unwrap();
    let client = RingClient::connect(
        RingClientConfig::new(upgraded.clone(), mobile.clone()),
        Arc::new(ProtocolEvents),
    )
    .unwrap();
    let mut removed = upgraded.head().roster().clone();
    removed.version += 1;
    removed.prev = Some(upgraded.head().hash());
    removed.signed_by = mobile.sign_key();
    let mut membership = removed.membership().unwrap();
    let daemon = membership
        .unit_for(&profile.daemon.sign_key)
        .unwrap()
        .clone();
    membership.enrollments.retain(|u| u.id != daemon.id);
    membership.retired.push(daemon.id.clone());
    membership.change = RosterChange::Remove {
        enrollment_id: daemon.id,
    };
    removed
        .members
        .retain(|m| m.sign_key != profile.daemon.sign_key);
    removed.set_membership(&membership);
    let signed = removed.sign(mobile.as_ref()).unwrap();
    xshell_protocol::ring::verify_successor(upgraded.head(), &signed).unwrap();
    client.publish_roster(&signed).unwrap();
    let removed = upgraded.extended(&[signed]).unwrap().0;
    wait_daemon_head(&b, &removed);
    let store = Store::new(b.home.root().join("desktop"));
    wait("Desktop retains exact accepted removal head", || {
        store
            .load()
            .unwrap()
            .0
            .is_some_and(|s| s.chain.head() == removed.head())
            && b.ring.chain().is_some_and(|c| c.head() == removed.head())
    });
    let mut confirmed = false;
    let result = b.ring.finish_retained_computer(&b.companion(), |p, lease| {
        confirmed = true;
        confirm(p, lease)
    });
    let version =
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), removed.ring_id());
    let policy: serde_json::Value = serde_json::from_slice(
        &std::fs::read(b.home.paths().ring_dir.join("companion.json")).unwrap(),
    )
    .unwrap();
    assert!(!confirmed && policy["obligation"]["proofs"].is_null() && version == removed.head().version(), "removed Daemon must be refused before confirmation/proofs/publication: confirmed={confirmed}, proofs_released={}, Relay version={version}", !policy["obligation"]["proofs"].is_null());
    assert!(result
        .err()
        .expect("Finish must refuse removed Daemon")
        .contains("removed"));
    // The shared protected Start gate must independently reject matching retained
    // removal trust, even when the Desktop controller has already refused it.
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    assert!(b
        .companion()
        .request(CompanionRequest::RetainedCompletionStart {
            intent: xshell_protocol::ring::pairing::adoption::AdoptionIntent {
                operation: xshell_protocol::ring::relay::wire::new_nonce().unwrap(),
                expires_at,
                target: profile.target(),
            },
            predecessor: removed
                .versions()
                .iter()
                .map(|r| r.token().into())
                .collect(),
        })
        .is_err());
    no_proofs(&b);
    assert_eq!(
        store.load().unwrap().0.unwrap().chain.head(),
        removed.head()
    );
    wait_daemon_head(&b, &removed);
    assert!(store.load_profile().unwrap().0.unwrap().journal.is_none());
    assert!(removed
        .authorized_member(&profile.daemon.sign_key)
        .is_none());
}

// ── Desktop Ring UI backend (#87) ─────────────────────────────────────────

use xshell_hostlink::ring::adoption::{ApprovalReadiness, JoinStatus};

#[test]
fn local_companion_client_errors_until_registered_and_readiness_follows_create() {
    let relay = workerd::Relay::start();
    let a = Computer::new(&relay);
    let error = a
        .ring
        .local_companion_client()
        .err()
        .expect("not registered");
    assert!(error.starts_with("not_registered"), "{error}");
    assert_eq!(
        a.ring.approval_readiness().unwrap(),
        ApprovalReadiness::NoProfile
    );
    a.ring.register_companion(&a.companion()).unwrap();
    let client = a.ring.local_companion_client().unwrap();
    assert_eq!(client.endpoint(), a.companion().endpoint());
    assert_eq!(
        a.ring.approval_readiness().unwrap(),
        ApprovalReadiness::NotEnrolled
    );
    // Create through the client the UI uses: the first full member can approve at once.
    match a.ring.create_computer(&client, confirm).unwrap() {
        AdoptionOutcome::Installed { .. } => {}
        _ => panic!("Create not installed"),
    }
    assert_eq!(
        a.ring.approval_readiness().unwrap(),
        ApprovalReadiness::Ready
    );
}

#[test]
fn legacy_ring_needs_upgrade_then_completion_then_is_ready() {
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    legacy(&b, false);
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::NeedsUpgrade
    );
    installed(
        b.ring
            .upgrade_retained_ring(&b.companion(), confirm)
            .unwrap(),
    );
    // Persisted half-way state: the dispatcher completes only.
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::NeedsCompletion
    );
    let client = b.ring.local_companion_client().unwrap();
    installed(b.ring.finish_retained_computer(&client, confirm).unwrap());
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::Ready
    );
}

#[test]
fn cancelled_join_start_aborts_the_ready_wait_and_never_shows_an_offer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    let relay = workerd::Relay::start();
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(Some((reached_tx, release_rx))));
    let hook_gate = gate.clone();
    let hook = AdoptionTestHook(Arc::new(move |point| {
        if point == AdoptionTestPoint::JoinSlotReady {
            // Only the first start is held at the slot acknowledgment.
            if let Some((reached, release)) = hook_gate.lock().unwrap().take() {
                reached.send(()).unwrap();
                release.recv().unwrap();
            }
        }
    }));
    let b = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = Some(hook));
    b.ring.register_companion(&b.companion()).unwrap();
    let client = b.ring.local_companion_client().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (ring, start_client, start_cancel) = (b.ring.clone(), client.clone(), cancel.clone());
    let start =
        std::thread::spawn(move || ring.begin_join_cancellable(&start_client, start_cancel, None));
    reached_rx.recv_timeout(WAIT).expect("readiness reached");
    // The UI's cancel command during the start.
    cancel.store(true, Ordering::Release);
    release_tx.send(()).unwrap();
    let error = start
        .join()
        .unwrap()
        .err()
        .expect("a cancelled start shows no code");
    assert!(error.starts_with("cancelled"), "{error}");
    // The cancelled driver ended: its reservation is gone and nothing is uncertain.
    let store = Store::new(b.home.root().join("desktop"));
    let stored = store.load_profile().unwrap().0.unwrap();
    assert!(stored.pending_join.is_none());
    assert!(stored.membership.is_none());
    assert!(stored.journal.as_ref().is_none_or(|j| matches!(
        j.phase,
        AdoptionPhase::Cancelled | AdoptionPhase::ExpiredBeforeProof
    )));
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::NotEnrolled
    );
    // The admission is free again: a new Join starts and waits for its approver.
    let offer = b
        .ring
        .begin_join_cancellable(&client, Arc::new(AtomicBool::new(false)), None)
        .unwrap();
    assert!(offer.is_ready());
    assert!(matches!(offer.join_status(), JoinStatus::Waiting));
    offer.cancel_join();
    drop(offer);
}

#[test]
fn approve_needs_an_upgraded_computer_desktop() {
    let relay = workerd::Relay::start();
    let a = Computer::new(&relay);
    legacy(&a, false);
    let code = xshell_protocol::ring::device_code::DeviceCode::generate().unwrap();
    let error = a.ring.approve_device_code(&code).unwrap_err();
    assert!(error.contains("approval requires"), "{error}");
}

#[test]
fn create_and_join_bind_the_chosen_relay_not_the_default() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let relay = workerd::Relay::start();
    // The default points nowhere: success proves the chosen Relay is bound.
    let unreachable = "ws://127.0.0.1:9/";
    let a = Computer::with_ring_config(&relay, |cfg| cfg.default_relay_url = unreachable.into());
    a.ring.register_companion(&a.companion()).unwrap();
    let chain = installed(
        a.ring
            .create_computer_on(&a.companion(), &relay.url(), confirm)
            .unwrap(),
    );
    assert_eq!(chain.head().roster().relay_url, relay.url());
    assert!(a
        .ring
        .create_computer_on(&a.companion(), "not a url", confirm)
        .is_err());
    let b = Computer::with_ring_config(&relay, |cfg| cfg.default_relay_url = unreachable.into());
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b
        .ring
        .begin_join_cancellable(
            &b.ring.local_companion_client().unwrap(),
            Arc::new(AtomicBool::new(false)),
            Some(&relay.url()),
        )
        .unwrap();
    assert!(offer.is_ready());
    offer.cancel_join();
    drop(offer);
}

#[test]
fn an_unresolved_join_stays_unresolved_while_registration_holds_the_admission() {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    let relay = workerd::Relay::start();
    // 1: abort a Join after its durable reservation (an earlier, now unresolved Join).
    // 2: hold the next registration's admission until released.
    let mode = Arc::new(AtomicU8::new(0));
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(Some((reached_tx, release_rx))));
    let hook_mode = mode.clone();
    let hook = AdoptionTestHook(Arc::new(move |point| {
        match hook_mode.load(Ordering::SeqCst) {
            1 if point == AdoptionTestPoint::JoinBeforeCounterpartStart => {
                hook_mode.store(0, Ordering::SeqCst);
                panic!("simulated crash after the Join reservation");
            }
            2 if point == AdoptionTestPoint::RegistrationAdmitted => {
                if let Some((reached, release)) = gate.lock().unwrap().take() {
                    reached.send(()).unwrap();
                    release.recv().unwrap();
                }
            }
            _ => {}
        }
    }));
    let b = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = Some(hook));
    b.ring.register_companion(&b.companion()).unwrap();
    mode.store(1, Ordering::SeqCst);
    let (ring, client) = (b.ring.clone(), b.ring.local_companion_client().unwrap());
    let crashed = std::thread::spawn(move || {
        ring.begin_join_cancellable(&client, Arc::new(AtomicBool::new(false)), None)
            .map(|_| ())
    })
    .join();
    assert!(crashed.is_err(), "the simulated crash ended the start");
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::Unresolved
    );
    assert!(!b.ring.adoption_busy());
    // The startup registration loop holds the admission: still unresolved, and busy.
    mode.store(2, Ordering::SeqCst);
    let (ring, companion) = (b.ring.clone(), b.companion());
    let registering = std::thread::spawn(move || ring.register_companion(&companion));
    reached_rx
        .recv_timeout(WAIT)
        .expect("registration admitted");
    assert!(b.ring.adoption_busy());
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::Unresolved
    );
    release_tx.send(()).unwrap();
    registering.join().unwrap().unwrap();
    assert!(!b.ring.adoption_busy());
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::Unresolved
    );
}

#[test]
fn a_live_join_is_busy_not_unresolved() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let relay = workerd::Relay::start();
    let b = Computer::new(&relay);
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b
        .ring
        .begin_join_cancellable(
            &b.ring.local_companion_client().unwrap(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap();
    assert!(b.ring.adoption_busy());
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::NotEnrolled
    );
    offer.cancel_join();
    drop(offer);
    assert!(!b.ring.adoption_busy());
    assert_eq!(
        b.ring.approval_readiness().unwrap(),
        ApprovalReadiness::NotEnrolled
    );
}

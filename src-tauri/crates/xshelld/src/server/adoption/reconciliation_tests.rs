//! Cryptographic unit/normal-owner-path fixtures, NOT public-workerd setup evidence.
use super::*;
use crate::server::{Config, Server};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_protocol::ring::activity::{policy_hash, ActivityEvidence, UnitActivity};
use xshell_protocol::ring::companion::*;
use xshell_protocol::ring::membership::*;
use xshell_protocol::ring::{b64, DeviceKeys, Member, RingId, Roster, Signature, Signer};

struct Fixture {
    handle: ServerHandle,
    chain: RosterChain,
    source: DeviceKeys,
    desktop: DeviceKeys,
    daemon: Arc<DeviceKeys>,
    other: DeviceKeys,
    client: CompanionClient,
    _home: tempfile::TempDir,
}
fn identity(keys: &DeviceKeys, name: &str) -> PublicIdentity {
    PublicIdentity {
        name: name.into(),
        sign_key: keys.sign_key(),
        noise_key: keys.noise_key(),
    }
}
fn complete_proofs(proposal: &Roster, keys: &[&DeviceKeys], pairing: bool) -> Roster {
    let context = if pairing {
        pairing_consent_context(proposal, &[42; 32]).unwrap()
    } else {
        local_consent_context(proposal).unwrap()
    };
    let message = consent_message(proposal, 1600, &context).unwrap();
    let proofs = keys
        .iter()
        .map(|k| ConsentProof {
            key: k.sign_key(),
            expires_at: 1600,
            context: context.clone(),
            signature: Signature::from_bytes(k.sign(&message).unwrap()),
        })
        .collect();
    let mut roster = proposal.clone();
    let mut membership = roster.membership().unwrap();
    match &mut membership.change {
        RosterChange::Genesis { proofs: p } | RosterChange::Enroll { proofs: p, .. } => *p = proofs,
        _ => panic!("fixture change"),
    }
    roster.set_membership(&membership);
    roster
}
impl Fixture {
    fn new(standalone: bool) -> Self {
        Self::with_hook(standalone, None)
    }
    fn with_hook(standalone: bool, hook: Option<crate::server::TestHook>) -> Self {
        let home = tempfile::Builder::new()
            .prefix("r5-evidence-")
            .tempdir()
            .unwrap();
        let paths = crate::paths::resolve(home.path(), None, None);
        let mut cfg = Config::new(home.path().into(), paths.clone());
        cfg.ring_backoff_unit = Duration::from_millis(10);
        cfg.test_hook = hook;
        let handle = Server::start(cfg).unwrap();
        handle.d.ring.identity().unwrap();
        let store = Store::new(paths.ring_dir.clone());
        let daemon = Arc::new(store.load_keys().unwrap().unwrap());
        let desktop = DeviceKeys::generate().unwrap();
        let source = DeviceKeys::generate().unwrap();
        let other = DeviceKeys::generate().unwrap();
        let other_daemon = DeviceKeys::generate().unwrap();
        let genesis_daemon = if standalone {
            &other_daemon
        } else {
            daemon.as_ref()
        };
        let mut genesis = Roster {
            v: 2,
            ring_id: RingId::derive(&desktop.sign_key()),
            version: 1,
            prev: None,
            relay_url: "ws://127.0.0.1:9".into(),
            signed_by: desktop.sign_key(),
            issued_at: 1000,
            members: vec![
                Member::new(
                    "desktop",
                    xshell_protocol::ring::Role::Desktop,
                    desktop.sign_key(),
                    desktop.noise_key(),
                    1000,
                ),
                Member::new(
                    "daemon",
                    xshell_protocol::ring::Role::Daemon,
                    genesis_daemon.sign_key(),
                    genesis_daemon.noise_key(),
                    1000,
                ),
            ],
            extra: Default::default(),
        };
        genesis.set_membership(&Membership {
            enrollments: vec![Enrollment {
                id: b64::encode(&[1; 32]),
                kind: EnrollmentKind::Computer,
                members: vec![desktop.sign_key(), genesis_daemon.sign_key()],
                enrolled_at: 1000,
            }],
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: Some(30),
                activity_key: source.sign_key(),
            },
            change: RosterChange::Genesis { proofs: vec![] },
        });
        let signed = complete_proofs(&genesis, &[&desktop, genesis_daemon], false)
            .sign(&desktop)
            .unwrap();
        let predecessor = RosterChain::from_chain(vec![signed]).unwrap();
        let target = if standalone {
            JoinTarget::StandaloneDaemon {
                daemon: identity(&daemon, "standalone"),
            }
        } else {
            JoinTarget::Computer {
                desktop: identity(&other, "other"),
                daemon: identity(&other_daemon, "other-daemon"),
            }
        };
        let intent = AdoptionIntent {
            operation: b64::encode(&[2; 32]),
            expires_at: 1600,
            target,
        };
        let mut proposal = predecessor.head().roster().clone();
        proposal.version += 1;
        proposal.prev = Some(predecessor.head().hash());
        proposal.issued_at = 1001;
        for (i, role) in intent.target.identities() {
            proposal
                .members
                .push(Member::new(&i.name, role, i.sign_key, i.noise_key, 1001));
        }
        let mut m = proposal.membership().unwrap();
        m.enrollments.push(Enrollment {
            id: intent.operation.clone(),
            kind: if standalone {
                EnrollmentKind::Daemon
            } else {
                EnrollmentKind::Computer
            },
            members: intent
                .target
                .identities()
                .iter()
                .map(|(i, _)| i.sign_key)
                .collect(),
            enrolled_at: 1001,
        });
        m.change = RosterChange::Enroll {
            enrollment_id: intent.operation.clone(),
            proofs: vec![],
        };
        proposal.set_membership(&m);
        let signers = if standalone {
            vec![daemon.as_ref()]
        } else {
            vec![&other, &other_daemon]
        };
        let signed = complete_proofs(&proposal, &signers, true)
            .sign(&desktop)
            .unwrap();
        let chain = predecessor.extended(&[signed]).unwrap().0;
        let tokens: Vec<String> = chain.versions().iter().map(|r| r.token().into()).collect();
        handle.d.ring.join(&tokens, None).unwrap();
        let client =
            CompanionClient::for_store(handle.companion_endpoint().unwrap().parent().unwrap())
                .unwrap();
        if standalone {
            let journal = StandaloneJournal {
                intent,
                proposal: CompanionProposal {
                    operation: b64::encode(&[2; 32]),
                    predecessor: predecessor
                        .versions()
                        .iter()
                        .map(|r| r.token().into())
                        .collect(),
                    proposal,
                    transcript: ConsentTranscript::Pairing {
                        final_noise_hash: b64::encode(&[42; 32]),
                    },
                },
                consent_may_have_left: true,
                accepted: tokens,
                installed: true,
            };
            accepted_record(&store, &journal).unwrap();
            write_private(
                &store.dir.join("standalone-adoption.json"),
                &serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
        } else {
            let desktop_dir = home.path().join("desktop");
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&desktop_dir)
                .unwrap();
            xshell_core::local_ipc::private_dir(&desktop_dir).unwrap();
            let profile = CompanionProfile::for_stores(
                desktop_dir.canonicalize().unwrap().to_str().unwrap().into(),
                store.dir.canonicalize().unwrap().to_str().unwrap().into(),
                identity(&desktop, "desktop"),
                identity(&daemon, "daemon"),
                client.endpoint().to_str().unwrap().into(),
            );
            let proof =
                Signature::from_bytes(desktop.sign(&registration_message(&profile)).unwrap());
            client
                .request(CompanionRequest::Register { profile, proof })
                .unwrap();
        }
        Self {
            handle,
            chain,
            source,
            desktop,
            daemon,
            other,
            client,
            _home: home,
        }
    }
    fn terminal_token(&self, key: xshell_protocol::ring::SignKey) -> String {
        let membership = self.chain.head().roster().membership().unwrap();
        let unit = membership.unit_for(&key).unwrap();
        let token = ActivityEvidence {
            ring_id: self.chain.ring_id().clone(),
            relay_url: self.chain.head().roster().relay_url.clone(),
            policy_head_hash: self.chain.head().hash(),
            policy_hash: policy_hash(&membership.policy),
            observed_at: 2000,
            units: vec![UnitActivity {
                enrollment_id: unit.id.clone(),
                member_keys: unit.members.clone(),
                online: false,
                last_active: unit.enrolled_at,
                terminal_expired: true,
            }],
        }
        .sign(&self.source)
        .unwrap();
        token
    }
    fn observe_terminal(&self, key: xshell_protocol::ring::SignKey) -> RosterChain {
        let token = self.terminal_token(key);
        let owner = self.handle.d.ring.push_link().unwrap().0;
        owner
            .test_with_stable_attempt(|| {
                owner.test_observe_activity(
                    &ChainPin {
                        version: self.chain.head().version(),
                        hash: self.chain.head().hash(),
                    },
                    self.daemon.sign_key(),
                    &token,
                    || {},
                )
            })
            .unwrap();
        let effective = owner.chain();
        assert_eq!(
            effective.head(),
            self.chain.head(),
            "no signed cleanup appended"
        );
        assert!(effective.authorized_member(&key).is_none());
        assert!(
            self.handle
                .d
                .ring
                .trusted_chain()
                .unwrap()
                .authorized_member(&key)
                .is_some(),
            "regression requires same-head cache blind spot"
        );
        effective
    }
}
#[test]
fn membership_snapshot_refuses_owner_same_head_local_terminal_evidence() {
    let f = Fixture::new(false);
    f.observe_terminal(f.daemon.sign_key());
    let error = f
        .client
        .request(CompanionRequest::MembershipSnapshot)
        .unwrap_err();
    assert!(error.contains("terminal evidence"), "{error}");
}
#[test]
fn standalone_status_uses_owner_same_head_terminal_evidence() {
    let f = Fixture::new(true);
    f.observe_terminal(f.daemon.sign_key());
    assert_eq!(
        f.handle
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        StandaloneRecoveryState::Terminal
    );
    assert_eq!(
        f.handle.reconcile_standalone_adoption().unwrap().state,
        StandaloneRecoveryState::Terminal
    );
}
#[test]
fn membership_snapshot_cannot_strip_another_computers_terminal_signer_fence() {
    let f = Fixture::new(false);
    let effective = f.observe_terminal(f.other.sign_key());
    assert!(effective.authorized_member(&f.desktop.sign_key()).is_some());
    assert!(effective.authorized_member(&f.daemon.sign_key()).is_some());
    // Otherwise valid Remove signed by the terminal other Computer, targeting the local
    // Computer. Without external observation the full signature/structure verifier accepts.
    let mut next = f.chain.head().roster().clone();
    next.version += 1;
    next.prev = Some(f.chain.head().hash());
    next.issued_at = 2001;
    next.signed_by = f.other.sign_key();
    let mut m = next.membership().unwrap();
    let removed = m.unit_for(&f.desktop.sign_key()).unwrap().clone();
    m.enrollments.retain(|u| u.id != removed.id);
    m.retired.push(removed.id.clone());
    m.change = RosterChange::Remove {
        enrollment_id: removed.id,
    };
    next.members
        .retain(|x| !removed.members.contains(&x.sign_key));
    next.set_membership(&m);
    let signed = next.sign(&f.other).unwrap();
    f.chain.extended(std::slice::from_ref(&signed)).unwrap();
    assert!(
        effective.extended(&[signed]).is_err(),
        "known terminal signer cannot authorize successor"
    );
    assert!(
        f.client
            .request(CompanionRequest::MembershipSnapshot)
            .is_err(),
        "never transfer a token-only chain that reauthorizes that signer"
    );
}

#[test]
fn owner_observation_test_seam_rejects_wrong_pin_principal_signature_and_stale_generation() {
    let f = Fixture::new(false);
    let owner = f.handle.d.ring.push_link().unwrap().0;
    let pin = ChainPin {
        version: f.chain.head().version(),
        hash: f.chain.head().hash(),
    };
    let token = f.terminal_token(f.other.sign_key());
    let wrong_pin = ChainPin {
        version: pin.version,
        hash: b64::encode(&[0; 32]),
    };
    assert!(owner
        .test_observe_activity(&wrong_pin, f.daemon.sign_key(), &token, || {})
        .is_err());
    assert!(owner
        .test_observe_activity(&pin, f.desktop.sign_key(), &token, || {})
        .is_err());
    let evidence = ActivityEvidence::parse(&token, &f.source.sign_key()).unwrap();
    assert!(owner
        .test_observe_activity(
            &pin,
            f.daemon.sign_key(),
            &evidence.sign(&f.desktop).unwrap(),
            || {}
        )
        .is_err());
    let mut wrong_binding = evidence;
    wrong_binding.policy_head_hash = wrong_pin.hash;
    assert!(owner
        .test_observe_activity(
            &pin,
            f.daemon.sign_key(),
            &wrong_binding.sign(&f.source).unwrap(),
            || {}
        )
        .is_err());
    assert!(owner.chain().terminal_enrollments().is_empty());
    assert!(owner
        .test_observe_activity(&pin, f.daemon.sign_key(), &token, || owner
            .stop(xshell_protocol::ring::relay::ByeReason::quit()))
        .is_err());
    assert!(
        owner.chain().terminal_enrollments().is_empty(),
        "stale generation cannot install observation"
    );
}

fn tokens(chain: &RosterChain) -> Vec<String> {
    chain.versions().iter().map(|r| r.token().into()).collect()
}
fn enroll_fixture(
    chain: &RosterChain,
    signer: &DeviceKeys,
    target: JoinTarget,
    signers: &[&DeviceKeys],
    id: u8,
) -> RosterChain {
    let mut proposal = chain.head().roster().clone();
    proposal.version += 1;
    proposal.prev = Some(chain.head().hash());
    proposal.issued_at += 1;
    proposal.signed_by = signer.sign_key();
    for (i, role) in target.identities() {
        proposal.members.push(Member::new(
            &i.name,
            role,
            i.sign_key,
            i.noise_key,
            proposal.issued_at,
        ));
    }
    let mut m = proposal.membership().unwrap();
    m.enrollments.push(Enrollment {
        id: b64::encode(&[id; 32]),
        kind: if matches!(target, JoinTarget::Computer { .. }) {
            EnrollmentKind::Computer
        } else {
            EnrollmentKind::Daemon
        },
        members: target
            .identities()
            .iter()
            .map(|(i, _)| i.sign_key)
            .collect(),
        enrolled_at: proposal.issued_at,
    });
    m.change = RosterChange::Enroll {
        enrollment_id: b64::encode(&[id; 32]),
        proofs: vec![],
    };
    proposal.set_membership(&m);
    let signed = complete_proofs(&proposal, signers, true)
        .sign(signer)
        .unwrap();
    chain.extended(&[signed]).unwrap().0
}
fn remove_fixture(
    chain: &RosterChain,
    signer: &DeviceKeys,
    key: xshell_protocol::ring::SignKey,
) -> RosterChain {
    let mut proposal = chain.head().roster().clone();
    proposal.version += 1;
    proposal.prev = Some(chain.head().hash());
    proposal.issued_at += 1;
    proposal.signed_by = signer.sign_key();
    let mut m = proposal.membership().unwrap();
    let unit = m.unit_for(&key).unwrap().clone();
    m.enrollments.retain(|u| u.id != unit.id);
    m.retired.push(unit.id.clone());
    m.change = RosterChange::Remove {
        enrollment_id: unit.id,
    };
    proposal
        .members
        .retain(|i| !unit.members.contains(&i.sign_key));
    proposal.set_membership(&m);
    chain.extended(&[proposal.sign(signer).unwrap()]).unwrap().0
}
fn other_terminal_standalone() -> (Fixture, RosterChain) {
    let mut f = Fixture::new(true);
    let c_daemon = DeviceKeys::generate().unwrap();
    f.chain = enroll_fixture(
        &f.chain,
        &f.desktop,
        JoinTarget::Computer {
            desktop: identity(&f.other, "C"),
            daemon: identity(&c_daemon, "C-daemon"),
        },
        &[&f.other, &c_daemon],
        3,
    );
    f.handle.d.ring.join(&tokens(&f.chain), None).unwrap();
    let effective = f.observe_terminal(f.desktop.sign_key()); // Computer B, not local D
    let extra = DeviceKeys::generate().unwrap();
    let newer = enroll_fixture(
        &f.chain,
        &f.other,
        JoinTarget::StandaloneDaemon {
            daemon: identity(&extra, "extra"),
        },
        &[&extra],
        4,
    );
    let effective_newer = effective.extended(newer.versions()).unwrap().0;
    assert!(effective_newer
        .authorized_member(&f.daemon.sign_key())
        .is_some());
    (f, effective_newer)
}
fn set_journal(f: &Fixture, accepted: &RosterChain, installed: bool) -> StandaloneJournal {
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let mut record = load_journal(&store).unwrap().unwrap();
    record.accepted = tokens(accepted);
    record.installed = installed;
    accepted_record(&store, &record).unwrap();
    let _lock = store.lock().unwrap();
    write_private(
        &store.dir.join("standalone-adoption.json"),
        &serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    record
}
fn budget() -> xshell_core::local_ipc::RequestBudget {
    xshell_core::local_ipc::RequestBudget::for_operation(
        Instant::now() + Duration::from_secs(5),
        Arc::new(AtomicBool::new(false)),
    )
}
fn register_standalone_profile(f: &Fixture) {
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let desktop_dir = f._home.path().join("desktop");
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&desktop_dir)
        .unwrap();
    let profile = CompanionProfile::for_stores(
        desktop_dir.canonicalize().unwrap().to_str().unwrap().into(),
        store.dir.canonicalize().unwrap().to_str().unwrap().into(),
        identity(&f.desktop, "desktop"),
        identity(&f.daemon, "daemon"),
        f.client.endpoint().to_str().unwrap().into(),
    );
    let proof = Signature::from_bytes(f.desktop.sign(&registration_message(&profile)).unwrap());
    f.client
        .request(CompanionRequest::Register { profile, proof })
        .unwrap();
}
#[test]
fn standalone_install_pending_denial_requires_exact_record_and_safe_nonuncertain_metadata() {
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    let f = Fixture::new(true);
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let record = load_journal(&store).unwrap().unwrap();
    let desktop = f._home.path().join("desktop");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&desktop)
        .unwrap();
    xshell_core::companion_reservation::reserve(f._home.path(), &desktop).unwrap();
    let pending = store.dir.join(xshell_core::companion_reservation::FILE);
    let original = std::fs::read(&pending).unwrap();
    let snapshot = || {
        ["keys.json", "roster.json", "standalone-adoption.json"]
            .map(|name| std::fs::read(store.dir.join(name)).unwrap())
    };
    let before = snapshot();
    let install = |operation: &str| {
        f.handle
            .d
            .ring
            .join_accepted_standalone(&record.accepted, operation, &budget())
    };
    install(&record.intent.operation).unwrap();
    assert!(
        install(&b64::encode(&[99; 32])).is_err(),
        "pending cannot authorize a different operation"
    );
    assert!(
        f.handle.d.ring.join(&record.accepted, None).is_err(),
        "Host origin still denied"
    );
    assert!(
        store.reserve_legacy_offer().is_err(),
        "new offers still denied"
    );
    assert_eq!(std::fs::read(&pending).unwrap(), original);
    let mut uncertain = xshell_core::companion_reservation::read_reservation(&store.dir)
        .unwrap()
        .unwrap();
    uncertain.legacy_outcome_uncertain = true;
    for bytes in [serde_json::to_vec(&uncertain).unwrap(), b"{".to_vec()] {
        write_private(&pending, &bytes).unwrap();
        assert!(install(&record.intent.operation).is_err());
        assert_eq!(std::fs::read(&pending).unwrap(), bytes);
        assert_eq!(snapshot(), before);
    }
    write_private(&pending, &original).unwrap();
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        install(&record.intent.operation).is_err(),
        "unsafe opened-object permissions denied"
    );
    std::fs::remove_file(&pending).unwrap();
    symlink(store.dir.join("keys.json"), &pending).unwrap();
    assert!(install(&record.intent.operation).is_err(), "symlink denied");
    std::fs::remove_file(&pending).unwrap();
    let fifo = std::ffi::CString::new(pending.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(
        install(&record.intent.operation).is_err(),
        "FIFO denied without blocking"
    );
    assert_eq!(snapshot(), before);
}

#[test]
fn standalone_install_actual_registration_remains_excluded() {
    let f = Fixture::new(true);
    register_standalone_profile(&f);
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let record = load_journal(&store).unwrap().unwrap();
    let policy = std::fs::read(store.dir.join("companion.json")).unwrap();
    let error = f
        .handle
        .d
        .ring
        .join_accepted_standalone(&record.accepted, &record.intent.operation, &budget())
        .unwrap_err();
    assert!(error.contains("registered counterpart"), "{error}");
    assert_eq!(
        std::fs::read(store.dir.join("companion.json")).unwrap(),
        policy
    );
}

#[test]
fn newer_standalone_install_preserves_other_computer_terminal_signer_and_export_fences() {
    let (f, accepted) = other_terminal_standalone();
    let record = set_journal(&f, &accepted, false);
    f.handle
        .d
        .ring
        .join_accepted_standalone(&record.accepted, &record.intent.operation, &budget())
        .unwrap();
    let owner = f.handle.d.ring.push_link().unwrap().0;
    assert_eq!(owner.chain().head(), accepted.head());
    assert!(
        owner
            .chain()
            .authorized_member(&f.desktop.sign_key())
            .is_none(),
        "newer install must not erase B terminal signer fence"
    );
    let stripped = RosterChain::from_tokens(&record.accepted).unwrap();
    let forbidden = remove_fixture(&stripped, &f.desktop, f.other.sign_key());
    assert!(owner.chain().extended(forbidden.versions()).is_err());
    let before = std::fs::read(f.handle.d.cfg.paths.ring_dir.join("roster.json")).unwrap();
    let bad = set_journal(&f, &forbidden, false);
    assert!(
        f.handle
            .d
            .ring
            .join_accepted_standalone(&bad.accepted, &bad.intent.operation, &budget())
            .is_err(),
        "terminal B-signed successor must be refused at actual install"
    );
    assert_eq!(
        std::fs::read(f.handle.d.cfg.paths.ring_dir.join("roster.json")).unwrap(),
        before
    );
    set_journal(&f, &accepted, true);
    register_standalone_profile(&f);
    assert!(f
        .client
        .request(CompanionRequest::MembershipSnapshot)
        .unwrap_err()
        .contains("terminal evidence"));
}
#[test]
fn standalone_install_rejects_a_forbidden_successor_before_any_store_write() {
    let (f, _) = other_terminal_standalone();
    let forbidden = remove_fixture(&f.chain, &f.desktop, f.other.sign_key());
    let record = set_journal(&f, &forbidden, false);
    let file = f.handle.d.cfg.paths.ring_dir.join("roster.json");
    let before = std::fs::read(&file).unwrap();
    assert!(
        f.handle
            .d
            .ring
            .join_accepted_standalone(&record.accepted, &record.intent.operation, &budget())
            .is_err(),
        "do not accept first and merge terminal IDs afterward"
    );
    assert_eq!(std::fs::read(&file).unwrap(), before);
}
#[test]
fn standalone_install_validates_newer_durable_suffix_against_effective_owner_before_activation() {
    let (f, accepted) = other_terminal_standalone();
    let stripped = RosterChain::from_tokens(&tokens(&accepted)).unwrap();
    let forbidden = remove_fixture(&stripped, &f.desktop, f.other.sign_key());
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    {
        let _lock = store.lock().unwrap();
        store.write_chain(&forbidden).unwrap();
    }
    let record = set_journal(&f, &accepted, false);
    assert!(
        f.handle
            .d
            .ring
            .join_accepted_standalone(&record.accepted, &record.intent.operation, &budget())
            .is_err(),
        "a newer token-backed disk suffix cannot bypass effective signer constraints"
    );
    let owner = f.handle.d.ring.push_link().unwrap().0;
    assert_eq!(owner.chain().head(), f.chain.head());
    assert!(owner
        .chain()
        .authorized_member(&f.desktop.sign_key())
        .is_none());
}
fn final_receipt_observes_terminal(was_installed: bool) {
    use std::sync::{mpsc, Mutex};
    let (at, reached) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let f = Fixture::with_hook(
        true,
        Some(crate::server::TestHook(Arc::new(move |_, point| {
            if point == crate::server::TestPoint::StandaloneInstalledBeforeJournal {
                at.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            false
        }))),
    );
    let mut record = set_journal(&f, &f.chain, was_installed);
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let before = std::fs::read(store.dir.join("standalone-adoption.json")).unwrap();
    let original_budget = budget();
    std::thread::scope(|scope| {
        let finish = scope.spawn(|| {
            f.handle.finish_standalone_reconciliation(
                &store,
                &mut record,
                &f.chain,
                &original_budget,
            )
        });
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        f.observe_terminal(f.daemon.sign_key()); // no head or journal change
        release.send(()).unwrap();
        let result = finish.join().unwrap().unwrap();
        assert_eq!(
            result.state,
            StandaloneRecoveryState::Terminal,
            "final receipt must use fresh effective evidence"
        );
    });
    assert_eq!(
        std::fs::read(store.dir.join("standalone-adoption.json")).unwrap(),
        before,
        "no fresh token-only Installed transition"
    );
}
#[test]
fn standalone_final_receipt_refuses_terminal_evidence_observed_after_install() {
    final_receipt_observes_terminal(false);
}
#[test]
fn standalone_already_installed_result_observes_fresh_terminal_evidence() {
    final_receipt_observes_terminal(true);
}

#[test]
fn ordinary_host_same_ring_install_preserves_terminal_fences_and_refuses_forbidden_signer() {
    let (f, accepted) = other_terminal_standalone();
    f.handle.d.ring.join(&tokens(&accepted), None).unwrap();
    let owner = f.handle.d.ring.push_link().unwrap().0;
    assert_eq!(owner.chain().head(), accepted.head());
    assert!(owner
        .chain()
        .authorized_member(&f.desktop.sign_key())
        .is_none());
    let clean = RosterChain::from_tokens(&tokens(&accepted)).unwrap();
    let forbidden = remove_fixture(&clean, &f.desktop, f.other.sign_key());
    let file = f.handle.d.cfg.paths.ring_dir.join("roster.json");
    let before = std::fs::read(&file).unwrap();
    assert!(f.handle.d.ring.join(&tokens(&forbidden), None).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), before);
}
#[test]
fn accepted_install_connector_admission_is_bounded_and_releases_owner_locks() {
    let f = Fixture::new(true);
    let record = set_journal(&f, &f.chain, false);
    let owner = f.handle.d.ring.push_link().unwrap().0;
    let until = Instant::now() + Duration::from_secs(5);
    let held = loop {
        assert!(Instant::now() < until);
        if let Some(guard) = owner.try_accepted_install() {
            break guard;
        }
        std::thread::yield_now();
    };
    let short = xshell_core::local_ipc::RequestBudget::for_operation(
        Instant::now() + Duration::from_millis(30),
        Arc::new(AtomicBool::new(false)),
    );
    let file = f.handle.d.cfg.paths.ring_dir.join("roster.json");
    let before = std::fs::read(&file).unwrap();
    let error = f
        .handle
        .d
        .ring
        .join_accepted_standalone(&record.accepted, &record.intent.operation, &short)
        .unwrap_err();
    assert!(error.contains("deadline"), "{error}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
    let query_budget = xshell_core::local_ipc::RequestBudget::for_operation(
        Instant::now() + Duration::from_millis(30),
        Arc::new(AtomicBool::new(false)),
    );
    let query_error = f
        .handle
        .d
        .ring
        .with_effective_chain_until(&query_budget, |_| -> Result<(), String> {
            panic!("held Connector must not admit query closure")
        })
        .unwrap_err();
    assert!(query_error.contains("deadline"));
    drop(held);
    f.handle
        .d
        .ring
        .join_accepted_standalone(&record.accepted, &record.intent.operation, &budget())
        .unwrap();
}

#[test]
fn controlled_attempt_transition_delivers_fresh_signed_evidence_and_reaches_export_assertions() {
    let f = Fixture::new(false);
    let owner = f.handle.d.ring.push_link().unwrap().0;
    let token = f.terminal_token(f.other.sign_key());
    owner
        .test_with_next_attempt(|| {
            owner.test_observe_activity(
                &ChainPin {
                    version: f.chain.head().version(),
                    hash: f.chain.head().hash(),
                },
                f.daemon.sign_key(),
                &token,
                || {},
            )
        })
        .unwrap();
    let effective = owner.chain();
    assert!(effective.authorized_member(&f.other.sign_key()).is_none());
    let forbidden = remove_fixture(&f.chain, &f.other, f.desktop.sign_key());
    assert!(effective.extended(forbidden.versions()).is_err());
    assert!(f
        .client
        .request(CompanionRequest::MembershipSnapshot)
        .unwrap_err()
        .contains("terminal evidence"));
}

#[test]
fn final_receipt_file_wait_linearizes_before_later_queued_terminal_observation() {
    use std::sync::mpsc;
    let f = Fixture::new(true);
    let mut record = set_journal(&f, &f.chain, false);
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let (guarded, at_guard) = mpsc::channel();
    let once = AtomicBool::new(false);
    f.handle.d.ring.set_effective_query_hook(Box::new(move || {
        if !once.swap(true, Ordering::SeqCst) {
            guarded.send(()).unwrap();
        }
    }));
    let file = store.lock().unwrap();
    let owner = f.handle.d.ring.push_link().unwrap().0;
    let token = f.terminal_token(f.daemon.sign_key());
    let original_budget = budget();
    std::thread::scope(|scope| {
        let finish = scope.spawn(|| {
            f.handle.finish_standalone_reconciliation(
                &store,
                &mut record,
                &f.chain,
                &original_budget,
            )
        });
        at_guard.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            owner.try_chain().is_none(),
            "receipt retains the effective owner guard across its file wait"
        );
        let (queued, requested) = mpsc::channel();
        let observed_fixture = &f;
        let observe = scope.spawn(move || {
            queued.send(()).unwrap();
            owner.test_with_stable_attempt(|| {
                owner.test_observe_activity(
                    &ChainPin {
                        version: observed_fixture.chain.head().version(),
                        hash: observed_fixture.chain.head().hash(),
                    },
                    observed_fixture.daemon.sign_key(),
                    &token,
                    || {},
                )
            })
        });
        requested.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(file);
        let result = finish.join().unwrap().unwrap();
        assert_eq!(
            result.state,
            StandaloneRecoveryState::Installed,
            "later queued evidence was not yet effective at the guarded receipt point"
        );
        observe.join().unwrap().unwrap();
    });
    assert!(
        load_journal(&store).unwrap().unwrap().installed,
        "retain the historical durable receipt"
    );
    assert_eq!(
        f.handle
            .standalone_adoption_status()
            .unwrap()
            .unwrap()
            .state,
        StandaloneRecoveryState::Terminal,
        "queued observation is retained for the next status, not lost"
    );
    assert_eq!(
        f.handle.d.ring.push_link().unwrap().0.chain().head(),
        f.chain.head()
    );
}

fn ordered_enroll_then_expire_fixture() -> (Fixture, RosterChain, RosterChain) {
    let mut f = Fixture::new(true);
    let c_daemon = DeviceKeys::generate().unwrap();
    f.chain = enroll_fixture(
        &f.chain,
        &f.desktop,
        JoinTarget::Computer {
            desktop: identity(&f.other, "C"),
            daemon: identity(&c_daemon, "C-daemon"),
        },
        &[&f.other, &c_daemon],
        3,
    );
    f.handle.d.ring.join(&tokens(&f.chain), None).unwrap();
    let fresh = DeviceKeys::generate().unwrap();
    // B is authorized at H and legitimately signs this Enroll at H+1.
    let enrolled = enroll_fixture(
        &f.chain,
        &f.desktop,
        JoinTarget::StandaloneDaemon {
            daemon: identity(&fresh, "new-daemon"),
        },
        &[&fresh],
        4,
    );
    let mut cleanup = enrolled.head().roster().clone();
    cleanup.version += 1;
    cleanup.prev = Some(enrolled.head().hash());
    cleanup.issued_at = 2001;
    cleanup.signed_by = f.other.sign_key();
    let evidence = f.terminal_token(f.desktop.sign_key());
    let mut m = cleanup.membership().unwrap();
    let expired = m.unit_for(&f.desktop.sign_key()).unwrap().clone();
    m.enrollments.retain(|u| u.id != expired.id);
    m.retired.push(expired.id.clone());
    m.change = RosterChange::Expire {
        enrollment_ids: vec![expired.id],
        evidence,
    };
    cleanup
        .members
        .retain(|member| !expired.members.contains(&member.sign_key));
    cleanup.set_membership(&m);
    let accepted = enrolled
        .extended(&[cleanup.sign(&f.other).unwrap()])
        .unwrap()
        .0;
    assert!(accepted.authorized_member(&f.desktop.sign_key()).is_none());
    assert!(f
        .handle
        .d
        .ring
        .push_link()
        .unwrap()
        .0
        .chain()
        .authorized_member(&f.desktop.sign_key())
        .is_some());
    (f, enrolled, accepted)
}
fn assert_cleanup_adopted(f: &Fixture, accepted: &RosterChain) {
    let cached = f.handle.d.ring.trusted_chain().unwrap();
    let owner = f.handle.d.ring.push_link().unwrap().0;
    assert_eq!(cached.head(), accepted.head());
    assert_eq!(owner.chain().head(), accepted.head());
    let expired_id = f
        .chain
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit_for(&f.desktop.sign_key())
        .unwrap()
        .id
        .clone();
    assert!(accepted.terminal_enrollments().contains(&expired_id));
    assert!(cached.terminal_enrollments().contains(&expired_id));
    assert!(owner.chain().terminal_enrollments().contains(&expired_id));
    assert!(cached.authorized_member(&f.desktop.sign_key()).is_none());
    assert!(owner
        .chain()
        .authorized_member(&f.desktop.sign_key())
        .is_none());
    assert_eq!(
        Store::new(f.handle.d.cfg.paths.ring_dir.clone())
            .read_chain()
            .unwrap()
            .unwrap()
            .head(),
        accepted.head()
    );
}
#[test]
fn candidate_enroll_then_expire_is_adopted_without_retroactive_terminal_preload() {
    let (f, _, accepted) = ordered_enroll_then_expire_fixture();
    let result = f.handle.d.ring.join(&tokens(&accepted), None);
    assert!(
        result.is_ok(),
        "valid ordered cleanup must apply after settlement: {result:?}"
    );
    assert_cleanup_adopted(&f, &accepted);
}
#[test]
fn durable_suffix_enroll_then_expire_is_adopted_without_retroactive_terminal_preload() {
    let (f, enrolled, accepted) = ordered_enroll_then_expire_fixture();
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    {
        let _lock = store.lock().unwrap();
        store.write_chain(&accepted).unwrap();
    }
    let result = f.handle.d.ring.join(&tokens(&enrolled), None);
    assert!(
        result.is_ok(),
        "valid ordered durable cleanup must apply coherently: {result:?}"
    );
    assert_cleanup_adopted(&f, &accepted);
}
fn cancellation_during_roster_write_enforces_trusted_removal(write_error: bool) {
    let armed = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let hit = Arc::new(AtomicBool::new(false));
    let (a, c, h) = (armed.clone(), cancel.clone(), hit.clone());
    let mut f = Fixture::with_hook(
        true,
        Some(crate::server::TestHook(Arc::new(move |_, point| {
            if point == crate::server::TestPoint::RingRosterTempSynced && a.load(Ordering::Acquire)
            {
                h.store(true, Ordering::Release);
                c.store(true, Ordering::Release);
                return write_error;
            }
            false
        }))),
    );
    // A genuinely unregistered standalone member may follow same-Ring Host updates;
    // no registration or admission fence is removed for this unit control.
    let c_daemon = DeviceKeys::generate().unwrap();
    f.chain = enroll_fixture(
        &f.chain,
        &f.desktop,
        JoinTarget::Computer {
            desktop: identity(&f.other, "C"),
            daemon: identity(&c_daemon, "C-daemon"),
        },
        &[&f.other, &c_daemon],
        3,
    );
    f.handle.d.ring.join(&tokens(&f.chain), None).unwrap();
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let removal = remove_fixture(&f.chain, &f.desktop, f.other.sign_key());
    let original = std::fs::read(store.dir.join("roster.json")).unwrap();
    let journal_path = store.dir.join("standalone-adoption.json");
    let original_journal = std::fs::read(&journal_path).unwrap();
    armed.store(true, Ordering::Release);
    let b = xshell_core::local_ipc::RequestBudget::for_operation(
        Instant::now() + Duration::from_secs(5),
        cancel,
    );
    let error = f
        .handle
        .d
        .ring
        .test_host_join_until(&tokens(&removal), &b)
        .unwrap_err();
    assert!(
        error.contains(if write_error {
            "could not be saved"
        } else {
            "counterpart endpoint is stopping"
        }),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&journal_path).unwrap(),
        original_journal,
        "an error must not create or change the durable Installed receipt"
    );
    assert!(
        hit.load(Ordering::Acquire),
        "must cross actual temp write+sync"
    );
    assert_eq!(
        f.handle.d.ring.trusted_chain().unwrap().head(),
        removal.head(),
        "cancel after settlement cannot abandon effective enforcement"
    );
    let owner = f.handle.d.ring.push_link().unwrap().0;
    assert!(owner
        .chain()
        .authorized_member(&f.other.sign_key())
        .is_none());
    if write_error {
        assert_eq!(
            std::fs::read(store.dir.join("roster.json")).unwrap(),
            original
        );
    } else {
        assert_eq!(store.read_chain().unwrap().unwrap().head(), removal.head());
    }
}
#[test]
fn cancelled_after_roster_sync_still_applies_durable_removal() {
    cancellation_during_roster_write_enforces_trusted_removal(false);
}
#[test]
fn cancelled_after_roster_sync_still_applies_unsaved_trusted_removal() {
    cancellation_during_roster_write_enforces_trusted_removal(true);
}

// Cryptographic units: only the verified snapshot read is substituted. The actual
// reconciliation budget, journal, accepted installation and final reporting run below.
fn standalone_recovery_replays_ordered_cleanup(was_installed: bool) {
    let (f, _, accepted) = ordered_enroll_then_expire_fixture();
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let mut original = load_journal(&store).unwrap().unwrap();
    original.accepted = tokens(&f.chain);
    original.installed = was_installed;
    if was_installed {
        // Another process already settled cleanup on disk, but the running owner
        // and historical Installed receipt still describe the older authority.
        let _lock = store.lock().unwrap();
        store.write_chain(&accepted).unwrap();
    }
    write_private(
        &store.dir.join("standalone-adoption.json"),
        &serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    let result = f
        .handle
        .reconcile_standalone_adoption_with(|record, keys, pin| {
            assert_eq!(record.accepted, original.accepted);
            assert_eq!(keys.sign_key(), f.daemon.sign_key());
            assert_eq!(f.chain.get(pin.version).unwrap().hash(), pin.hash);
            assert_eq!(
                f.handle.d.ring.push_link().unwrap().0.chain().head(),
                f.chain.head()
            );
            let _lock = store.lock().unwrap(); // no file lock across the snapshot read
            Ok(accepted.clone())
        })
        .expect("observed later cleanup must not forbid its earlier authorized signer");
    assert_eq!(result.state, StandaloneRecoveryState::Installed);
    assert_cleanup_adopted(&f, &accepted);
    let final_record = load_journal(&store).unwrap().unwrap();
    assert!(final_record.installed);
    assert_eq!(final_record.accepted, tokens(&accepted));
    assert_eq!(final_record.intent, original.intent);
}
#[test]
fn standalone_observed_recovery_replays_cleanup_after_the_authorized_enroll() {
    standalone_recovery_replays_ordered_cleanup(false);
}
#[test]
fn standalone_installed_recovery_still_adopts_newly_observed_cleanup() {
    standalone_recovery_replays_ordered_cleanup(true);
}
#[test]
fn standalone_observed_recovery_keeps_terminal_constraints_known_before_enroll() {
    let (f, _, accepted) = ordered_enroll_then_expire_fixture();
    f.observe_terminal(f.desktop.sign_key());
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let before = std::fs::read(store.dir.join("standalone-adoption.json")).unwrap();
    let error = f
        .handle
        .reconcile_standalone_adoption_with(|_, _, _| Ok(accepted))
        .unwrap_err();
    assert!(error.contains("terminal-expired signer"), "{error}");
    assert_eq!(
        std::fs::read(store.dir.join("standalone-adoption.json")).unwrap(),
        before
    );
    assert_eq!(
        f.handle.d.ring.push_link().unwrap().0.chain().head(),
        f.chain.head()
    );
}
#[test]
fn standalone_final_receipt_replays_cleanup_before_checking_durable_installation() {
    let (f, _, accepted) = ordered_enroll_then_expire_fixture();
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let mut record = load_journal(&store).unwrap().unwrap();
    record.accepted = tokens(&accepted);
    let bytes = serde_json::to_vec(&record).unwrap();
    write_private(&store.dir.join("standalone-adoption.json"), &bytes).unwrap();
    let budget = xshell_core::local_ipc::RequestBudget::for_operation(
        Instant::now() + Duration::from_secs(5),
        Arc::new(AtomicBool::new(false)),
    );
    let error = f
        .handle
        .finish_standalone_reconciliation(&store, &mut record, &accepted, &budget)
        .unwrap_err();
    assert!(error.contains("not durably installed"), "{error}");
    assert_eq!(
        std::fs::read(store.dir.join("standalone-adoption.json")).unwrap(),
        bytes
    );
    assert_eq!(
        f.handle.d.ring.push_link().unwrap().0.chain().head(),
        f.chain.head()
    );
}

/// Actual production in-process driver install continuation; network/word/consent steps
/// are not exercised here. Signed effective evidence uses the existing observer seam.
#[test]
fn in_process_standalone_driver_finalization_refuses_terminal_after_install() {
    use std::sync::{mpsc, Mutex};
    let (at, reached) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let f = Fixture::with_hook(
        true,
        Some(crate::server::TestHook(Arc::new(move |_, point| {
            if point == crate::server::TestPoint::StandaloneInstalledBeforeJournal {
                at.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            false
        }))),
    );
    let record = set_journal(&f, &f.chain, false);
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let before = std::fs::read(store.dir.join("standalone-adoption.json")).unwrap();
    let roster = std::fs::read(store.dir.join("roster.json")).unwrap();
    let driver = StandaloneDriver {
        store,
        owner: StandaloneOwner::Runtime(&f.handle.d.ring),
        exiting: Some(&f.handle.d.exiting),
    };
    std::thread::scope(|scope| {
        let install =
            scope.spawn(|| driver.install(&record.accepted, &record.intent.operation, &budget()));
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        f.observe_terminal(f.daemon.sign_key());
        release.send(()).unwrap();
        assert!(install.join().unwrap().unwrap_err().contains("terminal"));
    });
    assert_eq!(
        std::fs::read(driver.store.dir.join("standalone-adoption.json")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(driver.store.dir.join("roster.json")).unwrap(),
        roster
    );
}

#[test]
fn admitted_ring_companion_mutation_retains_store_through_shutdown_and_late_request_refuses() {
    use crate::server::ring::ownership::{StoreOwnership, StoreOwnershipLease};
    use std::sync::{mpsc, Mutex};
    let (at, reached) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let f = Fixture::with_hook(
        true,
        Some(crate::server::TestHook(Arc::new(move |_, point| {
            if point == crate::server::TestPoint::RingCompanionAdmitted {
                at.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            false
        }))),
    );
    let store = Store::new(f.handle.d.cfg.paths.ring_dir.clone());
    let desktop_dir = f._home.path().join("completion-desktop");
    crate::paths::ensure_private_dir(&desktop_dir).unwrap();
    let retained_name = &f.chain.head().member(&f.daemon.sign_key()).unwrap().name;
    let profile = CompanionProfile::for_stores(
        desktop_dir.to_str().unwrap().into(),
        store.dir.to_str().unwrap().into(),
        identity(&f.other, "completion desktop"),
        identity(&f.daemon, retained_name),
        f.client.endpoint().to_str().unwrap().into(),
    );
    let proof = Signature::from_bytes(f.other.sign(&registration_message(&profile)).unwrap());
    f.client
        .request(CompanionRequest::Register { profile, proof })
        .unwrap();
    let intent = AdoptionIntent {
        operation: b64::encode(&[76; 32]),
        expires_at: now() + 60,
        target: JoinTarget::Computer {
            desktop: identity(&f.other, "completion desktop"),
            daemon: identity(&f.daemon, retained_name),
        },
    };
    f.client
        .request(CompanionRequest::CompletionStart {
            intent: intent.clone(),
        })
        .unwrap();
    let mut proposal = f.chain.head().roster().clone();
    proposal.version += 1;
    proposal.prev = Some(f.chain.head().hash());
    proposal.issued_at = now();
    proposal.members.push(Member::new(
        "completion desktop",
        xshell_protocol::ring::Role::Desktop,
        f.other.sign_key(),
        f.other.noise_key(),
        proposal.issued_at,
    ));
    let mut membership = proposal.membership().unwrap();
    let singleton = membership
        .unit_for(&f.daemon.sign_key())
        .unwrap()
        .id
        .clone();
    membership.enrollments.retain(|unit| unit.id != singleton);
    membership.retired.push(singleton.clone());
    membership.enrollments.push(Enrollment {
        id: intent.operation.clone(),
        kind: EnrollmentKind::Computer,
        members: vec![f.other.sign_key(), f.daemon.sign_key()],
        enrolled_at: proposal.issued_at,
    });
    membership.change = RosterChange::CompleteComputer {
        replaces: vec![singleton],
        enrollment_id: intent.operation.clone(),
        proofs: vec![],
    };
    proposal.set_membership(&membership);
    let proposal = CompanionProposal {
        operation: intent.operation,
        predecessor: tokens(&f.chain),
        proposal,
        transcript: ConsentTranscript::Local,
    };
    let request = CompanionAdminRequest::Propose {
        proposal: Box::new(proposal.clone()),
    };
    let before = std::fs::read(store.dir.join("completion.json")).unwrap();
    std::thread::scope(|scope| {
        let admin = scope.spawn(|| {
            crate::server::completion::admin(
                &f.handle.d,
                Some(f.desktop.sign_key()),
                request.clone(),
            )
        });
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        f.handle.d.exit(crate::server::ExitReason::Shutdown);
        assert!(matches!(
            StoreOwnershipLease::acquire(&store.dir).unwrap(),
            StoreOwnership::Busy
        ));
        assert!(crate::server::completion::admin(
            &f.handle.d,
            Some(f.desktop.sign_key()),
            request.clone()
        )
        .is_err());
        assert_eq!(
            std::fs::read(store.dir.join("completion.json")).unwrap(),
            before
        );
        release.send(()).unwrap();
        admin.join().unwrap().unwrap();
    });
    let pending: CompletionPending =
        serde_json::from_slice(&std::fs::read(store.dir.join("completion.json")).unwrap()).unwrap();
    assert_eq!(pending.proposal, Some(proposal));
    assert!(matches!(
        StoreOwnershipLease::acquire(&store.dir).unwrap(),
        StoreOwnership::Acquired(_)
    ));
}

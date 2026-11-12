use xshell_protocol::ring::companion::{CompanionPolicy, CompanionProfile};
use xshell_protocol::ring::pairing::adoption::{AdoptionIntent, JoinTarget, PublicIdentity};
use xshell_protocol::ring::{b64, DeviceKeys};

fn identity(keys: &DeviceKeys) -> PublicIdentity {
    PublicIdentity {
        name: "computer".into(),
        sign_key: keys.sign_key(),
        noise_key: keys.noise_key(),
    }
}

fn fixture_profile(desktop: &DeviceKeys, daemon: &DeviceKeys) -> CompanionProfile {
    let base = std::env::temp_dir();
    CompanionProfile::for_stores(
        base.join("fixture-desktop").to_str().unwrap().into(),
        base.join("fixture-daemon").to_str().unwrap().into(),
        identity(desktop),
        identity(daemon),
        base.join("fixture-daemon/companion.sock")
            .to_str()
            .unwrap()
            .into(),
    )
}

#[test]
fn companion_possession_is_operation_scoped_and_never_consent() {
    let desktop = DeviceKeys::generate().unwrap();
    let daemon = DeviceKeys::generate().unwrap();
    let profile = fixture_profile(&desktop, &daemon);
    let intent = AdoptionIntent {
        operation: b64::encode(&[2; 32]),
        expires_at: 1600,
        target: JoinTarget::Computer {
            desktop: profile.desktop.clone(),
            daemon: profile.daemon.clone(),
        },
    };
    let mut policy = CompanionPolicy::new(profile).unwrap();
    policy.start(intent.clone(), 1000).unwrap();
    assert!(policy
        .possession(&daemon, &b64::encode(&[3; 32]), [4; 32], 1000)
        .is_err());
    policy
        .possession(&daemon, &intent.operation, [4; 32], 1000)
        .unwrap();
    assert!(policy
        .possession(&daemon, &intent.operation, [5; 32], 1000)
        .is_err());
    policy.cancel(&intent.operation).unwrap();
    assert!(policy
        .possession(&daemon, &intent.operation, [4; 32], 1000)
        .is_err());
    assert!(
        policy.start(intent, 1000).is_err(),
        "cancelled operation cannot be replayed"
    );
}

// Synthetic local policy unit fixture, not an enrolled Ring/controller success fixture.
fn prepared() -> (
    CompanionPolicy,
    DeviceKeys,
    DeviceKeys,
    AdoptionIntent,
    xshell_protocol::ring::Roster,
) {
    use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
    use xshell_protocol::ring::membership::{
        Enrollment, EnrollmentKind, Membership, RingPolicy, RosterChange,
    };
    use xshell_protocol::ring::{Member, RingId, Role, Roster};
    let desktop = DeviceKeys::generate().unwrap();
    let daemon = DeviceKeys::generate().unwrap();
    let profile = fixture_profile(&desktop, &daemon);
    let intent = AdoptionIntent {
        operation: b64::encode(&[12; 32]),
        expires_at: 1600,
        target: profile.target(),
    };
    let mut policy = CompanionPolicy::new(profile).unwrap();
    policy.start(intent.clone(), 1000).unwrap();
    let mut roster = Roster {
        v: 2,
        ring_id: RingId::derive(&desktop.sign_key()),
        version: 1,
        prev: None,
        relay_url: "ws://127.0.0.1:7777".into(),
        signed_by: desktop.sign_key(),
        issued_at: 1000,
        members: vec![
            Member::new(
                "computer",
                Role::Desktop,
                desktop.sign_key(),
                desktop.noise_key(),
                1000,
            ),
            Member::new(
                "computer",
                Role::Daemon,
                daemon.sign_key(),
                daemon.noise_key(),
                1000,
            ),
        ],
        extra: Default::default(),
    };
    roster.set_membership(&Membership {
        enrollments: vec![Enrollment {
            id: b64::encode(&[12; 32]),
            kind: EnrollmentKind::Computer,
            members: vec![desktop.sign_key(), daemon.sign_key()],
            enrolled_at: 1000,
        }],
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: Some(2592000),
            activity_key: desktop.sign_key(),
        },
        change: RosterChange::Genesis { proofs: vec![] },
    });
    policy
        .prepare(
            CompanionProposal {
                operation: intent.operation.clone(),
                predecessor: vec![],
                proposal: roster.clone(),
                transcript: ConsentTranscript::Local,
            },
            None,
            1000,
        )
        .unwrap();
    (policy, desktop, daemon, intent, roster)
}

#[test]
fn companion_consent_requires_exact_desktop_proof_and_cannot_be_replayed() {
    use xshell_protocol::ring::companion::CompanionPhase;
    use xshell_protocol::ring::membership::{consent_message, local_consent_context, ConsentProof};
    use xshell_protocol::ring::{Signature, Signer};
    let (mut policy, desktop, daemon, intent, roster) = prepared();
    let context = local_consent_context(&roster).unwrap();
    let message = consent_message(&roster, intent.expires_at, &context).unwrap();
    let mut proof = ConsentProof {
        key: daemon.sign_key(),
        expires_at: intent.expires_at,
        context: context.clone(),
        signature: Signature::from_bytes(daemon.sign(&message).unwrap()),
    };
    assert!(policy
        .consent(&daemon, &intent.operation, &proof, 1000)
        .is_err());
    proof.key = desktop.sign_key();
    proof.signature = Signature::from_bytes(desktop.sign(b"arbitrary bytes").unwrap());
    assert!(policy
        .consent(&daemon, &intent.operation, &proof, 1000)
        .is_err());
    proof.signature = Signature::from_bytes(desktop.sign(&message).unwrap());
    let mut changed = proof.clone();
    changed.context.hash = b64::encode(&[99; 32]);
    assert!(policy
        .consent(&daemon, &intent.operation, &changed, 1000)
        .is_err());
    assert!(policy
        .consent(&daemon, &b64::encode(&[99; 32]), &proof, 1000)
        .is_err());
    assert!(policy
        .consent(&daemon, &intent.operation, &proof, 1600)
        .is_err());
    let consent = policy
        .consent(&daemon, &intent.operation, &proof, 1000)
        .unwrap();
    assert!(xshell_protocol::ring::verify(
        &daemon.sign_key(),
        &message,
        &consent.signature
    ));
    assert!(policy
        .consent(&daemon, &intent.operation, &proof, 1000)
        .is_err());
    policy.cancel(&intent.operation).unwrap();
    assert_eq!(
        policy.phase(&intent.operation).unwrap(),
        CompanionPhase::ConsentMayHaveLeft
    );
}

#[test]
fn mobile_scope_and_host_confirmation_bits_are_not_companion_authority() {
    use xshell_protocol::ring::companion::CompanionRequest;
    for json in [
        r#"{"type":"sign","bytes":"eA"}"#,
        r#"{"type":"confirm","operation":"x","confirm":true}"#,
        r#"{"type":"start","scope":"mobile","confirm":true,"intent":{}}"#,
    ] {
        assert!(serde_json::from_str::<CompanionRequest>(json).is_err());
    }
}

#[test]
fn unsigned_local_preparation_never_authorizes_a_roster_chain() {
    let (_, desktop, _, _, roster) = prepared();
    xshell_protocol::ring::membership::verify_local_proposal(None, &roster).unwrap();
    // A real creator signature without the two consent proofs is still not membership.
    let signed = roster.sign(&desktop).unwrap();
    assert!(xshell_protocol::ring::RosterChain::from_chain(vec![signed]).is_err());
}

#[test]
fn uncertain_consent_survives_expiry_and_refuses_new_start() {
    use xshell_protocol::ring::companion::CompanionPhase;
    use xshell_protocol::ring::membership::{consent_message, local_consent_context, ConsentProof};
    use xshell_protocol::ring::{Signature, Signer};
    let (mut policy, desktop, daemon, intent, roster) = prepared();
    let context = local_consent_context(&roster).unwrap();
    let message = consent_message(&roster, intent.expires_at, &context).unwrap();
    let proof = ConsentProof {
        key: desktop.sign_key(),
        expires_at: intent.expires_at,
        context,
        signature: Signature::from_bytes(desktop.sign(&message).unwrap()),
    };
    policy
        .consent(&daemon, &intent.operation, &proof, 1000)
        .unwrap();
    let retained = serde_json::to_value(&policy).unwrap();
    let next = AdoptionIntent {
        operation: b64::encode(&[77; 32]),
        expires_at: 2201,
        target: intent.target.clone(),
    };
    assert!(
        policy.start(next, 1601).is_err(),
        "expiry is not proof of nonpublication"
    );
    assert_eq!(
        policy.phase(&intent.operation).unwrap(),
        CompanionPhase::ConsentMayHaveLeft
    );
    assert_eq!(serde_json::to_value(&policy).unwrap(), retained);
}

#[test]
fn cancelled_local_proposal_proof_cannot_be_reused_under_a_new_operation() {
    use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
    use xshell_protocol::ring::membership::{consent_message, local_consent_context, ConsentProof};
    use xshell_protocol::ring::{Signature, Signer};
    let (mut policy, desktop, daemon, old, roster) = prepared();
    let context = local_consent_context(&roster).unwrap();
    let proof = ConsentProof {
        key: desktop.sign_key(),
        expires_at: old.expires_at,
        context: context.clone(),
        signature: Signature::from_bytes(
            desktop
                .sign(&consent_message(&roster, old.expires_at, &context).unwrap())
                .unwrap(),
        ),
    };
    policy.cancel(&old.operation).unwrap();
    let new = AdoptionIntent {
        operation: b64::encode(&[88; 32]),
        expires_at: old.expires_at,
        target: old.target,
    };
    policy.start(new.clone(), 1000).unwrap();
    if policy
        .prepare(
            CompanionProposal {
                operation: new.operation.clone(),
                predecessor: vec![],
                proposal: roster,
                transcript: ConsentTranscript::Local,
            },
            None,
            1000,
        )
        .is_ok()
    {
        assert!(
            policy
                .consent(&daemon, &new.operation, &proof, 1000)
                .is_err(),
            "a new operation must not reuse the old local proof statement"
        );
    }
}

// Adversarial signed-history fixtures only; runtime positives use public enable/Upgrade.
fn retained_singleton_chain(
    signer: &DeviceKeys,
    registered: &DeviceKeys,
    noise: xshell_protocol::ring::NoiseKey,
    role: xshell_protocol::ring::Role,
) -> xshell_protocol::ring::RosterChain {
    use xshell_protocol::ring::membership::{
        Enrollment, EnrollmentKind, Membership, RingPolicy, RosterChange,
    };
    use xshell_protocol::ring::{Member, Role, RosterChain, SignedRoster};
    let g = SignedRoster::genesis(
        signer,
        signer.noise_key(),
        "retained",
        "ws://127.0.0.1:8787",
        100,
    )
    .unwrap();
    let mut roster = g.roster().clone();
    if signer.sign_key() == registered.sign_key() {
        roster.members[0].noise_key = noise;
        roster.members[0].role = role;
    } else {
        roster.members.push(Member::new(
            "candidate",
            role,
            registered.sign_key(),
            noise,
            100,
        ));
    }
    let g = roster.sign(signer).unwrap();
    let mut r = g.roster().clone();
    r.v = 2;
    r.version = 2;
    r.prev = Some(g.hash());
    r.issued_at = 101;
    let enrollments = r
        .members
        .iter()
        .enumerate()
        .map(|(i, m)| Enrollment {
            id: b64::encode(&[i as u8 + 1; 32]),
            kind: match m.role {
                Role::Desktop => EnrollmentKind::LegacyDesktop,
                Role::Daemon => EnrollmentKind::Daemon,
                Role::Mobile => EnrollmentKind::Mobile,
            },
            members: vec![m.sign_key],
            enrolled_at: 101,
        })
        .collect();
    r.set_membership(&Membership {
        enrollments,
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: None,
            activity_key: DeviceKeys::generate().unwrap().sign_key(),
        },
        change: RosterChange::Upgrade { proofs: vec![] },
    });
    RosterChain::from_chain(vec![g, r.sign(signer).unwrap()]).unwrap()
}
#[test]
fn unpaired_daemon_preparation_requires_exact_registered_desktop_key_noise_and_role() {
    use xshell_protocol::ring::Role;
    let desktop = DeviceKeys::generate().unwrap();
    let daemon = DeviceKeys::generate().unwrap();
    let other = DeviceKeys::generate().unwrap();
    let policy = CompanionPolicy::new(fixture_profile(&desktop, &daemon)).unwrap();
    let good = retained_singleton_chain(&desktop, &desktop, desktop.noise_key(), Role::Desktop);
    assert!(policy
        .completion_trust(Some(&good), None)
        .unwrap()
        .is_some());
    let wrong_key = retained_singleton_chain(&other, &other, other.noise_key(), Role::Desktop);
    let wrong_noise =
        retained_singleton_chain(&desktop, &desktop, other.noise_key(), Role::Desktop);
    let wrong_role = retained_singleton_chain(&other, &desktop, desktop.noise_key(), Role::Mobile);
    for bad in [&wrong_key, &wrong_noise, &wrong_role] {
        assert!(policy.completion_trust(Some(bad), None).is_err());
    }
    assert!(
        policy
            .completion_trust(Some(&good), Some(&wrong_key))
            .is_err(),
        "registered local trust cannot replace a foreign retained Daemon Ring"
    );
}

#[test]
fn retained_daemon_preparation_cannot_resurrect_its_previously_removed_identity() {
    use xshell_protocol::ring::membership::RosterChange;
    use xshell_protocol::ring::Role;
    let desktop = DeviceKeys::generate().unwrap();
    let daemon = DeviceKeys::generate().unwrap();
    let policy = CompanionPolicy::new(fixture_profile(&desktop, &daemon)).unwrap();
    let old = retained_singleton_chain(&desktop, &daemon, daemon.noise_key(), Role::Daemon);
    let mut removed = old.head().roster().clone();
    removed.version += 1;
    removed.prev = Some(old.head().hash());
    let mut m = removed.membership().unwrap();
    let unit = m.unit_for(&daemon.sign_key()).unwrap().clone();
    m.enrollments.retain(|u| u.id != unit.id);
    m.retired.push(unit.id.clone());
    m.change = RosterChange::Remove {
        enrollment_id: unit.id,
    };
    removed
        .members
        .retain(|member| member.sign_key != daemon.sign_key());
    removed.set_membership(&m);
    let removed = old.extended(&[removed.sign(&desktop).unwrap()]).unwrap().0;
    assert!(policy.completion_trust(Some(&removed), None).is_err());
    assert!(
        policy
            .completion_trust(Some(&removed), Some(&removed))
            .is_err(),
        "matching retained removal heads cannot restore the Daemon"
    );
}

#[test]
fn protected_desktop_only_completion_prepares_without_installing_supplied_trust() {
    use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
    use xshell_protocol::ring::membership::{Enrollment, EnrollmentKind, RosterChange};
    use xshell_protocol::ring::{Member, Role};
    let desktop = DeviceKeys::generate().unwrap();
    let daemon = DeviceKeys::generate().unwrap();
    let chain = retained_singleton_chain(&desktop, &desktop, desktop.noise_key(), Role::Desktop);
    let mut profile = fixture_profile(&desktop, &daemon);
    profile.desktop.name = chain
        .head()
        .member(&desktop.sign_key())
        .unwrap()
        .name
        .clone();
    let intent = AdoptionIntent {
        operation: b64::encode(&[63; 32]),
        expires_at: 600,
        target: profile.target(),
    };
    let mut policy = CompanionPolicy::new(profile).unwrap();
    policy.start(intent.clone(), 101).unwrap();
    let mut proposal = chain.head().roster().clone();
    proposal.version += 1;
    proposal.prev = Some(chain.head().hash());
    proposal.issued_at = 102;
    proposal.members.push(Member::new(
        "computer",
        Role::Daemon,
        daemon.sign_key(),
        daemon.noise_key(),
        102,
    ));
    let mut membership = proposal.membership().unwrap();
    let old = membership.unit_for(&desktop.sign_key()).unwrap().id.clone();
    membership.enrollments.clear();
    membership.retired.push(old.clone());
    membership.enrollments.push(Enrollment {
        id: intent.operation.clone(),
        kind: EnrollmentKind::Computer,
        members: vec![desktop.sign_key(), daemon.sign_key()],
        enrolled_at: 102,
    });
    membership.change = RosterChange::CompleteComputer {
        replaces: vec![old],
        enrollment_id: intent.operation.clone(),
        proofs: vec![],
    };
    proposal.set_membership(&membership);
    policy.prepare(CompanionProposal { operation: intent.operation.clone(), predecessor: chain.versions().iter().map(|r| r.token().into()).collect(), proposal, transcript: ConsentTranscript::Local }, None, 102).expect("protected registered Desktop-only predecessor is preparation trust, not Daemon membership");
    policy.check_prepared(&intent.operation, None, 102).unwrap();
    assert_eq!(
        policy.phase(&intent.operation).unwrap(),
        xshell_protocol::ring::companion::CompanionPhase::Prepared
    );
}

#[test]
fn matching_retained_completion_trust_requires_current_daemon_identity() {
    use xshell_protocol::ring::Role;
    let desktop = DeviceKeys::generate().unwrap();
    let daemon = DeviceKeys::generate().unwrap();
    let other = DeviceKeys::generate().unwrap();
    let policy = CompanionPolicy::new(fixture_profile(&desktop, &daemon)).unwrap();
    let good = retained_singleton_chain(&desktop, &daemon, daemon.noise_key(), Role::Daemon);
    assert!(policy.completion_trust(Some(&good), Some(&good)).is_ok());
    let wrong_noise = retained_singleton_chain(&desktop, &daemon, other.noise_key(), Role::Daemon);
    let wrong_role = retained_singleton_chain(&desktop, &daemon, daemon.noise_key(), Role::Mobile);
    for chain in [&wrong_noise, &wrong_role] {
        assert!(policy.completion_trust(Some(chain), Some(chain)).is_err());
    }
}

#[test]
fn fresh_create_preparation_still_refuses_unrelated_enrollments() {
    use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
    use xshell_protocol::ring::membership::{Enrollment, EnrollmentKind};
    use xshell_protocol::ring::{Member, Role};
    let (_, desktop, daemon, intent, mut proposal) = prepared();
    let mobile = DeviceKeys::generate().unwrap();
    proposal.members.push(Member::new(
        "unrelated",
        Role::Mobile,
        mobile.sign_key(),
        mobile.noise_key(),
        1000,
    ));
    let mut membership = proposal.membership().unwrap();
    membership.enrollments.push(Enrollment {
        id: b64::encode(&[64; 32]),
        kind: EnrollmentKind::Mobile,
        members: vec![mobile.sign_key()],
        enrolled_at: 1000,
    });
    proposal.set_membership(&membership);
    xshell_protocol::ring::membership::verify_local_proposal(None, &proposal).unwrap();
    let mut policy = CompanionPolicy::new(fixture_profile(&desktop, &daemon)).unwrap();
    policy.start(intent.clone(), 1000).unwrap();
    let error = policy
        .prepare(
            CompanionProposal {
                operation: intent.operation,
                predecessor: vec![],
                proposal,
                transcript: ConsentTranscript::Local,
            },
            None,
            1000,
        )
        .expect_err("fresh Create must remain exactly its local Computer");
    assert!(error.contains("counterpart mismatch"), "{error}");
}

// Cryptographic fixture only: no Relay acceptance or activity-wire delivery claim.
use super::protocol::ring::{
    activity::{policy_hash, ActivityEvidence, UnitActivity},
    b64,
    membership::{
        consent_message, pairing_consent_context, ConsentProof, Enrollment, EnrollmentKind,
        Membership, RingPolicy, RosterChange,
    },
    DeviceKeys, Member, Role, RosterChain, Signature, SignedRoster, Signer,
};

pub struct RecoveryChronology {
    pub reader: std::sync::Arc<DeviceKeys>,
    pub b: DeviceKeys,
    pub legacy: RosterChain,
    pub known: RosterChain,
    pub accepted: RosterChain,
    pub evidence: String,
    pub terminal_id: String,
}

pub fn recovery_chronology(url: &str) -> RecoveryChronology {
    let b = DeviceKeys::generate().unwrap();
    let reader = DeviceKeys::generate().unwrap();
    let source = DeviceKeys::generate().unwrap();
    let genesis = SignedRoster::genesis(&b, b.noise_key(), "B", url, 1000).unwrap();
    let mut c = genesis.roster().clone();
    c.version += 1;
    c.prev = Some(genesis.hash());
    c.issued_at += 1;
    c.members.push(Member::new(
        "C",
        Role::Desktop,
        reader.sign_key(),
        reader.noise_key(),
        1001,
    ));
    let legacy = RosterChain::from_chain(vec![genesis, c.sign(&b).unwrap()]).unwrap();
    let terminal_id = b64::encode(&[1; 32]);
    let mut h = legacy.head().roster().clone();
    h.v = 2;
    h.version += 1;
    h.prev = Some(legacy.head().hash());
    h.issued_at += 1;
    h.set_membership(&Membership {
        enrollments: vec![
            Enrollment {
                id: terminal_id.clone(),
                kind: EnrollmentKind::LegacyDesktop,
                members: vec![b.sign_key()],
                enrolled_at: 1002,
            },
            Enrollment {
                id: b64::encode(&[2; 32]),
                kind: EnrollmentKind::LegacyDesktop,
                members: vec![reader.sign_key()],
                enrolled_at: 1002,
            },
        ],
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: Some(30),
            activity_key: source.sign_key(),
        },
        change: RosterChange::Upgrade { proofs: vec![] },
    });
    let known = legacy.extended(&[h.sign(&b).unwrap()]).unwrap().0;
    let fresh = DeviceKeys::generate().unwrap();
    let mut enroll = known.head().roster().clone();
    enroll.version += 1;
    enroll.prev = Some(known.head().hash());
    enroll.issued_at += 1;
    enroll.members.push(Member::new(
        "new-daemon",
        Role::Daemon,
        fresh.sign_key(),
        fresh.noise_key(),
        enroll.issued_at,
    ));
    let mut m = enroll.membership().unwrap();
    let id = b64::encode(&[3; 32]);
    m.enrollments.push(Enrollment {
        id: id.clone(),
        kind: EnrollmentKind::Daemon,
        members: vec![fresh.sign_key()],
        enrolled_at: enroll.issued_at,
    });
    m.change = RosterChange::Enroll {
        enrollment_id: id,
        proofs: vec![],
    };
    enroll.set_membership(&m);
    let context = pairing_consent_context(&enroll, &[42; 32]).unwrap();
    let message = consent_message(&enroll, 1600, &context).unwrap();
    if let RosterChange::Enroll { proofs, .. } = &mut m.change {
        proofs.push(ConsentProof {
            key: fresh.sign_key(),
            expires_at: 1600,
            context,
            signature: Signature::from_bytes(fresh.sign(&message).unwrap()),
        });
    }
    enroll.set_membership(&m);
    let enrolled = known.extended(&[enroll.sign(&b).unwrap()]).unwrap().0;
    let evidence = ActivityEvidence {
        ring_id: known.ring_id().clone(),
        relay_url: url.into(),
        policy_head_hash: known.head().hash(),
        policy_hash: policy_hash(&m.policy),
        observed_at: 2000,
        units: vec![UnitActivity {
            enrollment_id: terminal_id.clone(),
            member_keys: vec![b.sign_key()],
            online: false,
            last_active: 1002,
            terminal_expired: true,
        }],
    }
    .sign(&source)
    .unwrap();
    let mut cleanup = enrolled.head().roster().clone();
    cleanup.version += 1;
    cleanup.prev = Some(enrolled.head().hash());
    cleanup.issued_at = 2001;
    cleanup.signed_by = reader.sign_key();
    m.enrollments.retain(|u| u.id != terminal_id);
    m.retired.push(terminal_id.clone());
    m.change = RosterChange::Expire {
        enrollment_ids: vec![terminal_id.clone()],
        evidence: evidence.clone(),
    };
    cleanup
        .members
        .retain(|member| member.sign_key != b.sign_key());
    cleanup.set_membership(&m);
    let accepted = enrolled
        .extended(&[cleanup.sign(&reader).unwrap()])
        .unwrap()
        .0;
    assert!(accepted.terminal_enrollments().contains(&terminal_id));
    assert!(known.authorized_member(&b.sign_key()).is_some());
    assert!(accepted.authorized_member(&b.sign_key()).is_none());
    RecoveryChronology {
        reader: std::sync::Arc::new(reader),
        b,
        legacy,
        known,
        accepted,
        evidence,
        terminal_id,
    }
}

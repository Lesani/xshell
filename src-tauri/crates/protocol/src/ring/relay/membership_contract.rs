//! Public Rust-client contracts for the active v2 Worker. Unlike the legacy in-process
//! TestRelay scenarios, these require activity signing and the restricted cleanup lane.
use super::contract::{keys, now, Recorder, RelayTarget, WAIT};
use super::{DepartureClient, DepartureStatus, RingClient, RingClientConfig};
use crate::ring::{
    activity::ActivityEvidence,
    b64,
    membership::{
        consent_message, local_consent_context, pairing_consent_context, ConsentProof, Enrollment,
        EnrollmentKind, Membership, RingPolicy, RosterChange,
    },
    DeviceKeys, Member, RingId, Role, Roster, RosterChain, Signature, SignedRoster, Signer,
};
use std::sync::Arc;

pub type MembershipScenario = (&'static str, fn(&RelayTarget));

pub const SCENARIOS: &[MembershipScenario] = &[
    ("membership_v2_loopback_genesis", loopback_genesis),
    (
        "membership_v2_legacy_upgrade_and_policy",
        legacy_upgrade_and_policy,
    ),
    (
        "membership_v2_completion_departure_and_reused_keys",
        completion_departure_and_reused_keys,
    ),
    (
        "membership_v2_terminal_ring_departure",
        terminal_ring_departure,
    ),
    (
        "membership_v2_terminal_evidence_survives_policy_increase",
        terminal_evidence_survives_policy_increase,
    ),
    (
        "membership_v2_departure_resigns_after_concurrent_commit",
        departure_resigns_after_concurrent_commit,
    ),
];
fn id(n: u8) -> String {
    b64::encode(&[n; 32])
}
fn unit(n: u8, kind: EnrollmentKind, ks: &[&DeviceKeys], at: u64) -> Enrollment {
    Enrollment {
        id: id(n),
        kind,
        members: ks.iter().map(|k| k.sign_key()).collect(),
        enrolled_at: at,
    }
}
fn with_proofs(mut r: Roster, ks: &[&DeviceKeys], signer: &DeviceKeys) -> SignedRoster {
    let mut m = r.membership().unwrap();
    let expires = r.issued_at + 600;
    let context = if matches!(m.change, RosterChange::Enroll { .. }) {
        pairing_consent_context(&r, &[42; 32]).unwrap()
    } else {
        local_consent_context(&r).unwrap()
    };
    let ps = ks
        .iter()
        .map(|k| ConsentProof {
            key: k.sign_key(),
            expires_at: expires,
            context: context.clone(),
            signature: Signature::from_bytes(
                k.sign(&consent_message(&r, expires, &context).unwrap())
                    .unwrap(),
            ),
        })
        .collect();
    match &mut m.change {
        RosterChange::Genesis { proofs }
        | RosterChange::Upgrade { proofs }
        | RosterChange::Enroll { proofs, .. }
        | RosterChange::CompleteComputer { proofs, .. } => *proofs = ps,
        _ => panic!("not a consent operation"),
    };
    r.set_membership(&m);
    r.sign(signer).unwrap()
}
fn draft(chain: &RosterChain, signer: &DeviceKeys, change: RosterChange) -> Roster {
    let head = chain.head();
    let mut r = head.roster().clone();
    r.version += 1;
    r.prev = Some(head.hash());
    r.signed_by = signer.sign_key();
    r.issued_at = now();
    let mut m = r.membership().unwrap();
    m.change = change;
    r.set_membership(&m);
    r
}
fn dial(t: &RelayTarget, chain: RosterChain, k: Arc<DeviceKeys>) -> RingClient {
    let mut cfg = RingClientConfig::new(chain, k);
    cfg.tls = t.tls.clone();
    RingClient::connect(cfg, Recorder::new()).unwrap()
}
fn upgraded(t: &RelayTarget, with_b: bool) -> (Vec<Arc<DeviceKeys>>, RosterChain) {
    let a = keys();
    let d = keys();
    let b = keys();
    let g = SignedRoster::genesis(&*a, a.noise_key(), "retained", &t.url, now()).unwrap();
    let old = g
        .next(&*a, now(), |r| {
            r.add(Member::new(
                "retained headless",
                Role::Daemon,
                d.sign_key(),
                d.noise_key(),
                now(),
            ));
            if with_b {
                r.add(Member::new(
                    "unproven desktop",
                    Role::Desktop,
                    b.sign_key(),
                    b.noise_key(),
                    now(),
                ));
            }
        })
        .unwrap();
    let key = RingClient::activity_key(&t.url, g.ring_id(), t.tls.clone(), WAIT).unwrap();
    let mut r = old.roster().clone();
    r.v = 2;
    r.version += 1;
    r.prev = Some(old.hash());
    r.issued_at = now();
    let mut enrollments = vec![
        unit(1, EnrollmentKind::LegacyDesktop, &[&a], r.issued_at),
        unit(3, EnrollmentKind::Daemon, &[&d], r.issued_at),
    ];
    if with_b {
        enrollments.push(unit(2, EnrollmentKind::LegacyDesktop, &[&b], r.issued_at));
    }
    r.set_membership(&Membership {
        enrollments,
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: Some(2_592_000),
            activity_key: key,
        },
        change: RosterChange::Upgrade { proofs: vec![] },
    });
    let u = r.sign(&*a).unwrap();
    (
        vec![a, d, b],
        RosterChain::from_chain(vec![g, old, u]).unwrap(),
    )
}
fn complete(chain: &RosterChain, ks: &[Arc<DeviceKeys>]) -> SignedRoster {
    let mut r = draft(
        chain,
        &ks[0],
        RosterChange::CompleteComputer {
            replaces: vec![id(1), id(3)],
            enrollment_id: id(5),
            proofs: vec![],
        },
    );
    let mut m = r.membership().unwrap();
    m.enrollments.retain(|u| {
        !matches!(
            u.kind,
            EnrollmentKind::Daemon | EnrollmentKind::LegacyDesktop
        ) || u.id == id(2)
    });
    m.enrollments.push(unit(
        5,
        EnrollmentKind::Computer,
        &[&ks[0], &ks[1]],
        r.issued_at,
    ));
    m.retired.extend([id(1), id(3)]);
    r.set_membership(&m);
    with_proofs(r, &[&ks[0], &ks[1]], &ks[0])
}
pub fn loopback_genesis(t: &RelayTarget) {
    let a = keys();
    let d = keys();
    let ring = RingId::derive(&a.sign_key());
    let key = RingClient::activity_key(&t.url, &ring, t.tls.clone(), WAIT).unwrap();
    let at = now();
    let mut r = Roster {
        v: 2,
        ring_id: ring.clone(),
        version: 1,
        prev: None,
        relay_url: t.url.clone(),
        signed_by: a.sign_key(),
        issued_at: at,
        members: vec![
            Member::new("desktop", Role::Desktop, a.sign_key(), a.noise_key(), at),
            Member::new("daemon", Role::Daemon, d.sign_key(), d.noise_key(), at),
        ],
        extra: Default::default(),
    };
    r.set_membership(&Membership {
        enrollments: vec![unit(5, EnrollmentKind::Computer, &[&a, &d], at)],
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: Some(2_592_000),
            activity_key: key,
        },
        change: RosterChange::Genesis { proofs: vec![] },
    });
    let g = with_proofs(r, &[&a, &d], &a);
    let client = dial(t, RosterChain::from_chain(vec![g]).unwrap(), a);
    assert_eq!(client.roster().roster().v, 2);
    assert_eq!(
        RingClient::activity_key(&t.url, &ring, t.tls.clone(), WAIT).unwrap(),
        key
    );
}
pub fn legacy_upgrade_and_policy(t: &RelayTarget) {
    let (ks, chain) = upgraded(t, true);
    let client = dial(t, chain, ks[0].clone());
    assert_eq!(
        client
            .roster()
            .roster()
            .membership()
            .unwrap()
            .enrollments
            .iter()
            .filter(|u| u.kind == EnrollmentKind::LegacyDesktop)
            .count(),
        2
    );
    let report = client.activity_evidence().unwrap();
    assert!(report.terminal_evidence.is_empty());
    let old = client.roster().roster().membership().unwrap();
    ActivityEvidence::parse(&report.snapshot, &old.policy.activity_key).unwrap();
    let policy = RingPolicy {
        inactivity_seconds: None,
        activity_key: old.policy.activity_key,
    };
    let preview = client.preview_policy(policy.clone()).unwrap();
    assert!(preview.affected.is_empty());
    let mut r = draft(
        &client.chain(),
        &ks[0],
        RosterChange::SetPolicy {
            affected: preview.affected,
            preview: preview.token,
        },
    );
    let mut m = r.membership().unwrap();
    m.policy = policy.clone();
    r.set_membership(&m);
    client.publish_roster(&r.sign(&*ks[0]).unwrap()).unwrap();
    assert_eq!(
        client.roster().roster().membership().unwrap().policy,
        policy
    );
}
pub fn completion_departure_and_reused_keys(t: &RelayTarget) {
    let (ks, chain) = upgraded(t, true);
    let a = dial(t, chain.clone(), ks[0].clone());
    let singleton = chain
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(1))
        .unwrap()
        .clone();
    let joined = complete(&chain, &ks);
    a.publish_roster(&joined).unwrap();
    let grouped = a.chain();
    let target = grouped
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(5))
        .unwrap()
        .clone();
    // Pending cleanup of the old singleton proves absence; it cannot remove its replacement.
    let mut old =
        DepartureClient::connect(chain, ks[0].clone(), singleton, t.tls.clone(), WAIT).unwrap();
    assert!(matches!(
        old.complete().unwrap(),
        DepartureStatus::Absent(_)
    ));
    assert!(old
        .chain()
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(5))
        .is_some());
    let b = dial(t, grouped.clone(), ks[2].clone());
    let mut departure =
        DepartureClient::connect(grouped, ks[1].clone(), target.clone(), t.tls.clone(), WAIT)
            .unwrap();
    // Advance the actual head before cleanup: the departing client must fetch and sign anew.
    let mobile = keys();
    let mut r = draft(
        &b.chain(),
        &ks[2],
        RosterChange::Enroll {
            enrollment_id: id(6),
            proofs: vec![],
        },
    );
    r.members.push(Member::new(
        "mobile",
        Role::Mobile,
        mobile.sign_key(),
        mobile.noise_key(),
        r.issued_at,
    ));
    let mut m = r.membership().unwrap();
    m.enrollments
        .push(unit(6, EnrollmentKind::Mobile, &[&mobile], r.issued_at));
    r.set_membership(&m);
    b.publish_roster(&with_proofs(r, &[&mobile], &ks[2]))
        .unwrap();
    assert!(matches!(
        departure.complete().unwrap(),
        DepartureStatus::Absent(_)
    ));
    let absent_chain = departure.chain().clone();
    // Reuse public identities only with fresh proofs and a new enrollment ID.
    let mut r = draft(
        &absent_chain,
        &ks[2],
        RosterChange::Enroll {
            enrollment_id: id(7),
            proofs: vec![],
        },
    );
    r.members.extend([
        joined.member(&ks[0].sign_key()).unwrap().clone(),
        joined.member(&ks[1].sign_key()).unwrap().clone(),
    ]);
    let mut m = r.membership().unwrap();
    m.enrollments.push(unit(
        7,
        EnrollmentKind::Computer,
        &[&ks[0], &ks[1]],
        r.issued_at,
    ));
    r.set_membership(&m);
    // Reconnect B from the verified cleanup head so the public publish API uses the exact pin.
    drop(b);
    let b = dial(t, absent_chain.clone(), ks[2].clone());
    b.publish_roster(&with_proofs(r, &[&ks[0], &ks[1]], &ks[2]))
        .unwrap();
    let mut old =
        DepartureClient::connect(absent_chain, ks[1].clone(), target, t.tls.clone(), WAIT).unwrap();
    assert!(matches!(
        old.complete().unwrap(),
        DepartureStatus::Absent(_)
    ));
    assert!(old
        .chain()
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(7))
        .is_some());
}
pub fn terminal_ring_departure(t: &RelayTarget) {
    let (ks, chain) = upgraded(t, false);
    let a = dial(t, chain.clone(), ks[0].clone());
    let joined = complete(&chain, &ks);
    a.publish_roster(&joined).unwrap();
    let chain = a.chain();
    let target = chain
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(5))
        .unwrap()
        .clone();
    drop(a);
    let mut leave = DepartureClient::connect(
        chain.clone(),
        ks[1].clone(),
        target.clone(),
        t.tls.clone(),
        WAIT,
    )
    .unwrap();
    assert!(matches!(
        leave.complete().unwrap(),
        DepartureStatus::Absent(_)
    ));
    assert!(leave.chain().head().roster().members.is_empty());
    let mut absent =
        DepartureClient::connect(chain, ks[0].clone(), target, t.tls.clone(), WAIT).unwrap();
    assert!(matches!(absent.sync().unwrap(), DepartureStatus::Absent(_)));
    assert!(absent.chain().head().roster().members.is_empty());
}

/// Force a real competing publication after the departing client fetched its exact head,
/// but before it signs. Only scheduling is controlled; signatures and sockets are real.
pub fn departure_resigns_after_concurrent_commit(t: &RelayTarget) {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    struct RacingSigner {
        inner: Arc<DeviceKeys>,
        advance: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        count: AtomicUsize,
    }
    impl Signer for RacingSigner {
        fn sign_key(&self) -> crate::ring::SignKey {
            self.inner.sign_key()
        }
        fn sign(&self, msg: &[u8]) -> Result<[u8; 64], crate::ring::SignError> {
            if msg.starts_with(b"xshell-roster-v2\n") {
                self.count.fetch_add(1, Ordering::SeqCst);
                if let Some(f) = self.advance.lock().unwrap().take() {
                    f();
                }
            }
            self.inner.sign(msg)
        }
    }
    let (ks, chain) = upgraded(t, true);
    let a = dial(t, chain.clone(), ks[0].clone());
    let joined = complete(&chain, &ks);
    a.publish_roster(&joined).unwrap();
    let chain = a.chain();
    let target = chain
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(5))
        .unwrap()
        .clone();
    let b = Arc::new(dial(t, chain.clone(), ks[2].clone()));
    let mobile = keys();
    let mut r = draft(
        &chain,
        &ks[2],
        RosterChange::Enroll {
            enrollment_id: id(6),
            proofs: vec![],
        },
    );
    r.members.push(Member::new(
        "concurrent",
        Role::Mobile,
        mobile.sign_key(),
        mobile.noise_key(),
        r.issued_at,
    ));
    let mut m = r.membership().unwrap();
    m.enrollments
        .push(unit(6, EnrollmentKind::Mobile, &[&mobile], r.issued_at));
    r.set_membership(&m);
    let successor = with_proofs(r, &[&mobile], &ks[2]);
    let signer = Arc::new(RacingSigner {
        inner: ks[1].clone(),
        advance: Mutex::new(Some(Box::new(move || {
            b.publish_roster(&successor).unwrap()
        }))),
        count: AtomicUsize::new(0),
    });
    let mut leave =
        DepartureClient::connect(chain, signer.clone(), target, t.tls.clone(), WAIT).unwrap();
    assert!(matches!(
        leave.complete().unwrap(),
        DepartureStatus::Absent(_)
    ));
    assert_eq!(signer.count.load(Ordering::SeqCst), 2);
    assert!(leave
        .chain()
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(6))
        .is_some());
}

pub fn terminal_evidence_survives_policy_increase(t: &RelayTarget) {
    let (ks, chain) = upgraded(t, true);
    let a = dial(t, chain.clone(), ks[0].clone());
    a.publish_roster(&complete(&chain, &ks)).unwrap();
    let b = dial(t, a.chain(), ks[2].clone());
    let key = b
        .roster()
        .roster()
        .membership()
        .unwrap()
        .policy
        .activity_key;
    let finite = RingPolicy {
        inactivity_seconds: Some(1),
        activity_key: key,
    };
    let preview = b.preview_policy(finite.clone()).unwrap();
    let mut r = draft(
        &b.chain(),
        &ks[2],
        RosterChange::SetPolicy {
            affected: preview.affected,
            preview: preview.token,
        },
    );
    let mut m = r.membership().unwrap();
    m.policy = finite;
    r.set_membership(&m);
    b.publish_roster(&r.sign(&*ks[2]).unwrap()).unwrap();
    let policy_head = b.roster();
    // Keep the Computer genuinely live through preview/commit. Otherwise a one-second
    // boundary can correctly invalidate that preview under a parallel contract run.
    drop(a);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let report = b.activity_evidence().unwrap();
    assert!(b
        .send(
            &ks[0].sign_key(),
            b"terminal enrollment cannot receive traffic"
        )
        .is_err());
    let unit = b
        .roster()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(5))
        .unwrap()
        .clone();
    let evidence = report
        .terminal_evidence
        .iter()
        .find(|token| {
            ActivityEvidence::parse(token, &key)
                .unwrap()
                .units
                .iter()
                .any(|u| u.enrollment_id == id(5))
        })
        .unwrap()
        .clone();
    let parsed = ActivityEvidence::parse(&evidence, &key).unwrap();
    assert!(parsed.binds(
        &policy_head,
        &policy_head.roster().membership().unwrap().policy
    ));
    assert!(parsed.eligible(
        &unit,
        &policy_head.roster().membership().unwrap().policy,
        true
    ));
    let never = RingPolicy {
        inactivity_seconds: None,
        activity_key: key,
    };
    let preview = b.preview_policy(never.clone()).unwrap();
    let mut r = draft(
        &b.chain(),
        &ks[2],
        RosterChange::SetPolicy {
            affected: preview.affected,
            preview: preview.token,
        },
    );
    let mut m = r.membership().unwrap();
    m.policy = never;
    r.set_membership(&m);
    b.publish_roster(&r.sign(&*ks[2]).unwrap()).unwrap();
    let mut cfg = RingClientConfig::new(b.chain(), ks[0].clone());
    cfg.tls = t.tls.clone();
    assert!(RingClient::connect(cfg, Recorder::new()).is_err());
    let mut r = draft(
        &b.chain(),
        &ks[2],
        RosterChange::Expire {
            enrollment_ids: vec![id(5)],
            evidence,
        },
    );
    let mut m = r.membership().unwrap();
    m.enrollments.retain(|u| u.id != id(5));
    m.retired.push(id(5));
    r.members.retain(|a| !unit.members.contains(&a.sign_key));
    r.set_membership(&m);
    b.publish_roster(&r.sign(&*ks[2]).unwrap()).unwrap();
    assert!(b
        .roster()
        .roster()
        .membership()
        .unwrap()
        .unit(&id(5))
        .is_none());
}

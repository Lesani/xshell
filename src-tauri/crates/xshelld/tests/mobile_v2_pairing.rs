#![cfg(unix)]
//! #87: phone pairing into v2 Rings, end to end on production workerd. Real Desktops and
//! Daemons (public Create / Join / legacy Upgrade), the phone through the protocol's guest
//! library (`adopt_mobile_as_guest_with_ready`, the Mobile FFI entry point).
mod common;
#[path = "desktop/relay_fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "desktop/workerd.rs"]
mod workerd;
use fixture::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use workerd::Relay;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision};
use xshell_hostlink::ring::{PairingEvent, PairingFlow, PairingObserver};
use xshell_protocol::ring::membership::{ConsentFlow, EnrollmentKind, RosterChange};
use xshell_protocol::ring::pairing::{PairError, PairRefusal, PairingOffer};
use xshell_protocol::ring::relay::contract::relay_version;
use xshell_protocol::ring::relay::pair::{
    adopt_mobile_as_guest_with_ready, AdoptionConsentRequest, MobileAdoptionRequest,
    MobileEnrollment, PairOptions,
};
use xshell_protocol::ring::{DeviceKeys, Role, RosterChain, SignKey};

#[derive(Default)]
struct Pairings(Mutex<Vec<PairingEvent>>);
impl PairingObserver for Pairings {
    fn pairing(&self, _: PairingFlow, event: &PairingEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}
impl Pairings {
    fn outcome(&self) -> PairingEvent {
        let until = Instant::now() + WAIT;
        loop {
            if let Some(e) = self
                .0
                .lock()
                .unwrap()
                .last()
                .filter(|e| **e != PairingEvent::Waiting)
            {
                return e.clone();
            }
            assert!(Instant::now() < until, "no pairing outcome");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn phone_keys(n: u8) -> Arc<DeviceKeys> {
    Arc::new(DeviceKeys::from_seeds(&[n; 32], &[n.wrapping_add(100); 32]))
}

fn connected(c: &Computer) {
    wait("Desktop connected to workerd", || {
        c.ring.view().connection == "connected"
    });
}

/// A fresh v2 phone offer on `c`, checked against its accepted head.
fn offer(c: &Computer) -> (PairingOffer, Arc<Pairings>) {
    let obs = Arc::new(Pairings::default());
    let shown = c.ring.pair_phone(obs.clone()).unwrap();
    let offer = PairingOffer::parse(&shown.payload).unwrap();
    let head = c.ring.chain().unwrap().head().clone();
    let anchor = offer.head.clone().expect("a v2 head shows a v2 offer");
    assert_eq!((anchor.version, anchor.hash), (head.version(), head.hash()));
    assert_eq!(offer.version(), 2);
    (offer, obs)
}

fn scan_with(
    offer: &PairingOffer,
    keys: &Arc<DeviceKeys>,
    confirm: impl FnOnce(AdoptionConsentRequest<'_>) -> Result<bool, PairError>,
) -> Result<MobileEnrollment, PairError> {
    adopt_mobile_as_guest_with_ready(
        &MobileAdoptionRequest {
            offer,
            keys: keys.clone(),
            name: "my phone",
            until: Instant::now() + Duration::from_secs(60),
        },
        &PairOptions::default(),
        None,
        || Ok(()),
        confirm,
    )
}

/// The app's auto-confirm (plan decision 3), after checking what it is asked to sign.
fn scan(offer: &PairingOffer, keys: &Arc<DeviceKeys>) -> Result<MobileEnrollment, PairError> {
    let key = keys.sign_key();
    scan_with(offer, keys, |r| {
        let m = r.verified.proposal.membership().unwrap();
        let RosterChange::Enroll { enrollment_id, .. } = &m.change else {
            panic!("not Enroll");
        };
        assert_eq!(m.unit(enrollment_id).unwrap().kind, EnrollmentKind::Mobile);
        assert_eq!(r.verified.proposal.member(&key).unwrap().role, Role::Mobile);
        Ok(true)
    })
}

fn assert_mobile_unit(chain: &RosterChain, key: &SignKey) {
    let head = chain.head();
    assert_eq!(head.member(key).unwrap().role, Role::Mobile);
    let m = head.roster().membership().unwrap();
    let unit = m.unit_for(key).unwrap();
    assert_eq!(unit.kind, EnrollmentKind::Mobile);
    assert_eq!(unit.members, vec![*key]);
}

fn relay_head(relay: &Relay, chain: &RosterChain) -> u64 {
    relay_version(&relay.target(), chain.ring_id())
}

fn wait_head(c: &Computer, version: u64) {
    wait("Desktop holds the Relay head", || {
        c.ring
            .chain()
            .is_some_and(|ch| ch.head().version() == version)
    });
}

#[test]
fn created_ring_pairs_a_phone_recovers_a_lost_ack_and_refuses_duplicates() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    let created = a.create();
    connected(&a);
    let (o, obs) = offer(&a);
    let phone = phone_keys(30);
    let enrolled = scan(&o, &phone).unwrap();
    assert!(!enrolled.already_enrolled);
    let head = enrolled.chain.head().clone();
    assert_eq!(head.version(), created.head().version() + 1);
    assert_eq!(head.roster().signed_by, o.sign_key);
    assert_mobile_unit(&enrolled.chain, &phone.sign_key());
    let RosterChange::Enroll { proofs, .. } = head.roster().membership().unwrap().change else {
        panic!("not Enroll");
    };
    assert_eq!(proofs.len(), 1);
    assert_eq!(proofs[0].key, phone.sign_key());
    assert_eq!(proofs[0].context.kind, ConsentFlow::Pairing);
    assert_eq!(relay_head(&relay, &created), head.version());
    assert_eq!(
        obs.outcome(),
        PairingEvent::Paired {
            name: "my phone".into(),
            role: Role::Mobile
        }
    );
    assert_eq!(a.ring.chain().unwrap().head(), &head);
    // The Relay head lists the Mobile unit (a fresh reader sees it there).
    assert!(a
        .ring
        .view()
        .members
        .iter()
        .any(|m| m.sign_key == phone.sign_key()));

    // Lost acknowledgment / failed phone persistence: the same keys on a fresh offer get
    // an authenticated "already enrolled" completion and no new version.
    let (o, obs) = offer(&a);
    let again = scan_with(&o, &phone, |_| panic!("no consent for a recovery")).unwrap();
    assert!(again.already_enrolled);
    assert_eq!(again.joined.version, head.version());
    assert_eq!(again.chain.head(), &head);
    assert_eq!(relay_head(&relay, &created), head.version());
    assert!(matches!(obs.outcome(), PairingEvent::Paired { .. }));

    // The same signing key with another Noise key, and another signing key with the same
    // Noise key: typed duplicate refusals, membership unchanged.
    let same_sign = Arc::new(DeviceKeys::from_seeds(&[30; 32], &[77; 32]));
    let same_noise = Arc::new(DeviceKeys::from_seeds(&[78; 32], &[130; 32]));
    assert_eq!(same_noise.noise_key(), phone.noise_key());
    for keys in [same_sign, same_noise] {
        let (o, obs) = offer(&a);
        assert_eq!(
            scan_with(&o, &keys, |_| panic!("no consent for a duplicate")).unwrap_err(),
            PairError::Refused(PairRefusal::Duplicate)
        );
        assert_eq!(
            obs.outcome(),
            PairingEvent::Failed {
                code: "duplicate".into()
            }
        );
        assert_eq!(relay_head(&relay, &created), head.version());
        assert_eq!(a.ring.chain().unwrap().head(), &head);
    }
}

#[test]
fn phone_decline_and_desktop_cancel_create_no_membership() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    let created = a.create();
    connected(&a);
    let version = created.head().version();
    // The phone declines after verification: no proof is signed, nothing is committed.
    let (o, obs) = offer(&a);
    let phone = phone_keys(31);
    assert_eq!(
        scan_with(&o, &phone, |_| Ok(false)).unwrap_err(),
        PairError::Cancelled
    );
    assert_eq!(
        obs.outcome(),
        PairingEvent::Failed {
            code: "cancelled".into()
        }
    );
    assert_eq!(relay_head(&relay, &created), version);
    assert!(a
        .ring
        .chain()
        .unwrap()
        .head()
        .member(&phone.sign_key())
        .is_none());
    // The Desktop cancels while the phone is looking at the proposal: a typed refusal.
    let (o, _obs) = offer(&a);
    let ring = a.ring.clone();
    let r = scan_with(&o, &phone, move |_| {
        ring.cancel_pairing(PairingFlow::Phone);
        Ok(true)
    });
    assert_eq!(r.unwrap_err(), PairError::Refused(PairRefusal::Cancelled));
    assert_eq!(relay_head(&relay, &created), version);
    assert!(a
        .ring
        .chain()
        .unwrap()
        .head()
        .member(&phone.sign_key())
        .is_none());
    // Only then a real pairing works.
    let (o, _) = offer(&a);
    scan(&o, &phone).unwrap();
    assert_eq!(relay_head(&relay, &created), version + 1);
}

type Tamper = Box<dyn Fn(&mut PairingOffer)>;

#[test]
fn every_tampered_offer_anchor_is_refused_before_consent() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    let created = a.create();
    connected(&a);
    let version = created.head().version();
    let other = DeviceKeys::generate().unwrap();
    let tampers: Vec<(&str, Tamper)> = vec![
        (
            "head hash",
            Box::new(|o| {
                o.head.as_mut().unwrap().hash = xshell_protocol::ring::b64::encode(&[1; 32])
            }),
        ),
        (
            "head version",
            Box::new(|o| o.head.as_mut().unwrap().version -= 1),
        ),
        (
            "approver signing key",
            Box::new(move |o| o.sign_key = other.sign_key()),
        ),
        (
            "approver Noise key",
            Box::new(|o| o.noise_key = DeviceKeys::generate().unwrap().noise_key()),
        ),
        (
            "ring",
            Box::new(|o| {
                o.ring_id = xshell_protocol::ring::RingId::derive(
                    &DeviceKeys::generate().unwrap().sign_key(),
                )
            }),
        ),
    ];
    let phone = phone_keys(32);
    for (what, tamper) in tampers {
        let (mut o, _) = offer(&a);
        tamper(&mut o);
        let r = scan_with(&o, &phone, |_| panic!("{what}: consent requested"));
        assert_eq!(r.unwrap_err(), PairError::Pin, "{what}");
        assert_eq!(relay_head(&relay, &created), version, "{what}");
    }
    assert!(a
        .ring
        .chain()
        .unwrap()
        .head()
        .member(&phone.sign_key())
        .is_none());
}

#[test]
fn an_expired_offer_is_refused_without_membership() {
    let relay = Relay::start();
    let a = Computer::with_ring_config(&relay, |c| c.pair_ttl = Duration::from_secs(2));
    let created = a.create();
    connected(&a);
    let (o, obs) = offer(&a);
    std::thread::sleep(Duration::from_millis(2500));
    let phone = phone_keys(33);
    assert_eq!(
        scan_with(&o, &phone, |_| panic!("consent after expiry")).unwrap_err(),
        PairError::Expired
    );
    assert_eq!(obs.outcome(), PairingEvent::Expired);
    assert_eq!(relay_head(&relay, &created), created.head().version());
}

#[test]
fn legacy_upgrade_keeps_the_v1_phone_and_pairs_a_new_one_on_v2() {
    use xshell_protocol::ring::relay::pair::{pair_as_guest_pinned, GuestRequest};
    let relay = Relay::start();
    let b = Computer::new(&relay);
    b.ring.enable(None, false).unwrap();
    connected(&b);
    // A v1 phone pairs through the unchanged v1 flow.
    let shown = b.ring.pair_phone(Arc::new(Pairings::default())).unwrap();
    let v1 = PairingOffer::parse(&shown.payload).unwrap();
    assert!(v1.head.is_none());
    let old_phone = phone_keys(34);
    let v1_chain = pair_as_guest_pinned(
        &GuestRequest {
            relay_url: &v1.relay_url,
            secret: &v1.secret,
            pin: Some(v1.noise_key),
            ring_id: Some(&v1.ring_id),
            keys: old_phone.clone(),
            role: Role::Mobile,
            name: "old phone",
            wait: Duration::ZERO,
        },
        &PairOptions::default(),
        None,
        Some(v1.sign_key),
    )
    .unwrap();
    assert_eq!(v1_chain.head().roster().v, 1);
    wait_head(&b, v1_chain.head().version());
    let upgraded = match b
        .ring
        .upgrade_retained_ring(&b.companion(), |p, lease| {
            Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
        })
        .unwrap()
    {
        AdoptionOutcome::Installed { chain, .. } => chain,
        _ => panic!("Upgrade not installed"),
    };
    assert_eq!(upgraded.head().roster().v, 2);
    assert_mobile_unit(&upgraded, &old_phone.sign_key());
    connected(&b);
    wait_head(&b, upgraded.head().version());
    // The upgraded phone scanning a v2 offer is already enrolled (exact Upgrade head).
    let (o, _) = offer(&b);
    let back = scan_with(&o, &old_phone, |_| panic!("no consent for a member")).unwrap();
    assert!(back.already_enrolled);
    assert_eq!(back.chain.head(), upgraded.head());
    // A new phone enrolls as its own Mobile unit on the upgraded Ring.
    let (o, _) = offer(&b);
    let phone = phone_keys(35);
    let enrolled = scan(&o, &phone).unwrap();
    assert!(!enrolled.already_enrolled);
    assert_eq!(
        enrolled.chain.head().version(),
        upgraded.head().version() + 1
    );
    assert_mobile_unit(&enrolled.chain, &phone.sign_key());
    assert_mobile_unit(&enrolled.chain, &old_phone.sign_key());
    assert_eq!(
        relay_head(&relay, &upgraded),
        enrolled.chain.head().version()
    );
}

#[test]
fn recovery_after_an_intervening_update_signed_by_another_desktop() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    connected(&a);
    let b = Computer::new(&relay);
    b.join(&a);
    connected(&b);
    let (o, _) = offer(&a);
    let phone = phone_keys(36);
    let first = scan(&o, &phone).unwrap();
    wait_head(&b, first.chain.head().version());
    // Desktop B pairs another phone: the head is now signed by B, not the approver A.
    let (o, _) = offer(&b);
    let second = scan(&o, &phone_keys(37)).unwrap();
    let b_key = second.chain.head().roster().signed_by;
    assert_ne!(b_key, first.chain.head().roster().signed_by);
    wait_head(&a, second.chain.head().version());
    // The first phone lost its acknowledgment: A's fresh offer recovers it with B's head.
    let (o, _) = offer(&a);
    let again = scan_with(&o, &phone, |_| panic!("no consent for a member")).unwrap();
    assert!(again.already_enrolled);
    assert_eq!(again.joined.signed_by, b_key);
    assert_eq!(again.chain.head(), second.chain.head());
    assert_mobile_unit(&again.chain, &phone.sign_key());
    assert_eq!(
        relay_head(&relay, &again.chain),
        second.chain.head().version()
    );
}

#[test]
fn proposal_contract_mobile_scenarios_on_production_workerd() {
    use xshell_protocol::ring::relay::membership_contract::PROPOSAL_SCENARIOS;
    let relay = Relay::start();
    for (name, scenario) in PROPOSAL_SCENARIOS
        .iter()
        .filter(|(name, _)| name.starts_with("proposal_mobile_"))
    {
        eprintln!("scenario {name}");
        scenario(&relay.target());
    }
}

/// A Desktop whose v2 phone pairing stops at `at` until the test resumes it.
fn paused(
    relay: &Relay,
    at: xshell_hostlink::ring::AdoptionTestPoint,
) -> (
    Computer,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached, reached_rx) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel::<()>();
    let resume_rx = Mutex::new(resume_rx);
    let c = Computer::with_ring_config(relay, move |cfg| {
        cfg.adoption_test_hook = Some(xshell_hostlink::ring::AdoptionTestHook(Arc::new(
            move |point| {
                if point == at {
                    let _ = reached.send(());
                    let _ = resume_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(60));
                }
            },
        )));
    });
    (c, reached_rx, resume)
}

#[test]
fn scoped_cancel_after_the_send_boundary_is_never_no_commit() {
    use xshell_protocol::pairing_access::enrollment::{
        EnrollmentEvidence, EnrollmentParameters, PairOperationId, RecipientIdentity,
    };
    use xshell_protocol::pairing_access::MobileRole;
    let mut relay = Relay::start();
    let (a, reached, resume) = paused(
        &relay,
        xshell_hostlink::ring::AdoptionTestPoint::PhonePublishedBeforeReadBack,
    );
    let created = a.create();
    connected(&a);
    let version = created.head().version();
    let scope = a
        .ring
        .enrollment_scope(created.ring_id(), "my phone".into())
        .unwrap();
    let target = scope.enrollment.clone().unwrap();
    let phone = phone_keys(40);
    let parameters = EnrollmentParameters {
        ring: created.ring_id().clone(),
        approver: RecipientIdentity {
            signing: scope.approver,
            noise: target.approver_noise,
        },
        recipient: RecipientIdentity {
            signing: phone.sign_key(),
            noise: phone.noise_key(),
        },
        relay: target.relay.clone(),
        role: MobileRole::Mobile,
        device_name: "my phone".into(),
        initial_head: target.initial_head.clone(),
        expires_at: xshell_protocol::ring::relay::contract::now() + 300,
    };
    let task = uuid::Uuid::new_v4();
    let operation = PairOperationId::new(uuid::Uuid::new_v4()).unwrap();
    let start = a
        .ring
        .enrollment_offer(task, operation, parameters)
        .unwrap();
    let scoped = PairingOffer::parse(start.offer.expose()).unwrap();
    assert!(scoped.head.is_some());
    let scanner = {
        let phone = phone.clone();
        std::thread::spawn(move || scan(&scoped, &phone))
    };
    reached.recv_timeout(Duration::from_secs(30)).unwrap();
    // The candidate was handed to the Relay; its acknowledgment is withheld.
    let evidence = |cancel| {
        a.ring
            .enrollment_status(task, operation, cancel)
            .receipt
            .map(|r| r.evidence)
    };
    assert_eq!(evidence(true), Some(EnrollmentEvidence::OutcomeUnknown));
    // The accepted read-back then fails too: still uncertain, never no-commit.
    relay.restart();
    resume.send(()).unwrap();
    assert!(scanner.join().unwrap().is_err());
    assert_eq!(evidence(false), Some(EnrollmentEvidence::OutcomeUnknown));
    connected(&a);
    // Whatever the Relay did, the same keys learn it: recovery, or a clean first pairing.
    let landed = relay_head(&relay, &created) == version + 1;
    let (o, _) = offer(&a);
    let again = scan(&o, &phone).unwrap();
    assert_eq!(again.already_enrolled, landed);
    assert_mobile_unit(&again.chain, &phone.sign_key());
}

#[test]
fn a_head_change_at_the_send_boundary_is_stale_not_uncertain() {
    let relay = Relay::start();
    let (a, reached, resume) = paused(
        &relay,
        xshell_hostlink::ring::AdoptionTestPoint::PhoneBeforePublish,
    );
    a.create();
    connected(&a);
    let b = Computer::new(&relay);
    b.join(&a);
    connected(&b);
    let (o, obs) = offer(&a);
    let phone = phone_keys(41);
    let scanner = {
        let phone = phone.clone();
        std::thread::spawn(move || scan(&o, &phone))
    };
    reached.recv_timeout(Duration::from_secs(30)).unwrap();
    // Between the predecessor check and the send, Desktop B moves the head.
    let (o2, _) = offer(&b);
    let moved = scan(&o2, &phone_keys(42)).unwrap();
    wait_head(&a, moved.chain.head().version());
    resume.send(()).unwrap();
    assert_eq!(
        scanner.join().unwrap().unwrap_err(),
        PairError::Refused(PairRefusal::Stale)
    );
    assert_eq!(
        obs.outcome(),
        PairingEvent::Failed {
            code: "stale".into()
        }
    );
    assert_eq!(
        relay_head(&relay, &moved.chain),
        moved.chain.head().version()
    );
    let head = a.ring.chain().unwrap().head().clone();
    assert_eq!(head, *moved.chain.head());
    assert!(head.member(&phone.sign_key()).is_none());
}

#[test]
fn a_phone_cancelled_at_consent_sends_an_authenticated_decline() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let relay = Relay::start();
    let a = Computer::new(&relay);
    let created = a.create();
    connected(&a);
    let phone = phone_keys(43);
    // The user cancels while the consent callback runs; the callback then declines (or,
    // racing, approves): either way the approver hears an authenticated cancellation.
    for approve in [false, true] {
        let (o, obs) = offer(&a);
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let r = adopt_mobile_as_guest_with_ready(
            &MobileAdoptionRequest {
                offer: &o,
                keys: phone.clone(),
                name: "my phone",
                until: Instant::now() + Duration::from_secs(60),
            },
            &PairOptions::default(),
            Some(&cancel),
            || Ok(()),
            move |_| {
                flag.store(true, Ordering::Release);
                Ok(approve)
            },
        );
        assert_eq!(r.unwrap_err(), PairError::Cancelled, "approve={approve}");
        assert_eq!(
            obs.outcome(),
            PairingEvent::Failed {
                code: "cancelled".into()
            },
            "approve={approve}"
        );
        assert_eq!(relay_head(&relay, &created), created.head().version());
        assert!(a
            .ring
            .chain()
            .unwrap()
            .head()
            .member(&phone.sign_key())
            .is_none());
    }
}

/// A Desktop whose test seams report every point reached and stop at the one chosen.
struct Seams {
    pause: Arc<Mutex<Option<xshell_hostlink::ring::AdoptionTestPoint>>>,
    reached: std::sync::mpsc::Receiver<xshell_hostlink::ring::AdoptionTestPoint>,
    resume: std::sync::mpsc::Sender<()>,
}
impl Seams {
    fn computer(relay: &Relay) -> (Computer, Seams) {
        let pause = Arc::new(Mutex::new(None));
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        let at = pause.clone();
        let c = Computer::with_ring_config(relay, move |cfg| {
            cfg.adoption_test_hook = Some(xshell_hostlink::ring::AdoptionTestHook(Arc::new(
                move |point| {
                    // Decide before reporting: the test clears the pause once it hears.
                    let stop = *at.lock().unwrap() == Some(point);
                    let _ = reached_tx.send(point);
                    if stop {
                        let _ = resume_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(60));
                    }
                },
            )));
        });
        (
            c,
            Seams {
                pause,
                reached,
                resume,
            },
        )
    }
    fn wait_for(&self, point: xshell_hostlink::ring::AdoptionTestPoint) {
        assert!(self.try_wait_for(point), "seam {point:?} not reached");
    }
    fn try_wait_for(&self, point: xshell_hostlink::ring::AdoptionTestPoint) -> bool {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.reached.recv_timeout(left) {
                Ok(p) if p == point => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }
}

/// Relay, Connector (the Desktop's view) and the Desktop store hold the same head.
fn agree(relay: &Relay, c: &Computer) -> xshell_protocol::ring::SignedRoster {
    wait("Relay, Connector and store agree", || {
        let stored = xshell_hostlink::ring::store::Store::new(c.home.root().join("desktop"))
            .load()
            .ok()
            .and_then(|l| l.0)
            .map(|s| s.chain.head().clone());
        let held = c.ring.chain().unwrap();
        relay_head(relay, &held) == held.head().version() && stored.as_ref() == Some(held.head())
    });
    c.ring.chain().unwrap().head().clone()
}

#[test]
fn a_removal_waits_for_a_paused_phone_publication_and_extends_its_head() {
    use xshell_hostlink::ring::AdoptionTestPoint as P;
    let relay = Relay::start();
    let (a, seams) = Seams::computer(&relay);
    a.create();
    connected(&a);
    let mut removable = vec![];
    for n in [50, 51] {
        let (o, _) = offer(&a);
        let k = phone_keys(n);
        scan(&o, &k).unwrap();
        removable.push(k);
    }
    for (round, (at, gone)) in [
        (P::PhoneBeforePublish, &removable[0]),
        (P::PhonePublishedBeforeReadBack, &removable[1]),
    ]
    .into_iter()
    .enumerate()
    {
        let before = agree(&relay, &a).version();
        *seams.pause.lock().unwrap() = Some(at);
        while seams.reached.try_recv().is_ok() {}
        let (o, _) = offer(&a);
        let phone = phone_keys(60 + round as u8);
        let scanner = {
            let phone = phone.clone();
            std::thread::spawn(move || scan(&o, &phone))
        };
        seams.wait_for(at);
        *seams.pause.lock().unwrap() = None;
        // A removal starts while the phone holds the membership-mutation reservation.
        let ring = a.ring.clone();
        let key = gone.sign_key();
        let removal = std::thread::spawn(move || ring.remove_member(&key));
        std::thread::sleep(Duration::from_millis(300));
        if removal.is_finished() {
            panic!("removal did not wait (round {round}): {:?}", removal.join());
        }
        seams.resume.send(()).unwrap();
        let enrolled = scanner.join().unwrap().unwrap();
        // (The phone's fetched chain may already include the removal that followed.)
        assert_eq!(enrolled.joined.version, before + 1);
        let removed = removal.join().unwrap().unwrap();
        // The removal was built on the phone's head: one linear chain, no fork.
        assert_eq!(removed.head().version(), before + 2);
        assert_eq!(
            removed.head().roster().prev.as_deref(),
            Some(enrolled.joined.hash.as_str())
        );
        let head = agree(&relay, &a);
        assert_eq!(&head, removed.head());
        assert_mobile_unit(&removed, &phone.sign_key());
        assert!(head.member(&gone.sign_key()).is_none());
    }
}

#[test]
fn a_phone_publication_after_a_paused_removal_is_stale_without_a_fork() {
    use xshell_hostlink::ring::AdoptionTestPoint as P;
    let relay = Relay::start();
    let (a, seams) = Seams::computer(&relay);
    a.create();
    connected(&a);
    let (o, _) = offer(&a);
    let gone = phone_keys(52);
    scan(&o, &gone).unwrap();
    let before = agree(&relay, &a).version();
    // The removal holds the reservation, stopped before its publication.
    *seams.pause.lock().unwrap() = Some(P::RemoveBeforePublish);
    let ring = a.ring.clone();
    let key = gone.sign_key();
    connected(&a);
    let removal = std::thread::spawn(move || ring.remove_member(&key));
    if !seams.try_wait_for(P::RemoveBeforePublish) {
        panic!(
            "removal never reached its publication: {:?}",
            removal.join()
        );
    }
    *seams.pause.lock().unwrap() = None;
    // Earlier pairings' seam reports are not this phone's.
    while seams.reached.try_recv().is_ok() {}
    // A phone consents meanwhile; its publication waits for the reservation.
    let (o, obs) = offer(&a);
    let phone = phone_keys(62);
    let scanner = {
        let phone = phone.clone();
        std::thread::spawn(move || scan(&o, &phone))
    };
    if !seams.try_wait_for(P::PhoneBeforeReservation) {
        seams.resume.send(()).unwrap();
        panic!(
            "phone never reached the reservation: {:?}; removal {:?}",
            scanner.join(),
            removal.join()
        );
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!scanner.is_finished());
    seams.resume.send(()).unwrap();
    let removed = removal.join().unwrap().unwrap();
    assert_eq!(removed.head().version(), before + 1);
    // The phone's proposal was frozen on the old head: refused as stale, nothing sent.
    assert_eq!(
        scanner.join().unwrap().unwrap_err(),
        PairError::Refused(PairRefusal::Stale)
    );
    assert_eq!(
        obs.outcome(),
        PairingEvent::Failed {
            code: "stale".into()
        }
    );
    let head = agree(&relay, &a);
    assert_eq!(&head, removed.head());
    assert!(head.member(&phone.sign_key()).is_none());
    // A fresh offer on the new head pairs it.
    let (o, _) = offer(&a);
    let enrolled = scan(&o, &phone).unwrap();
    assert_eq!(enrolled.chain.head().version(), before + 2);
    assert_eq!(&agree(&relay, &a), enrolled.chain.head());
}

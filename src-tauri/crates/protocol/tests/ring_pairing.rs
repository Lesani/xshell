//! Pairing through the test Relay's pairing pipe: a phone with a QR payload, a computer with
//! a code, and the refusals (expired, reused, a wrong code, a Relay that hides the chain).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use xshell_protocol::ring::pairing::{
    JoinRequest, Joined, PairCode, PairError, PairRefusal, PairSecret, PairingOffer,
};
use xshell_protocol::ring::relay::contract::{self, TestRing};
use xshell_protocol::ring::relay::pair::{
    host_pairing, pair_as_guest, pair_as_guest_pinned, pair_as_guest_pinned_with_joined,
    GuestRequest, HostRequest, PairOptions, PairPipe, PairingHost,
};
use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions};
use xshell_protocol::ring::relay::{ChainPin, RingClient, RingTimeouts};
use xshell_protocol::ring::{DeviceKeys, Member, RingError, Role, Signer};

fn opts() -> PairOptions {
    PairOptions {
        step: Duration::from_secs(5),
        ping_interval: Duration::from_millis(200),
        ring: RingTimeouts {
            connect: Duration::from_secs(5),
            ..RingTimeouts::default()
        },
        ..PairOptions::default()
    }
}

/// A Desktop's bookkeeping: one use, a deadline, and a Roster version per join.
struct Desk {
    ring: Mutex<TestRing>,
    client: RingClient,
    used: AtomicBool,
    until: Instant,
    publish: bool,
    reply_relay: Option<String>,
    advance_after_join: bool,
}

impl Desk {
    fn new(r: &TestRelay, ttl: Duration) -> Arc<Desk> {
        let ring = TestRing::new(&r.url());
        let (client, _) = contract::connect(&r.target(), &ring.chain, ring.desktop.clone());
        Arc::new(Desk {
            ring: Mutex::new(ring),
            client,
            used: AtomicBool::new(false),
            until: Instant::now() + ttl,
            publish: true,
            reply_relay: None,
            advance_after_join: false,
        })
    }
}

impl PairingHost for Desk {
    fn consume(&self) -> Result<(), PairRefusal> {
        if self.used.swap(true, Ordering::SeqCst) {
            return Err(PairRefusal::Used);
        }
        if Instant::now() >= self.until {
            return Err(PairRefusal::Expired);
        }
        Ok(())
    }

    fn add(&self, req: &JoinRequest) -> Result<Joined, PairRefusal> {
        let mut ring = self.ring.lock().unwrap();
        let next = ring.next(|d| {
            d.add(Member::new(
                &req.name,
                req.role,
                req.sign_key,
                req.noise_key,
                contract::now(),
            ))
        });
        if self.publish {
            self.client
                .publish_roster(&next)
                .map_err(|_| PairRefusal::PublishFailed)?;
        }
        if self.advance_after_join {
            let later = ring.next(|_| {});
            self.client
                .publish_roster(&later)
                .map_err(|_| PairRefusal::PublishFailed)?;
        }
        Ok(Joined {
            ring_id: next.ring_id().clone(),
            relay_url: self
                .reply_relay
                .clone()
                .unwrap_or_else(|| next.roster().relay_url.clone()),
            version: next.version(),
            hash: next.hash(),
            signed_by: next.roster().signed_by,
        })
    }
}

/// The Desktop shows a QR: opens the slot, waits, and runs its side once the phone is there.
fn show_qr(
    r: &TestRelay,
    desk: &Arc<Desk>,
    secret: &PairSecret,
) -> (
    PairingOffer,
    std::thread::JoinHandle<Result<JoinRequest, PairError>>,
) {
    let ring = desk.ring.lock().unwrap();
    let offer = PairingOffer {
        ring_id: ring.ring_id(),
        relay_url: r.url(),
        sign_key: ring.desktop.sign_key(),
        noise_key: ring.desktop.noise_key(),
        secret: secret.clone(),
        expires_at: contract::now() + 600,
        head: None,
    };
    let keys = ring.desktop.clone();
    let ring_id = ring.ring_id();
    drop(ring);
    let mut pipe = PairPipe::open(&r.url(), &secret.slot(), &opts()).unwrap();
    assert!(!pipe.has_peer());
    let (desk, secret) = (desk.clone(), secret.clone());
    let h = std::thread::spawn(move || {
        pipe.wait_peer(Instant::now() + Duration::from_secs(10), None)?;
        host_pairing(
            pipe,
            &HostRequest {
                keys: &keys,
                secret: &secret,
                ring_id: &ring_id,
                name: "desk",
                role: Role::Mobile,
            },
            &*desk,
            None,
        )
    });
    (offer, h)
}

fn phone(
    offer: &PairingOffer,
    keys: &Arc<DeviceKeys>,
) -> Result<xshell_protocol::ring::RosterChain, PairError> {
    // The phone parses what it scanned.
    let o = PairingOffer::parse(&offer.encode()).unwrap();
    pair_as_guest_pinned(
        &GuestRequest {
            relay_url: &o.relay_url,
            secret: &o.secret,
            pin: Some(o.noise_key),
            ring_id: Some(&o.ring_id),
            keys: keys.clone(),
            role: Role::Mobile,
            name: "phone",
            wait: Duration::ZERO,
        },
        &opts(),
        None,
        Some(o.sign_key),
    )
}

#[test]
fn guest_pairs_via_pipe() {
    let r = TestRelay::start();
    let desk = Desk::new(&r, Duration::from_secs(600));
    let secret = PairSecret::generate().unwrap();
    let (offer, host) = show_qr(&r, &desk, &secret);
    let me = contract::keys();
    let chain = phone(&offer, &me).unwrap();
    let req = host.join().unwrap().unwrap();
    assert_eq!(req.sign_key, me.sign_key());
    let m = chain.head().member(&me.sign_key()).unwrap();
    assert_eq!((m.role, m.noise_key), (Role::Mobile, me.noise_key()));
    assert_eq!(chain.head(), desk.ring.lock().unwrap().chain.head());
    // The phone is a member now: it connects like any device.
    let (c, _) = contract::connect(&r.target(), &chain, me.clone());
    assert!(!c.is_closed());
}

#[test]
fn enrollment_evidence_retains_joined_head_when_fetched_chain_is_newer() {
    let r = TestRelay::start();
    let mut desk = Desk::new(&r, Duration::from_secs(60));
    Arc::get_mut(&mut desk).unwrap().advance_after_join = true;
    let secret = PairSecret::generate().unwrap();
    let (offer, host) = show_qr(&r, &desk, &secret);
    let (chain, joined) = pair_as_guest_pinned_with_joined(
        &GuestRequest {
            relay_url: &offer.relay_url,
            secret: &offer.secret,
            pin: Some(offer.noise_key),
            ring_id: Some(&offer.ring_id),
            keys: contract::keys(),
            role: Role::Mobile,
            name: "phone",
            wait: Duration::ZERO,
        },
        &opts(),
        None,
        Some(offer.sign_key),
    )
    .unwrap();
    host.join().unwrap().unwrap();
    assert_eq!(joined.version, 3);
    assert_eq!(chain.head().version(), 4);
    assert_eq!(chain.get(joined.version).unwrap().hash(), joined.hash);
}

#[test]
fn computer_pairs_with_a_code() {
    let r = TestRelay::start();
    let desk = Desk::new(&r, Duration::from_secs(600));
    let code = PairCode::generate().unwrap();
    let typed = code.format().to_lowercase();
    let me = contract::keys();
    let url = r.url();
    let (me2, code2) = (me.clone(), code.clone());
    // `xshelld pair` waits on the slot first …
    let guest = std::thread::spawn(move || {
        let secret = code2.secret();
        pair_as_guest(
            &GuestRequest {
                relay_url: &url,
                secret: &secret,
                pin: None,
                ring_id: None,
                keys: me2,
                role: Role::Daemon,
                name: "host",
                wait: Duration::from_secs(10),
            },
            &opts(),
            None,
        )
    });
    // … and the Desktop joins it once the code is typed.
    let secret = PairCode::parse(&typed).unwrap().secret();
    // (The guest is on the slot by the time anyone has typed its code.)
    std::thread::sleep(Duration::from_millis(300));
    let pipe = PairPipe::open(&r.url(), &secret.slot(), &opts()).unwrap();
    assert!(pipe.has_peer());
    let ring = desk.ring.lock().unwrap();
    let (keys, ring_id) = (ring.desktop.clone(), ring.ring_id());
    drop(ring);
    let req = host_pairing(
        pipe,
        &HostRequest {
            keys: &keys,
            secret: &secret,
            ring_id: &ring_id,
            name: "desk",
            role: Role::Daemon,
        },
        &*desk,
        None,
    )
    .unwrap();
    assert_eq!(req.role, Role::Daemon);
    let chain = guest.join().unwrap().unwrap();
    assert_eq!(
        chain.head().member(&me.sign_key()).unwrap().role,
        Role::Daemon
    );
}

#[test]
fn expired_secret_fails() {
    let r = TestRelay::start();
    let desk = Desk::new(&r, Duration::ZERO);
    let secret = PairSecret::generate().unwrap();
    let (offer, host) = show_qr(&r, &desk, &secret);
    let me = contract::keys();
    assert_eq!(
        phone(&offer, &me).unwrap_err(),
        PairError::Refused(PairRefusal::Expired)
    );
    assert!(host.join().unwrap().is_err());
    assert_eq!(
        desk.ring.lock().unwrap().chain.head().version(),
        2,
        "nothing added"
    );
}

#[test]
fn reused_secret_fails_even_with_lax_relay() {
    // A Relay that lets a slot be met again: the Desktop's own bookkeeping refuses.
    let r = TestRelay::start_with(TestRelayOptions {
        lax_pairing: true,
        ..TestRelayOptions::default()
    });
    let desk = Desk::new(&r, Duration::from_secs(600));
    let secret = PairSecret::generate().unwrap();
    let (offer, host) = show_qr(&r, &desk, &secret);
    phone(&offer, &contract::keys()).unwrap();
    host.join().unwrap().unwrap();
    let (offer, host) = show_qr(&r, &desk, &secret);
    let second = contract::keys();
    assert_eq!(
        phone(&offer, &second).unwrap_err(),
        PairError::Refused(PairRefusal::Used)
    );
    assert!(host.join().unwrap().is_err());
    let head = desk.ring.lock().unwrap().chain.head().clone();
    assert!(head.member(&second.sign_key()).is_none());
}

#[test]
fn wrong_code_finds_nobody() {
    let r = TestRelay::start();
    let code = PairCode::generate().unwrap();
    let url = r.url();
    let c2 = code.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let guest = std::thread::spawn(move || {
        let secret = c2.secret();
        pair_as_guest(
            &GuestRequest {
                relay_url: &url,
                secret: &secret,
                pin: None,
                ring_id: None,
                keys: contract::keys(),
                role: Role::Daemon,
                name: "host",
                wait: Duration::from_secs(10),
            },
            &opts(),
            Some(&s2),
        )
    });
    std::thread::sleep(Duration::from_millis(200));
    // One character off: another slot, where nobody waits.
    let mut typed: Vec<char> = code.format().chars().collect();
    typed[0] = if typed[0] == '0' { '1' } else { '0' };
    let wrong = PairCode::parse(&typed.iter().collect::<String>()).unwrap();
    let p = PairPipe::open(&r.url(), &wrong.secret().slot(), &opts()).unwrap();
    assert!(!p.has_peer(), "nobody waits on a wrong code's slot");
    stop.store(true, Ordering::SeqCst);
    assert_eq!(guest.join().unwrap().unwrap_err(), PairError::Cancelled);
    // A phone whose Desktop is gone fails at once.
    let desk = Desk::new(&r, Duration::from_secs(600));
    let ring = desk.ring.lock().unwrap();
    let offer = PairingOffer {
        ring_id: ring.ring_id(),
        relay_url: r.url(),
        sign_key: ring.desktop.sign_key(),
        noise_key: ring.desktop.noise_key(),
        secret: PairSecret::generate().unwrap(),
        expires_at: 0,
        head: None,
    };
    drop(ring);
    assert_eq!(
        phone(&offer, &contract::keys()).unwrap_err(),
        PairError::NotFound
    );
}

#[test]
fn guest_waiting_for_a_code_expires() {
    let r = TestRelay::start();
    let secret = PairCode::generate().unwrap().secret();
    let e = pair_as_guest(
        &GuestRequest {
            relay_url: &r.url(),
            secret: &secret,
            pin: None,
            ring_id: None,
            keys: contract::keys(),
            role: Role::Daemon,
            name: "host",
            wait: Duration::from_millis(300),
        },
        &opts(),
        None,
    )
    .unwrap_err();
    assert_eq!(e, PairError::Expired);
}

#[test]
fn fetch_chain_pins_head() {
    let r = TestRelay::start();
    let mut ring = TestRing::new(&r.url());
    let (desk, _) = contract::connect(&r.target(), &ring.chain, ring.desktop.clone());
    let me = contract::keys();
    let next = ring.next(|d| d.add(contract::member(&me, "phone", Role::Mobile)));
    desk.publish_roster(&next).unwrap();
    let signer: Arc<dyn Signer> = me.clone();
    let fetch = |pin: ChainPin| {
        RingClient::fetch_chain(
            &r.url(),
            &ring.ring_id(),
            signer.clone(),
            &pin,
            None,
            RingTimeouts::default(),
        )
    };
    let chain = fetch(ChainPin {
        version: 3,
        hash: next.hash(),
    })
    .unwrap();
    assert_eq!(chain, ring.chain);
    // Another hash for that version (a fork) fails the pin.
    assert!(matches!(
        fetch(ChainPin {
            version: 3,
            hash: ring.chain.get(2).unwrap().hash(),
        }),
        Err(RingError::Invalid(_))
    ));
    // A device not in the head is refused by the Relay.
    let stranger: Arc<dyn Signer> = contract::keys();
    assert!(RingClient::fetch_chain(
        &r.url(),
        &ring.ring_id(),
        stranger,
        &ChainPin {
            version: 3,
            hash: next.hash(),
        },
        None,
        RingTimeouts::default(),
    )
    .is_err());
}

#[test]
fn fetch_chain_refuses_hidden_head() {
    // The Desktop says it added the phone in version 4, but the Relay holds only version 3
    // (it hid the newer one, or never got it): the pin fails.
    let r = TestRelay::start();
    let mut ring = TestRing::new(&r.url());
    let (desk, _) = contract::connect(&r.target(), &ring.chain, ring.desktop.clone());
    let me = contract::keys();
    let v3 = ring.next(|d| d.add(contract::member(&me, "phone", Role::Mobile)));
    desk.publish_roster(&v3).unwrap();
    let v4 = ring.next(|d| d.relay_url = "wss://other.example".into());
    let signer: Arc<dyn Signer> = me.clone();
    let r = RingClient::fetch_chain(
        &r.url(),
        &ring.ring_id(),
        signer,
        &ChainPin {
            version: 4,
            hash: v4.hash(),
        },
        None,
        RingTimeouts::default(),
    );
    assert!(matches!(r, Err(RingError::Invalid(_))), "{r:?}");
}

#[test]
fn message_four_redirect_is_rejected_before_chain_connection() {
    let r = TestRelay::start();
    let forbidden = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    forbidden.set_nonblocking(true).unwrap();
    let mut desk = Desk::new(&r, Duration::from_secs(600));
    Arc::get_mut(&mut desk).unwrap().reply_relay =
        Some(format!("ws://{}", forbidden.local_addr().unwrap()));
    let secret = PairSecret::generate().unwrap();
    let (offer, host) = show_qr(&r, &desk, &secret);
    assert!(phone(&offer, &contract::keys()).is_err());
    host.join().unwrap().unwrap();
    assert!(matches!(forbidden.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn valid_noise_wrong_approver_is_rejected_before_chain_fetch() {
    let r = TestRelay::start();
    let desk = Desk::new(&r, Duration::from_secs(600));
    let secret = PairSecret::from_bytes(&[5; 32]);
    let (mut offer, host) = show_qr(&r, &desk, &secret);
    offer.sign_key = DeviceKeys::from_seeds(&[91; 32], &[92; 32]).sign_key();
    let gets = r.roster_gets();
    assert!(matches!(
        phone(&offer, &contract::keys()),
        Err(PairError::Pin)
    ));
    host.join().unwrap().unwrap();
    assert_eq!(r.roster_gets(), gets);
}

// #87 BINDING AMENDMENT 1: every QR anchor of a v2 offer is checked against an otherwise
// valid, structurally verified Mobile proposal before the phone consents.
mod offer_anchor {
    use super::*;
    use xshell_protocol::ring::b64;
    use xshell_protocol::ring::membership::{
        Enrollment, EnrollmentKind, Membership, RingPolicy, RosterChange,
    };
    use xshell_protocol::ring::pairing::adoption::{
        enroll_proposal, AdoptionIntent, JoinTarget, PublicIdentity,
    };
    use xshell_protocol::ring::pairing::OfferHead;
    use xshell_protocol::ring::relay::pair::{
        adopt_mobile_as_guest_with_ready, check_offer_anchor, MobileAdoptionRequest,
    };
    use xshell_protocol::ring::relay::proposal::ProposalHandle;
    use xshell_protocol::ring::relay::VerifiedProposal;
    use xshell_protocol::ring::{RosterChain, SignedRoster};

    fn keys(n: u8) -> DeviceKeys {
        DeviceKeys::from_seeds(&[n; 32], &[n + 100; 32])
    }
    fn fixture() -> (DeviceKeys, VerifiedProposal, PairingOffer) {
        let a = keys(1);
        let relay = "wss://relay.example.com";
        let g = SignedRoster::genesis(&a, a.noise_key(), "desk", relay, 100).unwrap();
        let mut r = g.roster().clone();
        r.v = 2;
        r.version = 2;
        r.prev = Some(g.hash());
        r.issued_at = 101;
        r.set_membership(&Membership {
            enrollments: vec![Enrollment {
                id: b64::encode(&[1; 32]),
                kind: EnrollmentKind::LegacyDesktop,
                members: vec![a.sign_key()],
                enrolled_at: 101,
            }],
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: Some(2_592_000),
                activity_key: keys(9).sign_key(),
            },
            change: RosterChange::Upgrade { proofs: vec![] },
        });
        let chain = RosterChain::from_chain(vec![g, r.sign(&a).unwrap()]).unwrap();
        let phone = keys(2);
        let intent = AdoptionIntent {
            operation: b64::encode(&[7; 32]),
            expires_at: 400,
            target: JoinTarget::Mobile {
                phone: PublicIdentity {
                    sign_key: phone.sign_key(),
                    noise_key: phone.noise_key(),
                    name: "phone".into(),
                },
            },
        };
        let proposal = enroll_proposal(chain.head(), a.sign_key(), &intent, 102).unwrap();
        let handle = ProposalHandle {
            ring_id: chain.ring_id().clone(),
            relay_url: relay.into(),
            operation: intent.operation.clone(),
            head: chain.head().hash(),
            version: chain.head().version(),
            guest: phone.sign_key(),
            context: b64::encode(&[4; 32]),
            expires_at: 400,
            grant: b64::encode(&[5; 32]),
            resource: b64::encode(&[6; 32]),
        };
        let offer = PairingOffer {
            ring_id: chain.ring_id().clone(),
            relay_url: relay.into(),
            sign_key: a.sign_key(),
            noise_key: a.noise_key(),
            secret: PairSecret::from_bytes(&[3; 32]),
            expires_at: 400,
            head: Some(OfferHead {
                version: chain.head().version(),
                hash: chain.head().hash(),
            }),
        };
        (
            a,
            VerifiedProposal {
                chain,
                proposal,
                handle,
            },
            offer,
        )
    }

    #[test]
    fn each_mismatched_anchor_refuses_an_otherwise_valid_proposal() {
        let (_, verified, offer) = fixture();
        check_offer_anchor(&offer, &verified).unwrap();
        let other = keys(50);
        type Mutation = Box<dyn Fn(&mut PairingOffer)>;
        let mutations: Vec<(&str, Mutation)> = vec![
            ("no anchor (v1)", Box::new(|o| o.head = None)),
            (
                "head version",
                Box::new(|o| o.head.as_mut().unwrap().version += 1),
            ),
            (
                "head hash",
                Box::new(|o| o.head.as_mut().unwrap().hash = b64::encode(&[8; 32])),
            ),
            (
                "ring",
                Box::new(|o| {
                    o.ring_id = xshell_protocol::ring::RingId::derive(&keys(51).sign_key())
                }),
            ),
            (
                "relay",
                Box::new(|o| o.relay_url = "wss://other.example.com".into()),
            ),
            (
                "approver signing key",
                Box::new(move |o| o.sign_key = other.sign_key()),
            ),
            (
                "approver noise key",
                Box::new(|o| o.noise_key = keys(52).noise_key()),
            ),
        ];
        for (what, mutate) in mutations {
            let mut o = offer.clone();
            mutate(&mut o);
            assert_eq!(
                check_offer_anchor(&o, &verified),
                Err(PairError::Pin),
                "{what}"
            );
        }
    }

    #[test]
    fn a_v1_offer_is_not_a_v2_phone_enrollment() {
        let (_, _, mut offer) = fixture();
        offer.head = None;
        let r = adopt_mobile_as_guest_with_ready(
            &MobileAdoptionRequest {
                offer: &offer,
                keys: Arc::new(keys(2)),
                name: "phone",
                until: Instant::now() + Duration::from_secs(5),
            },
            &opts(),
            None,
            || panic!("no socket for a v1 offer"),
            |_| panic!("no consent for a v1 offer"),
        );
        assert!(matches!(r, Err(PairError::Invalid(_))));
    }
}

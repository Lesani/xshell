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
    host_pairing, pair_as_guest, GuestRequest, HostRequest, PairOptions, PairPipe, PairingHost,
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
        Ok(Joined {
            ring_id: next.ring_id().clone(),
            relay_url: next.roster().relay_url.clone(),
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
    pair_as_guest(
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

//! Pairwise Noise sessions over the test Relay: a Mobile opens a session to a Daemon that
//! echoes, and a hostile Relay that flips, replays, reorders, drops or injects envelopes
//! never gets anything past the session (it dies instead).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use xshell_protocol::ring::noise::{Header, Kind};
use xshell_protocol::ring::relay::contract::{self, TestRing, WAIT};
use xshell_protocol::ring::relay::sessions::{
    Incoming, SessionEvents, SessionStream, Sessions, SessionsConfig,
};
use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions, Verdict};
use xshell_protocol::ring::relay::wire::{ErrorCode, RelayFrame};
use xshell_protocol::ring::relay::{Connector, ConnectorConfig, LinkState, RingTimeouts};
use xshell_protocol::ring::{b64, DeviceKeys, Role};

fn relay() -> TestRelay {
    TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        ..TestRelayOptions::default()
    })
}

struct Side {
    sessions: Sessions,
    connector: Arc<Connector>,
    accepted: Arc<Mutex<Vec<SessionStream>>>,
}

fn cfg(keys: &Arc<DeviceKeys>) -> SessionsConfig {
    let mut c = SessionsConfig::new(keys.clone());
    c.hs_timeout = Duration::from_millis(500);
    c.open_timeout = Duration::from_secs(5);
    c.write_stall = Duration::from_secs(2);
    c
}

/// A device on the Relay; `echo`: accepts sessions and echoes what it reads.
/// What a side does with the sessions it accepts.
#[derive(Clone, Copy, PartialEq)]
enum Accept {
    None,
    /// Echoes what it reads.
    Echo,
    /// Keeps the stream and reads nothing.
    Hold,
    /// Reads and drops everything.
    Drain,
}

fn side(r: &TestRelay, ring: &TestRing, keys: &Arc<DeviceKeys>, echo: bool) -> Side {
    side_with(
        r,
        ring,
        keys,
        if echo { Accept::Echo } else { Accept::None },
    )
}

fn side_with(r: &TestRelay, ring: &TestRing, keys: &Arc<DeviceKeys>, mode: Accept) -> Side {
    let accepted: Arc<Mutex<Vec<SessionStream>>> = Arc::default();
    let acc = accepted.clone();
    let accept: Option<Arc<dyn Fn(Incoming) + Send + Sync>> = if mode != Accept::None {
        Some(Arc::new(move |inc: Incoming| {
            let mut s = inc.stream;
            acc.lock().unwrap().push(s.try_clone());
            if mode == Accept::Hold {
                return;
            }
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 100_000];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if mode == Accept::Echo && s.write_all(&buf[..n]).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }))
    } else {
        None
    };
    let sessions = Sessions::new(cfg(keys), accept);
    let mut client = contract::config(&r.target(), &ring.chain, keys.clone());
    client.timeouts = RingTimeouts {
        connect: Duration::from_secs(5),
        request: Duration::from_secs(2),
        ..RingTimeouts::default()
    };
    let mut cc = ConnectorConfig::new(client);
    cc.backoff_unit = Duration::from_millis(10);
    let connector = Arc::new(
        Connector::start(
            cc,
            Arc::new(SessionEvents {
                sessions: sessions.clone(),
                next: None,
            }),
        )
        .unwrap(),
    );
    sessions.attach(&connector);
    let deadline = Instant::now() + WAIT;
    while connector.state() != (LinkState::Connected { limited: false }) {
        assert!(Instant::now() < deadline, "connected");
        std::thread::sleep(Duration::from_millis(10));
    }
    Side {
        sessions,
        connector,
        accepted,
    }
}

fn echo_round(s: &mut SessionStream, data: &[u8]) {
    s.write_all(data).unwrap();
    let mut got = vec![0u8; data.len()];
    s.read_exact(&mut got).unwrap();
    assert_eq!(got, data);
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sessions_round_trip_over_test_relay() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let daemon = side(&r, &ring, &ring.daemon, true);
    let mobile = side(&r, &ring, &ring.mobile, false);
    let mut s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    assert_eq!(s.role(), Role::Daemon);
    echo_round(&mut s, b"hello");
    // Larger than one envelope: split and joined in order.
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    echo_round(&mut s, &big);
    let acc = daemon.accepted.lock().unwrap()[0].try_clone();
    assert_eq!(acc.role(), Role::Mobile);
    assert_eq!(acc.peer(), ring.mobile.sign_key());
    // A close reaches the other side, inside the encryption.
    s.close("done");
    assert!(acc.wait_closed(WAIT));
    assert_eq!(acc.ended().as_deref(), Some("done"));
    // A Desktop gets a desktop session.
    let desk = side(&r, &ring, &ring.desktop, false);
    let mut d = desk.sessions.open(&ring.daemon.sign_key()).unwrap();
    echo_round(&mut d, b"x");
    assert_eq!(daemon.accepted.lock().unwrap()[1].role(), Role::Desktop);
}

#[test]
fn daemon_role_peer_is_refused() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let _desk = side(&r, &ring, &ring.desktop, true);
    let daemon = side(&r, &ring, &ring.daemon, false);
    match daemon.sessions.open(&ring.desktop.sign_key()) {
        Err(e) => assert!(e.to_string().contains("forbidden"), "{e}"),
        Ok(_) => panic!("a daemon opened a session"),
    }
}

/// Opens a session from the Mobile and sets a tamper on what the Daemon receives.
fn tampered(
    f: impl FnMut(&xshell_protocol::ring::SignKey, Vec<u8>) -> Verdict + Send + 'static,
) -> (TestRelay, TestRing, Side, Side, SessionStream) {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let daemon = side(&r, &ring, &ring.daemon, true);
    let mobile = side(&r, &ring, &ring.mobile, false);
    let mut s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    echo_round(&mut s, b"before");
    r.tamper(&ring.ring_id(), &ring.daemon.sign_key(), Some(Box::new(f)));
    (r, ring, daemon, mobile, s)
}

fn is_data(p: &[u8]) -> bool {
    Header::parse(p).is_ok_and(|(h, _)| h.kind == Kind::Data)
}

fn daemon_session_dies(daemon: &Side) {
    let acc = daemon.accepted.lock().unwrap()[0].try_clone();
    assert!(acc.wait_closed(WAIT), "the daemon's session lives on");
    assert!(daemon.sessions.peers().is_empty());
}

#[test]
fn tamper_flip_replay_reorder_inject_rejected() {
    // A flipped bit.
    let (_r, _ring, daemon, _m, mut s) = tampered(|_, mut p| {
        if is_data(&p) {
            let last = p.len() - 1;
            p[last] ^= 1;
        }
        Verdict::Deliver(vec![p])
    });
    s.write_all(b"flipped").unwrap();
    daemon_session_dies(&daemon);

    // A replay (each message twice).
    let (_r, _ring, daemon, _m, mut s) = tampered(|_, p| Verdict::Deliver(vec![p.clone(), p]));
    s.write_all(b"twice").unwrap();
    daemon_session_dies(&daemon);

    // A reorder: hold one message back and deliver it after the next.
    let held: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
    let h = held.clone();
    let (_r, _ring, daemon, _m, mut s) = tampered(move |_, p| {
        let mut held = h.lock().unwrap();
        match held.take() {
            None => {
                *held = Some(p);
                Verdict::Deliver(vec![])
            }
            Some(first) => Verdict::Deliver(vec![p, first]),
        }
    });
    s.write_all(b"one").unwrap();
    s.write_all(b"two").unwrap();
    daemon_session_dies(&daemon);

    // An injected message: right header, wrong ciphertext.
    let (_r, _ring, daemon, _m, mut s) = tampered(|_, p| {
        let mut forged = p.clone();
        for b in &mut forged[26..] {
            *b = 0x41;
        }
        Verdict::Deliver(vec![forged, p])
    });
    s.write_all(b"genuine").unwrap();
    daemon_session_dies(&daemon);

    // A dropped message: the next one is out of order.
    let n = Arc::new(Mutex::new(0));
    let n2 = n.clone();
    let (_r, _ring, daemon, _m, mut s) = tampered(move |_, p| {
        let mut n = n2.lock().unwrap();
        *n += 1;
        if *n == 1 {
            Verdict::Deliver(vec![])
        } else {
            Verdict::Deliver(vec![p])
        }
    });
    s.write_all(b"lost").unwrap();
    s.write_all(b"next").unwrap();
    daemon_session_dies(&daemon);
}

#[test]
fn refused_final_message_kills_the_session() {
    // The Relay refuses the last message (a quota refusal, say) and nothing follows: the
    // sender's stream must fail at once, not wait for traffic that never comes.
    let (_r, _ring, _daemon, _mobile, mut s) = tampered(|_, _| Verdict::Refuse(ErrorCode::Quota));
    let _ = s.write_all(b"last words");
    assert!(s.wait_closed(WAIT), "the session lives on");
    let mut buf = [0u8; 8];
    assert!(s.read(&mut buf).is_err());
    assert!(s.write_all(b"more").is_err());
}

#[test]
fn relay_never_sees_plaintext_marker() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let _daemon = side(&r, &ring, &ring.daemon, true);
    let mobile = side(&r, &ring, &ring.mobile, false);
    r.record_payloads(true);
    let mut s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    let marker = b"PLAINTEXT-MARKER-6f1c2a";
    echo_round(&mut s, marker);
    let seen = r.recorded_payloads();
    assert!(seen.len() >= 4, "{} payloads", seen.len());
    for p in &seen {
        assert!(
            !p.windows(marker.len()).any(|w| w == marker),
            "the relay saw plaintext"
        );
        // The sign keys travel in the prologue, never in a message.
        let k = ring.mobile.sign_key().to_b64();
        assert!(!p.windows(k.len()).any(|w| w == k.as_bytes()));
    }
}

#[test]
fn session_replaced_on_reconnect() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let daemon = side(&r, &ring, &ring.daemon, true);
    let mobile = side(&r, &ring, &ring.mobile, false);
    let mut a = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    echo_round(&mut a, b"a");
    let mut b = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    echo_round(&mut b, b"b");
    let first = daemon.accepted.lock().unwrap()[0].try_clone();
    assert!(first.wait_closed(WAIT));
    assert!(!a.is_open(), "the initiator replaced its own session too");
    assert_eq!(daemon.sessions.peers(), vec![ring.mobile.sign_key()]);
    // A Relay reconnect ends every session (envelopes may have been lost).
    r.kick(&ring.ring_id(), &ring.daemon.sign_key(), 1011);
    assert!(
        b.wait_closed(WAIT) || {
            let mut buf = [0u8; 1];
            let _ = b.write_all(b"x");
            b.read(&mut buf).is_err()
        }
    );
    wait_until("the daemon's sessions ended", || {
        daemon.sessions.peers().is_empty()
    });
    let _ = &daemon.connector;
}

#[test]
fn replayed_hs1_cannot_replace_live() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let daemon = side(&r, &ring, &ring.daemon, true);
    let mobile = side(&r, &ring, &ring.mobile, false);
    let hs1: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
    let h = hs1.clone();
    r.tamper(
        &ring.ring_id(),
        &ring.daemon.sign_key(),
        Some(Box::new(move |_, p| {
            if Header::parse(&p).is_ok_and(|(h, _)| h.kind == Kind::Hs1) {
                *h.lock().unwrap() = Some(p.clone());
            }
            Verdict::Deliver(vec![p])
        })),
    );
    let mut s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    echo_round(&mut s, b"live");
    let replay = hs1.lock().unwrap().clone().expect("captured HS1");
    let frame = RelayFrame::Env {
        from: ring.mobile.sign_key(),
        payload: b64::encode(&replay),
    }
    .encode();
    assert!(r.inject(&ring.ring_id(), &ring.daemon.sign_key(), &frame));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(daemon.accepted.lock().unwrap().len(), 1, "no new session");
    echo_round(&mut s, b"still live");
}

#[test]
fn removal_and_presence_end_sessions() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let daemon = side(&r, &ring, &ring.daemon, true);
    let mobile = side(&r, &ring, &ring.mobile, false);
    let mut s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    echo_round(&mut s, b"x");
    // The Mobile drops off the Relay: its presence ends the Daemon's session.
    mobile.connector.abandon();
    let acc = daemon.accepted.lock().unwrap()[0].try_clone();
    assert!(acc.wait_closed(WAIT));
    // A Desktop removes the Mobile; the Daemon's next sweep ends a new session at once.
    let mobile = side(&r, &ring, &ring.mobile, false);
    let mut s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    echo_round(&mut s, b"y");
    let mkey = ring.mobile.sign_key();
    let next = ring.next(|d| {
        d.remove(&mkey);
    });
    let (desk, _) = contract::connect(&r.target(), &ring.up_to(2), ring.desktop.clone());
    desk.publish_roster(&next).unwrap();
    let acc = daemon.accepted.lock().unwrap()[1].try_clone();
    // Removed (the sweep) or offline (the Relay disconnects it): either way, it ends.
    assert!(acc.wait_closed(WAIT));
}

#[test]
fn a_killed_session_delivers_nothing_queued() {
    // Messages wait unread in the Daemon's queue; the session dies before they are read:
    // none of them may come out.
    let r = relay();
    let ring = TestRing::new(&r.url());
    let daemon = side_with(&r, &ring, &ring.daemon, Accept::Hold);
    let mobile = side(&r, &ring, &ring.mobile, false);
    let mut s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
    for _ in 0..5 {
        s.write_all(b"queued command\n").unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    mobile.connector.abandon();
    let mut acc = daemon.accepted.lock().unwrap()[0].try_clone();
    assert!(acc.wait_closed(WAIT));
    let mut buf = [0u8; 64];
    assert!(
        acc.read(&mut buf).is_err(),
        "queued bytes came out of a killed session"
    );
}

#[test]
fn close_is_the_last_message_in_nonce_order() {
    // Writes race a close: the peer sees every message in order and then the close, never
    // a message out of order (which would kill the session instead).
    for _ in 0..5 {
        let r = relay();
        let ring = TestRing::new(&r.url());
        let daemon = side_with(&r, &ring, &ring.daemon, Accept::Drain);
        let mobile = side(&r, &ring, &ring.mobile, false);
        let s = mobile.sessions.open(&ring.daemon.sign_key()).unwrap();
        let mut w = s.try_clone();
        let writer = std::thread::spawn(move || {
            // Paced: the receiver's queue is bounded (64 messages), and overflowing it kills
            // the session, which is not what this test is about.
            while w.write_all(b"spam spam spam").is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        s.close("done");
        writer.join().unwrap();
        let acc = daemon.accepted.lock().unwrap()[0].try_clone();
        assert!(acc.wait_closed(WAIT));
        assert_eq!(acc.ended().as_deref(), Some("done"));
    }
}

#[test]
fn answer_delayed_across_a_key_replacement_never_becomes_a_session() {
    // The Daemon's answer is held back while a new Roster version gives the Daemon another
    // Noise key: the phone, having adopted it, must not install the session the old key
    // answered for.
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let _daemon = side(&r, &ring, &ring.daemon, true);
    let mobile = side(&r, &ring, &ring.mobile, false);
    let held: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let h = held.clone();
    r.tamper(
        &ring.ring_id(),
        &ring.mobile.sign_key(),
        Some(Box::new(move |_, p| {
            if Header::parse(&p).is_ok_and(|(h, _)| h.kind == Kind::Hs2) {
                h.lock().unwrap().push(p);
                return Verdict::Deliver(vec![]);
            }
            Verdict::Deliver(vec![p])
        })),
    );
    let sessions = mobile.sessions.clone();
    let dkey = ring.daemon.sign_key();
    let opener = std::thread::spawn(move || sessions.open(&dkey));
    wait_until("an answer is held", || !held.lock().unwrap().is_empty());
    let other = DeviceKeys::generate().unwrap();
    let next = ring.next(|d| {
        let m = d.members.iter_mut().find(|m| m.sign_key == dkey).unwrap();
        m.noise_key = other.noise_key();
    });
    let (desk, _) = contract::connect(&r.target(), &ring.up_to(2), ring.desktop.clone());
    desk.publish_roster(&next).unwrap();
    wait_until("the phone adopted the new head", || {
        mobile.connector.chain().head().version() == 3
    });
    for p in held.lock().unwrap().drain(..) {
        let frame = RelayFrame::Env {
            from: dkey,
            payload: b64::encode(&p),
        }
        .encode();
        r.inject(&ring.ring_id(), &ring.mobile.sign_key(), &frame);
    }
    assert!(
        opener.join().unwrap().is_err(),
        "a session with the replaced key"
    );
    assert!(mobile.sessions.peers().is_empty());
}

/// The adapter passes the Connector's entitlement reports on to `next`: the slot from
/// `welcome` after `Connected`, then every broadcast.
#[test]
fn session_events_forward_entitlement_to_next() {
    use xshell_protocol::ring::entitlement::{sign_entitlement, GatewayKeys, Tier};
    use xshell_protocol::ring::relay::{ConnectorEvents, LinkState};

    #[derive(Default)]
    struct Seen(Mutex<Vec<Option<String>>>);
    impl ConnectorEvents for Seen {
        fn entitlement(&self, token: Option<&str>) {
            self.0.lock().unwrap().push(token.map(str::to_string));
        }
    }

    let gw = contract::keys();
    let r = TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        hosted: Some(GatewayKeys::new(&[gw.sign_key()])),
        ..TestRelayOptions::default()
    });
    let ring = TestRing::new(&r.url());
    let tok = |exp: u64| {
        sign_entitlement(
            &*gw,
            &ring.ring_id(),
            Tier::Hosted,
            "p",
            contract::now(),
            exp,
        )
        .unwrap()
    };
    let (desktop, _) = contract::connect(&r.target(), &ring.chain, ring.desktop.clone());
    let first = tok(contract::now() + 3600);
    desktop.put_entitlement(&first).expect("put");

    let seen = Arc::new(Seen::default());
    let mut client = contract::config(&r.target(), &ring.chain, ring.mobile.clone());
    client.timeouts.request = Duration::from_secs(2);
    let sessions = Sessions::new(cfg(&ring.mobile), None);
    let connector = Arc::new(
        Connector::start(
            ConnectorConfig::new(client),
            Arc::new(SessionEvents {
                sessions: sessions.clone(),
                next: Some(seen.clone()),
            }),
        )
        .unwrap(),
    );
    sessions.attach(&connector);
    let has = |t: &str| {
        seen.0
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.as_deref() == Some(t))
    };
    wait_until("the welcome slot", || has(&first));
    assert_eq!(connector.state(), LinkState::Connected { limited: false });

    let second = tok(contract::now() + 7200);
    desktop.put_entitlement(&second).expect("put");
    wait_until("the broadcast", || has(&second));
    assert_eq!(connector.entitlement().as_deref(), Some(second.as_str()));
    connector.abandon();
}

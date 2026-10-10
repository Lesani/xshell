//! The Ring client against the in-process test Relay: the contract scenarios (which
//! xshell-remote also runs against the real Worker), then what only a local Relay can
//! simulate: a malicious Relay, and hostile transports over TLS.

use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_protocol::ring::b64;
use xshell_protocol::ring::entitlement::GatewayKeys;
use xshell_protocol::ring::relay::contract::{self, *};
use xshell_protocol::ring::relay::test_relay::{
    Fault, TestRelay, TestRelayOptions, TestTls, TrickleProxy,
};
use xshell_protocol::ring::relay::wire::{
    ByeReason, CloseReason, ErrorCode, MemberPresence, PairRelayFrame, Presence, RelayFrame,
    HOSTED_QUOTA_FRAMES_PER_DAY,
};
use xshell_protocol::ring::relay::{RingClient, RingLimits, RingTimeouts};
use xshell_protocol::ring::{RingError, Role, RosterError, Signer};

fn relay() -> TestRelay {
    TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        ..TestRelayOptions::default()
    })
}

macro_rules! scenario {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            let r = relay();
            contract::$name(&r.target());
        }
    )*};
}

scenario!(
    member_auth_succeeds,
    non_member_auth_fails,
    forged_auth_signature_fails,
    failing_signer_fails_auth,
    signature_for_other_origin_fails,
    genesis_upload_creates_ring,
    genesis_from_underived_key_is_refused,
    non_member_cannot_seed_ring,
    member_added_later_supplies_extension,
    envelope_reaches_only_addressee_in_same_ring,
    envelope_from_is_stamped,
    oversize_envelope_is_refused,
    oversize_or_malformed_control_frame_closes,
    envelope_to_offline_member_reports_offline,
    presence_online,
    bye_reports_xshell_closed,
    drop_without_bye_reports_unreachable,
    unseen_member_is_never_connected,
    client_lists_online_members,
    roster_put_is_broadcast_and_accepted,
    relay_refuses_stale_or_mobile_signed_put,
    removed_member_is_disconnected,
    removed_member_is_unknown_recipient,
    auth_chain_signed_by_mobile_is_refused,
    client_syncs_newer_roster_on_welcome,
    entitlement_slot_round_trips,
    entitlement_slot_keeps_the_latest_put,
    auth_timeout_closes,
    replaced_socket_close_keeps_new_online,
    stale_bye_after_replacement_is_ignored,
    unknown_client_type_is_answered_and_tolerated,
    binary_frame_is_unsupported,
    readded_member_keeps_new_socket_online,
    pair_pipe_joins_two,
    pair_pipe_refuses_third,
    pair_pipe_caps_messages,
    pair_pipe_closes_peer,
    state_foreground_reaches_presence,
    state_foreground_clears_on_bye_and_drop,
    state_refused_from_non_mobile,
    push_unavailable_without_gateway,
);

/// A Relay forwarding to a fake gateway of its own, so scripted replies never cross tests.
fn push_relay() -> (TestRelay, Arc<FakeGateway>) {
    let gw = Arc::new(FakeGateway::start());
    let r = TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        push_gateway: Some(gw.url()),
        push_timeout: Duration::from_secs(2),
        ..TestRelayOptions::default()
    });
    (r, gw)
}

macro_rules! push_scenario {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            let (r, gw) = push_relay();
            let mut t = r.target();
            t.fake_gateway = Some(gw);
            contract::$name(&t);
        }
    )*};
}

push_scenario!(
    push_forwards_exactly_once_verbatim,
    push_refused_while_a_mobile_is_foreground,
    push_refused_from_non_daemon,
    push_relays_gateway_refusal_detail,
    push_bad_fields_refused,
);

#[test]
fn state_foreground_lease_expires() {
    let r = TestRelay::start_with(TestRelayOptions {
        foreground_lease: Duration::from_millis(1500),
        ..TestRelayOptions::default()
    });
    contract::state_foreground_lease_expires(&r.target());
}

#[test]
fn every_scenario_is_listed_and_run_here() {
    // The macros above and the lists must not drift apart.
    assert_eq!(SCENARIOS.len(), 40);
    assert_eq!(PUSH_SCENARIOS.len(), 5);
    assert_eq!(PUSH_UNAVAILABLE_SCENARIOS.len(), 1);
    assert_eq!(FOREGROUND_LEASE_SCENARIOS.len(), 1);
}

#[test]
fn push_gateway_timeout_is_unreachable() {
    let (r, gw) = push_relay();
    gw.set_delay(Duration::from_secs(3));
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (d, _) = connect(&t, &ring.chain, ring.daemon.clone());
    match d.push(&contract::push_request(), WAIT) {
        Err(RingError::Relay { code, detail }) => {
            assert_eq!(code, ErrorCode::PushFailed);
            assert_eq!(detail.as_deref(), Some("unreachable"));
        }
        other => panic!("expected unreachable, got {other:?}"),
    }
    assert_eq!(r.push_frames(), 1);
}

#[test]
fn push_counts_toward_the_quota() {
    let gw = Arc::new(FakeGateway::start());
    let r = TestRelay::start_with(TestRelayOptions {
        push_gateway: Some(gw.url()),
        quota_frames_per_day: Some(2),
        ..TestRelayOptions::default()
    });
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (d, _) = connect(&t, &ring.chain, ring.daemon.clone());
    d.push(&contract::push_request(), WAIT).expect("first");
    d.push(&contract::push_request(), WAIT).expect("second");
    match d.push(&contract::push_request(), WAIT) {
        Err(RingError::Relay { code, .. }) => assert_eq!(code, ErrorCode::Quota),
        other => panic!("expected quota, got {other:?}"),
    }
    assert_eq!(gw.requests().len(), 2);
}

#[test]
fn ring_moves_to_new_relay_with_full_chain() {
    // The default 10 s auth deadline: staging and verifying a 1.2 MiB chain can take longer
    // than the short deadline the other tests use on a loaded machine.
    let (old, new) = (TestRelay::start(), TestRelay::start());
    contract::ring_moves_to_new_relay_with_full_chain(&old.target(), &new.target());
}

#[test]
fn signature_for_relay_with_other_origin_fails() {
    // A Relay that knows itself by another origin refuses this device's signature.
    let r = TestRelay::start_with(TestRelayOptions {
        origin_override: Some("wss://relay.example.com".into()),
        ..TestRelayOptions::default()
    });
    let t = r.target();
    let ring = TestRing::new(&t.url);
    match try_connect(&t, &ring.chain, ring.desktop.clone()) {
        Err(RingError::Relay { code, .. }) => {
            assert_eq!(code, xshell_protocol::ring::relay::ErrorCode::BadSignature)
        }
        other => panic!("expected bad_signature, got {:?}", other.err()),
    }
}

// ---- A malicious Relay ----------------------------------------------------------------------

#[test]
fn client_refuses_forged_roster_from_relay() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (a, rec) = connect(&t, &ring.chain, ring.desktop.clone());
    let me = ring.desktop.sign_key();
    // A Mobile-signed v3 that adds an intruder Desktop.
    let intruder = keys();
    let forged = raw_next(ring.chain.head(), &*ring.mobile, |x| {
        x.members.push(member(&intruder, "intruder", Role::Desktop));
    });
    let frame = RelayFrame::Roster {
        roster: forged.token().to_string(),
    };
    assert!(r.inject(&ring.ring_id(), &me, &frame.encode()));
    assert_eq!(
        rec.wait_for(WAIT, |e| matches!(e, Event::RosterRejected(_))),
        Some(Event::RosterRejected(RosterError::SignerNotDesktop))
    );
    // A tampered token, and a rollback to v1.
    let (head, sig) = ring.chain.head().token().rsplit_once('.').unwrap();
    let tampered = format!("{}x.{sig}", &head[..head.len() - 1]);
    r.inject(
        &ring.ring_id(),
        &me,
        &RelayFrame::Roster { roster: tampered }.encode(),
    );
    let mut other_v1 = ring.chain.genesis().roster().clone();
    other_v1.issued_at += 1;
    let other_v1 = other_v1.sign(&*ring.desktop).unwrap();
    r.inject(
        &ring.ring_id(),
        &me,
        &RelayFrame::Roster {
            roster: other_v1.token().to_string(),
        }
        .encode(),
    );
    assert!(rec
        .wait_for(WAIT, |e| e == &Event::RosterRejected(RosterError::Stale))
        .is_some());
    assert_eq!(a.roster(), *ring.chain.head());
    assert!(!a.is_closed());
}

#[test]
fn client_ignores_unknown_frame_type() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (_a, rec_a) = connect(&t, &ring.chain, ring.desktop.clone());
    let (d, _) = connect(&t, &ring.chain, ring.daemon.clone());
    let me = ring.desktop.sign_key();
    r.inject(&ring.ring_id(), &me, r#"{"t":"push.v2","x":[1,2,3]}"#);
    d.send(&me, b"after").unwrap();
    assert!(rec_a
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"after")
        )
        .is_some());
}

#[test]
fn hostile_relay_frames_never_break_the_client() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (a, rec) = connect(&t, &ring.chain, ring.desktop.clone());
    let (d, _) = connect(&t, &ring.chain, ring.daemon.clone());
    let me = ring.desktop.sign_key();
    let daemon = ring.daemon.sign_key().to_b64();
    let junk: Vec<String> = vec![
        "".into(),
        "{".into(),
        "[]".into(),
        "null".into(),
        r#"{"t":5}"#.into(),
        r#"{"t":"env","t":"env"}"#.into(),
        r#"{"t":"env"}"#.into(),
        json!({"t":"env","from":daemon,"payload":"A"}).to_string(),
        json!({"t":"env","from":daemon,"payload":"+/+/"}).to_string(),
        json!({"t":"env","from":"AAAA","payload":""}).to_string(),
        json!({"t":"env","from":daemon,"payload":b64::encode(&vec![0; 65537])}).to_string(),
        json!({"t":"presence","signKey":"x","online":true}).to_string(),
        json!({"t":"presence","signKey":daemon,"online":"yes"}).to_string(),
        json!({"t":"roster","roster":"xro1.!!.!!"}).to_string(),
        json!({"t":"roster","roster":"x".repeat(70_000)}).to_string(),
        json!({"t":"roster.chain","id":1,"rosters":["nope"],"more":true}).to_string(),
        json!({"t":"ok","id":"1"}).to_string(),
        json!({"t":"error","code":7}).to_string(),
        json!({"t":"entitlement","token":"xet1.junk"}).to_string(),
        json!({"t":"welcome"}).to_string(),
        json!({"t":"challenge","v":99,"nonce":""}).to_string(),
        "\u{0}\u{1}".into(),
    ];
    for j in &junk {
        assert!(r.inject(&ring.ring_id(), &me, j));
    }
    d.send(&me, b"still alive").unwrap();
    assert!(rec
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"still alive")
        )
        .is_some());
    assert!(!a.is_closed());
    assert_eq!(a.roster(), *ring.chain.head());
    assert_eq!(a.entitlement(), None);
}

#[test]
fn relay_asserted_sender_and_presence_are_only_assertions() {
    // Until Noise (#9) authenticates peers, a malicious Relay can claim any member sent an
    // envelope and report any presence. The client delivers both, marked as Relay claims in
    // the API docs; it still drops envelopes from keys outside the trusted Roster.
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (a, rec) = connect(&t, &ring.chain, ring.desktop.clone());
    let me = ring.desktop.sign_key();
    let forged_env = RelayFrame::Env {
        from: ring.mobile.sign_key(),
        payload: b64::encode(b"never sent by the mobile"),
    };
    r.inject(&ring.ring_id(), &me, &forged_env.encode());
    assert!(rec
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { from, .. } if *from == ring.mobile.sign_key())
        )
        .is_some());
    let fake = RelayFrame::Presence(Presence {
        sign_key: ring.daemon.sign_key(),
        online: true,
        last_seen: Some(1),
        last_reason: None,
        foreground: false,
        foreground_until: None,
    });
    r.inject(&ring.ring_id(), &me, &fake.encode());
    assert!(rec.wait_presence(&ring.daemon.sign_key(), |p| matches!(
        p,
        MemberPresence::Online { .. }
    )));
    // A non-member sender is dropped.
    let stranger = keys();
    let foreign = RelayFrame::Env {
        from: stranger.sign_key(),
        payload: b64::encode(b"from outside"),
    };
    r.inject(&ring.ring_id(), &me, &foreign.encode());
    std::thread::sleep(QUIET);
    assert!(!rec
        .envelopes()
        .iter()
        .any(|(f, _)| *f == stranger.sign_key()));
    drop(a);
}

#[test]
fn gap_in_broadcast_triggers_resync() {
    let r = relay();
    let t = r.target();
    let mut ring = TestRing::new(&t.url);
    let (a, _) = connect(&t, &ring.chain, ring.desktop.clone());
    let (d, rec_d) = connect(&t, &ring.chain, ring.daemon.clone());
    // The Relay withholds v3 and v4 from the Daemon …
    assert!(r.fault(&ring.ring_id(), &ring.daemon.sign_key(), Fault::Mute));
    let v3 = ring.next(|_| {});
    let v4 = ring.next(|_| {});
    a.publish_roster(&v3).unwrap();
    a.publish_roster(&v4).unwrap();
    std::thread::sleep(QUIET);
    assert_eq!(d.roster().version(), 2);
    // … then shows it v4 alone: the Daemon asks for what it missed and verifies it.
    r.inject(
        &ring.ring_id(),
        &ring.daemon.sign_key(),
        &RelayFrame::Roster {
            roster: v4.token().to_string(),
        }
        .encode(),
    );
    assert!(rec_d
        .wait_for(WAIT, |e| e == &Event::Roster { version: 4 })
        .is_some());
    assert_eq!(d.chain(), ring.chain);
}

// ---- Hostile transports over TLS ------------------------------------------------------------

fn tls_relay() -> (TestRelay, Arc<rustls::ClientConfig>) {
    let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let cert_der = ck.cert.der().to_vec();
    let key_der = ck.signing_key.serialize_der();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der.clone()))
        .unwrap();
    let client = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let relay = TestRelay::start_with(TestRelayOptions {
        tls: Some(TestTls { cert_der, key_der }),
        ..TestRelayOptions::default()
    });
    (relay, Arc::new(client))
}

fn fast(
    t: &RelayTarget,
    ring: &contract::TestRing,
    who: Arc<dyn Signer>,
) -> (RingClient, Arc<Recorder>) {
    let mut cfg = config(t, &ring.chain, who);
    cfg.timeouts = RingTimeouts {
        connect: Duration::from_secs(10),
        ping_interval: Duration::from_millis(200),
        dead_after: Duration::from_millis(1500),
        request: WAIT,
        bye: Duration::from_millis(800),
    };
    cfg.limits = RingLimits {
        queue_bytes: 512 * 1024,
        write_buffer_bytes: 1100 * 1024,
    };
    let rec = Recorder::new();
    (RingClient::connect(cfg, rec.clone()).expect("connect"), rec)
}

#[test]
fn works_over_tls() {
    let (r, tls) = tls_relay();
    let mut t = r.target();
    t.tls = Some(tls);
    assert!(t.url.starts_with("wss://"));
    let ring = TestRing::new(&t.url);
    let (a, rec_a) = connect(&t, &ring.chain, ring.desktop.clone());
    let (d, _) = connect(&t, &ring.chain, ring.daemon.clone());
    d.send(&ring.desktop.sign_key(), b"over tls").unwrap();
    assert!(rec_a
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"over tls")
        )
        .is_some());
    d.bye(ByeReason::idle()).unwrap();
    assert!(rec_a.wait_presence(&ring.daemon.sign_key(), |p| {
        matches!(p, MemberPresence::Closed { reason, .. } if reason == "idle")
    }));
    // The default roots do not trust the test certificate.
    let mut untrusted = t.clone();
    untrusted.tls = None;
    assert!(matches!(
        try_connect(&untrusted, &ring.chain, ring.mobile.clone()),
        Err(RingError::Connect(_))
    ));
    drop(a);
}

#[test]
fn trickled_tls_records_do_not_stall_sends_or_keepalive() {
    let (r, tls) = tls_relay();
    // Relay → client bytes arrive 64 at a time, every millisecond: TLS records in pieces.
    let proxy = TrickleProxy::start(r.addr(), 64, Duration::from_millis(1));
    let t = RelayTarget {
        url: format!("wss://127.0.0.1:{}", proxy.addr().port()),
        tls: Some(tls),
        auth_timeout: Duration::from_secs(10),
        gateway: None,
        quota_frames_per_day: None,
        pair_opens_per_minute: None,
        pair_ttl: None,
        client_ip_header: None,
        fake_gateway: None,
        foreground_lease: None,
    };
    r.set_origin(&t.origin());
    let ring = TestRing::new(&t.url);
    let (a, _) = fast(&t, &ring, ring.desktop.clone());
    let (_d, rec_d) = fast(&t, &ring, ring.daemon.clone());
    for i in 0..10u8 {
        a.send(&ring.daemon.sign_key(), &vec![i; 1024]).unwrap();
    }
    assert!(rec_d
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == &vec![9u8; 1024])
        )
        .is_some());
    // Keepalive keeps going (and is answered) while reads arrive in fragments.
    let before = r.pings(&ring.ring_id(), &ring.desktop.sign_key());
    std::thread::sleep(Duration::from_millis(1000));
    assert!(r.pings(&ring.ring_id(), &ring.desktop.sign_key()) >= before + 3);
    assert!(!a.is_closed());
    let started = Instant::now();
    a.bye(ByeReason::quit()).unwrap();
    assert!(started.elapsed() < Duration::from_millis(1500));
}

#[test]
fn endless_fragments_end_as_dead_within_the_deadline() {
    let (r, tls) = tls_relay();
    let mut t = r.target();
    t.tls = Some(tls);
    let ring = TestRing::new(&t.url);
    let (a, rec_a) = fast(&t, &ring, ring.desktop.clone());
    let (_d, rec_d) = fast(&t, &ring, ring.daemon.clone());
    assert!(r.fault(
        &ring.ring_id(),
        &ring.desktop.sign_key(),
        Fault::EndlessFragments {
            chunk: 100,
            every: Duration::from_millis(20)
        }
    ));
    let started = Instant::now();
    // Sends and pings still go out while a message that never ends comes in.
    a.send(&ring.daemon.sign_key(), b"while fragmented")
        .unwrap();
    assert!(rec_d
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"while fragmented")
        )
        .is_some());
    // Two more pings (200 ms apart) before the 1.5 s dead deadline, polled so that a
    // loaded runner's scheduling does not count against it.
    let before = r.pings(&ring.ring_id(), &ring.desktop.sign_key());
    let by = Instant::now() + Duration::from_millis(1200);
    while r.pings(&ring.ring_id(), &ring.desktop.sign_key()) < before + 2 && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(r.pings(&ring.ring_id(), &ring.desktop.sign_key()) >= before + 2);
    // No complete frame arrives, so the connection is declared dead on time.
    assert_eq!(
        rec_a.closed(Duration::from_secs(4)),
        Some(CloseReason::Dead)
    );
    assert!(started.elapsed() < Duration::from_millis(1500 + 1500));
}

#[test]
fn peer_that_stops_reading_gives_backpressure_and_bounded_bye() {
    let (r, tls) = tls_relay();
    let mut t = r.target();
    t.tls = Some(tls);
    let ring = TestRing::new(&t.url);
    let mut cfg = config(&t, &ring.chain, ring.desktop.clone());
    cfg.timeouts = RingTimeouts {
        connect: Duration::from_secs(10),
        ping_interval: Duration::from_millis(200),
        dead_after: Duration::from_secs(30),
        request: WAIT,
        bye: Duration::from_millis(800),
    };
    cfg.limits = RingLimits {
        queue_bytes: 512 * 1024,
        write_buffer_bytes: 1100 * 1024,
    };
    let a = RingClient::connect(cfg, Recorder::new()).expect("connect");
    let (_d, _) = fast(&t, &ring, ring.daemon.clone());
    assert!(r.fault(
        &ring.ring_id(),
        &ring.desktop.sign_key(),
        Fault::StopReading
    ));
    let payload = vec![1u8; 60 * 1024];
    let started = Instant::now();
    let mut sent = 0usize;
    let full = loop {
        match a.send(&ring.daemon.sign_key(), &payload) {
            Ok(()) => sent += payload.len(),
            Err(RingError::Backpressure) => break true,
            Err(e) => panic!("unexpected {e}"),
        }
        if started.elapsed() > Duration::from_secs(20) {
            break false;
        }
    };
    assert!(full, "no backpressure after {sent} bytes");
    // The goodbye cannot get out, and says so within its deadline.
    let started = Instant::now();
    assert_eq!(a.bye(ByeReason::quit()), Err(RingError::Timeout));
    assert!(started.elapsed() < Duration::from_millis(800 + 600));

    // And a peer that stops reading is detected by keepalive.
    let (m, rec_m) = fast(&t, &ring, ring.mobile.clone());
    assert!(r.fault(&ring.ring_id(), &ring.mobile.sign_key(), Fault::StopReading));
    let started = Instant::now();
    assert_eq!(
        rec_m.closed(Duration::from_secs(4)),
        Some(CloseReason::Dead)
    );
    assert!(started.elapsed() < Duration::from_millis(1500 + 1000));
    assert!(m.is_closed());
}

#[test]
fn closed_fires_exactly_once() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (d, rec) = connect(&t, &ring.chain, ring.desktop.clone());
    d.bye(ByeReason::upgrade()).unwrap();
    std::thread::sleep(QUIET);
    let n = rec
        .events()
        .iter()
        .filter(|e| matches!(e, Event::Closed(_)))
        .count();
    assert_eq!(n, 1);
    let (d, rec) = connect(&t, &ring.chain, ring.desktop.clone());
    drop(d);
    assert_eq!(rec.closed(WAIT), Some(CloseReason::Local));
    drop(r);
    std::thread::sleep(QUIET);
    assert_eq!(
        rec.events()
            .iter()
            .filter(|e| matches!(e, Event::Closed(_)))
            .count(),
        1
    );
}

// ---- The Hosted Relay -----------------------------------------------------------------------

fn hosted_relay() -> (TestRelay, RelayTarget) {
    let gw = keys();
    let r = TestRelay::start_with(TestRelayOptions {
        hosted: Some(GatewayKeys::new(&[gw.sign_key()])),
        ..TestRelayOptions::default()
    });
    let mut t = r.target();
    t.gateway = Some(gw);
    (r, t)
}

macro_rules! hosted_scenario {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            let (_r, t) = hosted_relay();
            contract::$name(&t);
        }
    )*};
}

hosted_scenario!(
    hosted_first_token_enables_routing,
    hosted_routing_ends_when_the_last_token_expires,
    hosted_slot_keeps_hosted_over_push,
    hosted_slot_keeps_the_later_expiry,
    hosted_slot_replaces_an_expired_token,
    hosted_slot_keeps_the_better_of_simultaneous_puts,
);

/// Section 12: the decision time is read once, under the Ring's lock. A put token that
/// verified on arrival but has expired by then is refused, and the slot stays as it was.
#[test]
fn hosted_put_expiring_before_the_decision_is_refused() {
    let pause = Duration::from_millis(1200);
    let gw = keys();
    let r = TestRelay::start_with(TestRelayOptions {
        hosted: Some(GatewayKeys::new(&[gw.sign_key()])),
        entitlement_put_pause: pause,
        ..TestRelayOptions::default()
    });
    let mut t = r.target();
    t.gateway = Some(gw);
    let ring = TestRing::new(&t.url);
    let (a, rec_a) = connect(&t, &ring.chain, ring.desktop.clone());
    let push = contract::gateway_token(
        &t,
        &ring.ring_id(),
        xshell_protocol::ring::entitlement::Tier::Push,
        "p",
        contract::now() + 3600,
    );
    a.put_entitlement(&push).expect("put push");
    // Start right after a second boundary, so the token verifies on arrival (in this second)
    // and has expired when the Relay decides (in the next).
    while contract::now_ms() % 1000 > 100 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let late = contract::hosted_token(&t, &ring.ring_id(), contract::now() + 1);
    let started = Instant::now();
    let got = a.put_entitlement(&late);
    assert!(
        started.elapsed() >= pause,
        "verified on arrival, so refused only after the pause"
    );
    match got {
        Err(RingError::Relay { code, detail }) => {
            assert_eq!(code, ErrorCode::EntitlementInvalid);
            assert_eq!(detail.as_deref(), Some("expired"));
        }
        other => panic!("expected expired, got {other:?}"),
    }
    assert!(rec_a
        .wait_for(QUIET, |e| e == &Event::Entitlement(Some(late.clone())))
        .is_none());
    let (m, _) = connect(&t, &ring.chain, ring.mobile.clone());
    assert_eq!(m.entitlement().as_deref(), Some(push.as_str()));
    assert!(m.limited());
}

#[test]
fn every_hosted_scenario_is_run_here() {
    assert_eq!(HOSTED_SCENARIOS.len(), 6);
}

// ---- Quotas ---------------------------------------------------------------------------------

#[test]
fn quota_refuses_then_closes() {
    let r = TestRelay::start_with(TestRelayOptions {
        quota_frames_per_day: Some(40),
        ..TestRelayOptions::default()
    });
    contract::quota_refuses_then_closes(&r.target());
}

// ---- The pairing pipe's limits ---------------------------------------------------------------

#[test]
fn pair_pipe_expires() {
    let r = TestRelay::start_with(TestRelayOptions {
        pair_ttl: Duration::from_secs(1),
        ..TestRelayOptions::default()
    });
    contract::pair_pipe_expires(&r.target());
}

#[test]
fn pair_pipe_used_slot_stays_busy() {
    let r = TestRelay::start_with(TestRelayOptions {
        pair_ttl: Duration::from_secs(1),
        ..TestRelayOptions::default()
    });
    contract::pair_pipe_used_slot_stays_busy(&r.target());
}

#[test]
fn pair_refusal_burns_slot() {
    contract::pair_refusal_burns_slot(&cap_relay(None).target());
}

#[test]
fn pair_pipe_rate_limited() {
    let r = TestRelay::start_with(TestRelayOptions {
        pair_opens_per_minute: Some(5),
        ..TestRelayOptions::default()
    });
    contract::pair_pipe_rate_limited(&r.target());
}

#[test]
fn every_pair_limit_scenario_is_run_here() {
    assert_eq!(PAIR_TTL_SCENARIOS.len(), 2);
    assert_eq!(PAIR_RATE_SCENARIOS.len(), 1);
    assert_eq!(PAIR_CAP_SCENARIOS.len(), 3);
}

/// A Relay for the slot cap scenarios: the default caps and no rate limit, optionally taking
/// the client address from a trusted header.
fn cap_relay(header: Option<&str>) -> TestRelay {
    TestRelay::start_with(TestRelayOptions {
        client_ip_header: header.map(str::to_string),
        ..TestRelayOptions::default()
    })
}

#[test]
fn pair_prefix_cap_refuses_21st() {
    contract::pair_prefix_cap_refuses_21st(&cap_relay(None).target());
    contract::pair_prefix_cap_refuses_21st(&cap_relay(Some("X-Forwarded-For")).target());
}

#[test]
fn pair_reservation_released_on_meeting() {
    contract::pair_reservation_released_on_meeting(&cap_relay(None).target());
    // With a header, both scenarios share one Relay: each uses prefixes of its own.
    let r = cap_relay(Some("X-Real-IP"));
    for (_, s) in PAIR_CAP_SCENARIOS {
        s(&r.target());
    }
}

/// Opens a waiting pairing socket from `from` (in the target's client address header).
fn wait_from(t: &RelayTarget, from: Option<&str>) -> Result<RawConn, RingError> {
    let mut c = RawConn::open_pair_from(t, &fresh_slot(), from)?;
    assert!(matches!(
        c.pair_frame(Duration::from_secs(5)),
        Some(PairRelayFrame::Wait { .. })
    ));
    Ok(c)
}

fn is_status(r: Result<RawConn, RingError>, status: &str) -> bool {
    matches!(r, Err(RingError::Connect(m)) if m.contains(status))
}

#[test]
fn pair_client_address_comes_from_the_trusted_header() {
    let r = TestRelay::start_with(TestRelayOptions {
        client_ip_header: Some("X-Forwarded-For".into()),
        pair_opens_per_minute: Some(1),
        pair_max_slots_per_prefix: 2,
        ..TestRelayOptions::default()
    });
    let t = r.target();
    // The last list entry counts: the one the nearest proxy appended.
    let _a = wait_from(&t, Some("203.0.113.9, 192.0.2.1")).unwrap();
    assert!(is_status(wait_from(&t, Some("192.0.2.1")), "429"));
    let _b = wait_from(&t, Some("192.0.2.1, 198.51.100.7")).unwrap();
    // An IPv4-mapped IPv6 address is its IPv4 address.
    assert!(is_status(wait_from(&t, Some("::ffff:198.51.100.7")), "429"));
    // The rate limit counts IPv6 by its /64, brackets or not.
    let _c = wait_from(&t, Some("[2001:db8:1:2::1]")).unwrap();
    assert!(is_status(
        wait_from(&t, Some("2001:db8:1:2:ffff::9")),
        "429"
    ));
    // The slot cap counts IPv4 by its /24: the slots of 198.51.100.7 and .8 fill theirs.
    let _d = wait_from(&t, Some("198.51.100.8")).unwrap();
    assert!(is_status(wait_from(&t, Some("198.51.100.9")), "503"));
    // Without a usable header value, every open shares one bucket.
    let _e = wait_from(&t, None).unwrap();
    assert!(is_status(wait_from(&t, Some("not an address")), "429"));
}

#[test]
fn pair_peer_address_is_the_socket_s_without_a_trusted_header() {
    let r = TestRelay::start_with(TestRelayOptions {
        pair_opens_per_minute: Some(1),
        ..TestRelayOptions::default()
    });
    // An untrusted header changes nothing: both opens come from 127.0.0.1.
    let t = RelayTarget {
        client_ip_header: Some("X-Forwarded-For".into()),
        ..r.target()
    };
    let _a = wait_from(&t, Some("192.0.2.1")).unwrap();
    assert!(is_status(wait_from(&t, Some("192.0.2.2")), "429"));
}

#[test]
fn failed_pair_upgrade_releases_its_reservation_at_once() {
    let r = TestRelay::start_with(TestRelayOptions {
        pair_max_slots: 1,
        ..TestRelayOptions::default()
    });
    let t = r.target();
    // An upgrade without Sec-WebSocket-Key fails after admission reserved the only place.
    let mut raw = std::net::TcpStream::connect(r.addr()).unwrap();
    {
        use std::io::Write;
        write!(
            raw,
            "GET /v1/pair/{} HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n",
            fresh_slot()
        )
        .unwrap();
    }
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut raw, &mut buf);
    assert!(!String::from_utf8_lossy(&buf).contains(" 101 "));
    // The place is free at once: a valid new slot waits, with the lifetime far from over.
    let mut c = RawConn::open_pair(&t, &fresh_slot()).unwrap();
    assert!(matches!(
        c.pair_frame(Duration::from_secs(5)),
        Some(PairRelayFrame::Wait { .. })
    ));
    assert_eq!(r.pair_slots(), 1);
}

#[test]
fn pair_reservations_are_released_and_tombstones_forgotten() {
    let r = TestRelay::start_with(TestRelayOptions {
        pair_ttl: Duration::from_millis(500),
        pair_tombstone: Duration::from_millis(300),
        ..TestRelayOptions::default()
    });
    let t = r.target();
    // A meeting releases the reservation; the tombstone stays while the sockets do.
    let slot = fresh_slot();
    let mut a = RawConn::open_pair(&t, &slot).unwrap();
    assert!(matches!(
        a.pair_frame(Duration::from_secs(5)),
        Some(PairRelayFrame::Wait { .. })
    ));
    assert_eq!(r.pair_slots(), 1);
    let mut b = RawConn::open_pair(&t, &slot).unwrap();
    assert_eq!(
        b.pair_frame(Duration::from_secs(5)),
        Some(PairRelayFrame::Peer)
    );
    assert_eq!(r.pair_slots(), 0);
    assert_eq!(r.pair_slots_held(), 1);
    drop((a, b));
    // A waiting slot expires: its reservation goes, and its tombstone runs out too.
    let lone = fresh_slot();
    let _w = RawConn::open_pair(&t, &lone).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(r.pair_slots(), 0);
    assert_eq!(r.pair_slots_held(), 0);
    // Forgotten: a later open of the used slot starts a new slot.
    let mut again = RawConn::open_pair(&t, &slot).unwrap();
    assert!(matches!(
        again.pair_frame(Duration::from_secs(5)),
        Some(PairRelayFrame::Wait { .. })
    ));
    assert_eq!(r.pair_slots(), 1);
}

#[test]
fn pair_slots_are_capped_and_forgotten() {
    let r = TestRelay::start_with(TestRelayOptions {
        pair_max_slots: 2,
        ..TestRelayOptions::default()
    });
    let t = r.target();
    let _a = RawConn::open_pair(&t, &fresh_slot()).unwrap();
    let _b = RawConn::open_pair(&t, &fresh_slot()).unwrap();
    match RawConn::open_pair(&t, &fresh_slot()) {
        Err(RingError::Connect(m)) => assert!(m.contains("503"), "{m}"),
        other => panic!("expected HTTP 503, got {:?}", other.err()),
    }
    assert_eq!(r.pair_slots(), 2);
}

#[test]
fn concurrent_pair_opens_respect_the_slot_cap() {
    // Admission reserves the slot, so opens racing for the last places cannot overshoot.
    for _ in 0..5 {
        let r = TestRelay::start_with(TestRelayOptions {
            pair_max_slots: 2,
            ..TestRelayOptions::default()
        });
        let t = r.target();
        let barrier = Arc::new(std::sync::Barrier::new(6));
        let opens: Vec<_> = (0..6)
            .map(|_| {
                let (t, b) = (t.clone(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    RawConn::open_pair(&t, &fresh_slot()).ok()
                })
            })
            .collect();
        let held: Vec<RawConn> = opens
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(held.len(), 2);
        assert_eq!(r.pair_slots(), 2);
    }
}

#[test]
fn every_quota_scenario_is_run_here() {
    assert_eq!(QUOTA_SCENARIOS.len(), 1);
}

#[test]
fn spaced_ping_counts_against_the_quota() {
    // Only the exact bytes are auto-answered on a Worker; any other ping reaches the Relay.
    let r = TestRelay::start_with(TestRelayOptions {
        quota_frames_per_day: Some(1),
        ..TestRelayOptions::default()
    });
    let t = r.target();
    let get = r#"{"t":"roster.get","id":5,"since":0}"#;
    // On a fresh Ring the request is processed …
    let fresh = TestRing::new(&t.url);
    let mut f = RawConn::login(&t, &fresh.chain, &*fresh.desktop);
    f.send(get).unwrap();
    assert!(matches!(
        f.frame(WAIT),
        Some(RelayFrame::RosterChain { id: 5, .. })
    ));
    // … but after a spaced ping, which counts, it is over the quota.
    let ring = TestRing::new(&t.url);
    let mut a = RawConn::login(&t, &ring.chain, &*ring.desktop);
    a.send(r#"{"t": "ping"}"#).unwrap();
    assert_eq!(a.recv(WAIT), contract::Raw::Text(r#"{"t":"pong"}"#.into()));
    a.send(get).unwrap();
    assert!(matches!(
        a.frame(WAIT),
        Some(RelayFrame::Error {
            code: ErrorCode::Quota,
            id: Some(5),
            ..
        })
    ));
}

#[test]
fn hosted_relay_has_the_default_quota() {
    let (_r, t) = hosted_relay();
    assert_eq!(t.quota_frames_per_day, Some(HOSTED_QUOTA_FRAMES_PER_DAY));
    assert_eq!(relay().target().quota_frames_per_day, None);
}

// ---- Routing --------------------------------------------------------------------------------

#[test]
fn relay_url_with_a_path_is_served_by_suffix() {
    let r = relay();
    let t = RelayTarget {
        url: format!("{}/some/base", r.url()),
        ..r.target()
    };
    let ring = TestRing::new(&t.url);
    let (a, _) = connect(&t, &ring.chain, ring.desktop.clone());
    assert!(!a.is_closed());
    // Anything that is not `…/v1/ring/{ringId}` is not found.
    for path in [
        "/",
        "/v1/ring/short",
        "/v1/ring/",
        "/v1/ringx/abcdefghijklmnopq",
    ] {
        assert_eq!(http_status(&r, path, true), "404", "{path} (upgrade)");
        assert_eq!(http_status(&r, path, false), "404", "{path}");
    }
}

/// The status a plain HTTP/1.1 GET of `path` gets, with or without a WebSocket upgrade.
fn http_status(r: &TestRelay, path: &str, upgrade: bool) -> String {
    use std::io::{Read, Write};
    let mut tcp = std::net::TcpStream::connect(r.addr()).unwrap();
    tcp.set_read_timeout(Some(WAIT)).unwrap();
    let up = if upgrade {
        "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    } else {
        ""
    };
    write!(tcp, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{up}\r\n").unwrap();
    let mut head = [0u8; 12];
    tcp.read_exact(&mut head).unwrap();
    String::from_utf8_lossy(&head[9..]).into_owned()
}

#[test]
fn plain_http_gets_healthz_426_and_404() {
    let r = relay();
    assert_eq!(http_status(&r, "/healthz", false), "200");
    let ring = TestRing::new(&r.url());
    let path = format!("/base/v1/ring/{}", ring.ring_id());
    assert_eq!(http_status(&r, &path, false), "426");
    assert_eq!(http_status(&r, "/nope", false), "404");
    assert_eq!(http_status(&r, "/v1/ring/bad!id-0123456789", false), "404");
}

// ---- Staged candidates ----------------------------------------------------------------------

#[test]
fn staged_candidate_is_swept_after_its_ttl() {
    let r = relay(); // auth timeout 500 ms: the stage TTL is 1.5 s
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let mut c = RawConn::open(&t, &ring.ring_id()).unwrap();
    let _ = c.challenge();
    c.auth_chain(&[ring.chain.genesis(), ring.chain.head()]);
    std::thread::sleep(QUIET);
    assert_eq!(r.staged_chunks(), 1);
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(4);
    while r.staged_chunks() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(r.staged_chunks(), 0);
    // A committed candidate leaves nothing staged.
    let (_a, _) = connect(&t, &ring.chain, ring.desktop.clone());
    assert_eq!(r.staged_chunks(), 0);
    assert_eq!(r.head_version(&ring.ring_id()), Some(2));
}

#[test]
fn lost_candidate_fails_closed() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let mut c = RawConn::open(&t, &ring.ring_id()).unwrap();
    let (nonce, _) = c.challenge();
    c.auth_chain(&[ring.chain.genesis(), ring.chain.head()]);
    std::thread::sleep(QUIET);
    r.drop_staged();
    c.auth(&*ring.desktop, &t.origin(), &ring.ring_id(), &nonce);
    let e = c.error(WAIT).expect("an error");
    assert_eq!(
        (e.0, e.2.as_deref()),
        (
            xshell_protocol::ring::relay::ErrorCode::RosterInvalid,
            Some("invalid")
        )
    );
    assert_eq!(c.close_code(WAIT), Some(4003));
    assert_eq!(r.head_version(&ring.ring_id()), None);
}

// ---- Exactly one roster callback ------------------------------------------------------------

#[test]
fn roster_callback_fires_once_whatever_the_order() {
    for broadcast_before_ok in [false, true] {
        let r = TestRelay::start_with(TestRelayOptions {
            broadcast_before_ok,
            ..TestRelayOptions::default()
        });
        let t = r.target();
        let mut ring = TestRing::new(&t.url);
        let (a, rec_a) = connect(&t, &ring.chain, ring.desktop.clone());
        let v3 = ring.next(|_| {});
        a.publish_roster(&v3).unwrap();
        assert_eq!(a.roster(), v3);
        std::thread::sleep(QUIET);
        let n = rec_a
            .events()
            .iter()
            .filter(|e| **e == Event::Roster { version: 3 })
            .count();
        assert_eq!(n, 1, "broadcast_before_ok = {broadcast_before_ok}");
    }
}

// ---- Bounded work and deadlines -------------------------------------------------------------

fn slow_dead(
    t: &RelayTarget,
    ring: &contract::TestRing,
    who: Arc<dyn Signer>,
    bye: Duration,
) -> (RingClient, Arc<Recorder>) {
    let mut cfg = config(t, &ring.chain, who);
    cfg.timeouts = RingTimeouts {
        connect: Duration::from_secs(10),
        ping_interval: Duration::from_millis(200),
        dead_after: Duration::from_secs(30),
        request: WAIT,
        bye,
    };
    cfg.limits = RingLimits {
        queue_bytes: 512 * 1024,
        write_buffer_bytes: 1100 * 1024,
    };
    let rec = Recorder::new();
    (RingClient::connect(cfg, rec.clone()).expect("connect"), rec)
}

#[test]
fn zero_length_fragment_flood_does_not_starve_writes() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let (a, _) = slow_dead(&t, &ring, ring.desktop.clone(), Duration::from_secs(2));
    let (_d, rec_d) = connect(&t, &ring.chain, ring.daemon.clone());
    let me = ring.desktop.sign_key();
    assert!(r.fault(&ring.ring_id(), &me, Fault::ZeroFragmentFlood));
    std::thread::sleep(QUIET);
    a.send(&ring.daemon.sign_key(), b"during the flood")
        .unwrap();
    assert!(rec_d
        .wait_for(
            WAIT,
            |e| matches!(e, Event::Envelope { payload, .. } if payload == b"during the flood")
        )
        .is_some());
    // Pings every 200 ms: at least 3 of about 7 in 1.5 s, with slack for a loaded machine.
    let before = r.pings(&ring.ring_id(), &me);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(r.pings(&ring.ring_id(), &me) >= before + 3);
    let started = Instant::now();
    let _ = a.bye(ByeReason::quit());
    assert!(started.elapsed() < Duration::from_millis(2600));
    let deadline = Instant::now() + WAIT;
    loop {
        let p = r.presence(&ring.ring_id(), &me).unwrap();
        if p.last_reason.as_deref() == Some("quit") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the goodbye never reached the Relay: {p:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn bye_deadline_holds_when_the_peer_only_sends_pongs() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let bye = Duration::from_millis(800);
    let (a, rec_a) = slow_dead(&t, &ring, ring.desktop.clone(), bye);
    let (_d, _) = connect(&t, &ring.chain, ring.daemon.clone());
    assert!(r.fault(&ring.ring_id(), &ring.desktop.sign_key(), Fault::PongsOnly));
    // Fill everything so the goodbye sits behind data that never drains.
    let payload = vec![1u8; 60 * 1024];
    let started = Instant::now();
    loop {
        match a.send(&ring.daemon.sign_key(), &payload) {
            Ok(()) => {}
            Err(RingError::Backpressure) => break,
            Err(e) => panic!("unexpected {e}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no backpressure"
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(!a.is_closed(), "pongs keep the connection alive");
    let started = Instant::now();
    assert_eq!(a.bye(ByeReason::quit()), Err(RingError::Timeout));
    assert!(started.elapsed() < bye + Duration::from_millis(600));
    // The IO thread itself closed at the deadline, not merely the caller giving up.
    assert_eq!(
        rec_a.closed(Duration::from_millis(300)),
        Some(CloseReason::Bye)
    );
    assert!(started.elapsed() < bye + Duration::from_millis(900));
}

// ---- A Relay that sends out of phase --------------------------------------------------------

/// A one-connection fake Relay running `script`.
fn fake_relay(
    script: impl FnOnce(&mut tungstenite::WebSocket<std::net::TcpStream>) + Send + 'static,
) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://127.0.0.1:{}", l.local_addr().unwrap().port());
    std::thread::spawn(move || {
        let (tcp, _) = l.accept().unwrap();
        let mut ws = tungstenite::accept(tcp).unwrap();
        script(&mut ws);
        std::thread::sleep(Duration::from_secs(1));
    });
    url
}

fn send(ws: &mut tungstenite::WebSocket<std::net::TcpStream>, f: &RelayFrame) {
    let _ = ws.send(tungstenite::Message::text(f.encode()));
}

/// Reads until a client frame of type `t`.
fn read_until(ws: &mut tungstenite::WebSocket<std::net::TcpStream>, t: &str) {
    let needle = format!("\"t\":\"{t}\"");
    while let Ok(m) = ws.read() {
        if let tungstenite::Message::Text(s) = m {
            if s.as_str().contains(&needle) {
                return;
            }
        }
    }
}

fn challenge() -> RelayFrame {
    RelayFrame::Challenge {
        v: 1,
        nonce: b64::encode(&[7; 32]),
        roster_version: 2,
        caps: vec![],
    }
}

#[test]
fn session_frames_before_welcome_fail_the_connect() {
    let holder: Arc<std::sync::Mutex<Option<contract::TestRing>>> = Arc::default();
    let url = fake_relay(|ws| {
        send(ws, &challenge());
        read_until(ws, "auth");
        send(
            ws,
            &RelayFrame::Env {
                from: keys().sign_key(),
                payload: b64::encode(b"too early"),
            },
        );
    });
    let ring = TestRing::new(&url);
    let t = RelayTarget {
        url,
        tls: None,
        auth_timeout: Duration::from_secs(10),
        gateway: None,
        quota_frames_per_day: None,
        pair_opens_per_minute: None,
        pair_ttl: None,
        client_ip_header: None,
        fake_gateway: None,
        foreground_lease: None,
    };
    match try_connect(&t, &ring.chain, ring.desktop.clone()) {
        Err(RingError::Protocol(m)) => assert!(m.contains("env"), "{m}"),
        other => panic!("expected a protocol error, got {:?}", other.err()),
    }
    drop(holder);
}

#[test]
fn too_much_traffic_during_sync_fails_the_connect() {
    let ring = TestRing::new("ws://127.0.0.1:1");
    let me = ring.desktop.sign_key();
    let other = ring.daemon.sign_key();
    let url = fake_relay(move |ws| {
        send(ws, &challenge());
        read_until(ws, "auth");
        send(
            ws,
            &RelayFrame::Welcome {
                you: me,
                roster_version: 99,
                presence: vec![],
                entitlement: None,
                limited: false,
                caps: vec![],
            },
        );
        read_until(ws, "roster.get");
        // Never answer; flood session traffic instead.
        for _ in 0..300 {
            send(
                ws,
                &RelayFrame::Presence(Presence {
                    sign_key: other,
                    online: true,
                    last_seen: Some(1),
                    last_reason: None,
                    foreground: false,
                    foreground_until: None,
                }),
            );
        }
    });
    // The same Ring, pointed at the fake Relay.
    let mut ring = ring;
    let v3 = ring.next(|d| d.relay_url = url.clone());
    let _ = v3;
    let t = RelayTarget {
        url,
        tls: None,
        auth_timeout: Duration::from_secs(10),
        gateway: None,
        quota_frames_per_day: None,
        pair_opens_per_minute: None,
        pair_ttl: None,
        client_ip_header: None,
        fake_gateway: None,
        foreground_lease: None,
    };
    match try_connect(&t, &ring.chain, ring.desktop.clone()) {
        Err(RingError::Protocol(m)) => assert!(m.contains("too much"), "{m}"),
        other => panic!("expected a protocol error, got {:?}", other.err()),
    }
}

#[test]
fn empty_tls_records_do_not_starve_writes_keepalive_or_bye() {
    let (r, tls) = tls_relay();
    let mut t = r.target();
    t.tls = Some(tls);
    let ring = TestRing::new(&t.url);
    let bye = Duration::from_millis(800);
    let (a, rec_a) = slow_dead(&t, &ring, ring.desktop.clone(), bye);
    assert!(r.fault(
        &ring.ring_id(),
        &ring.desktop.sign_key(),
        Fault::EmptyTlsRecords
    ));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !r.flooding() {
        assert!(Instant::now() < deadline, "the flood never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    // Keepalive: pings keep reaching the socket.
    let before = r.raw_rx_bytes();
    std::thread::sleep(Duration::from_millis(1000));
    let pinged = r.raw_rx_bytes() - before;
    assert!(pinged >= 3 * 30, "only {pinged} bytes of pings in 1 s");
    // Sends still go out.
    let before = r.raw_rx_bytes();
    a.send(&ring.daemon.sign_key(), &vec![5u8; 8 * 1024])
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while r.raw_rx_bytes() - before < 8 * 1024 {
        assert!(Instant::now() < deadline, "the envelope never left");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The goodbye deadline holds.
    let started = Instant::now();
    assert_eq!(a.bye(ByeReason::quit()), Err(RingError::Timeout));
    assert!(started.elapsed() < bye + Duration::from_millis(600));
    assert_eq!(
        rec_a.closed(Duration::from_millis(300)),
        Some(CloseReason::Bye)
    );
}

#[test]
fn empty_auth_chain_is_a_bad_request() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let mut c = RawConn::open(&t, &ring.ring_id()).unwrap();
    let _ = c.challenge();
    c.auth_chain(&[]);
    assert_eq!(
        c.error(WAIT).map(|e| e.0),
        Some(xshell_protocol::ring::relay::ErrorCode::BadRequest)
    );
    assert_eq!(c.close_code(WAIT), Some(4000));
    assert_eq!(r.staged_chunks(), 0);
}

#[test]
fn staging_caps_physical_chunks_and_stored_bytes() {
    let ring_relay = |opts: TestRelayOptions| TestRelay::start_with(opts);
    // At most two chunks: three one-token frames are refused.
    let r = ring_relay(TestRelayOptions {
        max_stage_chunks: 2,
        ..TestRelayOptions::default()
    });
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let mut c = RawConn::open(&t, &ring.ring_id()).unwrap();
    let _ = c.challenge();
    c.auth_chain(&[ring.chain.genesis()]);
    c.auth_chain(&[ring.chain.head()]);
    c.auth_chain(&[ring.chain.head()]);
    let e = c.error(WAIT).expect("refused");
    assert_eq!(e.2.as_deref(), Some("too_large"));
    assert_eq!(c.close_code(WAIT), Some(4003));
    // A cap on serialized bytes below one token's size refuses the first frame.
    let r = ring_relay(TestRelayOptions {
        max_stage_bytes: 100,
        ..TestRelayOptions::default()
    });
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let mut c = RawConn::open(&t, &ring.ring_id()).unwrap();
    let _ = c.challenge();
    c.auth_chain(&[ring.chain.genesis()]);
    assert_eq!(
        c.error(WAIT).and_then(|e| e.2).as_deref(),
        Some("too_large")
    );
    assert_eq!(r.staged_chunks(), 0);
}

#[test]
fn staged_chunk_gap_fails_closed() {
    let r = relay();
    let t = r.target();
    let ring = TestRing::new(&t.url);
    let mut c = RawConn::open(&t, &ring.ring_id()).unwrap();
    let (nonce, _) = c.challenge();
    c.auth_chain(&[ring.chain.genesis()]);
    c.auth_chain(&[ring.chain.head()]);
    std::thread::sleep(QUIET);
    assert_eq!(r.staged_chunks(), 2);
    r.drop_staged_chunk(0);
    c.auth(&*ring.desktop, &t.origin(), &ring.ring_id(), &nonce);
    assert_eq!(c.error(WAIT).and_then(|e| e.2).as_deref(), Some("invalid"));
    assert_eq!(c.close_code(WAIT), Some(4003));
    assert_eq!(r.head_version(&ring.ring_id()), None);
    assert_eq!(r.staged_chunks(), 0);
}

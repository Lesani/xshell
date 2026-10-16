//! The Connector against the in-process test Relay: reconnect with backoff, goodbye, Relay
//! moves (followed, and farewells to the old Relay), removal, and fenced attempts.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use xshell_protocol::ring::entitlement::{sign_entitlement, GatewayKeys, Tier};
use xshell_protocol::ring::relay::contract::{
    config, connect, fake_entitlement, keys, now, TestRing, QUIET, WAIT,
};
use xshell_protocol::ring::relay::test_relay::{
    Fault, GateProxy, TestRelay, TestRelayOptions, TrickleProxy,
};
use xshell_protocol::ring::relay::wire::{ByeReason, ErrorCode, MemberPresence};
use xshell_protocol::ring::relay::{
    Connector, ConnectorConfig, ConnectorEvents, Farewell, FarewellState, LinkState, RingTimeouts,
};
use xshell_protocol::ring::{DeviceKeys, RingError, RingId, RosterChain, SignKey, Signer};

#[derive(Clone, Debug)]
enum Ev {
    State(LinkState),
    Chain(u64),
    Farewell(FarewellState),
    Held(String, u64),
    Entitlement(Option<String>),
    Error(ErrorCode, Option<SignKey>),
    Accepted,
}

#[derive(Default)]
struct Rec {
    ev: Mutex<Vec<Ev>>,
    cv: Condvar,
}

impl Rec {
    fn new() -> Arc<Rec> {
        Arc::new(Rec::default())
    }

    fn all(&self) -> Vec<Ev> {
        self.ev.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.ev.lock().unwrap().clear();
    }

    /// Waits for an event matching `pred`, seen or arriving within `t`.
    fn wait(&self, t: Duration, pred: impl Fn(&Ev) -> bool) -> Option<Ev> {
        let deadline = Instant::now() + t;
        let mut ev = self.ev.lock().unwrap();
        loop {
            if let Some(e) = ev.iter().find(|e| pred(e)) {
                return Some(e.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            ev = self.cv.wait_timeout(ev, left).unwrap().0;
        }
    }

    fn connected(&self) -> bool {
        self.wait(WAIT, |e| {
            matches!(e, Ev::State(LinkState::Connected { .. }))
        })
        .is_some()
    }
}

impl ConnectorEvents for Rec {
    fn state(&self, s: &LinkState) {
        self.ev.lock().unwrap().push(Ev::State(s.clone()));
        self.cv.notify_all();
    }
    fn roster(&self, c: &RosterChain) {
        self.ev.lock().unwrap().push(Ev::Chain(c.head().version()));
        self.cv.notify_all();
    }
    fn farewell(&self, f: &FarewellState) {
        self.ev.lock().unwrap().push(Ev::Farewell(f.clone()));
        self.cv.notify_all();
    }
    fn held(&self, relay: &str, version: u64) {
        self.ev
            .lock()
            .unwrap()
            .push(Ev::Held(relay.to_string(), version));
        self.cv.notify_all();
    }
    fn entitlement(&self, token: Option<&str>) {
        self.ev
            .lock()
            .unwrap()
            .push(Ev::Entitlement(token.map(str::to_string)));
        self.cv.notify_all();
    }
    fn error(&self, code: &ErrorCode, to: Option<SignKey>) {
        self.ev.lock().unwrap().push(Ev::Error(code.clone(), to));
        self.cv.notify_all();
    }
    fn accepted(&self) {
        self.ev.lock().unwrap().push(Ev::Accepted);
        self.cv.notify_all();
    }
}

/// A Relay behind a `TrickleProxy`: the trickle alone takes about half a second up to the
/// challenge, so the 500 ms auth deadline of `relay()` would race it; this one keeps the
/// default 10 s.
fn slow_relay() -> TestRelay {
    TestRelay::start()
}

fn relay() -> TestRelay {
    TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        ..TestRelayOptions::default()
    })
}

fn timeouts() -> RingTimeouts {
    RingTimeouts {
        connect: Duration::from_secs(5),
        ping_interval: Duration::from_millis(300),
        dead_after: Duration::from_secs(3),
        request: Duration::from_secs(2),
        bye: Duration::from_millis(800),
    }
}

fn cfg(r: &TestRelay, chain: &RosterChain, who: Arc<dyn Signer>) -> ConnectorConfig {
    let mut c = ConnectorConfig::new(config(&r.target(), chain, who));
    c.client.timeouts = timeouts();
    c.backoff_unit = Duration::from_millis(50);
    c.stable_after = Duration::from_secs(30);
    c
}

fn start(c: ConnectorConfig) -> (Connector, Arc<Rec>) {
    let rec = Rec::new();
    (Connector::start(c, rec.clone()).unwrap(), rec)
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn online(r: &TestRelay, ring: &TestRing, key: &SignKey) -> bool {
    r.presence(&ring.ring_id(), key).is_some_and(|p| p.online)
}

#[test]
fn reconnects_after_kick_with_backoff() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let key = ring.daemon.sign_key();
    let (c, rec) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
    assert!(rec.connected());
    assert_eq!(r.head_version(&ring.ring_id()), Some(2), "chain staged");

    for (n, wait) in [(1, 50u64), (2, 100)] {
        rec.clear();
        assert!(r.kick(&ring.ring_id(), &key, 1011));
        let w = rec
            .wait(WAIT, |e| matches!(e, Ev::State(LinkState::Waiting { .. })))
            .expect("waiting after the kick");
        match w {
            Ev::State(LinkState::Waiting { retry_in, .. }) => {
                assert_eq!(retry_in, Duration::from_millis(wait), "kick {n}")
            }
            other => panic!("{other:?}"),
        }
        assert!(rec.connected(), "reconnected after kick {n}");
        wait_until("online again", || online(&r, &ring, &key));
    }
    c.stop(ByeReason::quit());
}

#[test]
fn stable_connection_resets_backoff() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let key = ring.daemon.sign_key();
    let mut cc = cfg(&r, &ring.chain, ring.daemon.clone());
    cc.stable_after = Duration::ZERO;
    let (c, rec) = start(cc);
    assert!(rec.connected());
    for _ in 0..3 {
        rec.clear();
        assert!(r.kick(&ring.ring_id(), &key, 1011));
        assert!(rec.connected());
        assert!(
            !rec.all()
                .iter()
                .any(|e| matches!(e, Ev::State(LinkState::Waiting { .. }))),
            "a stable connection retries at once: {:?}",
            rec.all()
        );
        assert!(rec
            .all()
            .iter()
            .any(|e| matches!(e, Ev::State(LinkState::Connecting { attempt: 0 }))));
    }
    c.stop(ByeReason::quit());
}

#[test]
fn stop_says_bye_and_relay_records_reason() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let key = ring.daemon.sign_key();
    for reason in [ByeReason::quit(), ByeReason::idle(), ByeReason::upgrade()] {
        let (c, rec) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
        assert!(rec.connected());
        wait_until("online", || online(&r, &ring, &key));
        c.stop(reason.clone());
        let p = r.presence(&ring.ring_id(), &key).unwrap();
        assert!(!p.online);
        assert_eq!(p.last_reason.as_deref(), Some(reason.as_str()));
        assert_eq!(c.state(), LinkState::Stopped { error: None });
        // Idempotent.
        c.stop(ByeReason::quit());
    }
}

#[test]
fn dropping_without_stop_reports_unreachable() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let key = ring.daemon.sign_key();
    let (c, rec) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
    assert!(rec.connected());
    c.abandon();
    // No goodbye: the Relay notices the dropped socket on its own, shortly after.
    wait_until("unreachable", || {
        r.presence(&ring.ring_id(), &key)
            .is_some_and(|p| matches!(MemberPresence::from(&p), MemberPresence::Unreachable { .. }))
    });
}

#[test]
fn new_head_with_other_relay_url_moves_connection() {
    let (a, b) = (relay(), relay());
    let mut ring = TestRing::new(&a.url());
    let key = ring.daemon.sign_key();
    let (c, rec) = start(cfg(&a, &ring.chain, ring.daemon.clone()));
    assert!(rec.connected());
    let (desk, _) = connect(&a.target(), &ring.chain, ring.desktop.clone());
    let v3 = ring.next(|d| d.relay_url = b.url());
    rec.clear();
    desk.publish_roster(&v3).unwrap();
    assert!(rec.wait(WAIT, |e| matches!(e, Ev::Chain(3))).is_some());
    assert!(rec.connected(), "connected again, to the new Relay");
    wait_until("on the new Relay", || online(&b, &ring, &key));
    // The new Relay got the whole chain staged.
    assert_eq!(b.head_version(&ring.ring_id()), Some(3));
    assert_eq!(c.chain().head().version(), 3);
    c.stop(ByeReason::quit());
}

#[test]
fn removed_or_not_member_stops_without_retry() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let mobile = ring.mobile.sign_key();
    // Connected, then removed.
    let (c, rec) = start(cfg(&r, &ring.chain, ring.mobile.clone()));
    assert!(rec.connected());
    let (desk, _) = connect(&r.target(), &ring.chain, ring.desktop.clone());
    let v3 = ring.next(|d| {
        d.remove(&mobile);
    });
    desk.publish_roster(&v3).unwrap();
    assert!(rec
        .wait(WAIT, |e| matches!(
            e,
            Ev::State(LinkState::Stopped { error: Some(_) })
        ))
        .is_some());
    rec.clear();
    std::thread::sleep(Duration::from_millis(400));
    assert!(rec.all().is_empty(), "no retry: {:?}", rec.all());
    c.stop(ByeReason::quit());

    // Removed while away: the Relay's head no longer lists it.
    let (c, rec) = start(cfg(&r, &ring.up_to(2), ring.mobile.clone()));
    let e = rec
        .wait(WAIT, |e| matches!(e, Ev::State(LinkState::Stopped { .. })))
        .unwrap();
    assert!(
        matches!(&e, Ev::State(LinkState::Stopped { error: Some(m) }) if m.contains("not_member")),
        "{e:?}"
    );
    rec.clear();
    std::thread::sleep(Duration::from_millis(400));
    assert!(rec.all().is_empty(), "no retry: {:?}", rec.all());
    c.stop(ByeReason::quit());
}

#[test]
fn publish_while_disconnected_is_err() {
    // Nothing listens there.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://127.0.0.1:{}", dead.local_addr().unwrap().port());
    drop(dead);
    let mut ring = TestRing::new(&url);
    let mut cc = ConnectorConfig::new(xshell_protocol::ring::relay::RingClientConfig::new(
        ring.chain.clone(),
        ring.desktop.clone(),
    ));
    cc.client.timeouts = timeouts();
    cc.backoff_unit = Duration::from_millis(50);
    let (c, rec) = start(cc);
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::State(LinkState::Waiting { .. })))
        .is_some());
    let v3 = ring.next(|_| {});
    assert!(matches!(c.publish(&v3), Err(RingError::Closed(_))));
    assert!(c.members().is_none());
    c.stop(ByeReason::quit());
}

#[test]
fn set_chain_publishes_new_versions_on_the_same_relay() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    ring.next(|_| {});
    ring.next(|_| {});
    c.set_chain(ring.chain.clone());
    wait_until("published", || r.head_version(&ring.ring_id()) == Some(4));
    c.stop(ByeReason::quit());
}

#[test]
fn chain_is_reported_whole_after_catch_up() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let (desk, _) = connect(&r.target(), &ring.chain, ring.desktop.clone());
    for _ in 0..3 {
        let v = ring.next(|_| {});
        desk.publish_roster(&v).unwrap();
    }
    // A daemon that only knew version 2 catches up during connect.
    let (c, rec) = start(cfg(&r, &ring.up_to(2), ring.daemon.clone()));
    assert!(rec.wait(WAIT, |e| matches!(e, Ev::Chain(5))).is_some());
    assert_eq!(c.chain(), ring.chain);
    c.stop(ByeReason::quit());
}

#[test]
fn stop_during_authentication_then_reconnect() {
    let r = slow_relay();
    // Every byte from the Relay arrives slowly, so the connect is still authenticating when
    // stop comes.
    let proxy = TrickleProxy::start(r.addr(), 8, Duration::from_millis(15));
    let url = format!("ws://127.0.0.1:{}", proxy.addr().port());
    r.set_origin(&url);
    let ring = TestRing::new(&url);
    let key = ring.daemon.sign_key();
    let mut cc = ConnectorConfig::new(xshell_protocol::ring::relay::RingClientConfig::new(
        ring.chain.clone(),
        ring.daemon.clone(),
    ));
    cc.client.timeouts = timeouts();
    cc.backoff_unit = Duration::from_millis(50);
    let (old, orec) = start(cc.clone());
    assert!(orec
        .wait(WAIT, |e| matches!(
            e,
            Ev::State(LinkState::Connecting { .. })
        ))
        .is_some());
    std::thread::sleep(Duration::from_millis(100));
    old.stop(ByeReason::quit());
    // Stop returned: the old attempt ended (it may have authenticated and said goodbye).
    assert!(
        !online(&r, &ring, &key),
        "nothing of the old attempt is online"
    );
    orec.clear();

    let (new, nrec) = start(cc);
    assert!(nrec
        .wait(Duration::from_secs(10), |e| matches!(
            e,
            Ev::State(LinkState::Connected { .. })
        ))
        .is_some());
    // Nothing of the old attempt replaces the new socket later.
    std::thread::sleep(Duration::from_millis(800));
    assert!(online(&r, &ring, &key));
    assert!(
        !nrec
            .all()
            .iter()
            .any(|e| matches!(e, Ev::State(LinkState::Waiting { .. }))),
        "{:?}",
        nrec.all()
    );
    assert!(orec.all().is_empty(), "the stopped attempt stays silent");
    new.stop(ByeReason::quit());
}

// ---- publish_until (xshell#38) ----------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn publish_until_succeeds_when_sync_published_first() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    for _ in 0..5 {
        let next = ring.next(|_| {});
        // The worker's sync and this call race to upload the same version.
        c.set_chain(ring.chain.clone());
        c.publish_until(&next, Instant::now() + WAIT, None)
            .expect("published");
        assert_eq!(r.head_version(&ring.ring_id()), Some(next.version()));
    }
    c.stop(ByeReason::quit());
}

#[test]
fn publish_until_waits_for_connection() {
    let r = relay();
    // Ring authentication fails (the signed origin is wrong) until the origin is restored.
    let real = r.origin();
    r.set_origin("https://wrong.example");
    let mut ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::State(LinkState::Waiting { .. })))
        .is_some());
    let next = ring.next(|_| {});
    c.set_chain(ring.chain.clone());
    assert!(c.connection().is_none());
    assert!(
        matches!(c.publish(&next), Err(RingError::Closed(_))),
        "plain publish fails at once"
    );
    let started = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            r.set_origin(&real);
        });
        c.publish_until(&next, Instant::now() + WAIT, None)
            .expect("published once connected");
    });
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(r.head_version(&ring.ring_id()), Some(next.version()));
    c.stop(ByeReason::quit());
}

#[test]
fn publish_until_is_bounded_while_puts_go_unanswered() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    r.ignore_roster_puts(true);
    let next = ring.next(|_| {});
    // Far below the client's 2 s request timeout.
    let t = Instant::now();
    let e = c
        .publish_until(&next, t + Duration::from_millis(400), None)
        .unwrap_err();
    assert!(matches!(e, RingError::Timeout), "{e:?}");
    let took = t.elapsed();
    assert!(
        took < Duration::from_millis(1200),
        "bounded by the deadline: {took:?}"
    );
    // Cancellation ends a wait for an answer, too.
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let e = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(200));
            cancel.store(true, Ordering::Release);
        });
        c.publish_until(&next, t + WAIT, Some(&cancel)).unwrap_err()
    });
    assert!(matches!(e, RingError::Closed(_)), "{e:?}");
    assert!(
        t.elapsed() < Duration::from_millis(1200),
        "{:?}",
        t.elapsed()
    );
    // Already cancelled: no success reported even though the Relay would accept it.
    r.ignore_roster_puts(false);
    let e = c
        .publish_until(&next, Instant::now() + WAIT, Some(&cancel))
        .unwrap_err();
    assert!(matches!(e, RingError::Closed(_)), "{e:?}");
    c.stop(ByeReason::quit());
}

#[test]
fn publish_until_waits_for_the_versions_before_it() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    // The worker's upload of v3 goes unanswered: the client stays at v2.
    r.ignore_roster_puts(true);
    ring.next(|_| {});
    c.set_chain(ring.chain.clone());
    let v4 = ring.next(|_| {});
    c.set_chain(ring.chain.clone());
    // A version that skips one and is not in the trusted chain is refused at once.
    let alt3 = ring
        .up_to(2)
        .head()
        .next(&*ring.desktop, 1, |d| {
            d.relay_url = "ws://127.0.0.1:1".into()
        })
        .unwrap();
    let alt4 = alt3.next(&*ring.desktop, 2, |_| {}).unwrap();
    let t = Instant::now();
    let e = c.publish_until(&alt4, t + WAIT, None).unwrap_err();
    assert!(matches!(e, RingError::Roster(_)), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(1));
    let t = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            r.ignore_roster_puts(false);
        });
        // v4 cannot go before v3 (a Gap for the client): it waits for ordered sync.
        c.publish_until(&v4, Instant::now() + Duration::from_secs(8), None)
            .expect("published after v3");
    });
    assert!(t.elapsed() >= Duration::from_millis(300), "it waited");
    assert_eq!(r.head_version(&ring.ring_id()), Some(4));
    c.stop(ByeReason::quit());
}

#[test]
fn publish_until_accepts_exactly_a_version_the_relay_moved_past() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let me = ring.desktop.sign_key();
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    wait_until("online", || online(&r, &ring, &me));
    // This client stops hearing broadcasts, so it never sees what the other Desktop
    // publishes; the Relay's direct answers still reach it.
    assert!(r.fault(&ring.ring_id(), &me, Fault::Mute));
    let v2 = ring.chain.head().clone();
    let v3 = ring.next(|_| {});
    ring.next(|_| {});
    let (other, orec) = start(cfg(&r, &ring.up_to(2), ring.desktop2.clone()));
    assert!(orec.connected());
    other.set_chain(ring.chain.clone());
    wait_until("v4 on the Relay", || {
        r.head_version(&ring.ring_id()) == Some(4)
    });
    // The exact historical bytes: the Relay answers `roster_stale` while this client has
    // not seen v3; it retries, and once a new connection brings it the Relay's chain, v3 is
    // held.
    let stale_before = r.stale_roster_puts();
    std::thread::scope(|s| {
        s.spawn(|| {
            wait_until("a stale answer", || r.stale_roster_puts() > stale_before);
            std::thread::sleep(Duration::from_millis(100));
            assert!(r.kick(&ring.ring_id(), &me, 1011));
        });
        c.publish_until(&v3, Instant::now() + WAIT, None)
            .expect("v3 is held");
    });
    assert!(r.stale_roster_puts() > stale_before);
    // Different bytes at version 3: refused, not waited out.
    let alt = v2
        .next(&*ring.desktop, 1, |d| {
            d.relay_url = "ws://127.0.0.1:1".into()
        })
        .unwrap();
    assert_eq!(alt.version(), 3);
    assert_ne!(alt.token(), v3.token());
    let t = Instant::now();
    let e = c.publish_until(&alt, t + WAIT, None).unwrap_err();
    assert!(matches!(e, RingError::Roster(_)), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    other.stop(ByeReason::quit());
    c.stop(ByeReason::quit());
}

// ---- Entitlement (xshell-remote#14) ---------------------------------------------------------

/// A Hosted test Relay and the gateway key it trusts.
fn hosted_relay() -> (TestRelay, Arc<DeviceKeys>) {
    let gw = keys();
    let r = TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        hosted: Some(GatewayKeys::new(&[gw.sign_key()])),
        ..TestRelayOptions::default()
    });
    (r, gw)
}

fn token(gw: &DeviceKeys, ring: &RingId, tier: Tier, expires_at: u64) -> String {
    sign_entitlement(gw, ring, tier, "purchase", now(), expires_at).unwrap()
}

fn connected_as(rec: &Rec, limited: bool) -> bool {
    rec.wait(
        WAIT,
        |e| matches!(e, Ev::State(LinkState::Connected { limited: l }) if *l == limited),
    )
    .is_some()
}

/// The events after the first `Connected` report.
fn after_connected(rec: &Rec) -> Vec<Ev> {
    rec.all()
        .into_iter()
        .skip_while(|e| !matches!(e, Ev::State(LinkState::Connected { .. })))
        .skip(1)
        .collect()
}

#[test]
fn connector_put_entitlement_lifts_limit() {
    let (r, gw) = hosted_relay();
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(connected_as(&rec, true));
    // The welcome's empty slot follows the `Connected` report.
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::Entitlement(None)))
        .is_some());
    assert!(matches!(
        after_connected(&rec).first(),
        Some(Ev::Entitlement(None))
    ));
    assert!(!c.routing());
    assert_eq!(c.entitlement(), None);

    let t = token(&gw, &ring.ring_id(), Tier::Hosted, now() + 3600);
    // In two steps: queued at once, answered through the ticket.
    c.put_entitlement_start(&t)
        .expect("queued")
        .wait(WAIT)
        .expect("put");
    assert!(connected_as(&rec, false));
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::Entitlement(Some(x)) if *x == t))
        .is_some());
    wait_until("routing", || c.routing());
    assert_eq!(c.entitlement().as_deref(), Some(t.as_str()));
    // A token the Hosted Relay refuses comes back as its error.
    let foreign = token(&keys(), &ring.ring_id(), Tier::Hosted, now() + 3600);
    assert!(matches!(
        c.put_entitlement(&foreign),
        Err(RingError::Relay {
            code: ErrorCode::EntitlementInvalid,
            ..
        })
    ));
    c.stop(ByeReason::quit());
}

#[test]
fn connector_reports_entitlement_to_other_member() {
    let (r, gw) = hosted_relay();
    let ring = TestRing::new(&r.url());
    let (a, rec_a) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    let (b, rec_b) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
    assert!(connected_as(&rec_a, true) && connected_as(&rec_b, true));
    let t = token(&gw, &ring.ring_id(), Tier::Hosted, now() + 3600);
    a.put_entitlement(&t).expect("put");
    // The other member learns the token from the broadcast, without putting anything.
    assert!(rec_b
        .wait(WAIT, |e| matches!(e, Ev::Entitlement(Some(x)) if *x == t))
        .is_some());
    assert!(connected_as(&rec_b, false));
    assert_eq!(b.entitlement().as_deref(), Some(t.as_str()));
    wait_until("b routing", || b.routing());
    // A member connecting later gets it from `welcome`, right after `Connected`.
    let (m, rec_m) = start(cfg(&r, &ring.chain, ring.mobile.clone()));
    assert!(connected_as(&rec_m, false));
    assert!(rec_m
        .wait(WAIT, |e| matches!(e, Ev::Entitlement(Some(x)) if *x == t))
        .is_some());
    assert!(matches!(
        after_connected(&rec_m).first(),
        Some(Ev::Entitlement(Some(x))) if *x == t
    ));
    assert_eq!(m.entitlement().as_deref(), Some(t.as_str()));
    // A worse token is acknowledged but the slot keeps the better one (section 12).
    let push = token(&gw, &ring.ring_id(), Tier::Push, now() + 7200);
    b.put_entitlement(&push).expect("acknowledged");
    std::thread::sleep(QUIET);
    assert_eq!(a.entitlement().as_deref(), Some(t.as_str()));
    assert!(a.routing());
    for c in [a, b, m] {
        c.stop(ByeReason::quit());
    }
}

#[test]
fn connector_put_entitlement_not_connected() {
    // Never connected.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://127.0.0.1:{}", dead.local_addr().unwrap().port());
    drop(dead);
    let ring = TestRing::new(&url);
    let mut cc = ConnectorConfig::new(xshell_protocol::ring::relay::RingClientConfig::new(
        ring.chain.clone(),
        ring.desktop.clone(),
    ));
    cc.client.timeouts = timeouts();
    cc.backoff_unit = Duration::from_millis(50);
    let (c, rec) = start(cc);
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::State(LinkState::Waiting { .. })))
        .is_some());
    let t = fake_entitlement(&ring.ring_id());
    assert!(matches!(c.put_entitlement(&t), Err(RingError::Closed(_))));
    assert!(matches!(
        c.put_entitlement_start(&t),
        Err(RingError::Closed(_))
    ));
    assert_eq!(c.entitlement(), None);
    c.stop(ByeReason::quit());

    // Dropped and waiting out a long backoff: refused, and nothing is put on the next
    // connection.
    let (r, gw) = hosted_relay();
    let ring = TestRing::new(&r.url());
    let mut cc = cfg(&r, &ring.chain, ring.desktop.clone());
    cc.backoff_unit = Duration::from_secs(30);
    let (c, rec) = start(cc);
    assert!(connected_as(&rec, true));
    rec.clear();
    assert!(r.kick(&ring.ring_id(), &ring.desktop.sign_key(), 1011));
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::State(LinkState::Waiting { .. })))
        .is_some());
    let t = token(&gw, &ring.ring_id(), Tier::Hosted, now() + 3600);
    assert!(matches!(c.put_entitlement(&t), Err(RingError::Closed(_))));
    assert_eq!(c.entitlement(), None);
    rec.clear();
    c.kick();
    assert!(connected_as(&rec, true));
    std::thread::sleep(QUIET);
    assert!(!c.routing());
    assert_eq!(c.entitlement(), None);
    let (probe, _) = connect(&r.target(), &ring.chain, ring.mobile.clone());
    assert_eq!(probe.entitlement(), None);
    assert!(probe.limited());
    c.stop(ByeReason::quit());
}

#[test]
fn connector_quota_refusal_of_a_request_reaches_error() {
    let r = TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        quota_frames_per_day: Some(6),
        ..TestRelayOptions::default()
    });
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    let t = fake_entitlement(&ring.ring_id());
    let mut refused = None;
    for _ in 0..20 {
        match c.put_entitlement(&t) {
            Ok(()) => {}
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    // The request gets its answer …
    assert!(
        matches!(
            refused,
            Some(RingError::Relay {
                code: ErrorCode::Quota,
                ..
            })
        ),
        "{refused:?}"
    );
    // … and the shared path hears of it too.
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::Error(ErrorCode::Quota, None)))
        .is_some());
    c.stop(ByeReason::quit());
}

// ---- Quota probes (xshell#42) -------------------------------------------------------------

use xshell_protocol::ring::relay::contract::{member, raw_next, Event, RawConn, Recorder};
use xshell_protocol::ring::relay::wire::{ClientFrame, RelayFrame};
use xshell_protocol::ring::relay::RingClient;
use xshell_protocol::ring::Role;

fn quota_relay(limit: u64) -> TestRelay {
    TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        quota_frames_per_day: Some(limit),
        ..TestRelayOptions::default()
    })
}

/// Uses up the Ring's quota for the day from the phone's own socket.
fn use_up_quota(r: &TestRelay, ring: &TestRing) {
    let mut m = RawConn::login(&r.target(), &ring.chain, &*ring.mobile);
    let head = ring.chain.head().version();
    for id in 1..=1000 {
        m.send(&ClientFrame::RosterGet { id, since: head }.encode())
            .expect("send roster.get");
        match m.frame(WAIT) {
            Some(RelayFrame::RosterChain { .. }) => {}
            Some(RelayFrame::Error {
                code: ErrorCode::Quota,
                ..
            }) => return,
            other => panic!("expected roster.chain or quota, got {other:?}"),
        }
    }
    panic!("the quota was never reached");
}

/// The Desktop connected to a Relay that leaves every `roster.get` unanswered: a probe's
/// only answers are the frames injected for it.
fn deaf(r: &TestRelay, ring: &TestRing) -> (RingClient, Arc<Recorder>) {
    let mut cfg = config(&r.target(), &ring.chain, ring.desktop.clone());
    cfg.timeouts.request = Duration::from_millis(800);
    let rec = Recorder::new();
    let c = RingClient::connect(cfg, rec.clone()).unwrap();
    r.ignore_roster_gets(true);
    (c, rec)
}

fn chain_frame(id: u64, rosters: Vec<String>) -> String {
    RelayFrame::RosterChain {
        id,
        rosters,
        more: false,
    }
    .encode()
}

#[test]
fn connector_probe_answers_ok_then_quota() {
    let r = quota_relay(8);
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    assert_eq!(c.probe_start().unwrap().wait(WAIT), Ok(()));
    use_up_quota(&r, &ring);
    match c.probe_start().unwrap().wait(WAIT) {
        Err(RingError::Relay {
            code: ErrorCode::Quota,
            ..
        }) => {}
        other => panic!("expected a quota refusal, got {other:?}"),
    }
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::Error(ErrorCode::Quota, None)))
        .is_some());
    c.stop(ByeReason::quit());
}

#[test]
fn connector_probe_when_not_connected_is_closed() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let mut cc = cfg(&r, &ring.chain, ring.desktop.clone());
    cc.backoff_unit = Duration::from_secs(30);
    let (c, rec) = start(cc);
    assert!(rec.connected());
    rec.clear();
    assert!(r.kick(&ring.ring_id(), &ring.desktop.sign_key(), 1011));
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::State(LinkState::Waiting { .. })))
        .is_some());
    assert!(matches!(c.probe_start(), Err(RingError::Closed(_))));
    c.stop(ByeReason::quit());
}

/// A `roster.chain` carrying another request's id leaves that request waiting, and an `ok`
/// carrying a probe's id leaves the probe waiting: each takes only its own kind of answer.
#[test]
fn probe_answers_are_typed() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let (c, _rec) = deaf(&r, &ring);
    let c = Arc::new(c);
    let (rid, me) = (ring.ring_id(), ring.desktop.sign_key());
    let probe = c.probe_start().unwrap();
    let ok = RelayFrame::Ok { id: probe.id() }.encode();
    assert!(r.inject(&rid, &me, &ok));
    assert_eq!(
        probe.wait(Duration::from_millis(600)),
        Err(RingError::Timeout),
        "an ok does not answer a probe"
    );
    // A roster.put the Relay leaves unanswered takes the next id.
    r.ignore_roster_puts(true);
    let next = c.probe_start().unwrap();
    let put_id = next.id() + 1;
    let v3 = ring.next(|x| x.add(member(&keys(), "d3", Role::Daemon)));
    let c2 = c.clone();
    let put = std::thread::spawn(move || c2.publish_roster(&v3));
    std::thread::sleep(QUIET);
    assert!(r.inject(&rid, &me, &chain_frame(put_id, Vec::new())));
    assert_eq!(
        put.join().unwrap(),
        Err(RingError::Timeout),
        "a chain does not answer a roster.put"
    );
    // The probe's own chain answers it.
    assert!(r.inject(&rid, &me, &chain_frame(next.id(), Vec::new())));
    assert_eq!(next.wait(WAIT), Ok(()));
}

/// A chain that fails verification still answers the probe (the Relay accepted the frame);
/// the refusal is reported as before and the trusted head stands.
#[test]
fn probe_answered_by_a_rejected_chain_is_accepted_traffic() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let (c, rec) = deaf(&r, &ring);
    let forged = raw_next(ring.chain.head(), &*ring.mobile, |_| {});
    let probe = c.probe_start().unwrap();
    assert!(r.inject(
        &ring.ring_id(),
        &ring.desktop.sign_key(),
        &chain_frame(probe.id(), vec![forged.token().to_string()]),
    ));
    assert_eq!(probe.wait(WAIT), Ok(()));
    assert!(rec
        .wait_for(WAIT, |e| matches!(e, Event::RosterRejected(_)))
        .is_some());
    assert_eq!(c.roster().version(), 2);
}

/// A chain with a gap answers the probe too; the client asks for the missing versions as
/// before and trusts nothing it cannot verify.
#[test]
fn probe_answered_by_a_chain_with_a_gap_is_accepted_traffic() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let (c, rec) = deaf(&r, &ring);
    let v3 = ring
        .chain
        .head()
        .next(&*ring.desktop, now(), |x| {
            x.add(member(&keys(), "d3", Role::Daemon))
        })
        .unwrap();
    let v4 = v3
        .next(&*ring.desktop, now(), |x| {
            x.add(member(&keys(), "d4", Role::Daemon))
        })
        .unwrap();
    let probe = c.probe_start().unwrap();
    assert!(r.inject(
        &ring.ring_id(),
        &ring.desktop.sign_key(),
        &chain_frame(probe.id(), vec![v4.token().to_string()]),
    ));
    assert_eq!(probe.wait(WAIT), Ok(()));
    assert_eq!(c.roster().version(), 2);
    assert!(!rec
        .events()
        .iter()
        .any(|e| matches!(e, Event::RosterRejected(_) | Event::Roster { .. })));
}

/// A Relay ahead of the client answers the probe with the newer versions: the probe is
/// answered and the client trusts them, through the same path as any chain.
#[test]
fn probe_of_a_relay_that_is_ahead_brings_the_newer_head() {
    let r = relay();
    let mut ring = TestRing::new(&r.url());
    let (c, rec) = connect(&r.target(), &ring.chain, ring.desktop.clone());
    // This socket misses the broadcast of v3.
    assert!(r.fault(&ring.ring_id(), &ring.desktop.sign_key(), Fault::Mute));
    let (other, _) = connect(&r.target(), &ring.chain, ring.desktop2.clone());
    let v3 = ring.next(|x| x.add(member(&keys(), "d3", Role::Daemon)));
    other.publish_roster(&v3).unwrap();
    assert_eq!(r.head_version(&ring.ring_id()), Some(3));
    std::thread::sleep(QUIET);
    assert_eq!(c.roster().version(), 2, "the broadcast was withheld");
    assert_eq!(c.probe_start().unwrap().wait(WAIT), Ok(()));
    assert!(rec
        .wait_for(WAIT, |e| matches!(e, Event::Roster { version: 3 }))
        .is_some());
    assert_eq!(c.roster().version(), 3);
}

/// An `ok` and a probe's `roster.chain` are reported as accepted frames, before the request
/// gets its answer; a quota refusal is not.
#[test]
fn connector_reports_accepted_frames() {
    let r = quota_relay(3);
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.desktop.clone()));
    assert!(rec.connected());
    let accepted = |rec: &Rec| {
        rec.all()
            .iter()
            .filter(|e| matches!(e, Ev::Accepted))
            .count()
    };
    rec.clear();
    c.put_entitlement(&fake_entitlement(&ring.ring_id()))
        .unwrap();
    assert_eq!(accepted(&rec), 1, "the ok");
    assert_eq!(c.probe_start().unwrap().wait(WAIT), Ok(()));
    assert_eq!(accepted(&rec), 2, "the chain");
    use_up_quota(&r, &ring);
    assert!(c.probe_start().unwrap().wait(WAIT).is_err());
    assert!(rec
        .wait(WAIT, |e| matches!(e, Ev::Error(ErrorCode::Quota, None)))
        .is_some());
    assert_eq!(accepted(&rec), 2, "a refusal is not accepted");
    c.stop(ByeReason::quit());
}

// ---- Quota refusals at connect and quota closes (xshell#43) ------------------------------

use xshell_protocol::ring::relay::wire::{close, QUOTA_REFUSALS_BEFORE_CLOSE};

fn quota_errors(rec: &Rec) -> Vec<Option<SignKey>> {
    rec.all()
        .into_iter()
        .filter_map(|e| match e {
            Ev::Error(ErrorCode::Quota, to) => Some(to),
            _ => None,
        })
        .collect()
}

fn is_waiting(e: &Ev) -> bool {
    matches!(e, Ev::State(LinkState::Waiting { .. }))
}

/// A Relay that is ahead of the Daemon's chain, with the Ring's quota used up: the Relay
/// refuses the catch-up the connect asks for.
fn ahead_and_over_quota() -> (TestRelay, TestRing) {
    let r = quota_relay(8);
    let mut ring = TestRing::new(&r.url());
    let (desk, _) = connect(&r.target(), &ring.chain, ring.desktop.clone());
    let v3 = ring.next(|_| {});
    desk.publish_roster(&v3).unwrap();
    use_up_quota(&r, &ring);
    (r, ring)
}

#[test]
fn connect_refused_for_quota_reports_quota() {
    let (r, ring) = ahead_and_over_quota();
    let (c, rec) = start(cfg(&r, &ring.up_to(2), ring.daemon.clone()));
    let waiting = rec
        .wait(WAIT, is_waiting)
        .expect("waiting after the refusal");
    match &waiting {
        Ev::State(LinkState::Waiting { error, .. }) => assert!(error.contains("quota"), "{error}"),
        other => panic!("{other:?}"),
    }
    let all = rec.all();
    let report = all
        .iter()
        .position(|e| matches!(e, Ev::Error(ErrorCode::Quota, None)))
        .expect("the refusal is reported as quota");
    let first_wait = all.iter().position(is_waiting).unwrap();
    assert!(report < first_wait, "reported before Waiting: {all:?}");
    // Each refused attempt reports it.
    wait_until("a second report", || quota_errors(&rec).len() >= 2);
    assert!(quota_errors(&rec).iter().all(Option::is_none));
    assert!(!rec
        .all()
        .iter()
        .any(|e| matches!(e, Ev::State(LinkState::Connected { .. }))));
    c.stop(ByeReason::quit());
}

#[test]
fn quota_close_after_refusals_reports_quota() {
    let r = quota_relay(8);
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
    assert!(rec.connected());
    use_up_quota(&r, &ring);
    rec.clear();
    let phone = ring.mobile.sign_key();
    for _ in 0..QUOTA_REFUSALS_BEFORE_CLOSE {
        c.send(&phone, b"x").unwrap();
    }
    assert!(rec.wait(WAIT, is_waiting).is_some(), "{:?}", rec.all());
    let all = rec.all();
    let first_wait = all.iter().position(is_waiting).unwrap();
    let before: Vec<_> = all[..first_wait]
        .iter()
        .filter_map(|e| match e {
            Ev::Error(ErrorCode::Quota, to) => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(before.len(), QUOTA_REFUSALS_BEFORE_CLOSE + 1, "{all:?}");
    assert!(before[..QUOTA_REFUSALS_BEFORE_CLOSE]
        .iter()
        .all(|to| *to == Some(phone)));
    assert_eq!(before[QUOTA_REFUSALS_BEFORE_CLOSE], None, "the close");
    // Authentication costs no quota: the Daemon connects again.
    assert!(rec.connected());
    c.stop(ByeReason::quit());
}

#[test]
fn quota_close_code_alone_reports_quota() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
    assert!(rec.connected());
    rec.clear();
    assert!(r.kick(&ring.ring_id(), &ring.daemon.sign_key(), close::QUOTA));
    assert!(rec.wait(WAIT, is_waiting).is_some());
    let all = rec.all();
    let report = all
        .iter()
        .position(|e| matches!(e, Ev::Error(ErrorCode::Quota, None)))
        .expect("the close is reported as quota");
    assert!(report < all.iter().position(is_waiting).unwrap(), "{all:?}");
    assert!(rec.connected());
    c.stop(ByeReason::quit());
}

#[test]
fn other_close_reports_no_quota() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
    assert!(rec.connected());
    rec.clear();
    assert!(r.kick(&ring.ring_id(), &ring.daemon.sign_key(), 1011));
    assert!(rec.connected());
    std::thread::sleep(QUIET);
    assert!(quota_errors(&rec).is_empty(), "{:?}", rec.all());
    c.stop(ByeReason::quit());
}

/// A stable connection retries at once after an ordinary drop (see
/// `stable_connection_resets_backoff`), but not after a close for quota.
#[test]
fn quota_close_backs_off_even_after_a_stable_connection() {
    let r = relay();
    let ring = TestRing::new(&r.url());
    let mut cc = cfg(&r, &ring.chain, ring.daemon.clone());
    cc.stable_after = Duration::ZERO;
    let (c, rec) = start(cc);
    assert!(rec.connected());
    rec.clear();
    assert!(r.kick(&ring.ring_id(), &ring.daemon.sign_key(), close::QUOTA));
    match rec.wait(WAIT, is_waiting) {
        Some(Ev::State(LinkState::Waiting { retry_in, .. })) => {
            assert_eq!(retry_in, Duration::from_millis(50))
        }
        other => panic!("{other:?}: {:?}", rec.all()),
    }
    assert!(rec.connected());
    c.stop(ByeReason::quit());
}

// ---- Farewells (xshell#51, ADR-0012) -----------------------------------------------------

/// A Desktop's Connector on `chain`, its farewells retried quickly.
fn desk_cfg(r: &TestRelay, ring: &TestRing, chain: &RosterChain) -> ConnectorConfig {
    let mut c = cfg(r, chain, ring.desktop.clone());
    c.farewell_backoff_unit = Duration::from_millis(50);
    c.farewell_backoff_max = Duration::from_millis(400);
    c
}

/// A Relay behind a gate: the Ring names the gate, and the Relay knows itself by its origin.
fn gated(r: &TestRelay) -> GateProxy {
    let g = GateProxy::start(r.addr());
    r.set_origin(&g.url());
    g
}

fn is_done(e: &Ev, source: &str) -> bool {
    matches!(e, Ev::Farewell(FarewellState::Done { farewell }) if farewell.source == source)
}

/// Waits for a `Waiting` report of the farewell to `source` after at least `failures`.
fn waiting(rec: &Rec, source: &str, failures: u32) -> FarewellState {
    match rec.wait(Duration::from_secs(15), |e| {
        matches!(e, Ev::Farewell(FarewellState::Waiting { farewell, failures: n, .. })
            if farewell.source == source && *n >= failures)
    }) {
        Some(Ev::Farewell(s)) => s,
        other => panic!(
            "no farewell waiting for {source}: {other:?} {:?}",
            rec.all()
        ),
    }
}

fn held(rec: &Rec, relay: &str, version: u64) -> bool {
    rec.wait(
        WAIT,
        |e| matches!(e, Ev::Held(r, v) if r == relay && *v == version),
    )
    .is_some()
}

fn link_states(rec: &Rec) -> Vec<LinkState> {
    rec.all()
        .into_iter()
        .filter_map(|e| match e {
            Ev::State(s) => Some(s),
            _ => None,
        })
        .collect()
}

#[test]
fn a_farewell_is_owed_only_while_the_head_names_another_relay() {
    let (a, b) = ("ws://127.0.0.1:1", "ws://127.0.0.1:2");
    let mut ring = TestRing::new(a);
    let here = Farewell::new(&ring.chain, 2).unwrap();
    assert!(
        !here.still_owed(&ring.chain),
        "the head names the same Relay"
    );
    ring.next(|d| d.relay_url = b.into());
    let f = Farewell::new(&ring.chain, 2).unwrap();
    assert_eq!(
        f,
        Farewell {
            source: a.into(),
            from: 2,
            target: 3
        }
    );
    assert!(f.still_owed(&ring.chain));
    assert!(Farewell::new(&ring.chain, 4).is_none());
    let other = Farewell {
        source: "ws://127.0.0.1:3".into(),
        ..f.clone()
    };
    assert!(
        !other.still_owed(&ring.chain),
        "version 2 names another Relay"
    );
    let backwards = Farewell {
        target: 2,
        ..f.clone()
    };
    assert!(!backwards.still_owed(&ring.chain));
    ring.next(|d| d.relay_url = a.into());
    assert!(!f.still_owed(&ring.chain), "the head names it again");
}

#[test]
fn held_is_reported_for_the_dialed_relay_on_connect() {
    let (a, b) = (relay(), relay());
    let mut ring = TestRing::new(&a.url());
    let (desk, rec) = start(desk_cfg(&a, &ring, &ring.chain));
    assert!(held(&rec, &a.url(), 2));
    ring.next(|d| d.relay_url = b.url());
    desk.set_chain(ring.chain.clone());
    assert!(held(&rec, &b.url(), 3));
    desk.stop(ByeReason::quit());
}

/// Only a Relay that holds a Ring counts its traffic: a refusal or a close for quota says
/// the Relay holds it, like a connection (D3, A4).
#[test]
fn quota_refusals_report_the_relay_held() {
    let (r, ring) = ahead_and_over_quota();
    let (c, rec) = start(cfg(&r, &ring.up_to(2), ring.daemon.clone()));
    assert!(held(&rec, &r.url(), 2), "{:?}", rec.all());
    assert!(!rec
        .all()
        .iter()
        .any(|e| matches!(e, Ev::State(LinkState::Connected { .. }))));
    c.stop(ByeReason::quit());

    let r = relay();
    let ring = TestRing::new(&r.url());
    let (c, rec) = start(cfg(&r, &ring.chain, ring.daemon.clone()));
    assert!(rec.connected());
    rec.clear();
    assert!(r.kick(&ring.ring_id(), &ring.daemon.sign_key(), close::QUOTA));
    assert!(rec.wait(WAIT, is_waiting).is_some());
    let all = rec.all();
    assert!(
        all.iter()
            .any(|e| matches!(e, Ev::Held(relay, 2) if *relay == r.url())),
        "{all:?}"
    );
    c.stop(ByeReason::quit());
}

#[test]
fn a_farewell_tells_the_old_relay_and_its_devices_follow() {
    let (a, b) = (relay(), relay());
    let mut ring = TestRing::new(&a.url());
    let (desk, drec) = start(desk_cfg(&a, &ring, &ring.chain));
    let (dmn, mrec) = start(cfg(&a, &ring.chain, ring.daemon.clone()));
    assert!(drec.connected() && mrec.connected());
    ring.next(|d| d.relay_url = b.url());
    let f = Farewell::new(&ring.chain, 2).unwrap();
    assert_eq!(f.source, a.url());
    desk.set_chain(ring.chain.clone());
    desk.set_farewells(vec![f.clone()]);
    assert!(drec
        .wait(WAIT, |e| matches!(
            e,
            Ev::Farewell(FarewellState::Done { farewell }) if *farewell == f
        ))
        .is_some());
    assert_eq!(
        a.head_version(&ring.ring_id()),
        Some(3),
        "the old Relay knows"
    );
    assert!(held(&drec, &a.url(), 3));
    wait_until("both on the new Relay", || {
        online(&b, &ring, &ring.desktop.sign_key()) && online(&b, &ring, &ring.daemon.sign_key())
    });
    assert!(desk.farewells().is_empty());
    desk.stop(ByeReason::quit());
    dmn.stop(ByeReason::quit());
}

#[test]
fn the_link_follows_the_head_while_the_old_relay_refuses_the_farewell() {
    let (a, b) = (relay(), relay());
    let ga = gated(&a);
    let mut ring = TestRing::new(&ga.url());
    let key = ring.desktop.sign_key();
    // The old Relay holds the Ring, then goes away.
    let (d, _) = connect(&a.target(), &ring.chain, ring.daemon.clone());
    drop(d);
    ga.set_open(false);
    ring.next(|d| d.relay_url = b.url());
    let mut cc = desk_cfg(&b, &ring, &ring.chain);
    // Owed from the start (as after a restart with it on disk).
    cc.farewells = vec![Farewell::new(&ring.chain, 2).unwrap()];
    let (desk, rec) = start(cc);
    assert!(rec.connected());
    wait_until("on the new Relay", || online(&b, &ring, &key));
    waiting(&rec, &ga.url(), 2);
    assert_eq!(a.head_version(&ring.ring_id()), Some(2));
    assert!(matches!(
        desk.farewells().as_slice(),
        [FarewellState::Waiting { .. } | FarewellState::Sending { .. }]
    ));
    // The old Relay comes back: the farewell goes out, the link never moved.
    ga.set_open(true);
    assert!(rec.wait(WAIT, |e| is_done(e, &ga.url())).is_some());
    assert_eq!(a.head_version(&ring.ring_id()), Some(3));
    assert!(
        !link_states(&rec)
            .iter()
            .any(|s| matches!(s, LinkState::Waiting { .. })),
        "{:?}",
        rec.all()
    );
    desk.stop(ByeReason::quit());
}

#[test]
fn a_dead_old_relay_never_delays_the_link() {
    let b = relay();
    let (closed, invalid) = ("ws://127.0.0.1:1", "wss://relay.xshell.invalid");
    let mut ring = TestRing::new(closed);
    ring.next(|d| d.relay_url = invalid.into());
    ring.next(|d| d.relay_url = b.url());
    let mut cc = desk_cfg(&b, &ring, &ring.chain);
    cc.farewell_backoff_unit = Duration::from_millis(10);
    cc.farewell_backoff_max = Duration::from_millis(200);
    cc.farewells = vec![
        Farewell::new(&ring.chain, 2).unwrap(),
        Farewell::new(&ring.chain, 3).unwrap(),
    ];
    let (desk, rec) = start(cc);
    assert!(
        rec.wait(Duration::from_secs(2), |e| matches!(
            e,
            Ev::State(LinkState::Connected { .. })
        ))
        .is_some(),
        "connected at once: {:?}",
        rec.all()
    );
    wait_until("on the new Relay", || {
        online(&b, &ring, &ring.desktop.sign_key())
    });
    waiting(&rec, closed, 2);
    waiting(&rec, invalid, 2);
    // Every link state is about the head's Relay: one attempt, connected.
    assert_eq!(
        link_states(&rec),
        [
            LinkState::Connecting { attempt: 0 },
            LinkState::Connected { limited: false }
        ]
    );
    desk.stop(ByeReason::quit());
}

/// A → B → C while A is unreachable: once A is back, its farewell brings it straight to C,
/// so a device that only knew A never visits B.
#[test]
fn a_farewell_carries_the_newest_head() {
    let (a, b, c) = (relay(), relay(), relay());
    let ga = gated(&a);
    let mut ring = TestRing::new(&ga.url());
    let (desk, rec) = start(desk_cfg(&a, &ring, &ring.chain));
    assert!(rec.connected());
    assert!(held(&rec, &ga.url(), 2));
    ga.set_open(false);
    ring.next(|d| d.relay_url = b.url());
    desk.set_chain(ring.chain.clone());
    desk.set_farewells(vec![Farewell::new(&ring.chain, 2).unwrap()]);
    waiting(&rec, &ga.url(), 1);
    assert!(held(&rec, &b.url(), 3));
    ring.next(|d| d.relay_url = c.url());
    desk.set_chain(ring.chain.clone());
    desk.set_farewells(vec![
        Farewell::new(&ring.chain, 2).unwrap(),
        Farewell::new(&ring.chain, 3).unwrap(),
    ]);
    assert!(rec.wait(WAIT, |e| is_done(e, &b.url())).is_some());
    assert_eq!(b.head_version(&ring.ring_id()), Some(4));
    ga.set_open(true);
    assert!(rec
        .wait(WAIT, |e| matches!(
            e,
            Ev::Farewell(FarewellState::Done { farewell })
                if farewell.source == ga.url() && farewell.target == 4
        ))
        .is_some());
    assert_eq!(a.head_version(&ring.ring_id()), Some(4), "in one step");
    let (dmn, mrec) = start(cfg(&a, &ring.up_to(2), ring.daemon.clone()));
    wait_until("the Daemon on C", || {
        online(&c, &ring, &ring.daemon.sign_key())
    });
    assert!(mrec.wait(WAIT, |e| matches!(e, Ev::Chain(4))).is_some());
    let on_b = b
        .presence(&ring.ring_id(), &ring.daemon.sign_key())
        .unwrap();
    assert!(
        !on_b.online && on_b.last_seen.is_none(),
        "never on B: {on_b:?}"
    );
    dmn.stop(ByeReason::quit());
    desk.stop(ByeReason::quit());
}

/// A1: the old Relay commits A → B → C in one step and broadcasts each version; a device
/// still connected there fetches the rest before it follows, so it never dials B.
#[test]
fn a_device_on_the_old_relay_never_dials_an_intermediate_relay() {
    let (a, c) = (relay(), relay());
    let gb = GateProxy::start(c.addr());
    gb.set_open(false);
    let mut ring = TestRing::new(&a.url());
    let (dmn, mrec) = start(cfg(&a, &ring.chain, ring.daemon.clone()));
    assert!(mrec.connected());
    ring.next(|d| d.relay_url = gb.url());
    ring.next(|d| d.relay_url = c.url());
    let mut cc = desk_cfg(&c, &ring, &ring.chain);
    cc.farewells = vec![Farewell {
        source: a.url(),
        from: 2,
        target: 4,
    }];
    let (desk, rec) = start(cc);
    assert!(rec.wait(WAIT, |e| is_done(e, &a.url())).is_some());
    wait_until("the Daemon on C", || {
        online(&c, &ring, &ring.daemon.sign_key())
    });
    assert!(mrec.wait(WAIT, |e| matches!(e, Ev::Chain(4))).is_some());
    assert_eq!(gb.connections(), 0, "never dialed the intermediate Relay");
    dmn.stop(ByeReason::quit());
    desk.stop(ByeReason::quit());
}

#[test]
fn a_farewell_to_the_relay_the_head_names_again_is_dropped() {
    let (a, b) = (relay(), relay());
    let ga = gated(&a);
    ga.set_open(false);
    let mut ring = TestRing::new(&ga.url());
    let key = ring.desktop.sign_key();
    ring.next(|d| d.relay_url = b.url());
    let f = Farewell::new(&ring.chain, 2).unwrap();
    let mut cc = desk_cfg(&b, &ring, &ring.chain);
    cc.farewell_backoff_unit = Duration::from_secs(30);
    cc.farewells = vec![f.clone()];
    let (desk, rec) = start(cc);
    assert!(rec.connected());
    waiting(&rec, &ga.url(), 1);
    // Back to A.
    ring.next(|d| d.relay_url = ga.url());
    ga.set_open(true);
    desk.set_chain(ring.chain.clone());
    assert!(desk.farewells().is_empty());
    desk.set_farewells(vec![f.clone()]);
    assert!(desk.farewells().is_empty(), "not owed");
    wait_until("back on A", || online(&a, &ring, &key));
    assert_eq!(a.head_version(&ring.ring_id()), Some(4));
    std::thread::sleep(QUIET);
    assert!(!rec.all().iter().any(|e| is_done(e, &ga.url())));
    desk.stop(ByeReason::quit());

    // One kept on disk that the chain no longer owes is dropped at the start.
    let mut cc = desk_cfg(&a, &ring, &ring.chain);
    cc.farewells = vec![f];
    let (desk, _) = start(cc);
    assert!(desk.farewells().is_empty());
    desk.stop(ByeReason::quit());
}

/// A3: A → B → A while the farewell to A is in flight (held at its `auth`): it is cut, and
/// when its `auth` still reaches A the main link there survives it (a `replaced` blip at
/// most).
#[test]
fn an_obsolete_farewell_in_flight_is_cut() {
    let (a, b) = (slow_relay(), relay());
    let ga = gated(&a);
    let mut ring = TestRing::new(&ga.url());
    let key = ring.desktop.sign_key();
    ring.next(|d| d.relay_url = b.url());
    ga.hold_next();
    let mut cc = desk_cfg(&b, &ring, &ring.chain);
    cc.farewells = vec![Farewell::new(&ring.chain, 2).unwrap()];
    let (desk, rec) = start(cc);
    assert!(rec
        .wait(WAIT, |e| matches!(
            e,
            Ev::Farewell(FarewellState::Sending { .. })
        ))
        .is_some());
    wait_until("the farewell held", || ga.connections() == 1);
    ring.next(|d| d.relay_url = ga.url());
    desk.set_chain(ring.chain.clone());
    assert!(desk.farewells().is_empty());
    wait_until("the link on A", || online(&a, &ring, &key));
    ga.release();
    std::thread::sleep(Duration::from_millis(500));
    wait_until("connected on A", || {
        desk.state() == LinkState::Connected { limited: false } && online(&a, &ring, &key)
    });
    std::thread::sleep(QUIET);
    assert_eq!(desk.state(), LinkState::Connected { limited: false });
    assert!(online(&a, &ring, &key));
    assert!(!rec
        .all()
        .iter()
        .any(|e| matches!(e, Ev::Farewell(FarewellState::Done { .. }))));
    desk.stop(ByeReason::quit());
}

#[test]
fn farewells_are_reported_per_relay() {
    let (a1, a2, b) = (relay(), relay(), relay());
    let (g1, g2) = (gated(&a1), gated(&a2));
    g1.set_open(false);
    g2.set_open(false);
    let mut ring = TestRing::new(&g1.url());
    ring.next(|d| d.relay_url = g2.url());
    ring.next(|d| d.relay_url = b.url());
    let mut cc = desk_cfg(&b, &ring, &ring.chain);
    cc.farewells = vec![
        Farewell::new(&ring.chain, 3).unwrap(),
        Farewell::new(&ring.chain, 2).unwrap(),
    ];
    let (desk, rec) = start(cc);
    waiting(&rec, &g1.url(), 1);
    waiting(&rec, &g2.url(), 1);
    let sources = |c: &Connector| -> Vec<String> {
        c.farewells()
            .into_iter()
            .map(|s| match s {
                FarewellState::Sending { farewell }
                | FarewellState::Waiting { farewell, .. }
                | FarewellState::Done { farewell } => farewell.source,
            })
            .collect()
    };
    let mut both = vec![g1.url(), g2.url()];
    both.sort();
    assert_eq!(sources(&desk), both);
    g2.set_open(true);
    assert!(rec.wait(WAIT, |e| is_done(e, &g2.url())).is_some());
    assert_eq!(sources(&desk), [g1.url()]);
    assert!(!rec.all().iter().any(|e| is_done(e, &g1.url())));
    desk.stop(ByeReason::quit());
}

#[test]
fn stop_during_a_farewell_says_goodbye_on_the_old_relay() {
    let (a, b) = (slow_relay(), relay());
    // The old Relay answers slowly, so the stop arrives while the farewell is in flight.
    let proxy = TrickleProxy::start(a.addr(), 8, Duration::from_millis(15));
    let old = format!("ws://127.0.0.1:{}", proxy.addr().port());
    a.set_origin(&old);
    let mut ring = TestRing::new(&old);
    let key = ring.desktop.sign_key();
    ring.next(|d| d.relay_url = b.url());
    let mut cc = desk_cfg(&b, &ring, &ring.chain);
    cc.farewells = vec![Farewell::new(&ring.chain, 2).unwrap()];
    let (desk, rec) = start(cc);
    assert!(rec
        .wait(WAIT, |e| matches!(
            e,
            Ev::Farewell(FarewellState::Sending { .. })
        ))
        .is_some());
    std::thread::sleep(Duration::from_millis(100));
    desk.stop(ByeReason::quit());
    assert_eq!(a.head_version(&ring.ring_id()), Some(3));
    let p = a.presence(&ring.ring_id(), &key).unwrap();
    assert!(!p.online);
    assert_eq!(
        p.last_reason.as_deref(),
        Some("quit"),
        "said goodbye: {p:?}"
    );
}

/// A farewell to a Relay that refuses this Ring for quota: a failure of the farewell, never
/// a quota report (the quota is the old Relay's), but it says the Relay holds the Ring.
#[test]
fn a_farewell_to_a_relay_over_quota_reports_no_quota() {
    let (a, b) = (quota_relay(8), relay());
    let mut ring = TestRing::new(&a.url());
    // The old Relay is ahead of this Desktop's chain (another Desktop published there), so
    // the farewell's connect asks it for the rest, which the quota refuses.
    let (d2, _) = connect(&a.target(), &ring.chain, ring.desktop2.clone());
    let v3 = ring.next(|d| d.relay_url = b.url());
    let v4 = ring.next(|_| {});
    d2.publish_roster(&v3).unwrap();
    d2.publish_roster(&v4).unwrap();
    use_up_quota(&a, &ring);
    let mine = ring.up_to(3);
    let mut cc = desk_cfg(&b, &ring, &mine);
    cc.farewells = vec![Farewell::new(&mine, 2).unwrap()];
    let (desk, rec) = start(cc);
    match waiting(&rec, &a.url(), 1) {
        FarewellState::Waiting { error, .. } => assert!(error.contains("quota"), "{error}"),
        other => panic!("{other:?}"),
    }
    assert!(held(&rec, &a.url(), 2));
    assert!(rec.connected());
    assert!(quota_errors(&rec).is_empty(), "{:?}", rec.all());
    desk.stop(ByeReason::quit());
}

/// Frames before `auth` cost no quota: a farewell staged at `auth` reaches an old Relay over
/// its quota, and nothing about the quota is reported.
#[test]
fn a_farewell_reaches_an_old_relay_over_its_quota() {
    let (a, b) = (quota_relay(8), relay());
    let mut ring = TestRing::new(&a.url());
    use_up_quota(&a, &ring);
    ring.next(|d| d.relay_url = b.url());
    let mut cc = desk_cfg(&b, &ring, &ring.chain);
    cc.farewells = vec![Farewell::new(&ring.chain, 2).unwrap()];
    let (desk, rec) = start(cc);
    assert!(rec.wait(WAIT, |e| is_done(e, &a.url())).is_some());
    assert_eq!(a.head_version(&ring.ring_id()), Some(3));
    assert!(quota_errors(&rec).is_empty(), "{:?}", rec.all());
    desk.stop(ByeReason::quit());
}

/// A close for quota during a farewell is a failure that backs off.
#[test]
fn a_quota_close_during_a_farewell_backs_off() {
    let (a, b) = (relay(), relay());
    let mut ring = TestRing::new(&a.url());
    let key = ring.desktop.sign_key();
    let (d2, _) = connect(&a.target(), &ring.chain, ring.desktop2.clone());
    let v3 = ring.next(|d| d.relay_url = b.url());
    let v4 = ring.next(|_| {});
    d2.publish_roster(&v3).unwrap();
    d2.publish_roster(&v4).unwrap();
    // The farewell's connect waits for the rest of the chain, which never comes.
    a.ignore_roster_gets(true);
    let mine = ring.up_to(3);
    let mut cc = desk_cfg(&b, &ring, &mine);
    cc.farewell_backoff_unit = Duration::from_secs(30);
    cc.farewell_backoff_max = Duration::from_secs(60);
    cc.farewells = vec![Farewell::new(&mine, 2).unwrap()];
    let (desk, rec) = start(cc);
    wait_until("the farewell on A", || online(&a, &ring, &key));
    assert!(a.kick(&ring.ring_id(), &key, close::QUOTA));
    match waiting(&rec, &a.url(), 1) {
        FarewellState::Waiting {
            failures, retry_in, ..
        } => {
            assert_eq!(failures, 1);
            assert_eq!(retry_in, Duration::from_secs(30));
        }
        other => panic!("{other:?}"),
    }
    assert!(held(&rec, &a.url(), 2));
    std::thread::sleep(QUIET);
    assert!(!online(&a, &ring, &key), "no redial during the wait");
    assert!(quota_errors(&rec).is_empty(), "{:?}", rec.all());
    desk.stop(ByeReason::quit());
}

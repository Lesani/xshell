//! The Connector against the in-process test Relay: reconnect with backoff, goodbye, Relay
//! moves (followed, and owed to the old Relay), removal, and fenced attempts.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use xshell_protocol::ring::entitlement::{sign_entitlement, GatewayKeys, Tier};
use xshell_protocol::ring::relay::contract::{
    config, connect, fake_entitlement, keys, now, TestRing, QUIET, WAIT,
};
use xshell_protocol::ring::relay::test_relay::{Fault, TestRelay, TestRelayOptions, TrickleProxy};
use xshell_protocol::ring::relay::wire::{ByeReason, ErrorCode, MemberPresence};
use xshell_protocol::ring::relay::{
    Connector, ConnectorConfig, ConnectorEvents, LinkState, MoveJob, MoveState, RingTimeouts,
};
use xshell_protocol::ring::{DeviceKeys, RingError, RingId, RosterChain, SignKey, Signer};

#[derive(Clone, Debug)]
enum Ev {
    State(LinkState),
    Chain(u64),
    Moved(MoveState),
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
    fn moved(&self, m: &MoveState) {
        self.ev.lock().unwrap().push(Ev::Moved(m.clone()));
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
    c.set_chain(ring.chain.clone(), None);
    wait_until("published", || r.head_version(&ring.ring_id()) == Some(4));
    c.stop(ByeReason::quit());
}

#[test]
fn relay_move_publishes_on_the_old_relay_first() {
    let (a, b) = (relay(), relay());
    let mut ring = TestRing::new(&a.url());
    let (desk, drec) = start(cfg(&a, &ring.chain, ring.desktop.clone()));
    let (dmn, mrec) = start(cfg(&a, &ring.chain, ring.daemon.clone()));
    assert!(drec.connected() && mrec.connected());
    ring.next(|d| d.relay_url = b.url());
    let job = MoveJob::new(&ring.chain, 2).unwrap();
    assert_eq!(job.source, a.url());
    drec.clear();
    desk.set_chain(ring.chain.clone(), Some(job.clone()));
    assert!(drec
        .wait(
            WAIT,
            |e| matches!(e, Ev::Moved(MoveState::Done { job: j }) if *j == job)
        )
        .is_some());
    assert_eq!(
        a.head_version(&ring.ring_id()),
        Some(3),
        "the old Relay knows"
    );
    assert!(drec.connected());
    wait_until("both on the new Relay", || {
        online(&b, &ring, &ring.desktop.sign_key()) && online(&b, &ring, &ring.daemon.sign_key())
    });
    assert_eq!(desk.move_state(), Some(MoveState::Done { job }));
    desk.stop(ByeReason::quit());
    dmn.stop(ByeReason::quit());
}

#[test]
fn a_move_stays_on_the_old_relay_until_it_is_acknowledged() {
    let (a, b) = (relay(), relay());
    let mut ring = TestRing::new(&a.url());
    let key = ring.desktop.sign_key();
    a.refuse_roster_puts(true);
    ring.next(|d| d.relay_url = b.url());
    let job = MoveJob::new(&ring.chain, 2).unwrap();
    let mut cc = cfg(&a, &ring.chain, ring.desktop.clone());
    cc.move_attempts = 2;
    // Owed from the start (as after a restart with the job on disk).
    cc.pending_move = Some(job.clone());
    let (desk, drec) = start(cc.clone());
    // Batches fail and are reported, again and again; the Connector never moves.
    for _ in 0..2 {
        drec.clear();
        assert!(drec
            .wait(WAIT, |e| matches!(e, Ev::Moved(MoveState::Failed { .. })))
            .is_some());
    }
    assert!(online(&a, &ring, &key), "still reachable on the old Relay");
    assert!(
        !online(&b, &ring, &key),
        "not on the new Relay before the ack"
    );
    assert_eq!(b.head_version(&ring.ring_id()), None);
    assert_eq!(a.head_version(&ring.ring_id()), Some(2));
    assert!(matches!(desk.move_state(), Some(MoveState::Failed { .. })));

    // The old Relay takes it: only now does the Connector move.
    a.refuse_roster_puts(false);
    assert!(drec
        .wait(WAIT, |e| matches!(e, Ev::Moved(MoveState::Done { .. })))
        .is_some());
    assert_eq!(a.head_version(&ring.ring_id()), Some(3));
    wait_until("on the new Relay", || online(&b, &ring, &key));
    desk.stop(ByeReason::quit());

    // A restart with the job already acknowledged moves at once.
    let (desk, drec) = start(cc);
    assert!(drec
        .wait(WAIT, |e| matches!(e, Ev::Moved(MoveState::Done { .. })))
        .is_some());
    assert!(drec.connected());
    desk.stop(ByeReason::quit());
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

#[test]
fn stop_while_the_move_is_acknowledged_says_goodbye_on_the_old_relay() {
    let (a, b) = (slow_relay(), relay());
    // The old Relay answers slowly, so the stop arrives while the publication is in flight.
    let proxy = TrickleProxy::start(a.addr(), 8, Duration::from_millis(15));
    let old = format!("ws://127.0.0.1:{}", proxy.addr().port());
    a.set_origin(&old);
    let mut ring = TestRing::new(&old);
    let key = ring.desktop.sign_key();
    ring.next(|d| d.relay_url = b.url());
    let mut cc = ConnectorConfig::new(xshell_protocol::ring::relay::RingClientConfig::new(
        ring.chain.clone(),
        ring.desktop.clone(),
    ));
    cc.client.timeouts = timeouts();
    cc.backoff_unit = Duration::from_millis(50);
    cc.pending_move = MoveJob::new(&ring.chain, 2);
    let (desk, rec) = start(cc);
    assert!(rec
        .wait(Duration::from_secs(10), |e| matches!(
            e,
            Ev::State(LinkState::Connected { .. })
        ))
        .is_some());
    desk.stop(ByeReason::quit());
    assert!(rec
        .all()
        .iter()
        .any(|e| matches!(e, Ev::Moved(MoveState::Done { .. }))));
    assert_eq!(a.head_version(&ring.ring_id()), Some(3));
    let p = a.presence(&ring.ring_id(), &key).unwrap();
    assert_eq!(
        p.last_reason.as_deref(),
        Some("quit"),
        "said goodbye: {p:?}"
    );
    assert!(!online(&b, &ring, &key));
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
        c.set_chain(ring.chain.clone(), None);
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
    c.set_chain(ring.chain.clone(), None);
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
    c.set_chain(ring.chain.clone(), None);
    let v4 = ring.next(|_| {});
    c.set_chain(ring.chain.clone(), None);
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
    other.set_chain(ring.chain.clone(), None);
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

/// A move owed to a Relay that refuses this Ring for quota: those refusals are a move
/// failure, never a quota report (the quota is the old Relay's).
#[test]
fn a_move_owed_to_a_relay_over_quota_reports_no_quota() {
    let b = relay();
    // The old Relay is ahead of the move's start and over quota: its connect is refused.
    let (a, mut ring) = ahead_and_over_quota();
    ring.next(|d| d.relay_url = b.url());
    let job = MoveJob::new(&ring.chain, 2).unwrap();
    let mut cc = cfg(&a, &ring.chain, ring.desktop.clone());
    cc.move_attempts = 2;
    cc.pending_move = Some(job.clone());
    let (desk, drec) = start(cc);
    assert!(drec
        .wait(WAIT, |e| matches!(e, Ev::Moved(MoveState::Failed { .. })))
        .is_some());
    match drec.wait(WAIT, is_waiting) {
        Some(Ev::State(LinkState::Waiting { error, .. })) => {
            assert!(error.contains("quota"), "{error}")
        }
        other => panic!("{other:?}"),
    }
    assert!(quota_errors(&drec).is_empty(), "{:?}", drec.all());
    assert!(!online(&b, &ring, &ring.desktop.sign_key()));
    desk.stop(ByeReason::quit());
}

/// The same when the old Relay lets the Desktop in but refuses the versions it publishes
/// there for quota.
#[test]
fn a_move_whose_publication_is_refused_for_quota_reports_no_quota() {
    let (a, b) = (quota_relay(8), relay());
    let mut ring = TestRing::new(&a.url());
    use_up_quota(&a, &ring);
    ring.next(|d| d.relay_url = b.url());
    let job = MoveJob::new(&ring.chain, 2).unwrap();
    let mut cc = cfg(&a, &ring.chain, ring.desktop.clone());
    cc.move_attempts = 2;
    cc.pending_move = Some(job);
    let (desk, drec) = start(cc);
    assert!(drec
        .wait(WAIT, |e| matches!(
            e,
            Ev::Moved(MoveState::Failed { error, .. }) if error.contains("quota")
        ))
        .is_some());
    assert!(quota_errors(&drec).is_empty(), "{:?}", drec.all());
    assert!(!drec.all().iter().any(|e| matches!(e, Ev::Accepted)));
    desk.stop(ByeReason::quit());
}

/// A close for quota while a move is owed backs off, and a kick still ends that wait.
#[test]
fn a_quota_close_during_a_move_backs_off_until_kicked() {
    let (a, b) = (relay(), relay());
    let mut ring = TestRing::new(&a.url());
    let key = ring.desktop.sign_key();
    a.refuse_roster_puts(true);
    ring.next(|d| d.relay_url = b.url());
    let job = MoveJob::new(&ring.chain, 2).unwrap();
    let mut cc = cfg(&a, &ring.chain, ring.desktop.clone());
    // Waits long enough that only a kick ends them within the test.
    cc.backoff_unit = Duration::from_secs(30);
    cc.pending_move = Some(job);
    let (desk, drec) = start(cc);
    assert!(drec
        .wait(WAIT, |e| matches!(
            e,
            Ev::Moved(MoveState::Moving { error: Some(_), .. })
        ))
        .is_some());
    wait_until("on the old Relay", || online(&a, &ring, &key));
    drec.clear();
    assert!(a.kick(&ring.ring_id(), &key, close::QUOTA));
    match drec.wait(WAIT, is_waiting) {
        Some(Ev::State(LinkState::Waiting { retry_in, .. })) => {
            assert_eq!(retry_in, Duration::from_secs(30))
        }
        other => panic!("{other:?}: {:?}", drec.all()),
    }
    std::thread::sleep(QUIET);
    assert!(!online(&a, &ring, &key), "no redial during the wait");
    assert!(quota_errors(&drec).is_empty(), "{:?}", drec.all());
    desk.kick();
    assert!(drec.connected(), "the kick ends the wait");
    wait_until("back on the old Relay", || online(&a, &ring, &key));
    desk.stop(ByeReason::quit());
}

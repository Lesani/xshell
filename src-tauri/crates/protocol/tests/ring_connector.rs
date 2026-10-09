//! The Connector against the in-process test Relay: reconnect with backoff, goodbye, Relay
//! moves (followed, and owed to the old Relay), removal, and fenced attempts.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use xshell_protocol::ring::relay::contract::{config, connect, TestRing, WAIT};
use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions, TrickleProxy};
use xshell_protocol::ring::relay::wire::{ByeReason, MemberPresence};
use xshell_protocol::ring::relay::{
    Connector, ConnectorConfig, ConnectorEvents, LinkState, MoveJob, MoveState, RingTimeouts,
};
use xshell_protocol::ring::{RingError, RosterChain, SignKey, Signer};

#[derive(Clone, Debug)]
enum Ev {
    State(LinkState),
    Chain(u64),
    Moved(MoveState),
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

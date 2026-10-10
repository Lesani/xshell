#![cfg(unix)]
//! The Daemon as a Ring member: `ring.identity` and `ring.join`, its Relay connection
//! (presence, reconnect, goodbyes) and what it keeps on disk, against the in-process test
//! Relay.

mod common;
mod desktop;

use common::*;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_protocol::msg::{ClientMsg, JoinExpect, MEMBERSHIP_CHANGED};
use xshell_protocol::ring::relay::contract::{self, Recorder};
use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions};
use xshell_protocol::ring::relay::wire::{MemberPresence, Presence};
use xshell_protocol::ring::relay::{RingClient, RingTimeouts};
use xshell_protocol::ring::{
    DeviceKeys, Member, NoiseKey, Role as RingRole, RosterChain, SignKey, SignedRoster,
};
use xshelld::server::{Config, ExitReason, Role, ServerHandle};

fn relay() -> TestRelay {
    TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        ..TestRelayOptions::default()
    })
}

fn fast(c: &mut Config) {
    c.ring_backoff_unit = Duration::from_millis(10);
    c.ring_timeouts = RingTimeouts {
        connect: Duration::from_secs(5),
        ping_interval: Duration::from_millis(300),
        dead_after: Duration::from_secs(3),
        request: Duration::from_secs(2),
        bye: Duration::from_millis(800),
    };
}

fn server(h: &TestHome) -> ServerHandle {
    start(h, fast)
}

fn desktop(srv: &ServerHandle) -> Client {
    Client::in_process(srv, Role::Desktop)
}

struct Identity {
    sign: SignKey,
    noise: NoiseKey,
    raw: Value,
}

fn identity(c: &mut Client) -> Identity {
    let v = c.request(&ClientMsg::RingIdentity).expect("ring.identity");
    Identity {
        sign: SignKey::parse(v["signKey"].as_str().unwrap()).unwrap(),
        noise: NoiseKey::parse(v["noiseKey"].as_str().unwrap()).unwrap(),
        raw: v,
    }
}

fn now() -> u64 {
    contract::now()
}

/// A Ring created by a Desktop, with the Daemon `id` added in version 2.
struct Ring {
    desk: Arc<DeviceKeys>,
    chain: RosterChain,
}

impl Ring {
    fn new(url: &str, daemon: &Identity, role: RingRole) -> Ring {
        let desk = Arc::new(DeviceKeys::generate().unwrap());
        let v1 = SignedRoster::genesis(&*desk, desk.noise_key(), "desk", url, now()).unwrap();
        let v2 = v1
            .next(&*desk, now(), |d| {
                d.add(Member::new("host", role, daemon.sign, daemon.noise, now()))
            })
            .unwrap();
        Ring {
            desk,
            chain: RosterChain::from_chain(vec![v1, v2]).unwrap(),
        }
    }

    fn next(&mut self, edit: impl FnOnce(&mut xshell_protocol::ring::RosterDraft)) -> SignedRoster {
        let n = self.chain.head().next(&*self.desk, now(), edit).unwrap();
        self.chain.accept(std::slice::from_ref(&n)).unwrap();
        n
    }

    fn tokens(&self) -> Vec<String> {
        tokens(&self.chain)
    }

    fn desk_client(&self, r: &TestRelay) -> (RingClient, Arc<Recorder>) {
        contract::connect(&r.target(), &self.chain, self.desk.clone())
    }
}

fn tokens(c: &RosterChain) -> Vec<String> {
    c.versions().iter().map(|r| r.token().to_string()).collect()
}

fn join(c: &mut Client, tokens: Vec<String>) -> Result<Value, String> {
    c.request(&ClientMsg::RingJoin {
        rosters: tokens,
        expect: None,
    })
}

/// A conditional `ring.join`: only while the Host is in `ring_id` (`None`: in no Ring), at
/// `version` when given.
fn cjoin(
    c: &mut Client,
    tokens: Vec<String>,
    ring_id: Option<&str>,
    version: Option<u64>,
) -> Result<Value, String> {
    c.request(&ClientMsg::RingJoin {
        rosters: tokens,
        expect: Some(JoinExpect {
            ring_id: ring_id.map(String::from),
            version,
        }),
    })
}

fn presence(r: &TestRelay, ring: &Ring, key: &SignKey) -> Option<Presence> {
    r.presence(ring.chain.ring_id(), key)
}

fn wait_presence(
    r: &TestRelay,
    ring: &Ring,
    key: &SignKey,
    pred: impl Fn(&MemberPresence) -> bool,
) {
    let deadline = Instant::now() + T;
    loop {
        if presence(r, ring, key).is_some_and(|p| pred(&MemberPresence::from(&p))) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "presence: {:?}",
            presence(r, ring, key)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn online(m: &MemberPresence) -> bool {
    matches!(m, MemberPresence::Online { .. })
}

fn closed_with(reason: &'static str) -> impl Fn(&MemberPresence) -> bool {
    move |m| matches!(m, MemberPresence::Closed { reason: r, .. } if r == reason)
}

fn stored_versions(h: &TestHome) -> usize {
    let v: Value =
        serde_json::from_slice(&fs::read(h.paths().ring_dir.join("roster.json")).unwrap()).unwrap();
    v["rosters"].as_array().unwrap().len()
}

fn mode(p: &std::path::Path) -> u32 {
    fs::metadata(p).unwrap().mode() & 0o777
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + T;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn identity_is_stable_and_private() {
    let h = TestHome::new();
    let dir = h.paths().ring_dir;
    {
        let srv = server(&h);
        let mut c = desktop(&srv);
        let a = identity(&mut c);
        let b = identity(&mut c);
        assert_eq!(a.sign, b.sign);
        assert_eq!(a.noise, b.noise);
        assert!(a.raw["ring"].is_null());
        let name = a.raw["name"].as_str().unwrap();
        assert!(!name.is_empty() && name.len() <= 64);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("keys.json")), 0o600);
        // A broader mode found later is tightened, and the keys stay the same.
        fs::set_permissions(dir.join("keys.json"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        srv.shutdown();
        let srv = server(&h);
        let mut c = desktop(&srv);
        assert_eq!(identity(&mut c).sign, a.sign);
        assert_eq!(mode(&dir.join("keys.json")), 0o600);
        assert_eq!(mode(&dir), 0o700);
        srv.shutdown();
    }
    // A keys file that is a symlink is refused, never followed or replaced.
    let real = h.root().join("elsewhere.json");
    fs::rename(dir.join("keys.json"), &real).unwrap();
    std::os::unix::fs::symlink(&real, dir.join("keys.json")).unwrap();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let e = c.request(&ClientMsg::RingIdentity).unwrap_err();
    assert!(e.contains("symlink"), "{e}");
    assert!(fs::symlink_metadata(dir.join("keys.json"))
        .unwrap()
        .file_type()
        .is_symlink());
    // A ring dir that is a symlink is refused too.
    srv.shutdown();
    let h2 = TestHome::new();
    let other = h2.root().join("other");
    fs::create_dir(&other).unwrap();
    fs::create_dir_all(h2.paths().ring_dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&other, h2.paths().ring_dir).unwrap();
    let srv = server(&h2);
    let mut c = desktop(&srv);
    assert!(c.request(&ClientMsg::RingIdentity).is_err());
    assert_eq!(
        fs::read_dir(&other).unwrap().count(),
        0,
        "nothing written there"
    );
}

#[test]
fn mobile_cannot_use_ring_messages() {
    let h = TestHome::new();
    let srv = server(&h);
    let mut m = Client::in_process(&srv, Role::Mobile);
    for msg in [
        ClientMsg::RingIdentity,
        ClientMsg::RingJoin {
            rosters: vec![],
            expect: None,
        },
    ] {
        let e = m.request(&msg).unwrap_err();
        assert!(e.starts_with("forbidden for mobile"), "{e}");
    }
    assert!(!h.paths().ring_dir.exists(), "nothing created");
}

#[test]
fn join_connects_daemon_to_relay() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    assert_eq!(join(&mut c, ring.tokens()).unwrap()["version"], 2);
    let (desk, _rec) = ring.desk_client(&r);
    wait_until("the Daemon online for the Desktop", || {
        desk.members()
            .iter()
            .any(|m| m.member.sign_key == id.sign && online(&m.presence))
    });
    let i = identity(&mut c);
    assert_eq!(i.raw["ring"]["ringId"], ring.chain.ring_id().as_str());
    assert_eq!(i.raw["ring"]["version"], 2);
    assert_eq!(i.raw["ring"]["relayUrl"], r.url());
    assert_eq!(i.raw["ring"]["state"], "connected");
    assert_eq!(mode(&h.paths().ring_dir.join("roster.json")), 0o600);
}

#[test]
fn join_refuses_roster_without_this_daemon() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let other = Identity {
        sign: DeviceKeys::generate().unwrap().sign_key(),
        noise: DeviceKeys::generate().unwrap().noise_key(),
        raw: Value::Null,
    };
    for ring in [
        Ring::new(&r.url(), &other, RingRole::Daemon),
        Ring::new(&r.url(), &id, RingRole::Mobile),
    ] {
        assert_eq!(
            join(&mut c, ring.tokens()).unwrap_err(),
            "not a member of this Roster"
        );
    }
    assert!(join(&mut c, vec!["xro1.junk.junk".into()])
        .unwrap_err()
        .starts_with("roster refused"));
    assert!(identity(&mut c).raw["ring"].is_null());
}

#[test]
fn join_refuses_stale_or_forked_chain() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let mut ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    let v2 = ring.chain.clone();
    ring.next(|_| {});
    join(&mut c, ring.tokens()).unwrap();
    // Identical: a no-op.
    assert_eq!(join(&mut c, ring.tokens()).unwrap()["version"], 3);
    // Stale.
    assert_eq!(
        join(&mut c, tokens(&v2)).unwrap_err(),
        "roster refused: stale"
    );
    // A fork of version 3.
    let fork = v2
        .head()
        .next(&*ring.desk, now() + 1, |d| {
            d.relay_url = "wss://other.example".into()
        })
        .unwrap();
    let mut forked = tokens(&v2);
    forked.push(fork.token().to_string());
    assert_eq!(
        join(&mut c, forked).unwrap_err(),
        "roster refused: prev_mismatch"
    );
    assert_eq!(identity(&mut c).raw["ring"]["version"], 3);
}

/// Only a Desktop signs a new version: a chain whose head a Mobile signed is refused on the
/// local path too, and the trusted head stays.
#[test]
fn join_refuses_a_mobile_signed_chain() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let mut ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    let phone = DeviceKeys::generate().unwrap();
    ring.next(|d| {
        d.add(Member::new(
            "phone",
            RingRole::Mobile,
            phone.sign_key(),
            phone.noise_key(),
            now(),
        ))
    });
    join(&mut c, ring.tokens()).unwrap();
    let intruder = DeviceKeys::generate().unwrap();
    let forged = contract::raw_next(ring.chain.head(), &phone, |x| {
        x.members
            .push(contract::member(&intruder, "intruder", RingRole::Desktop));
    });
    let mut toks = ring.tokens();
    toks.push(forged.token().to_string());
    assert_eq!(
        join(&mut c, toks).unwrap_err(),
        "roster refused: signer_not_desktop"
    );
    assert_eq!(identity(&mut c).raw["ring"]["version"], 3);
}

#[test]
fn join_of_other_ring_replaces_membership() {
    let (a, b) = (relay(), relay());
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let one = Ring::new(&a.url(), &id, RingRole::Daemon);
    join(&mut c, one.tokens()).unwrap();
    wait_presence(&a, &one, &id.sign, online);
    // This Desktop lost its app data and made a new Ring.
    let two = Ring::new(&b.url(), &id, RingRole::Daemon);
    join(&mut c, two.tokens()).unwrap();
    wait_presence(&b, &two, &id.sign, online);
    wait_presence(&a, &one, &id.sign, closed_with("quit"));
    assert_eq!(
        identity(&mut c).raw["ring"]["ringId"],
        two.chain.ring_id().as_str()
    );
}

#[test]
fn membership_survives_restart() {
    let r = relay();
    let h = TestHome::new();
    let mut ring;
    let id;
    {
        let srv = server(&h);
        let mut c = desktop(&srv);
        id = identity(&mut c);
        ring = Ring::new(&r.url(), &id, RingRole::Daemon);
        join(&mut c, ring.tokens()).unwrap();
        wait_presence(&r, &ring, &id.sign, online);
        // Versions from the Relay while connected are kept, all of them.
        let (desk, _) = ring.desk_client(&r);
        for _ in 0..2 {
            let v = ring.next(|_| {});
            desk.publish_roster(&v).unwrap();
        }
        wait_until("four versions stored", || stored_versions(&h) == 4);
        drop(desk);
        srv.shutdown();
    }
    // Several versions published while the Daemon is away: it catches up while connecting
    // and stores the whole chain.
    let (desk, _) = ring.desk_client(&r);
    for _ in 0..3 {
        let v = ring.next(|_| {});
        desk.publish_roster(&v).unwrap();
    }
    let srv = server(&h);
    wait_presence(&r, &ring, &id.sign, online);
    wait_until("seven versions stored", || stored_versions(&h) == 7);
    let mut c = desktop(&srv);
    assert_eq!(identity(&mut c).raw["ring"]["version"], 7);
    srv.shutdown();
    // And from disk again.
    let srv = server(&h);
    wait_presence(&r, &ring, &id.sign, online);
    let mut c = desktop(&srv);
    assert_eq!(identity(&mut c).raw["ring"]["version"], 7);
}

#[test]
fn shutdown_says_bye_quit() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    join(&mut c, ring.tokens()).unwrap();
    wait_presence(&r, &ring, &id.sign, online);
    assert_eq!(srv.shutdown(), ExitReason::Shutdown);
    // The goodbye is over when the Daemon has exited.
    assert!(closed_with("quit")(&MemberPresence::from(
        &presence(&r, &ring, &id.sign).unwrap()
    )));
}

#[test]
fn idle_exit_says_bye_idle() {
    let r = relay();
    let h = TestHome::new();
    let srv = start(&h, |c| {
        fast(c);
        c.idle_timeout = Duration::from_millis(300);
    });
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    join(&mut c, ring.tokens()).unwrap();
    wait_presence(&r, &ring, &id.sign, online);
    // The Relay socket does not keep the Daemon from going idle.
    c.shutdown();
    drop(c);
    assert_eq!(srv.wait_timeout(T), Some(ExitReason::Idle));
    wait_presence(&r, &ring, &id.sign, closed_with("idle"));
}

#[test]
fn daemon_upgrade_says_bye_upgrade() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    join(&mut c, ring.tokens()).unwrap();
    wait_presence(&r, &ring, &id.sign, online);
    c.request(&ClientMsg::DaemonUpgrade).unwrap();
    assert_eq!(srv.wait_timeout(T), Some(ExitReason::Upgrade));
    assert!(closed_with("upgrade")(&MemberPresence::from(
        &presence(&r, &ring, &id.sign).unwrap()
    )));
}

#[test]
fn reconnects_after_relay_drops_it() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    join(&mut c, ring.tokens()).unwrap();
    wait_presence(&r, &ring, &id.sign, online);
    // The Desktop's view of the Relay's presence pushes: each drop and reconnect shows up
    // as an event, however short the gap (sampling the record can miss it).
    let (_desk, rec) = ring.desk_client(&r);
    let count = |want: fn(&MemberPresence) -> bool| {
        rec.events()
            .iter()
            .filter(|e| {
                matches!(e, contract::Event::Presence { key, presence }
                    if *key == id.sign && want(presence))
            })
            .count()
    };
    for n in 1..=3 {
        assert!(r.kick(ring.chain.ring_id(), &id.sign, 1011));
        wait_until("dropped and back", || {
            count(|m| matches!(m, MemberPresence::Unreachable { .. })) >= n && count(online) >= n
        });
    }
    srv.shutdown();
}

#[test]
fn crash_shows_unreachable() {
    let r = relay();
    let h = TestHome::new();
    let mut p = ServeProc::start(&h, &[]);
    let mut c = Client::connect(&h.paths().socket);
    c.hello(range(1, 1));
    let id = identity(&mut c);
    let ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    join(&mut c, ring.tokens()).unwrap();
    wait_presence(&r, &ring, &id.sign, online);
    unsafe { libc::kill(p.pid(), libc::SIGKILL) };
    assert!(p.wait_exit(T).is_some());
    wait_presence(&r, &ring, &id.sign, |m| {
        matches!(m, MemberPresence::Unreachable { .. })
    });
}

#[test]
fn follows_new_roster_relay_url() {
    let (a, b) = (relay(), relay());
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let mut ring = Ring::new(&a.url(), &id, RingRole::Daemon);
    join(&mut c, ring.tokens()).unwrap();
    wait_presence(&a, &ring, &id.sign, online);
    let (desk, _) = ring.desk_client(&a);
    let v3 = ring.next(|d| d.relay_url = b.url());
    desk.publish_roster(&v3).unwrap();
    wait_presence(&b, &ring, &id.sign, online);
    wait_until("version 3 stored", || stored_versions(&h) == 3);
    assert_eq!(identity(&mut c).raw["ring"]["relayUrl"], b.url());
}

#[test]
fn conditional_join_checks_the_membership() {
    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let mut c = desktop(&srv);
    let id = identity(&mut c);
    let mut ring = Ring::new(&r.url(), &id, RingRole::Daemon);
    let rid = ring.chain.ring_id().as_str().to_string();
    // Unpaired: a join expecting some Ring is refused, one expecting none goes through.
    assert_eq!(
        cjoin(&mut c, ring.tokens(), Some(&rid), None).unwrap_err(),
        MEMBERSHIP_CHANGED
    );
    assert!(identity(&mut c).raw["ring"].is_null());
    assert_eq!(
        cjoin(&mut c, ring.tokens(), None, None).unwrap()["version"],
        2
    );
    // Now in that Ring: "none" no longer holds, the Ring does, at its version.
    ring.next(|d| d.relay_url = r.url() + "/");
    assert_eq!(
        cjoin(&mut c, ring.tokens(), None, None).unwrap_err(),
        MEMBERSHIP_CHANGED
    );
    assert_eq!(
        cjoin(&mut c, ring.tokens(), Some(&rid), Some(1)).unwrap_err(),
        MEMBERSHIP_CHANGED
    );
    assert_eq!(stored_versions(&h), 2, "nothing stored by a refusal");
    assert_eq!(
        cjoin(&mut c, ring.tokens(), Some(&rid), Some(2)).unwrap()["version"],
        3
    );
    // Another Ring, expecting the current one at its version: the Host moves.
    let other = Ring::new(&r.url(), &id, RingRole::Daemon);
    assert_eq!(
        cjoin(&mut c, other.tokens(), Some(&rid), Some(3)).unwrap()["version"],
        2
    );
    assert_eq!(
        identity(&mut c).raw["ring"]["ringId"],
        other.chain.ring_id().as_str()
    );
    srv.shutdown();
}

/// Two Desktops both saw the Host unpaired; their conditional joins race. Exactly one
/// wins, and the Host stays in the winner's Ring.
#[test]
fn competing_conditional_joins_leave_one_ring() {
    let r = relay();
    for _ in 0..4 {
        let h = TestHome::new();
        let srv = server(&h);
        let mut a = desktop(&srv);
        let mut b = desktop(&srv);
        let id = identity(&mut a);
        assert!(identity(&mut b).raw["ring"].is_null());
        let ra = Ring::new(&r.url(), &id, RingRole::Daemon);
        let rb = Ring::new(&r.url(), &id, RingRole::Daemon);
        // Both identity replies are in; now both join at once.
        let barrier = std::sync::Barrier::new(2);
        let (x, y) = std::thread::scope(|s| {
            let ja = s.spawn(|| {
                barrier.wait();
                cjoin(&mut a, ra.tokens(), None, None)
            });
            let jb = s.spawn(|| {
                barrier.wait();
                cjoin(&mut b, rb.tokens(), None, None)
            });
            (ja.join().unwrap(), jb.join().unwrap())
        });
        assert!(x.is_ok() != y.is_ok(), "exactly one wins: {x:?} {y:?}");
        let (winner, loser) = if x.is_ok() { (&ra, &y) } else { (&rb, &x) };
        assert_eq!(loser.as_ref().unwrap_err(), MEMBERSHIP_CHANGED);
        let mut c = desktop(&srv);
        assert_eq!(
            identity(&mut c).raw["ring"]["ringId"],
            winner.chain.ring_id().as_str()
        );
        srv.shutdown();
    }
}

/// The Desktop side end to end: `DesktopRing` over a real host link to this Daemon.
#[test]
fn desktop_ring_enables_and_sees_local_daemon() {
    use std::sync::mpsc;
    use xshell_hostlink::ring::{
        DesktopRing, DesktopRingConfig, LocalIdentity, RingObserver, RingView,
    };

    struct Quiet;
    impl RingObserver for Quiet {
        fn changed(&self, _: &RingView) {}
    }
    fn sync<T: Send + 'static>(f: impl FnOnce(Box<dyn FnOnce(T) + Send>)) -> T {
        let (tx, rx) = mpsc::channel();
        f(Box::new(move |r| {
            let _ = tx.send(r);
        }));
        rx.recv_timeout(T).expect("no reply")
    }

    let r = relay();
    let h = TestHome::new();
    let srv = server(&h);
    let tap = Arc::new(desktop::Tap::default());
    let desk = desktop::socket_desk(&srv.socket, tap, |_| {});
    desk.wait_usable();
    let host = desk.host();
    let gen = host.status().link_generation;
    let id = sync(|w| host.ring_identity(gen, w)).expect("ring.identity");
    let local = LocalIdentity::from_json(&id).unwrap();

    let app = tempfile::tempdir().unwrap();
    let mut cfg = DesktopRingConfig::new(app.path().join("ring"), "desk".into());
    cfg.default_relay_url = r.url();
    cfg.backoff_unit = Duration::from_millis(10);
    let ring = DesktopRing::open(cfg, Arc::new(Quiet));
    let v = ring.enable(Some(local.clone()), false).unwrap();
    assert_eq!(v.members.len(), 2);
    let chain = ring.chain().unwrap();
    let joined = sync(|w| host.ring_join(tokens(&chain), None, gen, w)).expect("ring.join");
    assert_eq!(joined["version"], 1);
    wait_until("the Daemon connected, as the Desktop sees it", || {
        ring.view()
            .members
            .iter()
            .any(|m| m.this_computer && m.presence.kind == "online")
    });
    // Moving the Ring moves this Daemon too.
    let b = relay();
    let c = ring.set_relay_url(&b.url()).unwrap();
    sync(|w| host.ring_join(tokens(&c), None, gen, w)).expect("ring.join");
    let rid = c.ring_id().clone();
    wait_until("the Daemon on the new relay", || {
        b.presence(&rid, &local.sign_key).is_some_and(|p| p.online)
    });
    ring.quit();
}

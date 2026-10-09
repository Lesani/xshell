//! Seam 2 harness: the test Relay, in-process Daemons in a Ring the test signs itself, and
//! peers (phones, Desktops) built from the protocol's Connector and sessions.

use super::*;
use std::sync::Arc;
use xshell_protocol::ring::relay::contract;
use xshell_protocol::ring::relay::sessions::{SessionEvents, Sessions, SessionsConfig};
use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions};
use xshell_protocol::ring::relay::{Connector, ConnectorConfig, LinkState, RingTimeouts};
use xshell_protocol::ring::{
    DeviceKeys, Member, NoiseKey, Role as RingRole, RosterChain, SignKey, SignedRoster,
};

pub fn relay() -> TestRelay {
    relay_with(TestRelayOptions::default())
}

/// A test Relay with a short auth deadline and `opts` otherwise.
pub fn relay_with(opts: TestRelayOptions) -> TestRelay {
    TestRelay::start_with(TestRelayOptions {
        auth_timeout: Duration::from_millis(500),
        ..opts
    })
}

pub fn timeouts() -> RingTimeouts {
    RingTimeouts {
        connect: Duration::from_secs(5),
        ping_interval: Duration::from_millis(300),
        dead_after: Duration::from_secs(3),
        request: Duration::from_secs(2),
        bye: Duration::from_millis(800),
    }
}

pub fn fast(c: &mut Config) {
    c.ring_backoff_unit = Duration::from_millis(10);
    c.ring_timeouts = timeouts();
}

pub fn wait_until(what: &str, within: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + within;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn identity(srv: &ServerHandle) -> (SignKey, NoiseKey, Value) {
    let mut c = Client::in_process(srv, Role::Desktop);
    let v = c.request(&ClientMsg::RingIdentity).expect("ring.identity");
    c.shutdown();
    (
        SignKey::parse(v["signKey"].as_str().unwrap()).unwrap(),
        NoiseKey::parse(v["noiseKey"].as_str().unwrap()).unwrap(),
        v,
    )
}

pub fn tokens(c: &RosterChain) -> Vec<String> {
    c.versions().iter().map(|r| r.token().to_string()).collect()
}

pub fn join(srv: &ServerHandle, chain: &RosterChain) {
    let mut c = Client::in_process(srv, Role::Desktop);
    c.request(&ClientMsg::RingJoin {
        rosters: tokens(chain),
        expect: None,
    })
    .expect("ring.join");
    // Gone, so the tests can count connections.
    c.shutdown();
}

/// The connections once the short-lived local ones are gone.
pub fn settled(srv: &ServerHandle) -> usize {
    std::thread::sleep(Duration::from_millis(200));
    srv.connections()
}

pub fn online(r: &TestRelay, ring: &xshell_protocol::ring::RingId, key: &SignKey) -> bool {
    r.presence(ring, key).is_some_and(|p| p.online)
}

/// A phone (or a Desktop) on the Relay: a Connector and the sessions over it.
pub struct Peer {
    pub sessions: Sessions,
    pub connector: Arc<Connector>,
}

impl Peer {
    pub fn new(r: &TestRelay, chain: &RosterChain, keys: &Arc<DeviceKeys>) -> Peer {
        let mut sc = SessionsConfig::new(keys.clone());
        sc.hs_timeout = Duration::from_millis(500);
        sc.open_timeout = Duration::from_secs(5);
        let sessions = Sessions::new(sc, None);
        let mut client = contract::config(&r.target(), chain, keys.clone());
        client.timeouts = timeouts();
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
        wait_until("the peer connected", T, || {
            connector.state() == (LinkState::Connected { limited: false })
        });
        Peer {
            sessions,
            connector,
        }
    }

    /// A protocol client over a session to `daemon`, after its hello.
    pub fn client(&self, daemon: &SignKey) -> Client {
        let s = self.sessions.open(daemon).expect("session opens");
        let mut c = Client::from_io(s.try_clone(), s);
        c.hello(range(1, 1));
        c
    }
}

/// A Desktop's keys and chain, with the Daemon in version 2.
pub struct Ring {
    pub desk: Arc<DeviceKeys>,
    pub chain: RosterChain,
}

impl Ring {
    pub fn new(url: &str, daemon: (SignKey, NoiseKey)) -> Ring {
        let desk = Arc::new(DeviceKeys::generate().unwrap());
        let now = contract::now();
        let v1 = SignedRoster::genesis(&*desk, desk.noise_key(), "desk", url, now).unwrap();
        let v2 = v1
            .next(&*desk, now, |d| {
                d.add(Member::new(
                    "host",
                    RingRole::Daemon,
                    daemon.0,
                    daemon.1,
                    now,
                ))
            })
            .unwrap();
        Ring {
            desk,
            chain: RosterChain::from_chain(vec![v1, v2]).unwrap(),
        }
    }

    pub fn add(&mut self, k: &DeviceKeys, role: RingRole) -> SignedRoster {
        self.add_keys(k.sign_key(), k.noise_key(), role)
    }

    pub fn add_keys(&mut self, sign: SignKey, noise: NoiseKey, role: RingRole) -> SignedRoster {
        self.next(|d| d.add(Member::new("dev", role, sign, noise, contract::now())))
    }

    pub fn next(
        &mut self,
        edit: impl FnOnce(&mut xshell_protocol::ring::RosterDraft),
    ) -> SignedRoster {
        let n = self
            .chain
            .head()
            .next(&*self.desk, contract::now(), edit)
            .unwrap();
        self.chain.accept(std::slice::from_ref(&n)).unwrap();
        n
    }
}

/// A Daemon in a Ring the test signs, with a Mobile and a second Desktop added and published.
pub struct Setup {
    pub r: TestRelay,
    pub h: TestHome,
    pub srv: ServerHandle,
    pub ring: Ring,
    pub daemon: SignKey,
    pub mobile: Arc<DeviceKeys>,
    pub desk2: Arc<DeviceKeys>,
}

pub fn setup() -> Setup {
    setup_with(fast)
}

pub fn setup_with(tweak: impl FnOnce(&mut Config)) -> Setup {
    setup_on(relay(), tweak)
}

/// [`setup_with`] on the Relay `r`.
pub fn setup_on(r: TestRelay, tweak: impl FnOnce(&mut Config)) -> Setup {
    let h = TestHome::new();
    let srv = start(&h, tweak);
    let (sign, noise, _) = identity(&srv);
    let mut ring = Ring::new(&r.url(), (sign, noise));
    let (mobile, desk2) = (contract::keys(), contract::keys());
    ring.add(&mobile, RingRole::Mobile);
    ring.add(&desk2, RingRole::Desktop);
    join(&srv, &ring.chain);
    let id = ring.chain.ring_id().clone();
    wait_until("the daemon is online", T, || online(&r, &id, &sign));
    Setup {
        r,
        h,
        srv,
        ring,
        daemon: sign,
        mobile,
        desk2,
    }
}

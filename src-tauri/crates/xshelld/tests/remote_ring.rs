#![cfg(unix)]
//! Remote Hosts join the Ring (#21): the Desktop's real Ring worker (xshell-hostlink's
//! `SyncWorker`) over real host links to real Daemons, each an in-process `Server` in its
//! own test home, with the in-process test Relay. "Reachable" is what Settings → Mobile
//! shows: the member online on the Relay, as the Desktop sees it.

mod common;
mod desktop;

use common::*;
use desktop::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_hostlink::ring::{
    DesktopRing, DesktopRingConfig, HostSync, LocalIdentity, RingObserver, RingView, SyncWorker,
};
use xshell_hostlink::{
    FileSource, HostConfig, HostStatus, Manager, ManagerConfig, Observer, TransportFactory,
};
use xshell_protocol::msg::{ClientMsg, TerminalInfo};
use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions};
use xshell_protocol::ring::relay::RingTimeouts;
use xshell_protocol::ring::{DeviceKeys, Member, Role, RosterChain, SignKey, SignedRoster};
use xshelld::server::{Config, Role as ConnRole, ServerHandle};

const A: &str = "h_aaaaaaaa";
const B: &str = "h_bbbbbbbb";
const WAIT: Duration = Duration::from_secs(20);

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

/// A Remote Host: its home and its Daemon.
struct Remote {
    home: TestHome,
    srv: Option<ServerHandle>,
}

impl Remote {
    fn start() -> Remote {
        let home = TestHome::new();
        let srv = start(&home, fast);
        Remote {
            home,
            srv: Some(srv),
        }
    }

    fn socket(&self) -> std::path::PathBuf {
        self.home.paths().socket
    }

    /// Its Daemon's identity, asked directly.
    fn key(&self) -> SignKey {
        let mut c = direct(self.srv.as_ref().unwrap());
        let v = c.request(&ClientMsg::RingIdentity).unwrap();
        LocalIdentity::from_json(&v).unwrap().sign_key
    }

    fn ring_id(&self) -> Option<String> {
        let mut c = direct(self.srv.as_ref().unwrap());
        let v = c.request(&ClientMsg::RingIdentity).unwrap();
        v["ring"]["ringId"].as_str().map(String::from)
    }

    fn stop(&mut self) {
        if let Some(s) = self.srv.take() {
            s.shutdown();
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A Desktop connection straight to a Remote Host's Daemon, for checking its state. It
/// waits as long as the rest of the test: on a loaded machine (the whole workspace's tests
/// at once, every Daemon fsyncing its keys and Roster) an answer can take more than the
/// harness's usual 5 s, and these checks are about state, not latency.
fn direct(srv: &ServerHandle) -> Client {
    Client::in_process_within(srv, ConnRole::Desktop, WAIT)
}

/// The app's Observer, minus the events: only the Ring hooks.
struct Obs(&'static HostSync);

impl Observer for Obs {
    fn status(&self, s: &HostStatus) {
        self.0.observe(s);
    }
    fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
    fn renamed(&self, host: &str) {
        self.0.renamed(host);
    }
}

struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}

/// A Desktop: its Manager, Ring and worker.
struct App {
    m: Arc<Manager>,
    sync: &'static HostSync,
    ring: Arc<DesktopRing>,
    worker: Arc<SyncWorker>,
    _dir: tempfile::TempDir,
}

impl Drop for App {
    fn drop(&mut self) {
        self.sync.shutdown();
        self.m.shutdown();
        self.ring.quit();
    }
}

fn host(id: &str, name: &str, daemon_command: Option<String>) -> HostConfig {
    HostConfig {
        id: id.into(),
        name: name.into(),
        ssh_target: "local".into(),
        color: None,
        daemon_command,
        launch_prefixes: Default::default(),
    }
}

impl App {
    fn new(r: &TestRelay, factory: Arc<dyn TransportFactory>) -> App {
        let sync: &'static HostSync = Box::leak(Box::new(HostSync::new()));
        let mut c = ManagerConfig::new(
            VERSION,
            factory,
            Arc::new(FileSource(bin().into())),
            Arc::new(Obs(sync)),
        );
        c.backoff_unit = Duration::from_millis(50);
        c.hello_timeout = Duration::from_secs(15);
        let m = Arc::new(Manager::new(c));
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = DesktopRingConfig::new(dir.path().join("ring"), "desk".into());
        cfg.default_relay_url = r.url();
        cfg.hosted_relay_url = r.url();
        cfg.backoff_unit = Duration::from_millis(10);
        let ring = DesktopRing::open(cfg, Arc::new(Quiet));
        let weak = Arc::downgrade(&m);
        let worker = SyncWorker::new(
            ring.clone(),
            sync,
            Box::new(move |id| weak.upgrade()?.host(id)),
            Box::new(|| {}),
        );
        worker.start().unwrap();
        App {
            worker,
            m,
            sync,
            ring,
            _dir: dir,
        }
    }

    fn with_remotes(r: &TestRelay, remotes: &[(&str, &Remote)]) -> App {
        let paths: HashMap<String, _> = remotes
            .iter()
            .map(|(id, h)| (id.to_string(), h.socket()))
            .collect();
        Self::new(r, Arc::new(MapFactory { paths }))
    }
}

fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn online(ring: &DesktopRing, key: &SignKey) -> bool {
    ring.view()
        .members
        .iter()
        .any(|m| m.sign_key == *key && m.role == Role::Daemon && m.presence.kind == "online")
}

fn version(ring: &DesktopRing) -> u64 {
    ring.chain().unwrap().head().version()
}

fn enabled(r: &TestRelay, remotes: &[(&str, &Remote)]) -> App {
    let app = App::with_remotes(r, remotes);
    app.ring.enable(None, false).unwrap();
    app
}

#[test]
fn remote_hosts_join_the_ring_when_they_connect() {
    let r = relay();
    let (a, b) = (Remote::start(), Remote::start());
    let app = enabled(&r, &[(A, &a), (B, &b)]);
    app.m
        .configure(vec![host(A, "Alpha", None), host(B, "Beta", None)])
        .unwrap();
    let (ka, kb) = (a.key(), b.key());
    wait("both online", || {
        online(&app.ring, &ka) && online(&app.ring, &kb)
    });
    let v = app.ring.view();
    let names: Vec<_> = v
        .members
        .iter()
        .map(|m| (m.name.as_str(), m.role, m.host_id.as_deref()))
        .collect();
    assert_eq!(
        names,
        [
            ("desk", Role::Desktop, None),
            ("Alpha", Role::Daemon, Some(A)),
            ("Beta", Role::Daemon, Some(B))
        ]
    );
    assert_eq!(version(&app.ring), 2, "one version for both");
    assert!(app.worker.host_states().is_empty());
    let rid = app.ring.view().ring_id.unwrap();
    assert_eq!(a.ring_id().as_deref(), Some(rid.as_str()));
}

#[test]
fn a_host_connecting_later_joins_without_extra_steps() {
    let r = relay();
    let (a, b) = (Remote::start(), Remote::start());
    let app = enabled(&r, &[(A, &a), (B, &b)]);
    app.m.configure(vec![host(A, "Alpha", None)]).unwrap();
    let ka = a.key();
    wait("a online", || online(&app.ring, &ka));
    app.m
        .configure(vec![host(A, "Alpha", None), host(B, "Beta", None)])
        .unwrap();
    let kb = b.key();
    wait("b online", || online(&app.ring, &kb));
    assert!(online(&app.ring, &ka));
    assert_eq!(version(&app.ring), 3);
}

#[test]
fn enabling_mobile_access_adds_connected_hosts() {
    let r = relay();
    let (a, b) = (Remote::start(), Remote::start());
    let app = App::with_remotes(&r, &[(A, &a), (B, &b)]);
    app.m
        .configure(vec![host(A, "Alpha", None), host(B, "Beta", None)])
        .unwrap();
    wait("both connected", || {
        [A, B]
            .iter()
            .all(|id| app.m.host(id).is_some_and(|h| h.status().usable()))
    });
    std::thread::sleep(Duration::from_millis(600));
    assert!(a.ring_id().is_none(), "no Ring yet: nothing joined");
    app.ring.enable(None, false).unwrap();
    app.sync.poke_all();
    let (ka, kb) = (a.key(), b.key());
    wait("both online", || {
        online(&app.ring, &ka) && online(&app.ring, &kb)
    });
    assert_eq!(version(&app.ring), 2, "added together in v2");
}

#[test]
fn remote_hosts_follow_a_relay_change() {
    let (one, two) = (relay(), relay());
    let (a, b) = (Remote::start(), Remote::start());
    let app = enabled(&one, &[(A, &a), (B, &b)]);
    app.m
        .configure(vec![host(A, "Alpha", None), host(B, "Beta", None)])
        .unwrap();
    let (ka, kb) = (a.key(), b.key());
    wait("both online", || {
        online(&app.ring, &ka) && online(&app.ring, &kb)
    });
    let rid = app.ring.view().ring_id.unwrap();
    app.ring.set_relay_url(&two.url()).unwrap();
    app.sync.poke_all();
    wait("both on the new relay", || {
        [ka, kb]
            .iter()
            .all(|k| two.presence(&rid, k).is_some_and(|p| p.online))
    });
    wait("seen online there", || {
        online(&app.ring, &ka) && online(&app.ring, &kb)
    });
}

#[test]
fn reinstalled_host_replaces_its_member() {
    let r = relay();
    let mut a = Remote::start();
    let app = enabled(&r, &[(A, &a)]);
    app.m.configure(vec![host(A, "Alpha", None)]).unwrap();
    let old = a.key();
    wait("online", || online(&app.ring, &old));
    // A reinstall: the Daemon's ring state is gone, the Host reconnects to a new one.
    a.stop();
    std::fs::remove_dir_all(a.home.paths().ring_dir).unwrap();
    a.srv = Some(start(&a.home, fast));
    let new = a.key();
    assert_ne!(new, old);
    wait("the new key online", || online(&app.ring, &new));
    let c = app.ring.chain().unwrap();
    assert!(c.head().member(&old).is_none(), "the old key is gone");
    assert_eq!(c.head().member(&new).unwrap().name, "Alpha");
    assert_eq!(c.head().roster().members.len(), 2);
}

#[test]
fn host_in_another_ring_is_left_alone_until_claimed() {
    let r = relay();
    let a = Remote::start();
    // Another Desktop's Ring holds the Host.
    let key = a.key();
    let noise = {
        let mut c = direct(a.srv.as_ref().unwrap());
        let v = c.request(&ClientMsg::RingIdentity).unwrap();
        LocalIdentity::from_json(&v).unwrap().noise_key
    };
    let other = Arc::new(DeviceKeys::generate().unwrap());
    let g = SignedRoster::genesis(&*other, other.noise_key(), "other", &r.url(), 1).unwrap();
    let v2 = g
        .next(&*other, 2, |d| {
            d.add(Member::new("theirs", Role::Daemon, key, noise, 2))
        })
        .unwrap();
    let foreign = RosterChain::from_chain(vec![g, v2]).unwrap();
    let mut c = direct(a.srv.as_ref().unwrap());
    c.request(&ClientMsg::RingJoin {
        rosters: foreign
            .versions()
            .iter()
            .map(|x| x.token().to_string())
            .collect(),
        expect: None,
    })
    .unwrap();
    let foreign_id = foreign.ring_id().as_str().to_string();

    let app = enabled(&r, &[(A, &a)]);
    app.m.configure(vec![host(A, "Shared", None)]).unwrap();
    wait("other ring", || {
        app.worker.state_of(A) == Some("other-ring")
    });
    let st = app.worker.host_states();
    assert_eq!((st[0].name.as_str(), st[0].state), ("Shared", "other-ring"));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        a.ring_id().as_deref(),
        Some(foreign_id.as_str()),
        "left alone"
    );
    assert_eq!(version(&app.ring), 1);
    app.worker.claim(A).unwrap();
    wait("claimed and online", || online(&app.ring, &key));
    let ours = app.ring.view().ring_id.unwrap();
    assert_eq!(a.ring_id().as_deref(), Some(ours.as_str()));
    // The member can be online on the Relay before the worker has the join's answer, which
    // is what clears the Host's `other-ring` state (#39): wait for that, not for a moment.
    wait("the other-ring state cleared", || {
        app.worker.host_states().is_empty()
    });
}

/// A Host added with a managed install (probe, upload, `xshelld connect`) joins too.
#[test]
fn managed_install_joins_the_ring() {
    let r = relay();
    let home = TestHome::new();
    let _guard = DaemonGuard::new(&home);
    let app = App::new(&r, local_factory(&home));
    app.ring.enable(None, false).unwrap();
    app.m.configure(vec![host(ID, "Installed", None)]).unwrap();
    wait("a member online", || {
        app.ring
            .view()
            .members
            .iter()
            .any(|m| m.host_id.as_deref() == Some(ID) && m.presence.kind == "online")
    });
    let s = app.m.host(ID).unwrap().status();
    assert_eq!(s.daemon_version.as_deref(), Some(VERSION));
    assert!(home
        .home()
        .join(".xshell/server")
        .join(VERSION)
        .join("xshelld")
        .exists());
}

/// Two Desktops with Rings of their own share a Host: it joins one of them, and the other
/// leaves it there (no tug-of-war on reconnects).
#[test]
fn two_desktops_never_take_a_host_from_each_other() {
    let r = relay();
    let a = Remote::start();
    let one = enabled(&r, &[(A, &a)]);
    let two = enabled(&r, &[(A, &a)]);
    one.m.configure(vec![host(A, "Shared", None)]).unwrap();
    two.m.configure(vec![host(A, "Shared", None)]).unwrap();
    let key = a.key();
    wait("settled", || {
        let held = [&one, &two]
            .iter()
            .filter(|x| online(&x.ring, &key))
            .count();
        let left = [&one, &two]
            .iter()
            .filter(|x| x.worker.state_of(A) == Some("other-ring"))
            .count();
        held == 1 && left == 1
    });
    let (winner, loser) = if online(&one.ring, &key) {
        (&one, &two)
    } else {
        (&two, &one)
    };
    let rid = winner.ring.view().ring_id.unwrap();
    // Both reconnect: the Host stays where it is.
    for x in [&one, &two] {
        x.m.configure(vec![host(A, "Shared", Some("x".into()))])
            .unwrap();
    }
    wait("reconnected", || {
        [&one, &two]
            .iter()
            .all(|x| x.m.host(A).is_some_and(|h| h.status().usable()))
    });
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(a.ring_id().as_deref(), Some(rid.as_str()));
    wait("the loser still leaves it", || {
        loser.worker.state_of(A) == Some("other-ring")
    });
    // The loser may have added the key before its join was refused; that member never
    // connects (removing members is #22's).
    assert!(!online(&loser.ring, &key));
}

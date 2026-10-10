#![cfg(unix)]
//! Seam 2: sessions through the Relay into the Daemon. The test Relay, the real
//! `DesktopRing` (pairing), in-process Daemons and the protocol's Ring client standing in for
//! the phone. A session is one more protocol connection with the peer's Roster role.

mod common;

use common::ring::*;
use common::*;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_hostlink::ring::{
    DesktopRing, DesktopRingConfig, LocalIdentity, PairingEvent, PairingFlow, PairingObserver,
    RingObserver, RingView,
};
use xshell_protocol::msg::ClientMsg;
use xshell_protocol::ring::noise::{Header, Initiator, Kind};
use xshell_protocol::ring::pairing::PairingOffer;
use xshell_protocol::ring::relay::contract;
use xshell_protocol::ring::relay::pair::{pair_as_guest, GuestRequest, PairOptions};
use xshell_protocol::ring::relay::sessions::SessionError;
use xshell_protocol::ring::relay::test_relay::{TestRelay, Verdict};
use xshell_protocol::ring::relay::wire::{ClientFrame, ErrorCode, RelayFrame};
use xshell_protocol::ring::relay::LinkState;
use xshell_protocol::ring::{b64, DeviceKeys, RingError, Role as RingRole, RosterChain, SignKey};
use xshelld::server::{Role, ServerHandle};

const FORBIDDEN: &str = "forbidden for mobile";

// ---- Pairing a phone through the real DesktopRing --------------------------------------------

#[derive(Default)]
struct Events(Mutex<Vec<PairingEvent>>);

impl PairingObserver for Events {
    fn pairing(&self, _: PairingFlow, e: &PairingEvent) {
        self.0.lock().unwrap().push(e.clone());
    }
}

struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}

fn desktop_ring(dir: &std::path::Path, url: &str) -> Arc<DesktopRing> {
    let mut cfg = DesktopRingConfig::new(dir.join("ring"), "desk".into());
    cfg.default_relay_url = url.into();
    cfg.hosted_relay_url = url.into();
    cfg.backoff_unit = Duration::from_millis(10);
    cfg.timeouts = timeouts();
    cfg.owner_retry = Duration::from_millis(50);
    DesktopRing::open(cfg, Arc::new(Quiet))
}

fn local(v: &Value) -> LocalIdentity {
    LocalIdentity::from_json(v).unwrap()
}

fn guest_opts() -> PairOptions {
    PairOptions {
        step: Duration::from_secs(5),
        ring: timeouts(),
        ..PairOptions::default()
    }
}

/// The phone scans a QR code from `ring`: it is a member when this returns.
fn pair_phone(ring: &DesktopRing, phone: &Arc<DeviceKeys>) -> RosterChain {
    let ev = Arc::new(Events::default());
    let offer = ring.pair_phone(ev).unwrap();
    let o = PairingOffer::parse(&offer.payload).unwrap();
    pair_as_guest(
        &GuestRequest {
            relay_url: &o.relay_url,
            secret: &o.secret,
            pin: Some(o.noise_key),
            ring_id: Some(&o.ring_id),
            keys: phone.clone(),
            role: RingRole::Mobile,
            name: "phone",
            wait: Duration::ZERO,
        },
        &guest_opts(),
        None,
    )
    .expect("the phone pairs")
}

/// A Desktop with Mobile access on, its local Daemon `srv` in the Ring and online.
fn desktop_with_daemon(
    r: &TestRelay,
    dir: &std::path::Path,
    srv: &ServerHandle,
) -> Arc<DesktopRing> {
    let ring = desktop_ring(dir, &r.url());
    let (sign, _, v) = identity(srv);
    ring.enable(Some(local(&v)), false).unwrap();
    join(srv, &ring.chain().unwrap());
    let id = ring.chain().unwrap().ring_id().clone();
    wait_until("the daemon is online", T, || online(r, &id, &sign));
    ring
}

#[test]
fn paired_mobile_session_enforces_mobile_role() {
    let r = relay();
    let h = TestHome::new();
    let srv = start(&h, fast);
    let t = tempfile::tempdir().unwrap();
    let ring = desktop_with_daemon(&r, t.path(), &srv);
    let (daemon, _, _) = identity(&srv);
    let phone_keys = contract::keys();
    let chain = pair_phone(&ring, &phone_keys);
    let phone = Peer::new(&r, &chain, &phone_keys);
    let mut c = phone.client(&daemon);
    let cwd = h.project("p");
    let cwd = cwd.to_string_lossy().into_owned();
    // Files: refused. Session data: served.
    assert_eq!(
        c.call("list_dir", json!({ "path": cwd })),
        Err(format!("{FORBIDDEN}: call list_dir"))
    );
    assert!(c.call("list_claude_projects", json!({})).is_ok());
    // A shell Terminal: refused.
    let e = c
        .request(&open_msg(Uuid::new_v4(), sh_spec(&h.project("p"))))
        .unwrap_err();
    assert!(e.starts_with(FORBIDDEN), "{e}");
    // The local Desktop is unaffected and may do all of it.
    let mut d = Client::in_process(&srv, Role::Desktop);
    assert!(d.call("list_dir", json!({ "path": cwd })).is_ok());
    ring.quit();
}

/// The second computer `h2` (running `srv2`) pairs itself with `xshelld pair`, its code
/// typed on the Desktop: it never gets a local `ring.join`. Returns once it is online.
fn pair_second_computer(r: &TestRelay, ring: &DesktopRing, h2: &TestHome, srv2: &ServerHandle) {
    let (d2, _, _) = identity(srv2);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    struct Lines(std::sync::mpsc::Sender<String>, Vec<u8>);
    impl Write for Lines {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.1.extend_from_slice(b);
            while let Some(i) = self.1.iter().position(|c| *c == b'\n') {
                let line: Vec<u8> = self.1.drain(..=i).collect();
                let _ = self
                    .0
                    .send(String::from_utf8_lossy(&line).trim().to_string());
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let opts = xshelld::cli::Opts {
        home: Some(h2.home()),
        relay: Some(r.url()),
        name: Some("second".into()),
        ..Default::default()
    };
    let paths2 = h2.paths();
    let pairing = std::thread::spawn(move || {
        xshelld::pair::run_pair_to(&opts, &paths2, &mut Lines(tx, Vec::new()))
    });
    let code = loop {
        let l = rx.recv_timeout(T).expect("the code is shown");
        if let Some(c) = l.strip_prefix("Pairing code: ") {
            break c.to_string();
        }
    };
    std::thread::sleep(Duration::from_millis(200));
    ring.pair_computer(&code, Arc::new(Events::default()))
        .unwrap();
    assert_eq!(pairing.join().unwrap(), 0);
    let id = ring.chain().unwrap().ring_id().clone();
    wait_until("the second daemon is online", T, || online(r, &id, &d2));
}

#[test]
fn every_daemon_accepts_new_member_within_seconds() {
    let r = relay();
    let (h1, h2) = (TestHome::new(), TestHome::new());
    let (srv1, srv2) = (start(&h1, fast), start(&h2, fast));
    let t = tempfile::tempdir().unwrap();
    let ring = desktop_with_daemon(&r, t.path(), &srv1);
    pair_second_computer(&r, &ring, &h2, &srv2);
    let (d2, _, _) = identity(&srv2);

    // A phone pairs; both Daemons take its sessions within seconds of the new version.
    let (d1, _, _) = identity(&srv1);
    let phone_keys = contract::keys();
    let chain = pair_phone(&ring, &phone_keys);
    let paired = Instant::now();
    let phone = Peer::new(&r, &chain, &phone_keys);
    for d in [d1, d2] {
        let mut c = phone.client(&d);
        assert!(c.call("list_claude_projects", json!({})).is_ok());
    }
    assert!(
        paired.elapsed() < Duration::from_secs(5),
        "{:?}",
        paired.elapsed()
    );
    ring.quit();
}

#[test]
fn desktop_role_session_is_desktop() {
    let s = setup();
    let desk = Peer::new(&s.r, &s.ring.chain, &s.desk2);
    let mut c = desk.client(&s.daemon);
    let cwd = s.h.project("p").to_string_lossy().into_owned();
    assert!(c.call("list_dir", json!({ "path": cwd })).is_ok());
}

#[test]
fn daemon_role_peer_refused() {
    let mut s = setup();
    let other = contract::keys();
    let next = s.ring.add(&other, RingRole::Daemon);
    let (c, _) = contract::connect(&s.r.target(), &s.ring.chain, s.ring.desk.clone());
    c.publish_roster(&next).ok();
    let peer = Peer::new(&s.r, &s.ring.chain, &other);
    match peer.sessions.open(&s.daemon) {
        Err(SessionError::Refused(e)) => assert_eq!(e, "forbidden"),
        Err(e) => panic!("expected a refusal, got {e}"),
        Ok(_) => panic!("a daemon opened a session"),
    }
}

#[test]
fn unknown_noise_key_dropped() {
    // A member that answers the handshake with another Noise key than the Roster's: the
    // Daemon finds no member by that key and stays silent.
    let mut s = setup();
    let real = contract::keys();
    let (sign_seed, _) = real.seeds();
    let liar = Arc::new(DeviceKeys::from_seeds(&sign_seed, &[42; 32]));
    let next = s.ring.add(&real, RingRole::Mobile);
    let (c, _) = contract::connect(&s.r.target(), &s.ring.chain, s.ring.desk.clone());
    c.publish_roster(&next).unwrap();
    let peer = Peer::new(&s.r, &s.ring.chain, &liar);
    let base = settled(&s.srv);
    let t0 = Instant::now();
    assert_eq!(
        peer.sessions.open(&s.daemon).err(),
        Some(SessionError::Timeout)
    );
    assert!(t0.elapsed() >= Duration::from_secs(4));
    assert_eq!(s.srv.connections(), base);
}

fn wait_connections(srv: &ServerHandle, n: usize, within: Duration) {
    let deadline = Instant::now() + within;
    while srv.connections() != n {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {n} connections (have {})",
            srv.connections()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn removal_cuts_live_session() {
    // The Relay keeps the old head (it withholds its answer to the new version, for longer
    // than the test runs) and keeps routing; the Daemon learns of the removal locally
    // (`ring.join`) and cuts the session at once, with its Relay connection untouched.
    let mut s = setup_with(|c| {
        fast(c);
        c.ring_timeouts.request = Duration::from_secs(30);
    });
    let phone = Peer::new(&s.r, &s.ring.chain, &s.mobile);
    let base = settled(&s.srv);
    let mut c = phone.client(&s.daemon);
    assert!(c.call("list_claude_projects", json!({})).is_ok());
    assert_eq!(s.srv.connections(), base + 1);
    s.r.ignore_roster_puts(true);
    let mkey = s.mobile.sign_key();
    s.ring.next(|d| {
        d.remove(&mkey);
    });
    let t0 = Instant::now();
    join(&s.srv, &s.ring.chain);
    wait_connections(&s.srv, base, Duration::from_secs(3));
    assert!(t0.elapsed() < Duration::from_secs(3));
    // The Relay still has the old head and still routes to the phone; the Daemon refuses it.
    let id = s.ring.chain.ring_id().clone();
    assert_eq!(s.r.head_version(&id), Some(4));
    assert!(online(&s.r, &id, &s.mobile.sign_key()));
    assert!(phone.sessions.open(&s.daemon).is_err());
}

#[test]
fn removal_rules_even_when_it_cannot_be_saved() {
    // The roster file cannot be written and the Relay withholds its answers: the removal
    // still disconnects the phone at once, and a fresh handshake from it is refused.
    let mut s = setup_with(|c| {
        fast(c);
        c.ring_timeouts.request = Duration::from_secs(30);
    });
    let phone = Peer::new(&s.r, &s.ring.chain, &s.mobile);
    let base = settled(&s.srv);
    let mut c = phone.client(&s.daemon);
    assert!(c.call("list_claude_projects", json!({})).is_ok());
    s.r.ignore_roster_puts(true);
    let roster = s.h.paths().ring_dir.join("roster.json");
    std::fs::remove_file(&roster).unwrap();
    std::fs::create_dir(&roster).unwrap();
    let mkey = s.mobile.sign_key();
    s.ring.next(|d| {
        d.remove(&mkey);
    });
    let t0 = Instant::now();
    let mut j = Client::in_process(&s.srv, Role::Desktop);
    let e = j
        .request(&ClientMsg::RingJoin {
            rosters: tokens(&s.ring.chain),
            expect: None,
        })
        .unwrap_err();
    j.shutdown();
    assert!(e.contains("could not be saved"), "{e}");
    wait_connections(&s.srv, base, Duration::from_secs(3));
    assert!(t0.elapsed() < Duration::from_secs(3));
    assert!(phone.sessions.open(&s.daemon).is_err());
}

#[test]
fn an_orderly_close_delivers_every_byte_first() {
    // A Desktop sends a lot, then its last request, then closes the session at once: the
    // bridge into the Daemon is still busy with the bulk when the close arrives, and the
    // last request must still be served.
    let s = setup();
    let mut d = Client::in_process(&s.srv, Role::Desktop);
    let term = Uuid::new_v4();
    d.open(term, sh_spec(&s.h.project("p")));
    let peer = Peer::new(&s.r, &s.ring.chain, &s.desk2);
    let stream = peer.sessions.open(&s.daemon).unwrap();
    let mut c = Client::from_io(stream.try_clone(), stream.try_clone());
    c.hello(range(1, 1));
    // Frames of a kind the Daemon skips: about 2 MiB of bulk.
    let mut bulk = Vec::new();
    let payload = vec![0u8; 256 * 1024];
    for _ in 0..8 {
        bulk.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
        bulk.push(9);
        bulk.extend_from_slice(&payload);
    }
    c.send_raw(&bulk);
    c.send(&ClientMsg::TermClose { terminal: term }, None);
    stream.close("done");
    // The last request reached the Daemon: the Terminal ended.
    d.expect_msg("the terminal's exit", |m| {
        matches!(m, xshell_protocol::msg::ServerMsg::TermExit { terminal, .. } if *terminal == term)
    });
}

#[test]
fn presence_drop_ends_session() {
    let s = setup();
    let phone = Peer::new(&s.r, &s.ring.chain, &s.mobile);
    let base = settled(&s.srv);
    let mut c = phone.client(&s.daemon);
    assert!(c.call("list_claude_projects", json!({})).is_ok());
    assert_eq!(s.srv.connections(), base + 1);
    // The phone vanishes from the Relay without a word.
    phone.connector.abandon();
    wait_connections(&s.srv, base, T);
}

#[test]
fn relay_reconnect_ends_sessions() {
    let s = setup();
    let phone = Peer::new(&s.r, &s.ring.chain, &s.mobile);
    let base = settled(&s.srv);
    let mut c = phone.client(&s.daemon);
    assert!(c.call("list_claude_projects", json!({})).is_ok());
    // The Daemon's Relay socket drops; it reconnects, and the session is gone.
    assert!(s.r.kick(s.ring.chain.ring_id(), &s.daemon, 1011));
    wait_connections(&s.srv, base, T);
    let id = s.ring.chain.ring_id().clone();
    wait_until("the daemon is back", T, || online(&s.r, &id, &s.daemon));
    // A new session works.
    let mut c = phone.client(&s.daemon);
    assert!(c.call("list_claude_projects", json!({})).is_ok());
}

#[test]
fn tampering_relay_end_to_end() {
    let s = setup();
    // A Terminal the local Desktop runs.
    let mut d = Client::in_process(&s.srv, Role::Desktop);
    let term = Uuid::new_v4();
    d.open(term, sh_spec(&s.h.project("p")));
    let base = settled(&s.srv);
    let phone = Peer::new(&s.r, &s.ring.chain, &s.mobile);
    s.r.record_payloads(true);
    let mut c = phone.client(&s.daemon);
    let marker = "TAMPER-MARKER-77a1";
    let _ = c.call("list_claude_projects", json!({ "marker": marker }));
    // The Relay saw only ciphertext.
    for p in s.r.recorded_payloads() {
        assert!(!p.windows(marker.len()).any(|w| w == marker.as_bytes()));
    }
    // It flips a bit in the phone's next message: the session dies, nothing reaches the
    // Daemon, and the Terminal runs on.
    s.r.tamper(
        s.ring.chain.ring_id(),
        &s.daemon,
        Some(Box::new(|_, mut p: Vec<u8>| {
            if Header::parse(&p).is_ok_and(|(h, _)| h.kind == Kind::Data) {
                let last = p.len() - 1;
                p[last] ^= 1;
            }
            Verdict::Deliver(vec![p])
        })),
    );
    c.send(&ClientMsg::TermClose { terminal: term }, Some(77));
    wait_connections(&s.srv, base, T);
    s.r.tamper(s.ring.chain.ring_id(), &s.daemon, None);
    let list = d.terminals_where(|l| l.iter().any(|t| t.terminal == term));
    assert!(
        list.iter()
            .any(|t| t.terminal == term && t.exit_code.is_none()),
        "{list:?}"
    );
    d.attach(term);
    d.marker(term, "still-here");
}

// ---- Roster management (#22) -------------------------------------------------------------------

/// Acceptance (a): the Desktop removes a paired phone, and every Daemon cuts it within
/// seconds through the Relay alone (neither gets a local `ring.join` after the removal; the
/// second never got one). The phone stops, and can neither log in nor open a session again.
#[test]
fn removing_a_phone_cuts_it_on_every_daemon_within_seconds() {
    let r = relay();
    let (h1, h2) = (TestHome::new(), TestHome::new());
    let (srv1, srv2) = (start(&h1, fast), start(&h2, fast));
    let t = tempfile::tempdir().unwrap();
    let ring = desktop_with_daemon(&r, t.path(), &srv1);
    pair_second_computer(&r, &ring, &h2, &srv2);
    let ((d1, _, _), (d2, _, _)) = (identity(&srv1), identity(&srv2));
    let phone_keys = contract::keys();
    let chain = pair_phone(&ring, &phone_keys);
    let phone = Peer::new(&r, &chain, &phone_keys);
    let (base1, base2) = (settled(&srv1), settled(&srv2));
    let mut c1 = phone.client(&d1);
    let mut c2 = phone.client(&d2);
    assert!(c1.call("list_claude_projects", json!({})).is_ok());
    assert!(c2.call("list_claude_projects", json!({})).is_ok());
    assert_eq!(
        (srv1.connections(), srv2.connections()),
        (base1 + 1, base2 + 1)
    );

    let t0 = Instant::now();
    let c = ring.remove_member(&phone_keys.sign_key()).unwrap();
    assert!(c.head().member(&phone_keys.sign_key()).is_none());
    wait_connections(&srv1, base1, Duration::from_secs(5));
    wait_connections(&srv2, base2, Duration::from_secs(5));
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());

    wait_until("the phone stops", T, || {
        matches!(
            phone.connector.state(),
            LinkState::Stopped { error: Some(_) }
        )
    });
    match contract::try_connect(&r.target(), &chain, phone_keys.clone()) {
        Err(RingError::Relay { code, .. }) => assert_eq!(code, ErrorCode::NotMember),
        Err(e) => panic!("expected not_member, got {e}"),
        Ok(_) => panic!("the removed phone logged in"),
    }
    assert!(phone.sessions.open(&d1).is_err());
    assert!(phone.sessions.open(&d2).is_err());
    assert!(ring
        .view()
        .members
        .iter()
        .all(|m| m.sign_key != phone_keys.sign_key()));
    ring.quit();
}

fn env_frame(from: &SignKey, payload: &[u8]) -> String {
    json!({"t": "env", "from": from, "payload": b64::encode(payload)}).to_string()
}

/// Acceptance (b): a version a phone signed (adding an intruder Desktop, dropping the real
/// one) is refused by the Relay, and by the Daemon and the Desktop when a Relay sends it
/// anyway; the intruder it names gets no session.
#[test]
fn mobile_signed_roster_is_refused_by_every_device() {
    let r = relay();
    let h = TestHome::new();
    let srv = start(&h, fast);
    let t = tempfile::tempdir().unwrap();
    let ring = desktop_with_daemon(&r, t.path(), &srv);
    let (daemon, _, _) = identity(&srv);
    let phone_keys = contract::keys();
    let chain = pair_phone(&ring, &phone_keys);
    let rid = chain.ring_id().clone();
    let version = chain.head().version();
    let me = ring
        .view()
        .members
        .iter()
        .find(|m| m.this_app)
        .unwrap()
        .sign_key;
    let intruder = contract::keys();
    let forged = contract::raw_next(chain.head(), &*phone_keys, |x| {
        x.members.retain(|m| m.sign_key != me);
        x.members
            .push(contract::member(&intruder, "intruder", RingRole::Desktop));
    });

    // The Relay refuses the phone's upload.
    {
        let mut raw = contract::RawConn::login(&r.target(), &chain, &*phone_keys);
        raw.send(
            &ClientFrame::RosterPut {
                id: 1,
                roster: forged.token().to_string(),
            }
            .encode(),
        )
        .unwrap();
        let e = raw.error(contract::WAIT).expect("an error");
        assert_eq!(
            (e.0, e.1, e.2.as_deref()),
            (
                ErrorCode::RosterInvalid,
                Some(1),
                Some("signer_not_desktop")
            )
        );
    }
    assert_eq!(r.head_version(&rid), Some(version));

    // A live phone session.
    let phone = Peer::new(&r, &chain, &phone_keys);
    let base = settled(&srv);
    let mut c = phone.client(&daemon);
    assert!(c.call("list_claude_projects", json!({})).is_ok());
    let daemon_version = || identity(&srv).2["ring"]["version"].as_u64();
    assert_eq!(daemon_version(), Some(version));

    // A Relay sends the forged version anyway, to the Daemon and to the Desktop.
    let frame = RelayFrame::Roster {
        roster: forged.token().to_string(),
    }
    .encode();
    assert!(r.inject(&rid, &daemon, &frame));
    assert!(r.inject(&rid, &me, &frame));
    // Markers behind it on the same sockets, so the checks below come after it was handled:
    // the session's next call (Daemon) and a presence frame (Desktop).
    assert!(c.call("list_claude_projects", json!({})).is_ok());
    let marker = json!({
        "t": "presence", "signKey": daemon, "online": false,
        "lastSeen": 1, "lastReason": "marker-22",
    })
    .to_string();
    assert!(r.inject(&rid, &me, &marker));
    wait_until("the desktop handled the marker", T, || {
        ring.view()
            .members
            .iter()
            .any(|m| m.sign_key == daemon && m.presence.reason.as_deref() == Some("marker-22"))
    });
    assert_eq!(daemon_version(), Some(version));
    let v = ring.view();
    assert_eq!(v.version, Some(version));
    assert!(v.members.iter().any(|m| m.this_app));
    assert!(v.members.iter().all(|m| m.sign_key != intruder.sign_key()));
    assert_eq!(ring.chain().unwrap().head().version(), version);

    // The intruder's handshake, injected straight to the Daemon (no client of its own would
    // send it): no session, and no answer. The test Relay records every envelope the Daemon
    // sends, before it refuses one to a non-member. The phone's own handshake, injected the
    // same way right after, is the marker: the Daemon handles its envelopes in order, so once
    // it answered the phone it had handled the intruder's.
    let dm = chain.head().member(&daemon).unwrap().clone();
    r.record_payloads(true);
    let (ii, hs1) = Initiator::start(&intruder, &rid, &dm, contract::now_ms()).unwrap();
    assert!(r.inject(&rid, &daemon, &env_frame(&intruder.sign_key(), &hs1)));
    let (pi, hs1) = Initiator::start(&phone_keys, &rid, &dm, contract::now_ms() + 60_000).unwrap();
    assert!(r.inject(&rid, &daemon, &env_frame(&phone_keys.sign_key(), &hs1)));
    let answered = |sid| {
        r.recorded_attempts().iter().any(|(from, _, p)| {
            *from == daemon
                && Header::parse(p).is_ok_and(|(h, _)| h.kind == Kind::Hs2 && h.sid == sid)
        })
    };
    wait_until("the daemon answers the phone", T, || answered(pi.sid()));
    let attempts = r.recorded_attempts();
    assert!(
        attempts.iter().all(|(_, to, _)| *to != intruder.sign_key()),
        "the daemon answered the intruder"
    );
    assert!(!answered(ii.sid()));
    // No session for the intruder (the phone's new handshake replaces its old session).
    assert!(srv.connections() <= base + 1);
    ring.quit();
}

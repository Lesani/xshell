#![cfg(unix)]
//! Seam 2: push notifications from the Daemon (#27). The test Relay forwards to a fake Push
//! Gateway; a phone (the protocol's Connector and sessions) registers over its session; an
//! in-process Daemon runs fake agents whose Agent Status the tests drive with `term.event`,
//! as the hooks would.

mod common;

use common::ring::*;
use common::*;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::AgentStatus;
use xshell_core::launch::LaunchSpec;
use xshell_protocol::msg::{ClientMsg, PushTriggers};
use xshell_protocol::ring::push::{self, collapse_id, PushAgent, PushPayload, PushStatus};
use xshell_protocol::ring::relay::contract::{self, FakeGateway, RawConn, Reply};
use xshell_protocol::ring::relay::test_relay::{Fault, TestRelay, TestRelayOptions};
use xshell_protocol::ring::relay::wire::ClientFrame;
use xshell_protocol::ring::{DeviceKeys, NoiseKey, RingId, Role as RingRole};
use xshelld::server::{Config, PushHooks, PushPoint, Role};

use AgentStatus::*;

/// The push window in these tests.
const W: Duration = Duration::from_millis(1000);
/// Long enough to be sure nothing more comes.
const QUIET: Duration = Duration::from_millis(1600);

/// Fake `claude` and `codex`: they log their `XSHELL_*` environment and pid into their working
/// directory and wait; `codex` answers every input line with an OSC 9 notification.
fn push_agents() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let bin = Path::new(env!("CARGO_TARGET_TMPDIR")).join("push-agents");
        fs::create_dir_all(&bin).unwrap();
        for (name, body) in [
            ("claude", "exec sleep 1000"),
            (
                "codex",
                "while read line; do printf '\\033]9;Approve the edit?\\007'; done",
            ),
        ] {
            let tmp = bin.join(format!(".{name}.{}", std::process::id()));
            fs::write(
                &tmp,
                format!(
                    "#!/bin/sh\ntrap '' HUP\nenv | grep '^XSHELL_' >> env.log\necho $$ >> pids.log\n{body}\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
            fs::rename(&tmp, bin.join(name)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", path);
    });
}

fn tweak(c: &mut Config) {
    fast(c);
    c.push_window = W;
    c.push_timeout = Duration::from_secs(4);
    c.push_retry = Duration::from_millis(400);
}

/// A phone with its seal secret.
struct Phone {
    peer: Peer,
    keys: Arc<DeviceKeys>,
    seal: [u8; 32],
    blob: String,
}

impl Phone {
    fn new(
        r: &TestRelay,
        chain: &xshell_protocol::ring::RosterChain,
        keys: &Arc<DeviceKeys>,
        n: u8,
    ) -> Phone {
        Phone {
            peer: Peer::new(r, chain, keys),
            keys: keys.clone(),
            seal: [0x70 + n; 32],
            blob: format!("xpb1.test.phone{n}"),
        }
    }

    fn seal_key(&self) -> NoiseKey {
        push::seal_key_of(&self.seal).unwrap()
    }

    fn register_raw(
        &self,
        daemon: &xshell_protocol::ring::SignKey,
        msg: &ClientMsg,
    ) -> Result<Value, String> {
        let mut c = self.peer.client(daemon);
        let r = c.request(msg);
        c.shutdown();
        r
    }

    fn register(&self, daemon: &xshell_protocol::ring::SignKey, needs_you: bool, finished: bool) {
        self.register_raw(
            daemon,
            &ClientMsg::PushRegister {
                blob: self.blob.clone(),
                seal_key: self.seal_key().to_b64(),
                triggers: PushTriggers {
                    needs_you,
                    finished,
                },
            },
        )
        .expect("push.register");
    }
}

/// A Daemon in a Ring whose Relay forwards to a fake gateway, a registered phone, and a
/// local Desktop connection.
struct P {
    s: Setup,
    gw: Arc<FakeGateway>,
    phone: Phone,
    desk: Client,
    reapers: Vec<FakeReaper>,
}

fn harness_with(opts: TestRelayOptions, cfg: impl FnOnce(&mut Config), register: bool) -> P {
    push_agents();
    let gw = Arc::new(FakeGateway::start());
    let r = relay_with(TestRelayOptions {
        push_gateway: Some(gw.url()),
        push_timeout: Duration::from_secs(3),
        ..opts
    });
    let s = setup_on(r, cfg);
    let phone = Phone::new(&s.r, &s.ring.chain, &s.mobile, 1);
    if register {
        phone.register(&s.daemon, true, true);
    }
    let desk = Client::in_process(&s.srv, Role::Desktop);
    P {
        s,
        gw,
        phone,
        desk,
        reapers: Vec::new(),
    }
}

fn harness() -> P {
    harness_with(TestRelayOptions::default(), tweak, true)
}

impl P {
    fn ring_id(&self) -> RingId {
        self.s.ring.chain.ring_id().clone()
    }

    /// Opens `agent` in a Project of its own; its UUID and run.
    fn open(&mut self, name: &str, agent: &str) -> (Uuid, u64) {
        self.open_with(name, agent, |s| s)
    }

    /// [`P::open`], with the launch spec changed by `f`.
    fn open_with(
        &mut self,
        name: &str,
        agent: &str,
        f: impl FnOnce(LaunchSpec) -> LaunchSpec,
    ) -> (Uuid, u64) {
        let cwd = self.s.h.project(name);
        self.reapers.push(FakeReaper(cwd.join("pids.log")));
        let t = Uuid::new_v4();
        self.desk.open(
            t,
            f(LaunchSpec {
                agent: Some(agent.into()),
                shell_mode: Some("claude".into()),
                // A session of its own: a Host runs one session in one Terminal only.
                session_id: Some(Uuid::new_v4().to_string()),
                cwd: cwd.to_string_lossy().into_owned(),
                ..Default::default()
            }),
        );
        (t, wait_run(&cwd, t, None))
    }

    /// What a hook reports.
    fn event(&mut self, (t, run): (Uuid, u64), status: AgentStatus) {
        self.desk
            .request(&ClientMsg::TermEvent {
                terminal: t,
                run,
                status,
                session_id: None,
            })
            .expect("term.event");
    }

    fn requests(&self) -> Vec<Value> {
        self.gw.requests()
    }

    /// Waits for `n` gateway requests, then makes sure no more come.
    fn exactly(&self, n: usize) -> Vec<Value> {
        let got = self.gw.wait_requests(n, T);
        assert!(got.len() >= n, "expected {n} pushes, got {}", got.len());
        std::thread::sleep(QUIET);
        let got = self.requests();
        assert_eq!(got.len(), n, "{:#?}", self.opened(&self.phone));
        got
    }

    /// Every push the gateway got for `phone`, opened with its seal secret.
    fn opened(&self, phone: &Phone) -> Vec<PushPayload> {
        self.requests()
            .iter()
            .filter(|v| v["blob"] == phone.blob.as_str())
            .map(|v| open(phone, &self.ring_id(), v).1)
            .collect()
    }

    fn cwd(&self, name: &str) -> String {
        self.s.h.project(name).to_string_lossy().into_owned()
    }

    fn push_json(&self) -> Value {
        let p = self.s.h.paths().ring_dir.join("push.json");
        serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
    }
}

fn open(phone: &Phone, ring: &RingId, v: &Value) -> (NoiseKey, PushPayload) {
    push::open(&phone.seal, ring, v["sealedPayload"].as_str().unwrap()).expect("the push opens")
}

/// The run of Terminal `t` launched in `cwd`, once its fake logged it (other than `not`).
fn wait_run(cwd: &Path, t: Uuid, not: Option<u64>) -> u64 {
    let deadline = Instant::now() + T;
    loop {
        let log = fs::read_to_string(cwd.join("env.log")).unwrap_or_default();
        let run = log.lines().rev().find_map(|l| {
            let id = l.strip_prefix("XSHELL_TERMINAL_ID=")?;
            let (u, r) = id.split_once('.')?;
            (u == t.to_string())
                .then(|| r.parse::<u64>().ok())
                .flatten()
        });
        if let Some(r) = run.filter(|r| Some(*r) != not) {
            return r;
        }
        assert!(Instant::now() < deadline, "no run logged for {t}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn foreground_at_relay(p: &P, on: bool) {
    let id = p.ring_id();
    let k = p.s.mobile.sign_key();
    wait_until("the relay's foreground flag", T, || {
        p.s.r.presence(&id, &k).is_some_and(|x| x.foreground == on)
    });
    // The presence broadcast reaches the Daemon.
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn status_change_sends_one_push_only_the_mobile_opens() {
    let mut p = harness();
    let t = p.open("app", "claude");
    p.event(t, Working);
    p.event(t, NeedsYou);
    let got = p.exactly(1);
    let v = &got[0];
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["blob", "collapseId", "sealedPayload"]);
    assert_eq!(v["blob"], p.phone.blob.as_str());
    assert_eq!(v["collapseId"], collapse_id(&p.s.daemon).as_str());
    let (sender, payload) = open(&p.phone, &p.ring_id(), v);
    let (_, daemon_noise, _) = identity(&p.s.srv);
    assert_eq!(sender, daemon_noise);
    assert_eq!(payload.host, p.s.daemon);
    assert_eq!(payload.terminal, t.0);
    assert_eq!(payload.status, PushStatus::NeedsYou);
    assert_eq!(payload.agent, PushAgent::Claude);
    assert_eq!(payload.project, p.cwd("app"));
    assert_eq!(payload.needs_you, 1);
    assert!(push::check_fresh(
        &payload,
        xshell_protocol::ring::relay::contract::now_ms(),
        None
    )
    .is_ok());
    // Not the phone's session key, not a stranger's.
    let sealed = v["sealedPayload"].as_str().unwrap();
    let (_, session_secret) = p.phone.keys.seeds();
    assert!(push::open(&session_secret, &p.ring_id(), sealed).is_err());
    assert!(push::open(&[0x55; 32], &p.ring_id(), sealed).is_err());
}

#[test]
fn no_push_while_a_mobile_is_in_foreground() {
    let mut p = harness();
    let t = p.open("app", "claude");
    p.phone.peer.connector.set_foreground(true);
    foreground_at_relay(&p, true);
    p.event(t, NeedsYou);
    std::thread::sleep(QUIET);
    assert!(p.requests().is_empty());
    // The Daemon held it back itself: the Relay saw no push.
    assert_eq!(p.s.r.push_frames(), 0);
    p.phone.peer.connector.set_foreground(false);
    foreground_at_relay(&p, false);
    p.event(t, Finished);
    let got = p.exactly(1);
    assert_eq!(
        open(&p.phone, &p.ring_id(), &got[0]).1.status,
        PushStatus::Finished
    );
}

#[test]
fn relay_suppresses_when_daemon_view_is_stale() {
    let mut p = harness();
    let t = p.open("app", "claude");
    // The Daemon hears no presence from now on: it does not know the phone is in front.
    assert!(p.s.r.fault(&p.ring_id(), &p.s.daemon, Fault::Mute));
    p.phone.peer.connector.set_foreground(true);
    foreground_at_relay(&p, true);
    p.event(t, NeedsYou);
    wait_until("the push reached the relay", T, || p.s.r.push_frames() == 1);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        p.requests().is_empty(),
        "the relay forwarded while foreground"
    );
}

#[test]
fn foreground_lease_runs_out_on_a_silent_phone() {
    let mut p = harness_with(
        TestRelayOptions {
            foreground_lease: Duration::from_millis(1500),
            ..TestRelayOptions::default()
        },
        tweak,
        true,
    );
    let t = p.open("app", "claude");
    // The phone's app goes quiet with its socket open: no pings, no state(false).
    p.phone
        .peer
        .connector
        .stop(xshell_protocol::ring::relay::ByeReason::quit());
    let mut raw = RawConn::login(&p.s.r.target(), &p.s.ring.chain, &*p.s.mobile);
    raw.send(&ClientFrame::State { foreground: true }.encode())
        .unwrap();
    foreground_at_relay(&p, true);
    p.event(t, NeedsYou);
    std::thread::sleep(Duration::from_millis(300));
    assert!(p.requests().is_empty());
    // The lease runs out; pushes resume while the socket is still open.
    let id = p.ring_id();
    let k = p.s.mobile.sign_key();
    wait_until("the lease ran out", Duration::from_secs(5), || {
        p.s.r
            .presence(&id, &k)
            .is_some_and(|x| x.online && !x.foreground)
    });
    std::thread::sleep(Duration::from_millis(300));
    p.event(t, Finished);
    p.exactly(1);
    drop(raw);
}

#[test]
fn burst_on_one_host_collapses() {
    let mut p = harness();
    let (a, b) = (p.open("a", "claude"), p.open("b", "claude"));
    p.event(a, NeedsYou);
    // The leading push is out (the Daemon checks each change is still current when its turn
    // comes, so without this wait a slow machine could skip a change gone stale).
    p.gw.wait_requests(1, T);
    p.event(b, NeedsYou);
    p.event(a, Finished);
    p.event(b, Finished);
    p.event(a, NeedsYou);
    let got = p.exactly(2);
    assert_eq!(got[0]["collapseId"], got[1]["collapseId"]);
    let opened = p.opened(&p.phone);
    assert_eq!(
        (opened[0].terminal, opened[0].status),
        (a.0, PushStatus::NeedsYou)
    );
    // The trailing one: the newest event that is still current.
    assert_eq!(
        (opened[1].terminal, opened[1].status),
        (a.0, PushStatus::NeedsYou)
    );
    assert_eq!(opened[1].needs_you, 1);
    assert!(opened[1].seq > opened[0].seq);
}

#[test]
fn trailing_dropped_when_no_longer_current() {
    let mut p = harness();
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    p.gw.wait_requests(1, T);
    // A second eligible change inside the window waits; the Terminal then goes working.
    p.event(t, Finished);
    p.event(t, Working);
    let got = p.exactly(1);
    assert_eq!(
        open(&p.phone, &p.ring_id(), &got[0]).1.status,
        PushStatus::NeedsYou
    );
}

#[test]
fn trailing_takes_the_newest_current_event() {
    let mut p = harness();
    let (a, b) = (p.open("a", "claude"), p.open("b", "claude"));
    p.event(a, Finished);
    p.gw.wait_requests(1, T);
    p.event(a, NeedsYou);
    p.event(b, NeedsYou);
    // b's change is no longer current; a's still is.
    p.event(b, Working);
    p.exactly(2);
    let o = p.opened(&p.phone);
    assert_eq!((o[1].terminal, o[1].status), (a.0, PushStatus::NeedsYou));
}

#[test]
fn trigger_off_stops_that_kind() {
    let mut p = harness();
    p.phone.register(&p.s.daemon, false, true);
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    std::thread::sleep(QUIET);
    assert!(p.requests().is_empty());
    p.event(t, Finished);
    p.exactly(1);
    // Re-enabled: that kind pushes again.
    p.phone.register(&p.s.daemon, true, true);
    p.event(t, NeedsYou);
    p.exactly(2);
    assert_eq!(p.opened(&p.phone)[1].status, PushStatus::NeedsYou);
}

#[test]
fn interrupt_input_does_not_push() {
    let mut p = harness();
    let t = p.open("app", "claude");
    p.event(t, Working);
    // Esc ends the turn: finished, from input, not from a hook.
    p.desk.input(t.0, "\x1b");
    let list = p.desk.terminals_where(|l| {
        l.iter()
            .any(|i| i.terminal == t.0 && i.agent_status == Some(Finished))
    });
    assert!(!list.is_empty());
    std::thread::sleep(QUIET);
    assert!(p.requests().is_empty());
}

#[test]
fn wrapped_agent_does_not_push() {
    let mut p = harness();
    // A Claude in a wrapping shell reports through its hooks, but the Mobile never lists it.
    let w = p.open_with("wrapped", "claude", |s| LaunchSpec {
        shell_command: Some("/bin/sh".into()),
        shell_id: Some("bash".into()),
        ..s
    });
    p.event(w, NeedsYou);
    let a = p.open("app", "claude");
    p.event(a, NeedsYou);
    // One push, about the direct agent, counting only it.
    let got = p.exactly(1);
    let (_, payload) = open(&p.phone, &p.ring_id(), &got[0]);
    assert_eq!(payload.terminal, a.0);
    assert_eq!(payload.project, p.cwd("app"));
    assert_eq!(payload.needs_you, 1);
    for o in p.opened(&p.phone) {
        assert_ne!(o.terminal, w.0);
        assert_ne!(o.project, p.cwd("wrapped"));
    }
}

#[test]
fn codex_osc_needs_you_pushes() {
    let mut p = harness();
    let t = p.open("app", "codex");
    // Enter starts a turn (input), the agent's OSC 9 then says it needs you (output).
    p.desk.input(t.0, "go\r");
    let got = p.exactly(1);
    let (_, payload) = open(&p.phone, &p.ring_id(), &got[0]);
    assert_eq!(payload.status, PushStatus::NeedsYou);
    assert_eq!(payload.agent, PushAgent::Codex);
}

#[test]
fn push_register_mobile_only_and_validated() {
    let mut p = harness_with(TestRelayOptions::default(), tweak, false);
    // The Daemon offers `push`.
    let stream = p.phone.peer.sessions.open(&p.s.daemon).unwrap();
    let mut c = Client::from_io(stream.try_clone(), stream);
    let (hello, _) = c.hello(range(1, 1));
    assert!(hello.capabilities.iter().any(|c| c == "push"));
    c.shutdown();
    let good = |seal_key: String| ClientMsg::PushRegister {
        blob: "xpb1.test.ok".into(),
        seal_key,
        triggers: PushTriggers {
            needs_you: true,
            finished: true,
        },
    };
    let sk = p.phone.seal_key().to_b64();
    // A local Desktop and a Desktop's session: refused.
    let e = p.desk.request(&good(sk.clone())).unwrap_err();
    assert_eq!(e, "push.register is for a Mobile");
    let desk = Peer::new(&p.s.r, &p.s.ring.chain, &p.s.desk2);
    let mut dc = desk.client(&p.s.daemon);
    assert_eq!(
        dc.request(&good(sk.clone())).unwrap_err(),
        "push.register is for a Mobile"
    );
    assert!(dc.request(&ClientMsg::PushUnregister).is_err());
    // A Mobile: validated.
    let bad_blob = ClientMsg::PushRegister {
        blob: "nope".into(),
        seal_key: sk.clone(),
        triggers: PushTriggers {
            needs_you: true,
            finished: true,
        },
    };
    let d = p.s.daemon;
    assert!(p.phone.register_raw(&d, &bad_blob).is_err());
    let own = p.s.mobile.noise_key().to_b64();
    let e = p.phone.register_raw(&d, &good(own)).unwrap_err();
    assert!(e.contains("session key"), "{e}");
    assert!(p.phone.register_raw(&d, &good("AAAA".into())).is_err());
    assert!(p.phone.register_raw(&d, &good(sk)).is_ok());
    assert_eq!(
        p.push_json()["mobiles"][p.s.mobile.sign_key().to_b64()]["blob"],
        "xpb1.test.ok"
    );
    // Unregistered: no more pushes.
    p.phone
        .register_raw(&d, &ClientMsg::PushUnregister)
        .unwrap();
    assert!(p.push_json()["mobiles"].as_object().unwrap().is_empty());
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    std::thread::sleep(QUIET);
    assert!(p.requests().is_empty());
}

#[test]
fn registration_survives_restart_mode_0600() {
    let mut p = harness();
    let path = p.s.h.paths().ring_dir.join("push.json");
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    p.exactly(1);
    let first = p.opened(&p.phone)[0].seq;
    // Restart: the registration and the sequence carry over.
    let P {
        s,
        gw,
        phone,
        desk,
        reapers: _reapers,
    } = p;
    desk.shutdown();
    let Setup {
        r,
        h,
        srv,
        ring,
        daemon,
        ..
    } = s;
    srv.shutdown();
    let srv = start(&h, tweak);
    let id = ring.chain.ring_id().clone();
    wait_until("the daemon is back", T, || online(&r, &id, &daemon));
    let mut desk = Client::in_process(&srv, Role::Desktop);
    let cwd = h.project("app2");
    let _reaper = FakeReaper(cwd.join("pids.log"));
    let t = Uuid::new_v4();
    desk.open(t, claude_spec(&cwd, Some(&Uuid::new_v4().to_string())));
    let run = wait_run(&cwd, t, None);
    desk.request(&ClientMsg::TermEvent {
        terminal: t,
        run,
        status: NeedsYou,
        session_id: None,
    })
    .unwrap();
    let got = gw.wait_requests(2, T);
    assert_eq!(got.len(), 2);
    let (_, second) = open(&phone, &id, &got[1]);
    assert!(second.seq > first, "{} <= {first}", second.seq);
}

#[test]
fn removed_mobile_registration_is_cleared() {
    let mut p = harness();
    let mk = p.s.mobile.sign_key();
    assert!(p.push_json()["mobiles"].get(mk.to_b64()).is_some());
    p.s.ring.next(|d| {
        d.remove(&mk);
    });
    join(&p.s.srv, &p.s.ring.chain);
    wait_until("the registration is gone", T, || {
        p.push_json()["mobiles"].get(mk.to_b64()).is_none()
    });
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    std::thread::sleep(QUIET);
    assert!(p.requests().is_empty());
}

#[test]
fn device_gone_makes_registration_dormant_until_reregistered() {
    let mut p = harness();
    p.gw.respond(Reply::refusal(410, "device_gone"));
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    p.exactly(1);
    let mk = p.s.mobile.sign_key().to_b64();
    assert_eq!(p.push_json()["mobiles"][&mk]["dormant"], "device_gone");
    p.event(t, Finished);
    std::thread::sleep(QUIET);
    assert_eq!(p.requests().len(), 1);
    p.phone.register(&p.s.daemon, true, true);
    assert_eq!(p.push_json()["mobiles"][&mk]["dormant"], Value::Null);
    p.event(t, NeedsYou);
    p.exactly(2);
}

#[test]
fn reregister_fences_an_old_answer() {
    let mut p = harness();
    p.gw.hold(true);
    p.gw.respond(Reply::refusal(410, "device_gone"));
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    p.gw.wait_requests(1, T);
    // A new registration while the old attempt is still out; then the old answer lands.
    p.phone.register(&p.s.daemon, true, true);
    p.gw.hold(false);
    std::thread::sleep(Duration::from_millis(500));
    let mk = p.s.mobile.sign_key().to_b64();
    assert_eq!(p.push_json()["mobiles"][&mk]["dormant"], Value::Null);
    p.event(t, Finished);
    p.exactly(2);
}

#[test]
fn quota_exceeded_pauses_pushes() {
    let mut p = harness();
    p.gw.respond(Reply::refusal(429, "quota_exceeded"));
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    p.exactly(1);
    p.event(t, Finished);
    std::thread::sleep(QUIET);
    assert_eq!(p.requests().len(), 1);
    // Registering again lifts the pause.
    p.phone.register(&p.s.daemon, true, true);
    p.event(t, NeedsYou);
    p.exactly(2);
}

#[test]
fn reconcile_pending_is_retried_once() {
    let mut p = harness();
    p.gw.respond(Reply::refusal(503, "reconcile_pending"));
    p.gw.respond(Reply::refusal(503, "reconcile_pending"));
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    let got = p.exactly(2);
    // The retry carries the same push.
    assert_eq!(got[0]["blob"], got[1]["blob"]);
}

/// A `reconcile_pending` whose retry is due after `during` changed things: no retry.
fn retry_dropped_when(during: impl FnOnce(&mut P, (Uuid, u64))) {
    let mut p = harness_with(
        TestRelayOptions::default(),
        |c| {
            tweak(c);
            c.push_retry = Duration::from_secs(2);
        },
        true,
    );
    p.gw.respond(Reply::refusal(503, "reconcile_pending"));
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    p.gw.wait_requests(1, T);
    std::thread::sleep(Duration::from_millis(200));
    during(&mut p, t);
    std::thread::sleep(Duration::from_millis(2500));
    let got = p.requests();
    let retried = got
        .iter()
        .filter(|v| v["blob"] == p.phone.blob.as_str())
        .filter_map(|v| {
            let (_, x) = open(&p.phone, &p.ring_id(), v);
            (x.terminal == t.0 && x.status == PushStatus::NeedsYou).then_some(())
        })
        .count();
    assert_eq!(retried, 1, "the stale push was retried");
}

#[test]
fn retry_dropped_when_resolved() {
    retry_dropped_when(|p, t| p.event(t, Working));
}

#[test]
fn retry_dropped_after_relaunch() {
    retry_dropped_when(|p, t| {
        p.desk
            .request(&ClientMsg::TermRelaunch {
                terminal: t.0,
                skip_permissions: true,
            })
            .expect("relaunch");
        wait_run(Path::new(&p.cwd("app")), t.0, Some(t.1));
    });
}

#[test]
fn retry_dropped_after_unregister() {
    retry_dropped_when(|p, _| {
        p.phone
            .register_raw(&p.s.daemon, &ClientMsg::PushUnregister)
            .unwrap();
    });
}

#[test]
fn retry_dropped_after_trigger_off() {
    retry_dropped_when(|p, _| p.phone.register(&p.s.daemon, false, true));
}

#[test]
fn relay_without_push_cap_gets_no_push_frames() {
    push_agents();
    let s = setup_on(relay(), tweak);
    let phone = Phone::new(&s.r, &s.ring.chain, &s.mobile, 1);
    phone.register(&s.daemon, true, true);
    let mut desk = Client::in_process(&s.srv, Role::Desktop);
    let cwd = s.h.project("app");
    let _reaper = FakeReaper(cwd.join("pids.log"));
    let t = Uuid::new_v4();
    desk.open(t, claude_spec(&cwd, Some(&Uuid::new_v4().to_string())));
    let run = wait_run(&cwd, t, None);
    desk.request(&ClientMsg::TermEvent {
        terminal: t,
        run,
        status: NeedsYou,
        session_id: None,
    })
    .unwrap();
    std::thread::sleep(QUIET);
    assert_eq!(s.r.push_frames(), 0);
}

#[test]
fn each_mobile_gets_its_own_push() {
    let mut p = harness();
    let other = contract::keys();
    let v = p.s.ring.add(&other, RingRole::Mobile);
    join(&p.s.srv, &p.s.ring.chain);
    let _ = v;
    let phone2 = Phone::new(&p.s.r, &p.s.ring.chain, &other, 2);
    phone2.register(&p.s.daemon, true, true);
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    let got = p.exactly(2);
    let mut blobs: Vec<&str> = got.iter().map(|v| v["blob"].as_str().unwrap()).collect();
    blobs.sort();
    assert_eq!(blobs, ["xpb1.test.phone1", "xpb1.test.phone2"]);
    for v in &got {
        let mine = if v["blob"] == p.phone.blob.as_str() {
            (&p.phone, &phone2)
        } else {
            (&phone2, &p.phone)
        };
        let sealed = v["sealedPayload"].as_str().unwrap();
        assert!(push::open(&mine.0.seal, &p.ring_id(), sealed).is_ok());
        assert!(push::open(&mine.1.seal, &p.ring_id(), sealed).is_err());
    }
}

#[test]
fn slow_gateway_keeps_the_pipeline_current() {
    let mut p = harness();
    let other = contract::keys();
    p.s.ring.add(&other, RingRole::Mobile);
    join(&p.s.srv, &p.s.ring.chain);
    let phone2 = Phone::new(&p.s.r, &p.s.ring.chain, &other, 2);
    phone2.register(&p.s.daemon, true, true);
    // Every answer takes longer than the window.
    p.gw.set_delay(W * 2);
    let (a, b) = (p.open("a", "claude"), p.open("b", "claude"));
    p.event(a, NeedsYou);
    p.gw.wait_requests(2, T);
    // While both sends are out, more changes come in and are still taken.
    p.event(b, NeedsYou);
    p.event(a, Finished);
    p.event(b, Finished);
    p.event(b, NeedsYou);
    let got = p.gw.wait_requests(4, Duration::from_secs(8));
    assert_eq!(got.len(), 4, "{got:#?}");
    std::thread::sleep(W * 3);
    assert_eq!(p.requests().len(), 4);
    for phone in [&p.phone, &phone2] {
        let o = p.opened(phone);
        assert_eq!(o.len(), 2);
        assert_eq!((o[0].terminal, o[0].status), (a.0, PushStatus::NeedsYou));
        // Collapsed into the newest current change.
        assert_eq!((o[1].terminal, o[1].status), (b.0, PushStatus::NeedsYou));
        assert_eq!(o[1].needs_you, 1);
    }
}

// ---- Ordering: the submission boundary, attempt ownership, persistence, shutdown -------------

/// Holds the push thread that reaches `point` once armed, until released.
#[derive(Default)]
struct Gate {
    armed: std::sync::atomic::AtomicBool,
    hit: std::sync::Mutex<bool>,
    open: std::sync::Mutex<bool>,
    hit_cv: std::sync::Condvar,
    open_cv: std::sync::Condvar,
}

impl Gate {
    fn hooks(self: &Arc<Self>, point: PushPoint) -> PushHooks {
        let g = self.clone();
        PushHooks {
            at: Some(Arc::new(move |p| {
                if p != point || !g.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                *g.hit.lock().unwrap() = true;
                g.hit_cv.notify_all();
                let mut open = g.open.lock().unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                while !*open && Instant::now() < deadline {
                    open = g
                        .open_cv
                        .wait_timeout(open, Duration::from_millis(50))
                        .unwrap()
                        .0;
                }
            })),
            ..PushHooks::default()
        }
    }

    fn arm(&self) {
        *self.hit.lock().unwrap() = false;
        *self.open.lock().unwrap() = false;
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn wait_hit(&self) {
        let deadline = Instant::now() + T;
        let mut hit = self.hit.lock().unwrap();
        while !*hit {
            assert!(Instant::now() < deadline, "the push never reached the gate");
            hit = self
                .hit_cv
                .wait_timeout(hit, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.open_cv.notify_all();
    }
}

fn gated(point: PushPoint) -> (P, Arc<Gate>) {
    let gate = Arc::new(Gate::default());
    let hooks = gate.hooks(point);
    let p = harness_with(
        TestRelayOptions::default(),
        move |c| {
            tweak(c);
            c.push_hooks = hooks;
        },
        true,
    );
    (p, gate)
}

/// A push held between its reservation (persisted, sealed) and its submission while
/// `change` alters one guard: it never reaches the Relay.
fn dropped_at_submission(change: impl FnOnce(&mut P, (Uuid, u64))) {
    let (mut p, gate) = gated(PushPoint::Submit);
    let t = p.open("app", "claude");
    gate.arm();
    p.event(t, NeedsYou);
    gate.wait_hit();
    change(&mut p, t);
    gate.release();
    std::thread::sleep(QUIET);
    assert_eq!(p.s.r.push_frames(), 0, "submitted after a guard changed");
    assert!(p.requests().is_empty());
}

#[test]
fn submission_checks_the_status_again() {
    dropped_at_submission(|p, t| p.event(t, Working));
}

#[test]
fn submission_checks_the_generation_again() {
    dropped_at_submission(|p, _| p.phone.register(&p.s.daemon, true, true));
}

#[test]
fn submission_checks_the_head_again() {
    dropped_at_submission(|p, _| {
        let other = contract::keys();
        p.s.ring.add(&other, RingRole::Desktop);
        join(&p.s.srv, &p.s.ring.chain);
    });
}

#[test]
fn submission_checks_foreground_again() {
    dropped_at_submission(|p, _| {
        p.phone.peer.connector.set_foreground(true);
        foreground_at_relay(p, true);
    });
}

#[test]
fn submission_checks_shutdown_again() {
    let (p, gate) = gated(PushPoint::Submit);
    let mut p = p;
    let t = p.open("app", "claude");
    gate.arm();
    p.event(t, NeedsYou);
    gate.wait_hit();
    let P { s, gw, desk, .. } = p;
    desk.shutdown();
    let Setup { r, srv, .. } = s;
    let stopping = std::thread::spawn(move || srv.shutdown());
    std::thread::sleep(Duration::from_millis(500));
    gate.release();
    stopping.join().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(r.push_frames(), 0);
    assert!(gw.requests().is_empty());
}

#[test]
fn only_the_owning_attempt_frees_the_slot() {
    let mut p = harness();
    let (a, b, c) = (
        p.open("a", "claude"),
        p.open("b", "claude"),
        p.open("c", "claude"),
    );
    let slow = Duration::from_millis(2000);
    p.gw.respond_after(Reply::delivered(), slow);
    p.gw.respond_after(Reply::delivered(), slow);
    let t0 = Instant::now();
    p.event(a, NeedsYou);
    p.gw.wait_requests(1, T);
    // A is out. The registration is replaced; B must still wait for A.
    p.phone
        .register_raw(&p.s.daemon, &ClientMsg::PushUnregister)
        .unwrap();
    p.phone.register(&p.s.daemon, true, true);
    p.event(b, NeedsYou);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        p.requests().len(),
        1,
        "a second push while the first was out"
    );
    // A's answer frees the slot for B, and B's attempt then owns it: C waits for B.
    let got = p.gw.wait_requests(2, Duration::from_secs(5));
    assert_eq!(got.len(), 2);
    assert!(t0.elapsed() >= slow);
    let b_out = Instant::now();
    p.event(c, NeedsYou);
    std::thread::sleep(Duration::from_millis(800));
    if b_out.elapsed() < slow {
        assert_eq!(
            p.requests().len(),
            2,
            "a third push while the second was out"
        );
    }
    p.gw.wait_requests(3, Duration::from_secs(6));
    let o = p.opened(&p.phone);
    assert_eq!(o.len(), 3);
    assert_eq!(
        o.iter().map(|x| x.terminal).collect::<Vec<_>>(),
        vec![a.0, b.0, c.0]
    );
}

fn clock_far_behind() -> u64 {
    1_000
}

#[test]
fn a_seq_that_cannot_be_saved_is_never_sent() {
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f2 = fail.clone();
    let mut p = harness_with(
        TestRelayOptions::default(),
        move |c| {
            tweak(c);
            c.push_hooks.fail_saves = Some(f2);
        },
        true,
    );
    let t = p.open("app", "claude");
    p.event(t, NeedsYou);
    p.exactly(1);
    let first = p.opened(&p.phone)[0].seq;
    // Writes fail: the push is dropped, and not retried once they work again.
    fail.store(true, std::sync::atomic::Ordering::SeqCst);
    p.event(t, Finished);
    std::thread::sleep(QUIET);
    assert_eq!(p.requests().len(), 1);
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(QUIET);
    assert_eq!(p.requests().len(), 1);
    // A restart with a clock far below the last seq sent: the next seq is still above it.
    let P {
        s, gw, phone, desk, ..
    } = p;
    desk.shutdown();
    let Setup {
        r,
        h,
        srv,
        ring,
        daemon,
        ..
    } = s;
    srv.shutdown();
    let srv = start(&h, |c| {
        tweak(c);
        c.push_hooks.clock = Some(clock_far_behind);
    });
    let id = ring.chain.ring_id().clone();
    wait_until("the daemon is back", T, || online(&r, &id, &daemon));
    let mut desk = Client::in_process(&srv, Role::Desktop);
    let cwd = h.project("app2");
    let _reaper = FakeReaper(cwd.join("pids.log"));
    let t = Uuid::new_v4();
    desk.open(t, claude_spec(&cwd, Some(&Uuid::new_v4().to_string())));
    let run = wait_run(&cwd, t, None);
    desk.request(&ClientMsg::TermEvent {
        terminal: t,
        run,
        status: NeedsYou,
        session_id: None,
    })
    .unwrap();
    let got = gw.wait_requests(2, T);
    assert_eq!(got.len(), 2);
    let (_, second) = open(&phone, &id, &got[1]);
    assert!(second.seq > first, "{} <= {first}", second.seq);
}

#[test]
fn shutdown_waits_for_a_held_save() {
    let (mut p, gate) = gated(PushPoint::Save);
    let t = p.open("app", "claude");
    gate.arm();
    p.event(t, NeedsYou);
    gate.wait_hit();
    let P {
        s, gw, phone, desk, ..
    } = p;
    desk.shutdown();
    let Setup {
        r,
        h,
        srv,
        ring,
        daemon,
        ..
    } = s;
    let stopping = std::thread::spawn(move || srv.shutdown());
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !stopping.is_finished(),
        "the lock was released under a held write"
    );
    gate.release();
    stopping.join().unwrap();
    // The successor starts on an intact store, and pushes.
    let srv = start(&h, tweak);
    let id = ring.chain.ring_id().clone();
    wait_until("the daemon is back", T, || online(&r, &id, &daemon));
    let file: Value =
        serde_json::from_slice(&fs::read(h.paths().ring_dir.join("push.json")).unwrap()).unwrap();
    assert!(file["mobiles"]
        .get(phone.keys.sign_key().to_b64())
        .is_some());
    let mut desk = Client::in_process(&srv, Role::Desktop);
    let cwd = h.project("app2");
    let _reaper = FakeReaper(cwd.join("pids.log"));
    let t = Uuid::new_v4();
    desk.open(t, claude_spec(&cwd, Some(&Uuid::new_v4().to_string())));
    let run = wait_run(&cwd, t, None);
    desk.request(&ClientMsg::TermEvent {
        terminal: t,
        run,
        status: NeedsYou,
        session_id: None,
    })
    .unwrap();
    assert_eq!(gw.wait_requests(1, T).len(), 1);
}

#[test]
fn daemon_lease_runs_out_without_presence_updates() {
    let mut p = harness_with(
        TestRelayOptions {
            foreground_lease: Duration::from_millis(2000),
            ..TestRelayOptions::default()
        },
        tweak,
        true,
    );
    let t = p.open("app", "claude");
    p.phone
        .peer
        .connector
        .stop(xshell_protocol::ring::relay::ByeReason::quit());
    let mut raw = RawConn::login(&p.s.r.target(), &p.s.ring.chain, &*p.s.mobile);
    raw.send(&ClientFrame::State { foreground: true }.encode())
        .unwrap();
    let since = Instant::now();
    // The Daemon has the foreground record; from now on it hears no presence at all.
    foreground_at_relay(&p, true);
    assert!(p.s.r.fault(&p.ring_id(), &p.s.daemon, Fault::Mute));
    p.event(t, NeedsYou);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        since.elapsed() < Duration::from_millis(1800),
        "too slow to test the lease"
    );
    // Held back by the Daemon itself, from its cached lease.
    assert_eq!(p.s.r.push_frames(), 0);
    // The cached lease runs out with no update: the next change goes out.
    while since.elapsed() < Duration::from_millis(2500) {
        std::thread::sleep(Duration::from_millis(50));
    }
    p.event(t, Finished);
    p.exactly(1);
    assert_eq!(p.s.r.push_frames(), 1);
    drop(raw);
}

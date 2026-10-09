//! The Desktop's host link (xshell-hostlink) against the real `xshelld`, through a local
//! `sh -c` transport and the daemon-command override, and directly over the Daemon's local
//! socket. A test that holds for both runs twice: `<name>` (command) and
//! `<name>_over_socket`.
#![cfg(unix)]

mod common;
mod desktop;

use common::{claude_spec, make_jsonl, raw_spec, shared_fake_agents, Fake, FakeReaper};
use desktop::*;
use serde_json::json;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_hostlink::status::{IncompatibleReason, Phase};
use xshell_hostlink::StatusKind;
use xshell_protocol::msg::ProtocolRange;

/// `#[test] $cmd` runs `$body(Via::Command)`, `#[test] $sock` runs `$body(Via::Socket)`.
macro_rules! on_both {
    ($cmd:ident, $sock:ident, $body:ident) => {
        #[test]
        fn $cmd() {
            $body(Via::Command)
        }
        #[test]
        fn $sock() {
            $body(Via::Socket)
        }
    };
}

on_both!(
    connects_and_calls_get_home_dir,
    connects_and_calls_get_home_dir_over_socket,
    connects_and_calls_get_home_dir_on
);
fn connects_and_calls_get_home_dir_on(via: Via) {
    let fx = Fx::via(via);
    let s = fx.a.wait_usable();
    assert_eq!(s.status, StatusKind::Connected);
    assert_eq!(s.daemon_version.as_deref(), Some(VERSION));
    assert_eq!(s.protocol, Some(1));
    let home = call(&fx.a.host(), "get_home_dir", json!({})).unwrap();
    assert_eq!(home, json!(fx.home.home().to_string_lossy()));
    // A Daemon error comes back as a `remote` error with its text.
    let e = call(&fx.a.host(), "no_such_method", json!({})).unwrap_err();
    assert_eq!(e.code, xshell_hostlink::HostErrorCode::Remote);
}

on_both!(
    open_attach_input_roundtrip,
    open_attach_input_roundtrip_over_socket,
    open_attach_input_roundtrip_on
);
fn open_attach_input_roundtrip_on(via: Via) {
    let fx = Fx::via(via);
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    let pid = open(&h, t, fx.project(), sink.clone());
    assert!(pid.is_some());
    marker(&h, &sink, t, "abc");
}

on_both!(
    terminals_follow_open_update_close,
    terminals_follow_open_update_close_over_socket,
    terminals_follow_open_update_close_on
);
fn terminals_follow_open_update_close_on(via: Via) {
    let fx = Fx::via(via);
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    open(&h, t, fx.project(), sink.clone());
    fx.a.rec.wait_list("with the new terminal", |l| {
        l.iter().any(|i| i.terminal == t)
    });
    let mut meta = serde_json::Map::new();
    meta.insert("title".into(), json!("x"));
    let r: Result<_, _> = {
        let (tx, rx) = std::sync::mpsc::channel();
        h.term_update(
            t,
            Some("s1".into()),
            Some(meta),
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx.recv_timeout(W).unwrap()
    };
    r.unwrap();
    fx.a.rec.wait_list("with the update", |l| {
        l.iter().any(|i| {
            i.terminal == t && i.spec.session_id.as_deref() == Some("s1") && i.meta["title"] == "x"
        })
    });
    let n = fx.a.rec.list_count();
    close(&h, t);
    fx.a.rec
        .wait_list_from(n, "without it", |l| l.iter().all(|i| i.terminal != t));
    let (_, bytes) = sink.wait_exit();
    assert_eq!(bytes, sink.len() as u64, "exit watermark = bytes delivered");
}

on_both!(
    reconnect_reattaches_with_replay,
    reconnect_reattaches_with_replay_over_socket,
    reconnect_reattaches_with_replay_on
);
fn reconnect_reattaches_with_replay_on(via: Via) {
    let fx = Fx::via(via);
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    open(&h, t, fx.project(), sink.clone());
    marker(&h, &sink, t, "before");
    let n = fx.a.rec.count();
    let from = sink.len();
    // As a dropped ssh would end.
    let cut = fx.sever();
    let (i, _) =
        fx.a.rec
            .wait_status_from(n, "reconnecting", |s| s.status == StatusKind::Reconnecting);
    let start = Instant::now();
    fx.a.rec.wait_status_from(i, "connected again", usable);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(fx.redialed_since(cut));
    // The replay (a reset, then the earlier output) arrived without a new attach from us.
    let after = sink.wait_from(from, "before");
    let reset = after.find("\x1bc").expect("replay starts with a reset");
    assert!(after[reset..].contains("before"), "{after:?}");
    marker(&h, &sink, t, "after");
}

on_both!(
    disconnect_ends_nothing,
    disconnect_ends_nothing_over_socket,
    disconnect_ends_nothing_on
);
fn disconnect_ends_nothing_on(via: Via) {
    let fx = Fx::via(via);
    fx.a.wait_usable();
    let t = Uuid::new_v4();
    let pid = open(&fx.a.host(), t, fx.project(), VecSink::new()).unwrap();
    fx.a.m.shutdown();
    let b = fx.another_desk(|_| {});
    let list = b.rec.wait_list("first list", |_| true);
    let info = list.iter().find(|i| i.terminal == t).expect("still listed");
    assert_eq!(info.pid, Some(pid));
    assert!(pid_alive(pid as i32));
}

on_both!(
    two_desktops_share_terminal,
    two_desktops_share_terminal_over_socket,
    two_desktops_share_terminal_on
);
fn two_desktops_share_terminal_on(via: Via) {
    let fx = Fx::via(via);
    fx.a.wait_usable();
    let b = fx.another_desk(|_| {});
    b.wait_usable();
    let t = Uuid::new_v4();
    let sa = VecSink::new();
    open(&fx.a.host(), t, fx.project(), sa.clone());
    b.rec
        .wait_list("A's terminal", |l| l.iter().any(|i| i.terminal == t));
    let sb = VecSink::new();
    assert_eq!(attach(&b.host(), t, sb.clone()), None);
    let from = sb.len();
    marker(&fx.a.host(), &sa, t, "shared");
    sb.wait_from(from, "shared");
}

on_both!(
    status_exposes_daemon_capabilities,
    status_exposes_daemon_capabilities_over_socket,
    status_exposes_daemon_capabilities_on
);
fn status_exposes_daemon_capabilities_on(via: Via) {
    let fx = Fx::via(via);
    let s = fx.a.wait_usable();
    assert!(
        s.daemon_capabilities.iter().any(|c| c == "term.relaunch"),
        "{:?}",
        s.daemon_capabilities
    );
}

on_both!(
    relaunch_keeps_sink_no_exit,
    relaunch_keeps_sink_no_exit_over_socket,
    relaunch_keeps_sink_no_exit_on
);
/// A Relaunch keeps the Tab's sink: no exit, a reset, then the new process's output.
fn relaunch_keeps_sink_no_exit_on(via: Via) {
    // Before the Daemon starts: it inherits the fake agents' PATH.
    shared_fake_agents();
    let fx = Fx::via(via);
    fx.a.wait_usable();
    let h = fx.a.host();
    let cwd = fx.home.project("app");
    let fake = Fake::in_dir(&cwd);
    let _reaper = FakeReaper(fake.pids_log.clone());
    let sid = "11111111-2222-3333-4444-555555555555";
    make_jsonl(&fx.home, &cwd, sid);
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    let old = open(&h, t, claude_spec(&cwd, Some(sid)), sink.clone()).unwrap();
    sink.wait_from(0, &format!("pid {old} "));
    // Only what arrives from the relaunch on: the open's replay starts with a reset too.
    let from = sink.len();
    let new = relaunch(&h, t, true).unwrap().expect("pid");
    let text = sink.wait_from(from, &format!("pid {new} "));
    let first = text.find(&format!("pid {new} ")).unwrap();
    let reset = text.find("\x1bc").expect("a reset from the relaunch");
    assert!(
        reset < first,
        "the reset must precede the new output: {text:?}"
    );
    fx.a.rec.wait_list("with the relaunched terminal", |l| {
        l.iter()
            .any(|i| i.terminal == t && i.pid == Some(new) && i.spec.skip_permissions == Some(true))
    });
    assert!(sink.exits.lock().unwrap().is_empty());
    assert_eq!(
        fake.wait_launches(2)[1],
        vec!["--dangerously-skip-permissions", "--resume", sid]
    );
}

on_both!(
    launch_prefix_wraps_agent_and_survives_relaunch,
    launch_prefix_wraps_agent_and_survives_relaunch_over_socket,
    launch_prefix_wraps_agent_and_survives_relaunch_on
);
/// A Host's launch prefix runs the agent under it, and a Relaunch (built by the Daemon from
/// the stored spec) keeps it. Changing the prefix keeps the connection.
fn launch_prefix_wraps_agent_and_survives_relaunch_on(via: Via) {
    // Before the Daemon starts: it inherits the fake agents' PATH.
    shared_fake_agents();
    let fx = Fx::via(via);
    let first = fx.a.wait_usable();
    assert!(first
        .daemon_capabilities
        .iter()
        .any(|c| c == "launch.prefix"));
    let cwd = fx.home.project("app");
    let fake = Fake::in_dir(&cwd);
    let _reaper = FakeReaper(fake.pids_log.clone());
    let log = cwd.join("prefix.log");
    let wrap = fx.home.script(
        "wrap",
        &format!(
            "printf '%s|' \"$@\" >> '{}'; echo >> '{}'; shift; exec \"$@\"",
            log.display(),
            log.display()
        ),
    );
    let mut cfg = host_config(Some(override_cmd()));
    cfg.launch_prefixes
        .insert("claude".into(), format!("'{}' --tag", wrap.display()));
    fx.a.m.configure(vec![cfg]).unwrap();

    let sid = "11111111-2222-3333-4444-555555555555";
    make_jsonl(&fx.home, &cwd, sid);
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    let old = open(&fx.a.host(), t, claude_spec(&cwd, Some(sid)), sink.clone()).unwrap();
    sink.wait_from(0, &format!("pid {old} "));
    let new = relaunch(&fx.a.host(), t, true).unwrap().expect("pid");
    sink.wait_from(0, &format!("pid {new} "));
    assert_eq!(
        fake.wait_launches(2),
        vec![
            vec!["--resume".to_string(), sid.into()],
            vec![
                "--dangerously-skip-permissions".into(),
                "--resume".into(),
                sid.into()
            ],
        ]
    );
    let lines = std::fs::read_to_string(&log).unwrap();
    // The Agent Status hooks follow the agent's own arguments.
    let hooks = format!("--settings|{}|", fx.home.paths().claude_hooks.display());
    assert_eq!(
        lines,
        format!(
            "--tag|claude|--resume|{sid}|{hooks}\n--tag|claude|--dangerously-skip-permissions|--resume|{sid}|{hooks}\n"
        )
    );
    // The prefix change was applied without a reconnect.
    assert_eq!(
        fx.a.host().status().config_generation,
        first.config_generation
    );
}

/// A Terminal that ignores SIGHUP delays the old Daemon's exit by its kill grace (2 s).
fn hup_proof(fx: &Fx) -> xshell_core::launch::LaunchSpec {
    let s = fx.home.script("hupproof", "trap '' HUP\nexec sleep 1000");
    raw_spec(&fx.home.project("p"), &s)
}

#[test]
fn upgrade_restores_same_uuid_new_pid() {
    // Command only: nothing starts a Daemon over a socket.
    let fx = Fx::new();
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let pid = open(&h, t, hup_proof(&fx), VecSink::new()).unwrap();
    let old_daemon = fx.daemon_pid().expect("daemon pid");
    let n = fx.a.rec.count();
    upgrade(&h).unwrap();
    // Until the old Daemon is gone, the Host stays in phase `upgrading`, never connected.
    assert!(common::wait_dead(old_daemon, Duration::from_secs(10)));
    let (c, s) =
        fx.a.rec
            .wait_status_from(n, "connected after the upgrade", |s| {
                usable(s) && s.phase.is_none()
            });
    assert_eq!(s.status, StatusKind::Connected);
    assert_eq!(s.phase, None);
    let during = fx.a.rec.statuses.lock().unwrap()[n..c].to_vec();
    assert!(!during.is_empty());
    assert!(
        during.iter().all(|s| s.phase == Some(Phase::Upgrading)),
        "{during:#?}"
    );
    assert!(during.iter().any(|s| s.status == StatusKind::Reconnecting));
    let new_daemon = fx.daemon_pid().expect("new daemon pid");
    assert_ne!(new_daemon, old_daemon);
    let list = fx.a.rec.wait_list("restored", |l| {
        l.iter()
            .any(|i| i.terminal == t && i.pid.is_some() && i.pid != Some(pid))
    });
    assert_eq!(list.len(), 1);
}

on_both!(
    incompatible_when_ranges_disjoint,
    incompatible_when_ranges_disjoint_over_socket,
    incompatible_when_ranges_disjoint_on
);
fn incompatible_when_ranges_disjoint_on(via: Via) {
    let fx = Fx::with_via(via, |c| c.ours = ProtocolRange { min: 2, max: 3 });
    let s =
        fx.a.rec
            .wait_status("incompatible", |s| s.status == StatusKind::Incompatible);
    assert_eq!(s.incompatible_reason, Some(IncompatibleReason::DaemonOlder));
    assert!(s.last_error.as_deref().unwrap().contains("1..1"), "{s:?}");
    assert_eq!(s.daemon_version.as_deref(), Some(VERSION));
    let e = call(&fx.a.host(), "get_home_dir", json!({})).unwrap_err();
    assert_eq!(e.code, xshell_hostlink::HostErrorCode::Incompatible);
}

#[test]
fn shutdown_leaves_no_children() {
    let fx = Fx::new();
    fx.a.wait_usable();
    let pid = fx.a.host().child_pid().unwrap() as i32;
    assert!(pid_alive(pid));
    let start = Instant::now();
    fx.a.m.shutdown();
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(wait_dead(pid), "transport {pid} survived");
}

/// `shutdown_leaves_no_children` over a socket: no child to kill, the socket is shut down.
#[test]
fn shutdown_closes_the_socket() {
    let fx = Fx::via(Via::Socket);
    fx.a.wait_usable();
    let conn = fx.mark();
    let start = Instant::now();
    fx.a.m.shutdown();
    assert!(start.elapsed() < Duration::from_secs(3));
    fx.assert_conn_gone(conn);
}

on_both!(
    config_replacement_reattaches,
    config_replacement_reattaches_over_socket,
    config_replacement_reattaches_on
);
fn config_replacement_reattaches_on(via: Via) {
    let fx = Fx::via(via);
    let first = fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    open(&h, t, fx.project(), sink.clone());
    marker(&h, &sink, t, "first");
    let n = fx.a.rec.count();
    let from = sink.len();
    // Same Daemon, different command text: the connection settings changed.
    fx.a.m
        .configure(vec![host_config(Some(format!("exec {}", override_cmd())))])
        .unwrap();
    let (_, s) =
        fx.a.rec
            .wait_status_from(n, "connected on the new config", usable);
    assert!(s.config_generation > first.config_generation);
    // The kept sink was re-attached by the handle: replay, then live input.
    let after = sink.wait_from(from, "first");
    assert!(after.contains("\x1bc"), "{after:?}");
    marker(&fx.a.host(), &sink, t, "second");
}

/// A sink that, once armed, blocks inside `data` until released: a stalled delivery.
#[derive(Default)]
struct Gate {
    armed: std::sync::atomic::AtomicBool,
    inside: std::sync::atomic::AtomicBool,
    open: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl xshell_hostlink::TermSink for Gate {
    fn data(&self, _: &[u8]) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        if self.armed.load(SeqCst) {
            self.inside.store(true, SeqCst);
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.cv.wait(open).unwrap();
            }
        }
        true
    }
    fn exit(&self, _: i32, _: u64) {}
}

impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

/// Opens the gate on drop, so a failing test never leaves the reader stuck.
struct Release(std::sync::Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Open a terminal whose sink then blocks the link's reader inside a delivery.
fn stall_delivery(fx: &Fx) -> (Uuid, std::sync::Arc<Gate>) {
    use std::sync::atomic::Ordering::SeqCst;
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let gate = std::sync::Arc::new(Gate::default());
    let (tx, rx) = std::sync::mpsc::channel();
    h.term_open(
        xshell_protocol::msg::OpenSpec {
            terminal: t,
            launch: fx.project(),
            cols: 80,
            rows: 24,
            meta: Default::default(),
        },
        gate.clone(),
        Box::new(move |r| {
            let _ = tx.send(r);
        }),
    );
    rx.recv_timeout(W).unwrap().unwrap();
    gate.armed.store(true, SeqCst);
    h.term_input(t, "echo stall\n".into()).unwrap();
    let deadline = Instant::now() + W;
    while !gate.inside.load(SeqCst) {
        assert!(Instant::now() < deadline, "the delivery never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    (t, gate)
}

on_both!(
    input_never_waits_on_a_blocked_delivery,
    input_never_waits_on_a_blocked_delivery_over_socket,
    input_never_waits_on_a_blocked_delivery_on
);
fn input_never_waits_on_a_blocked_delivery_on(via: Via) {
    let fx = Fx::via(via);
    let (t, gate) = stall_delivery(&fx);
    let _release = Release(gate.clone());
    let h = fx.a.host();
    for i in 0..20 {
        let start = Instant::now();
        h.term_input(t, format!("echo {i}\n")).unwrap();
        h.term_resize(t, 100, 30 + i).unwrap();
        let _ = h.snapshot();
        fx.a.m.kick_all();
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "{:?}",
            start.elapsed()
        );
    }
}

on_both!(
    shutdown_is_bounded_with_a_blocked_delivery,
    shutdown_is_bounded_with_a_blocked_delivery_over_socket,
    shutdown_is_bounded_with_a_blocked_delivery_on
);
fn shutdown_is_bounded_with_a_blocked_delivery_on(via: Via) {
    let fx = Fx::via(via);
    let (_t, gate) = stall_delivery(&fx);
    let release = Release(gate.clone());
    let conn = fx.mark();
    let start = Instant::now();
    fx.a.m.shutdown();
    assert!(
        start.elapsed() < Duration::from_millis(3500),
        "{:?}",
        start.elapsed()
    );
    fx.assert_conn_gone(conn);
    // Still stalled in the delivery: closing the socket alone does not end the reader.
    if let (Some(tap), Mark::Dial(n)) = (&fx.tap, conn) {
        assert!(!tap.reader_ended(n - 1, Duration::from_millis(100)));
    }
    // Released, the stalled reader sees the closed stream and ends.
    drop(release);
    fx.assert_reader_ended(conn);
}

on_both!(
    shutdown_is_bounded_with_a_concurrent_configure,
    shutdown_is_bounded_with_a_concurrent_configure_over_socket,
    shutdown_is_bounded_with_a_concurrent_configure_on
);
fn shutdown_is_bounded_with_a_concurrent_configure_on(via: Via) {
    let fx = Fx::via(via);
    let (_t, gate) = stall_delivery(&fx);
    let release = Release(gate.clone());
    let conn = fx.mark();
    let handle = fx.a.host();
    // The configure replaces the Host: it stops it, then waits on the blocked state.
    std::thread::scope(|s| {
        let cfg = s.spawn(|| {
            fx.a.m
                .configure(vec![host_config(Some(format!("exec {}", override_cmd())))])
        });
        std::thread::sleep(Duration::from_millis(200));
        let start = Instant::now();
        fx.a.m.shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(3500),
            "{:?}",
            start.elapsed()
        );
        fx.assert_conn_gone(conn);
        drop(release);
        fx.assert_reader_ended(conn);
        cfg.join().unwrap().unwrap();
    });
    // The configure that finished after the shutdown started nothing.
    std::thread::sleep(Duration::from_millis(300));
    assert!(fx.a.m.snapshot().is_empty());
    fx.assert_no_new_conn(conn, &handle);
}

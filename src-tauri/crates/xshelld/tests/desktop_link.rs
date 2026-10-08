//! The Desktop's host link (xshell-hostlink) against the real `xshelld`, through a local
//! `sh -c` transport and the daemon-command override.
#![cfg(unix)]

mod common;
mod desktop;

use common::raw_spec;
use desktop::*;
use serde_json::json;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::protocol::msg::ProtocolRange;
use xshell_hostlink::status::{IncompatibleReason, Phase};
use xshell_hostlink::StatusKind;

#[test]
fn connects_and_calls_get_home_dir() {
    let fx = Fx::new();
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

#[test]
fn open_attach_input_roundtrip() {
    let fx = Fx::new();
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    let pid = open(&h, t, fx.project(), sink.clone());
    assert!(pid.is_some());
    marker(&h, &sink, t, "abc");
}

#[test]
fn terminals_follow_open_update_close() {
    let fx = Fx::new();
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

#[test]
fn reconnect_reattaches_with_replay() {
    let fx = Fx::new();
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    open(&h, t, fx.project(), sink.clone());
    marker(&h, &sink, t, "before");
    let n = fx.a.rec.count();
    let from = sink.len();
    let ssh = h.child_pid().expect("transport pid") as i32;
    // Kill the transport's whole process group, as a dropped ssh would end.
    unsafe { libc::kill(-ssh, libc::SIGKILL) };
    let (i, _) =
        fx.a.rec
            .wait_status_from(n, "reconnecting", |s| s.status == StatusKind::Reconnecting);
    let start = Instant::now();
    fx.a.rec.wait_status_from(i, "connected again", usable);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_ne!(h.child_pid(), Some(ssh as u32));
    // The replay (a reset, then the earlier output) arrived without a new attach from us.
    let after = sink.wait_from(from, "before");
    let reset = after.find("\x1bc").expect("replay starts with a reset");
    assert!(after[reset..].contains("before"), "{after:?}");
    marker(&h, &sink, t, "after");
}

#[test]
fn disconnect_ends_nothing() {
    let fx = Fx::new();
    fx.a.wait_usable();
    let t = Uuid::new_v4();
    let pid = open(&fx.a.host(), t, fx.project(), VecSink::new()).unwrap();
    fx.a.m.shutdown();
    let b = Fx::desk(&fx.home, |_| {});
    let list = b.rec.wait_list("first list", |_| true);
    let info = list.iter().find(|i| i.terminal == t).expect("still listed");
    assert_eq!(info.pid, Some(pid));
    assert!(pid_alive(pid as i32));
}

#[test]
fn two_desktops_share_terminal() {
    let fx = Fx::new();
    fx.a.wait_usable();
    let b = Fx::desk(&fx.home, |_| {});
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

/// A Terminal that ignores SIGHUP delays the old Daemon's exit by its kill grace (2 s).
fn hup_proof(fx: &Fx) -> xshell_core::launch::LaunchSpec {
    let s = fx.home.script("hupproof", "trap '' HUP\nexec sleep 1000");
    raw_spec(&fx.home.project("p"), &s)
}

#[test]
fn upgrade_restores_same_uuid_new_pid() {
    let fx = Fx::new();
    fx.a.wait_usable();
    let h = fx.a.host();
    let t = Uuid::new_v4();
    let pid = open(&h, t, hup_proof(&fx), VecSink::new()).unwrap();
    let old_daemon = fx.guard.pid().expect("daemon pid");
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
    let new_daemon = fx.guard.pid().expect("new daemon pid");
    assert_ne!(new_daemon, old_daemon);
    let list = fx.a.rec.wait_list("restored", |l| {
        l.iter()
            .any(|i| i.terminal == t && i.pid.is_some() && i.pid != Some(pid))
    });
    assert_eq!(list.len(), 1);
}

#[test]
fn incompatible_when_ranges_disjoint() {
    let fx = Fx::with(|c| c.ours = ProtocolRange { min: 2, max: 3 });
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

#[test]
fn config_replacement_reattaches() {
    let fx = Fx::new();
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

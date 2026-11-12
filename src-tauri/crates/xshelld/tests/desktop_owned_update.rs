//! Ownership policy at the real Server/HostLink boundary, with a live dummy Terminal.
#![cfg(unix)]
mod common;
mod desktop;

use common::{Client, TestHome};
use desktop::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use xshell_hostlink::{
    BinarySource, CancelToken, Fetched, HostConfig, HostErrorCode, StatusKind, Transport,
    TransportFactory, Wanted,
};
use xshell_protocol::{build::BuildId, msg::ProtocolRange};

struct NoBinary(Arc<AtomicUsize>);
impl BinarySource for NoBinary {
    fn fetch(&self, _: &Wanted, _: &CancelToken) -> Result<Fetched, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("test forbids replacement bytes".into())
    }
}
struct RecordingFactory(Arc<dyn TransportFactory>, Arc<Mutex<Vec<String>>>);
impl TransportFactory for RecordingFactory {
    fn for_host(&self, cfg: &HostConfig) -> Box<dyn Transport> {
        struct Recording(Box<dyn Transport>, Arc<Mutex<Vec<String>>>);
        impl Transport for Recording {
            fn command(&self, command: &str) -> xshell_hostlink::transport::CommandSpec {
                self.1.lock().unwrap().push(command.into());
                self.0.command(command)
            }
            fn describe(&self) -> String {
                "owned-update fixture".into()
            }
        }
        Box::new(Recording(self.0.for_host(cfg), self.1.clone()))
    }
}

#[test]
fn server_advertises_only_actual_desktop_ownership() {
    for owned in [false, true] {
        let home = TestHome::new();
        let _server = common::start(&home, |cfg| {
            cfg.gui_bound = owned.then_some(xshelld::server::GuiBound {
                parent_pid: std::process::id(),
            });
        });
        let mut client = Client::connect(&home.paths().socket);
        let (hello, _) = client.hello(common::range(1, 1));
        assert_eq!(
            hello
                .capabilities
                .iter()
                .any(|c| c == "daemon.desktop-owned"),
            owned
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Desktop,
    LegacyDesktop,
    Persistent,
}

fn check_update_policy(incompatible: bool, ownership: Ownership, check_diagnosis: bool) {
    let blocked = ownership == Ownership::Desktop;
    let home = TestHome::new();
    let _server = common::start(&home, |cfg| {
        cfg.gui_bound = (ownership != Ownership::Persistent).then_some(xshelld::server::GuiBound {
            parent_pid: std::process::id(),
        });
        if ownership == Ownership::LegacyDesktop {
            cfg.hide_capabilities.push("daemon.desktop-owned".into());
        }
    });
    let mut client = Client::connect(&home.paths().socket);
    client.hello(common::range(1, 1));
    let terminal = uuid::Uuid::new_v4();
    client.open(terminal, common::sh_spec(&home.project("dummy")));
    let list =
        client.terminals_where(|l| l.iter().any(|t| t.terminal == terminal && t.pid.is_some()));
    let pid = list
        .iter()
        .find(|t| t.terminal == terminal)
        .unwrap()
        .pid
        .unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let fetches = Arc::new(AtomicUsize::new(0));
    let rec = Recorder::new();
    let mut cfg = manager_config(
        Arc::new(RecordingFactory(local_factory(&home), commands.clone())),
        Arc::new(NoBinary(fetches.clone())),
        rec.clone(),
    );
    // Explicitly eligible, differing metadata: no dependence on the worktree's build stamp.
    cfg.desktop_build = Some(BuildId {
        commit: "abcdef0123456789".into(),
        dirty: None,
        time: 42,
    });
    if incompatible {
        cfg.ours = ProtocolRange { min: 2, max: 2 };
    }
    let desk = Desk::new(cfg, rec, host_config(Some(override_cmd())));
    let status = desk.rec.wait_status("eligible target", |s| {
        s.status
            == if incompatible {
                StatusKind::Incompatible
            } else {
                StatusKind::BuildDiffers
            }
    });
    assert_eq!(status.daemon_version.as_deref(), Some(VERSION));
    assert_ne!(status.daemon_build, status.desktop_build);
    assert!(
        status.update_action.is_some(),
        "fixture must be build-eligible"
    );
    if check_diagnosis {
        assert_eq!(
            serde_json::to_value(&status).unwrap()["updateUnavailable"],
            if blocked {
                serde_json::json!("desktop-owned")
            } else {
                serde_json::Value::Null
            }
        );
    }
    let before = commands.lock().unwrap().clone();
    let handle = desk.host();
    for bound in [false, true] {
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = Box::new(move |r| {
            tx.send(r).unwrap();
        });
        if bound {
            handle.upgrade_target(
                Some(xshell_hostlink::handle::UpdateTarget {
                    config_generation: status.config_generation,
                    link_generation: status.link_generation,
                    daemon_version: status.daemon_version.clone(),
                    daemon_build: status.daemon_build.clone(),
                }),
                waiter,
            );
        } else {
            handle.upgrade(waiter);
        }
        let error = rx.recv_timeout(W).unwrap().unwrap_err();
        if !blocked {
            // Neither unknown legacy ownership nor a Persistent Server is guessed blocked.
            assert_ne!(error.code, HostErrorCode::Invalid);
            assert!(fetches.load(Ordering::SeqCst) > 0);
            break;
        }
        assert_eq!(error.code, HostErrorCode::Invalid);
        assert_eq!(
            error.message,
            "The Desktop on this Host manages its Daemon; update xshell there instead."
        );
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert_eq!(
            *commands.lock().unwrap(),
            before,
            "no prepare, upload, selection or stop command"
        );
        assert!(handle.status().phase.is_none());
    }
    let (_, list) = Client::connect(&home.paths().socket).hello(common::range(1, 1));
    assert!(list
        .iter()
        .any(|t| t.terminal == terminal && t.pid == Some(pid)));
    assert!(common::alive(pid as i32));
    desk.m.shutdown();
}

#[test]
fn usable_owned_hello_retains_diagnosis() {
    check_update_policy(false, Ownership::Desktop, true);
}
#[test]
fn incompatible_owned_hello_retains_diagnosis() {
    check_update_policy(true, Ownership::Desktop, true);
}
#[test]
fn usable_owned_backend_refuses_before_effects() {
    check_update_policy(false, Ownership::Desktop, false);
}
#[test]
fn incompatible_owned_backend_refuses_before_effects() {
    check_update_policy(true, Ownership::Desktop, false);
}
#[test]
fn legacy_unix_ownership_is_unknown() {
    check_update_policy(false, Ownership::LegacyDesktop, true);
}
#[test]
fn incompatible_legacy_unix_ownership_is_unknown() {
    check_update_policy(true, Ownership::LegacyDesktop, true);
}

#[test]
fn persistent_unix_replacement_remains_allowed() {
    check_update_policy(false, Ownership::Persistent, true);
}
#[test]
fn incompatible_persistent_unix_replacement_remains_allowed() {
    check_update_policy(true, Ownership::Persistent, true);
}

#[test]
fn authoritative_reconnect_and_config_generation_replace_ownership_evidence() {
    let home = TestHome::new();
    let server = common::start(&home, |cfg| {
        cfg.gui_bound = Some(xshelld::server::GuiBound {
            parent_pid: std::process::id(),
        });
    });
    let desk = desktop::socket_desk(&home.paths().socket, Arc::new(Tap::default()), |cfg| {
        cfg.desktop_build = Some(BuildId {
            commit: "abcdef0123456789".into(),
            dirty: None,
            time: 42,
        });
    });
    let first = desk.wait_usable();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["updateUnavailable"],
        "desktop-owned"
    );
    let mut config = desk.host().saved_config().expect("configured route");
    config.ssh_target = "different-target".into();
    let from = desk.rec.count();
    desk.m.configure(vec![config]).unwrap();
    let (index, reset) = desk.rec.wait_status_from(from, "new configuration", |s| {
        s.config_generation > first.config_generation && s.daemon_version.is_none()
    });
    assert_eq!(
        reset.update_unavailable, None,
        "configuration reset clears old ownership before another Hello"
    );
    let (_, first) = desk
        .rec
        .wait_status_from(index, "owned target in new configuration", |s| {
            usable(s) && s.config_generation == reset.config_generation
        });
    assert!(first.update_unavailable.is_some());
    let from = desk.rec.count();
    server.shutdown();
    let (_, disconnected) = desk.rec.wait_status_from(from, "owner disconnected", |s| {
        s.status == StatusKind::Reconnecting
    });
    assert_eq!(
        disconnected.update_unavailable, first.update_unavailable,
        "disconnect does not erase known ownership"
    );
    let _persistent = common::start(&home, |_| {});
    desk.host().kick();
    let (_, next) = desk
        .rec
        .wait_status_from(from, "Persistent successor", |s| {
            usable(s) && s.link_generation > first.link_generation
        });
    assert_eq!(
        next.update_unavailable, None,
        "authoritative new Hello replaces diagnosis"
    );
    assert_eq!(next.config_generation, first.config_generation);
    assert!(!next
        .daemon_capabilities
        .iter()
        .any(|c| c == "daemon.desktop-owned"));
    desk.m.shutdown();
}

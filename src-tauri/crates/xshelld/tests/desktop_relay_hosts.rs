#![cfg(unix)]
#![cfg_attr(
    not(feature = "legacy-pair-test-control"),
    allow(unused_imports, dead_code)
)]
//! First S3 owner-backend vertical. Actual public pairing/controllers, production workerd,
//! actual Daemons and Manager HostHandles. No SSH configuration for Relay-only positives.
mod common;
#[path = "desktop/workerd.rs"]
mod workerd;
use common::claude_history;
use serde_json::json;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use workerd::Relay;
use xshell_hostlink::ring::hosts::{relay_host_id, Reachability};
use xshell_hostlink::{HostConfig, TermSink, Transport, TransportFactory};
use xshell_protocol::msg::OpenSpec;
use xshelld::server::{Config, Server};
#[path = "desktop/relay_fixture.rs"]
mod fixture;
use fixture::*;

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn v1_standalone_pairing_discovers_host_without_ssh_or_computer_completion() {
    let relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let guest = Computer::new(&relay);
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    assert!(host.saved_config().is_none());
    assert_eq!(host.child_pid(), None);
    assert_eq!(
        owner.manager.available_hosts().len(),
        1,
        "Desktop is not a Host"
    );
    assert_eq!(owner.ring.chain().unwrap().head().roster().v, 1);
    assert!(owner
        .ring
        .chain()
        .unwrap()
        .head()
        .roster()
        .membership()
        .is_err());
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &relay.target(),
            owner.ring.chain().unwrap().ring_id()
        ),
        2
    );
    owner.manager.configure(vec![]).unwrap();
    assert!(Arc::ptr_eq(&host, &owner.manager.host(host.id()).unwrap()));
    assert!(!owner.events.available.lock().unwrap().is_empty());
    call(&host, "list_dir", json!({"path":guest.home.home()}));
    // Same version and build as this Desktop: nothing to update, and nothing blocks it (S4).
    assert_eq!(host.status().route_update_unavailable, None);
    assert_eq!(host.status().update_action, None);
    assert_eq!(host.status().os, None, "Relay hello does not prove OS");
    assert!(reply(|w| host.upgrade(w)).is_err());
    for method in ["daemon.upgrade", "daemon.upgrade-selected", "ring.join"] {
        assert!(reply(|w| host.call(method.into(), json!({}), w)).is_err());
    }
}

#[test]
fn v2_public_join_discovers_existing_hosts_bidirectionally_and_same_names_stay_distinct() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    let a_chain = a.create();
    let b = Computer::new(&relay);
    let chain = b.join(&a);
    let a_key = a_chain
        .head()
        .roster()
        .members
        .iter()
        .find(|m| m.role == xshell_protocol::ring::Role::Daemon)
        .unwrap()
        .sign_key;
    let b_key = chain
        .head()
        .roster()
        .members
        .iter()
        .find(|m| m.role == xshell_protocol::ring::Role::Daemon && m.sign_key != a_key)
        .unwrap()
        .sign_key;
    let a_to_b = a.relay_host(b_key);
    let b_to_a = b.relay_host(a_key);
    assert_ne!(a_to_b.id(), b_to_a.id());
    // A Ring Host is never probed over SSH: its OS comes from its Daemon's update info, so
    // a new Shell there matches it (round 6, D1: a Linux Desktop started Bash on Windows).
    // `relay_host` returns as soon as the Host is usable.
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    // Known the moment the Host is usable (published after the answer), never later.
    for host in [&a_to_b, &b_to_a] {
        assert_eq!(host.status().os.as_deref(), Some(os));
        assert_eq!(host.status().arch.as_deref(), Some(std::env::consts::ARCH));
    }
    for c in [&a, &b] {
        wait("two signed Daemons", || {
            c.manager.available_hosts().len() == 2
        });
        let rows = c.manager.available_hosts();
        assert_ne!(rows[0].id, rows[1].id);
        assert_eq!(
            rows[0].name, rows[1].name,
            "names are display, not identity"
        );
        assert!(rows.iter().all(|h| h.daemon_key.is_some()));
        c.manager.configure(vec![]).unwrap();
        assert_eq!(c.manager.available_hosts().len(), 2);
    }
    call(&a_to_b, "list_claude_projects", json!({}));
    call(&b_to_a, "list_claude_projects", json!({}));
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn relay_host_history_open_attach_input_relaunch_close_once() {
    let mut relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let guest = Computer::new(&relay);
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    let project = guest.home.project("project");
    claude_history(&guest.home, &project, "retained-session");
    let history = call(&host, "list_claude_projects", json!({}));
    assert!(history.to_string().contains("project"), "{history}");
    let launches = guest.home.root().join("launches");
    let fake = common::Fake::in_dir(&project);
    let _reaper = common::FakeReaper(fake.pids_log.clone());
    std::fs::write(project.join("agent.sh"), format!("echo started >> '{}'; echo READY; while IFS= read -r line; do echo \"REPLY:$line\"; done", launches.display())).unwrap();
    let terminal = Uuid::new_v4();
    let sink = Arc::new(Sink::default());
    let spec = OpenSpec {
        terminal,
        launch: common::claude_spec(&project, Some("retained-session")),
        cols: 80,
        rows: 24,
        meta: Default::default(),
        first_message: None,
        adopt_existing: false,
    };
    let opened = reply(|w| host.term_open(spec, sink.clone(), w)).unwrap();
    assert!(opened.pid.is_some());
    wait("initial output", || {
        String::from_utf8_lossy(&sink.0.lock().unwrap()).contains("READY")
    });
    host.term_input(terminal, "hello\n".into(), None, None)
        .unwrap();
    wait("input reply", || {
        String::from_utf8_lossy(&sink.0.lock().unwrap()).contains("REPLY:hello")
    });
    let second = Arc::new(Sink::default());
    reply(|w| host.term_attach(terminal, second.clone(), w)).unwrap();
    wait("attach replay", || {
        String::from_utf8_lossy(&second.0.lock().unwrap()).contains("READY")
    });
    assert_eq!(
        std::fs::read_to_string(&launches).unwrap().lines().count(),
        1
    );
    let before = host.status().link_generation;
    relay.restart();
    wait("same Host reconnected", || {
        host.status().usable() && host.status().link_generation > before
    });
    let after = host
        .terminals()
        .unwrap()
        .into_iter()
        .find(|t| t.terminal == terminal)
        .unwrap();
    assert_eq!(
        after.pid, opened.pid,
        "Relay reconnect must not launch a replacement"
    );
    assert_eq!(
        std::fs::read_to_string(&launches).unwrap().lines().count(),
        1
    );
    host.term_input(terminal, "after-reconnect\n".into(), None, None)
        .unwrap();
    wait("reattached output", || {
        String::from_utf8_lossy(&second.0.lock().unwrap()).contains("REPLY:after-reconnect")
    });
    reply(|w| host.term_relaunch(terminal, true, w)).unwrap();
    wait("one explicit relaunch", || {
        std::fs::read_to_string(&launches).unwrap().lines().count() == 2
    });
    reply(|w| host.term_close(terminal, w)).unwrap();
    wait("closed", || {
        host.terminals()
            .is_some_and(|list| list.iter().all(|t| t.terminal != terminal))
    });
    assert_eq!(
        std::fs::read_to_string(&launches).unwrap().lines().count(),
        2
    );
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn offline_signed_host_remains_visible_and_owner_shutdown_fences_retained_handle() {
    let mut relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let mut guest = Computer::new(&relay);
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    guest.daemon.take().unwrap().shutdown();
    wait("closed or unreachable presence", || {
        owner.manager.available_hosts().iter().any(|h| {
            h.daemon_key == Some(key)
                && matches!(
                    h.reachability,
                    Reachability::Closed { .. } | Reachability::Unreachable { .. }
                )
        })
    });
    assert_eq!(owner.manager.available_hosts().len(), 1);
    wait("unusable", || !host.status().usable());
    assert!(reply(|w| host.call("list_dir".into(), json!({"path":"/"}), w)).is_err());
    relay.restart();
    wait("offline row survives Relay reconnect", || {
        owner.manager.available_hosts().len() == 1
    });
    let at = Instant::now();
    owner.ring.quit();
    assert!(at.elapsed() < Duration::from_secs(5));
    assert!(owner.manager.host(host.id()).is_none());
    assert!(reply(|w| host.call("list_dir".into(), json!({"path":"/"}), w)).is_err());
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn signed_revocation_fences_live_host_and_never_reenrolls_it() {
    let relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let guest = Computer::new(&relay);
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    owner.ring.remove_member(&key).unwrap();
    wait("revoked Host removed", || {
        owner.manager.host(host.id()).is_none()
    });
    assert!(reply(|w| host.call("list_dir".into(), json!({"path":"/"}), w)).is_err());
    assert!(host
        .term_input(Uuid::new_v4(), "must not send".into(), None, None)
        .is_err());
    owner.manager.configure(vec![]).unwrap();
    owner.manager.kick_all();
    assert!(owner.manager.available_hosts().is_empty());
    assert!(owner
        .ring
        .chain()
        .unwrap()
        .authorized_member(&key)
        .is_none());
}

/// Pauses only the actual Daemon Host worker, after the real Noise handshake. Dropping
/// the gate releases it even on test panic; no fake stream or authorized row is supplied.
#[derive(Default)]
struct HostGate {
    state: Mutex<(bool, bool)>,
    cv: std::sync::Condvar,
}
impl HostGate {
    fn hook(self: &Arc<Self>) -> xshelld::server::TestHook {
        let gate = self.clone();
        xshelld::server::TestHook(Arc::new(move |_, point| {
            if point != xshelld::server::TestPoint::RelayHostAccepted {
                return false;
            }
            let mut state = gate.state.lock().unwrap();
            state.0 = true;
            gate.cv.notify_all();
            while !state.1 {
                state = gate.cv.wait(state).unwrap();
            }
            false
        }))
    }
    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.cv.notify_all();
    }
    fn entered(&self) -> bool {
        self.state.lock().unwrap().0
    }
}
struct ReleaseGate(Arc<HostGate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn cancelled_owner_shutdown_during_real_host_handshake_never_installs_a_late_link() {
    late_host_link(false);
}
#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn revocation_during_real_host_handshake_never_installs_a_late_link() {
    late_host_link(true);
}
#[cfg(feature = "legacy-pair-test-control")]
fn late_host_link(revoke: bool) {
    let relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let gate = Arc::new(HostGate::default());
    let guest = Computer::with_config(&relay, |cfg| cfg.test_hook = Some(gate.hook()));
    let _release = ReleaseGate(gate.clone());
    let key = guest.legacy_pair(&owner, &relay);
    let id = relay_host_id(&key);
    wait("real Host worker reached", || gate.entered());
    let host = owner.manager.host(&id).unwrap();
    assert!(!host.status().usable());
    let at = Instant::now();
    if revoke {
        owner.ring.remove_member(&key).unwrap();
    } else {
        owner.ring.quit();
    }
    wait("handle removed", || owner.manager.host(&id).is_none());
    assert!(at.elapsed() < Duration::from_secs(5));
    gate.release();
    assert!(reply(|w| host.call("list_dir".into(), json!({"path":"/"}), w)).is_err());
    wait("Daemon's cancelled session worker drained", || {
        guest.daemon.as_ref().unwrap().connections() == 0
    });
    assert!(
        !host.status().usable(),
        "late Host hello installed a usable Link"
    );
    assert!(owner.manager.host(&id).is_none());
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn ring_route_keeps_desktop_owned_diagnosis() {
    let relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let guest = Computer::with_config(&relay, |cfg| {
        cfg.gui_bound = Some(xshelld::server::GuiBound {
            parent_pid: std::process::id(),
        });
    });
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    let status = host.status();
    // A GUI-bound Daemon offers no `daemon.stage`; at this Desktop's own version and build
    // there is nothing to update, so no Ring limitation is shown either (S4).
    assert_eq!(status.route_update_unavailable, None);
    assert_eq!(
        status.update_unavailable,
        Some(xshell_hostlink::status::UpdateUnavailable::DesktopOwned)
    );
    assert_eq!(status.update_action, None);
    assert_eq!(status.os, None);
    assert!(reply(|w| host.upgrade(w)).is_err());
    call(&host, "list_claude_projects", json!({}));
}

#[test]
fn independently_local_daemon_identity_resolves_relay_alias_to_local() {
    struct LocalOnly(std::path::PathBuf);
    impl TransportFactory for LocalOnly {
        fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
            panic!("Local must not construct SSH")
        }
        fn direct(&self, cfg: &HostConfig) -> Option<Box<dyn xshell_hostlink::Dialer>> {
            assert_eq!(cfg.id, "local");
            Some(Box::new(xshell_hostlink::UnixSocketDialer {
                path: self.0.clone(),
            }))
        }
    }
    let relay = Relay::start();
    let owner = Computer::with_factory(
        &relay,
        |_| {},
        |home| Arc::new(LocalOnly(home.paths().socket)),
    );
    owner.manager.set_local(HostConfig {
        id: "local".into(),
        name: "This computer".into(),
        ssh_target: "local".into(),
        color: None,
        daemon_command: None,
        launch_prefixes: Default::default(),
    });
    wait("Local usable", || {
        owner.manager.host("local").unwrap().status().usable()
    });
    let chain = owner.create();
    let key = chain
        .head()
        .roster()
        .members
        .iter()
        .find(|m| m.role == xshell_protocol::ring::Role::Daemon)
        .unwrap()
        .sign_key;
    let id = relay_host_id(&key);
    wait("Relay alias independently resolved to Local", || {
        owner.manager.host(&id).is_some_and(|h| h.id() == "local")
    });
    assert_eq!(owner.manager.available_hosts().len(), 1);
    let local = owner.manager.host("local").unwrap();
    let bound = owner.manager.host(&id).unwrap();
    assert_eq!(
        bound.status().config_generation,
        local.status().config_generation
    );
    reply(|waiter| local.ring_identity(local.status().link_generation, waiter)).unwrap();
    assert_eq!(owner.manager.available_hosts()[0].aliases, ["local", &id]);
    call(
        &owner.manager.host(&id).unwrap(),
        "list_claude_projects",
        json!({}),
    );
    assert_eq!(
        owner.daemon.as_ref().unwrap().connections(),
        1,
        "no competing self-Relay HostLink"
    );
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn saved_ssh_configuration_coexists_with_discovery_without_authorizing_same_name_aliases() {
    struct SavedSsh;
    impl TransportFactory for SavedSsh {
        fn for_host(&self, cfg: &HostConfig) -> Box<dyn Transport> {
            Box::new(xshell_hostlink::SshTransport::new(cfg.ssh_target.clone()))
        }
    }
    let relay = Relay::start();
    let owner = Computer::with_factory(&relay, |_| {}, |_| Arc::new(SavedSsh));
    owner.ring.enable(None, false).unwrap();
    let guest = Computer::new(&relay);
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    let name = owner.manager.available_hosts()[0].name.clone();
    // Negative SSH control only. No successful Relay assertion depends on this target.
    owner
        .manager
        .configure(vec![HostConfig {
            id: "h_aaaaaaaa".into(),
            name,
            ssh_target: "xshell-unresolvable.invalid".into(),
            color: None,
            daemon_command: Some("xshelld".into()),
            launch_prefixes: Default::default(),
        }])
        .unwrap();
    let rows = owner.manager.available_hosts();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "h_aaaaaaaa");
    assert_eq!(
        rows[0].daemon_key, None,
        "a shared name cannot prove identity"
    );
    assert_eq!(rows[1].id, host.id());
    assert_eq!(rows[1].aliases, [host.id()]);
    owner.manager.configure(vec![]).unwrap();
    assert_eq!(owner.manager.available_hosts().len(), 1);
    assert!(Arc::ptr_eq(&host, &owner.manager.host(host.id()).unwrap()));
    call(&host, "list_claude_projects", json!({}));
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn manager_shutdown_closes_only_the_host_session_not_the_elected_connector() {
    let relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let guest = Computer::new(&relay);
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    call(&host, "list_claude_projects", json!({}));
    assert_eq!(guest.daemon.as_ref().unwrap().connections(), 1);
    owner.manager.shutdown();
    wait("remote Host connection retired", || {
        guest.daemon.as_ref().unwrap().connections() == 0
    });
    assert_eq!(
        owner.ring.view().connection,
        "connected",
        "closing one Host must not stop the elected Connector"
    );
    assert!(owner
        .ring
        .chain()
        .unwrap()
        .authorized_member(&key)
        .is_some());
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn owner_quit_is_bounded_while_a_real_relay_delivery_is_blocked() {
    struct BlockingSink(Arc<HostGate>);
    impl TermSink for BlockingSink {
        fn data(&self, _: &[u8]) -> bool {
            let mut state = self.0.state.lock().unwrap();
            state.0 = true;
            self.0.cv.notify_all();
            while !state.1 {
                state = self.0.cv.wait(state).unwrap();
            }
            true
        }
        fn exit(&self, _: i32, _: u64) {}
    }
    let relay = Relay::start();
    let owner = Computer::new(&relay);
    owner.ring.enable(None, false).unwrap();
    let guest = Computer::new(&relay);
    let gate = Arc::new(HostGate::default());
    let _release = ReleaseGate(gate.clone());
    let key = guest.legacy_pair(&owner, &relay);
    let host = owner.relay_host(key);
    let script = guest
        .home
        .script("blocked-output", "echo BLOCKED; exec sleep 1000");
    let terminal = Uuid::new_v4();
    let (tx, rx) = mpsc::channel();
    host.term_open(
        OpenSpec {
            terminal,
            launch: common::raw_spec(&guest.home.project("blocked"), &script),
            cols: 80,
            rows: 24,
            meta: Default::default(),
            first_message: None,
            adopt_existing: false,
        },
        Arc::new(BlockingSink(gate.clone())),
        Box::new(move |r| {
            let _ = tx.send(r);
        }),
    );
    wait("real output delivery blocked", || gate.entered());
    owner.manager.configure(vec![]).unwrap();
    let at = Instant::now();
    owner.ring.quit();
    assert!(
        at.elapsed() < Duration::from_millis(3500),
        "{:?}",
        at.elapsed()
    );
    assert!(owner.manager.host(host.id()).is_none());
    gate.release();
    let _ = rx.recv_timeout(WAIT).unwrap();
    wait("blocked Relay reader ended", || {
        guest.daemon.as_ref().unwrap().connections() == 0
    });
    let independent = common::Client::in_process(
        guest.daemon.as_ref().unwrap(),
        xshelld::server::Role::Desktop,
    );
    assert!(
        independent.log.iter().any(|event| matches!(event,
        common::Ev::Msg(xshell_protocol::msg::ServerMsg::Terminals { list })
            if list.iter().any(|t| t.terminal == terminal && t.exit_code.is_none()))),
        "owner quit must not close the Remote Terminal"
    );
}

#[test]
#[cfg(feature = "legacy-pair-test-control")]
fn same_configured_endpoint_with_foreign_live_key_cannot_keep_the_old_ring_alias() {
    // A configured-route negative, not an actual-SSH equivalence positive: the stable
    // endpoint reaches two real Daemons in turn. Ring authority comes only from public
    // legacy pairing and public foreign Create, both through production workerd.
    struct ConfiguredSocket(std::sync::OnceLock<std::path::PathBuf>);
    impl TransportFactory for ConfiguredSocket {
        fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
            panic!("configured socket must not invoke SSH");
        }
        fn direct(&self, cfg: &HostConfig) -> Option<Box<dyn xshell_hostlink::Dialer>> {
            assert_eq!(cfg.id, "h_aaaaaaaa");
            Some(Box::new(xshell_hostlink::UnixSocketDialer {
                path: self.0.get().unwrap().clone(),
            }))
        }
    }
    let relay = Relay::start();
    let endpoint = Arc::new(ConfiguredSocket(std::sync::OnceLock::new()));
    let owner = Computer::with_factory(&relay, |_| {}, |_| endpoint.clone());
    owner.ring.enable(None, false).unwrap();
    let mut original = Computer::new(&relay);
    let key = original.legacy_pair(&owner, &relay);
    endpoint.0.set(original.home.paths().socket).unwrap();
    let sync = Box::leak(Box::new(xshell_hostlink::ring::HostSync::new()));
    *owner.events.sync.lock().unwrap() = Some(sync);
    let worker = xshell_hostlink::ring::SyncWorker::new(
        owner.ring.clone(),
        sync,
        Box::new({
            let manager = owner.manager.clone();
            move |id| manager.host(id)
        }),
        Box::new(|| {}),
    );
    worker.start().unwrap();
    owner
        .manager
        .configure(vec![HostConfig {
            id: "h_aaaaaaaa".into(),
            name: "Saved".into(),
            ssh_target: "stable-target".into(),
            color: None,
            daemon_command: None,
            launch_prefixes: Default::default(),
        }])
        .unwrap();
    let alias = relay_host_id(&key);
    wait("retained association established by live HostSync", || {
        owner
            .manager
            .host(&alias)
            .is_some_and(|h| h.id() == "h_aaaaaaaa")
    });
    let addressed_to_original = owner.manager.host(&alias).unwrap();
    let saved = owner.manager.host("h_aaaaaaaa").unwrap();
    assert_eq!(
        call(&addressed_to_original, "get_home_dir", json!({})),
        json!(original.home.home()),
        "valid alias access must work before replacement"
    );
    assert_eq!(
        addressed_to_original.status().route_update_unavailable,
        Some(xshell_hostlink::status::RouteUpdateUnavailable::IdentityBoundAlias)
    );
    assert_eq!(saved.status().route_update_unavailable, None);
    let mut foreign = Computer::new(&relay);
    let foreign_chain = foreign.create();
    let foreign_key = foreign_chain
        .head()
        .roster()
        .members
        .iter()
        .find(|member| member.role == xshell_protocol::ring::Role::Daemon)
        .unwrap()
        .sign_key;
    assert_ne!(foreign_key, key);
    foreign.daemon.take().unwrap().shutdown();
    original.daemon.take().unwrap().shutdown();
    let mut replacement_paths = foreign.home.paths();
    replacement_paths.socket = endpoint.0.get().unwrap().clone();
    foreign.daemon =
        Some(Server::start(Config::new(foreign.home.home(), replacement_paths)).unwrap());
    wait("foreign current identity refused by HostSync", || {
        worker.state_of("h_aaaaaaaa") == Some("other-ring")
    });
    assert!(
        owner
            .manager
            .host(&alias)
            .is_none_or(|h| h.id() != saved.id()),
        "r_K still resolves to configured K2"
    );
    let forbidden = foreign
        .home
        .home()
        .join("alias-must-not-execute-on-replacement");
    let refused =
        reply(|w| addressed_to_original.call("make_dir".into(), json!({"path": forbidden}), w));
    assert!(refused.is_err(), "retained r_K access executed on K2");
    assert!(!forbidden.exists(), "refused alias request reached K2");
    let input_log = foreign.home.home().join("independent-input");
    let script = foreign.home.script(
        "replacement-input",
        &format!(
            "echo READY; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
            input_log.display()
        ),
    );
    let terminal = Uuid::new_v4();
    let sink = Arc::new(Sink::default());
    reply(|waiter| {
        saved.term_open(
            OpenSpec {
                terminal,
                launch: common::raw_spec(&foreign.home.project("input"), &script),
                cols: 80,
                rows: 24,
                meta: Default::default(),
                first_message: None,
                adopt_existing: false,
            },
            sink.clone(),
            waiter,
        )
    })
    .unwrap();
    wait("independent K2 Terminal ready", || {
        String::from_utf8_lossy(&sink.0.lock().unwrap()).contains("READY")
    });
    assert!(addressed_to_original
        .term_input(terminal, "wrong identity\n".into(), None, None)
        .is_err());
    assert!(addressed_to_original.term_resize(terminal, 1, 1).is_err());
    assert!(addressed_to_original.term_detach(terminal).is_err());
    saved
        .term_input(terminal, "independent identity\n".into(), None, None)
        .unwrap();
    wait("independent input executed", || {
        std::fs::read_to_string(&input_log).is_ok_and(|text| text.contains("independent identity"))
    });
    assert!(!std::fs::read_to_string(&input_log)
        .unwrap()
        .contains("wrong identity"));
    reply(|waiter| saved.term_close(terminal, waiter)).unwrap();
    assert_eq!(
        call(&saved, "get_home_dir", json!({})),
        json!(foreign.home.home())
    );
    assert!(owner
        .ring
        .chain()
        .unwrap()
        .authorized_member(&foreign_key)
        .is_none());
    assert_eq!(
        foreign.ring.chain().unwrap().head().hash(),
        foreign_chain.head().hash()
    );
    let mut independent = common::Client::in_process(
        foreign.daemon.as_ref().unwrap(),
        xshelld::server::Role::Desktop,
    );
    let identity = independent
        .request(&xshell_protocol::msg::ClientMsg::RingIdentity)
        .unwrap();
    assert_eq!(identity["ring"]["ringId"], json!(foreign_chain.ring_id()));
    let connections = foreign.daemon.as_ref().unwrap().connections();
    let at = Instant::now();
    owner.manager.shutdown();
    assert!(at.elapsed() < Duration::from_millis(3500));
    wait(
        "configured connection retired while alias access survives",
        || foreign.daemon.as_ref().unwrap().connections() < connections,
    );
    assert!(
        reply(|waiter| addressed_to_original.call("get_home_dir".into(), json!({}), waiter))
            .is_err()
    );
    sync.shutdown();
}

/// Waits up to `limit` for `f`; returns when it first held.
fn wait_up_to(what: &str, limit: Duration, f: impl Fn() -> bool) -> Instant {
    let until = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < until, "timed out after {limit:?}: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
    Instant::now()
}

/// The owner's row for the Ring Host with Daemon key `key`.
fn host_row(
    c: &Computer,
    key: xshell_protocol::ring::SignKey,
) -> Option<xshell_hostlink::ring::hosts::AvailableHost> {
    c.manager
        .available_hosts()
        .into_iter()
        .find(|h| h.daemon_key == Some(key))
}

fn online(c: &Computer, key: xshell_protocol::ring::SignKey) -> bool {
    host_row(c, key).is_some_and(|h| h.reachability == Reachability::Online)
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Milliseconds until a handle's next timed retry (0 when none is scheduled).
fn retry_in_ms(host: &xshell_hostlink::HostHandle) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    host.status()
        .next_retry_at
        .map_or(0, |at| at.saturating_sub(now))
}

/// B2 (#110): a Computer Device's Daemon that was offline long enough for the owner's Relay
/// HostHandle to back off far shows its version within seconds of its Daemon key turning
/// Online again: presence Online kicks the handle instead of waiting out the backoff. The
/// version must come from a fresh Hello (a new usable link generation), not one left over.
#[test]
fn relay_host_version_known_within_seconds_of_coming_online() {
    let relay = Relay::start();
    // Owner retries after 1, 2, 4, 8, 16 ... s, so a timed retry cannot pass the deadline.
    let a = Computer::with_manager(&relay, |m| m.backoff_unit = Duration::from_secs(1));
    a.create();
    let mut b = Computer::new(&relay);
    b.join(&a);
    let key = common::ring::identity(b.daemon.as_ref().unwrap()).0;
    let host = a.relay_host(key);
    let before = host.status().link_generation;
    assert!(host.status().daemon_version.is_some());
    b.stop_daemon();
    wait("the Daemon key is no longer Online", || !online(&a, key));
    // Several failed dials: the next timed retry is at least 12 s away. Only a kick can
    // bring an attempt before that scheduled time.
    wait_up_to("the owner's backoff grows", Duration::from_secs(60), || {
        !host.status().usable() && retry_in_ms(&host) >= 12_000
    });
    let scheduled = host.status().next_retry_at.expect("a scheduled retry");
    b.restart_daemon(|_| {});
    wait_up_to(
        "the Daemon key Online again",
        Duration::from_secs(20),
        || online(&a, key),
    );
    // When Online is observed, the timed retry still lies beyond the 3 s deadline, so only
    // the presence-triggered attempt can satisfy what follows.
    let deadline = Duration::from_secs(3);
    assert!(
        scheduled > epoch_ms() + deadline.as_millis() as u64 + 500,
        "inconclusive: the timed retry is due within the deadline (restart too slow)"
    );
    wait_up_to("a fresh Hello within 3 s of Online", deadline, || {
        let s = host.status();
        s.usable() && s.link_generation > before
    });
    assert!(
        epoch_ms() < scheduled,
        "the fresh Hello came no sooner than the timed retry"
    );
    // What Settings > Ring shows for the Computer Device: the row its Daemon key stands for
    // is Online and that Host's status names the version of the new connection.
    let row = host_row(&a, key).unwrap();
    assert_eq!(row.reachability, Reachability::Online);
    let status = a
        .events
        .statuses
        .lock()
        .unwrap()
        .get(&row.id)
        .cloned()
        .unwrap();
    assert!(status.link_generation > before);
    assert!(status.daemon_version.is_some());
}

/// B2/S5: only a Host's own transition to Online kicks its handle. An Online Host that
/// refuses connections keeps its backoff while another Host's presence goes and comes back.
#[test]
fn another_hosts_presence_churn_does_not_bypass_an_online_hosts_backoff() {
    let relay = Relay::start();
    let a = Computer::with_manager(&relay, |m| m.backoff_unit = Duration::from_secs(1));
    a.create();
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let x = {
        let refuse = refuse.clone();
        Computer::with_config(&relay, move |cfg| {
            cfg.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
                point == xshelld::server::TestPoint::RelayHostAccepted
                    && refuse.load(std::sync::atomic::Ordering::SeqCst)
            })))
        })
    };
    x.join(&a);
    let x_key = common::ring::identity(x.daemon.as_ref().unwrap()).0;
    let mut y = Computer::new(&relay);
    y.join(&a);
    let y_key = common::ring::identity(y.daemon.as_ref().unwrap()).0;
    a.relay_host(y_key);
    let id = relay_host_id(&x_key);
    wait("X has a handle", || a.manager.host(&id).is_some());
    let hx = a.manager.host(&id).unwrap();
    // X stays Online but refuses: its retries back off to at least 8 s.
    wait_up_to("X's backoff grows", Duration::from_secs(40), || {
        online(&a, x_key) && !hx.status().usable() && retry_in_ms(&hx) >= 8_000
    });
    let attempts = hx.link_generation();
    let window = Instant::now() + Duration::from_millis(retry_in_ms(&hx).saturating_sub(500));
    y.stop_daemon();
    wait("Y offline", || !online(&a, y_key));
    y.restart_daemon(|_| {});
    wait("Y Online again", || online(&a, y_key));
    a.relay_host(y_key);
    assert!(
        Instant::now() < window,
        "Y's presence churn outlasted X's backoff window: inconclusive"
    );
    assert!(online(&a, x_key));
    assert_eq!(
        hx.link_generation(),
        attempts,
        "another Host's presence kicked X's handle"
    );
    // X's own handle still connects once it stops refusing (at its next timed retry).
    refuse.store(false, std::sync::atomic::Ordering::SeqCst);
    wait_up_to(
        "X usable after its timed retry",
        Duration::from_secs(40),
        || hx.status().usable(),
    );
}

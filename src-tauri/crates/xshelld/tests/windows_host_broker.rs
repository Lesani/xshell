#![cfg(windows)]
//! Separate native Desktop backend processes; no GUI, installed profiles or services.
#[path = "desktop/windows_fixture.rs"]
#[allow(dead_code)]
mod fixture;
mod win;
#[path = "desktop/workerd.rs"]
mod workerd;
use fixture::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use workerd::Relay;
use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig};
use xshell_hostlink::{HostAccess, HostError, Manager, ManagerConfig, TermSink};
use xshell_protocol::msg::OpenSpec;
include!("desktop/broker_process.rs");

struct Setup {
    relay: Relay,
    owner: Computer,
    remote: Computer,
    host: String,
}
impl Setup {
    fn new() -> Self {
        Self::with_config(|_| {})
    }
    fn with_config(tweak: impl FnOnce(&mut xshelld::server::Config)) -> Self {
        let relay = Relay::start();
        let owner = Computer::new(&relay);
        let created = owner.create();
        let remote = Computer::with_config(&relay, tweak);
        let joined = remote.join(&owner);
        let key = joined
            .head()
            .roster()
            .members
            .iter()
            .find(|m| {
                m.role == xshell_protocol::ring::Role::Daemon
                    && created.authorized_member(&m.sign_key).is_none()
            })
            .unwrap()
            .sign_key;
        owner.relay_host(key);
        let host = xshell_hostlink::ring::hosts::relay_host_id(&key);
        remote.ring.quit();
        remote.manager.shutdown();
        owner.ring.quit();
        owner.manager.shutdown();
        wait("fixture Links drained", || {
            remote.daemon.as_ref().unwrap().connections() == 0
        });
        Self {
            relay,
            owner,
            remote,
            host,
        }
    }
    fn process(&self) -> Process {
        let mut p = Process::start(&self.owner.home.root().join("desktop"));
        p.usable(&self.host);
        p
    }
    fn spec(&self, terminal: Uuid) -> (OpenSpec, std::path::PathBuf) {
        let project = self.remote.home.project("broker-native-cmd");
        let launches = project.join("launches.log");
        let script = project.join("agent.cmd");
        std::fs::write(&script, "@echo off\r\necho NATIVE-READY\r\n").unwrap();
        let agent = std::path::PathBuf::from(
            std::env::var_os("XSHELLD_TEST_BROKER_AGENT")
                .expect("absolute native broker fixture launcher required"),
        );
        assert!(agent.is_absolute() && agent.is_file());
        let executable = project.join("agent.exe");
        std::fs::copy(agent, &executable).unwrap();
        let launch = xshell_core::launch::LaunchSpec {
            cwd: project.to_str().unwrap().into(),
            shell_mode: Some("raw".into()),
            shell_command: Some(executable.to_str().unwrap().into()),
            ..Default::default()
        };
        (
            OpenSpec {
                terminal,
                launch,
                cols: 100,
                rows: 30,
                meta: Default::default(),
                first_message: None,
                adopt_existing: false,
            },
            launches,
        )
    }
    fn endpoint(&self) -> std::path::PathBuf {
        xshell_core::local_ipc::host_broker_endpoint(
            &xshell_core::local_ipc::canonical_store(&self.owner.home.root().join("desktop"))
                .unwrap(),
        )
        .unwrap()
    }
}
fn target_snapshot<'a>(snapshot: &'a Value, host: &str) -> &'a Value {
    let row = snapshot["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["aliases"].as_array().unwrap().iter().any(|id| id == host))
        .unwrap();
    &snapshot["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["status"]["host"] == row["id"])
        .unwrap()["status"]
}
#[test]
fn owner_follower_have_one_connector_session_and_host_link_after_idle_and_relay_restart() {
    let mut setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    assert_eq!(owner.ok(json!({"op":"snapshot"}))["owner"], true);
    assert_eq!(follower.ok(json!({"op":"snapshot"}))["owner"], false);
    std::thread::sleep(Duration::from_secs(6));
    setup.relay.restart();
    owner.usable(&setup.host);
    follower.usable(&setup.host);
    for p in [&mut owner, &mut follower] {
        assert!(p.ok(json!({"op":"call","host":setup.host,"method":"list_dir","params":{"path":setup.remote.home.home()}})).is_array());
    }
    assert_eq!(setup.remote.daemon.as_ref().unwrap().connections(), 1);
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &setup.relay.target(),
            setup.owner.ring.chain().as_ref().unwrap().ring_id()
        ),
        2
    );
    follower.quit();
    owner.quit();
}
fn handover(forced: bool) {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let terminal = Uuid::new_v4();
    let (spec, launches) = setup.spec(terminal);
    let opened = owner.ok(json!({"op":"open","host":setup.host,"spec":spec}));
    owner.text(terminal, "NATIVE-READY");
    follower.ok(json!({"op":"attach","host":setup.host,"terminal":terminal}));
    follower.text(terminal, "NATIVE-READY");
    let before = follower.ok(json!({"op":"snapshot"}));
    let status = target_snapshot(&before, &setup.host);
    let old = json!({"configGeneration":status["configGeneration"],"linkGeneration":status["linkGeneration"]});
    if forced {
        owner.child.kill().unwrap();
        owner.child.wait().unwrap();
    } else {
        owner.quit();
    }
    let until = Instant::now() + WAIT;
    loop {
        let snapshot = follower.ok(json!({"op":"snapshot"}));
        if snapshot["owner"] == true
            && snapshot["snapshots"].as_array().unwrap().iter().any(|s| {
                s["terminals"].as_array().is_some_and(|ts| {
                    ts.iter()
                        .any(|t| t["terminal"] == terminal.to_string() && t["pid"] == opened["pid"])
                })
            })
        {
            break;
        }
        assert!(Instant::now() < until, "handover failed: {snapshot}");
        std::thread::sleep(Duration::from_millis(20));
    }
    follower.usable(&setup.host);
    let until = Instant::now() + WAIT;
    loop {
        let out = follower.ok(json!({"op":"output","terminal":terminal}));
        if out["resets"].as_u64().unwrap() > 0
            && out["text"]
                .as_str()
                .unwrap()
                .matches("NATIVE-READY")
                .count()
                >= 2
        {
            break;
        }
        assert!(
            Instant::now() < until,
            "reset/replay subscription not restored: {out}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let stale=follower.command(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"echo BAD-STALE\r","target":old}));
    assert_eq!(stale["error"]["code"], "offline");
    follower.ok(
        json!({"op":"input","host":setup.host,"terminal":terminal,"data":"echo NATIVE-AFTER\r"}),
    );
    follower.text(terminal, "NATIVE-AFTER");
    assert!(win::alive(opened["pid"].as_u64().unwrap() as u32));
    assert_eq!(
        std::fs::read_to_string(launches).unwrap().lines().count(),
        1,
        "open must never replay"
    );
    follower.quit();
}
#[test]
fn clean_owner_handover_preserves_terminal_uuid_pid_and_never_replays_open() {
    handover(false);
}
#[test]
fn forced_owner_handover_preserves_terminal_uuid_pid_and_never_replays_open() {
    handover(true);
}
#[test]
fn hello_drip_budget_and_partial_frame_budget_do_not_end_healthy_streams() {
    use std::io::Read;
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut drip =
        xshell_core::local_ipc::protected_connect(&setup.endpoint(), Duration::from_secs(2))
            .unwrap();
    drip.write_all(&[0]).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    drip.write_all(&[0]).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    drip.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert_eq!(
        drip.read(&mut [0]).unwrap(),
        0,
        "Hello progress cannot renew five seconds"
    );
    let root =
        xshell_core::local_ipc::canonical_store(&setup.owner.home.root().join("desktop")).unwrap();
    let profile = xshell_core::local_ipc::host_broker_profile(&root).unwrap();
    let mut raw =
        xshell_core::local_ipc::protected_connect(&setup.endpoint(), Duration::from_secs(2))
            .unwrap();
    let bytes =
        serde_json::to_vec(&json!({"type":"hello","version":1,"profile_id":profile})).unwrap();
    raw.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
    raw.write_all(&0_u64.to_be_bytes()).unwrap();
    raw.write_all(&bytes).unwrap();
    // Drain Welcome and Snapshot, then a partial key header spends the fixed assembly budget.
    for _ in 0..2 {
        let mut len = [0; 4];
        raw.read_exact(&mut len).unwrap();
        let mut data = vec![0; 8 + u32::from_be_bytes(len) as usize];
        raw.read_exact(&mut data).unwrap();
    }
    raw.write_all(&[0]).unwrap();
    std::thread::sleep(Duration::from_secs(31));
    raw.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    raw.read_to_end(&mut Vec::new()).unwrap();
    owner.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    owner.quit();
}
#[test]
fn long_call_exceeds_companion_budget_and_cancel_isolated_between_clients() {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let name = win::unique_pipe();
    let listener = xshell_core::pipe::PipeListener::bind(&name).unwrap();
    let id = owner.send(
        json!({"op":"call","host":setup.host,"method":"read_text_file","params":{"path":name}}),
    );
    let mut stream = listener
        .accept_until(Instant::now() + WAIT, &|| false)
        .unwrap();
    let cancelled_name = win::unique_pipe();
    let cancelled_listener = xshell_core::pipe::PipeListener::bind(&cancelled_name).unwrap();
    let start_id = follower.send(json!({"op":"start-call","host":setup.host,"method":"read_text_file","params":{"path":cancelled_name}}));
    let request_id = follower.receive(start_id)["result"].as_u64().unwrap();
    let mut cancelled_stream = cancelled_listener
        .accept_until(Instant::now() + WAIT, &|| false)
        .unwrap();
    follower.ok(json!({"op":"cancel","host":setup.host,"request":request_id}));
    assert_eq!(
        follower.receive(start_id + 1_000_000)["error"]["code"],
        "indeterminate"
    );
    follower.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    cancelled_stream
        .write_all(b"late-cancelled-result")
        .unwrap();
    cancelled_stream.shutdown(std::net::Shutdown::Both).unwrap();
    std::thread::sleep(Duration::from_secs(6));
    stream.write_all(b"native-long-call").unwrap();
    stream.shutdown(std::net::Shutdown::Both).unwrap();
    assert_eq!(owner.receive(id)["result"], "native-long-call");
    follower.quit();
    owner.quit();
}
#[test]
fn large_legal_reply_preserves_bytes_across_native_broker() {
    let setup = Setup::with_config(|cfg| cfg.conn_total_cap = 80 * 1024 * 1024);
    let mut owner = setup.process();
    let mut follower = setup.process();
    let file = setup.remote.home.root().join("large.txt");
    let bytes = "x".repeat(20 * 1024 * 1024);
    std::fs::write(&file, &bytes).unwrap();
    let reply = follower.ok(
        json!({"op":"call","host":setup.host,"method":"read_text_file","params":{"path":file}}),
    );
    assert_eq!(reply.as_str().unwrap(), bytes);
    follower.quit();
    owner.quit();
}

#[test]
fn one_large_profile_assembly_rejects_another_request_busy_without_dispatch() {
    use std::io::Read;
    fn send(stream: &mut xshell_core::pipe::PipeStream, value: &Value) {
        let b = serde_json::to_vec(value).unwrap();
        stream.write_all(&(b.len() as u32).to_be_bytes()).unwrap();
        stream
            .write_all(
                &value
                    .get("request_id")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_be_bytes(),
            )
            .unwrap();
        stream.write_all(&b).unwrap();
    }
    fn read(stream: &mut xshell_core::pipe::PipeStream) -> Value {
        let mut h = [0; 4];
        stream.read_exact(&mut h).unwrap();
        let mut key = [0; 8];
        stream.read_exact(&mut key).unwrap();
        let mut b = vec![0; u32::from_be_bytes(h) as usize];
        stream.read_exact(&mut b).unwrap();
        serde_json::from_slice(&b).unwrap()
    }
    let setup = Setup::with_config(|cfg| cfg.conn_total_cap = 80 * 1024 * 1024);
    let mut owner = setup.process();
    let dir = setup
        .owner
        .home
        .root()
        .join("desktop")
        .canonicalize()
        .unwrap();
    let profile = xshell_core::local_ipc::host_broker_profile(&dir).unwrap();
    let connect = || {
        let mut stream = xshell_core::local_ipc::protected_connect(
            &xshell_core::local_ipc::host_broker_endpoint(&dir).unwrap(),
            Duration::from_secs(2),
        )
        .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        send(
            &mut stream,
            &json!({"type":"hello","version":1,"profile_id":profile}),
        );
        let welcome = read(&mut stream);
        let snapshot = read(&mut stream);
        (stream, welcome, snapshot)
    };
    let (mut first, welcome, snapshot) = connect();
    let (mut second, second_welcome, _) = connect();
    let (row, status) = target_row_status(&snapshot, &setup.host);
    let key = &row["daemonKey"];
    let path = setup.remote.home.root().join("admitted-large.txt");
    std::fs::write(&path, "L".repeat(20 * 1024 * 1024)).unwrap();
    let forbidden = setup
        .remote
        .home
        .root()
        .join("large-refusal-must-not-dispatch");
    let make_request = |epoch: &Value, method: &str, params: Value| json!({"type":"request","owner_epoch":epoch,"request_id":1,"host":setup.host,"canonical":key,"expected":{"configGeneration":status["configGeneration"],"linkGeneration":status["linkGeneration"]},"operation":{"operation":"call","method":method,"params":params}});
    // Serialize the contender before pausing the first reader, so its preparation cannot
    // accidentally run out the owner's finite socket write-stall budget.
    let contender = make_request(
        &second_welcome["owner_epoch"],
        "make_dir",
        json!({"path":forbidden,"padding":"P".repeat(20*1024*1024)}),
    );
    let contender = serde_json::to_vec(&contender).unwrap();
    send(
        &mut first,
        &make_request(
            &welcome["owner_epoch"],
            "read_text_file",
            json!({"path":path}),
        ),
    );
    loop {
        let mut h = [0; 4];
        first.read_exact(&mut h).unwrap();
        let mut key = [0; 8];
        first.read_exact(&mut key).unwrap();
        let len = u32::from_be_bytes(h) as usize;
        if len > 16 * 1024 * 1024 {
            break;
        }
        let mut skip = vec![0; len];
        first.read_exact(&mut skip).unwrap();
    }
    second
        .write_all(&(contender.len() as u32).to_be_bytes())
        .unwrap();
    second.write_all(&1u64.to_be_bytes()).unwrap();
    second.write_all(&contender).unwrap();
    loop {
        let event = read(&mut second);
        if event["type"] == "reply" {
            assert_eq!(event["result"]["Err"]["code"], "busy", "{event}");
            break;
        }
    }
    assert!(!forbidden.exists());
    drop(first);
    drop(second);
    owner.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    owner.quit();
}

fn target_row_status<'a>(snapshot: &'a Value, host: &str) -> (&'a Value, &'a Value) {
    let row = snapshot["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["aliases"].as_array().unwrap().iter().any(|v| v == host))
        .unwrap();
    (row, target_snapshot(snapshot, host))
}
#[test]
fn blocked_viewer_does_not_stall_other_input_or_its_own_mutation_reply() {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let terminal = Uuid::new_v4();
    let (spec, _launches) = setup.spec(terminal);
    owner.ok(json!({"op":"open","host":setup.host,"spec":spec}));
    owner.text(terminal, "NATIVE-READY");
    follower.ok(json!({"op":"attach","host":setup.host,"terminal":terminal,"slow":true}));
    owner.ok(
        json!({"op":"input","host":setup.host,"terminal":terminal,"data":"echo ISOLATED-MARKER\r"}),
    );
    owner.text(terminal, "ISOLATED-MARKER");
    let dir = setup
        .remote
        .home
        .root()
        .join("mutation-during-blocked-viewer");
    follower.ok(json!({"op":"call","host":setup.host,"method":"make_dir","params":{"path":dir}}));
    assert!(dir.is_dir());
    assert_eq!(
        follower.ok(json!({"op":"output","terminal":terminal}))["len"],
        0
    );
    follower.ok(json!({"op":"unblock","terminal":terminal}));
    follower.text(terminal, "ISOLATED-MARKER");
    owner.ok(
        json!({"op":"input","host":setup.host,"terminal":terminal,"data":"echo AFTER-UNBLOCK\r"}),
    );
    follower.text(terminal, "AFTER-UNBLOCK");
    follower.quit();
    owner.quit();
}

fn direct_read(setup: &Setup, path: &std::path::Path) -> Result<Value, HostError> {
    let mut cfg = DesktopRingConfig::new(
        setup.owner.home.root().join("desktop"),
        "direct control".into(),
    );
    cfg.owner_retry = Duration::from_millis(20);
    cfg.backoff_unit = Duration::from_millis(10);
    let ring = DesktopRing::open(cfg, Arc::new(Quiet));
    let manager = Arc::new(Manager::new(ManagerConfig::new(
        "1.5.0",
        Arc::new(NoSsh),
        Arc::new(NoSsh),
        Arc::new(Events::default()),
    )));
    ring.attach_manager(&manager).unwrap();
    wait("direct control usable", || {
        manager
            .host(&setup.host)
            .is_some_and(|h| h.status().usable())
    });
    let h = manager.host(&setup.host).unwrap();
    let result = reply(|w| h.call("read_text_file".into(), json!({"path":path}), w));
    ring.quit();
    manager.shutdown();
    result
}
#[test]
fn default_transport_resource_refusal_is_not_hidden_or_truncated_by_broker() {
    let setup = Setup::new();
    let path = setup.remote.home.root().join("over-default-budget.txt");
    std::fs::write(&path, "D".repeat(20 * 1024 * 1024)).unwrap();
    let direct = direct_read(&setup, &path).unwrap_err();
    assert_eq!(direct.code, xshell_hostlink::HostErrorCode::Offline);
    let mut owner = setup.process();
    let mut follower = setup.process();
    let broker = follower.command(
        json!({"op":"call","host":setup.host,"method":"read_text_file","params":{"path":path}}),
    );
    assert!(
        broker.get("result").is_none(),
        "resource refusal must not become empty/truncated success"
    );
    assert_eq!(broker["error"]["code"], "indeterminate");
    follower.quit();
    owner.quit();
}

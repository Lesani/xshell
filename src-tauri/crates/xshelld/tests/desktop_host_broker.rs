#![cfg(unix)]
//! Separate Desktop processes, one private profile, production workerd and real Daemons.
mod common;
#[path = "desktop/relay_fixture.rs"]
#[allow(dead_code)]
mod fixture;
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

struct OwnedSocketFactory(std::path::PathBuf);
impl xshell_hostlink::TransportFactory for OwnedSocketFactory {
    fn for_host(&self, _: &xshell_hostlink::HostConfig) -> Box<dyn xshell_hostlink::Transport> {
        panic!("owned socket control must not use SSH")
    }
    fn direct(
        &self,
        cfg: &xshell_hostlink::HostConfig,
    ) -> Option<Box<dyn xshell_hostlink::Dialer>> {
        Some(Box::new(xshell_hostlink::UnixSocketDialer {
            path: if cfg.id == "local" {
                self.0.clone()
            } else {
                cfg.ssh_target.clone().into()
            },
        }))
    }
}
struct ChildTransports {
    local: Option<std::path::PathBuf>,
    saved: AtomicBool,
}
impl xshell_hostlink::TransportFactory for ChildTransports {
    fn for_host(&self, _: &xshell_hostlink::HostConfig) -> Box<dyn xshell_hostlink::Transport> {
        panic!("broker test must not construct SSH")
    }
    fn direct(
        &self,
        cfg: &xshell_hostlink::HostConfig,
    ) -> Option<Box<dyn xshell_hostlink::Dialer>> {
        let path = if cfg.id == "local" {
            self.local.clone().expect("unexpected Local probe")
        } else {
            assert!(self.saved.load(Ordering::Acquire));
            assert_eq!(cfg.id, "h_aaaaaaaa");
            std::path::PathBuf::from(&cfg.ssh_target)
        };
        Some(Box::new(xshell_hostlink::UnixSocketDialer { path }))
    }
}
fn local_config() -> xshell_hostlink::HostConfig {
    xshell_hostlink::HostConfig {
        id: "local".into(),
        name: "Local".into(),
        ssh_target: "local".into(),
        color: None,
        daemon_command: None,
        launch_prefixes: Default::default(),
    }
}

#[derive(Default)]
struct Output {
    bytes: Mutex<Vec<u8>>,
    exits: Mutex<Vec<(i32, u64)>>,
    resets: Mutex<usize>,
    resyncs: Mutex<usize>,
    blocked: Mutex<bool>,
    cv: Condvar,
}
impl TermSink for Output {
    fn data(&self, bytes: &[u8]) -> bool {
        let mut blocked = self.blocked.lock().unwrap();
        while *blocked {
            blocked = self.cv.wait(blocked).unwrap();
        }
        drop(blocked);
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        true
    }
    fn exit(&self, code: i32, bytes: u64) {
        self.exits.lock().unwrap().push((code, bytes));
    }
    fn generation_reset(&self) {
        *self.resets.lock().unwrap() += 1;
    }
    fn resync_required(&self) {
        *self.resyncs.lock().unwrap() += 1;
    }
}
fn emit(value: Value) {
    println!("BROKER:{value}");
    std::io::stdout().flush().unwrap();
}
#[test]
fn broker_process() {
    let Ok(profile) = std::env::var("XSHELL_BROKER_TEST_PROFILE") else {
        return;
    };
    let mut cfg = DesktopRingConfig::new(profile.into(), "same profile".into());
    cfg.owner_retry = Duration::from_millis(20);
    cfg.backoff_unit = Duration::from_millis(10);
    let ring = DesktopRing::open(cfg, Arc::new(Quiet));
    let local_socket =
        std::env::var_os("XSHELL_BROKER_TEST_LOCAL_SOCKET").map(std::path::PathBuf::from);
    let events = Arc::new(Events::default());
    let transports = Arc::new(ChildTransports {
        local: local_socket.clone(),
        saved: AtomicBool::new(false),
    });
    let mut cfg = ManagerConfig::new("1.5.0", transports.clone(), Arc::new(NoSsh), events.clone());
    cfg.backoff_unit = Duration::from_millis(
        std::env::var("XSHELL_BROKER_TEST_BACKOFF_MS")
            .ok()
            .map(|v| v.parse().unwrap())
            .unwrap_or(10),
    );
    if let Ok(ms) = std::env::var("XSHELL_BROKER_TEST_HELLO_MS") {
        cfg.hello_timeout = Duration::from_millis(ms.parse().unwrap());
    }
    let manager = Arc::new(Manager::new(cfg));
    if local_socket.is_some() {
        manager.set_local(local_config());
        wait("child Local usable", || {
            manager.host("local").unwrap().status().usable()
        });
        let h = manager.host("local").unwrap();
        reply(|w| h.ring_identity(h.link_generation(), w)).unwrap();
    }
    ring.attach_manager(&manager).unwrap();
    emit(json!({"ready": true, "pid": std::process::id()}));
    let mut outputs = HashMap::<Uuid, Arc<Output>>::new();
    let mut host_sync: Option<&'static xshell_hostlink::ring::HostSync> = None;
    let mut _sync_worker = None;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let command: Value = serde_json::from_str(&line).unwrap();
        let id = command["id"].as_u64().unwrap();
        let op = command["op"].as_str().unwrap();
        if op == "quit" {
            if let Some(sync) = host_sync {
                sync.shutdown();
            }
            ring.quit();
            manager.shutdown();
            emit(json!({"id": id, "result": null}));
            return;
        }
        if op == "configure-socket" {
            if host_sync.is_none() {
                let sync = Box::leak(Box::new(xshell_hostlink::ring::HostSync::new()));
                *events.sync.lock().unwrap() = Some(sync);
                let worker = xshell_hostlink::ring::SyncWorker::new(
                    ring.clone(),
                    sync,
                    Box::new({
                        let m = manager.clone();
                        move |id| m.sync_host(id)
                    }),
                    Box::new(|| {}),
                );
                worker.start().unwrap();
                host_sync = Some(sync);
                _sync_worker = Some(worker);
            }
            transports.saved.store(true, Ordering::Release);
            manager
                .configure(vec![xshell_hostlink::HostConfig {
                    id: "h_aaaaaaaa".into(),
                    name: "Saved".into(),
                    ssh_target: command["socket"].as_str().unwrap().into(),
                    color: None,
                    daemon_command: None,
                    launch_prefixes: Default::default(),
                }])
                .unwrap();
            let alias = command["alias"].as_str().unwrap();
            wait("configured canonical proof", || {
                manager.host(alias).is_some_and(|h| h.id() == "h_aaaaaaaa")
                    && manager
                        .access(alias)
                        .is_some_and(|h| h.status().host == "h_aaaaaaaa")
            });
            emit(json!({"id":id,"result":null}));
            continue;
        }
        if op == "observed" {
            emit(
                json!({"id":id,"result":events.statuses.lock().unwrap().values().cloned().collect::<Vec<_>>()}),
            );
            continue;
        }
        if op == "snapshot" {
            emit(
                json!({"id": id, "result": {"hosts": manager.available_hosts(), "snapshots": manager.snapshot(), "owner": ring.pairing_ipc_owner(), "rawLocalConfig":manager.host("local").map(|h|h.status().config_generation),"accessible":manager.available_hosts().iter().flat_map(|h|h.aliases.iter()).filter(|id|manager.access(id).is_some()).cloned().collect::<Vec<_>>()}}),
            );
            continue;
        }
        let terminal = command
            .get("terminal")
            .and_then(Value::as_str)
            .map(|s| Uuid::parse_str(s).unwrap());
        if op == "unblock" {
            let output = &outputs[&terminal.unwrap()];
            *output.blocked.lock().unwrap() = false;
            output.cv.notify_all();
            emit(json!({"id":id,"result":null}));
            continue;
        }
        if op == "output" {
            let output = &outputs[&terminal.unwrap()];
            let (bytes, len) = {
                let bytes = output.bytes.lock().unwrap();
                (
                    if command["tail"] == true {
                        bytes[bytes.len().saturating_sub(65536)..].to_vec()
                    } else {
                        bytes.clone()
                    },
                    bytes.len(),
                )
            };
            emit(
                json!({"id": id, "result": {"text": String::from_utf8_lossy(&bytes), "len": len, "exits": *output.exits.lock().unwrap(), "resets": *output.resets.lock().unwrap(), "resyncs": *output.resyncs.lock().unwrap()}}),
            );
            continue;
        }
        let host = command["host"].as_str().unwrap();
        let Some(h) = manager.access(host) else {
            emit(json!({"id":id,"error":{"code":"unknown-host"}}));
            continue;
        };
        let done = move |result: Result<Value, HostError>| match result {
            Ok(result) => emit(json!({"id":id,"result":result})),
            Err(error) => emit(json!({"id":id,"error":error})),
        };
        match op {
            "kick" => {
                if let HostAccess::Broker(proxy) = h {
                    proxy.kick_with_reply(Box::new(done));
                } else {
                    h.kick();
                    done(Ok(Value::Null));
                }
            }
            "call" => h.call(
                command["method"].as_str().unwrap().into(),
                command["params"].clone(),
                Box::new(done),
            ),
            "start-call" => {
                if let HostAccess::Broker(proxy) = h {
                    let request = proxy.call_cancellable(
                        command["method"].as_str().unwrap().into(),
                        command["params"].clone(),
                        Box::new(move |r| match r {
                            Ok(result) => emit(json!({"id":id+1000000,"result":result})),
                            Err(error) => emit(json!({"id":id+1000000,"error":error})),
                        }),
                    );
                    emit(json!({"id":id,"result":request}));
                } else {
                    panic!("Relay escaped broker")
                }
            }
            "cancel" => {
                if let HostAccess::Broker(proxy) = h {
                    proxy.cancel(command["request"].as_u64().unwrap());
                    done(Ok(Value::Null));
                } else {
                    panic!("Relay escaped broker")
                }
            }
            "cancelled-call" => {
                if let HostAccess::Broker(proxy) = h {
                    let request = proxy.call_cancellable(
                        command["method"].as_str().unwrap().into(),
                        command["params"].clone(),
                        Box::new(done),
                    );
                    proxy.cancel(request);
                } else {
                    panic!("Relay access escaped broker");
                }
            }
            "open" => {
                let spec: OpenSpec = serde_json::from_value(command["spec"].clone()).unwrap();
                let sink = Arc::new(Output::default());
                outputs.insert(spec.terminal, sink.clone());
                h.term_open(
                    spec,
                    sink,
                    Box::new(move |r| done(r.map(|r| serde_json::to_value(r).unwrap()))),
                );
            }
            "attach" => {
                let terminal = terminal.unwrap();
                let sink = Arc::new(Output::default());
                *sink.blocked.lock().unwrap() = command["slow"] == true;
                outputs.insert(terminal, sink.clone());
                h.term_attach(
                    terminal,
                    sink,
                    Box::new(move |r| done(r.map(|exit| json!({"exitCode":exit})))),
                );
            }
            "input" => done(
                h.term_input(
                    terminal.unwrap(),
                    command["data"].as_str().unwrap().into(),
                    None,
                    command
                        .get("target")
                        .map(|target| serde_json::from_value(target.clone()).unwrap()),
                )
                .map(|_| Value::Null),
            ),
            "resize" => done(
                h.term_resize(
                    terminal.unwrap(),
                    command["cols"].as_u64().unwrap() as u16,
                    command["rows"].as_u64().unwrap() as u16,
                )
                .map(|_| Value::Null),
            ),
            "detach" => done(h.term_detach(terminal.unwrap()).map(|_| Value::Null)),
            "close" => h.term_close(terminal.unwrap(), Box::new(done)),
            _ => panic!("unknown test command {op}"),
        }
    }
    ring.quit();
    manager.shutdown();
}
struct Process {
    child: Child,
    input: ChildStdin,
    replies: mpsc::Receiver<Value>,
    next: u64,
    deferred: HashMap<u64, Value>,
}
impl Process {
    fn start(profile: &std::path::Path) -> Self {
        Self::start_with_local(profile, None)
    }
    fn start_with_local(profile: &std::path::Path, local: Option<&std::path::Path>) -> Self {
        Self::start_options(profile, local, None)
    }
    fn start_options(
        profile: &std::path::Path,
        local: Option<&std::path::Path>,
        timing: Option<(u64, u64)>,
    ) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "broker_process", "--nocapture"])
            .env("XSHELL_BROKER_TEST_PROFILE", profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(local) = local {
            command.env("XSHELL_BROKER_TEST_LOCAL_SOCKET", local);
        }
        if let Some((hello, backoff)) = timing {
            command
                .env("XSHELL_BROKER_TEST_HELLO_MS", hello.to_string())
                .env("XSHELL_BROKER_TEST_BACKOFF_MS", backoff.to_string());
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, replies) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Some(line) = line.strip_prefix("BROKER:") {
                    if tx.send(serde_json::from_str(line).unwrap()).is_err() {
                        break;
                    }
                }
            }
        });
        assert_eq!(replies.recv_timeout(WAIT).unwrap()["ready"], true);
        Self {
            child,
            input,
            replies,
            next: 0,
            deferred: HashMap::new(),
        }
    }
    fn send(&mut self, mut command: Value) -> u64 {
        self.next += 1;
        command["id"] = json!(self.next);
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        self.next
    }
    fn receive(&mut self, id: u64) -> Value {
        if let Some(reply) = self.deferred.remove(&id) {
            return reply;
        }
        loop {
            let reply = self
                .replies
                .recv_timeout(Duration::from_secs(60))
                .expect("child reply");
            let got = reply["id"].as_u64().unwrap();
            if got == id {
                return reply;
            }
            self.deferred.insert(got, reply);
        }
    }
    fn command(&mut self, command: Value) -> Value {
        let id = self.send(command);
        self.receive(id)
    }
    fn ok(&mut self, command: Value) -> Value {
        let reply = self.command(command);
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
    fn usable(&mut self, host: &str) {
        let until = Instant::now() + WAIT;
        loop {
            let snapshot = self.ok(json!({"op":"snapshot"}));
            let representative = snapshot["hosts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| {
                    h["aliases"]
                        .as_array()
                        .is_some_and(|aliases| aliases.iter().any(|alias| alias == host))
                })
                .map(|h| h["id"].clone())
                .unwrap_or(json!(host));
            if snapshot["accessible"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == host)
                && snapshot["snapshots"].as_array().unwrap().iter().any(|s| {
                    s["status"]["host"] == representative && s["status"]["status"] == "connected"
                })
            {
                return;
            }
            assert!(Instant::now() < until, "not usable: {snapshot}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn text(&mut self, terminal: Uuid, needle: &str) -> Value {
        let until = Instant::now() + WAIT;
        loop {
            let output = self.ok(json!({"op":"output","terminal":terminal}));
            if output["text"].as_str().unwrap().contains(needle) {
                return output;
            }
            assert!(Instant::now() < until, "missing {needle}: {output}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn quit(&mut self) {
        self.ok(json!({"op":"quit"}));
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn target_snapshot<'a>(snapshot: &'a Value, host: &str) -> (&'a Value, &'a Value) {
    let row = snapshot["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|alias| alias == host)
        })
        .expect("verified target descriptor");
    let status = &snapshot["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["status"]["host"] == row["id"])
        .expect("target status")["status"];
    (row, status)
}
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
    fn with_config(config: impl FnOnce(&mut xshelld::server::Config)) -> Self {
        let relay = Relay::start();
        let owner = Computer::new(&relay);
        let created = owner.create();
        let original = created
            .head()
            .roster()
            .members
            .iter()
            .find(|member| member.role == xshell_protocol::ring::Role::Daemon)
            .unwrap()
            .sign_key;
        let remote = Computer::with_config(&relay, config);
        let joined = remote.join(&owner);
        let key = joined
            .head()
            .roster()
            .members
            .iter()
            .find(|member| {
                member.role == xshell_protocol::ring::Role::Daemon && member.sign_key != original
            })
            .unwrap()
            .sign_key;
        owner.relay_host(key);
        let host = xshell_hostlink::ring::hosts::relay_host_id(&key);
        // Keep both publicly enrolled Daemons and their retained identities, but retire
        // fixture Desktops so only the tested profile contributes Host connections.
        remote.ring.quit();
        remote.manager.shutdown();
        owner.ring.quit();
        owner.manager.shutdown();
        wait("fixture Desktop Host Links drained", || {
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
    fn spec(
        &self,
        terminal: Uuid,
        script: &str,
    ) -> (OpenSpec, std::path::PathBuf, common::FakeReaper) {
        let project = self.remote.home.project("broker-project");
        let fake = common::Fake::in_dir(&project);
        let reaper = common::FakeReaper(fake.pids_log.clone());
        let launches = self.remote.home.root().join("broker-launches");
        std::fs::write(
            project.join("agent.sh"),
            format!("echo launch >> '{}'; {script}", launches.display()),
        )
        .unwrap();
        (
            OpenSpec {
                terminal,
                launch: common::claude_spec(&project, None),
                cols: 80,
                rows: 24,
                meta: Default::default(),
                first_message: None,
                adopt_existing: false,
            },
            launches,
            reaper,
        )
    }
}
#[test]
fn owner_and_follower_discover_with_one_connector_session_and_host_link() {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    assert_eq!(owner.ok(json!({"op":"snapshot"}))["owner"], true);
    assert_eq!(follower.ok(json!({"op":"snapshot"}))["owner"], false);
    for process in [&mut owner, &mut follower] {
        let result = process.ok(json!({"op":"call","host":setup.host,"method":"list_dir","params":{"path":setup.remote.home.home()}}));
        assert!(result.is_array());
    }
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &setup.relay.target(),
            setup.owner.ring.chain().unwrap().ring_id()
        ),
        2
    );
    assert_eq!(
        setup.remote.daemon.as_ref().unwrap().connections(),
        1,
        "followers must not create another Noise session/Host Link"
    );
    follower.quit();
    owner.quit();
}
#[test]
fn open_shared_sink_replay_independent_detach_and_viewer_size_arbitration() {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let terminal = Uuid::new_v4();
    let (spec, launches, _reaper) = setup.spec(terminal, "echo READY; while IFS= read -r line; do if [ \"$line\" = size ]; then stty size; elif [ \"$line\" = end ]; then exit 7; else echo REPLY:$line; fi; done");
    let opened = owner.ok(json!({"op":"open","host":setup.host,"spec":spec}));
    assert!(opened["pid"].as_u64().is_some());
    owner.text(terminal, "READY");
    follower.ok(json!({"op":"attach","host":setup.host,"terminal":terminal}));
    follower.text(terminal, "READY");
    owner.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"both\n"}));
    owner.text(terminal, "REPLY:both");
    follower.text(terminal, "REPLY:both");
    follower.ok(json!({"op":"resize","host":setup.host,"terminal":terminal,"cols":91,"rows":31}));
    follower.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"size\n"}));
    owner.text(terminal, "31 91");
    owner.ok(json!({"op":"resize","host":setup.host,"terminal":terminal,"cols":82,"rows":22}));
    owner.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"size\n"}));
    follower.text(terminal, "22 82");
    owner.ok(json!({"op":"detach","host":setup.host,"terminal":terminal}));
    follower.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"size\n"}));
    follower.text(terminal, "31 91");
    follower.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"end\n"}));
    let until = Instant::now() + WAIT;
    loop {
        let out = follower.ok(json!({"op":"output","terminal":terminal}));
        if !out["exits"].as_array().unwrap().is_empty() {
            assert_eq!(out["exits"][0][0], 7);
            assert_eq!(out["exits"][0][1], out["len"]);
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(launches).unwrap().lines().count(),
        1
    );
    follower.quit();
    owner.quit();
}
fn handover(forced: bool) {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let terminal = Uuid::new_v4();
    let (spec, launches, _reaper) = setup.spec(
        terminal,
        "echo READY; while IFS= read -r line; do echo REPLY:$line; done",
    );
    let opened = owner.ok(json!({"op":"open","host":setup.host,"spec":spec}));
    follower.ok(json!({"op":"attach","host":setup.host,"terminal":terminal}));
    follower.text(terminal, "READY");
    let before = follower.ok(json!({"op":"snapshot"}));
    let (_, old_status) = target_snapshot(&before, &setup.host);
    let old_target = json!({"configGeneration":old_status["configGeneration"],"linkGeneration":old_status["linkGeneration"]});
    if forced {
        owner.child.kill().unwrap();
        owner.child.wait().unwrap();
    } else {
        owner.quit();
    }
    let until = Instant::now() + WAIT;
    loop {
        let s = follower.ok(json!({"op":"snapshot"}));
        if s["owner"] == true
            && s["snapshots"].as_array().unwrap().iter().any(|s| {
                s["terminals"].as_array().is_some_and(|list| {
                    list.iter()
                        .any(|t| t["terminal"] == terminal.to_string() && t["pid"] == opened["pid"])
                })
            })
        {
            break;
        }
        assert!(Instant::now() < until, "handover failed: {s}");
        std::thread::sleep(Duration::from_millis(20));
    }
    follower.usable(&setup.host);
    let until = Instant::now() + WAIT;
    loop {
        let out = follower.ok(json!({"op":"output","terminal":terminal}));
        if out["resets"].as_u64().unwrap() > 0
            && out["text"].as_str().unwrap().matches("READY").count() >= 2
        {
            break;
        }
        assert!(Instant::now() < until, "subscription not restored: {out}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let stale=follower.command(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"stale-must-not-run\n","target":old_target}));
    assert_eq!(stale["error"]["code"], "offline");
    let now = follower.ok(json!({"op":"snapshot"}));
    let (_, status) = target_snapshot(&now, &setup.host);
    let target = json!({"configGeneration":status["configGeneration"],"linkGeneration":status["linkGeneration"]});
    follower.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"after-handover\n","target":target}));
    follower.text(terminal, "REPLY:after-handover");
    assert_eq!(
        std::fs::read_to_string(launches).unwrap().lines().count(),
        1
    );
    follower.ok(json!({"op":"close","host":setup.host,"terminal":terminal}));
    follower.quit();
}
#[test]
fn clean_owner_exit_hands_over_same_remote_uuid_pid_without_open_replay() {
    handover(false)
}
#[test]
fn forced_owner_loss_hands_over_same_remote_uuid_pid_without_open_replay() {
    handover(true)
}
#[test]
fn colliding_client_request_ids_cancel_and_long_calls_are_isolated() {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let fifo = setup.remote.home.root().join("slow-read");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let request = owner.send(
        json!({"op":"call","host":setup.host,"method":"read_text_file","params":{"path":fifo}}),
    );
    let fifo2 = setup.remote.home.root().join("cancelled-read");
    let c = std::ffi::CString::new(fifo2.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let start_id=follower.send(json!({"op":"start-call","host":setup.host,"method":"read_text_file","params":{"path":fifo2}}));
    let request_id = follower.receive(start_id)["result"].as_u64().unwrap();
    use std::os::fd::FromRawFd;
    let until = Instant::now() + WAIT;
    let writer = loop {
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            break unsafe { std::fs::File::from_raw_fd(fd) };
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    };
    follower.ok(json!({"op":"cancel","host":setup.host,"request":request_id}));
    let cancelled = follower.receive(start_id + 1000000);
    assert_eq!(cancelled["error"]["code"], "indeterminate");
    follower.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    std::thread::sleep(Duration::from_secs(6));
    let path = fifo.clone();
    std::thread::spawn(move || {
        std::fs::write(path, "past-companion-budget").unwrap();
    });
    assert_eq!(owner.receive(request)["result"], "past-companion-budget");
    let mut writer = writer;
    writer.write_all(b"cancelled").unwrap();
    drop(writer);
    follower.quit();
    owner.quit();
}
#[test]
fn reconnect_reset_precedes_replay_and_keeps_other_viewer() {
    let mut setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let terminal = Uuid::new_v4();
    let (spec, launches, _reaper) = setup.spec(
        terminal,
        "printf '\\033[?2004hREADY'; while IFS= read -r line; do echo REPLY:$line; done",
    );
    owner.ok(json!({"op":"open","host":setup.host,"spec":spec}));
    follower.ok(json!({"op":"attach","host":setup.host,"terminal":terminal}));
    follower.text(terminal, "READY");
    setup.relay.restart();
    std::thread::sleep(Duration::from_millis(400));
    follower.usable(&setup.host);
    let until = Instant::now() + WAIT;
    loop {
        let out = follower.ok(json!({"op":"output","terminal":terminal}));
        if out["resets"].as_u64().unwrap() > 0 {
            break;
        }
        assert!(Instant::now() < until, "no reset: {out}");
        std::thread::sleep(Duration::from_millis(20));
    }
    follower.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"reset-ok\n"}));
    owner.text(terminal, "REPLY:reset-ok");
    follower.text(terminal, "REPLY:reset-ok");
    assert_eq!(
        std::fs::read_to_string(launches).unwrap().lines().count(),
        1
    );
    follower.ok(json!({"op":"close","host":setup.host,"terminal":terminal}));
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
fn admitted_maximum_legal_rpc_matches_direct_link_with_explicit_transport_budget() {
    // Existing production budgets are not enlarged by the broker. This owned Server's
    // existing explicit budget admits a maximum legal Host frame for the framing control.
    let setup = Setup::with_config(|cfg| cfg.conn_total_cap = 80 * 1024 * 1024);
    let path = setup.remote.home.root().join("maximum.txt");
    let len = 64 * 1024 * 1024 - 4096;
    std::fs::write(&path, "L".repeat(len)).unwrap();
    let direct = direct_read(&setup, &path).unwrap();
    assert_eq!(direct.as_str().unwrap().len(), len);
    drop(direct);
    let mut owner = setup.process();
    let mut follower = setup.process();
    let broker = follower.ok(
        json!({"op":"call","host":setup.host,"method":"read_text_file","params":{"path":path}}),
    );
    assert_eq!(broker.as_str().unwrap().len(), len);
    assert!(broker.as_str().unwrap().bytes().all(|b| b == b'L'));
    drop(broker);
    follower.quit();
    owner.quit();
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

#[test]
fn lost_mutation_reply_is_indeterminate_and_open_is_never_replayed() {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (seen, gate) = (entered.clone(), release.clone());
    let setup = Setup::with_config(move |cfg| {
        cfg.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
            if point == xshelld::server::TestPoint::StartInput {
                seen.store(true, Ordering::Release);
                let (lock, cv) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
            }
            false
        })))
    });
    let mut owner = setup.process();
    let mut follower = setup.process();
    let terminal = Uuid::new_v4();
    let pidfile = setup.remote.home.root().join("lost-open-pid");
    let (spec, launches, _reaper) = setup.spec(
        terminal,
        &format!(
            "echo $$ > '{}'; echo READY; while IFS= read -r line; do echo REPLY:$line; done",
            pidfile.display()
        ),
    );
    let request = follower.send(json!({"op":"open","host":setup.host,"spec":spec}));
    wait("real mutation performed before its reply", || {
        entered.load(Ordering::Acquire) && launches.exists() && pidfile.exists()
    });
    owner.child.kill().unwrap();
    owner.child.wait().unwrap();
    let reply = follower.receive(request);
    assert_eq!(reply["error"]["code"], "indeterminate", "{reply}");
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    follower.usable(&setup.host);
    let pid = std::fs::read_to_string(pidfile)
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    let until = Instant::now() + WAIT;
    loop {
        let s = follower.ok(json!({"op":"snapshot"}));
        if s["snapshots"].as_array().unwrap().iter().any(|s| {
            s["terminals"].as_array().is_some_and(|ts| {
                ts.iter()
                    .any(|t| t["terminal"] == terminal.to_string() && t["pid"] == pid)
            })
        }) {
            break;
        }
        assert!(Instant::now() < until, "lost mutation not reconciled: {s}");
        std::thread::sleep(Duration::from_millis(20));
    }
    follower.text(terminal, "READY");
    assert_eq!(
        std::fs::read_to_string(launches).unwrap().lines().count(),
        1
    );
    follower.ok(json!({"op":"close","host":setup.host,"terminal":terminal}));
    follower.quit();
}
#[test]
fn slow_viewer_resync_does_not_stall_other_input_or_mutation_reply() {
    let setup = Setup::new();
    let mut owner = setup.process();
    let mut follower = setup.process();
    let terminal = Uuid::new_v4();
    let (spec,_launches,_reaper)=setup.spec(terminal,"echo READY; while IFS= read -r line; do if [ \"$line\" = flood ]; then i=0; while [ $i -lt 192 ]; do head -c 65536 /dev/zero | tr '\\000' x; sleep .1; i=$((i+1)); done; printf '\\nFLOOD-END\\n'; else echo REPLY:$line; fi; done");
    owner.ok(json!({"op":"open","host":setup.host,"spec":spec}));
    owner.text(terminal, "READY");
    follower.ok(json!({"op":"attach","host":setup.host,"terminal":terminal,"slow":true}));
    owner.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"flood\n"}));
    let dir = setup.remote.home.root().join("mutation-during-slow-viewer");
    follower.ok(json!({"op":"call","host":setup.host,"method":"make_dir","params":{"path":dir}}));
    assert!(dir.is_dir());
    let until = Instant::now() + Duration::from_secs(40);
    loop {
        let out = owner.ok(json!({"op":"output","terminal":terminal,"tail":true}));
        if out["text"].as_str().unwrap().contains("FLOOD-END") {
            assert_eq!(out["resyncs"], 0);
            break;
        }
        assert!(Instant::now() < until, "fast viewer stalled: {out}");
        std::thread::sleep(Duration::from_millis(20));
    }
    follower.ok(json!({"op":"unblock","terminal":terminal}));
    let until = Instant::now() + WAIT;
    loop {
        let out = follower.ok(json!({"op":"output","terminal":terminal,"tail":true}));
        if out["resyncs"].as_u64().unwrap() > 0 {
            break;
        }
        assert!(
            Instant::now() < until,
            "slow viewer was not explicitly resynced: {out}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    follower.ok(json!({"op":"attach","host":setup.host,"terminal":terminal}));
    follower.text(terminal, "FLOOD-END");
    follower.ok(json!({"op":"input","host":setup.host,"terminal":terminal,"data":"after-slow\n"}));
    let until = Instant::now() + Duration::from_secs(40);
    loop {
        let out = owner.ok(json!({"op":"output","terminal":terminal,"tail":true}));
        if out["text"].as_str().unwrap().contains("REPLY:after-slow") {
            assert_eq!(out["resets"], 0);
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    follower.ok(json!({"op":"close","host":setup.host,"terminal":terminal}));
    follower.quit();
    owner.quit();
}
#[test]
fn wrong_profile_epoch_identity_or_framing_cannot_forward_host_requests() {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let setup = Setup::new();
    let mut owner = setup.process();
    let dir = setup
        .owner
        .home
        .root()
        .join("desktop")
        .canonicalize()
        .unwrap();
    let path = dir.join("host-broker.sock");
    let profile = format!("{:x}", Sha256::digest(dir.as_os_str().as_encoded_bytes()));
    let send = |stream: &mut std::os::unix::net::UnixStream, value: Value| {
        let b = serde_json::to_vec(&value).unwrap();
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
    };
    let read = |stream: &mut std::os::unix::net::UnixStream| {
        let mut h = [0; 4];
        stream.read_exact(&mut h).unwrap();
        let mut key = [0; 8];
        stream.read_exact(&mut key).unwrap();
        let mut b = vec![0; u32::from_be_bytes(h) as usize];
        stream.read_exact(&mut b).unwrap();
        serde_json::from_slice::<Value>(&b).unwrap()
    };
    let connect = || {
        let s = xshell_core::local_ipc::protected_connect(&path, Duration::from_secs(2)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s
    };
    let mut wrong = connect();
    send(
        &mut wrong,
        json!({"type":"hello","version":1,"profile_id":"foreign-profile"}),
    );
    let mut byte = [0];
    assert_eq!(wrong.read(&mut byte).unwrap(), 0);
    let mut malformed = connect();
    malformed.write_all(&u32::MAX.to_be_bytes()).unwrap();
    assert_eq!(malformed.read(&mut byte).unwrap(), 0);
    let mut stale = connect();
    send(
        &mut stale,
        json!({"type":"hello","version":1,"profile_id":profile}),
    );
    let welcome = read(&mut stale);
    assert_eq!(welcome["type"], "welcome");
    let snapshot = read(&mut stale);
    let (row, status) = target_snapshot(&snapshot, &setup.host);
    let forbidden = setup.remote.home.root().join("must-not-exist");
    let mut request = json!({"type":"request","owner_epoch":Uuid::new_v4(),"request_id":1,"host":setup.host,"canonical":row["daemonKey"],"expected":{"configGeneration":status["configGeneration"],"linkGeneration":status["linkGeneration"]},"operation":{"operation":"call","method":"make_dir","params":{"path":forbidden}}});
    send(&mut stale, request.clone());
    let mut trailing = Vec::new();
    stale.read_to_end(&mut trailing).unwrap();
    assert!(!forbidden.exists());
    let mut identity = connect();
    send(
        &mut identity,
        json!({"type":"hello","version":1,"profile_id":profile}),
    );
    let welcome = read(&mut identity);
    read(&mut identity);
    request["owner_epoch"] = welcome["owner_epoch"].clone();
    request["canonical"] = json!(xshell_protocol::ring::DeviceKeys::generate()
        .unwrap()
        .sign_key());
    send(&mut identity, request);
    loop {
        let event = read(&mut identity);
        if event["type"] == "reply" {
            assert!(event.get("result").is_some());
            assert!(event["result"].get("Err").is_some(), "{event}");
            break;
        }
    }
    assert!(!forbidden.exists());
    owner.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    owner.quit();
}

fn direct_and_alias_overlap(direct_first: bool) {
    direct_and_alias_overlap_with_targets(direct_first, false, false)
}
fn direct_and_alias_overlap_with_targets(
    direct_first: bool,
    public_targets: bool,
    follower_local: bool,
) {
    use xshell_hostlink::{HostConfig, Transport, TransportFactory};
    struct LocalOnly(std::path::PathBuf);
    impl TransportFactory for LocalOnly {
        fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
            panic!("Local must not use SSH")
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
    let precreated = if public_targets {
        let chain = owner.create();
        let key = chain
            .head()
            .roster()
            .members
            .iter()
            .find(|m| m.role == xshell_protocol::ring::Role::Daemon)
            .unwrap()
            .sign_key;
        let id = xshell_hostlink::ring::hosts::relay_host_id(&key);
        wait(
            "owned Relay handle allocates a distinct configuration generation",
            || owner.manager.host(&id).is_some(),
        );
        Some(chain)
    } else {
        None
    };
    owner.manager.set_local(HostConfig {
        id: "local".into(),
        name: "Local".into(),
        ssh_target: "local".into(),
        color: None,
        daemon_command: None,
        launch_prefixes: Default::default(),
    });
    wait("Local usable", || {
        owner.manager.host("local").unwrap().status().usable()
    });
    let direct = owner.manager.access("local").unwrap();
    assert!(matches!(direct, HostAccess::Direct(_)));
    let project = owner.home.project("overlap");
    let fake = common::Fake::in_dir(&project);
    let _reaper = common::FakeReaper(fake.pids_log.clone());
    let launches = owner.home.root().join("overlap-launches");
    std::fs::write(project.join("agent.sh"),format!("echo launch >> '{}'; echo READY; while IFS= read -r line; do if [ \"$line\" = size ]; then stty size; else echo REPLY:$line; fi; done",launches.display())).unwrap();
    let terminal = Uuid::new_v4();
    let spec = OpenSpec {
        terminal,
        launch: common::claude_spec(&project, None),
        cols: 80,
        rows: 24,
        meta: Default::default(),
        first_message: None,
        adopt_existing: false,
    };
    let direct_sink = Arc::new(Output::default());
    if direct_first {
        reply(|w| direct.term_open(spec.clone(), direct_sink.clone(), w)).unwrap();
        wait("Direct before association output", || {
            String::from_utf8_lossy(&direct_sink.bytes.lock().unwrap()).contains("READY")
        });
    }
    let chain = precreated.unwrap_or_else(|| owner.create());
    let key = chain
        .head()
        .roster()
        .members
        .iter()
        .find(|m| m.role == xshell_protocol::ring::Role::Daemon)
        .unwrap()
        .sign_key;
    let alias = xshell_hostlink::ring::hosts::relay_host_id(&key);
    let root = owner.manager.host("local").unwrap();
    reply(|w| root.ring_identity(root.link_generation(), w)).unwrap();
    wait("owner broker alias proof", || {
        owner.manager.access(&alias).is_some()
    });
    let mut follower = Process::start_with_local(
        &owner.home.root().join("desktop"),
        follower_local.then_some(owner.home.paths().socket.as_path()),
    );
    follower.usable(&alias);
    if direct_first {
        follower.ok(json!({"op":"attach","host":alias,"terminal":terminal}));
    } else {
        follower.ok(json!({"op":"open","host":alias,"spec":spec}));
        reply(|w| direct.term_attach(terminal, direct_sink.clone(), w)).unwrap();
    }
    follower.text(terminal, "READY");
    wait("Direct output", || {
        String::from_utf8_lossy(&direct_sink.bytes.lock().unwrap()).contains("READY")
    });
    wait("only the expected Direct physical Links remain", || {
        owner.daemon.as_ref().unwrap().connections() == if follower_local { 2 } else { 1 }
    });
    assert_eq!(
        owner.daemon.as_ref().unwrap().connections(),
        if follower_local { 2 } else { 1 },
        "one physical Link for owner Direct+alias, a distinct permitted follower Direct Link"
    );
    if public_targets {
        let raw = follower.ok(json!({"op":"snapshot"}));
        assert_ne!(
            raw["rawLocalConfig"],
            json!(
                owner
                    .manager
                    .host("local")
                    .unwrap()
                    .status()
                    .config_generation
            ),
            "the two actual Direct configurations must differ"
        );
        let target_from =
            |status: &xshell_hostlink::HostStatus| xshell_hostlink::handle::InputTarget {
                config_generation: status.config_generation,
                link_generation: status.link_generation,
            };
        let status = owner
            .manager
            .snapshot()
            .into_iter()
            .find(|s| s.status.host == "local")
            .unwrap()
            .status;
        owner
            .manager
            .access(&alias)
            .unwrap()
            .term_input(
                terminal,
                "owner-target\n".into(),
                None,
                Some(target_from(&status)),
            )
            .unwrap();
        wait("owner public target reaches alias", || {
            String::from_utf8_lossy(&direct_sink.bytes.lock().unwrap())
                .contains("REPLY:owner-target")
        });
        direct
            .term_input(
                terminal,
                "direct-target\n".into(),
                None,
                Some(target_from(&status)),
            )
            .unwrap();
        let update = xshell_hostlink::handle::UpdateTarget {
            config_generation: status.config_generation,
            link_generation: status.link_generation,
            daemon_version: status.daemon_version.clone(),
            daemon_build: status.daemon_build.clone(),
        };
        let result = reply(|w| direct.upgrade_target(Some(update), w));
        assert!(
            !result
                .as_ref()
                .is_err_and(|e| e.message.contains("target changed")),
            "current public guarded update was rejected before normal eligibility: {result:?}"
        );
        let observed = owner
            .events
            .statuses
            .lock()
            .unwrap()
            .get("local")
            .unwrap()
            .clone();
        owner
            .manager
            .access(&alias)
            .unwrap()
            .term_input(
                terminal,
                "owner-observer\n".into(),
                None,
                Some(target_from(&observed)),
            )
            .unwrap();
        let snapshot = follower.ok(json!({"op":"snapshot"}));
        let (_, status) = target_snapshot(&snapshot, &alias);
        let target = json!({"configGeneration":status["configGeneration"],"linkGeneration":status["linkGeneration"]});
        follower.ok(json!({"op":"input","host":alias,"terminal":terminal,"data":"follower-target\n","target":target}));
        follower.text(terminal, "REPLY:follower-target");
        let observed = follower.ok(json!({"op":"observed"}));
        let observed = observed
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["host"] == "local")
            .unwrap();
        follower.ok(json!({"op":"input","host":alias,"terminal":terminal,"data":"follower-observer\n","target":{"configGeneration":observed["configGeneration"],"linkGeneration":observed["linkGeneration"]}}));
        follower.text(terminal, "REPLY:follower-observer");
    }
    direct.term_resize(terminal, 85, 25).unwrap();
    follower.ok(json!({"op":"input","host":alias,"terminal":terminal,"data":"size\n"}));
    follower.text(terminal, if direct_first { "25 85" } else { "24 80" });
    follower.ok(json!({"op":"resize","host":alias,"terminal":terminal,"cols":91,"rows":31}));
    follower.ok(json!({"op":"input","host":alias,"terminal":terminal,"data":"size\n"}));
    follower.text(terminal, "31 91");
    direct
        .term_input(terminal, "both\n".into(), None, None)
        .unwrap();
    follower.text(terminal, "REPLY:both");
    wait("Direct not stolen", || {
        String::from_utf8_lossy(&direct_sink.bytes.lock().unwrap()).contains("REPLY:both")
    });
    direct
        .term_input(terminal, "size\n".into(), None, None)
        .unwrap();
    wait("Direct input restores its actual size", || {
        String::from_utf8_lossy(&direct_sink.bytes.lock().unwrap())
            .matches("25 85")
            .count()
            >= if direct_first { 2 } else { 1 }
    });
    assert_eq!(
        *direct_sink.resets.lock().unwrap(),
        0,
        "another Attach must not reset Direct"
    );
    if direct_first {
        direct.term_detach(terminal).unwrap();
        follower
            .ok(json!({"op":"input","host":alias,"terminal":terminal,"data":"surviving-broker\n"}));
        follower.text(terminal, "REPLY:surviving-broker");
        reply(|w| direct.term_attach(terminal, direct_sink.clone(), w)).unwrap();
        follower.ok(json!({"op":"detach","host":alias,"terminal":terminal}));
    } else {
        follower.ok(json!({"op":"detach","host":alias,"terminal":terminal}));
    }
    direct
        .term_input(terminal, "surviving-direct\n".into(), None, None)
        .unwrap();
    wait("Direct survives alias detach", || {
        String::from_utf8_lossy(&direct_sink.bytes.lock().unwrap())
            .contains("REPLY:surviving-direct")
    });
    assert_eq!(
        std::fs::read_to_string(launches).unwrap().lines().count(),
        1
    );
    if public_targets {
        let old = owner
            .manager
            .snapshot()
            .into_iter()
            .find(|s| s.status.host == "local")
            .unwrap()
            .status;
        let old_target = xshell_hostlink::handle::InputTarget {
            config_generation: old.config_generation,
            link_generation: old.link_generation,
        };
        follower.quit();
        owner.ring.quit();
        assert!(direct
            .term_input(
                terminal,
                "stale-owner-target\n".into(),
                None,
                Some(old_target)
            )
            .is_err());
        let old_update = xshell_hostlink::handle::UpdateTarget {
            config_generation: old.config_generation,
            link_generation: old.link_generation,
            daemon_version: old.daemon_version,
            daemon_build: old.daemon_build,
        };
        let error = reply(|w| direct.upgrade_target(Some(old_update), w)).unwrap_err();
        assert!(error.message.contains("target changed"));
        let fresh = owner
            .manager
            .snapshot()
            .into_iter()
            .find(|s| s.status.host == "local")
            .unwrap()
            .status;
        direct
            .term_input(
                terminal,
                "independent-after-withdrawal\n".into(),
                None,
                Some(xshell_hostlink::handle::InputTarget {
                    config_generation: fresh.config_generation,
                    link_generation: fresh.link_generation,
                }),
            )
            .unwrap();
        wait(
            "independent Direct survives with current published token",
            || {
                String::from_utf8_lossy(&direct_sink.bytes.lock().unwrap())
                    .contains("REPLY:independent-after-withdrawal")
            },
        );
    } else {
        follower.quit();
    }
    reply(|w| direct.term_close(terminal, w)).unwrap();
}
#[test]
fn direct_before_ring_association_and_alias_follower_share_one_sink() {
    direct_and_alias_overlap(true)
}
#[test]
fn alias_open_then_direct_attach_keeps_both_viewers_and_independent_detach() {
    direct_and_alias_overlap(false)
}

#[test]
fn one_large_profile_assembly_rejects_another_request_busy_without_dispatch() {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    fn send(stream: &mut std::os::unix::net::UnixStream, value: &Value) {
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
    fn read(stream: &mut std::os::unix::net::UnixStream) -> Value {
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
    let profile = format!("{:x}", Sha256::digest(dir.as_os_str().as_encoded_bytes()));
    let connect = || {
        let mut stream = xshell_core::local_ipc::protected_connect(
            &dir.join("host-broker.sock"),
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
    let (row, status) = target_snapshot(&snapshot, &setup.host);
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

#[test]
fn protected_broker_hello_budget_is_absolute_and_does_not_end_other_streams() {
    use std::io::Read;
    let setup = Setup::new();
    let mut owner = setup.process();
    let path = setup
        .owner
        .home
        .root()
        .join("desktop")
        .canonicalize()
        .unwrap()
        .join("host-broker.sock");
    let mut stream =
        xshell_core::local_ipc::protected_connect(&path, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let began = Instant::now();
    stream.write_all(&100u32.to_be_bytes()).unwrap();
    stream.write_all(&0u64.to_be_bytes()).unwrap();
    stream.write_all(b"{").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    stream.write_all(b" ").unwrap();
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).unwrap(), 0);
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "progress must not renew Hello budget"
    );
    owner.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    owner.quit();
}

#[test]
fn public_snapshot_and_observer_targets_work_for_owner_and_distinct_follower_direct_alias_links() {
    direct_and_alias_overlap_with_targets(true, true, true);
}

#[test]
fn retargeted_configured_representative_attaches_new_canonical_uuid_without_cached_success() {
    use xshell_hostlink::{HostConfig, HostStatus};
    let relay = Relay::start();
    let owner = Computer::with_factory(
        &relay,
        |_| {},
        |home| Arc::new(OwnedSocketFactory(home.paths().socket)),
    );
    owner.ring.enable(None, false).unwrap();
    let first = Computer::new(&relay);
    let k1 = first.legacy_pair(&owner, &relay);
    let second = Computer::new(&relay);
    let k2 = second.legacy_pair(&owner, &relay);
    let sync = Box::leak(Box::new(xshell_hostlink::ring::HostSync::new()));
    *owner.events.sync.lock().unwrap() = Some(sync);
    let worker = xshell_hostlink::ring::SyncWorker::new(
        owner.ring.clone(),
        sync,
        Box::new({
            let m = owner.manager.clone();
            move |id| m.sync_host(id)
        }),
        Box::new(|| {}),
    );
    worker.start().unwrap();
    let cfg = |path: std::path::PathBuf| HostConfig {
        id: "h_aaaaaaaa".into(),
        name: "Saved".into(),
        ssh_target: path.to_string_lossy().into(),
        color: None,
        daemon_command: None,
        launch_prefixes: Default::default(),
    };
    let (r1, r2) = (
        xshell_hostlink::ring::hosts::relay_host_id(&k1),
        xshell_hostlink::ring::hosts::relay_host_id(&k2),
    );
    owner
        .manager
        .configure(vec![cfg(first.home.paths().socket)])
        .unwrap();
    wait(
        "first canonical representative and current broker snapshot",
        || {
            owner
                .manager
                .host(&r1)
                .is_some_and(|h| h.id() == "h_aaaaaaaa")
                && owner
                    .manager
                    .access(&r1)
                    .is_some_and(|h| h.status().host == "h_aaaaaaaa")
        },
    );
    let terminal = Uuid::new_v4();
    let script1 = first.home.script(
        "identity-one",
        "echo K1READY; while IFS= read -r line; do echo K1:$line; done",
    );
    let spec = |home: &common::TestHome, script: &std::path::Path| OpenSpec {
        terminal,
        launch: common::raw_spec(&home.project("canonical"), script),
        cols: 80,
        rows: 24,
        meta: Default::default(),
        first_message: None,
        adopt_existing: false,
    };
    let direct = owner.manager.access("h_aaaaaaaa").unwrap();
    let initial = Arc::new(Output::default());
    let open1 =
        reply(|w| direct.term_open(spec(&first.home, &script1), initial.clone(), w)).unwrap();
    let mut reaper = common::PidReaper(open1.pid.into_iter().map(|pid| pid as i32).collect());
    let old_alias = owner.manager.access(&r1).unwrap();
    let retained = Arc::new(Output::default());
    reply(|w| old_alias.term_attach(terminal, retained.clone(), w)).unwrap();
    wait("retained K1 viewer", || {
        String::from_utf8_lossy(&retained.bytes.lock().unwrap()).contains("K1READY")
    });
    owner
        .manager
        .configure(vec![cfg(second.home.paths().socket)])
        .unwrap();
    wait(
        "second canonical representative and current broker snapshot",
        || {
            owner
                .manager
                .host(&r2)
                .is_some_and(|h| h.id() == "h_aaaaaaaa")
                && owner
                    .manager
                    .access(&r2)
                    .is_some_and(|h| h.status().host == "h_aaaaaaaa")
        },
    );
    let script2 = second.home.script(
        "identity-two",
        "echo K2READY; while IFS= read -r line; do echo K2:$line; done",
    );
    let direct2 = owner.manager.access("h_aaaaaaaa").unwrap();
    let current = Arc::new(Output::default());
    let open2 =
        reply(|w| direct2.term_open(spec(&second.home, &script2), current.clone(), w)).unwrap();
    reaper.0.extend(open2.pid.map(|pid| pid as i32));
    wait("independent K2 Direct output", || {
        String::from_utf8_lossy(&current.bytes.lock().unwrap()).contains("K2READY")
    });
    let mut follower = Process::start(&owner.home.root().join("desktop"));
    follower.usable(&r2);
    follower.ok(json!({"op":"attach","host":r2,"terminal":terminal}));
    follower.text(terminal, "K2READY");
    follower.ok(json!({"op":"input","host":r2,"terminal":terminal,"data":"broker-live\n"}));
    follower.text(terminal, "K2:broker-live");
    let status: HostStatus = owner
        .manager
        .snapshot()
        .into_iter()
        .find(|s| s.status.host == "h_aaaaaaaa")
        .unwrap()
        .status;
    let target = xshell_hostlink::handle::InputTarget {
        config_generation: status.config_generation,
        link_generation: status.link_generation,
    };
    assert!(old_alias
        .term_input(terminal, "wrong-key\n".into(), None, Some(target))
        .is_err());
    direct2
        .term_input(terminal, "direct-live\n".into(), None, Some(target))
        .unwrap();
    wait("K2 Direct continuity", || {
        String::from_utf8_lossy(&current.bytes.lock().unwrap()).contains("K2:direct-live")
    });
    assert!(!String::from_utf8_lossy(&retained.bytes.lock().unwrap()).contains("K2"));
    old_alias.term_detach(terminal).unwrap();
    direct2
        .term_input(terminal, "after-old-detach\n".into(), None, None)
        .unwrap();
    follower.text(terminal, "K2:after-old-detach");
    wait(
        "original identity still reachable for explicit cleanup",
        || {
            owner
                .manager
                .access(&r1)
                .is_some_and(|h| h.status().usable())
        },
    );
    reply(|w| owner.manager.access(&r1).unwrap().term_close(terminal, w)).unwrap();
    reply(|w| direct2.term_close(terminal, w)).unwrap();
    follower.quit();
    sync.shutdown();
}

#[test]
fn post_hello_partial_header_expires_and_reclaims_its_slot_while_healthy_client_works() {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let setup = Setup::new();
    let mut owner = setup.process();
    let dir = setup
        .owner
        .home
        .root()
        .join("desktop")
        .canonicalize()
        .unwrap();
    let profile = format!("{:x}", Sha256::digest(dir.as_os_str().as_encoded_bytes()));
    let mut stream = xshell_core::local_ipc::protected_connect(
        &dir.join("host-broker.sock"),
        Duration::from_secs(2),
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(35)))
        .unwrap();
    let hello =
        serde_json::to_vec(&json!({"type":"hello","version":1,"profile_id":profile})).unwrap();
    stream
        .write_all(&(hello.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&0u64.to_be_bytes()).unwrap();
    stream.write_all(&hello).unwrap();
    for _ in 0..2 {
        let mut header = [0; 12];
        stream.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let mut data = vec![0; len];
        stream.read_exact(&mut data).unwrap();
    }
    stream.write_all(&[0]).unwrap();
    owner.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    // Drain any already queued changed snapshots until the partial-header lease expires.
    let mut trailing = Vec::new();
    stream.read_to_end(&mut trailing).unwrap();
    let mut replacement = setup.process();
    replacement.ok(json!({"op":"call","host":setup.host,"method":"get_home_dir","params":{}}));
    replacement.quit();
    owner.quit();
}

#[test]
fn owner_loss_mutable_alias_retarget_does_not_deliver_other_canonical_same_uuid() {
    use xshell_hostlink::HostConfig;
    let relay = Relay::start();
    let owner = Computer::with_factory(
        &relay,
        |_| {},
        |home| Arc::new(OwnedSocketFactory(home.paths().socket)),
    );
    owner.ring.enable(None, false).unwrap();
    let first = Computer::new(&relay);
    let k1 = first.legacy_pair(&owner, &relay);
    let second = Computer::new(&relay);
    let k2 = second.legacy_pair(&owner, &relay);
    let sync = Box::leak(Box::new(xshell_hostlink::ring::HostSync::new()));
    *owner.events.sync.lock().unwrap() = Some(sync);
    let worker = xshell_hostlink::ring::SyncWorker::new(
        owner.ring.clone(),
        sync,
        Box::new({
            let m = owner.manager.clone();
            move |id| m.sync_host(id)
        }),
        Box::new(|| {}),
    );
    worker.start().unwrap();
    owner
        .manager
        .configure(vec![HostConfig {
            id: "h_aaaaaaaa".into(),
            name: "Saved".into(),
            ssh_target: first.home.paths().socket.to_string_lossy().into(),
            color: None,
            daemon_command: None,
            launch_prefixes: Default::default(),
        }])
        .unwrap();
    let (r1, r2) = (
        xshell_hostlink::ring::hosts::relay_host_id(&k1),
        xshell_hostlink::ring::hosts::relay_host_id(&k2),
    );
    wait("initial mutable alias K1", || {
        owner
            .manager
            .host(&r1)
            .is_some_and(|h| h.id() == "h_aaaaaaaa")
            && owner
                .manager
                .access(&r1)
                .is_some_and(|h| h.status().host == "h_aaaaaaaa")
    });
    let terminal = Uuid::new_v4();
    let script1 = first.home.script(
        "recovery-one",
        "echo K1READY; while IFS= read -r line; do echo K1:$line; done",
    );
    let spec = |home: &common::TestHome, script: &std::path::Path| OpenSpec {
        terminal,
        launch: common::raw_spec(&home.project("recovery"), script),
        cols: 80,
        rows: 24,
        meta: Default::default(),
        first_message: None,
        adopt_existing: false,
    };
    let open1 = reply(|w| {
        owner.manager.access("h_aaaaaaaa").unwrap().term_open(
            spec(&first.home, &script1),
            Arc::new(Output::default()),
            w,
        )
    })
    .unwrap();
    let mut reaper = common::PidReaper(open1.pid.into_iter().map(|pid| pid as i32).collect());
    // Explicit independent Direct creation makes the same UUID genuinely exist on K2.
    let auxiliary = Arc::new(Manager::new(ManagerConfig::new(
        "1.5.0",
        Arc::new(OwnedSocketFactory(second.home.paths().socket)),
        Arc::new(NoSsh),
        Arc::new(Events::default()),
    )));
    auxiliary.set_local(local_config());
    wait("independent K2 control", || {
        auxiliary.host("local").unwrap().status().usable()
    });
    let touched = second.home.root().join("K2-input");
    let script2 = second.home.script(
        "recovery-two",
        &format!(
            "echo K2READY; while IFS= read -r line; do echo K2:$line; echo \"$line\" >> '{}'; done",
            touched.display()
        ),
    );
    let open2 = reply(|w| {
        auxiliary.access("local").unwrap().term_open(
            spec(&second.home, &script2),
            Arc::new(Output::default()),
            w,
        )
    })
    .unwrap();
    reaper.0.extend(open2.pid.map(|pid| pid as i32));
    auxiliary.shutdown();
    let mut follower = Process::start(&owner.home.root().join("desktop"));
    follower.usable("h_aaaaaaaa");
    follower.ok(json!({"op":"attach","host":"h_aaaaaaaa","terminal":terminal}));
    follower.text(terminal, "K1READY");
    owner.ring.quit();
    follower.ok(json!({"op":"configure-socket","socket":second.home.paths().socket,"alias":r2}));
    follower.usable(&r2);
    follower.ok(json!({"op":"input","host":"h_aaaaaaaa","terminal":terminal,"data":"explicit-K2-control\n"}));
    wait("K2 is live at mutable alias", || {
        std::fs::read_to_string(&touched).is_ok_and(|s| s.contains("explicit-K2-control"))
    });
    follower.usable(&r1);
    follower.ok(json!({"op":"input","host":r1,"terminal":terminal,"data":"original-identity\n"}));
    let output = follower.text(terminal, "K1:original-identity");
    assert!(
        !output["text"].as_str().unwrap().contains("K2"),
        "retained K1 subscription delivered K2 output after owner loss"
    );
    follower.ok(json!({"op":"close","host":r1,"terminal":terminal}));
    follower.ok(json!({"op":"close","host":"h_aaaaaaaa","terminal":terminal}));
    follower.quit();
    sync.shutdown();
}

const KICK_BACKOFF_UNIT: Duration = Duration::from_millis(2000);
const KICK_HORIZON: Duration = Duration::from_secs(1);

fn kick_ready_before_natural_retry(
    snapshot: &Value,
    host: &str,
    sampled_epoch_ms: u64,
    sampled_at: Instant,
    now: Instant,
    ready_until: Instant,
) -> Option<Instant> {
    let (row, status) = target_snapshot(snapshot, host);
    if now >= ready_until
        || row["activeRoute"] != "relay"
        || row["reachability"]["kind"] != "online"
    {
        return None;
    }
    // nextRetryAt is epoch milliseconds; supervisor waits use Instant. Anchor before
    // the snapshot RPC, not at its reply, so RPC age consumes the remaining backoff.
    // Remove one millisecond for epoch truncation; this boundary is conservative.
    let remaining_ms = status["nextRetryAt"]
        .as_u64()?
        .checked_sub(sampled_epoch_ms)?
        .checked_sub(1)?;
    let natural_retry = sampled_at.checked_add(Duration::from_millis(remaining_ms))?;
    (natural_retry.checked_duration_since(now)? > KICK_HORIZON + KICK_BACKOFF_UNIT / 10)
        .then_some(natural_retry)
}

#[test]
fn kick_ready_before_natural_retry_rejects_non_online_and_aged_snapshot() {
    // Serialized snapshot predicate coverage only: no transport or signed-presence proof.
    let mut snapshot = json!({
        "hosts": [{"id":"target", "aliases":["alias"], "activeRoute":"relay",
            "reachability":{"kind":"unreachable"}},
            {"id":"other", "aliases":["other"], "activeRoute":"relay",
            "reachability":{"kind":"online"}}],
        "snapshots": [{"status":{"host":"other", "nextRetryAt":99000}},
            {"status":{"host":"target", "nextRetryAt":12000}}]
    });
    let at = Instant::now();
    let until = at + WAIT;
    let ready = |snapshot: &Value, now, until| {
        kick_ready_before_natural_retry(snapshot, "alias", 10000, at, now, until)
    };
    let (_, status) = target_snapshot(&snapshot, "alias");
    assert!(!status["nextRetryAt"].is_null(), "old readiness premise");
    assert!(ready(&snapshot, at, until).is_none());
    snapshot["hosts"][0]["reachability"]["kind"] = json!("online");
    let boundary = at + Duration::from_millis(1999);
    assert_eq!(ready(&snapshot, at, until), Some(boundary));
    assert!(ready(&snapshot, at + Duration::from_millis(799), until).is_none());
    assert!(ready(&snapshot, at + Duration::from_secs(2), until).is_none());
    assert!(
        ready(&snapshot, until, until).is_none(),
        "late RPC exceeds WAIT"
    );
    snapshot["snapshots"][1]["status"]["nextRetryAt"] = json!(10000);
    assert!(
        ready(&snapshot, at, until).is_none(),
        "expired epoch deadline"
    );
    snapshot["snapshots"][1]["status"]["nextRetryAt"] = json!("12000");
    assert!(
        ready(&snapshot, at, until).is_none(),
        "deadline must be numeric"
    );
}

#[test]
fn offline_authorized_kick_reaches_owner_supervisor_during_real_backoff() {
    use std::sync::atomic::AtomicUsize;
    let paused = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));
    let (pause, count) = (paused.clone(), hits.clone());
    let mut setup = Setup::with_config(move |cfg| {
        cfg.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
            if point == xshelld::server::TestPoint::RelayHostAccepted {
                count.fetch_add(1, Ordering::AcqRel);
                while pause.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            false
        })))
    });
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let _release = Release(paused.clone());
    // Short Hello and a 2000ms backoff unit expose the existing supervisor wait;
    // the actual scheduled delay can be a multiple of that unit. Neither production
    // deadline nor retry policy is enlarged to conceal a failed operation.
    let mut owner = Process::start_options(
        &setup.owner.home.root().join("desktop"),
        None,
        Some((100, KICK_BACKOFF_UNIT.as_millis() as u64)),
    );
    owner.usable(&setup.host);
    paused.store(true, Ordering::Release);
    setup.relay.restart();
    let until = Instant::now() + WAIT;
    let (before, at, natural_retry) = loop {
        let sampled_at = Instant::now();
        let sampled_epoch_ms = xshell_hostlink::status::now_ms();
        let snapshot = owner.ok(json!({"op":"snapshot"}));
        let now = Instant::now();
        assert!(
            now < until,
            "no Online Relay target in actual backoff with sufficient remaining time within WAIT: {snapshot}"
        );
        if kick_ready_before_natural_retry(
            &snapshot,
            &setup.host,
            sampled_epoch_ms,
            sampled_at,
            now,
            until,
        )
        .is_some()
        {
            let before = hits.load(Ordering::Acquire);
            let at = Instant::now();
            // Recheck the same aged sample immediately before Kick. Never refresh the
            // accepted natural-retry boundary after sending the command.
            if let Some(natural_retry) = kick_ready_before_natural_retry(
                &snapshot,
                &setup.host,
                sampled_epoch_ms,
                sampled_at,
                at,
                until,
            ) {
                break (before, at, natural_retry);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let assert_expedited = || {
        let now = Instant::now();
        assert!(
            now.duration_since(at) < KICK_HORIZON && now < natural_retry,
            "owner kick did not advance supervisor within 1s and before the sampled natural retry: elapsed={:?}, remaining={:?}",
            now.duration_since(at),
            natural_retry.checked_duration_since(now)
        );
    };
    owner.ok(json!({"op":"kick","host":setup.host}));
    assert_expedited();
    while hits.load(Ordering::Acquire) == before {
        assert_expedited();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_expedited();
    paused.store(false, Ordering::Release);
    owner.usable(&setup.host);
    owner.quit();
}

fn failed_shared_attach_retries_on_live_link(
    broker_first: bool,
    callback_retry: bool,
    open_retry: bool,
) {
    use std::sync::atomic::AtomicUsize;
    let terminal = Uuid::new_v4();
    let attempts = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
    struct Release(Arc<(Mutex<(bool, bool)>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0 .0.lock().unwrap().1 = true;
            self.0 .1.notify_all();
        }
    }
    let _release = Release(gate.clone());
    let (held, count) = (gate.clone(), attempts.clone());
    let relay = Relay::start();
    let owner = Computer::with_factory(
        &relay,
        move |cfg| {
            cfg.test_hook = Some(xshelld::server::TestHook(Arc::new(move |id, point| {
                if id == terminal
                    && point == xshelld::server::TestPoint::AttachDecide
                    && count.fetch_add(1, Ordering::AcqRel) == 0
                {
                    let mut state = held.0.lock().unwrap();
                    state.0 = true;
                    held.1.notify_all();
                    while !state.1 {
                        state = held.1.wait(state).unwrap();
                    }
                }
                false
            })));
        },
        |home| Arc::new(OwnedSocketFactory(home.paths().socket)),
    );
    owner.manager.set_local(local_config());
    wait("primary Direct usable", || {
        owner.manager.host("local").unwrap().status().usable()
    });
    let chain = owner.create();
    let key = chain
        .head()
        .roster()
        .members
        .iter()
        .find(|member| member.role == xshell_protocol::ring::Role::Daemon)
        .unwrap()
        .sign_key;
    let alias_id = xshell_hostlink::ring::hosts::relay_host_id(&key);
    let raw = owner.manager.host("local").unwrap();
    reply(|w| raw.ring_identity(raw.link_generation(), w)).unwrap();
    wait("current broker Local association", || {
        owner
            .manager
            .access(&alias_id)
            .is_some_and(|access| access.status().host == "local")
    });
    let direct = owner.manager.access("local").unwrap();
    let broker = owner.manager.access(&alias_id).unwrap();
    assert!(matches!(broker, HostAccess::Broker(_)));
    let generation = raw.link_generation();
    let (initiator, joiner) = if broker_first {
        (broker.clone(), direct.clone())
    } else {
        (direct.clone(), broker.clone())
    };
    let (first_tx, first_rx) = mpsc::channel();
    initiator.term_attach(
        terminal,
        Arc::new(Output::default()),
        Box::new(move |r| {
            first_tx.send(r).unwrap();
        }),
    );
    wait("first physical Attach held before registry lookup", || {
        gate.0.lock().unwrap().0
    });

    let auxiliary = Arc::new(Manager::new(ManagerConfig::new(
        "1.5.0",
        Arc::new(OwnedSocketFactory(owner.home.paths().socket)),
        Arc::new(NoSsh),
        Arc::new(Events::default()),
    )));
    auxiliary.set_local(local_config());
    wait("independent creation control usable", || {
        auxiliary.host("local").unwrap().status().usable()
    });
    let script = owner.home.script(
        "admission-retry",
        "echo RETRY-READY; while IFS= read -r line; do echo RETRY:$line; done",
    );
    let spec = OpenSpec {
        terminal,
        launch: common::raw_spec(&owner.home.project("admission"), &script),
        cols: 80,
        rows: 24,
        meta: Default::default(),
        first_message: None,
        adopt_existing: false,
    };
    let retry_sink = Arc::new(Output::default());
    let (joined_tx, joined_rx) = mpsc::channel();
    let (retried_tx, retried_rx) = mpsc::channel();
    let retry_access = joiner.clone();
    let aux = auxiliary.clone();
    let output = retry_sink.clone();
    let counter = attempts.clone();
    // Keep the PID reaper outside callbacks, including on an assertion failure.
    let pids = Arc::new(Mutex::new(common::PidReaper::default()));
    let reaper = pids.clone();
    joiner.term_attach(
        terminal,
        Arc::new(Output::default()),
        Box::new(move |result| {
            assert!(result.is_err());
            if callback_retry {
                if open_retry {
                    let before = counter.load(Ordering::Acquire);
                    retry_access.term_open(
                        spec,
                        output,
                        Box::new(move |r| {
                            if let Ok(opened) = &r {
                                reaper
                                    .lock()
                                    .unwrap()
                                    .0
                                    .extend(opened.pid.map(|pid| pid as i32));
                            }
                            retried_tx.send((before, r.map(|_| None))).unwrap();
                        }),
                    );
                } else {
                    let opened = reply(|w| {
                        aux.access("local")
                            .unwrap()
                            .term_open(spec, Arc::new(Output::default()), w)
                    })
                    .unwrap();
                    reaper
                        .lock()
                        .unwrap()
                        .0
                        .extend(opened.pid.map(|pid| pid as i32));
                    let before = counter.load(Ordering::Acquire);
                    // Retry before forwarding the failed joining result to the test caller.
                    retry_access.term_attach(
                        terminal,
                        output,
                        Box::new(move |r| {
                            retried_tx.send((before, r)).unwrap();
                        }),
                    );
                }
            }
            joined_tx.send(result).unwrap();
        }),
    );
    assert!(
        joined_rx.try_recv().is_err(),
        "joining Attach must wait for physical admission"
    );
    assert_eq!(
        attempts.load(Ordering::Acquire),
        1,
        "joining viewer must not send another Attach"
    );
    gate.0.lock().unwrap().1 = true;
    gate.1.notify_all();
    let first_error = first_rx.recv_timeout(WAIT).unwrap().unwrap_err();
    let join_error = joined_rx.recv_timeout(WAIT).unwrap().unwrap_err();
    assert_eq!(first_error.code, xshell_hostlink::HostErrorCode::Remote);
    assert_eq!(join_error.code, xshell_hostlink::HostErrorCode::Remote);
    assert!(
        raw.status().usable(),
        "the failed Attach must leave the Link live"
    );
    assert_eq!(raw.link_generation(), generation);
    let (before, retry) = if callback_retry {
        retried_rx.recv_timeout(WAIT).unwrap()
    } else {
        let script = owner.home.script(
            "admission-later",
            "echo RETRY-READY; while IFS= read -r line; do echo RETRY:$line; done",
        );
        let opened = reply(|w| {
            auxiliary.access("local").unwrap().term_open(
                OpenSpec {
                    terminal,
                    launch: common::raw_spec(&owner.home.project("admission"), &script),
                    cols: 80,
                    rows: 24,
                    meta: Default::default(),
                    first_message: None,
                    adopt_existing: false,
                },
                Arc::new(Output::default()),
                w,
            )
        })
        .unwrap();
        pids.lock()
            .unwrap()
            .0
            .extend(opened.pid.map(|pid| pid as i32));
        let before = attempts.load(Ordering::Acquire);
        (
            before,
            reply(|w| joiner.term_attach(terminal, retry_sink.clone(), w)),
        )
    };
    assert!(
        retry.is_ok(),
        "fresh joining retry reused cached failed admission: {retry:?}"
    );
    assert_eq!(
        attempts.load(Ordering::Acquire),
        before + 1,
        "retry must send a fresh physical Attach"
    );
    wait("fresh retry output", || {
        String::from_utf8_lossy(&retry_sink.bytes.lock().unwrap()).contains("RETRY-READY")
    });
    // A later successful joining viewer must be preserved, not retired by old cleanup.
    let other = if broker_first { broker } else { direct };
    let surviving = Arc::new(Output::default());
    reply(|w| other.term_attach(terminal, surviving.clone(), w)).unwrap();
    joiner
        .term_input(terminal, "both\n".into(), None, None)
        .unwrap();
    wait("successful joined viewer output", || {
        String::from_utf8_lossy(&surviving.bytes.lock().unwrap()).contains("RETRY:both")
    });
    joiner.term_detach(terminal).unwrap();
    other
        .term_input(terminal, "survives\n".into(), None, None)
        .unwrap();
    wait("successful independent viewer survives detach", || {
        String::from_utf8_lossy(&surviving.bytes.lock().unwrap()).contains("RETRY:survives")
    });
    assert_eq!(
        raw.link_generation(),
        generation,
        "retry must not reconnect to escape failed state"
    );
    reply(|w| other.term_close(terminal, w)).unwrap();
    auxiliary.shutdown();
}

#[test]
fn failed_broker_first_attach_does_not_poison_joining_direct_retry() {
    failed_shared_attach_retries_on_live_link(true, false, false);
}
#[test]
fn failed_direct_first_attach_preserves_joining_broker_retry() {
    failed_shared_attach_retries_on_live_link(false, false, false);
}
#[test]
fn failed_joining_direct_callback_can_retry_before_initiating_callback_returns() {
    failed_shared_attach_retries_on_live_link(true, true, false);
}

#[test]
fn failed_joining_direct_callback_can_explicitly_open_without_cached_failed_viewers() {
    failed_shared_attach_retries_on_live_link(true, true, true);
}

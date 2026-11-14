// Separate-process broker driver shared by the native backend suites.
#[cfg(unix)]
fn local_dialer(path: std::path::PathBuf) -> Box<dyn xshell_hostlink::Dialer> {
    Box::new(xshell_hostlink::UnixSocketDialer { path })
}
#[cfg(windows)]
fn local_dialer(name: std::path::PathBuf) -> Box<dyn xshell_hostlink::Dialer> {
    Box::new(xshell_hostlink::NamedPipeDialer { name })
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
        Some(local_dialer(path))
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

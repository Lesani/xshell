#![cfg(unix)]
//! Add to Ring for a saved SSH Host (#115), against the actual `xshelld pair` CLI, a real
//! `DesktopRing` and the unchanged production workerd Relay. The "remote" command runs
//! locally through `LocalShellTransport` (the SSH route's stand-in): the Host's "Daemon
//! command" is a script that runs this build's `xshelld` with the Host fixture's home.
//! Run with TMPDIR on SSD or RAM (see `unix_pair_driver.rs`).
mod common;
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use xshell_hostlink::ring::adoption::CreateDecision;
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::ssh_host::{add_ssh_host, AddCode, AddRequest, PairBinary, Timeouts};
use xshell_hostlink::ring::store::Store;
use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig, RingObserver, RingView};
use xshell_hostlink::{
    BinarySource, CancelToken, LocalShellTransport, Manager, ManagerConfig, Observer,
    TransportFactory,
};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::{RosterChain, SignKey};
use xshelld::server::{Config, Server, ServerHandle};

struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}
impl Observer for Quiet {
    fn status(&self, _: &xshell_hostlink::HostStatus) {}
    fn terminals(&self, _: &str, _: &[xshell_protocol::msg::TerminalInfo]) {}
}
struct NoFallback;
impl TransportFactory for NoFallback {
    fn for_host(&self, _: &xshell_hostlink::HostConfig) -> Box<dyn xshell_hostlink::Transport> {
        panic!("no SSH/Direct here");
    }
}
impl BinarySource for NoFallback {
    fn fetch(
        &self,
        _: &xshell_hostlink::Wanted,
        _: &xshell_hostlink::CancelToken,
    ) -> Result<xshell_hostlink::Fetched, String> {
        panic!("no install here");
    }
}

/// One machine: a home with its Daemon's paths, and a Desktop store.
struct Fixture {
    home: tempfile::TempDir,
    paths: xshelld::paths::Paths,
    desktop: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        common::isolate_home();
        let home = tempfile::Builder::new()
            .prefix("ssh-ring-")
            .tempdir()
            .unwrap();
        let root = home.path().canonicalize().unwrap();
        let paths = xshelld::paths::resolve(&root, None, Some(&root.join("run/daemon.sock")));
        Self {
            desktop: root.join("desktop"),
            home,
            paths,
        }
    }
    fn server(&self) -> ServerHandle {
        Server::start(Config::new(self.home.path().into(), self.paths.clone())).unwrap()
    }
    fn ring(&self, relay: &str) -> Arc<DesktopRing> {
        let mut cfg = DesktopRingConfig::new(self.desktop.clone(), "Computer".into());
        cfg.default_relay_url = relay.into();
        cfg.owner_retry = Duration::from_millis(20);
        DesktopRing::open(cfg, Arc::new(Quiet))
    }
    fn companion(&self) -> CompanionClient {
        CompanionClient::for_store(&self.paths.ring_dir).unwrap()
    }
    fn manager(&self, ring: &DesktopRing) -> Arc<Manager> {
        let manager = Arc::new(Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(NoFallback),
            Arc::new(NoFallback),
            Arc::new(Quiet),
        )));
        ring.attach_manager(&manager).unwrap();
        manager
    }
    /// The Host's "Daemon command": this build's `xshelld` on this fixture's home.
    fn daemon_command(&self) -> String {
        let script = self.home.path().join("xd");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nexec '{}' \"$@\" --home '{}' --socket '{}'\n",
                env!("CARGO_BIN_EXE_xshelld"),
                self.home.path().display(),
                self.paths.socket.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        format!("'{}'", script.display())
    }
    /// The SSH route's stand-in: `sh -c` here, with the Host's own environment.
    fn transport(&self) -> LocalShellTransport {
        LocalShellTransport {
            env: vec![
                ("HOME".into(), self.home.path().into()),
                ("XSHELL_GET_URL".into(), common::closed_get_url().into()),
                ("XSHELLD_PAIR_TTL_MS".into(), "30000".into()),
            ],
        }
    }
    /// `xshelld pair` started directly (to hold another machine's words).
    fn pair(&self, relay: &str) -> PairChild {
        let mut child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
            .arg("pair")
            .args(["--relay", relay])
            .arg("--home")
            .arg(self.home.path())
            .arg("--socket")
            .arg(&self.paths.socket)
            .env("XSHELL_GET_URL", common::closed_get_url())
            .env("XSHELLD_PAIR_TTL_MS", "30000")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let _ = tx.send(line);
            }
        });
        PairChild { child, lines }
    }
    fn chain(&self) -> RosterChain {
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.paths.ring_dir.join("roster.json")).unwrap(),
        )
        .unwrap();
        RosterChain::from_tokens(
            &serde_json::from_value::<Vec<String>>(json["rosters"].clone()).unwrap(),
        )
        .unwrap()
    }
}

struct PairChild {
    child: Child,
    lines: mpsc::Receiver<String>,
}
impl PairChild {
    fn code(&self) -> DeviceCode {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let line = self
                .lines
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .expect("the CLI printed words");
            if let Ok(code) = DeviceCode::parse(&line) {
                return code;
            }
        }
    }
}
impl Drop for PairChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn create(ring: &DesktopRing, client: &CompanionClient) {
    ring.create_computer(client, |p, lease| {
        Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
    })
    .unwrap();
}

fn identity(daemon: &ServerHandle) -> (SignKey, serde_json::Value) {
    let (key, _, v) = common::ring::identity(daemon);
    (key, v)
}

/// An approver: a Computer that created a Ring.
fn approver(relay: &str) -> (Fixture, ServerHandle, Arc<DesktopRing>) {
    let a = Fixture::new();
    let daemon = a.server();
    let ring = a.ring(relay);
    create(&ring, &a.companion());
    (a, daemon, ring)
}

/// Nothing was prepared or signed: the head is unchanged and no outgoing proposal exists.
fn assert_nothing_signed(a: &Fixture, ring: &DesktopRing, version: u64) {
    assert_eq!(ring.chain().unwrap().head().version(), version);
    let profile = Store::new(a.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert!(profile.outgoing.is_none(), "a proposal was prepared");
}

#[test]
fn ssh_host_ends_up_a_member_and_the_result_names_it() {
    let relay = workerd::Relay::start();
    let (_a, _ad, ar) = approver(&relay.url());
    let b = Fixture::new();
    let bd = b.server();
    let (b_key, b_identity) = identity(&bd);
    assert!(b_identity["ring"].is_null());
    let before = ar.chain().unwrap().head().version();
    let transport = b.transport();
    let binary = PairBinary::Command(b.daemon_command());
    let cancel = CancelToken::new();
    let request = AddRequest {
        ring: &ar,
        name: "dev box",
        transport: &transport,
        binary: &binary,
        cancel: &cancel,
        timeouts: Timeouts::default(),
    };
    let added = add_ssh_host(&request, || Ok(b_identity.clone()), || true).unwrap();
    assert!(added.paired);
    assert_eq!(added.names, vec!["dev box".to_string()]);
    assert!(added.version > before);
    let head = ar.chain().unwrap();
    assert_eq!(head.head().version(), added.version);
    let member = head
        .authorized_member(&b_key)
        .expect("the Host is a member");
    assert_eq!(member.name, "dev box");
    // The Host's own Daemon holds the same Ring.
    assert_eq!(b.chain().ring_id(), head.ring_id());
    assert!(b.chain().authorized_member(&b_key).is_some());

    // Again: the identity says this Ring, so success without running `pair`.
    let (_, again) = identity(&bd);
    assert_eq!(again["ring"]["ringId"], head.ring_id().as_str());
    let request = AddRequest {
        transport: &LocalShellTransport::default(),
        binary: &PairBinary::Command("false".into()),
        ..request
    };
    let added = add_ssh_host(&request, || Ok(again), || true).unwrap();
    assert!(!added.paired);
    assert_eq!(added.names, vec!["dev box".to_string()]);
    assert_eq!(added.version, head.head().version());
    ar.quit();
}

#[test]
fn host_in_another_ring_is_refused_without_running_pair() {
    let relay = workerd::Relay::start();
    let (_a, _ad, ar) = approver(&relay.url());
    let (_c, cd, cr) = approver(&relay.url());
    let (_, other) = identity(&cd);
    assert!(other["ring"]["ringId"].is_string());
    let version = ar.chain().unwrap().head().version();
    let request = AddRequest {
        ring: &ar,
        name: "theirs",
        transport: &LocalShellTransport::default(),
        binary: &PairBinary::Command("exit 99; :".into()),
        cancel: &CancelToken::new(),
        timeouts: Timeouts::default(),
    };
    let e = add_ssh_host(&request, || Ok(other), || true).unwrap_err();
    assert_eq!(e.code, AddCode::OtherRing, "{e}");
    assert_eq!(ar.chain().unwrap().head().version(), version);
    cr.quit();
    ar.quit();
}

/// AM1: words of another machine are refused before anything is prepared or signed.
#[test]
fn substituted_code_is_refused_and_nothing_is_signed() {
    let relay = workerd::Relay::start();
    let (a, _ad, ar) = approver(&relay.url());
    let b = Fixture::new();
    let bd = b.server();
    let (b_key, _) = identity(&bd);
    let c = Fixture::new();
    let _cd = c.server();
    let c_pair = c.pair(&relay.url());
    let c_code = c_pair.code();
    let version = ar.chain().unwrap().head().version();
    let e = ar.approve_device_code_for(&c_code, &b_key).unwrap_err();
    assert!(e.starts_with("substituted: "), "{e}");
    assert_nothing_signed(&a, &ar, version);
    assert!(!c.paths.ring_dir.join("roster.json").exists());
    ar.quit();
}

/// AM1: a registered Computer's words (it runs its own Desktop) are refused as such, before
/// anything is prepared or signed, also through the whole Add to Ring (`own-desktop`).
#[test]
fn computer_intent_is_refused_and_nothing_is_signed() {
    let relay = workerd::Relay::start();
    let (a, _ad, ar) = approver(&relay.url());
    let b = Fixture::new();
    let bd = b.server();
    let br = b.ring(&relay.url());
    br.register_companion(&b.companion()).unwrap();
    let _manager = b.manager(&br);
    let (b_key, b_identity) = identity(&bd);
    let version = ar.chain().unwrap().head().version();

    let child = b.pair(&relay.url());
    let code = child.code();
    let e = ar.approve_device_code_for(&code, &b_key).unwrap_err();
    assert!(e.starts_with("computer_intent: "), "{e}");
    assert_nothing_signed(&a, &ar, version);
    drop(child);

    // Through Add to Ring.
    let transport = b.transport();
    let binary = PairBinary::Command(b.daemon_command());
    let cancel = CancelToken::new();
    let request = AddRequest {
        ring: &ar,
        name: "laptop",
        transport: &transport,
        binary: &binary,
        cancel: &cancel,
        timeouts: Timeouts::default(),
    };
    let until = Instant::now() + Duration::from_secs(10);
    let e = loop {
        // The earlier offer may still be closing on the Computer's Desktop.
        let e = add_ssh_host(&request, || Ok(b_identity.clone()), || true).unwrap_err();
        if e.code != AddCode::Unfinished || Instant::now() >= until {
            break e;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(e.code, AddCode::OwnDesktop, "{e}");
    assert_nothing_signed(&a, &ar, version);
    assert!(br.chain().is_none());
    br.quit();
    ar.quit();
}

/// Sol's diff review 4: the saved Host changes (edited, or a new link) while Add to Ring
/// runs. Before the identity is used: `pair` never starts. After it, before the approval:
/// the words are read but nothing is approved. Both are `failed: the Host changed`.
#[test]
fn a_host_that_changes_during_the_action_is_not_approved() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let relay = workerd::Relay::start();
    let (a, _ad, ar) = approver(&relay.url());
    let b = Fixture::new();
    let bd = b.server();
    let (b_key, b_identity) = identity(&bd);
    let version = ar.chain().unwrap().head().version();
    let transport = b.transport();
    let cancel = CancelToken::new();

    // Changed before the identity reply is used: the command never runs.
    let marker = b.home.path().join("pair-ran");
    let never = PairBinary::Command(format!("touch '{}'; :", marker.display()));
    let request = AddRequest {
        ring: &ar,
        name: "dev box",
        transport: &transport,
        binary: &never,
        cancel: &cancel,
        timeouts: Timeouts::default(),
    };
    let e = add_ssh_host(&request, || Ok(b_identity.clone()), || false).unwrap_err();
    assert_eq!(e.to_string(), "failed: the Host changed");
    assert!(!marker.exists(), "pair ran for a changed Host");
    assert_nothing_signed(&a, &ar, version);

    // Unchanged until `pair` runs, changed before the approval: nothing is approved.
    let binary = PairBinary::Command(b.daemon_command());
    let request = AddRequest {
        binary: &binary,
        ..request
    };
    let asked = AtomicUsize::new(0);
    let e = add_ssh_host(
        &request,
        || Ok(b_identity.clone()),
        || asked.fetch_add(1, Ordering::SeqCst) < 2,
    )
    .unwrap_err();
    assert_eq!(e.to_string(), "failed: the Host changed");
    assert_eq!(asked.load(Ordering::SeqCst), 3, "asked before the approval");
    assert_nothing_signed(&a, &ar, version);
    assert!(ar.chain().unwrap().authorized_member(&b_key).is_none());
    assert!(!b.paths.ring_dir.join("roster.json").exists());
    ar.quit();
}

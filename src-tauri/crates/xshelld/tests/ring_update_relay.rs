#![cfg(target_os = "linux")]
//! A Desktop updates a Ring Host over the Relay (S4; ADR-0019): no SSH, no configured route.
//! Production workerd, a real Desktop Manager and a real `xshelld` process on the Host; the
//! new build is this test binary with another commit stamped in (see `ring_update.rs`).
mod common;
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use common::{alive, bin, TestHome};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use workerd::Relay;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::hosts::{relay_host_id, AvailableHost};
use xshell_hostlink::ring::{
    DesktopRing, DesktopRingConfig, PairingEvent, PairingFlow, PairingObserver, RingObserver,
    RingView,
};
use xshell_hostlink::status::RouteUpdateUnavailable;
use xshell_hostlink::{
    BinarySource, CancelToken, Fetched, HostConfig, HostError, HostHandle, HostStatus, Manager,
    ManagerConfig, Observer, Origin, TermSink, Transport, TransportFactory, Wanted,
};
use xshell_protocol::build::{install_dir_name, BuildId};
use xshell_protocol::msg::{OpenSpec, TerminalInfo};
use xshell_protocol::ring::{RosterChain, SignKey};
use xshelld::server::{Config, Server, ServerHandle};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const WAIT: Duration = Duration::from_secs(30);

fn own_build() -> BuildId {
    xshell_protocol::build::parse_stamp(env!("XSHELL_BUILDINFO_STAMP"))
        .unwrap()
        .expect("tests run from a git checkout")
}

fn stripped() -> &'static Path {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let m = fs::metadata(bin()).unwrap();
        let mtime = m
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let out = PathBuf::from(format!("/tmp/xd-stripped-{}-{mtime}", m.len()));
        if out.is_file() {
            return out;
        }
        let tmp = PathBuf::from(format!("{}.{}.tmp", out.display(), std::process::id()));
        assert!(Command::new("strip")
            .arg("-o")
            .arg(&tmp)
            .arg(bin())
            .status()
            .expect("strip (binutils) is required")
            .success());
        fs::rename(&tmp, &out).unwrap();
        out
    })
}

fn patched(commit: &str) -> (Vec<u8>, BuildId) {
    let mut bytes = fs::read(stripped()).unwrap();
    let own = own_build();
    let marker = b"@@XSHELL-BUILD-V1:";
    let mut i = 0;
    while let Some(at) = bytes[i..].windows(marker.len()).position(|w| w == marker) {
        let start = i + at + marker.len();
        bytes[start..start + 12].copy_from_slice(commit.as_bytes());
        i = start + 12;
    }
    (
        bytes,
        BuildId {
            commit: commit.into(),
            ..own
        },
    )
}

/// The build a Desktop sends: fixed bytes for every target.
struct Bytes(Vec<u8>);
impl BinarySource for Bytes {
    fn fetch(&self, _: &Wanted, _: &CancelToken) -> Result<Fetched, String> {
        Ok(Fetched {
            bytes: self.0.clone(),
            origin: Origin::Bundled("<test build>".into()),
        })
    }
}

struct NoSsh;
impl TransportFactory for NoSsh {
    fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
        panic!("a Ring Host update must not construct an SSH transport")
    }
    fn direct(&self, _: &HostConfig) -> Option<Box<dyn xshell_hostlink::Dialer>> {
        panic!("a Ring Host update must not probe a configured route")
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<HostStatus>>);
impl Observer for Events {
    fn status(&self, s: &HostStatus) {
        self.0.lock().unwrap().push(s.clone());
    }
    fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
    fn available(&self, _: &[AvailableHost]) {}
}
struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}
impl PairingObserver for Quiet {
    fn pairing(&self, _: PairingFlow, _: &PairingEvent) {}
}

fn wait(what: &str, f: impl Fn() -> bool) {
    let until = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < until, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reply<T: Send + 'static>(
    send: impl FnOnce(Box<dyn FnOnce(Result<T, HostError>) + Send>),
) -> Result<T, HostError> {
    let (tx, rx) = std::sync::mpsc::channel();
    send(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    rx.recv_timeout(Duration::from_secs(120)).unwrap()
}

#[derive(Default)]
struct Sink(Mutex<Vec<u8>>);
impl TermSink for Sink {
    fn data(&self, b: &[u8]) -> bool {
        self.0.lock().unwrap().extend_from_slice(b);
        true
    }
    fn exit(&self, _: i32, _: u64) {}
}

/// A Desktop with its own in-process Daemon: `version`/`build` is what it sends.
struct Desktop {
    _daemon: ServerHandle,
    ring: Arc<DesktopRing>,
    manager: Arc<Manager>,
    events: Arc<Events>,
    _home: TestHome,
}

impl Desktop {
    fn new(relay: &Relay, build: BuildId, bytes: Vec<u8>) -> Desktop {
        let home = TestHome::new();
        let mut cfg = Config::new(home.home(), home.paths());
        cfg.ring_backoff_unit = Duration::from_millis(10);
        let daemon = Server::start(cfg).unwrap();
        let mut rc = DesktopRingConfig::new(home.root().join("desktop"), "desk".into());
        rc.default_relay_url = relay.url();
        rc.owner_retry = Duration::from_millis(20);
        rc.backoff_unit = Duration::from_millis(10);
        let ring = DesktopRing::open(rc, Arc::new(Quiet));
        let events = Arc::new(Events::default());
        let mut mc = ManagerConfig::new(
            VERSION,
            Arc::new(NoSsh),
            Arc::new(Bytes(bytes)),
            events.clone(),
        );
        mc.desktop_build = Some(build);
        mc.backoff_unit = Duration::from_millis(20);
        let manager = Arc::new(Manager::new(mc));
        ring.attach_manager(&manager).unwrap();
        let companion =
            CompanionClient::for_store(daemon.companion_endpoint().unwrap().parent().unwrap())
                .unwrap();
        ring.register_companion(&companion).unwrap();
        match ring
            .create_computer(&companion, |p, lease| {
                Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
            })
            .unwrap()
        {
            AdoptionOutcome::Installed { .. } => {}
            _ => panic!("Create not installed"),
        }
        Desktop {
            _daemon: daemon,
            ring,
            manager,
            events,
            _home: home,
        }
    }

    fn host(&self, key: SignKey) -> Arc<HostHandle> {
        let id = relay_host_id(&key);
        wait("Ring Host usable", || {
            self.manager.host(&id).is_some_and(|h| h.status().usable())
        });
        self.manager.host(&id).unwrap()
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        self.ring.quit();
        self.manager.shutdown();
    }
}

/// A machine whose Daemon is a real process of this build, joined to the Desktop's Ring.
struct Machine {
    h: TestHome,
    child: Option<Child>,
    ring: Option<Arc<DesktopRing>>,
}

impl Machine {
    fn start(relay: &Relay, desktop: &Desktop, env: &[(&str, &str)]) -> (Machine, SignKey) {
        let h = TestHome::new();
        let own = install_dir_name(VERSION, Some(&own_build()));
        let exe = xshell_hostlink::install::install_local(stripped(), &h.home(), &own).unwrap();
        let mut c = Command::new(&exe);
        c.arg("serve")
            .arg("--interactive-env")
            .env("HOME", h.home())
            .env("XDG_RUNTIME_DIR", h.run())
            .env("XSHELLD_LOGIN_ENV", "0")
            .env("XSHELL_SERVICE_MECHANISM", "none")
            .env("XSHELLD_TEST_TRIAL_SETTLE_MS", "800")
            .env("XSHELLD_TEST_TRIAL_TERM_MS", "1000")
            .env_remove("XSHELLD_IDLE_TIMEOUT_MS")
            .env_remove("XSHELLD_HOME")
            .env_remove("XSHELLD_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        let child = c.spawn().unwrap();
        common::connect_socket(&h.paths().socket);
        let store = xshell_core::local_ipc::canonical_store(&h.paths().ring_dir).unwrap();
        let endpoint = xshell_core::local_ipc::companion_endpoint(&store).unwrap();
        wait("companion endpoint", || endpoint.exists());
        let companion = CompanionClient::for_store(endpoint.parent().unwrap()).unwrap();
        let mut rc = DesktopRingConfig::new(h.root().join("desktop"), "box".into());
        rc.default_relay_url = relay.url();
        rc.owner_retry = Duration::from_millis(20);
        rc.backoff_unit = Duration::from_millis(10);
        let ring = DesktopRing::open(rc, Arc::new(Quiet));
        ring.register_companion(&companion).unwrap();
        let offer = ring.begin_join(&companion).unwrap();
        let code = offer.code.clone();
        let chain: RosterChain = std::thread::scope(|scope| {
            let approval = scope.spawn(|| desktop.ring.approve_device_code(&code));
            let result = offer.wait().unwrap();
            approval.join().unwrap().unwrap();
            match result {
                AdoptionOutcome::Installed { chain, .. } => chain,
                _ => panic!("Join not installed"),
            }
        });
        let _ = chain;
        // This machine's Daemon key, from the Daemon itself.
        let mut c = common::Client::connect(&h.paths().socket);
        c.hello(common::range(1, 1));
        let id = c
            .request(&xshell_protocol::msg::ClientMsg::RingIdentity)
            .unwrap();
        let key = SignKey::parse(id["signKey"].as_str().unwrap()).unwrap();
        (
            Machine {
                h,
                child: Some(child),
                ring: Some(ring),
            },
            key,
        )
    }

    fn pid(&self) -> i32 {
        self.child.as_ref().unwrap().id() as i32
    }

    fn installed(&self, dir: &str) -> PathBuf {
        xshell_core::server::server_root(&self.h.home())
            .join(dir)
            .join("xshelld")
    }

    fn trial_closed(&self) -> bool {
        let root = xshell_core::server::server_root(&self.h.home());
        !root.join("trial").exists() && !xshell_hostlink::trial::held(&self.h.home())
    }

    fn log(&self) -> String {
        fs::read_to_string(self.h.paths().log).unwrap_or_default()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(r) = self.ring.take() {
            r.quit();
        }
        for _ in 0..3 {
            if let Some(pid) = fs::read_to_string(self.h.paths().pid)
                .ok()
                .and_then(|p| p.trim().parse::<i32>().ok())
            {
                if alive(pid) {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    common::wait_dead(pid, Duration::from_secs(5));
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn name_of(d: &Desktop, id: &str) -> String {
    d.manager
        .available_hosts()
        .into_iter()
        .find(|h| h.id == id)
        .map(|h| h.name)
        .unwrap()
}

fn exe_of(pid: i32) -> PathBuf {
    fs::read_link(format!("/proc/{pid}/exe")).unwrap()
}

fn open_shell(host: &HostHandle, cwd: &Path) -> (Uuid, Arc<Sink>, u32) {
    let terminal = Uuid::new_v4();
    let sink = Arc::new(Sink::default());
    let spec = OpenSpec {
        terminal,
        launch: common::sh_spec(cwd),
        cols: 80,
        rows: 24,
        meta: Default::default(),
        first_message: None,
        adopt_existing: false,
    };
    let opened = reply(|w| host.term_open(spec, sink.clone(), w)).unwrap();
    (terminal, sink, opened.pid.unwrap())
}

/// The whole path: a Desktop with another build updates a Ring Host over the Relay; the
/// Host's Daemon re-executes in the same process, comes back through the Relay with that
/// build, and its Terminal is there again under the same UUID.
#[test]
fn relay_desktop_updates_other_build_reexec_same_pid_terminal_resumes() {
    let relay = Relay::start();
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build.clone(), bytes);
    let (m, key) = Machine::start(&relay, &desktop, &[]);
    let host = desktop.host(key);
    let s = host.status();
    assert_eq!(s.route_update_unavailable, None, "{s:?}");
    assert_eq!(
        s.update_action,
        Some(xshell_hostlink::status::UpdateAction::Replace)
    );
    assert!(s.daemon_capabilities.iter().any(|c| c == "daemon.stage"));
    let pid = m.pid();
    let (t, _sink, old_terminal) = open_shell(&host, &m.h.project("p"));
    let before = host.status().link_generation;
    reply(|w| host.upgrade(w)).unwrap_or_else(|e| panic!("{e:?}\n{}", m.log()));
    let dir = install_dir_name(VERSION, Some(&build));
    wait("updated and connected again", || {
        let s = host.status();
        s.usable()
            && s.phase.is_none()
            && s.link_generation > before
            && s.daemon_build.as_ref() == Some(&build)
    });
    assert_eq!(host.status().last_error, None);
    assert!(alive(pid));
    assert_eq!(exe_of(pid), m.installed(&dir), "{}", m.log());
    wait("Terminal back under its UUID", || {
        host.terminals().is_some_and(|l| {
            l.iter()
                .any(|i| i.terminal == t && i.pid.is_some() && i.pid != Some(old_terminal))
        })
    });
    wait("trial accepted", || m.trial_closed());
    assert!(m.log().contains("accepted"), "{}", m.log());
    assert_eq!(exe_of(pid), m.installed(&dir));
    // The phases the Desktop showed: sending, then restarting.
    let phases: Vec<_> = desktop
        .events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.host == host.id())
        .filter_map(|s| s.phase)
        .collect();
    assert!(
        phases.contains(&xshell_hostlink::Phase::Installing),
        "{phases:?}"
    );
    assert!(
        phases.contains(&xshell_hostlink::Phase::Upgrading),
        "{phases:?}"
    );
}

/// Round 9 (D1): the Desktop's update command reaches a Ring Host the way the window does.
/// Every UI lookup of a Ring Host goes through the elected broker, which cannot update a
/// Daemon ("Update all" then failed at once, shown as "hasn't reconnected"); the update
/// route of the owning window uses its own link.
#[test]
fn update_through_the_desktop_route_reaches_the_ring_host() {
    let relay = Relay::start();
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build.clone(), bytes);
    let (m, key) = Machine::start(&relay, &desktop, &[]);
    let host = desktop.host(key);
    let id = host.id().to_string();
    let before = host.status().link_generation;
    // What the UI holds: the broker's view and the target it reads from it.
    let ui = desktop.manager.access(&id).unwrap();
    assert!(matches!(ui, xshell_hostlink::HostAccess::Broker(_)));
    // The UI reads the broker's publication of the connected Host (it follows the link
    // shortly after the Host is usable).
    let read = || {
        let seen = ui.snapshot().status;
        xshell_hostlink::handle::UpdateTarget {
            config_generation: seen.config_generation,
            link_generation: seen.link_generation,
            daemon_version: seen.daemon_version.clone(),
            daemon_build: seen.daemon_build.clone(),
        }
    };
    let mut target = read();
    let until = Instant::now() + Duration::from_secs(15);
    while ui.snapshot().status.link_generation == 0 || !ui.snapshot().status.usable() {
        assert!(
            Instant::now() < until,
            "the broker never published the connected Host"
        );
        std::thread::sleep(Duration::from_millis(50));
        target = read();
    }
    let refused = reply(|w| ui.upgrade_target(Some(target.clone()), w)).unwrap_err();
    assert!(refused.message.contains("broker"), "{refused:?}");
    // A stale target is refused before anything is sent.
    let stale = xshell_hostlink::handle::UpdateTarget {
        link_generation: target.link_generation + 1,
        ..target.clone()
    };
    assert!(reply(|w| desktop.manager.upgrade_target(&id, stale, w)).is_err());
    // What the update command does: the owner's own link, with the UI's target (read again
    // if the publication followed a newer link meanwhile, as a user would see it).
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        match reply(|w| desktop.manager.upgrade_target(&id, target.clone(), w)) {
            Ok(_) => break,
            Err(e) if e.message.contains("target changed") && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(100));
                target = read();
            }
            Err(e) => panic!("{e:?}\n{}", m.log()),
        }
    }
    wait("updated and connected again", || {
        let s = host.status();
        s.usable()
            && s.phase.is_none()
            && s.link_generation > before
            && s.daemon_build.as_ref() == Some(&build)
    });
    assert_eq!(host.status().last_error, None);
}

/// The new build does not come up: the Host goes back to its build, and the Desktop says
/// the update was rolled back (copy `update.rolledBack`).
#[test]
fn relay_update_rolled_back_is_reported() {
    let relay = Relay::start();
    let build = BuildId {
        commit: "0123456789ab".into(),
        ..own_build()
    };
    let v = serde_json::json!({"name":"xshelld","version":VERSION,"protocol":{"min":1,"max":1},"os":"Linux","arch":"x86_64","build":build});
    let script = format!(
        "#!/bin/sh\n# @@XSHELL-BUILD-V1:{}:{}@@\ncase \"$1\" in --version) echo '{v}'; exit 0;; esac\nexit 1\n",
        build.display(),
        build.time
    );
    let desktop = Desktop::new(&relay, build.clone(), script.into_bytes());
    let (m, key) = Machine::start(&relay, &desktop, &[]);
    let host = desktop.host(key);
    let own = install_dir_name(VERSION, Some(&own_build()));
    let (t, _sink, _) = open_shell(&host, &m.h.project("p"));
    let _ = reply(|w| host.upgrade(w));
    wait("rolled back and connected again", || {
        let s = host.status();
        s.usable()
            && s.daemon_build.as_ref() == Some(&own_build())
            && s.last_error
                .as_deref()
                .is_some_and(|e| e.contains("back on version"))
    });
    let s = host.status();
    assert_eq!(
        s.last_error.as_deref(),
        Some(
            format!(
                "The update failed. {} is back on version {VERSION}. Try again.",
                name_of(&desktop, host.id())
            )
            .as_str()
        )
    );
    assert_eq!(
        s.error_hint,
        Some(xshell_hostlink::HostErrorHint::UpdateFailed)
    );
    wait("trial closed", || m.trial_closed());
    assert_eq!(
        fs::read_to_string(xshell_core::server::server_root(&m.h.home()).join("selected"))
            .unwrap()
            .trim(),
        own
    );
    wait("Terminal restored under the old build", || {
        host.terminals()
            .is_some_and(|l| l.iter().any(|i| i.terminal == t))
    });
}

/// A Ring Host whose xshell offers no update over the Ring shows how to update it there,
/// offers no action, and refuses an update without any SSH.
#[test]
fn old_daemon_without_capability_shows_install_command() {
    let relay = Relay::start();
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build, bytes);
    let (_m, key) = Machine::start(
        &relay,
        &desktop,
        &[("XSHELLD_TEST_HIDE_CAPABILITIES", "daemon.stage")],
    );
    let host = desktop.host(key);
    let s = host.status();
    if s.daemon_capabilities.iter().any(|c| c == "daemon.stage") {
        panic!("the test Daemon still offers daemon.stage: {s:?}");
    }
    assert_eq!(
        s.route_update_unavailable,
        Some(RouteUpdateUnavailable::RingUpdateUnsupported)
    );
    assert_eq!(s.update_action, None);
    let e = reply(|w| host.upgrade(w)).unwrap_err();
    assert_eq!(
        e.message,
        format!(
            "{} is too old to update through your Ring. Update xshell directly on that machine.",
            name_of(&desktop, host.id())
        )
    );
    assert_eq!(
        serde_json::to_value(RouteUpdateUnavailable::RingUpdateUnsupported).unwrap(),
        serde_json::json!("ring-update-unsupported")
    );
}

/// A Ring Host that is not online cannot be updated; nothing is queued.
#[test]
fn ring_update_refused_offline() {
    let relay = Relay::start();
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build, bytes);
    let (mut m, key) = Machine::start(&relay, &desktop, &[]);
    let host = desktop.host(key);
    let pid = m.pid();
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = m.child.as_mut().unwrap().wait();
    wait("offline", || !host.status().usable());
    let e = reply(|w| host.upgrade(w)).unwrap_err();
    assert_eq!(
        e.message,
        format!(
            "{} is offline. Try updating it when it is online.",
            name_of(&desktop, host.id())
        )
    );
}

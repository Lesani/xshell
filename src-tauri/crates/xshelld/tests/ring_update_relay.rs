#![cfg(target_os = "linux")]
//! A Desktop updates a Ring Host over the Relay (S4; ADR-0019): no SSH, no configured route.
//! Production workerd, a real Desktop Manager and a real `xshelld` process on the Host; the
//! new build is this test binary with another commit stamped in (see `ring_update.rs`).
mod common;
#[path = "desktop/workerd.rs"]
#[allow(dead_code)]
mod workerd;
use common::get_stub::GetStub;
use common::{alive, bin, closed_get_url, TestHome};
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
/// A Daemon from before `daemon.update-latest` (1.6.0-rjw.3), as far as a Desktop can tell.
const RJW3: (&str, &str) = ("XSHELLD_TEST_HIDE_CAPABILITIES", "daemon.update-latest");
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

/// The next version after this build's (its last digit plus one): same length, so it can
/// replace this build's version in the binary.
fn next_version() -> String {
    let last = VERSION.chars().last().unwrap();
    let d = last.to_digit(10).expect("the version ends in a digit");
    assert!(
        d < 9,
        "next_version: {VERSION} ends in 9; pick another way to make a newer version"
    );
    format!("{}{}", &VERSION[..VERSION.len() - 1], d + 1)
}

/// The release a Ring Host downloads: [`patched`] with every occurrence of this build's
/// version replaced by [`next_version`], checked through its own `--version`.
fn released(commit: &str) -> (Vec<u8>, BuildId, String) {
    let (mut bytes, build) = patched(commit);
    let next = next_version();
    let (from, to) = (VERSION.as_bytes(), next.as_bytes());
    let mut i = 0;
    let mut n = 0;
    while let Some(at) = bytes[i..].windows(from.len()).position(|w| w == from) {
        let start = i + at;
        bytes[start..start + to.len()].copy_from_slice(to);
        i = start + to.len();
        n += 1;
    }
    assert!(n > 0, "no version string in the build");
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("xshelld");
    fs::write(&p, &bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
    let mut c = Command::new(&p);
    c.arg("--version").stdout(Stdio::piped());
    let out = common::spawn_retrying(&mut c)
        .unwrap()
        .wait_with_output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["version"], serde_json::json!(next), "{v}");
    assert_eq!(v["build"]["commit"], serde_json::json!(commit), "{v}");
    (bytes, build, next)
}

fn triple() -> &'static str {
    xshell_protocol::build::host_triple("Linux", std::env::consts::ARCH).unwrap()
}

/// A Desktop's build source that must never be asked: a release update sends no bytes.
struct NoFetch;
impl BinarySource for NoFetch {
    fn fetch(&self, w: &Wanted, _: &CancelToken) -> Result<Fetched, String> {
        panic!("a release update must not fetch this Desktop's build ({w:?})")
    }
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
        Self::with_source(relay, build, Arc::new(Bytes(bytes)))
    }

    fn with_source(relay: &Relay, build: BuildId, source: Arc<dyn BinarySource>) -> Desktop {
        let home = TestHome::new();
        let mut cfg = Config::new(home.home(), home.paths());
        // Never the real release server.
        cfg.get_base = closed_get_url();
        cfg.ring_backoff_unit = Duration::from_millis(10);
        let daemon = Server::start(cfg).unwrap();
        let mut rc = DesktopRingConfig::new(home.root().join("desktop"), "desk".into());
        rc.default_relay_url = relay.url();
        rc.owner_retry = Duration::from_millis(20);
        rc.backoff_unit = Duration::from_millis(10);
        let ring = DesktopRing::open(rc, Arc::new(Quiet));
        let events = Arc::new(Events::default());
        let mut mc = ManagerConfig::new(VERSION, Arc::new(NoSsh), source, events.clone());
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
    /// `serve` from this home's installed copy of this build. `XSHELL_GET_URL` is a closed
    /// local port unless `env` names a stub.
    fn spawn(h: &TestHome, env: &[(&str, &str)]) -> Child {
        let own = install_dir_name(VERSION, Some(&own_build()));
        let exe = xshell_core::server::server_root(&h.home())
            .join(&own)
            .join("xshelld");
        let mut c = Command::new(&exe);
        c.arg("serve")
            .arg("--interactive-env")
            .env("HOME", h.home())
            .env("XDG_RUNTIME_DIR", h.run())
            .env("XSHELLD_LOGIN_ENV", "0")
            .env("XSHELL_SERVICE_MECHANISM", "none")
            .env("XSHELLD_TEST_TRIAL_SETTLE_MS", "800")
            .env("XSHELLD_TEST_TRIAL_TERM_MS", "1000")
            .env("XSHELL_GET_URL", closed_get_url())
            .env_remove("XSHELLD_IDLE_TIMEOUT_MS")
            .env_remove("XSHELLD_HOME")
            .env_remove("XSHELLD_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        let child = common::spawn_retrying(&mut c).unwrap();
        common::connect_socket(&h.paths().socket);
        child
    }

    /// End this machine's Daemon and start it again with `env` (another build, as far as
    /// a Desktop can tell, when `env` hides capabilities).
    fn restart(&mut self, env: &[(&str, &str)]) {
        if let Some(mut c) = self.child.take() {
            unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
            let _ = c.wait();
        }
        self.child = Some(Self::spawn(&self.h, env));
    }

    fn start(relay: &Relay, desktop: &Desktop, env: &[(&str, &str)]) -> (Machine, SignKey) {
        let h = TestHome::new();
        let own = install_dir_name(VERSION, Some(&own_build()));
        xshell_hostlink::install::install_local(stripped(), &h.home(), &own).unwrap();
        let child = Self::spawn(&h, env);
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
    // A Host from before `daemon.update-latest` (1.6.0-rjw.3) takes this Desktop's build.
    let (m, key) = Machine::start(&relay, &desktop, &[RJW3]);
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
    let (m, key) = Machine::start(&relay, &desktop, &[RJW3]);
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
    let (m, key) = Machine::start(&relay, &desktop, &[RJW3]);
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

// ---- Ring Hosts that update themselves from the latest release (D4, D5) ----

fn ver(v: &str) -> semver::Version {
    semver::Version::parse(v).unwrap()
}

/// What the Host offers: the action, the version it leads to and who supplies it.
fn offer(
    s: &HostStatus,
) -> (
    Option<xshell_hostlink::status::UpdateAction>,
    Option<String>,
    Option<xshell_hostlink::UpdateSource>,
) {
    (s.update_action, s.update_version.clone(), s.update_source)
}

fn release_offer(
    v: &str,
) -> (
    Option<xshell_hostlink::status::UpdateAction>,
    Option<String>,
    Option<xshell_hostlink::UpdateSource>,
) {
    (
        Some(xshell_hostlink::status::UpdateAction::Update),
        Some(v.to_string()),
        Some(xshell_hostlink::UpdateSource::Release),
    )
}

fn selected_of(m: &Machine) -> Option<String> {
    fs::read_to_string(xshell_core::server::server_root(&m.h.home()).join("selected"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn phases(d: &Desktop, host: &str) -> Vec<xshell_hostlink::Phase> {
    d.events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.host == host)
        .filter_map(|s| s.phase)
        .collect()
}

/// A capable Ring Host is outdated only against the latest release: another Desktop build
/// is no reason to offer anything, and an equal release neither.
#[test]
fn capable_host_offers_release_update_only_when_newer() {
    let relay = Relay::start();
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build, bytes);
    let (_m, key) = Machine::start(&relay, &desktop, &[]);
    let host = desktop.host(key);
    let s = host.status();
    assert!(
        s.daemon_capabilities
            .iter()
            .any(|c| c == "daemon.update-latest"),
        "{s:?}"
    );
    assert_eq!(s.status, xshell_hostlink::StatusKind::Connected, "{s:?}");
    assert_eq!(offer(&s), (None, None, None));
    desktop.manager.set_latest_release(ver(VERSION));
    std::thread::sleep(Duration::from_millis(300));
    let s = host.status();
    assert_eq!(s.status, xshell_hostlink::StatusKind::Connected, "{s:?}");
    assert_eq!(offer(&s), (None, None, None));
    let next = next_version();
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        let s = host.status();
        s.status == xshell_hostlink::StatusKind::UpgradePending && offer(&s) == release_offer(&next)
    });
    // An unknown latest release would offer nothing; a known one stays (this run).
    assert_eq!(desktop.manager.latest_release(), Some(ver(&next)));
}

/// D5 end to end over the Relay: the Host downloads the release from the (stub) release
/// server, installs and restarts into it in the same process; the Desktop sends no bytes.
#[test]
fn capable_host_updates_from_release() {
    let relay = Relay::start();
    let stub = GetStub::start();
    let (rel, rel_build, next) = released("0123456789ab");
    stub.release(&next, triple(), &rel, None);
    let desk_build = BuildId {
        commit: "bbbbbbbbbbbb".into(),
        ..own_build()
    };
    let desktop = Desktop::with_source(&relay, desk_build, Arc::new(NoFetch));
    let url = stub.url();
    let (m, key) = Machine::start(&relay, &desktop, &[("XSHELL_GET_URL", &url)]);
    let host = desktop.host(key);
    let (t, _sink, old_terminal) = open_shell(&host, &m.h.project("p"));
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
    let pid = m.pid();
    let before = host.status().link_generation;
    reply(|w| host.upgrade(w)).unwrap_or_else(|e| panic!("{e:?}\n{}", m.log()));
    // The latest value moves on after the Host answered: the running update still expects
    // the version and build the Host installed.
    desktop.manager.set_latest_release(ver("99.0.0"));
    let dir = install_dir_name(&next, Some(&rel_build));
    wait("updated and connected again", || {
        let s = host.status();
        s.usable()
            && s.phase.is_none()
            && s.link_generation > before
            && s.daemon_version.as_deref() == Some(next.as_str())
            && s.daemon_build.as_ref() == Some(&rel_build)
    });
    let s = host.status();
    assert_eq!(s.last_error, None, "{s:?}");
    assert_eq!(offer(&s), release_offer("99.0.0"));
    assert!(alive(pid));
    assert_eq!(exe_of(pid), m.installed(&dir), "{}", m.log());
    wait("Terminal back under its UUID", || {
        host.terminals().is_some_and(|l| {
            l.iter()
                .any(|i| i.terminal == t && i.pid.is_some() && i.pid != Some(old_terminal))
        })
    });
    wait("trial accepted", || m.trial_closed());
    assert_eq!(exe_of(pid), m.installed(&dir));
    let seen = phases(&desktop, host.id());
    assert!(
        seen.contains(&xshell_hostlink::Phase::Preparing),
        "{seen:?}"
    );
    assert!(
        seen.contains(&xshell_hostlink::Phase::Upgrading),
        "{seen:?}"
    );
    assert!(
        !seen.contains(&xshell_hostlink::Phase::Installing),
        "nothing was sent: {seen:?}"
    );
    assert_eq!(stub.count(&GetStub::asset(&next, triple())), 1);
}

/// The Host finds nothing newer (this Desktop's latest value is stale): the update ends
/// without a restart and without an error.
#[test]
fn capable_host_current_answer_finishes_quietly() {
    let relay = Relay::start();
    let stub = GetStub::start();
    stub.body("/latest", format!("{VERSION}\n"));
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build, bytes);
    let url = stub.url();
    let (m, key) = Machine::start(&relay, &desktop, &[("XSHELL_GET_URL", &url)]);
    let host = desktop.host(key);
    let next = next_version();
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
    let pid = m.pid();
    let before = host.status().link_generation;
    assert_eq!(reply(|w| host.upgrade(w)).unwrap(), serde_json::Value::Null);
    let s = host.status();
    assert_eq!(s.link_generation, before, "no restart");
    assert_eq!(
        (s.phase, s.last_error.clone(), s.error_hint),
        (None, None, None)
    );
    assert!(s.usable());
    assert!(alive(pid));
    assert_eq!(
        exe_of(pid),
        m.installed(&install_dir_name(VERSION, Some(&own_build())))
    );
    assert!(
        stub.hits().iter().all(|h| h == "/latest"),
        "{:?}",
        stub.hits()
    );
    // The answer withdraws the offer while this Desktop's latest value stays: nothing to
    // wait for, nothing on the bar.
    let s = host.status();
    assert_eq!(s.status, xshell_hostlink::StatusKind::Connected, "{s:?}");
    assert_eq!(offer(&s), (None, None, None));
    let e = reply(|w| host.upgrade(w)).unwrap_err();
    assert!(e.message.contains("does not need an update"), "{e:?}");
    // The same value again changes nothing; a newer one is offered again.
    desktop.manager.set_latest_release(ver(&next));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(offer(&host.status()), (None, None, None));
    desktop.manager.set_latest_release(ver("99.0.0"));
    wait("offered again", || {
        let s = host.status();
        s.status == xshell_hostlink::StatusKind::UpgradePending
            && offer(&s) == release_offer("99.0.0")
    });
}

/// Every failure of the download leaves the Host running its build, reachable, with nothing
/// installed, and says so in approved words (amendments 2 and 7).
#[test]
fn capable_host_failed_download_keeps_running() {
    let relay = Relay::start();
    let stub = GetStub::start();
    let (rel, rel_build, next) = released("0123456789ab");
    let (bytes, build) = patched("bbbbbbbbbbbb");
    let desktop = Desktop::new(&relay, build, bytes);
    let url = stub.url();
    let (m, key) = Machine::start(&relay, &desktop, &[("XSHELL_GET_URL", &url)]);
    let host = desktop.host(key);
    let name = name_of(&desktop, host.id());
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
    let pid = m.pid();
    let own = install_dir_name(VERSION, Some(&own_build()));
    let check = |what: &str, want: String| {
        let before = host.status().link_generation;
        let e = reply(|w| host.upgrade(w)).unwrap_err();
        assert_eq!(e.message, want, "{what}");
        let s = host.status();
        assert_eq!(s.last_error.as_deref(), Some(want.as_str()), "{what}");
        assert_eq!(
            s.error_hint,
            Some(xshell_hostlink::HostErrorHint::UpdateFailed),
            "{what}"
        );
        assert!(s.usable() && s.phase.is_none(), "{what}: {s:?}");
        assert_eq!(s.link_generation, before, "{what}: no restart");
        assert_eq!(s.daemon_build.as_ref(), Some(&own_build()), "{what}");
        assert!(alive(pid), "{what}");
        assert_eq!(exe_of(pid), m.installed(&own), "{what}");
        assert!(
            !m.installed(&install_dir_name(&next, Some(&rel_build)))
                .exists(),
            "{what}: nothing installed"
        );
        assert_ne!(
            selected_of(&m).as_deref(),
            Some(install_dir_name(&next, Some(&rel_build)).as_str())
        );
    };
    // A wrong checksum.
    stub.release(&next, triple(), &rel, Some(&"00".repeat(32)));
    check("checksum", xshell_hostlink::handle::release_bad(&name));
    // A build that reports another version than the release's.
    let (wrong, _) = patched("0123456789ab");
    stub.release(&next, triple(), &wrong, None);
    check("version", xshell_hostlink::handle::release_bad(&name));
    // No build for this machine type.
    stub.release(&next, triple(), &rel, None);
    stub.remove(&GetStub::asset(&next, triple()));
    check(
        "no build",
        xshell_hostlink::handle::RING_NO_BUILD.to_string(),
    );
    // The release server fails.
    stub.set("/latest", common::get_stub::Route::Status(500));
    check(
        "server error",
        xshell_hostlink::handle::release_download_failed(&name),
    );
}

/// Sol 5: the release does not come up; the Host goes back to its build with its live
/// Terminal restored, records the failure, and the Desktop says so in the approved words.
#[test]
fn relay_release_update_rolled_back_is_reported() {
    let relay = Relay::start();
    let stub = GetStub::start();
    let (rel, rel_build, next) = released("0123456789ab");
    stub.release(&next, triple(), &rel, None);
    let dir = install_dir_name(&next, Some(&rel_build));
    let desktop = Desktop::with_source(&relay, own_build(), Arc::new(NoFetch));
    let url = stub.url();
    let (m, key) = Machine::start(
        &relay,
        &desktop,
        &[
            ("XSHELL_GET_URL", &url),
            ("XSHELLD_TEST_FAIL_RESTORE", &dir),
        ],
    );
    let host = desktop.host(key);
    let own = install_dir_name(VERSION, Some(&own_build()));
    let (t, _sink, _) = open_shell(&host, &m.h.project("p"));
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
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
            xshell_hostlink::handle::ring_rolled_back(&name_of(&desktop, host.id()), VERSION)
                .as_str()
        )
    );
    assert_eq!(
        s.error_hint,
        Some(xshell_hostlink::HostErrorHint::UpdateFailed)
    );
    // Still outdated against the release: offered again.
    assert_eq!(offer(&s), release_offer(&next));
    wait("trial closed", || m.trial_closed());
    assert_eq!(selected_of(&m).as_deref(), Some(own.as_str()));
    // Whichever process serves now (a rollback may start a new one), it runs the old build.
    let now: i32 = fs::read_to_string(m.h.paths().pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(exe_of(now), m.installed(&own));
    // The failure is recorded on the Host.
    let mut c = common::Client::connect(&m.h.paths().socket);
    c.hello(common::range(1, 1));
    let info = c
        .request(&xshell_protocol::msg::ClientMsg::DaemonUpdateInfo)
        .unwrap();
    assert_eq!(
        info["lastFailure"]["target"],
        serde_json::json!(dir),
        "{info}"
    );
    // The live Terminal is there again under the old build.
    wait("Terminal restored under the old build", || {
        host.terminals()
            .is_some_and(|l| l.iter().any(|i| i.terminal == t && i.pid.is_some()))
    });
}

/// Sol 4: a Host busy with another update (here a fetch over its local socket) refuses the
/// Desktop's in the approved words, and nothing restarts.
#[test]
fn relay_release_update_busy_is_reported() {
    let relay = Relay::start();
    let stub = GetStub::start();
    let (rel, _, next) = released("0123456789ab");
    stub.release(&next, triple(), &rel, None);
    let asset = GetStub::asset(&next, triple());
    stub.gate(&asset);
    let desktop = Desktop::with_source(&relay, own_build(), Arc::new(NoFetch));
    let url = stub.url();
    let (m, key) = Machine::start(&relay, &desktop, &[("XSHELL_GET_URL", &url)]);
    let host = desktop.host(key);
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
    let mut local = common::Client::connect(&m.h.paths().socket);
    local.hello(common::range(1, 1));
    local.timeout = WAIT;
    let rid = local.request_id();
    local.send(
        &xshell_protocol::msg::ClientMsg::DaemonUpdateLatest,
        Some(rid),
    );
    stub.wait_hits(&asset, 1, WAIT);
    let before = host.status().link_generation;
    let e = reply(|w| host.upgrade(w)).unwrap_err();
    assert_eq!(
        e.message,
        xshell_hostlink::handle::ring_busy(&name_of(&desktop, host.id()))
    );
    let s = host.status();
    assert!(s.usable() && s.phase.is_none(), "{s:?}");
    assert_eq!(s.link_generation, before);
    stub.open(&asset);
    assert_eq!(
        local.wait_res(rid).unwrap()["current"],
        serde_json::json!(false)
    );
    assert_eq!(
        stub.count(&asset),
        1,
        "the Desktop's request downloaded nothing"
    );
}

/// Sol 4: the link reconnects while the Host downloads; the stale operation never restarts
/// the Host.
#[test]
fn relay_release_update_reconnect_during_request_does_not_restart() {
    let mut relay = Relay::start();
    let stub = GetStub::start();
    let (rel, rel_build, next) = released("0123456789ab");
    stub.release(&next, triple(), &rel, None);
    let asset = GetStub::asset(&next, triple());
    stub.gate(&asset);
    let desktop = Desktop::with_source(&relay, own_build(), Arc::new(NoFetch));
    let url = stub.url();
    let (m, key) = Machine::start(&relay, &desktop, &[("XSHELL_GET_URL", &url)]);
    let host = desktop.host(key);
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
    let pid = m.pid();
    let before = host.status().link_generation;
    let (tx, rx) = std::sync::mpsc::channel();
    host.upgrade(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    stub.wait_hits(&asset, 1, WAIT);
    relay.restart();
    wait("connected again on a new link", || {
        let s = host.status();
        s.usable() && s.link_generation > before
    });
    stub.open(&asset);
    let r = rx.recv_timeout(Duration::from_secs(120)).unwrap();
    assert!(r.is_err(), "{r:?}");
    // Give a wrongly issued restart time to show.
    std::thread::sleep(Duration::from_secs(2));
    let own = install_dir_name(VERSION, Some(&own_build()));
    assert!(alive(pid));
    assert_eq!(exe_of(pid), m.installed(&own), "{}", m.log());
    assert_ne!(
        selected_of(&m).as_deref(),
        Some(install_dir_name(&next, Some(&rel_build)).as_str())
    );
    assert!(
        !phases(&desktop, host.id()).contains(&xshell_hostlink::Phase::Upgrading),
        "no restart was requested"
    );
    let s = host.status();
    assert!(s.usable() && s.phase.is_none(), "{s:?}");
    assert_eq!(s.daemon_version.as_deref(), Some(VERSION));
}

/// Sol 4: a Host that comes back without `daemon.update-latest` (rolled back to an older
/// build) is offered this Desktop's build again.
#[test]
fn host_back_without_capability_is_offered_the_desktop_build() {
    let relay = Relay::start();
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build, bytes);
    let (mut m, key) = Machine::start(&relay, &desktop, &[]);
    let host = desktop.host(key);
    let next = next_version();
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
    let before = host.status().link_generation;
    m.restart(&[RJW3]);
    wait("back without the capability", || {
        let s = host.status();
        s.usable()
            && s.link_generation > before
            && !s
                .daemon_capabilities
                .iter()
                .any(|c| c == "daemon.update-latest")
    });
    let s = host.status();
    assert_eq!(s.status, xshell_hostlink::StatusKind::BuildDiffers, "{s:?}");
    assert_eq!(
        offer(&s),
        (
            Some(xshell_hostlink::status::UpdateAction::Replace),
            Some(VERSION.to_string()),
            Some(xshell_hostlink::UpdateSource::Desktop)
        )
    );
}

/// Sol 4: the broker view a follower window reads shows the owner's release offer, and
/// keeps refusing an update itself.
#[test]
fn broker_view_shows_release_offer_and_refuses_update() {
    let relay = Relay::start();
    let (bytes, build) = patched("0123456789ab");
    let desktop = Desktop::new(&relay, build, bytes);
    let (_m, key) = Machine::start(&relay, &desktop, &[]);
    let host = desktop.host(key);
    let id = host.id().to_string();
    let ui = desktop.manager.access(&id).unwrap();
    assert!(matches!(ui, xshell_hostlink::HostAccess::Broker(_)));
    let next = next_version();
    desktop.manager.set_latest_release(ver(&next));
    wait("the broker publishes the release offer", || {
        offer(&ui.snapshot().status) == release_offer(&next)
    });
    let seen = ui.snapshot().status;
    let target = xshell_hostlink::handle::UpdateTarget {
        config_generation: seen.config_generation,
        link_generation: seen.link_generation,
        daemon_version: seen.daemon_version.clone(),
        daemon_build: seen.daemon_build.clone(),
    };
    let refused = reply(|w| ui.upgrade_target(Some(target), w)).unwrap_err();
    assert!(refused.message.contains("broker"), "{refused:?}");
    // What it reads is what a follower process receives: the fields survive the wire.
    let wire = serde_json::to_value(&seen).unwrap();
    assert_eq!(wire["updateVersion"], serde_json::json!(next));
    assert_eq!(wire["updateSource"], serde_json::json!("release"));
}

/// Mario's sequence: an rjw.3 Host is updated by the old path; while that update is still on
/// trial, the release is offered and Update is clicked. The Host refuses at once as busy
/// (approved sentence, within seconds), keeps running the first target, and the same
/// Update succeeds once the trial is accepted.
#[test]
fn release_update_during_open_trial_reports_busy_then_succeeds() {
    let relay = Relay::start();
    let stub = GetStub::start();
    let (rel, rel_build, next) = released("0123456789ab");
    stub.release(&next, triple(), &rel, None);
    let (bytes, first) = patched("aaaaaaaaaaaa");
    let desktop = Desktop::new(&relay, first.clone(), bytes);
    let url = stub.url();
    let own = install_dir_name(VERSION, Some(&own_build()));
    let (m, key) = Machine::start(
        &relay,
        &desktop,
        &[
            ("XSHELL_GET_URL", &url),
            // Only the first build is "rjw.3"; the build it is updated to offers the release.
            ("XSHELLD_TEST_HIDE_CAPABILITIES", "daemon.update-latest"),
            ("XSHELLD_TEST_HIDE_CAPABILITIES_ONLY_IN", &own),
            // The first update stays on trial long enough to update again meanwhile.
            ("XSHELLD_TEST_TRIAL_SETTLE_MS", "12000"),
        ],
    );
    let host = desktop.host(key);
    let name = name_of(&desktop, host.id());
    let before = host.status().link_generation;
    reply(|w| host.upgrade(w)).unwrap_or_else(|e| panic!("{e:?}\n{}", m.log()));
    let first_dir = install_dir_name(VERSION, Some(&first));
    wait("on the first target, offering the release", || {
        let s = host.status();
        s.usable()
            && s.phase.is_none()
            && s.link_generation > before
            && s.daemon_build.as_ref() == Some(&first)
            && s.daemon_capabilities
                .iter()
                .any(|c| c == "daemon.update-latest")
    });
    assert!(!m.trial_closed(), "the first update is still on trial");
    desktop.manager.set_latest_release(ver(&next));
    wait("release offered", || {
        offer(&host.status()) == release_offer(&next)
    });
    let pid = m.pid();
    let at = Instant::now();
    let e = reply(|w| host.upgrade(w)).unwrap_err();
    assert!(
        at.elapsed() < Duration::from_secs(10),
        "answered after {:?}",
        at.elapsed()
    );
    assert_eq!(
        e.message,
        xshell_hostlink::handle::ring_busy(&name),
        "{}",
        m.log()
    );
    let s = host.status();
    assert!(s.usable() && s.phase.is_none(), "{s:?}");
    assert_eq!(
        s.last_error.as_deref(),
        Some(xshell_hostlink::handle::ring_busy(&name).as_str())
    );
    assert_eq!(s.daemon_build.as_ref(), Some(&first));
    assert!(alive(pid));
    assert_eq!(exe_of(pid), m.installed(&first_dir));
    assert_eq!(
        stub.count(&GetStub::asset(&next, triple())),
        0,
        "nothing downloaded"
    );
    assert!(!m
        .installed(&install_dir_name(&next, Some(&rel_build)))
        .parent()
        .unwrap()
        .exists());
    // The trial is accepted; the same Update then goes through.
    wait("first trial accepted", || m.trial_closed());
    let before = host.status().link_generation;
    reply(|w| host.upgrade(w)).unwrap_or_else(|e| panic!("{e:?}\n{}", m.log()));
    wait("on the release", || {
        let s = host.status();
        s.usable()
            && s.phase.is_none()
            && s.link_generation > before
            && s.daemon_version.as_deref() == Some(next.as_str())
            && s.daemon_build.as_ref() == Some(&rel_build)
    });
    assert_eq!(host.status().last_error, None);
}

//! Build identity in action (#49) against real Daemons in a temp home: installs keyed by
//! build, the stamp check before an upload, and "Replace with this Desktop's build", for a
//! Remote Host (over a local `sh` transport) and for the Local Host's Persistent Daemon.
//!
//! A Daemon from before #49 is played by a copy of the workspace `xshelld` whose embedded
//! stamp is corrupted: it then reports no build, exactly like one built before #49. Patching
//! the binary is why these tests are Linux only.
#![cfg(target_os = "linux")]

mod common;
mod desktop;

use common::{alive, bin, DaemonGuard, TestHome};
use desktop::*;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use xshell_hostlink::transport::CommandSpec;
use xshell_hostlink::{
    BinarySource, CancelToken, Fetched, FileSource, HostConfig, LocalShellTransport, StatusKind,
    Transport, TransportFactory, Wanted,
};
use xshell_protocol::build::{install_dir_name, BuildId};

/// This workspace's build: the one `bin()` carries. `None` (built without git): the tests
/// have nothing to compare and pass vacuously.
fn ours() -> Option<BuildId> {
    xshelld::build_id::BUILD.clone()
}

/// A copy of the workspace `xshelld` at `dest` whose stamp's first commit digit is `digit`:
/// `b'z'` makes the stamp unparsable (the binary reports no build, as before #49); a hex
/// digit makes it another, known build ([`other_build`]).
fn patched_bin(dest: &Path, digit: u8) {
    let mut bytes = std::fs::read(bin()).unwrap();
    let stamp = xshelld::build_id::STAMP.as_bytes();
    let mut replacement = stamp.to_vec();
    replacement["@@XSHELL-BUILD-V1:".len()] = digit;
    let mut n = 0;
    let mut i = 0;
    while let Some(at) = bytes[i..].windows(stamp.len()).position(|w| w == stamp) {
        let at = i + at;
        bytes[at..at + stamp.len()].copy_from_slice(&replacement);
        i = at + stamp.len();
        n += 1;
    }
    assert!(n > 0, "the stamp is not in {}", bin());
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(dest, bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// The first commit digit that makes another build than ours, and that build.
fn other_build(ours: &BuildId) -> (u8, BuildId) {
    let digit = if ours.commit.starts_with('0') {
        b'1'
    } else {
        b'0'
    };
    let mut b = ours.clone();
    b.commit
        .replace_range(0..1, std::str::from_utf8(&[digit]).unwrap());
    (digit, b)
}

fn private_dirs(h: &TestHome) {
    use std::os::unix::fs::PermissionsExt;
    let x = h.home().join(".xshell");
    for d in [x.clone(), x.join("server")] {
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    for e in std::fs::read_dir(x.join("server")).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}

/// `serve` from `exe`, as some earlier `connect` started it. Reaped (and ended) on drop.
struct Old(Child);

impl Old {
    fn start(h: &TestHome, exe: &Path) -> Old {
        let c = Command::new(exe)
            .arg("serve")
            .env("HOME", h.home())
            .env("XDG_RUNTIME_DIR", h.run())
            .env("XSHELLD_LOGIN_ENV", "0")
            .env("XSHELLD_IDLE_TIMEOUT_MS", "60000")
            .env_remove("XSHELLD_HOME")
            .env_remove("XSHELLD_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        common::connect_socket(&h.paths().socket);
        Old(c)
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    fn wait_dead(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.0.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Old {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn daemon_pid(h: &TestHome) -> Option<i32> {
    std::fs::read_to_string(h.paths().pid)
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn exe_of(pid: i32) -> PathBuf {
    std::fs::read_link(format!("/proc/{pid}/exe")).unwrap()
}

// ── Remote Host ───────────────────────────────────────────────────────────

/// `sh -c` with the test home's environment; records what each command was for and, once
/// `legacy` holds a script, runs it instead of the managed `connect`.
struct Recording {
    env: Vec<(OsString, OsString)>,
    events: Arc<Mutex<Vec<String>>>,
    legacy: Arc<Mutex<Option<String>>>,
    old_pid: Arc<AtomicI32>,
}

impl Transport for Recording {
    fn command(&self, remote: &str) -> CommandSpec {
        let what = if remote.contains("/selected") {
            format!(
                "select (old Daemon alive: {})",
                alive(self.old_pid.load(Ordering::SeqCst))
            )
        } else if remote.contains("uname") {
            "probe".into()
        } else if remote.contains("cat >") {
            "upload".into()
        } else if remote.ends_with(" connect'") {
            "connect".into()
        } else {
            "other".into()
        };
        self.events.lock().unwrap().push(what.clone());
        let run = match (&*self.legacy.lock().unwrap(), what.as_str()) {
            (Some(script), "connect") => script.clone(),
            _ => remote.to_string(),
        };
        LocalShellTransport {
            env: self.env.clone(),
        }
        .command(&run)
    }
    fn describe(&self) -> String {
        "recording".into()
    }
}

struct RecordingFactory {
    env: Vec<(OsString, OsString)>,
    events: Arc<Mutex<Vec<String>>>,
    legacy: Arc<Mutex<Option<String>>>,
    old_pid: Arc<AtomicI32>,
}

impl TransportFactory for RecordingFactory {
    fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
        Box::new(Recording {
            env: self.env.clone(),
            events: self.events.clone(),
            legacy: self.legacy.clone(),
            old_pid: self.old_pid.clone(),
        })
    }
}

struct Counting(FileSource, Arc<AtomicUsize>);

impl BinarySource for Counting {
    fn fetch(&self, w: &Wanted, c: &CancelToken) -> Result<Fetched, String> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.fetch(w, c)
    }
}

struct Remote {
    d: Desk,
    events: Arc<Mutex<Vec<String>>>,
    legacy: Arc<Mutex<Option<String>>>,
    fetches: Arc<AtomicUsize>,
    old: Option<Old>,
    old_bin: PathBuf,
    _guard: DaemonGuard,
    home: TestHome,
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.d.m.shutdown();
    }
}

impl Remote {
    /// A Host where an earlier Desktop installed `~/.xshell/server/<version>/xshelld` (a
    /// binary patched with `digit`, see [`patched_bin`]) and its Daemon runs (`old_runs`);
    /// then this Desktop (`desktop_build`, its binary source `src`) connects.
    fn start(digit: u8, old_runs: bool, desktop_build: Option<BuildId>, src: PathBuf) -> Remote {
        let home = TestHome::new();
        let old_bin = home
            .home()
            .join(".xshell/server")
            .join(VERSION)
            .join("xshelld");
        patched_bin(&old_bin, digit);
        private_dirs(&home);
        let guard = DaemonGuard::new(&home);
        let old = old_runs.then(|| Old::start(&home, &old_bin));
        let events = Arc::new(Mutex::new(Vec::new()));
        let legacy = Arc::new(Mutex::new(None));
        let old_pid = Arc::new(AtomicI32::new(old.as_ref().map_or(0, Old::pid)));
        let factory = RecordingFactory {
            env: vec![
                ("HOME".into(), home.home().into()),
                ("XDG_RUNTIME_DIR".into(), home.run().into()),
                ("XSHELLD_LOGIN_ENV".into(), "0".into()),
                ("XSHELLD_IDLE_TIMEOUT_MS".into(), "5000".into()),
            ],
            events: events.clone(),
            legacy: legacy.clone(),
            old_pid,
        };
        let fetches = Arc::new(AtomicUsize::new(0));
        let rec = Recorder::new();
        let mut cfg = manager_config(
            Arc::new(factory),
            Arc::new(Counting(FileSource(src), fetches.clone())),
            rec.clone(),
        );
        cfg.desktop_build = desktop_build;
        let d = Desk::new(cfg, rec, host_config(None));
        Remote {
            d,
            events,
            legacy,
            fetches,
            old,
            old_bin,
            _guard: guard,
            home,
        }
    }

    fn server(&self) -> PathBuf {
        self.home.home().join(".xshell/server")
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

/// With its build known, the Desktop installs into `<version>+<build>` beside an earlier
/// plain-version install, connects through it, and never uploads again on reconnect. Nothing
/// is selected by an install.
#[test]
fn keyed_install_then_reconnect() {
    let Some(ours) = ours() else { return };
    let r = Remote::start(b'z', false, Some(ours.clone()), bin().into());
    let s = r.d.wait_usable();
    assert_eq!(s.status, StatusKind::Connected);
    assert_eq!(s.daemon_build.as_ref(), Some(&ours));
    assert_eq!(s.desktop_build.as_ref(), Some(&ours));
    let keyed = r.server().join(install_dir_name(VERSION, Some(&ours)));
    assert_eq!(
        std::fs::read(keyed.join("xshelld")).unwrap(),
        std::fs::read(bin()).unwrap()
    );
    // The serve that connect started is the keyed build.
    assert_eq!(exe_of(daemon_pid(&r.home).unwrap()), keyed.join("xshelld"));
    assert!(r.old_bin.is_file(), "the earlier install was touched");
    assert!(!r.server().join("selected").exists());
    assert_eq!(r.fetches.load(Ordering::SeqCst), 1);
    let n = r.d.rec.count();
    let ssh = r.d.host().child_pid().unwrap() as i32;
    unsafe { libc::kill(-ssh, libc::SIGKILL) };
    let (i, _) =
        r.d.rec
            .wait_status_from(n, "reconnecting", |s| s.status == StatusKind::Reconnecting);
    r.d.rec.wait_status_from(i, "connected again", usable);
    assert_eq!(r.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(r.events().iter().filter(|e| *e == "upload").count(), 1);
}

/// A binary that is not this Desktop's build is never uploaded; the Host says how to
/// rebuild it.
#[test]
fn stale_binary_is_refused_before_upload() {
    let Some(ours) = ours() else { return };
    let (digit, other) = other_build(&ours);
    let t = tempfile::tempdir().unwrap();
    let stale = t.path().join("xshelld-stale");
    patched_bin(&stale, digit);
    let r = Remote::start(b'z', false, Some(ours.clone()), stale.clone());
    let s = r.d.rec.wait_status("refused", |s| s.last_error.is_some());
    assert_eq!(
        s.last_error.as_deref(),
        Some(
            format!(
                "the bundled xshelld at {} reports {}; this Desktop expects {}; rebuild it: cargo build -p xshelld --profile release-daemon --target {}",
                stale.display(),
                other.display(),
                ours.display(),
                if cfg!(target_arch = "aarch64") {
                    "aarch64-unknown-linux-musl"
                } else {
                    "x86_64-unknown-linux-musl"
                }
            )
            .as_str()
        )
    );
    assert_eq!(
        s.error_hint,
        Some(xshell_hostlink::HostErrorHint::BinaryUnavailable)
    );
    assert!(r.fetches.load(Ordering::SeqCst) >= 1);
    assert!(
        !r.events().contains(&"upload".to_string()),
        "{:?}",
        r.events()
    );
    assert!(!r
        .server()
        .join(install_dir_name(VERSION, Some(&ours)))
        .exists());
}

/// Mario's case: a Daemon from before #49 (same version, no build in its hello) runs on the
/// Host. It is "build differs"; Replace installs this build, selects it while the old Daemon
/// still runs (C1), asks the old one to exit, and the Daemon that comes back is this build.
#[test]
fn replace_a_daemon_from_before_49() {
    let Some(ours) = ours() else { return };
    let mut r = Remote::start(b'z', true, Some(ours.clone()), bin().into());
    let s =
        r.d.rec
            .wait_status("build differs", |s| s.status == StatusKind::BuildDiffers);
    assert_eq!(s.daemon_version.as_deref(), Some(VERSION));
    assert_eq!(s.daemon_build, None);
    assert_eq!(s.desktop_build.as_ref(), Some(&ours));
    assert!(s.last_error.is_none());
    let old_pid = r.old.as_ref().unwrap().pid();
    assert_eq!(daemon_pid(&r.home), Some(old_pid));
    let dir = install_dir_name(VERSION, Some(&ours));
    assert!(!r.server().join("selected").exists());
    let before = r.events().len();
    let from = r.d.rec.count();
    upgrade(&r.d.host()).unwrap();
    let (_, s) = r.d.rec.wait_status_from(from, "replaced", |s| {
        s.status == StatusKind::Connected && s.phase.is_none()
    });
    assert_eq!(s.daemon_build.as_ref(), Some(&ours));
    assert!(s.last_error.is_none(), "{s:?}");
    assert!(
        r.old.as_mut().unwrap().wait_dead(),
        "the old Daemon still runs"
    );
    assert_eq!(
        std::fs::read_to_string(r.server().join("selected")).unwrap(),
        format!("{dir}\n")
    );
    let new = daemon_pid(&r.home).unwrap();
    assert_ne!(new, old_pid);
    assert_eq!(exe_of(new), r.server().join(&dir).join("xshelld"));
    // Installed (checked) first, then selected while the old Daemon still ran, then the
    // reconnect.
    let ev = r.events()[before..].to_vec();
    let at = |e: &str| {
        ev.iter()
            .position(|x| x == e)
            .unwrap_or_else(|| panic!("{e}: {ev:?}"))
    };
    assert!(
        at("probe") < at("select (old Daemon alive: true)"),
        "{ev:?}"
    );
    assert!(
        at("select (old Daemon alive: true)") < at("connect"),
        "{ev:?}"
    );
}

/// C4: another Desktop's pre-#49 connector wins the restart and starts its own build again,
/// ignoring `selected`. The Host stays "build differs" and says so; Replace can be repeated.
#[test]
fn legacy_connector_wins_the_restart() {
    let Some(ours) = ours() else { return };
    let mut r = Remote::start(b'z', true, Some(ours.clone()), bin().into());
    r.d.rec
        .wait_status("build differs", |s| s.status == StatusKind::BuildDiffers);
    // Like `connect` before #49: start its own `serve` until one holds the lock, then
    // bridge to it.
    *r.legacy.lock().unwrap() = Some(format!(
        r#"OLD='{old}'; d="$XDG_RUNTIME_DIR/xshell"; mine=" "; i=0
while [ $i -lt 100 ]; do
  i=$((i+1))
  setsid "$OLD" serve </dev/null >/dev/null 2>&1 &
  mine="$mine$! "
  j=0
  while [ $j -lt 20 ]; do
    j=$((j+1))
    cur=$(cat "$d/daemon.pid" 2>/dev/null)
    case "$mine" in *" $cur "*) [ -n "$cur" ] && [ -S "$d/daemon.sock" ] && exec "$OLD" connect;; esac
    sleep 0.05
  done
done
exit 1"#,
        old = r.old_bin.display()
    ));
    let from = r.d.rec.count();
    upgrade(&r.d.host()).unwrap();
    let (_, s) = r.d.rec.wait_status_from(from, "still different", |s| {
        s.status == StatusKind::BuildDiffers && s.last_error.is_some()
    });
    assert!(r.old.as_mut().unwrap().wait_dead());
    assert_eq!(s.daemon_build, None);
    assert_eq!(
        s.last_error.as_deref(),
        Some(
            format!(
                "xshelld {VERSION} (unknown build) is still running after replacement; expected {VERSION} ({}). Try Replace again. If this repeats, quit other xshell apps using this host.",
                ours.display()
            )
            .as_str()
        )
    );
    let legacy_pid = daemon_pid(&r.home).unwrap();
    assert_eq!(exe_of(legacy_pid), r.old_bin);
    // The other Desktop is gone: Replace again works.
    *r.legacy.lock().unwrap() = None;
    let from = r.d.rec.count();
    upgrade(&r.d.host()).unwrap();
    let (_, s) = r.d.rec.wait_status_from(from, "replaced", |s| {
        s.status == StatusKind::Connected && s.phase.is_none()
    });
    assert_eq!(s.daemon_build.as_ref(), Some(&ours));
    assert!(s.last_error.is_none());
    assert!(common::wait_dead(legacy_pid, Duration::from_secs(15)));
}

// ── Local Host (Persistent Daemon) ───────────────────────────────────────

/// C1 for the Local Host: an old Persistent Daemon runs and this build's directory does not
/// exist yet. Replace installs it, selects it, and the Daemon restarts from it.
#[test]
fn local_replace_installs_selects_and_restarts() {
    let Some(ours) = ours() else { return };
    let home = TestHome::new();
    let _guard = DaemonGuard::new(&home);
    let old_bin = home
        .home()
        .join(".xshell/server")
        .join(VERSION)
        .join("xshelld");
    patched_bin(&old_bin, b'z');
    private_dirs(&home);
    let mut old = Old::start(&home, &old_bin);
    let a = LocalDesk::with(
        &home,
        LocalOpts {
            persistent: true,
            build: Some(ours.clone()),
            ..Default::default()
        },
    );
    let s = a
        .rec
        .wait_status("build differs", |s| s.status == StatusKind::BuildDiffers);
    assert_eq!(s.daemon_build, None);
    let dir = install_dir_name(VERSION, Some(&ours));
    let server = home.home().join(".xshell/server");
    assert!(!server.join(&dir).exists());
    let from = a.rec.count();
    upgrade(&a.host()).unwrap();
    let (_, s) = a.rec.wait_status_from(from, "replaced", |s| {
        s.status == StatusKind::Connected && s.phase.is_none()
    });
    assert_eq!(s.daemon_build.as_ref(), Some(&ours));
    assert!(old.wait_dead());
    assert_eq!(
        std::fs::read_to_string(server.join("selected")).unwrap(),
        format!("{dir}\n")
    );
    assert_eq!(
        exe_of(daemon_pid(&home).unwrap()),
        server.join(&dir).join("xshelld")
    );
}

/// A stale sidecar refuses Replace before any Terminal is touched: the old Daemon keeps
/// running and nothing is installed or selected.
#[test]
fn local_replace_refuses_a_stale_sidecar() {
    let Some(ours) = ours() else { return };
    let (digit, other) = other_build(&ours);
    let home = TestHome::new();
    let _guard = DaemonGuard::new(&home);
    let old_bin = home
        .home()
        .join(".xshell/server")
        .join(VERSION)
        .join("xshelld");
    patched_bin(&old_bin, b'z');
    private_dirs(&home);
    let old = Old::start(&home, &old_bin);
    let t = tempfile::tempdir().unwrap();
    let sidecar = t.path().join("xshelld");
    patched_bin(&sidecar, digit);
    let a = LocalDesk::with(
        &home,
        LocalOpts {
            persistent: true,
            build: Some(ours.clone()),
            bin: Some(sidecar.clone()),
            ..Default::default()
        },
    );
    a.rec
        .wait_status("build differs", |s| s.status == StatusKind::BuildDiffers);
    let e = upgrade(&a.host()).unwrap_err();
    assert_eq!(
        e.message,
        format!(
            "the bundled xshelld at {} reports {}; this Desktop expects {}; rebuild it: cargo build -p xshelld",
            sidecar.display(),
            other.display(),
            ours.display()
        )
    );
    let s = a.rec.wait_status("the reason", |s| s.last_error.is_some());
    assert_eq!(s.status, StatusKind::BuildDiffers);
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(old.pid()));
    assert_eq!(daemon_pid(&home), Some(old.pid()));
    let server = home.home().join(".xshell/server");
    assert!(!server.join("selected").exists());
    assert!(!server.join(install_dir_name(VERSION, Some(&ours))).exists());
}

const SWITCH: Duration = Duration::from_secs(30);

fn mode(h: &TestHome) -> String {
    std::fs::read_to_string(h.paths().mode)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Turning the Persistent Daemon off while the app's `xshelld` is another build refuses
/// before anything stops: the running Daemon (this build) keeps its Terminals.
#[test]
fn local_switch_refuses_a_stale_sidecar_before_stopping() {
    let Some(ours) = ours() else { return };
    let (digit, _) = other_build(&ours);
    let home = TestHome::new();
    let _guard = DaemonGuard::new(&home);
    let running = Old::start(&home, Path::new(bin()));
    let t = tempfile::tempdir().unwrap();
    let sidecar = t.path().join("xshelld");
    patched_bin(&sidecar, digit);
    let a = LocalDesk::with(
        &home,
        LocalOpts {
            persistent: true,
            build: Some(ours.clone()),
            bin: Some(sidecar),
            ..Default::default()
        },
    );
    let s = a.wait_usable();
    assert_eq!(s.status, StatusKind::Connected);
    let e = a.daemon.switch(&a.host(), false, 0, SWITCH).unwrap_err();
    let xshell_hostlink::SwitchError::Failed(m) = e else {
        panic!("{e:?}")
    };
    assert!(m.contains("rebuild it: cargo build -p xshelld"), "{m}");
    assert!(a.daemon.persistent());
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(running.pid()));
    assert_eq!(daemon_pid(&home), Some(running.pid()));
    assert_eq!(mode(&home), "persistent");
    assert_eq!(a.daemon.gui_pid(), None);
}

/// A Persistent Daemon of another build ("Different build") is usable: switching it off
/// hands its Terminals to a GUI-bound child of this build.
#[test]
fn local_switch_off_from_build_differs() {
    let Some(ours) = ours() else { return };
    let home = TestHome::new();
    let _guard = DaemonGuard::new(&home);
    let old_bin = home
        .home()
        .join(".xshell/server")
        .join(VERSION)
        .join("xshelld");
    patched_bin(&old_bin, b'z');
    private_dirs(&home);
    let mut old = Old::start(&home, &old_bin);
    let a = LocalDesk::with(
        &home,
        LocalOpts {
            persistent: true,
            build: Some(ours.clone()),
            ..Default::default()
        },
    );
    a.rec
        .wait_status("build differs", |s| s.status == StatusKind::BuildDiffers);
    a.daemon.switch(&a.host(), false, 0, SWITCH).unwrap();
    assert!(!a.daemon.persistent());
    assert!(old.wait_dead());
    assert_eq!(mode(&home), "gui-bound");
    let gui = a.daemon.gui_pid().expect("a GUI-bound child");
    assert_eq!(daemon_pid(&home), Some(gui as i32));
    a.rec.wait_status("this build, GUI-bound", |s| {
        s.status == StatusKind::Connected && s.daemon_build.as_ref() == Some(&ours)
    });
}

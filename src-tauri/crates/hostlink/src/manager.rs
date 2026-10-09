//! Every configured Host. The frontend pushes the whole list (`configure`); the manager
//! keeps, restarts or stops each Host by id.

use crate::cancel::CancelToken;
use crate::config::{validate, validate_one, HostConfig};
use crate::handle::HostHandle;
use crate::install::{probe_override, probe_script, run_probe, target_triple, BinarySource};
use crate::link::LinkLimits;
use crate::status::{HostSnapshot, HostStatus, HostTestResult};
use crate::transport::TransportFactory;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use xshell_core::protocol::msg::{ProtocolRange, TerminalInfo};

/// Receives every status change and every `terminals` list. Called under a Host's lock, in
/// order; must not call back into the manager.
pub trait Observer: Send + Sync {
    fn status(&self, s: &HostStatus);
    fn terminals(&self, host: &str, list: &[TerminalInfo]);
}

pub struct ManagerConfig {
    pub desktop_version: String,
    pub ours: ProtocolRange,
    pub transports: Arc<dyn TransportFactory>,
    pub binaries: Arc<dyn BinarySource>,
    pub observer: Arc<dyn Observer>,
    /// 1 s in production; the backoff schedule is in these units.
    pub backoff_unit: Duration,
    /// A connection up this long resets the backoff.
    pub stable_after: Duration,
    pub hello_timeout: Duration,
    pub call_timeout: Duration,
    /// After `daemon.upgrade` is acknowledged, the old Daemon must close the link by then;
    /// otherwise it gets a SIGTERM through its pidfile.
    pub upgrade_close_timeout: Duration,
    /// `term.open`, `term.attach`, `term.close`, `term.update` and `daemon.upgrade`.
    pub term_timeout: Duration,
    pub link_limits: LinkLimits,
}

impl ManagerConfig {
    pub fn new(
        desktop_version: impl Into<String>,
        transports: Arc<dyn TransportFactory>,
        binaries: Arc<dyn BinarySource>,
        observer: Arc<dyn Observer>,
    ) -> Self {
        Self {
            desktop_version: desktop_version.into(),
            ours: xshell_core::protocol::PROTOCOL,
            transports,
            binaries,
            observer,
            backoff_unit: Duration::from_secs(1),
            stable_after: Duration::from_secs(10),
            hello_timeout: Duration::from_secs(30),
            call_timeout: Duration::from_secs(60),
            upgrade_close_timeout: Duration::from_secs(15),
            term_timeout: Duration::from_secs(30),
            link_limits: LinkLimits::default(),
        }
    }
}

/// Stopping is bounded: whatever has not finished by then is abandoned (its children are
/// already killed by the cancel).
const STOP_DEADLINE: Duration = Duration::from_secs(3);

pub struct Manager {
    mc: Arc<ManagerConfig>,
    hosts: Mutex<Vec<Arc<HostHandle>>>,
    configure_lock: Mutex<()>,
    config_gen: AtomicU64,
    shut: AtomicBool,
    /// Manager-wide work outside any Host (connection tests); cancelled by `shutdown`.
    cancel: CancelToken,
}

fn join_all(joins: Vec<JoinHandle<()>>, deadline: Instant) {
    for j in joins {
        while !j.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if j.is_finished() {
            let _ = j.join();
        }
    }
}

impl Manager {
    pub fn new(cfg: ManagerConfig) -> Self {
        Self {
            mc: Arc::new(cfg),
            hosts: Mutex::new(Vec::new()),
            configure_lock: Mutex::new(()),
            config_gen: AtomicU64::new(0),
            shut: AtomicBool::new(false),
            cancel: CancelToken::new(),
        }
    }

    fn next_gen(&self) -> u64 {
        self.config_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Idempotent. Unchanged Hosts keep running; a changed SSH target or daemon command
    /// restarts the Host but keeps its attachments (re-attached on the new link); removed
    /// Hosts are stopped.
    pub fn configure(&self, hosts: Vec<HostConfig>) -> Result<(), String> {
        validate(&hosts)?;
        let _one = self.configure_lock.lock().unwrap();
        if self.shut.load(Ordering::SeqCst) {
            return Ok(());
        }
        let current: HashMap<String, Arc<HostHandle>> = self
            .hosts
            .lock()
            .unwrap()
            .iter()
            .map(|h| (h.id().to_string(), h.clone()))
            .collect();
        let mut joins = Vec::new();
        let mut replaced = Vec::new();
        let mut next = Vec::new();
        for cfg in &hosts {
            match current.get(&cfg.id) {
                Some(h) => {
                    if cfg.connection_differs(&h.config()) {
                        joins.extend(h.stop_begin());
                        replaced.push((h.clone(), cfg.clone()));
                    } else {
                        h.set_display(cfg.clone());
                    }
                    next.push(h.clone());
                }
                None => {
                    let h = Arc::new(HostHandle::new(
                        cfg.clone(),
                        self.mc.clone(),
                        self.next_gen(),
                    ));
                    next.push(h);
                }
            }
        }
        for (id, h) in &current {
            if !hosts.iter().any(|c| &c.id == id) {
                joins.extend(h.stop_begin());
            }
        }
        join_all(joins, Instant::now() + STOP_DEADLINE);
        for (h, cfg) in &replaced {
            h.reset(cfg.clone(), self.next_gen());
        }
        // Serialized with `shutdown` by the hosts lock: either it already took the list
        // (and `shut` is set, so nothing starts) or it will take the new one.
        let mut hs = self.hosts.lock().unwrap();
        if self.shut.load(Ordering::SeqCst) {
            return Ok(());
        }
        for (h, _) in &replaced {
            h.start();
        }
        for h in &next {
            if !current.contains_key(h.id()) {
                h.start();
            }
        }
        *hs = next;
        Ok(())
    }

    pub fn host(&self, id: &str) -> Option<Arc<HostHandle>> {
        self.hosts
            .lock()
            .unwrap()
            .iter()
            .find(|h| h.id() == id)
            .cloned()
    }

    pub fn snapshot(&self) -> Vec<HostSnapshot> {
        let hosts = self.hosts.lock().unwrap().clone();
        hosts.iter().map(|h| h.snapshot()).collect()
    }

    pub fn kick_all(&self) {
        let hosts = self.hosts.lock().unwrap().clone();
        for h in hosts {
            h.kick();
        }
    }

    /// Probe only: SSH works, the platform, the installed version. Never installs or starts
    /// a Daemon.
    pub fn test(&self, cfg: &HostConfig) -> HostTestResult {
        let mut r = HostTestResult {
            ok: false,
            os: None,
            arch: None,
            triple: None,
            installed_version: None,
            error: None,
            error_hint: None,
        };
        if let Err(e) = validate_one(cfg) {
            r.error = Some(e);
            return r;
        }
        let t = self.mc.transports.for_host(cfg);
        if let Some(cmd) = cfg.daemon_override() {
            match probe_override(&*t, cmd, &self.cancel) {
                Ok(p) => {
                    r.ok = true;
                    r.installed_version = Some(p.installed.version);
                    if let (Some(os), Some(arch)) = (&p.os, &p.arch) {
                        r.triple = target_triple(os, arch).ok().map(Into::into);
                    }
                    r.os = p.os;
                    r.arch = p.arch;
                }
                Err(e) => {
                    r.error = Some(e.message);
                    r.error_hint = e.hint;
                }
            }
            return r;
        }
        let script = probe_script(&self.mc.desktop_version);
        match run_probe(&*t, &script, &self.cancel) {
            Ok(p) => {
                r.installed_version = p.installed.map(|d| d.version);
                match target_triple(&p.os, &p.arch) {
                    Ok(tr) => {
                        r.ok = true;
                        r.triple = Some(tr.into());
                    }
                    Err(e) => {
                        r.error = Some(e);
                        r.error_hint = Some(crate::errors::HostErrorHint::UnsupportedPlatform);
                    }
                }
                r.os = Some(p.os);
                r.arch = Some(p.arch);
            }
            Err(e) => {
                r.error = Some(e.message);
                r.error_hint = e.hint;
            }
        }
        r
    }

    /// Stop every Host in parallel: kill each ssh, join, and return within 3 s of entry
    /// whatever else is going on (a configure in progress, a sink blocked in a delivery).
    /// No new configuration starts anything afterwards. Daemons and their Terminals keep
    /// running. Idempotent.
    pub fn shutdown(&self) {
        let deadline = Instant::now() + STOP_DEADLINE;
        self.shut.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        let hosts: Vec<Arc<HostHandle>> = std::mem::take(&mut *self.hosts.lock().unwrap());
        let mut joins = Vec::new();
        for h in &hosts {
            h.retire();
            joins.extend(h.stop_begin());
        }
        join_all(joins, deadline);
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::test_host;
    use crate::install::testutil::Fails;
    use crate::status::StatusKind;
    use crate::transport::{CommandSpec, LocalShellTransport, Transport};
    use std::sync::Condvar;

    pub struct NullObserver;
    impl Observer for NullObserver {
        fn status(&self, _: &HostStatus) {}
        fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
    }

    #[derive(Default)]
    pub struct Recorder {
        pub st: Mutex<Vec<HostStatus>>,
        pub lists: Mutex<Vec<(String, Vec<TerminalInfo>)>>,
        cv: Condvar,
    }

    impl Recorder {
        pub fn new() -> Arc<Recorder> {
            Arc::new(Recorder::default())
        }
        pub fn statuses(&self) -> Vec<HostStatus> {
            self.st.lock().unwrap().clone()
        }
        pub fn wait_status(&self, pred: impl Fn(&HostStatus) -> bool) -> HostStatus {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut g = self.st.lock().unwrap();
            loop {
                if let Some(s) = g.iter().find(|s| pred(s)) {
                    return s.clone();
                }
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "no matching status in {:#?}", *g);
                g = self.cv.wait_timeout(g, left).unwrap().0;
            }
        }
    }

    impl Observer for Recorder {
        fn status(&self, s: &HostStatus) {
            self.st.lock().unwrap().push(s.clone());
            self.cv.notify_all();
        }
        fn terminals(&self, host: &str, list: &[TerminalInfo]) {
            self.lists
                .lock()
                .unwrap()
                .push((host.into(), list.to_vec()));
            self.cv.notify_all();
        }
    }

    /// Every remote command runs this local script instead.
    pub struct ScriptTransport(pub String);
    impl Transport for ScriptTransport {
        fn command(&self, _remote: &str) -> CommandSpec {
            LocalShellTransport::default().command(&self.0)
        }
        fn describe(&self) -> String {
            "script".into()
        }
    }

    pub struct ScriptFactory(pub String);
    impl TransportFactory for ScriptFactory {
        fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
            Box::new(ScriptTransport(self.0.clone()))
        }
    }

    pub fn script_factory(s: &str) -> Arc<dyn TransportFactory> {
        Arc::new(ScriptFactory(s.into()))
    }

    pub fn test_config(observer: Arc<dyn Observer>) -> ManagerConfig {
        let mut c = ManagerConfig::new(
            "1.5.0",
            script_factory("exit 1"),
            Arc::new(Fails("no binaries in this test")),
            observer,
        );
        c.backoff_unit = Duration::from_millis(10);
        c.stable_after = Duration::from_millis(500);
        c.hello_timeout = Duration::from_secs(5);
        c
    }

    #[test]
    fn configure_validates_and_diffs() {
        let rec = Recorder::new();
        let m = Manager::new(test_config(rec.clone()));
        assert!(m.configure(vec![test_host("local", "x")]).is_err());
        let a = test_host("h_aaaaaaaa", "a");
        let b = test_host("h_bbbbbbbb", "b");
        m.configure(vec![a.clone(), b.clone()]).unwrap();
        let ha = m.host("h_aaaaaaaa").unwrap();
        let gen_a = ha.status().config_generation;
        let gen_b = m.host("h_bbbbbbbb").unwrap().status().config_generation;
        assert_ne!(gen_a, gen_b);
        // Renaming keeps the running Host; retargeting replaces its configuration.
        let mut a2 = a.clone();
        a2.name = "renamed".into();
        let mut b2 = b.clone();
        b2.ssh_target = "b2".into();
        m.configure(vec![b2.clone(), a2.clone()]).unwrap();
        assert!(Arc::ptr_eq(&ha, &m.host("h_aaaaaaaa").unwrap()));
        assert_eq!(ha.config().name, "renamed");
        assert_eq!(ha.status().config_generation, gen_a);
        let hb = m.host("h_bbbbbbbb").unwrap();
        assert!(hb.status().config_generation > gen_b);
        assert_eq!(hb.config().ssh_target, "b2");
        let order: Vec<String> = m.snapshot().iter().map(|s| s.status.host.clone()).collect();
        assert_eq!(order, ["h_bbbbbbbb", "h_aaaaaaaa"]);
        // Removing stops it.
        m.configure(vec![a2]).unwrap();
        assert!(m.host("h_bbbbbbbb").is_none());
        assert_eq!(m.snapshot().len(), 1);
        assert!(m.snapshot()[0].terminals.is_none());
        m.shutdown();
        m.configure(vec![b]).unwrap();
        assert!(m.snapshot().is_empty());
        let _ = StatusKind::Connected;
    }

    #[cfg(unix)]
    #[test]
    fn test_reports_probe() {
        let mut mc = test_config(Arc::new(NullObserver));
        mc.transports = script_factory(
            "echo 'Welcome'; echo 'Linux aarch64'; echo @@XSHELL@@; echo '{\"name\":\"xshelld\",\"version\":\"1.4.0\",\"protocol\":{\"min\":1,\"max\":1}}'",
        );
        let m = Manager::new(mc);
        let r = m.test(&test_host("h_aaaaaaaa", "a"));
        assert_eq!(
            r,
            HostTestResult {
                ok: true,
                os: Some("Linux".into()),
                arch: Some("aarch64".into()),
                triple: Some("aarch64-unknown-linux-musl".into()),
                installed_version: Some("1.4.0".into()),
                error: None,
                error_hint: None,
            }
        );
        let mut mc = test_config(Arc::new(NullObserver));
        mc.transports = script_factory(
            "echo 'ssh: Could not resolve hostname x: Name or service not known' >&2; exit 255",
        );
        let r = Manager::new(mc).test(&test_host("h_aaaaaaaa", "a"));
        assert!(!r.ok);
        assert_eq!(r.error_hint, Some(crate::errors::HostErrorHint::Unresolved));
    }

    /// A Daemon-command host may allow only `<cmd> --version` / `<cmd> connect` over ssh
    /// (ForceCommand): the test must send exactly `<cmd> --version`, nothing else.
    #[cfg(unix)]
    #[test]
    fn test_with_daemon_command_sends_exactly_cmd_version() {
        struct Recording(Arc<Mutex<Vec<String>>>);
        impl TransportFactory for Recording {
            fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
                struct T(Arc<Mutex<Vec<String>>>);
                impl Transport for T {
                    fn command(&self, remote: &str) -> CommandSpec {
                        self.0.lock().unwrap().push(remote.to_string());
                        // Behave like a ForceCommand that only knows the exact form.
                        let script = if remote == "xshelld --version" {
                            r#"echo '{"name":"xshelld","version":"1.5.0","protocol":{"min":1,"max":1},"os":"Linux","arch":"x86_64"}'"#
                        } else {
                            "echo 'usage: agent <name>' >&2; exit 2"
                        };
                        LocalShellTransport::default().command(script)
                    }
                    fn describe(&self) -> String {
                        "recording".into()
                    }
                }
                Box::new(T(self.0.clone()))
            }
        }
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut mc = test_config(Arc::new(NullObserver));
        mc.transports = Arc::new(Recording(sent.clone()));
        let mut host = test_host("h_aaaaaaaa", "a");
        host.daemon_command = Some("xshelld".into());
        let r = Manager::new(mc).test(&host);
        assert_eq!(*sent.lock().unwrap(), vec!["xshelld --version".to_string()]);
        assert_eq!(
            r,
            HostTestResult {
                ok: true,
                os: Some("Linux".into()),
                arch: Some("x86_64".into()),
                triple: Some("x86_64-unknown-linux-musl".into()),
                installed_version: Some("1.5.0".into()),
                error: None,
                error_hint: None,
            }
        );
    }

    /// An older Daemon without os/arch in `--version` still passes; a refused command fails
    /// with the host's own message and the daemon-command hint.
    #[cfg(unix)]
    #[test]
    fn test_with_daemon_command_old_daemon_and_refusal() {
        let mut host = test_host("h_aaaaaaaa", "a");
        host.daemon_command = Some("xshelld".into());
        let mut mc = test_config(Arc::new(NullObserver));
        mc.transports = script_factory(
            r#"echo noise; echo '{"name":"xshelld","version":"1.4.0","protocol":{"min":1,"max":1}}'"#,
        );
        let r = Manager::new(mc).test(&host);
        assert!(r.ok);
        assert_eq!(r.installed_version.as_deref(), Some("1.4.0"));
        assert_eq!((r.os, r.arch, r.triple), (None, None, None));

        let mut mc = test_config(Arc::new(NullObserver));
        mc.transports = script_factory("echo 'usage: agent <name>' >&2; exit 2");
        let r = Manager::new(mc).test(&host);
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("usage: agent <name>"));
        assert_eq!(
            r.error_hint,
            Some(crate::errors::HostErrorHint::DaemonCommandFailed)
        );
    }

    /// Picks the local script by what the remote command is for.
    #[cfg(unix)]
    struct ByPurpose {
        probe: String,
        upload: String,
        connect: String,
    }

    #[cfg(unix)]
    impl TransportFactory for ByPurpose {
        fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
            struct T(String, String, String);
            impl Transport for T {
                fn command(&self, remote: &str) -> CommandSpec {
                    let s = if remote.contains("@@XSHELL@@") {
                        &self.0
                    } else if remote.contains("cat >") {
                        &self.1
                    } else {
                        &self.2
                    };
                    LocalShellTransport::default().command(s)
                }
                fn describe(&self) -> String {
                    "by-purpose".into()
                }
            }
            Box::new(T(
                self.probe.clone(),
                self.upload.clone(),
                self.connect.clone(),
            ))
        }
    }

    #[cfg(unix)]
    fn shutdown_during(
        probe: &str,
        upload: &str,
        connect: &str,
        override_: bool,
        phase_ok: impl Fn(&HostStatus) -> bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let pidf = dir.path().join("pid");
        let stall = format!("echo $$ > '{}'; exec sleep 1000", pidf.display());
        let pick = |s: &str| {
            if s == "STALL" {
                stall.clone()
            } else {
                s.to_string()
            }
        };
        let rec = Recorder::new();
        let mut mc = test_config(rec.clone());
        mc.transports = Arc::new(ByPurpose {
            probe: pick(probe),
            upload: pick(upload),
            connect: pick(connect),
        });
        // Bigger than any pipe buffer: an upload to a reader that never reads stalls.
        mc.binaries = Arc::new(crate::install::testutil::Bytes(vec![0u8; 8 << 20]));
        let m = Manager::new(mc);
        let mut h = test_host("h_aaaaaaaa", "a");
        if override_ {
            h.daemon_command = Some("xd".into());
        }
        m.configure(vec![h]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidf)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break p;
            }
            assert!(
                Instant::now() < deadline,
                "the stalling command never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        std::thread::sleep(Duration::from_millis(100));
        rec.wait_status(&phase_ok);
        let start = Instant::now();
        m.shutdown();
        assert!(
            start.elapsed() <= Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        // Reaped and gone (no zombie: kill(0) fails only once it is reaped).
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "child {pid} survived");
    }

    #[cfg(unix)]
    const PROBE_NOT_INSTALLED: &str = "echo 'Linux x86_64'; echo @@XSHELL@@";

    #[cfg(unix)]
    #[test]
    fn shutdown_during_probe() {
        shutdown_during("STALL", "exit 1", "exit 1", false, |s| {
            s.phase == Some(crate::status::Phase::Probing)
        });
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_during_upload() {
        shutdown_during(PROBE_NOT_INSTALLED, "STALL", "exit 1", false, |s| {
            s.phase == Some(crate::status::Phase::Installing)
        });
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_during_handshake() {
        shutdown_during("exit 1", "exit 1", "STALL", true, |s| {
            s.status == StatusKind::Reconnecting && s.phase.is_none()
        });
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_cancels_a_stalled_connection_test() {
        let dir = tempfile::tempdir().unwrap();
        let pidf = dir.path().join("pid");
        let mut mc = test_config(Arc::new(NullObserver));
        mc.transports = script_factory(&format!("echo $$ > '{}'; exec sleep 1000", pidf.display()));
        let m = Arc::new(Manager::new(mc));
        let m2 = m.clone();
        // An unsaved host: the probe runs outside every configured Host.
        let t = std::thread::spawn(move || m2.test(&test_host("h_cccccccc", "c")));
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidf)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break p;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        let start = Instant::now();
        m.shutdown();
        let r = t.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(!r.ok);
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "probe {pid} survived");
        // And nothing runs after the shutdown.
        assert!(!m.test(&test_host("h_cccccccc", "c")).ok);
    }

    /// A Daemon that completes the handshake and hangs up at once, every time.
    #[cfg(unix)]
    #[test]
    fn flapping_connection_goes_offline() {
        use crate::link::testpeer::{hello_frame, terminals_frame};
        let dir = tempfile::tempdir().unwrap();
        let frames = dir.path().join("frames");
        let mut b = hello_frame(1, 1, "1.5.0");
        b.extend(terminals_frame(vec![]));
        std::fs::write(&frames, b).unwrap();
        let rec = Recorder::new();
        let mut mc = test_config(rec.clone());
        mc.transports = script_factory(&format!("cat '{}'; sleep 0.05", frames.display()));
        mc.stable_after = Duration::from_secs(10);
        let m = Manager::new(mc);
        let mut h = test_host("h_dddddddd", "d");
        h.daemon_command = Some("xd".into());
        m.configure(vec![h]).unwrap();
        let s = rec.wait_status(|s| s.status == StatusKind::Offline);
        assert!(s.next_retry_at.is_some());
        let connected = rec
            .statuses()
            .iter()
            .filter(|s| s.status == StatusKind::Connected)
            .count();
        assert!(connected >= 3, "it did connect each time ({connected})");
        m.shutdown();
    }
}

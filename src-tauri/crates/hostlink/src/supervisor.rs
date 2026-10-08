//! One thread per Host: install, connect, handshake, re-attach, publish, park until the link
//! drops, back off, repeat. Every phase watches the run's cancel token.

use crate::cancel::CancelToken;
use crate::errors::HostError;
use crate::errors::{classify_ssh_failure, HostErrorHint};
use crate::handle::Shared;
use crate::install::{ensure_installed, kill_daemon_script, os_name};
use crate::link::{Link, LinkError, LinkIo};
use crate::process::{self, run_script, Proc};
use crate::status::{now_ms, IncompatibleReason, Phase, StatusKind};
use crate::transport::{connect_command, sh_wrap, Transport};
use crate::version::{self, classify, Classified};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BACKOFF: [u64; 7] = [1, 2, 4, 8, 16, 32, 60];
const FIRST_LIST_TIMEOUT: Duration = Duration::from_secs(5);
const OFFLINE_AFTER: u32 = 3;

/// The wait before retry `n` (0-based) in units: 1, 2, 4 … 32, then 60 for good.
pub fn backoff(n: u32, unit: Duration) -> Duration {
    unit * BACKOFF[(n as usize).min(BACKOFF.len() - 1)] as u32
}

/// Consecutive failures, reset only by a connection that stayed up long enough.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub failures: u32,
}

impl Backoff {
    /// A connect attempt failed: count it and return the wait before the next one.
    pub fn failed(&mut self, unit: Duration) -> Duration {
        self.failures += 1;
        backoff(self.failures - 1, unit)
    }

    /// An established connection ended after `up_for`: a stable one resets the count and
    /// retries at once; a short-lived one keeps the count and waits.
    pub fn ended(&mut self, up_for: Duration, stable_after: Duration, unit: Duration) -> Duration {
        if up_for >= stable_after {
            self.failures = 0;
            Duration::ZERO
        } else {
            backoff(self.failures, unit)
        }
    }
}

/// Run the pidfile SIGTERM script on the Host.
pub(crate) fn kill_daemon(t: &dyn Transport, cancel: &CancelToken) -> Result<(), HostError> {
    match run_script(
        t,
        &sh_wrap(&kill_daemon_script()),
        None,
        Duration::from_secs(30),
        cancel,
    ) {
        Ok(o) if o.code == Some(0) => Ok(()),
        Ok(o) => Err(HostError::offline(if o.stderr.is_empty() {
            format!("stopping the Daemon failed (exit {:?})", o.code)
        } else {
            o.stderr
        })),
        Err(e) => Err(HostError::offline(e.to_string())),
    }
}

struct Failure {
    message: String,
    hint: Option<HostErrorHint>,
    incompatible: Option<(IncompatibleReason, String)>,
    /// The installed binary looks missing: probe again next time.
    reinstall: bool,
}

struct Connected {
    link: Arc<Link>,
    proc: Proc,
    gen: u64,
    at: Instant,
}

enum Attempt {
    Connected(Connected),
    Failed(Failure),
    Cancelled,
}

enum Parked {
    Stop,
    Closed(String),
    UpgradeTimeout,
}

enum Waited {
    Stop,
    Kick,
    Elapsed,
}

struct Sup {
    sh: Arc<Shared>,
    cancel: CancelToken,
    transport: Box<dyn Transport>,
    managed: bool,
    override_: Option<String>,
    backoff: Backoff,
    install_verified: bool,
    last_gen: u64,
}

pub(crate) fn run(sh: Arc<Shared>, cancel: CancelToken) {
    let cfg = sh.lock().cfg.clone();
    let transport = sh.mc.transports.for_host(&cfg);
    let override_ = cfg.daemon_override().map(String::from);
    let mut s = Sup {
        sh,
        cancel,
        transport,
        managed: override_.is_none(),
        override_,
        backoff: Backoff::default(),
        install_verified: false,
        last_gen: 0,
    };
    s.run();
}

impl Sup {
    fn unit(&self) -> Duration {
        self.sh.mc.backoff_unit
    }

    fn set_phase(&self, p: Option<Phase>) {
        let mut st = self.sh.lock();
        st.status.phase = if st.upgrading {
            Some(p.unwrap_or(Phase::Upgrading))
        } else {
            p
        };
        self.sh.publish(&mut st);
    }

    fn run(&mut self) {
        let mut delay = Duration::ZERO;
        loop {
            if !delay.is_zero() {
                match self.wait(delay) {
                    Waited::Stop => break,
                    Waited::Kick | Waited::Elapsed => {}
                }
            }
            if self.cancel.is_cancelled() {
                break;
            }
            {
                let mut st = self.sh.lock();
                if st.status.status != StatusKind::Incompatible {
                    st.status
                        .set_kind(if self.backoff.failures < OFFLINE_AFTER {
                            StatusKind::Reconnecting
                        } else {
                            StatusKind::Offline
                        });
                }
                st.status.next_retry_at = None;
                st.kick = false;
                self.sh.publish(&mut st);
            }
            delay = match self.attempt() {
                Attempt::Cancelled => break,
                Attempt::Failed(f) => self.failed(f),
                Attempt::Connected(c) => match self.park(&c) {
                    Parked::Stop => {
                        c.link.close();
                        break;
                    }
                    Parked::Closed(why) => self.ended(c, why),
                    Parked::UpgradeTimeout => {
                        // The old Daemon did not exit by itself: SIGTERM it, then reconnect.
                        let _ = kill_daemon(&*self.transport, &self.cancel);
                        c.link.close();
                        self.ended(c, "the Daemon did not exit for the upgrade".into())
                    }
                },
            };
        }
        // Stopped: drop the link of our last generation (a reset may have fenced it).
        let link = {
            let mut st = self.sh.lock();
            if st.gen == self.last_gen {
                st.link.take()
            } else {
                None
            }
        };
        if let Some(l) = link {
            l.close();
        }
    }

    /// Wait on the Host's condvar until stopped, kicked, or `d` passes.
    fn wait(&self, d: Duration) -> Waited {
        let deadline = Instant::now() + d;
        let mut st = self.sh.lock();
        loop {
            if self.cancel.is_cancelled() {
                return Waited::Stop;
            }
            if st.kick {
                st.kick = false;
                return Waited::Kick;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Waited::Elapsed;
            }
            st = self.sh.cv.wait_timeout(st, left).unwrap().0;
        }
    }

    fn failed(&mut self, f: Failure) -> Duration {
        if f.reinstall {
            self.install_verified = false;
        }
        let mut delay = self.backoff.failed(self.unit());
        let mut st = self.sh.lock();
        match f.incompatible {
            Some((reason, version)) => {
                delay = backoff(u32::MAX, self.unit());
                st.status.set_kind(StatusKind::Incompatible);
                st.status.incompatible_reason = Some(reason);
                st.status.daemon_version = Some(version);
            }
            None => {
                st.status
                    .set_kind(if self.backoff.failures < OFFLINE_AFTER {
                        StatusKind::Reconnecting
                    } else {
                        StatusKind::Offline
                    });
                st.status.incompatible_reason = None;
            }
        }
        st.status.phase = st.upgrading.then_some(Phase::Upgrading);
        st.status.last_error = Some(f.message);
        st.status.error_hint = f.hint;
        st.status.protocol = None;
        st.status.next_retry_at = Some(now_ms() + delay.as_millis() as u64);
        self.sh.publish(&mut st);
        delay
    }

    fn ended(&mut self, c: Connected, why: String) -> Duration {
        let up_for = c.at.elapsed();
        drop(c.link);
        // ssh has usually exited with the stream; make sure, and reap it.
        c.proc.child.kill_and_wait(Duration::from_secs(2));
        let mut st = self.sh.lock();
        let delay = if st.upgrading {
            Duration::ZERO
        } else {
            self.backoff
                .ended(up_for, self.sh.mc.stable_after, self.unit())
        };
        st.link = None;
        st.status
            .set_kind(if self.backoff.failures < OFFLINE_AFTER {
                StatusKind::Reconnecting
            } else {
                StatusKind::Offline
            });
        st.status.last_error = Some(why);
        st.status.error_hint = None;
        st.status.next_retry_at = (!delay.is_zero()).then(|| now_ms() + delay.as_millis() as u64);
        self.sh.publish(&mut st);
        delay
    }

    fn park(&self, c: &Connected) -> Parked {
        let mut st = self.sh.lock();
        loop {
            if self.cancel.is_cancelled() || st.gen != c.gen {
                return Parked::Stop;
            }
            if st.link_closed {
                return Parked::Closed(
                    st.close_reason
                        .clone()
                        .unwrap_or_else(|| "connection lost".into()),
                );
            }
            // Connected: a kick has nothing to retry.
            st.kick = false;
            let now = Instant::now();
            let wait = match st.upgrade_deadline {
                Some(d) if d <= now => {
                    st.upgrade_deadline = None;
                    return Parked::UpgradeTimeout;
                }
                Some(d) => d - now,
                None => Duration::from_secs(3600),
            };
            st = self.sh.cv.wait_timeout(st, wait).unwrap().0;
        }
    }

    fn attempt(&mut self) -> Attempt {
        let mc = self.sh.mc.clone();
        let version = mc.desktop_version.clone();
        if self.managed && !self.install_verified {
            self.set_phase(Some(Phase::Probing));
            let on_upload = || self.set_phase(Some(Phase::Installing));
            match ensure_installed(
                &*self.transport,
                &*mc.binaries,
                &version,
                mc.ours,
                &self.cancel,
                &on_upload,
            ) {
                Err(e) if e.cancelled => return Attempt::Cancelled,
                Err(e) => {
                    self.set_phase(None);
                    return Attempt::Failed(Failure {
                        message: e.message,
                        hint: e.hint,
                        incompatible: None,
                        reinstall: false,
                    });
                }
                Ok(i) => {
                    self.install_verified = true;
                    let mut st = self.sh.lock();
                    st.status.os = os_name(&i.os).map(String::from);
                    st.status.arch = Some(i.arch);
                    self.sh.publish(&mut st);
                }
            }
        }
        self.set_phase(None);
        let cmd = self
            .transport
            .command(&connect_command(self.override_.as_deref(), &version));
        let mut proc = match process::spawn(&cmd, &self.cancel) {
            Ok(p) => p,
            Err(_) if self.cancel.is_cancelled() => return Attempt::Cancelled,
            Err(e) => {
                return Attempt::Failed(Failure {
                    message: format!("cannot run {}: {e}", self.transport.describe()),
                    hint: classify_ssh_failure("", Some(&e), None),
                    incompatible: None,
                    reinstall: false,
                })
            }
        };
        let gen = self.sh.begin_link(Some(proc.pid()));
        self.last_gen = gen;
        let io = LinkIo {
            read: Box::new(proc.stdout.take().expect("piped stdout")),
            write: Box::new(proc.stdin.take().expect("piped stdin")),
        };
        let established =
            Link::establish(io, mc.ours, &version, self.sh.events(gen), mc.hello_timeout);
        let (link, hello, noise) = match established {
            Ok(x) => x,
            Err(_) if self.cancel.is_cancelled() => return Attempt::Cancelled,
            Err(e) => return Attempt::Failed(self.link_failure(&proc, e)),
        };
        if let Some(n) = noise {
            eprintln!(
                "xshell: host {}: ignored output before the handshake: {}",
                self.sh.id,
                n.trim()
            );
        }
        let (negotiated, upgrade_pending) = match classify(&version, mc.ours, &hello, self.managed)
        {
            Classified::Compatible {
                negotiated,
                upgrade_pending,
            } => (negotiated, upgrade_pending),
            Classified::Incompatible { reason, message } => {
                link.close();
                return Attempt::Failed(Failure {
                    message,
                    hint: None,
                    incompatible: Some((reason_json(reason), hello.version)),
                    reinstall: false,
                });
            }
        };
        if !self.sh.wait_first_list(gen, FIRST_LIST_TIMEOUT) {
            link.close();
            if self.cancel.is_cancelled() {
                return Attempt::Cancelled;
            }
            return Attempt::Failed(Failure {
                message: "xshelld sent no terminals list".into(),
                hint: None,
                incompatible: None,
                reinstall: false,
            });
        }
        let managed = self.managed;
        let adopted = self.sh.adopt(gen, &link, |st| {
            let mismatch = managed && st.upgrade_expect && hello.version != version;
            st.status.set_kind(if upgrade_pending || mismatch {
                StatusKind::UpgradePending
            } else {
                StatusKind::Connected
            });
            st.status.phase = None;
            st.status.daemon_version = Some(hello.version.clone());
            st.status.protocol = Some(negotiated);
            st.status.last_error = mismatch.then(|| {
                format!(
                    "xshelld {} is still running after the upgrade to {version}",
                    hello.version
                )
            });
            st.status.error_hint = None;
            st.status.incompatible_reason = None;
            st.status.next_retry_at = None;
            st.upgrading = false;
            st.upgrade_expect = false;
            st.upgrade_deadline = None;
        });
        if !adopted {
            link.close();
            if self.cancel.is_cancelled() {
                return Attempt::Cancelled;
            }
            let why = self
                .sh
                .lock()
                .close_reason
                .clone()
                .unwrap_or_else(|| "connection lost".into());
            return Attempt::Failed(Failure {
                message: why,
                hint: None,
                incompatible: None,
                reinstall: false,
            });
        }
        Attempt::Connected(Connected {
            link,
            proc,
            gen,
            at: Instant::now(),
        })
    }

    fn link_failure(&self, proc: &Proc, e: LinkError) -> Failure {
        match e {
            LinkError::Incompatible { hello, message } => {
                let reason = match version::classify(
                    &self.sh.mc.desktop_version,
                    self.sh.mc.ours,
                    &hello,
                    self.managed,
                ) {
                    Classified::Incompatible { reason, .. } => reason,
                    Classified::Compatible { .. } => version::IncompatibleReason::Older,
                };
                proc.child.kill_and_wait(Duration::from_secs(2));
                Failure {
                    message,
                    hint: None,
                    incompatible: Some((reason_json(reason), hello.version)),
                    reinstall: false,
                }
            }
            LinkError::Failed(m) => {
                // Let ssh finish so its stderr and exit code are complete.
                let status = proc.child.wait_timeout(Duration::from_secs(1));
                proc.child.kill_and_wait(Duration::from_secs(2));
                proc.wait_stderr(Duration::from_millis(500));
                let stderr = proc.stderr_text();
                let code = status.and_then(|s| s.code());
                let reinstall = code == Some(127) || stderr.contains("No such file");
                Failure {
                    hint: classify_ssh_failure(&stderr, None, code),
                    message: if stderr.is_empty() { m } else { stderr },
                    incompatible: None,
                    reinstall,
                }
            }
        }
    }
}

fn reason_json(r: version::IncompatibleReason) -> IncompatibleReason {
    match r {
        version::IncompatibleReason::Older => IncompatibleReason::DaemonOlder,
        version::IncompatibleReason::Newer => IncompatibleReason::DaemonNewer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_host;
    use crate::manager::tests::{script_factory, test_config, Recorder};
    use crate::manager::Manager;
    use crate::status::HostStatus;

    #[test]
    fn backoff_schedule() {
        let s: Vec<u64> = (0..8)
            .map(|n| backoff(n, Duration::from_secs(1)).as_secs())
            .collect();
        assert_eq!(s, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(
            backoff(u32::MAX, Duration::from_millis(10)),
            Duration::from_millis(600)
        );
    }

    #[test]
    fn reset_only_after_stable() {
        let unit = Duration::from_secs(1);
        let stable = Duration::from_secs(10);
        let mut b = Backoff::default();
        assert_eq!(b.failed(unit), Duration::from_secs(1));
        assert_eq!(b.failed(unit), Duration::from_secs(2));
        assert_eq!(b.failed(unit), Duration::from_secs(4));
        // Connected, dropped after 3 s: the count stays.
        assert_eq!(
            b.ended(Duration::from_secs(3), stable, unit),
            Duration::from_secs(8)
        );
        assert_eq!(b.failures, 3);
        assert_eq!(b.failed(unit), Duration::from_secs(8));
        // Up for 10 s: reset, retry at once.
        assert_eq!(b.ended(stable, stable, unit), Duration::ZERO);
        assert_eq!(b.failures, 0);
        assert_eq!(b.failed(unit), Duration::from_secs(1));
    }

    fn manager_with(script: &str, override_: bool, rec: Arc<Recorder>) -> (Manager, String) {
        let mut mc = test_config(rec);
        mc.transports = script_factory(script);
        mc.backoff_unit = Duration::from_millis(10);
        let m = Manager::new(mc);
        let mut h = test_host("h_ab12cd34", "x");
        if override_ {
            h.daemon_command = Some("xd".into());
        }
        m.configure(vec![h]).unwrap();
        (m, "h_ab12cd34".into())
    }

    #[test]
    fn offline_after_three_failures() {
        let rec = Recorder::new();
        let (m, id) = manager_with("exit 1", true, rec.clone());
        let got = rec.wait_status(|s| s.status == StatusKind::Offline && s.next_retry_at.is_some());
        assert_eq!(got.host, id);
        let kinds: Vec<StatusKind> = rec.statuses().iter().map(|s| s.status).collect();
        let first_offline = kinds
            .iter()
            .position(|k| *k == StatusKind::Offline)
            .unwrap();
        assert!(kinds[..first_offline]
            .iter()
            .all(|k| *k == StatusKind::Reconnecting));
        // Three failed attempts published their retry while still reconnecting.
        let retries = rec
            .statuses()
            .iter()
            .filter(|s| s.status == StatusKind::Reconnecting && s.next_retry_at.is_some())
            .count();
        assert_eq!(retries, 2, "{kinds:?}");
        m.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn stderr_tail_is_last_error() {
        let rec = Recorder::new();
        let (m, _) = manager_with(
            "echo 'Permission denied (publickey).' >&2; exit 255",
            true,
            rec.clone(),
        );
        let s = rec.wait_status(|s| s.last_error.is_some());
        assert!(
            s.last_error
                .as_deref()
                .unwrap()
                .contains("Permission denied (publickey)."),
            "{s:?}"
        );
        assert_eq!(s.error_hint, Some(HostErrorHint::PermissionDenied));
        m.shutdown();
    }

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_child() {
        let rec = Recorder::new();
        let (m, id) = manager_with("exec sleep 1000", true, rec.clone());
        let h = m.host(&id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(p) = h.child_pid() {
                break p;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(pid_alive(pid));
        let start = Instant::now();
        m.shutdown();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!pid_alive(pid), "child {pid} survived");
    }

    #[test]
    fn kick_retries_now() {
        let rec = Recorder::new();
        let mut mc = test_config(rec.clone());
        mc.transports = script_factory("exit 1");
        mc.backoff_unit = Duration::from_secs(60);
        let m = Manager::new(mc);
        let mut h = test_host("h_ab12cd34", "x");
        h.daemon_command = Some("xd".into());
        m.configure(vec![h]).unwrap();
        let failed = |s: &HostStatus| s.next_retry_at.is_some();
        rec.wait_status(failed);
        let n = rec.statuses().iter().filter(|s| failed(s)).count();
        let start = Instant::now();
        m.host("h_ab12cd34").unwrap().kick();
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if rec.statuses().iter().filter(|s| failed(s)).count() > n {
                break;
            }
            assert!(Instant::now() < deadline, "no retry after the kick");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(start.elapsed() < Duration::from_millis(500));
        m.shutdown();
    }
}

//! One thread per Host: install, connect, handshake, re-attach, publish, park until the link
//! drops, back off, repeat. Every phase watches the run's cancel token.

use crate::cancel::CancelToken;
use crate::dial::{CommandDialer, Connection, DialError, Dialer};
use crate::errors::HostError;
use crate::errors::HostErrorHint;
use crate::handle::Shared;
use crate::install::{build_text, ensure_installed, kill_daemon_script, os_name, selected_remote};
use crate::link::{Link, LinkError};
use crate::process::run_script;
use crate::status::{
    now_ms, IncompatibleReason, Phase, RouteUpdateUnavailable, StatusKind, UpdateUnavailable,
};
use crate::transport::{connect_command, sh_wrap, Transport};
use crate::version::{self, classify, Classified};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_protocol::build::BuildId;

const FIRST_LIST_TIMEOUT: Duration = Duration::from_secs(5);
const OFFLINE_AFTER: u32 = 3;
/// How often waits re-check the cancel token and the kick flag.
const POLL: Duration = Duration::from_millis(50);
const REATTACH_RETRIES: u32 = 30;

pub use xshell_protocol::backoff::{backoff, Backoff};

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
    /// The reason, the Daemon's version and its build.
    incompatible: Option<(IncompatibleReason, String, Option<BuildId>)>,
    /// The installed binary looks missing: probe again next time.
    reinstall: bool,
}

struct Connected {
    /// Attachments not re-attached yet (the send queue was full).
    left: usize,
    link: Arc<Link>,
    conn: Box<dyn Connection>,
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
    /// Install and the upgrade kill script; also what `dialer` runs `connect` through,
    /// unless the Host is reached directly.
    transport: Option<Arc<dyn Transport>>,
    dialer: Box<dyn Dialer>,
    /// Installed over the transport (no Daemon command, not direct).
    managed: bool,
    /// Reached over the transport (not direct): managed, or through a Daemon command. Both
    /// are this Desktop's to upgrade or replace on request (#55); only a managed one is
    /// installed automatically.
    scripted: bool,
    /// The Daemon command, if any. Such a Host connects through the Host's `selected` build
    /// when there is one (#55: Upgrade now and Replace select this Desktop's build), else
    /// through exactly `<command> connect`.
    override_: Option<String>,
    backoff: Backoff,
    install_verified: bool,
    last_gen: u64,
    /// A Ring Host reached through the Relay: updated over its link when its Daemon offers
    /// `daemon.stage` (S4).
    relay: bool,
}

pub(crate) fn run(sh: Arc<Shared>, cancel: CancelToken) {
    let cfg = sh.lock().cfg.clone();
    let relay = matches!(cfg.route, crate::config::HostRoute::Relay(_));
    let (transport, dialer, managed, scripted, override_) = match &cfg.route {
        crate::config::HostRoute::Relay(d) => (
            None,
            Box::new(d.clone()) as Box<dyn Dialer>,
            false,
            false,
            None,
        ),
        crate::config::HostRoute::Configured(cfg) => {
            let direct = sh.mc.transports.direct(cfg);
            let override_ = cfg.daemon_override().map(String::from);
            match direct {
                Some(d) => (None, d, false, false, override_),
                None => {
                    let transport: Arc<dyn Transport> = sh.mc.transports.for_host(cfg).into();
                    let dialer = Box::new(CommandDialer {
                        transport: transport.clone(),
                        remote_cmd: connect_command(override_.as_deref(), &sh.mc.install_dir()),
                    });
                    let managed = override_.is_none();
                    (
                        Some(transport),
                        dialer as Box<dyn Dialer>,
                        managed,
                        true,
                        override_,
                    )
                }
            }
        }
    };
    let mut s = Sup {
        sh,
        cancel,
        transport,
        dialer,
        managed,
        scripted,
        override_,
        backoff: Backoff::default(),
        install_verified: false,
        last_gen: 0,
        relay,
    };
    s.run();
}

impl Sup {
    fn unit(&self) -> Duration {
        self.sh.mc.backoff_unit
    }

    /// Whether this Desktop upgrades the Daemon: one reached over the transport (managed or
    /// through a Daemon command), or one its direct dialer says it may (the Local Host's
    /// Persistent Daemon). Read live: that can change.
    fn upgradable(&self) -> bool {
        self.scripted || self.dialer.upgradable()
    }

    fn set_phase(&self, p: Option<Phase>) {
        let mut st = self.sh.lock();
        st.status.phase = if st.upgrading {
            Some(p.unwrap_or(if st.upgrade_expect {
                Phase::Upgrading
            } else {
                Phase::Preparing
            }))
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
                crate::handle::expire_upgrade(&self.sh, &mut st);
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
                self.sh.kick.store(false, Ordering::SeqCst);
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
                        if self.scripted
                            && matches!(
                                self.sh.lock().status.os.as_deref(),
                                Some("linux" | "macos")
                            )
                        {
                            let _ = kill_daemon(
                                self.transport.as_deref().expect("scripted route"),
                                &self.cancel,
                            );
                        } else if self.dialer.upgradable() && cfg!(unix) {
                            let _ = self.dialer.stop_incompatible(&self.cancel);
                        }
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
                let l = st.link().cloned();
                self.sh.set_link(&mut st, None);
                l
            } else {
                None
            }
        };
        if let Some(l) = link {
            l.close();
        }
    }

    /// Wait until stopped, kicked, or `d` passes. Polls the token and the kick flag:
    /// stopping and kicking never need the `State` lock.
    fn wait(&self, d: Duration) -> Waited {
        let deadline = Instant::now() + d;
        let mut st = self.sh.lock();
        loop {
            crate::handle::expire_upgrade(&self.sh, &mut st);
            if self.cancel.is_cancelled() {
                return Waited::Stop;
            }
            if self.sh.kick.swap(false, Ordering::SeqCst) {
                return Waited::Kick;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Waited::Elapsed;
            }
            st = self.sh.cv.wait_timeout(st, left.min(POLL)).unwrap().0;
        }
    }

    fn failed(&mut self, f: Failure) -> Duration {
        if f.reinstall {
            self.install_verified = false;
        }
        let mut delay = self.backoff.failed(self.unit());
        let mut st = self.sh.lock();
        match f.incompatible {
            Some((reason, version, build)) => {
                delay = backoff(u32::MAX, self.unit());
                st.status.set_kind(StatusKind::Incompatible);
                st.status.incompatible_reason = Some(reason);
                st.status.daemon_version = Some(version);
                st.status.daemon_build = build;
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
        st.status.phase = st.upgrading.then_some(if st.upgrade_expect {
            Phase::Upgrading
        } else {
            Phase::Preparing
        });
        if st.status.error_hint != Some(HostErrorHint::UpdateFailed) {
            st.status.last_error = Some(f.message);
            st.status.error_hint = f.hint;
        }
        st.status.protocol = None;
        st.status.daemon_capabilities.clear();
        st.status.next_retry_at = Some(now_ms() + delay.as_millis() as u64);
        self.sh.publish(&mut st);
        delay
    }

    fn ended(&mut self, c: Connected, why: String) -> Duration {
        let up_for = c.at.elapsed();
        drop(c.link);
        // ssh has usually exited with the stream; make sure, and reap it.
        c.conn.end();
        let mut st = self.sh.lock();
        let delay = if st.upgrading {
            Duration::ZERO
        } else {
            self.backoff
                .ended(up_for, self.sh.mc.stable_after, self.unit())
        };
        self.sh.set_link(&mut st, None);
        st.upgrade_deadline = None;
        st.status
            .set_kind(if self.backoff.failures < OFFLINE_AFTER {
                StatusKind::Reconnecting
            } else {
                StatusKind::Offline
            });
        if st.status.error_hint != Some(HostErrorHint::UpdateFailed) {
            st.status.last_error = Some(why);
            st.status.error_hint = None;
        }
        st.status.next_retry_at = (!delay.is_zero()).then(|| now_ms() + delay.as_millis() as u64);
        self.sh.publish(&mut st);
        delay
    }

    fn park(&self, c: &Connected) -> Parked {
        // Attachments the adoption could not send yet (queue full): bounded retries.
        let mut left = c.left;
        let mut retry_delay = Duration::from_millis(10);
        let mut retries = 0u32;
        let mut retry_at = Instant::now() + retry_delay;
        let mut st = self.sh.lock();
        loop {
            crate::handle::expire_upgrade(&self.sh, &mut st);
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
            // Fresh identity proof may make previously gated attachments eligible.
            // Preserve the same bounded backpressure retries as ordinary adoption.
            if self.sh.kick.swap(false, Ordering::SeqCst) {
                left = self.sh.reattach_locked(&mut st, c.gen, &c.link);
                retries = 0;
                retry_delay = Duration::from_millis(10);
                retry_at = Instant::now() + retry_delay;
            }
            let now = Instant::now();
            if left > 0 && retry_at <= now {
                left = self.sh.reattach_locked(&mut st, c.gen, &c.link);
                retries += 1;
                if left > 0 && retries >= REATTACH_RETRIES {
                    drop(st);
                    c.link.close();
                    return Parked::Closed(format!(
                        "could not re-attach {left} terminals; reconnecting"
                    ));
                }
                retry_delay = (retry_delay * 2).min(Duration::from_secs(1));
                retry_at = now + retry_delay;
            }
            let mut wait = POLL;
            match st.upgrade_deadline {
                Some(d) if d <= now => {
                    st.upgrade_deadline = None;
                    return Parked::UpgradeTimeout;
                }
                Some(d) => wait = wait.min(d - now),
                None => {}
            }
            if left > 0 {
                wait = wait.min(retry_at.saturating_duration_since(now));
            }
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
                self.transport.as_deref().expect("scripted route"),
                &*mc.binaries,
                &version,
                mc.desktop_build.as_ref(),
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
        if let (Some(cmd), true) = (&self.override_, self.scripted) {
            // Each attempt: the selected build may have changed since (an upgrade, another
            // Desktop). A host that refuses the script keeps the plain command.
            // Platform probe is the exact configured command, before any POSIX script.
            let probe = crate::install::probe_override(
                self.transport.as_deref().expect("scripted route"),
                cmd,
                &self.cancel,
            )
            .ok();
            let platform = probe
                .as_ref()
                .and_then(|p| p.os.as_deref())
                .and_then(os_name)
                .map(String::from)
                .or_else(|| self.sh.lock().status.os.clone());
            if let Some(probe) = &probe {
                let mut st = self.sh.lock();
                st.status.os = probe.os.as_deref().and_then(os_name).map(String::from);
                st.status.arch = probe.arch.clone();
                self.sh.publish(&mut st);
            }
            let dir = if matches!(platform.as_deref(), Some("linux" | "macos")) {
                selected_remote(
                    self.transport.as_deref().expect("scripted route"),
                    cmd,
                    &self.cancel,
                )
            } else {
                None
            };
            if self.cancel.is_cancelled() {
                return Attempt::Cancelled;
            }
            self.dialer = Box::new(CommandDialer {
                transport: self.transport.clone().expect("scripted route"),
                remote_cmd: match &dir {
                    Some(d) => connect_command(None, d),
                    None => connect_command(Some(cmd), ""),
                },
            });
        }
        self.set_phase(None);
        let dialed = match self.dialer.dial(&self.cancel) {
            Ok(d) => d,
            Err(DialError::Cancelled) => return Attempt::Cancelled,
            Err(DialError::Failed { message, hint, .. }) => {
                return Attempt::Failed(Failure {
                    message,
                    hint,
                    incompatible: None,
                    reinstall: false,
                })
            }
        };
        let conn = dialed.conn;
        let gen = self.sh.begin_link(conn.pid());
        self.last_gen = gen;
        let established = Link::establish_with(
            dialed.io,
            mc.ours,
            &version,
            mc.desktop_build.clone(),
            self.sh.events(gen),
            mc.hello_timeout,
            mc.link_limits,
        );
        let (link, hello, noise) = match established {
            Ok(x) => x,
            Err(_) if self.cancel.is_cancelled() => return Attempt::Cancelled,
            Err(e) => return Attempt::Failed(self.link_failure(&*conn, e)),
        };
        if let Some(n) = noise {
            eprintln!(
                "xshell: host {}: ignored output before the handshake: {}",
                self.sh.id,
                n.trim()
            );
        }
        if self.cancel.is_cancelled() || !self.dialer.authorized() {
            link.close();
            return Attempt::Cancelled;
        }
        let ring_stage = self.relay
            && hello
                .capabilities
                .iter()
                .any(|c| c == xshell_protocol::cap::DAEMON_STAGE);
        let upgradable = self.upgradable() || ring_stage;
        let ours_build = mc.desktop_build.as_ref();
        // A Ring Host that differs from this Desktop and cannot take an update over the Ring
        // shows how to update it there instead (S4).
        let ring_unsupported = self.relay
            && !ring_stage
            && version::update_eligible(&version, ours_build, &hello.version, hello.build.as_ref());
        // A Ring Host that updates itself from the latest release (ADR-0019 amendment, D4):
        // outdated only against that release, never "build differs".
        let release_capable = self.relay
            && hello
                .capabilities
                .iter()
                .any(|c| c == xshell_protocol::cap::DAEMON_UPDATE_LATEST);
        let (negotiated, upgrade_pending, build_differs) =
            match classify(&version, ours_build, mc.ours, &hello, upgradable) {
                Classified::Compatible {
                    negotiated,
                    upgrade_pending,
                    build_differs,
                } => (negotiated, upgrade_pending, build_differs),
                Classified::Incompatible { reason, message } => {
                    let mut st = self.sh.lock();
                    st.release_updates = false;
                    st.status.update_unavailable =
                        UpdateUnavailable::from_hello(st.status.os.as_deref(), &hello, self.relay);
                    if self.relay {
                        // No common protocol: not over the Ring either.
                        st.status.route_update_unavailable =
                            Some(RouteUpdateUnavailable::RingUpdateUnsupported);
                    }
                    drop(st);
                    link.close();
                    return Attempt::Failed(Failure {
                        message,
                        hint: None,
                        incompatible: Some((reason_json(reason), hello.version, hello.build)),
                        reinstall: false,
                    });
                }
            };
        let (upgrade_pending, build_differs) = if release_capable {
            (
                version::release_outdated(mc.latest().as_ref(), &hello.version),
                false,
            )
        } else {
            (upgrade_pending, build_differs)
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
        let relay = self.relay;
        // A Ring Host is published with its OS already known (or known to be unknown), so an
        // OS-dependent launch right after it connects never falls back to Linux.
        let platform = if relay {
            self.platform_now(&link, PLATFORM_WAIT)
        } else {
            None
        };
        let adopted = self.sh.adopt(gen, &link, |st| {
            if let Some((os, arch)) = &platform {
                st.status.os = Some((*os).into());
                st.status.arch = Some(arch.clone());
            }
            // After an upgrade or a replacement (R7, C4): the Daemon must be this Desktop's
            // version and build. Another Desktop's connector may have won the restart.
            // Authorization/classification belongs to the current mode; completion belongs
            // to the already accepted operation, including a GUI-bound successor.
            let expect = st.upgrade_expect;
            st.release_updates = release_capable;
            // The identity the update must come back with: the release the Host installed
            // (D5), or this Desktop's build.
            let (want_v, want_b) = st
                .upgrade_identity
                .clone()
                .map(|(v, b)| (v, Some(b)))
                .unwrap_or_else(|| (version.clone(), ours_build.cloned()));
            let ver_mismatch = expect && hello.version != want_v;
            let build_mismatch = expect
                && want_b
                    .as_ref()
                    .is_some_and(|b| !version::same_build(hello.build.as_ref(), b));
            st.status.set_kind(if upgrade_pending || ver_mismatch {
                StatusKind::UpgradePending
            } else if build_differs || build_mismatch {
                StatusKind::BuildDiffers
            } else {
                StatusKind::Connected
            });
            st.status.phase = if st.upgrading && !expect { Some(Phase::Preparing) } else { None };
            st.status.update_unavailable = UpdateUnavailable::from_hello(st.status.os.as_deref(), &hello, relay);
            if relay {
                st.status.route_update_unavailable =
                    ring_unsupported.then_some(RouteUpdateUnavailable::RingUpdateUnsupported);
            }
            st.status.daemon_version = Some(hello.version.clone());
            st.status.daemon_build = hello.build.clone();
            st.status.protocol = Some(negotiated);
            st.status.daemon_capabilities = hello.capabilities.clone();
            let previous_update_error = (st.status.error_hint == Some(HostErrorHint::UpdateFailed) && (upgrade_pending || build_differs))
                .then(|| st.status.last_error.clone()).flatten();
            // A Ring Host back on an older version than its update: the update failed and
            // it runs that one (a Windows Desktop started its own build again, rjw.9 D4; a
            // Linux trial's rollback is reported in the same words by `check_rolled_back`).
            let back_on_older = relay
                && ver_mismatch
                && matches!(
                    (semver::Version::parse(&hello.version), semver::Version::parse(&want_v)),
                    (Ok(have), Ok(want)) if have.cmp_precedence(&want).is_lt()
                );
            st.status.last_error = if back_on_older {
                Some(crate::handle::ring_rolled_back(&st.cfg.name, &hello.version))
            } else if ver_mismatch {
                Some(format!(
                    "xshelld {} is still running after the upgrade to {want_v}",
                    hello.version
                ))
            } else if build_mismatch {
                Some(format!(
                    "xshelld {} ({}) is still running after replacement; expected {want_v} ({}). Try Replace again. If this repeats, quit other xshell apps using this host.",
                    hello.version,
                    build_text(hello.build.as_ref()),
                    build_text(want_b.as_ref())
                ))
            } else { previous_update_error };
            st.status.error_hint = st.status.last_error.as_ref().map(|_| HostErrorHint::UpdateFailed);
            st.status.incompatible_reason = None;
            st.status.next_retry_at = None;
            if expect {
                st.upgrading = false;
                st.upgrade_expect = false;
                st.upgrade_identity = None;
                st.upgrade_operation = None;
                st.upgrade_complete_deadline = None;
                st.upgrade_deadline = None;
            }
        });
        let Some(left) = adopted else {
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
        };
        if ring_stage {
            self.check_rolled_back(&link, gen, &hello);
        }
        Attempt::Connected(Connected {
            link,
            conn,
            gen,
            at: Instant::now(),
            left,
        })
    }

    /// A Ring Host is never probed over SSH, so its OS is unknown until its Daemon says which
    /// target it is (`daemon.update-info`, answered by every Ring-era Daemon whether or not
    /// it stages updates). Asked once on link-up, before the Host is published as usable,
    /// waiting at most `wait`. `None`: unknown (an older Daemon answers the request with an
    /// error, or it did not answer in time); the Desktop's fallback then applies.
    fn platform_now(&self, link: &Arc<Link>, wait: Duration) -> Option<(&'static str, String)> {
        let (tx, rx) = std::sync::mpsc::channel();
        link.request(
            xshell_protocol::msg::ClientMsg::DaemonUpdateInfo,
            wait,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        )
        .ok()?;
        let info = rx.recv_timeout(wait).ok()?.ok()?;
        let info = serde_json::from_value::<xshell_protocol::msg::UpdateInfo>(info).ok()?;
        platform_of_target(&info.target)
    }

    /// After this Desktop updated a Ring Host and another build came back: ask the Daemon
    /// whether that update was rolled back, and say so (S4, copy `update.rolledBack`).
    fn check_rolled_back(&self, link: &Arc<Link>, gen: u64, hello: &xshell_protocol::msg::Hello) {
        const ASK_FOR: Duration = Duration::from_secs(15 * 60);
        let theirs = xshell_protocol::build::install_dir_name(&hello.version, hello.build.as_ref());
        let dir = {
            let mut st = self.sh.lock();
            match st.ring_update.clone() {
                Some((_, at)) if at.elapsed() > ASK_FOR => {
                    st.ring_update = None;
                    return;
                }
                Some((dir, _)) if dir != theirs => dir,
                _ => return,
            }
        };
        let weak = Arc::downgrade(&self.sh);
        let old = hello.version.clone();
        let _ = link.request(
            xshell_protocol::msg::ClientMsg::DaemonUpdateInfo,
            self.sh.mc.term_timeout,
            Box::new(move |r| {
                let Some(sh) = weak.upgrade() else { return };
                let Ok(info) = r.and_then(|v| {
                    serde_json::from_value::<xshell_protocol::msg::UpdateInfo>(v)
                        .map_err(|e| crate::errors::HostError::offline(e.to_string()))
                }) else {
                    return;
                };
                if !info.last_failure.is_some_and(|f| f.target == dir) {
                    return;
                }
                let mut st = sh.lock();
                if st.gen != gen || st.upgrading {
                    return;
                }
                st.ring_update = None;
                st.status.last_error = Some(crate::handle::ring_rolled_back(&st.cfg.name, &old));
                st.status.error_hint = Some(HostErrorHint::UpdateFailed);
                sh.publish(&mut st);
            }),
        );
    }

    fn link_failure(&self, conn: &dyn Connection, e: LinkError) -> Failure {
        match e {
            LinkError::Incompatible { hello, message } => {
                {
                    let mut st = self.sh.lock();
                    st.status.update_unavailable =
                        UpdateUnavailable::from_hello(st.status.os.as_deref(), &hello, self.relay);
                    if self.relay {
                        st.status.route_update_unavailable =
                            Some(RouteUpdateUnavailable::RingUpdateUnsupported);
                    }
                }
                let reason = match version::classify(
                    &self.sh.mc.desktop_version,
                    self.sh.mc.desktop_build.as_ref(),
                    self.sh.mc.ours,
                    &hello,
                    self.upgradable(),
                ) {
                    Classified::Incompatible { reason, .. } => reason,
                    Classified::Compatible { .. } => version::IncompatibleReason::Older,
                };
                conn.end();
                Failure {
                    message,
                    hint: None,
                    incompatible: Some((reason_json(reason), hello.version, hello.build)),
                    reinstall: false,
                }
            }
            LinkError::Failed(m) => {
                let d = conn.diagnose(m);
                Failure {
                    message: d.message,
                    hint: d.hint,
                    incompatible: None,
                    reinstall: d.reinstall,
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

/// How long link-up waits for a Ring Host's platform before publishing it as unknown.
const PLATFORM_WAIT: Duration = Duration::from_secs(3);

/// The OS (as `HostStatus::os` names it) and arch of a Daemon target triple
/// (`xshell_protocol::build::host_triple`).
pub(crate) fn platform_of_target(target: &str) -> Option<(&'static str, String)> {
    let arch = target
        .split('-')
        .next()
        .filter(|a| !a.is_empty())?
        .to_string();
    let os = if target.contains("-windows") {
        "windows"
    } else if target.contains("-linux") {
        "linux"
    } else if target.contains("-apple-darwin") {
        "macos"
    } else {
        return None;
    };
    Some((os, arch))
}

#[cfg(test)]
mod tests {
    #[test]
    fn platform_of_every_daemon_target() {
        use super::platform_of_target as p;
        for (os, arch) in [
            ("windows", "x86_64"),
            ("Linux", "x86_64"),
            ("Linux", "aarch64"),
            ("Darwin", "arm64"),
            ("Darwin", "x86_64"),
        ] {
            let triple = xshell_protocol::build::host_triple(os, arch).unwrap();
            let (got, got_arch) = p(triple).unwrap();
            assert_eq!(Some(got), crate::install::os_name(os), "{triple}");
            assert_eq!(got_arch, triple.split('-').next().unwrap());
        }
        assert_eq!(p("x86_64-pc-windows-msvc").unwrap().0, "windows");
        assert!(p("riscv64-unknown-freebsd").is_none());
        assert!(p("").is_none());
    }

    use super::*;
    use crate::config::test_host;
    use crate::manager::tests::{script_factory, test_config, Recorder};
    use crate::manager::Manager;
    use crate::status::HostStatus;

    #[test]
    fn accepted_non_rpc_update_has_a_completion_deadline_even_while_disconnected() {
        let mut mc = test_config(Recorder::new());
        mc.transports = script_factory("exit 1");
        let sh = Arc::new(Shared::new(test_host("h_ab12cd34", "x"), Arc::new(mc), 1));
        let transport: Arc<dyn Transport> =
            Arc::new(crate::transport::LocalShellTransport::default());
        let supervisor = Sup {
            sh: sh.clone(),
            cancel: CancelToken::new(),
            transport: Some(transport.clone()),
            dialer: Box::new(CommandDialer {
                transport,
                remote_cmd: "unused".into(),
            }),
            managed: false,
            scripted: true,
            override_: None,
            backoff: Backoff::default(),
            install_verified: true,
            last_gen: 0,
            relay: false,
        };
        {
            let mut st = sh.lock();
            st.upgrading = true;
            st.upgrade_operation = Some(7);
        }
        crate::handle::accept_upgrade(&sh, 7, 0, false);
        {
            let mut st = sh.lock();
            assert!(st.upgrade_deadline.is_none()); // No RPC-close deadline for forced stops.
            assert!(st.upgrade_complete_deadline.is_some());
            st.upgrade_complete_deadline = Some(Instant::now());
            crate::handle::expire_upgrade(&sh, &mut st);
            assert!(!st.upgrading && !st.upgrade_expect);
            assert_eq!(st.status.phase, None);
            assert!(st
                .status
                .last_error
                .as_deref()
                .unwrap()
                .contains("did not reconnect"));
        }
        // Expiration does not cancel ordinary reconnect retries.
        assert!(!supervisor.cancel.is_cancelled());
    }

    /// The schedule lives in `xshell_protocol::backoff`; supervision uses it as is.
    #[test]
    fn backoff_schedule() {
        let s: Vec<u64> = (0..8)
            .map(|n| backoff(n, Duration::from_secs(1)).as_secs())
            .collect();
        assert_eq!(s, [1, 2, 4, 8, 16, 32, 60, 60]);
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

//! The Desktop keeps this machine's Daemon running (ADR-0017): at start, on Linux and macOS,
//! it runs `<installed xshelld> service install --json` once in the background and keeps the
//! report for `local_host_info`. The Local Host's first start waits for that report briefly,
//! so on a first launch the service usually starts the Daemon before the Desktop would.
//!
//! Development builds skip it unless `XSHELL_SERVICE=1`, so `tauri dev` never installs a unit
//! for a dev build. The child inherits the environment, so `XSHELL_SERVICE_MECHANISM` forces
//! a mechanism in tests and end-to-end runs (A1).

use super::local::ServiceInfo;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// `service install` gets this long.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The Local Host's first start waits this long for the report.
pub const FIRST_WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct ServiceState {
    report: Mutex<Option<ServiceInfo>>,
    done: Condvar,
}

impl ServiceState {
    pub fn set(&self, info: ServiceInfo) {
        *self.report.lock().unwrap() = Some(info);
        self.done.notify_all();
    }

    /// The last report; `None` while the install still runs.
    pub fn get(&self) -> Option<ServiceInfo> {
        self.report.lock().unwrap().clone()
    }

    /// Wait until a report is there, at most `within`.
    pub fn wait_first(&self, within: Duration) -> Option<ServiceInfo> {
        let deadline = Instant::now() + within;
        let mut g = self.report.lock().unwrap();
        loop {
            if let Some(r) = g.as_ref() {
                return Some(r.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            g = self.done.wait_timeout(g, left).unwrap().0;
        }
    }

    /// Forget the report (a retry runs the install again).
    pub fn reset(&self) {
        *self.report.lock().unwrap() = None;
    }
}

/// Whether this build installs the service: release builds always, debug builds only with
/// `XSHELL_SERVICE=1`.
pub fn should_install(debug: bool, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    !debug || env("XSHELL_SERVICE").as_deref() == Some("1".as_ref())
}

/// The report `service install --json` printed (its last JSON line).
pub fn parse_report(stdout: &str) -> Result<ServiceInfo, String> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or("xshelld service install printed no report")?;
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("unreadable service report: {e}"))?;
    let mechanism = v["mechanism"]
        .as_str()
        .ok_or("the service report names no mechanism")?
        .to_string();
    let warnings: Vec<String> = v["warnings"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(ServiceInfo {
        mechanism: Some(mechanism),
        linger: v["linger"].as_bool(),
        error: (!warnings.is_empty()).then(|| warnings.join("; ")),
    })
}

/// Run `bin service install --json`, ended after `timeout`.
pub fn run_install(bin: &Path, timeout: Duration) -> ServiceInfo {
    let failed = |e: String| ServiceInfo {
        mechanism: None,
        linger: None,
        error: Some(e),
    };
    let mut child = match Command::new(bin)
        .args(["service", "install", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return failed(format!("cannot run {}: {e}", bin.display())),
    };
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let o = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let e = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let Some(status) = status else {
        return failed(format!(
            "xshelld service install did not finish within {timeout:?}"
        ));
    };
    let stdout = o.join().unwrap_or_default();
    let stderr = e.join().unwrap_or_default();
    if !status.success() {
        return failed(format!(
            "xshelld service install failed ({status}): {}",
            stderr.trim()
        ));
    }
    parse_report(&stdout).unwrap_or_else(failed)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("xshelld");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn parses_report() {
        let r = parse_report(
            "noise\n{\"mechanism\":\"systemd\",\"installed\":[\"systemd\"],\"active\":true,\"linger\":false,\"running\":true,\"launcher\":\"/l\",\"changed\":true,\"warnings\":[\"cannot turn on linger\"]}\n",
        )
        .unwrap();
        assert_eq!(r.mechanism.as_deref(), Some("systemd"));
        assert_eq!(r.linger, Some(false));
        assert_eq!(r.error.as_deref(), Some("cannot turn on linger"));
        let r = parse_report(r#"{"mechanism":"profile","linger":null,"warnings":[]}"#).unwrap();
        assert_eq!(
            r,
            ServiceInfo {
                mechanism: Some("profile".into()),
                linger: None,
                error: None
            }
        );
        assert!(parse_report("").is_err());
        assert!(parse_report("{\"x\":1}").is_err());
    }

    #[test]
    fn install_timeout_is_reported() {
        let t = tempfile::tempdir().unwrap();
        let bin = script(t.path(), "exec sleep 30");
        let t0 = Instant::now();
        let r = run_install(&bin, Duration::from_millis(300));
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(r.mechanism, None);
        assert!(r.error.unwrap().contains("did not finish"));
        // A failing install reports its stderr; a good one its report and arguments.
        let bin = script(t.path(), "echo boom >&2; exit 3");
        assert!(run_install(&bin, INSTALL_TIMEOUT)
            .error
            .unwrap()
            .contains("boom"));
        let bin = script(
            t.path(),
            "[ \"$*\" = 'service install --json' ] || exit 9\necho '{\"mechanism\":\"none\",\"linger\":null,\"warnings\":[]}'",
        );
        assert_eq!(
            run_install(&bin, INSTALL_TIMEOUT).mechanism.as_deref(),
            Some("none")
        );
    }

    #[test]
    fn debug_skips_unless_opted_in() {
        let none = |_: &str| None;
        assert!(should_install(false, &none));
        assert!(!should_install(true, &none));
        let on = |k: &str| (k == "XSHELL_SERVICE").then(|| "1".into());
        assert!(should_install(true, &on));
        let other = |k: &str| (k == "XSHELL_SERVICE").then(|| "0".into());
        assert!(!should_install(true, &other));
    }

    #[test]
    fn wait_first_returns_once_set() {
        let s = std::sync::Arc::new(ServiceState::default());
        assert_eq!(s.wait_first(Duration::from_millis(50)), None);
        let s2 = s.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            s2.set(ServiceInfo::default());
        });
        assert_eq!(
            s.wait_first(Duration::from_secs(5)),
            Some(ServiceInfo::default())
        );
        s.reset();
        assert_eq!(s.get(), None);
    }
}

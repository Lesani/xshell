//! How the Local Host runs its Terminals: in a GUI-bound Daemon (ADR-0005) when the app
//! finds its `xshelld`, otherwise in this process as before.

use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalMode {
    /// New local Tabs are Terminals of a Daemon started from `bin`.
    Daemon { bin: PathBuf },
    /// Local Tabs run in this process; `reason` is logged and returned to the frontend.
    InProcess { reason: String },
}

/// What `local_host_info` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalHostInfo {
    /// `"daemon"` or `"in-process"`.
    pub mode: &'static str,
    pub reason: Option<String>,
}

impl LocalMode {
    pub fn info(&self) -> LocalHostInfo {
        match self {
            LocalMode::Daemon { .. } => LocalHostInfo {
                mode: "daemon",
                reason: None,
            },
            LocalMode::InProcess { reason } => LocalHostInfo {
                mode: "in-process",
                reason: Some(reason.clone()),
            },
        }
    }
}

/// The Daemon binary, in this order:
/// 1. `XSHELL_DAEMON_BIN` (development and tests);
/// 2. `xshelld` next to the app's executable: the bundled sidecar (Tauri strips the target
///    triple from sidecar names), and `target/debug/xshelld` under `tauri dev`.
///
/// `XSHELL_LOCAL_DAEMON=0` keeps local Tabs in this process. Windows always does (#24).
pub fn resolve_daemon_binary(
    exe_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> LocalMode {
    let in_process = |reason: String| LocalMode::InProcess { reason };
    if cfg!(not(unix)) {
        return in_process("local terminals run in the app on this platform".into());
    }
    if env("XSHELL_LOCAL_DAEMON").as_deref() == Some("0".as_ref()) {
        return in_process("XSHELL_LOCAL_DAEMON=0".into());
    }
    if let Some(p) = env("XSHELL_DAEMON_BIN").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() {
            LocalMode::Daemon { bin: p }
        } else {
            in_process(format!("XSHELL_DAEMON_BIN {} is not a file", p.display()))
        };
    }
    let Some(dir) = exe_dir else {
        return in_process("the app's directory is unknown".into());
    };
    let bin = dir.join("xshelld");
    if bin.is_file() {
        LocalMode::Daemon { bin }
    } else {
        in_process(format!("no xshelld in {}", dir.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolve(dir: Option<&Path>, env: &[(&str, &str)]) -> LocalMode {
        let env: HashMap<String, OsString> = env
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        resolve_daemon_binary(dir, &|k| env.get(k).cloned())
    }

    #[cfg(unix)]
    #[test]
    fn daemon_binary_resolution_order() {
        let t = tempfile::tempdir().unwrap();
        let exe_dir = t.path().join("app");
        std::fs::create_dir(&exe_dir).unwrap();
        let other = t.path().join("dev-xshelld");
        std::fs::write(&other, "").unwrap();
        let other_s = other.to_str().unwrap();

        // Nothing found: in-process, with the reason.
        let m = resolve(Some(&exe_dir), &[]);
        assert!(matches!(&m, LocalMode::InProcess { reason } if reason.contains("no xshelld")));
        assert_eq!(m.info().mode, "in-process");
        assert_eq!(
            resolve(None, &[]),
            LocalMode::InProcess {
                reason: "the app's directory is unknown".into()
            }
        );

        // The sidecar next to the executable.
        std::fs::write(exe_dir.join("xshelld"), "").unwrap();
        assert_eq!(
            resolve(Some(&exe_dir), &[]),
            LocalMode::Daemon {
                bin: exe_dir.join("xshelld")
            }
        );
        assert_eq!(resolve(Some(&exe_dir), &[]).info().mode, "daemon");
        assert_eq!(resolve(Some(&exe_dir), &[]).info().reason, None);

        // The environment goes first; a wrong path does not fall back silently.
        assert_eq!(
            resolve(Some(&exe_dir), &[("XSHELL_DAEMON_BIN", other_s)]),
            LocalMode::Daemon { bin: other.clone() }
        );
        assert!(matches!(
            resolve(
                Some(&exe_dir),
                &[("XSHELL_DAEMON_BIN", "/nonexistent/xshelld")]
            ),
            LocalMode::InProcess { .. }
        ));
        // The escape hatch wins over everything.
        assert!(matches!(
            resolve(
                Some(&exe_dir),
                &[("XSHELL_LOCAL_DAEMON", "0"), ("XSHELL_DAEMON_BIN", other_s)]
            ),
            LocalMode::InProcess { .. }
        ));
        assert!(matches!(
            resolve(Some(&exe_dir), &[("XSHELL_LOCAL_DAEMON", "1")]),
            LocalMode::Daemon { .. }
        ));
    }

    #[test]
    fn info_json() {
        let i = LocalMode::InProcess { reason: "r".into() }.info();
        assert_eq!(
            serde_json::to_string(&i).unwrap(),
            r#"{"mode":"in-process","reason":"r"}"#
        );
    }
}

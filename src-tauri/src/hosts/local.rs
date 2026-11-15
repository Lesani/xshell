//! How the Local Host runs its Terminals.
//!
//! Linux and macOS (ADR-0017): always in the one always-on Daemon of this machine. The
//! Desktop embeds its own `xshelld` and installs it under `~/.xshell/server/<dir>/`; a
//! sidecar or `XSHELL_DAEMON_BIN` wins for development and tests. In-process Terminals are
//! unreachable there (`XSHELL_LOCAL_DAEMON=0` remains, for tests only): failing to start the
//! Daemon is an error with the log path and Retry, never a fallback.
//!
//! Windows (unchanged): a GUI-bound Daemon (ADR-0005, #24) when the app finds its `xshelld`,
//! otherwise in this process.

use serde::Serialize;
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The `settings.json` key of the Persistent Daemon setting. Rust reads and writes it: it is
/// needed before the frontend loads, when the Local Host starts dialing.
pub const PERSISTENT_KEY: &str = "local_persistent_daemon";

/// Where the Daemon binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonSource {
    /// A file: `XSHELL_DAEMON_BIN` or the sidecar next to the app.
    File,
    /// The bytes embedded in this Desktop, installed to `bin` before the first start.
    Embedded,
    /// This build carries no `xshelld` (debug builds): every start fails with this reason.
    Missing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalMode {
    /// New local Tabs are Terminals of a Daemon started from `bin`.
    Daemon { bin: PathBuf, source: DaemonSource },
    /// Local Tabs run in this process; `reason` is logged and returned to the frontend.
    /// Windows without a Daemon; on Unix only `XSHELL_LOCAL_DAEMON=0` (tests).
    InProcess { reason: String },
    /// Unix: the Local Host's Daemon cannot even be set up (no home directory, …).
    Failed { reason: String },
}

/// What `local_host_info` returns (and the `local:daemon` event carries).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalHostInfo {
    /// `"daemon"`, `"in-process"` or `"failed"`.
    pub mode: &'static str,
    pub reason: Option<String>,
    /// Legacy (always unsupported on every platform now); S7 removes it.
    pub persistent: PersistentInfo,
    /// The Daemon's state; `None` outside Daemon mode.
    pub daemon: Option<DaemonInfo>,
    /// What keeps the Daemon running; `None` outside Daemon mode (and on Windows).
    pub service: Option<ServiceInfo>,
}

/// The Local Host's Daemon as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    /// `"starting"`, `"ready"` (hello done and Terminals listed) or `"failed"`.
    pub state: &'static str,
    pub error: Option<String>,
    /// `"older-desktop"`: a window of an older xshell runs this computer's Terminals in its
    /// GUI-bound Daemon; close it first (A10). `None` otherwise.
    pub error_kind: Option<&'static str>,
    /// The Daemon log, for the error view.
    pub log: Option<String>,
}

/// `xshelld service install`'s outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ServiceInfo {
    /// `"systemd"`, `"cron"`, `"profile"` or `"none"`; `None` until the report arrives.
    pub mechanism: Option<String>,
    pub linger: Option<bool>,
    pub error: Option<String>,
}

/// The Persistent Daemon setting, for Settings → Hosts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistentInfo {
    /// Whether the setting can be offered (Linux and macOS, Daemon mode).
    pub supported: bool,
    pub enabled: bool,
    /// The connected Daemon's mode, `"gui-bound"` or `"persistent"`; `None` while not
    /// connected.
    pub running: Option<&'static str>,
    /// The Daemon log, for error text.
    pub log: Option<String>,
}

impl LocalMode {
    pub fn info(
        &self,
        persistent: PersistentInfo,
        daemon: Option<DaemonInfo>,
        service: Option<ServiceInfo>,
    ) -> LocalHostInfo {
        match self {
            LocalMode::Daemon { .. } => LocalHostInfo {
                mode: "daemon",
                reason: None,
                persistent,
                daemon,
                service,
            },
            LocalMode::InProcess { reason } => LocalHostInfo {
                mode: "in-process",
                reason: Some(reason.clone()),
                persistent: PersistentInfo::default(),
                daemon: None,
                service: None,
            },
            // The Daemon object is kept (Unix): the error view needs the log path.
            LocalMode::Failed { reason } => LocalHostInfo {
                mode: "failed",
                reason: Some(reason.clone()),
                persistent: PersistentInfo::default(),
                daemon,
                service: None,
            },
        }
    }
}

/// The Persistent Daemon setting is no longer offered anywhere: Linux and macOS always run
/// the always-on Daemon (ADR-0017), Windows only the GUI-bound one.
pub fn persistent_supported(_mode: &LocalMode, _linux_or_macos: bool) -> bool {
    false
}

/// The Local Host Daemon's state from what hostlink knows: `ready` once the Local Host
/// finished hello and has its Terminal list (A8); `failed` when a start failed, the Daemon
/// is incompatible, or the link is offline with an error; otherwise `starting`.
pub fn daemon_state(
    start_error: Option<String>,
    status: Option<&xshell_hostlink::HostStatus>,
    listed: bool,
) -> (&'static str, Option<String>) {
    use xshell_hostlink::StatusKind;
    if let Some(e) = start_error {
        return ("failed", Some(e));
    }
    let Some(st) = status else {
        return ("starting", None);
    };
    match st.status {
        _ if st.usable() && listed => ("ready", None),
        StatusKind::Incompatible => (
            "failed",
            Some(
                st.last_error
                    .clone()
                    .unwrap_or_else(|| "this computer's Daemon is incompatible".into()),
            ),
        ),
        StatusKind::Offline => ("failed", st.last_error.clone()),
        _ => ("starting", None),
    }
}

/// The [`DaemonInfo::error_kind`] of a start error.
pub fn error_kind(error: Option<&str>) -> Option<&'static str> {
    error
        .filter(|e| e.starts_with("older-desktop:"))
        .map(|_| "older-desktop")
}

/// The `settings.json` key of the quit prompt's setting.
pub const KEEP_SESSIONS_KEY: &str = "local_keep_sessions";

/// Whether quitting asks before leaving local Terminals running (ADR-0017).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepSessions {
    /// Ask "Keep N sessions running?" (the default).
    Ask,
    /// Always keep them running, without asking.
    Always,
}

impl KeepSessions {
    pub fn as_str(self) -> &'static str {
        match self {
            KeepSessions::Ask => "ask",
            KeepSessions::Always => "always",
        }
    }

    pub fn parse(s: &str) -> Option<KeepSessions> {
        match s {
            "ask" => Some(KeepSessions::Ask),
            "always" => Some(KeepSessions::Always),
            _ => None,
        }
    }
}

/// The stored setting; `Ask` when missing or invalid.
pub fn read_keep_sessions(store: Option<&dyn SettingStore>) -> KeepSessions {
    store
        .and_then(|s| s.get(KEEP_SESSIONS_KEY))
        .and_then(|v| v.as_str().and_then(KeepSessions::parse))
        .unwrap_or(KeepSessions::Ask)
}

/// The store plugin's view of `settings.json`: a cache that `set` changes at once and `save`
/// writes to disk.
pub trait RawStore: Send + Sync {
    fn get(&self, key: &str) -> Option<Value>;
    fn set(&self, key: &str, value: Value);
    fn delete(&self, key: &str);
    fn save(&self) -> Result<(), String>;
}

/// Where the setting is stored (`settings.json`, shared with the frontend).
pub trait SettingStore: Send + Sync {
    /// The last value known to be on disk.
    fn get(&self, key: &str) -> Option<Value>;
    /// Set and write to disk now. On error the cache is put back, so nothing reports a
    /// value that is not on disk.
    fn set_saved(&self, key: &str, value: Value) -> Result<(), String>;
}

/// [`SettingStore`] over a [`RawStore`]: tracks the last durable value apart from the cache,
/// and saves on every commit, retries included.
pub struct CommitStore<S: RawStore> {
    raw: S,
    durable: std::sync::Mutex<std::collections::HashMap<String, Option<Value>>>,
}

impl<S: RawStore> CommitStore<S> {
    pub fn new(raw: S) -> Self {
        Self {
            raw,
            durable: Default::default(),
        }
    }
}

impl<S: RawStore> SettingStore for CommitStore<S> {
    fn get(&self, key: &str) -> Option<Value> {
        let d = self.durable.lock().unwrap();
        match d.get(key) {
            Some(v) => v.clone(),
            // Not written by us yet: what was loaded from disk.
            None => self.raw.get(key),
        }
    }

    fn set_saved(&self, key: &str, value: Value) -> Result<(), String> {
        let mut d = self.durable.lock().unwrap();
        let before = self.raw.get(key);
        self.raw.set(key, value.clone());
        match self.raw.save() {
            Ok(()) => {
                d.insert(key.into(), Some(value));
                Ok(())
            }
            Err(e) => {
                match before {
                    Some(v) => self.raw.set(key, v),
                    None => self.raw.delete(key),
                }
                Err(e)
            }
        }
    }
}

/// The stored setting; off when absent or not a boolean.
pub fn read_persistent(store: Option<&dyn SettingStore>) -> bool {
    store
        .and_then(|s| s.get(PERSISTENT_KEY))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// What `local_daemon_set_persistent` answers on failure: `other-app`, `timeout`,
/// `confirm-again:<n>` (n Terminals run now), or `failed:<message>`.
pub fn switch_error_code(e: &xshell_hostlink::SwitchError) -> String {
    match e {
        xshell_hostlink::SwitchError::OtherApp => "other-app".into(),
        xshell_hostlink::SwitchError::Timeout => "timeout".into(),
        xshell_hostlink::SwitchError::ConfirmAgain(n) => format!("confirm-again:{n}"),
        xshell_hostlink::SwitchError::Failed(m) => format!("failed:{m}"),
    }
}

/// Turn the setting on or off around `switch(target)`, which hands the Terminals over and
/// returns its result and the mode that then runs (`true`: Persistent). The stored value is
/// committed with a checked save at each commit point: on before the hand-over (a failed save
/// aborts it), then whatever runs. If that last save fails, operation goes back to the last
/// durable mode, so the next start agrees with what runs, and the error is reported.
pub fn apply_switch(
    store: &dyn SettingStore,
    enabled: bool,
    mut switch: impl FnMut(bool) -> (Result<(), xshell_hostlink::SwitchError>, bool),
) -> Result<(), String> {
    if enabled {
        store
            .set_saved(PERSISTENT_KEY, Value::Bool(true))
            .map_err(|e| format!("failed:{e}"))?;
    }
    let (r, now) = switch(enabled);
    match store.set_saved(PERSISTENT_KEY, Value::Bool(now)) {
        Ok(()) => r.map_err(|e| switch_error_code(&e)),
        Err(e) => {
            let durable = read_persistent(Some(store));
            if durable != now {
                let _ = switch(durable);
            }
            Err(format!("failed:{e}"))
        }
    }
}

/// The Daemon binary, in this order:
/// 1. `XSHELL_DAEMON_BIN` (development and tests);
/// 2. `xshelld` (`xshelld.exe` on Windows) next to the app's executable: the bundled sidecar
///    (Tauri strips the target triple from sidecar names), the one in the portable zip, and
///    `target/debug/xshelld` under `tauri dev`.
///
/// `XSHELL_LOCAL_DAEMON=0` keeps local Tabs in this process, as does an app without one
/// (the bare portable `xshell.exe`).
pub fn resolve_daemon_binary(
    exe_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> LocalMode {
    let in_process = |reason: String| LocalMode::InProcess { reason };
    if env("XSHELL_LOCAL_DAEMON").as_deref() == Some("0".as_ref()) {
        return in_process("XSHELL_LOCAL_DAEMON=0".into());
    }
    if let Some(p) = env("XSHELL_DAEMON_BIN").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() {
            LocalMode::Daemon {
                bin: p,
                source: DaemonSource::File,
            }
        } else {
            in_process(format!("XSHELL_DAEMON_BIN {} is not a file", p.display()))
        };
    }
    let Some(dir) = exe_dir else {
        return in_process("the app's directory is unknown".into());
    };
    let bin = dir.join(daemon_file_name());
    if bin.is_file() {
        LocalMode::Daemon {
            bin,
            source: DaemonSource::File,
        }
    } else {
        in_process(format!("no xshelld in {}", dir.display()))
    }
}

/// Which Daemon the Local Host uses. Windows (`unix == false`): exactly
/// [`resolve_daemon_binary`]. Unix, in this order:
/// 1. `XSHELL_LOCAL_DAEMON=0`: in-process (tests only);
/// 2. `XSHELL_DAEMON_BIN`, even if it is not a file (the start then reports it);
/// 3. the sidecar next to the app;
/// 4. the embedded Daemon, installed at `install_bin` (`~/.xshell/server/<dir>/xshelld`);
/// 5. otherwise `Missing`: every start fails with the reason, and Retry shows it.
pub fn resolve_local(
    exe_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
    unix: bool,
    embedded: bool,
    install_bin: Option<&Path>,
) -> LocalMode {
    if !unix {
        return match resolve_daemon_binary(exe_dir, env) {
            LocalMode::Daemon { bin, .. } => LocalMode::Daemon {
                bin,
                source: DaemonSource::File,
            },
            other => other,
        };
    }
    if env("XSHELL_LOCAL_DAEMON").as_deref() == Some("0".as_ref()) {
        return LocalMode::InProcess {
            reason: "XSHELL_LOCAL_DAEMON=0".into(),
        };
    }
    let file = |bin: PathBuf| LocalMode::Daemon {
        bin,
        source: DaemonSource::File,
    };
    if let Some(p) = env("XSHELL_DAEMON_BIN").filter(|p| !p.is_empty()) {
        return file(PathBuf::from(p));
    }
    let sidecar = exe_dir.map(|d| d.join(daemon_file_name()));
    if let Some(s) = sidecar.as_ref().filter(|s| s.is_file()) {
        return file(s.clone());
    }
    if embedded {
        return match install_bin {
            Some(bin) => LocalMode::Daemon {
                bin: bin.to_path_buf(),
                source: DaemonSource::Embedded,
            },
            None => LocalMode::Failed {
                reason: "the home directory is unknown".into(),
            },
        };
    }
    LocalMode::Daemon {
        bin: sidecar.unwrap_or_else(|| PathBuf::from(daemon_file_name())),
        source: DaemonSource::Missing(
            "this build carries no xshelld; for development: cargo build -p xshelld".into(),
        ),
    }
}

/// The Daemon's file name on this platform.
pub fn daemon_file_name() -> String {
    format!("xshelld{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(env: &[(&str, &str)]) -> HashMap<String, OsString> {
        env.iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect()
    }

    fn resolve_win(dir: Option<&Path>, env: &[(&str, &str)]) -> LocalMode {
        let env = env_of(env);
        resolve_local(dir, &|k| env.get(k).cloned(), false, true, None)
    }

    fn resolve_unix(
        dir: Option<&Path>,
        env: &[(&str, &str)],
        embedded: bool,
        install: Option<&Path>,
    ) -> LocalMode {
        let env = env_of(env);
        resolve_local(dir, &|k| env.get(k).cloned(), true, embedded, install)
    }

    fn file(bin: PathBuf) -> LocalMode {
        LocalMode::Daemon {
            bin,
            source: DaemonSource::File,
        }
    }

    #[test]
    fn resolve_unix_order() {
        let t = tempfile::tempdir().unwrap();
        let exe_dir = t.path().join("app");
        std::fs::create_dir(&exe_dir).unwrap();
        let other = t.path().join("dev-xshelld");
        std::fs::write(&other, "").unwrap();
        let other_s = other.to_str().unwrap();
        let install = t.path().join("home/.xshell/server/1.5.0+abc/xshelld");
        // Nothing beside the app, nothing embedded: Missing, with the reason, never in-process.
        match resolve_unix(Some(&exe_dir), &[], false, Some(&install)) {
            LocalMode::Daemon {
                source: DaemonSource::Missing(r),
                bin,
            } => {
                assert!(r.contains("carries no xshelld"), "{r}");
                assert_eq!(bin, exe_dir.join("xshelld"));
            }
            m => panic!("{m:?}"),
        }
        // Embedded: installed under the home.
        assert_eq!(
            resolve_unix(Some(&exe_dir), &[], true, Some(&install)),
            LocalMode::Daemon {
                bin: install.clone(),
                source: DaemonSource::Embedded
            }
        );
        assert!(matches!(
            resolve_unix(Some(&exe_dir), &[], true, None),
            LocalMode::Failed { .. }
        ));
        // The sidecar wins over the embedded copy.
        let sidecar = exe_dir.join("xshelld");
        std::fs::write(&sidecar, "").unwrap();
        assert_eq!(
            resolve_unix(Some(&exe_dir), &[], true, Some(&install)),
            file(sidecar.clone())
        );
        // The environment wins, even a path that is not a file (the start reports it).
        assert_eq!(
            resolve_unix(
                Some(&exe_dir),
                &[("XSHELL_DAEMON_BIN", other_s)],
                true,
                None
            ),
            file(other.clone())
        );
        assert_eq!(
            resolve_unix(
                Some(&exe_dir),
                &[("XSHELL_DAEMON_BIN", "/nonexistent/xshelld")],
                true,
                None
            ),
            file("/nonexistent/xshelld".into())
        );
        // The test switch wins over everything.
        assert!(matches!(
            resolve_unix(
                Some(&exe_dir),
                &[("XSHELL_LOCAL_DAEMON", "0"), ("XSHELL_DAEMON_BIN", other_s)],
                true,
                Some(&install)
            ),
            LocalMode::InProcess { .. }
        ));
    }

    #[test]
    fn resolve_windows_unchanged() {
        let t = tempfile::tempdir().unwrap();
        let exe_dir = t.path().join("app");
        std::fs::create_dir(&exe_dir).unwrap();
        // Every fallback stays in-process; nothing embedded applies.
        assert!(
            matches!(&resolve_win(Some(&exe_dir), &[]), LocalMode::InProcess { reason } if reason.contains("no xshelld"))
        );
        assert_eq!(
            resolve_win(None, &[]),
            LocalMode::InProcess {
                reason: "the app's directory is unknown".into()
            }
        );
        assert!(matches!(
            resolve_win(
                Some(&exe_dir),
                &[("XSHELL_DAEMON_BIN", "/nonexistent/xshelld")]
            ),
            LocalMode::InProcess { .. }
        ));
        let sidecar = exe_dir.join(daemon_file_name());
        std::fs::write(&sidecar, "").unwrap();
        assert_eq!(resolve_win(Some(&exe_dir), &[]), file(sidecar));
        assert!(matches!(
            resolve_win(Some(&exe_dir), &[("XSHELL_LOCAL_DAEMON", "0")]),
            LocalMode::InProcess { .. }
        ));
    }

    #[test]
    fn info_json_daemon_state_and_service() {
        let i = LocalMode::InProcess { reason: "r".into() }.info(
            PersistentInfo {
                supported: true,
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(
            serde_json::to_string(&i).unwrap(),
            r#"{"mode":"in-process","reason":"r","persistent":{"supported":false,"enabled":false,"running":null,"log":null},"daemon":null,"service":null}"#
        );
        let i = LocalMode::Failed {
            reason: "no home".into(),
        }
        .info(Default::default(), None, None);
        assert_eq!(
            serde_json::to_string(&i).unwrap(),
            r#"{"mode":"failed","reason":"no home","persistent":{"supported":false,"enabled":false,"running":null,"log":null},"daemon":null,"service":null}"#
        );
        let i = file("/x".into()).info(
            Default::default(),
            Some(DaemonInfo {
                state: "ready",
                error: None,
                error_kind: None,
                log: Some("/h/.xshell/log/xshelld.log".into()),
            }),
            Some(ServiceInfo {
                mechanism: Some("profile".into()),
                linger: None,
                error: None,
            }),
        );
        assert_eq!(
            serde_json::to_string(&i).unwrap(),
            r#"{"mode":"daemon","reason":null,"persistent":{"supported":false,"enabled":false,"running":null,"log":null},"daemon":{"state":"ready","error":null,"errorKind":null,"log":"/h/.xshell/log/xshelld.log"},"service":{"mechanism":"profile","linger":null,"error":null}}"#
        );
    }

    #[test]
    fn daemon_state_ready_only_when_negotiated_and_listed() {
        use xshell_hostlink::{HostStatus, StatusKind};
        let mut st = HostStatus::initial("local", "1.5.0", None, 1);
        assert_eq!(daemon_state(None, None, false), ("starting", None));
        assert_eq!(daemon_state(None, Some(&st), false), ("starting", None));
        st.set_kind(StatusKind::Connected);
        // Connected but no Terminal list yet: not ready.
        assert_eq!(daemon_state(None, Some(&st), false), ("starting", None));
        assert_eq!(daemon_state(None, Some(&st), true), ("ready", None));
        // A start error wins.
        assert_eq!(
            daemon_state(Some("exited with 1".into()), Some(&st), true),
            ("failed", Some("exited with 1".into()))
        );
        // An incompatible (older) Daemon or a refused hello: failed, with the reason.
        st.set_kind(StatusKind::Incompatible);
        st.last_error = Some("the Daemon is older".into());
        assert_eq!(
            daemon_state(None, Some(&st), true),
            ("failed", Some("the Daemon is older".into()))
        );
        st.set_kind(StatusKind::Offline);
        st.last_error = Some("hello refused".into());
        assert_eq!(
            daemon_state(None, Some(&st), false),
            ("failed", Some("hello refused".into()))
        );
    }

    #[test]
    fn persistent_never_offered() {
        for m in [
            file("/x".into()),
            LocalMode::InProcess { reason: "r".into() },
        ] {
            assert!(!persistent_supported(&m, true));
            assert!(!persistent_supported(&m, false));
        }
    }

    #[test]
    fn keep_sessions_defaults_to_ask() {
        assert_eq!(read_keep_sessions(None), KeepSessions::Ask);
        let f = Fake::with(None);
        assert_eq!(read_keep_sessions(Some(&*f)), KeepSessions::Ask);
        f.set_saved(KEEP_SESSIONS_KEY, Value::from("bogus"))
            .unwrap();
        assert_eq!(read_keep_sessions(Some(&*f)), KeepSessions::Ask);
        f.set_saved(KEEP_SESSIONS_KEY, Value::Bool(true)).unwrap();
        assert_eq!(read_keep_sessions(Some(&*f)), KeepSessions::Ask);
        f.set_saved(KEEP_SESSIONS_KEY, Value::from("always"))
            .unwrap();
        assert_eq!(read_keep_sessions(Some(&*f)), KeepSessions::Always);
        assert_eq!(KeepSessions::parse("ask"), Some(KeepSessions::Ask));
        assert_eq!(KeepSessions::Always.as_str(), "always");
    }

    use std::sync::Mutex;
    use xshell_hostlink::SwitchError;

    /// The plugin store: `set` changes the cache at once, `save` copies it to "disk" (or
    /// fails). Every save is logged with the value saved.
    #[derive(Default)]
    struct Raw {
        cache: Mutex<std::collections::HashMap<String, Value>>,
        disk: Mutex<std::collections::HashMap<String, Value>>,
        log: Arc<Mutex<Vec<String>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl RawStore for Raw {
        fn get(&self, key: &str) -> Option<Value> {
            self.cache.lock().unwrap().get(key).cloned()
        }
        fn set(&self, key: &str, value: Value) {
            self.cache.lock().unwrap().insert(key.into(), value);
        }
        fn delete(&self, key: &str) {
            self.cache.lock().unwrap().remove(key);
        }
        fn save(&self) -> Result<(), String> {
            let v = self.get(PERSISTENT_KEY).unwrap_or(Value::Null);
            self.log.lock().unwrap().push(format!("save {v}"));
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("disk full".into());
            }
            *self.disk.lock().unwrap() = self.cache.lock().unwrap().clone();
            Ok(())
        }
    }

    use std::sync::Arc;

    struct Fake {
        store: CommitStore<Raw>,
        log: Arc<Mutex<Vec<String>>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl std::ops::Deref for Fake {
        type Target = CommitStore<Raw>;
        fn deref(&self) -> &Self::Target {
            &self.store
        }
    }

    impl Fake {
        fn with(v: Option<Value>) -> Fake {
            let raw = Raw::default();
            if let Some(v) = v {
                raw.cache
                    .lock()
                    .unwrap()
                    .insert(PERSISTENT_KEY.into(), v.clone());
                raw.disk.lock().unwrap().insert(PERSISTENT_KEY.into(), v);
            }
            let (log, fail) = (raw.log.clone(), raw.fail.clone());
            Fake {
                store: CommitStore::new(raw),
                log,
                fail,
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn note(&self, s: &str) {
            self.log.lock().unwrap().push(s.into());
        }
        fn fail(&self, on: bool) {
            self.fail.store(on, std::sync::atomic::Ordering::SeqCst);
        }
        fn cache(&self) -> Option<Value> {
            self.store.raw.get(PERSISTENT_KEY)
        }
        fn disk(&self) -> Option<Value> {
            self.store
                .raw
                .disk
                .lock()
                .unwrap()
                .get(PERSISTENT_KEY)
                .cloned()
        }
        fn on(&self) -> bool {
            read_persistent(Some(&self.store))
        }
    }

    /// The startup read: only a stored `true` turns it on.
    #[test]
    fn startup_reads_the_stored_setting() {
        assert!(!read_persistent(None));
        assert!(!Fake::with(None).on());
        assert!(!Fake::with(Some(Value::Bool(false))).on());
        assert!(!Fake::with(Some(Value::from("true"))).on());
        assert!(Fake::with(Some(Value::Bool(true))).on());
    }

    #[test]
    fn switching_on_saves_before_the_hand_over() {
        let f = Fake::with(None);
        let r = apply_switch(&*f, true, |t| {
            f.note(&format!("switch {t}"));
            (Ok(()), true)
        });
        assert_eq!(r, Ok(()));
        assert_eq!(f.log(), ["save true", "switch true", "save true"]);
        assert!(f.on());
        assert_eq!(f.disk(), Some(Value::Bool(true)));
    }

    /// A failed save before the hand-over aborts it and leaves the cache as it was; a retry
    /// saves again.
    #[test]
    fn switching_on_aborts_when_the_save_fails() {
        let f = Fake::with(Some(Value::Bool(false)));
        f.fail(true);
        let r = apply_switch(&*f, true, |_| panic!("switched without a saved setting"));
        assert_eq!(r, Err("failed:disk full".into()));
        assert_eq!(
            f.cache(),
            Some(Value::Bool(false)),
            "the cache kept the unsaved value"
        );
        assert!(!f.on());
        f.fail(false);
        let r = apply_switch(&*f, true, |_| (Ok(()), true));
        assert_eq!(r, Ok(()));
        assert_eq!(f.disk(), Some(Value::Bool(true)));
        // Absent before: removed again on failure.
        let g = Fake::with(None);
        g.fail(true);
        let _ = apply_switch(&*g, true, |_| panic!("switched"));
        assert_eq!(g.cache(), None);
    }

    #[test]
    fn switching_off_saves_after_the_hand_over() {
        let f = Fake::with(Some(Value::Bool(true)));
        let r = apply_switch(&*f, false, |t| {
            f.note(&format!("switch {t}"));
            (Ok(()), false)
        });
        assert_eq!(r, Ok(()));
        assert_eq!(f.log(), ["switch false", "save false"]);
        assert!(!f.on());
        assert_eq!(f.disk(), Some(Value::Bool(false)));
    }

    /// The off commit fails: operation goes back to the durable mode (on), and the error is
    /// reported.
    #[test]
    fn switching_off_reconciles_when_the_save_fails() {
        let f = Fake::with(Some(Value::Bool(true)));
        f.fail(true);
        let r = apply_switch(&*f, false, |t| {
            f.note(&format!("switch {t}"));
            (Ok(()), t)
        });
        assert_eq!(r, Err("failed:disk full".into()));
        assert_eq!(f.log(), ["switch false", "save false", "switch true"]);
        assert_eq!(f.cache(), Some(Value::Bool(true)));
        assert!(f.on());
    }

    /// A failed switch stores what runs, and reports the switch's error.
    #[test]
    fn failed_switch_stores_what_runs() {
        let f = Fake::with(None);
        let r = apply_switch(&*f, true, |_| (Err(SwitchError::Timeout), false));
        assert_eq!(r, Err("timeout".into()));
        assert!(!f.on());
        assert_eq!(f.disk(), Some(Value::Bool(false)));
        let f = Fake::with(Some(Value::Bool(true)));
        let r = apply_switch(&*f, false, |_| (Err(SwitchError::OtherApp), true));
        assert_eq!(r, Err("other-app".into()));
        assert!(f.on());
        let r = apply_switch(&*f, false, |_| (Err(SwitchError::ConfirmAgain(2)), true));
        assert_eq!(r, Err("confirm-again:2".into()));
        assert!(f.on());
        let r = apply_switch(&*f, true, |_| (Err(SwitchError::Failed("x".into())), true));
        assert_eq!(r, Err("failed:x".into()));
    }
}

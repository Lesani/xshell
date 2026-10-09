//! How the Local Host runs its Terminals: in a Daemon (ADR-0005) when the app finds its
//! `xshelld`, otherwise in this process as before. The Daemon is GUI-bound unless the
//! Persistent Daemon setting (#25, Linux and macOS) is on.

use serde::Serialize;
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The `settings.json` key of the Persistent Daemon setting. Rust reads and writes it: it is
/// needed before the frontend loads, when the Local Host starts dialing.
pub const PERSISTENT_KEY: &str = "local_persistent_daemon";

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
    pub persistent: PersistentInfo,
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
    pub fn info(&self, persistent: PersistentInfo) -> LocalHostInfo {
        match self {
            LocalMode::Daemon { .. } => LocalHostInfo {
                mode: "daemon",
                reason: None,
                persistent,
            },
            LocalMode::InProcess { reason } => LocalHostInfo {
                mode: "in-process",
                reason: Some(reason.clone()),
                persistent: PersistentInfo::default(),
            },
        }
    }
}

/// The setting is offered only on Linux and macOS, and only when local Tabs run in a Daemon.
pub fn persistent_supported(mode: &LocalMode, linux_or_macos: bool) -> bool {
    linux_or_macos && matches!(mode, LocalMode::Daemon { .. })
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
        assert_eq!(m.info(Default::default()).mode, "in-process");
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
        assert_eq!(
            resolve(Some(&exe_dir), &[]).info(Default::default()).mode,
            "daemon"
        );
        assert_eq!(
            resolve(Some(&exe_dir), &[]).info(Default::default()).reason,
            None
        );

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
        let i = LocalMode::InProcess { reason: "r".into() }.info(PersistentInfo {
            supported: true,
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_string(&i).unwrap(),
            r#"{"mode":"in-process","reason":"r","persistent":{"supported":false,"enabled":false,"running":null,"log":null}}"#
        );
        let i = LocalMode::Daemon { bin: "/x".into() }.info(PersistentInfo {
            supported: true,
            enabled: true,
            running: Some("persistent"),
            log: Some("/l".into()),
        });
        assert_eq!(
            serde_json::to_string(&i).unwrap(),
            r#"{"mode":"daemon","reason":null,"persistent":{"supported":true,"enabled":true,"running":"persistent","log":"/l"}}"#
        );
    }

    #[test]
    fn persistent_supported_only_linux_macos_daemon_mode() {
        let d = LocalMode::Daemon { bin: "/x".into() };
        let p = LocalMode::InProcess { reason: "r".into() };
        assert!(persistent_supported(&d, true));
        assert!(!persistent_supported(&d, false));
        assert!(!persistent_supported(&p, true));
        assert_eq!(
            super::super::linux_or_macos(),
            cfg!(any(target_os = "linux", target_os = "macos"))
        );
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

//! Moving saved in-process Local Tabs into the local Daemon across xshell instances (several
//! may share one settings file):
//! - the lock: an exclusive advisory lock on `local-migration.lock` in the app data dir, held
//!   by the process until it unlocks or exits (a crash releases it too);
//! - the journal: `local-migration.json` next to it, replaced atomically;
//! - the settings guard: an instance without the lock saves settings.json with the disk's
//!   `open_tabs`, `open_groups` and `terminal_zoom`, never its own (possibly stale) copies.

use serde_json::Value;
use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};

const POLL: Duration = Duration::from_millis(50);

/// The held lock file, if this process holds it.
#[derive(Default)]
pub struct MigrationLock(Mutex<Option<File>>);

/// Take the lock on `path`, waiting up to `wait` for another holder to let go. `None`: still
/// held elsewhere after `wait`.
pub fn acquire(path: &Path, wait: Duration) -> io::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let deadline = Instant::now() + wait;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Some(f)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
}

fn data_file(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|d| d.join(name))
        .map_err(|e| e.to_string())
}

fn lock_path(app: &AppHandle) -> Result<PathBuf, String> {
    data_file(app, "local-migration.lock")
}

const JOURNAL: &str = "local-migration.json";

/// The journal at `path`, if there is one.
pub fn read_journal(path: &Path) -> io::Result<Option<Value>> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Replace the journal at `path` atomically: a synced temp file renamed over it.
pub fn write_journal(path: &Path, j: &Value) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&serde_json::to_vec(j).map_err(io::Error::other)?)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    sync_dir(dir)
}

pub fn clear_journal(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => path.parent().map_or(Ok(()), sync_dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub fn local_migration_journal_read(app: AppHandle) -> Result<Option<Value>, String> {
    read_journal(&data_file(&app, JOURNAL)?).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn local_migration_journal_write(app: AppHandle, journal: Value) -> Result<(), String> {
    write_journal(&data_file(&app, JOURNAL)?, &journal).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn local_migration_journal_clear(app: AppHandle) -> Result<(), String> {
    clear_journal(&data_file(&app, JOURNAL)?).map_err(|e| e.to_string())
}

/// The keys only the migration lock's holder writes.
const GUARDED: [&str; 3] = ["open_tabs", "open_groups", "terminal_zoom"];

/// Set: this process does not hold the lock, and saves of the settings store at this path
/// keep the guarded keys as they are on disk.
///
/// Not covered (a known, pre-existing risk for every setting with two instances): two
/// processes saving the shared settings.json at the same moment. Each save reads the disk
/// copy and writes the whole file, so the later write wins and an interleaved one can lose
/// the other's change; the store plugin has no cross-process locking.
static GUARD: Mutex<Option<PathBuf>> = Mutex::new(None);

type SerializeResult = Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>;

/// `cache` as the settings file, with the guarded keys taken from the file at `guard` (a key
/// missing there is left out).
pub fn serialize_guarded(cache: &HashMap<String, Value>, guard: Option<&Path>) -> SerializeResult {
    let Some(path) = guard else {
        return Ok(serde_json::to_vec_pretty(cache)?);
    };
    let disk: HashMap<String, Value> = match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => return Err(e.into()),
    };
    let mut out = cache.clone();
    for k in GUARDED {
        match disk.get(k) {
            Some(v) => out.insert(k.to_string(), v.clone()),
            None => out.remove(k),
        };
    }
    Ok(serde_json::to_vec_pretty(&out)?)
}

/// The store plugin's serializer (settings.json is the app's only store).
pub fn serialize_settings(cache: &HashMap<String, Value>) -> SerializeResult {
    let guard = GUARD.lock().unwrap().clone();
    serialize_guarded(cache, guard.as_deref())
}

/// Make the last save of settings.json durable: fsync the file and its directory.
#[tauri::command]
pub fn local_migration_sync_settings(app: AppHandle) -> Result<(), String> {
    let path =
        tauri_plugin_store::resolve_store_path(&app, "settings.json").map_err(|e| e.to_string())?;
    sync_file(&path).map_err(|e| e.to_string())
}

pub fn sync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()?;
    path.parent().map_or(Ok(()), sync_dir)
}

/// Turn the settings guard on for the rest of this process's life.
#[tauri::command]
pub fn local_migration_guard(app: AppHandle) -> Result<(), String> {
    let path =
        tauri_plugin_store::resolve_store_path(&app, "settings.json").map_err(|e| e.to_string())?;
    *GUARD.lock().unwrap() = Some(path);
    Ok(())
}

/// Answers whether this process holds the lock (already, or within `wait_ms`).
#[tauri::command]
pub async fn local_migration_lock(
    app: AppHandle,
    state: State<'_, MigrationLock>,
    wait_ms: u64,
) -> Result<bool, String> {
    if state.0.lock().unwrap().is_some() {
        return Ok(true);
    }
    let path = lock_path(&app)?;
    let got = tauri::async_runtime::spawn_blocking(move || {
        acquire(&path, Duration::from_millis(wait_ms))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let Some(f) = got else { return Ok(false) };
    let mut held = state.0.lock().unwrap();
    // A concurrent call of this process got there first: keep that one.
    if held.is_none() {
        *held = Some(f);
    }
    Ok(true)
}

#[tauri::command]
pub fn local_migration_unlock(state: State<'_, MigrationLock>) {
    // Dropping the file releases the lock.
    state.0.lock().unwrap().take();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("local-migration.lock");
        let a = acquire(&path, Duration::ZERO).unwrap();
        assert!(a.is_some());
        // A second open file description (another instance) cannot take it.
        let t0 = Instant::now();
        assert!(acquire(&path, Duration::from_millis(200))
            .unwrap()
            .is_none());
        assert!(t0.elapsed() >= Duration::from_millis(200));
        drop(a);
        // A child forked meanwhile by another test holds the description until its exec
        // (the file is close-on-exec), so allow a moment.
        assert!(acquire(&path, Duration::from_secs(2)).unwrap().is_some());
    }

    #[test]
    fn journal_is_replaced_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d").join(JOURNAL);
        assert_eq!(read_journal(&path).unwrap(), None);
        write_journal(&path, &serde_json::json!({ "sent": ["a"] })).unwrap();
        write_journal(&path, &serde_json::json!({ "sent": ["b"] })).unwrap();
        assert_eq!(
            read_journal(&path).unwrap(),
            Some(serde_json::json!({ "sent": ["b"] }))
        );
        assert!(!path.with_extension("json.tmp").exists());
        sync_file(&path).unwrap();
        clear_journal(&path).unwrap();
        clear_journal(&path).unwrap();
        assert!(sync_file(&path).is_err());
        assert_eq!(read_journal(&path).unwrap(), None);
    }

    /// B cached A's saved Tabs; A closed one and regrouped; B then saves another setting.
    #[test]
    fn guarded_save_keeps_the_disk_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let disk = serde_json::json!({ "open_tabs": [{ "id": "x" }], "open_groups": [{ "id": "g" }], "theme": "dark" });
        std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
        let cache: HashMap<String, Value> = serde_json::from_value(serde_json::json!({
            "open_tabs": [{ "id": "x" }, { "id": "y" }], "open_groups": [], "terminal_zoom": { "y": 20 }, "theme": "light"
        })).unwrap();
        let out: Value =
            serde_json::from_slice(&serialize_guarded(&cache, Some(&path)).unwrap()).unwrap();
        assert_eq!(
            out,
            serde_json::json!({ "open_tabs": [{ "id": "x" }], "open_groups": [{ "id": "g" }], "theme": "light" })
        );
        // Unguarded: the cache as it is.
        let out: Value = serde_json::from_slice(&serialize_guarded(&cache, None).unwrap()).unwrap();
        assert_eq!(out["open_tabs"].as_array().unwrap().len(), 2);
        // A corrupt file is never overwritten from a guarded save.
        std::fs::write(&path, b"{").unwrap();
        assert!(serialize_guarded(&cache, Some(&path)).is_err());
    }

    #[test]
    fn waits_for_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-migration.lock");
        let a = acquire(&path, Duration::ZERO).unwrap().unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(a);
        });
        assert!(acquire(&path, Duration::from_secs(5)).unwrap().is_some());
        t.join().unwrap();
    }
}

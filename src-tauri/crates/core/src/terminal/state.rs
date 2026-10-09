//! The Daemon's persisted Terminal list (`~/.xshell/daemon/terminals.json`): what a restart
//! relaunches, and which leftover processes it must end first.

use crate::launch::LaunchSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const STATE_VERSION: u32 = 1;

/// A process as it was when recorded: a pid plus its start time, so a reused pid can be told
/// apart from the original. `start_time` is platform-specific (Linux: clock ticks since boot;
/// macOS: microseconds since the epoch) and `None` where it cannot be read.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProcIdentity {
    pub pid: i32,
    #[serde(default)]
    pub start_time: Option<u64>,
}

/// The process that last ran a Terminal, so a restarted Daemon can end it if it survived.
/// Cleared (`None` in [`PersistedTerminal`]) once the Terminal's process has exited.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Leader {
    /// The session leader; also its process group id.
    pub pid: u32,
    /// The leader's start time (see [`ProcIdentity`]).
    #[serde(default)]
    pub start_time: Option<u64>,
    /// Process groups seen in the Terminal (the leader's and the last foreground job's),
    /// each identified by its group leader.
    #[serde(default)]
    pub groups: Vec<ProcIdentity>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PersistedTerminal {
    pub terminal: Uuid,
    pub spec: LaunchSpec,
    #[serde(default)]
    pub meta: Map<String, Value>,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: u64,
    #[serde(default)]
    pub leader: Option<Leader>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StateFile {
    pub version: u32,
    pub terminals: Vec<PersistedTerminal>,
}

/// What [`load`] found.
#[derive(Debug, Default)]
pub struct Loaded {
    pub terminals: Vec<PersistedTerminal>,
    /// Set when the file was unreadable and was renamed to this path.
    pub moved_aside: Option<PathBuf>,
}

/// Read the state file. A missing file is an empty list; a corrupt one is renamed to
/// `<name>.corrupt-<ms>` and also yields an empty list.
pub fn load(path: &Path) -> io::Result<Loaded> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded::default()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice::<StateFile>(&bytes) {
        Ok(f) => Ok(Loaded {
            terminals: f.terminals,
            moved_aside: None,
        }),
        Err(_) => {
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".corrupt-{ms}"));
            let aside = path.with_file_name(name);
            fs::rename(path, &aside)?;
            Ok(Loaded {
                terminals: Vec::new(),
                moved_aside: Some(aside),
            })
        }
    }
}

/// Replace the state file atomically: write `<path>.tmp` (0600), fsync, rename, fsync the
/// directory. Creates the parent directory (0700) if needed.
pub fn save_atomic(path: &Path, list: &[PersistedTerminal]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    let mut db = fs::DirBuilder::new();
    db.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut db, 0o700);
    db.create(dir)?;
    let file = StateFile {
        version: STATE_VERSION,
        terminals: list.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp)?;
    f.write_all(&json)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use serde_json::json;

    fn entry(cwd: &str) -> PersistedTerminal {
        let mut meta = Map::new();
        meta.insert("title".into(), json!("T"));
        PersistedTerminal {
            terminal: Uuid::new_v4(),
            spec: LaunchSpec {
                cwd: cwd.into(),
                session_id: Some("s".into()),
                ..Default::default()
            },
            meta,
            cols: 80,
            rows: 24,
            created_at_ms: 1,
            leader: Some(Leader {
                pid: 42,
                start_time: Some(7),
                groups: vec![
                    ProcIdentity {
                        pid: 42,
                        start_time: Some(7),
                    },
                    ProcIdentity {
                        pid: 43,
                        start_time: None,
                    },
                ],
            }),
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let fx = Fixture::new();
        let path = fx.dir.path().join("daemon/terminals.json");
        let list = vec![entry("/a"), entry("/b")];
        save_atomic(&path, &list).unwrap();
        assert_eq!(load(&path).unwrap().terminals, list);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dmode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dmode & 0o077, 0);
        }
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["terminals.json".to_string()]);
    }

    #[test]
    fn missing_file_empty() {
        let fx = Fixture::new();
        let l = load(&fx.dir.path().join("nope.json")).unwrap();
        assert!(l.terminals.is_empty() && l.moved_aside.is_none());
    }

    #[test]
    fn corrupt_moved_aside() {
        let fx = Fixture::new();
        let path = fx.write("d/terminals.json", "{garbage");
        let l = load(&path).unwrap();
        assert!(l.terminals.is_empty());
        let aside = l.moved_aside.unwrap();
        assert!(aside
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("terminals.json.corrupt-"));
        assert!(aside.exists() && !path.exists());
    }

    #[test]
    fn persisted_skip_permissions_roundtrip() {
        let fx = Fixture::new();
        let path = fx.dir.path().join("terminals.json");
        let mut e = entry("/a");
        e.spec.skip_permissions = Some(true);
        save_atomic(&path, std::slice::from_ref(&e)).unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("\"skipPermissions\": true"));
        assert_eq!(load(&path).unwrap().terminals, vec![e]);
    }

    #[test]
    fn persisted_without_field_loads_none() {
        let fx = Fixture::new();
        let path = fx.write(
            "terminals.json",
            r#"{"version":1,"terminals":[{"terminal":"6f1c1f2e-8a4e-4b7c-9d2a-1b2c3d4e5f60",
                "spec":{"cwd":"/w","sessionId":"s"},"cols":80,"rows":24,"createdAtMs":3}]}"#,
        );
        let l = load(&path).unwrap();
        assert_eq!(l.terminals[0].spec.skip_permissions, None);
    }

    #[test]
    fn older_entries_without_leader_load() {
        let fx = Fixture::new();
        let path = fx.write(
            "terminals.json",
            r#"{"version":1,"terminals":[{"terminal":"6f1c1f2e-8a4e-4b7c-9d2a-1b2c3d4e5f60",
                "spec":{"cwd":"/w"},"cols":80,"rows":24,"createdAtMs":3}]}"#,
        );
        let l = load(&path).unwrap();
        assert_eq!(l.terminals.len(), 1);
        assert_eq!(l.terminals[0].leader, None);
    }
}

//! Private local copies of Remote Host files that the explorer opens in their default app
//! (Lesani/xshell#46, D1). The sync-back state machine is TypeScript (`src/files/syncBack.ts`);
//! this module owns the copies on disk.
//!
//! Layout, one folder per (Host, path) below the canonical root `<app cache>/open/`:
//!
//! ```text
//! open/<key>/meta.json      recovery metadata, written by the webview (atomic: temp + rename)
//! open/<key>/f/<basename>   the payload the default app edits
//! open/<key>/snap/<rand>    a snapshot of the payload while it uploads
//! open/<key>/dl/<basename>  a download in progress, installed over the payload
//! ```
//!
//! `<key>` is the first 20 hex digits of sha256(host + "\0" + remote path). Every folder is
//! private (0700 on unix; an owner-only DACL on Windows) and created here, never through a
//! symlink, junction or other reparse point. Every command that takes a path refuses one
//! outside the root or through a symlink.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The root folder's name inside the app cache directory.
pub const OPEN_DIR: &str = "open";
const META: &str = "meta.json";
const PAYLOAD_DIR: &str = "f";
const SNAP_DIR: &str = "snap";
const INCOMING_DIR: &str = "dl";
/// A copy whose last activity is older than this is removed at startup, unless pending.
pub const OPEN_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Snapshots and downloads left over by a crash are removed at startup after this.
pub const SCRATCH_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// `open_copy_install` refused: the payload is not the version the caller expected. The
/// webview compares against this prefix.
pub const LOCAL_CHANGED: &str = "local copy changed";

/// The paths of one copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyPaths {
    pub dir: String,
    pub payload: String,
    pub incoming: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyStat {
    pub mtime_ms: u64,
    pub size: u64,
}

/// A snapshot of the payload: its path, `sha256:<hex>` and the payload's size and mtime
/// when the snapshot was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub path: String,
    pub version: String,
    pub size: u64,
    pub mtime_ms: u64,
}

fn err(p: &Path, what: impl std::fmt::Display) -> String {
    format!("{}: {what}", p.display())
}

/// The folder name of the copy of `remote_path` on `host`.
pub fn copy_key(host: &str, remote_path: &str) -> String {
    let mut h = Sha256::new();
    h.update(host.as_bytes());
    h.update([0u8]);
    h.update(remote_path.as_bytes());
    let mut s = String::with_capacity(20);
    for b in &h.finalize()[..10] {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The local file name of a Remote Host path: its last component, by the path's own rules
/// (a drive-letter or UNC path is Windows: `\` and `/` separate; any other path is POSIX:
/// only `/` does). Refused when it is empty, `.` or `..`, or holds a NUL or a separator of
/// this machine.
pub fn copy_basename(remote_path: &str) -> Result<String, String> {
    let b = remote_path.as_bytes();
    let windows = (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || remote_path.starts_with("\\\\");
    let name = if windows {
        remote_path.rsplit(['/', '\\']).next().unwrap_or("")
    } else {
        remote_path.rsplit('/').next().unwrap_or("")
    };
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('\0')
        || (cfg!(windows) && name.contains(['\\', ':', '*', '?', '"', '<', '>', '|']));
    if bad {
        return Err(format!("cannot open {remote_path:?} as a local copy"));
    }
    Ok(name.to_string())
}

fn is_link(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    m.file_type().is_symlink()
}

/// Make `dir` an existing private folder: created new (its parent must exist), or an existing
/// real folder of this user, made private again. A symlink or reparse point is refused.
fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    match fs::symlink_metadata(dir) {
        Ok(m) => check_existing_dir(dir, &m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_private_dir(dir),
        Err(e) => Err(err(dir, e)),
    }
}

fn check_existing_dir(dir: &Path, m: &fs::Metadata) -> Result<(), String> {
    if is_link(m) {
        return Err(err(dir, "is a symlink"));
    }
    if !m.is_dir() {
        return Err(err(dir, "is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if m.uid() != unsafe { libc::geteuid() } {
            return Err(err(dir, "is owned by another user"));
        }
        if m.mode() & 0o777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| err(dir, e))?;
        }
    }
    #[cfg(windows)]
    xshell_core::private_fs::ensure_dir(dir).map_err(|e| err(dir, e))?;
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        // Created meanwhile: check what is there now.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let m = fs::symlink_metadata(dir).map_err(|e| err(dir, e))?;
            check_existing_dir(dir, &m)
        }
        Err(e) => Err(err(dir, e)),
    }
}

/// Created with its owner-only DACL in one step; one created meanwhile is checked the same
/// way (a reparse point or another owner is refused).
#[cfg(windows)]
fn create_private_dir(dir: &Path) -> Result<(), String> {
    xshell_core::private_fs::ensure_dir(dir).map_err(|e| err(dir, e))
}

/// The canonical root `<cache>/open`, created private.
pub fn root_in(cache: &Path) -> Result<PathBuf, String> {
    area_in(cache, OPEN_DIR)
}

/// The canonical private folder `<cache>/<name>`.
fn area_in(cache: &Path, name: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(cache).map_err(|e| err(cache, e))?;
    // dunce: a plain `C:\...` path on Windows, not a `\\?\` one the default apps may not take.
    let cache = dunce::canonicalize(cache).map_err(|e| err(cache, e))?;
    let root = cache.join(name);
    ensure_private_dir(&root)?;
    Ok(root)
}

// ── The drag area ─────────────────────────────────────────────────────
//
// A Remote Host file dragged out to the OS (D3) is first downloaded to
// `<app cache>/drag/<key>/<basename>`, `<key>` as for open copies. The folders are private
// and created here like the open copies' (no symlinks or reparse points); the file itself
// keeps the default mode, as the file manager copies it with its permissions. The webview
// keeps a prepared copy for 10 minutes and then releases it; the startup sweep removes
// what is older than [`SCRATCH_MAX_AGE`].

/// The drag area's folder name inside the app cache directory.
pub const DRAG_DIR: &str = "drag";

/// The canonical drag area `<cache>/drag`, created private.
pub fn drag_root_in(cache: &Path) -> Result<PathBuf, String> {
    area_in(cache, DRAG_DIR)
}

/// The local path a drag of `remote_path` on `host` is prepared at; its folder is created.
pub fn drag_copy_path_in(root: &Path, host: &str, remote_path: &str) -> Result<String, String> {
    let name = copy_basename(remote_path)?;
    let dir = root.join(copy_key(host, remote_path));
    ensure_private_dir(&dir)?;
    Ok(dir.join(name).to_string_lossy().into_owned())
}

/// Remove a prepared drag copy: `p` is its folder or the file in it.
pub fn drag_release(root: &Path, p: &Path) -> Result<(), String> {
    let parts = inside(root, p)?;
    let dir = match parts.len() {
        1 => p,
        2 => p.parent().unwrap_or(p),
        _ => return Err(err(p, "is not a drag copy")),
    };
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(dir, e)),
    }
}

/// Startup cleanup of the drag area: remove the copies idle for [`SCRATCH_MAX_AGE`].
pub fn sweep_drag(root: &Path, now: SystemTime) {
    for dir in copy_dirs(root) {
        let mut youngest = fs::symlink_metadata(&dir).map_or(Duration::ZERO, |m| age(&m, now));
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Ok(m) = fs::symlink_metadata(e.path()) {
                    youngest = youngest.min(age(&m, now));
                }
            }
        }
        if youngest > SCRATCH_MAX_AGE {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

/// The copy folder of `remote_path` on `host` and its sub-folders, created private.
pub fn copy_paths(root: &Path, host: &str, remote_path: &str) -> Result<CopyPaths, String> {
    let name = copy_basename(remote_path)?;
    let dir = root.join(copy_key(host, remote_path));
    ensure_private_dir(&dir)?;
    for sub in [PAYLOAD_DIR, SNAP_DIR, INCOMING_DIR] {
        ensure_private_dir(&dir.join(sub))?;
    }
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    Ok(CopyPaths {
        payload: s(dir.join(PAYLOAD_DIR).join(&name)),
        incoming: s(dir.join(INCOMING_DIR).join(&name)),
        dir: s(dir),
    })
}

/// `p`'s components below `root`, after checking that `p` is inside it and that no existing
/// component (the last one included) is a symlink.
fn inside<'a>(root: &Path, p: &'a Path) -> Result<Vec<&'a OsStr>, String> {
    if !p.is_absolute() {
        return Err(err(p, "is not absolute"));
    }
    let rel = p
        .strip_prefix(root)
        .map_err(|_| err(p, "is outside the open-copies folder"))?;
    let mut parts = Vec::new();
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(err(p, "is not a plain path"));
        };
        cur.push(name);
        match fs::symlink_metadata(&cur) {
            Ok(m) if is_link(&m) => return Err(err(&cur, "is a symlink")),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(&cur, e)),
        }
        parts.push(name);
    }
    if parts.is_empty() {
        return Err(err(p, "is the open-copies folder itself"));
    }
    Ok(parts)
}

/// `p` must be `<root>/<key>/<sub>/<name>`.
fn file_in(root: &Path, p: &Path, sub: &str) -> Result<(), String> {
    let parts = inside(root, p)?;
    if parts.len() != 3 || parts[1] != OsStr::new(sub) {
        return Err(err(p, format!("is not a file in {sub}/ of a copy")));
    }
    Ok(())
}

fn mtime_ms(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

/// Size and mtime of a copy's payload; `None` while it does not exist (an editor's
/// rename-save, or deleted).
pub fn stat(root: &Path, p: &Path) -> Result<Option<CopyStat>, String> {
    file_in(root, p, PAYLOAD_DIR)?;
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_file() => Ok(Some(CopyStat {
            mtime_ms: mtime_ms(&m),
            size: m.len(),
        })),
        Ok(_) => Err(err(p, "is not a regular file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(p, e)),
    }
}

fn session_dir(root: &Path, dir: &Path) -> Result<(), String> {
    let parts = inside(root, dir)?;
    if parts.len() != 1 {
        return Err(err(dir, "is not a copy folder"));
    }
    Ok(())
}

/// Create a new owner-only file (never over an existing one or through a symlink).
fn create_new_private(p: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(p)
}

/// Read (`meta` None) or replace a copy's `meta.json`. A replace is atomic.
pub fn meta(root: &Path, dir: &Path, meta: Option<&Value>) -> Result<Option<Value>, String> {
    session_dir(root, dir)?;
    let path = dir.join(META);
    let Some(v) = meta else {
        return read_meta(&path);
    };
    let tmp = dir.join(format!(".{META}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let write = || -> io::Result<()> {
        let mut f = create_new_private(&tmp)?;
        f.write_all(&serde_json::to_vec(v).map_err(io::Error::other)?)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        err(&path, e)
    })?;
    Ok(Some(v.clone()))
}

fn read_meta(path: &Path) -> Result<Option<Value>, String> {
    match fs::symlink_metadata(path) {
        Ok(m) if is_link(&m) || !m.is_file() => return Err(err(path, "is not a regular file")),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(err(path, e)),
    }
    let bytes = fs::read(path).map_err(|e| err(path, e))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| err(path, e))
}

/// Copy the payload to a new private file in `snap/`, hashing it on the way. Size and mtime
/// are the payload's as opened, so a later save is told apart.
pub fn snapshot(root: &Path, payload: &Path) -> Result<Snapshot, String> {
    file_in(root, payload, PAYLOAD_DIR)?;
    let mut src = fs::File::open(payload).map_err(|e| err(payload, e))?;
    let m = src.metadata().map_err(|e| err(payload, e))?;
    if !m.is_file() {
        return Err(err(payload, "is not a regular file"));
    }
    let dir = payload
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| err(payload, "has no copy folder"))?;
    let snap_dir = dir.join(SNAP_DIR);
    if !is_real_dir(&snap_dir) {
        return Err(err(&snap_dir, "is not a private directory"));
    }
    let snap = snap_dir.join(uuid::Uuid::new_v4().simple().to_string());
    let mut copy = || -> io::Result<String> {
        let mut dst = create_new_private(&snap)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            h.update(&buf[..n]);
            dst.write_all(&buf[..n])?;
        }
        dst.sync_all()?;
        let mut s = String::from("sha256:");
        for b in h.finalize() {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        Ok(s)
    };
    let version = copy().map_err(|e| {
        let _ = fs::remove_file(&snap);
        err(payload, e)
    })?;
    Ok(Snapshot {
        path: snap.to_string_lossy().into_owned(),
        version,
        size: m.len(),
        mtime_ms: mtime_ms(&m),
    })
}

/// What the caller expects at the payload path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect<'a> {
    /// Nothing.
    Absent,
    /// A file with this content (`sha256:<hex>`), or nothing (a missing copy has nothing to
    /// lose).
    Version(&'a str),
}

impl<'a> Expect<'a> {
    /// `"absent"`, or a version.
    pub fn parse(s: &'a str) -> Self {
        if s == "absent" {
            Expect::Absent
        } else {
            Expect::Version(s)
        }
    }
}

/// Which file a path names, and its size and mtime: two equal observations are the same file
/// with (as far as the filesystem tells) the same content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Obs {
    id: (u64, u64),
    size: u64,
    mtime_ms: u64,
}

#[cfg(unix)]
fn obs_of_handle(f: &fs::File) -> io::Result<Obs> {
    use std::os::unix::fs::MetadataExt;
    let m = f.metadata()?;
    Ok(Obs {
        id: (m.dev(), m.ino()),
        size: m.len(),
        mtime_ms: mtime_ms(&m),
    })
}

#[cfg(windows)]
fn obs_of_handle(f: &fs::File) -> io::Result<Obs> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let m = f.metadata()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Obs {
        id: (
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        ),
        size: m.len(),
        mtime_ms: mtime_ms(&m),
    })
}

/// The file at `p` now, never through a symlink or reparse point. `None`: nothing there.
fn obs_at(p: &Path) -> io::Result<Option<Obs>> {
    let link = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if is_link(&link) || !link.is_file() {
        return Err(io::Error::other("is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(Obs {
            id: (link.dev(), link.ino()),
            size: link.len(),
            mtime_ms: mtime_ms(&link),
        }))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let f = match fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(p)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        obs_of_handle(&f).map(Some)
    }
}

/// `sha256:<hex>` of what `f` reads from its current position. `between` runs after each
/// chunk (a test seam).
fn hash_reader(f: &mut impl Read, between: &mut dyn FnMut()) -> io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                h.update(&buf[..n]);
                between();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let mut s = String::from("sha256:");
    for b in h.finalize() {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// The payload as it is now: `None` when there is none, else its observation and content
/// hash. The file must not change while it is hashed.
fn observe_payload(payload: &Path) -> Result<Option<(Obs, String)>, String> {
    observe_payload_with(payload, &mut || {})
}

/// [`observe_payload`] with a hook between hashed chunks (tests).
fn observe_payload_with(
    payload: &Path,
    mid_hash: &mut dyn FnMut(),
) -> Result<Option<(Obs, String)>, String> {
    let mut f = match fs::File::open(payload) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(err(payload, e)),
    };
    let before = obs_of_handle(&f).map_err(|e| err(payload, e))?;
    if !f.metadata().map_err(|e| err(payload, e))?.is_file() {
        return Err(err(payload, "is not a regular file"));
    }
    let version = hash_reader(&mut f, mid_hash).map_err(|e| err(payload, e))?;
    let after = obs_of_handle(&f).map_err(|e| err(payload, e))?;
    if before != after {
        return Err(format!("{LOCAL_CHANGED}: {}", payload.display()));
    }
    // The path must still name the file that was hashed.
    if obs_at(payload).map_err(|e| err(payload, e))? != Some(before) {
        return Err(format!("{LOCAL_CHANGED}: {}", payload.display()));
    }
    Ok(Some((before, version)))
}

fn expected(expect: Expect<'_>, current: Option<&(Obs, String)>) -> bool {
    match (expect, current) {
        (_, None) => true,
        (Expect::Version(want), Some((_, have))) => want == have,
        (Expect::Absent, Some(_)) => false,
    }
}

/// Move a finished download (`dl/<name>`) over the payload (`f/<name>`) of the same copy,
/// but only while the payload is what the caller last saw ([`Expect`]). Right before the
/// rename the payload must still be the very file that was hashed, unchanged (same file id,
/// size and mtime); otherwise [`LOCAL_CHANGED`], and the download is discarded. Returns the
/// size and mtime of the installed file, i.e. of the downloaded content.
pub fn install(
    root: &Path,
    incoming: &Path,
    payload: &Path,
    expect: Expect<'_>,
) -> Result<CopyStat, String> {
    install_with(root, incoming, payload, expect, &mut || {}, &mut || {})
}

/// [`install`] with hooks inside the payload's hash and between the check and the rename
/// (tests).
fn install_with(
    root: &Path,
    incoming: &Path,
    payload: &Path,
    expect: Expect<'_>,
    mid_hash: &mut dyn FnMut(),
    before_rename: &mut dyn FnMut(),
) -> Result<CopyStat, String> {
    file_in(root, incoming, INCOMING_DIR)?;
    file_in(root, payload, PAYLOAD_DIR)?;
    let same_copy = incoming.parent().and_then(Path::parent)
        == payload.parent().and_then(Path::parent)
        && incoming.file_name() == payload.file_name();
    if !same_copy {
        return Err(err(incoming, "belongs to another copy"));
    }
    let refuse = || {
        let _ = fs::remove_file(incoming);
        format!("{LOCAL_CHANGED}: {}", payload.display())
    };
    let current = match observe_payload_with(payload, mid_hash) {
        Ok(c) => c,
        Err(e) if e.starts_with(LOCAL_CHANGED) => return Err(refuse()),
        Err(e) => return Err(e),
    };
    if !expected(expect, current.as_ref()) {
        return Err(refuse());
    }
    let new = fs::File::open(incoming).map_err(|e| err(incoming, e))?;
    let installed = obs_of_handle(&new).map_err(|e| err(incoming, e))?;
    before_rename();
    // Last look: the same file as hashed, or still nothing.
    let now = obs_at(payload).map_err(|e| err(payload, e))?;
    if now != current.as_ref().map(|c| c.0) {
        return Err(refuse());
    }
    fs::rename(incoming, payload).map_err(|e| {
        let _ = fs::remove_file(incoming);
        err(payload, e)
    })?;
    Ok(CopyStat {
        mtime_ms: installed.mtime_ms,
        size: installed.size,
    })
}

/// Remove a copy folder, or one snapshot or download file. Missing is fine. With a guard
/// `(payload, expect)`, a copy folder is removed only while its payload is what the caller
/// last saw, else [`LOCAL_CHANGED`]: closing an explorer never deletes edits it has not
/// seen. A missing payload counts as a change unless [`Expect::Absent`] (it may be an
/// editor's rename-save in progress).
pub fn release(root: &Path, p: &Path, guard: Option<(&Path, Expect<'_>)>) -> Result<(), String> {
    release_with(root, p, guard, &mut || {})
}

/// [`release`] with a hook inside the payload's hash (tests).
fn release_with(
    root: &Path,
    p: &Path,
    guard: Option<(&Path, Expect<'_>)>,
    mid_hash: &mut dyn FnMut(),
) -> Result<(), String> {
    let parts = inside(root, p)?;
    let r = match parts.len() {
        1 => {
            if let Some((payload, expect)) = guard {
                check_payload(root, p, payload, expect, mid_hash)?;
            }
            fs::remove_dir_all(p)
        }
        3 if parts[1] == OsStr::new(SNAP_DIR) || parts[1] == OsStr::new(INCOMING_DIR) => {
            fs::remove_file(p)
        }
        _ => return Err(err(p, "is not a copy folder, snapshot or download")),
    };
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(p, e)),
    }
}

/// `payload` is the payload of copy `dir` and matches `expect` (see [`release`]).
fn check_payload(
    root: &Path,
    dir: &Path,
    payload: &Path,
    expect: Expect<'_>,
    mid_hash: &mut dyn FnMut(),
) -> Result<(), String> {
    file_in(root, payload, PAYLOAD_DIR)?;
    if payload.parent().and_then(Path::parent) != Some(dir) {
        return Err(err(payload, "belongs to another copy"));
    }
    let ok = match (expect, observe_payload_with(payload, mid_hash)?) {
        (Expect::Absent, None) => true,
        (Expect::Version(want), Some((_, have))) => want == have,
        _ => false,
    };
    if !ok {
        return Err(format!("{LOCAL_CHANGED}: {}", payload.display()));
    }
    Ok(())
}

/// A real directory: not a symlink, junction or other reparse point.
fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.is_dir() && !is_link(&m))
        .unwrap_or(false)
}

fn copy_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(root) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| {
            fs::symlink_metadata(e.path())
                .map(|m| m.is_dir() && !is_link(&m))
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect()
}

fn is_pending(meta: &Option<Value>) -> bool {
    meta.as_ref()
        .and_then(|m| m.get("pending"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether a copy may hold changes that were not uploaded. Its meta is not trusted alone (a
/// failed write may have left "clean" behind): pending, or a payload whose content is not
/// the meta's synced version, or one that cannot be checked, or a payload with no readable
/// meta.
fn needs_recovery(dir: &Path, meta: &Option<Value>) -> bool {
    if is_pending(meta) {
        return true;
    }
    let f_dir = dir.join(PAYLOAD_DIR);
    let Some(m) = meta else {
        // No meta: anything in f/ is unaccounted for.
        return is_real_dir(&f_dir) && fs::read_dir(&f_dir).is_ok_and(|mut rd| rd.next().is_some());
    };
    let Some(name) = m
        .get("remotePath")
        .and_then(Value::as_str)
        .and_then(|p| copy_basename(p).ok())
    else {
        return true;
    };
    if !is_real_dir(&f_dir) {
        return fs::symlink_metadata(&f_dir).is_ok();
    }
    match observe_payload(&f_dir.join(name)) {
        Ok(None) => false,
        Ok(Some((_, have))) => m.get("localVersion").and_then(Value::as_str) != Some(have.as_str()),
        Err(_) => true,
    }
}

/// The metadata of every copy that may hold changes that were not uploaded (see
/// [`needs_recovery`]), each with its `dir`, and `pending: true`. A copy that is pending only
/// because its payload differs from its meta also gets `untrusted: true`.
pub fn pending(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for dir in copy_dirs(root) {
        let m = read_meta(&dir.join(META)).ok().flatten();
        if !needs_recovery(&dir, &m) {
            continue;
        }
        let untrusted = !is_pending(&m);
        if let Some(Value::Object(mut o)) = m {
            if o.get("host").and_then(Value::as_str).is_none()
                || o.get("remotePath").and_then(Value::as_str).is_none()
            {
                continue;
            }
            o.insert("pending".into(), Value::Bool(true));
            if untrusted {
                o.insert("untrusted".into(), Value::Bool(true));
            }
            o.insert(
                "dir".into(),
                Value::String(dir.to_string_lossy().into_owned()),
            );
            out.push(Value::Object(o));
        }
    }
    out
}

fn age(m: &fs::Metadata, now: SystemTime) -> Duration {
    m.modified()
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .unwrap_or_default()
}

/// The youngest age among a copy folder, its metadata and its payload.
fn last_activity(dir: &Path, now: SystemTime) -> Duration {
    let mut ages = Vec::new();
    for p in [dir.to_path_buf(), dir.join(META)] {
        if let Ok(m) = fs::symlink_metadata(&p) {
            ages.push(age(&m, now));
        }
    }
    let f_dir = dir.join(PAYLOAD_DIR);
    if let Ok(rd) = fs::read_dir(&f_dir).map_err(drop).and_then(|rd| {
        if is_real_dir(&f_dir) {
            Ok(rd)
        } else {
            Err(())
        }
    }) {
        for e in rd.flatten() {
            if let Ok(m) = fs::symlink_metadata(e.path()) {
                ages.push(age(&m, now));
            }
        }
    }
    ages.into_iter().min().unwrap_or_default()
}

/// Startup cleanup: remove copies idle for [`OPEN_MAX_AGE`] that hold no changes (see
/// [`needs_recovery`]), and snapshots or downloads older than [`SCRATCH_MAX_AGE`]. A copy
/// that may hold changes is never removed.
pub fn sweep(root: &Path, now: SystemTime) {
    for dir in copy_dirs(root) {
        let m = read_meta(&dir.join(META)).ok().flatten();
        if last_activity(&dir, now) > OPEN_MAX_AGE && !needs_recovery(&dir, &m) {
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        for sub in [SNAP_DIR, INCOMING_DIR] {
            let sub = dir.join(sub);
            // Never into a linked folder: only what is really ours is removed.
            if !is_real_dir(&sub) {
                continue;
            }
            let Ok(rd) = fs::read_dir(&sub) else {
                continue;
            };
            for e in rd.flatten() {
                if let Ok(m) = fs::symlink_metadata(e.path()) {
                    if !m.is_dir() && age(&m, now) > SCRATCH_MAX_AGE {
                        let _ = fs::remove_file(e.path());
                    }
                }
            }
        }
    }
}

/// Check a path the user asked to open in its default app: absolute and existing.
pub fn check_openable(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(err(p, "is not absolute"));
    }
    fs::metadata(p).map_err(|e| err(p, e))?;
    Ok(p.to_path_buf())
}

// ── Tauri commands ────────────────────────────────────────────────────

fn root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager as _;
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    root_in(&cache)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

/// Open a file or folder of this machine in its default app (ShellExecute on Windows, `open`
/// on macOS, xdg-open / gio on Linux).
#[tauri::command]
pub async fn open_local_path(path: String) -> Result<(), String> {
    blocking(move || {
        let p = check_openable(&path)?;
        open::that_detached(&p).map_err(|e| format!("Couldn't open {}: {e}", p.display()))
    })
    .await
}

#[tauri::command]
pub async fn open_copy_path(
    app: tauri::AppHandle,
    host: String,
    remote_path: String,
) -> Result<CopyPaths, String> {
    blocking(move || copy_paths(&root(&app)?, &host, &remote_path)).await
}

#[tauri::command]
pub async fn open_copy_stat(
    app: tauri::AppHandle,
    path: String,
) -> Result<Option<CopyStat>, String> {
    blocking(move || stat(&root(&app)?, Path::new(&path))).await
}

#[tauri::command]
pub async fn open_copy_meta(
    app: tauri::AppHandle,
    dir: String,
    meta: Option<Value>,
) -> Result<Option<Value>, String> {
    blocking(move || self::meta(&root(&app)?, Path::new(&dir), meta.as_ref())).await
}

#[tauri::command]
pub async fn open_copy_snapshot(
    app: tauri::AppHandle,
    payload: String,
) -> Result<Snapshot, String> {
    blocking(move || snapshot(&root(&app)?, Path::new(&payload))).await
}

#[tauri::command]
pub async fn open_copy_install(
    app: tauri::AppHandle,
    incoming: String,
    payload: String,
    expect: String,
) -> Result<CopyStat, String> {
    blocking(move || {
        install(
            &root(&app)?,
            Path::new(&incoming),
            Path::new(&payload),
            Expect::parse(&expect),
        )
    })
    .await
}

#[tauri::command]
pub async fn open_copy_release(
    app: tauri::AppHandle,
    path: String,
    payload: Option<String>,
    expect: Option<String>,
) -> Result<(), String> {
    blocking(move || {
        let guard = match (&payload, &expect) {
            (Some(p), Some(e)) => Some((Path::new(p.as_str()), Expect::parse(e))),
            (None, None) => None,
            _ => return Err("a release guard needs both payload and expect".into()),
        };
        release(&root(&app)?, Path::new(&path), guard)
    })
    .await
}

#[tauri::command]
pub async fn open_copies_pending(app: tauri::AppHandle) -> Result<Vec<Value>, String> {
    blocking(move || Ok(pending(&root(&app)?))).await
}

fn drag_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager as _;
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    drag_root_in(&cache)
}

/// Where a drag of a Remote Host file is prepared (see the drag area above).
#[tauri::command]
pub async fn drag_copy_path(
    app: tauri::AppHandle,
    host: String,
    remote_path: String,
) -> Result<String, String> {
    blocking(move || drag_copy_path_in(&drag_root(&app)?, &host, &remote_path)).await
}

#[tauri::command]
pub async fn drag_copy_release(app: tauri::AppHandle, path: String) -> Result<(), String> {
    blocking(move || drag_release(&drag_root(&app)?, Path::new(&path))).await
}

/// Run [`sweep`] on the app's copies and [`sweep_drag`] on the drag area, off the startup
/// path.
pub fn sweep_in_background(app: &tauri::AppHandle) {
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("open-copies-sweep".into())
        .spawn(move || {
            let now = SystemTime::now();
            if let Ok(r) = root(&app) {
                sweep(&r, now);
            }
            if let Ok(r) = drag_root(&app) {
                sweep_drag(&r, now);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fx {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = root_in(&tmp.path().join("cache")).unwrap();
        Fx { _tmp: tmp, root }
    }

    fn set_mtime(p: &Path, t: SystemTime) {
        fs::File::options()
            .write(true)
            .open(p)
            .or_else(|_| fs::File::open(p))
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn sha(s: &str) -> String {
        let mut out = String::from("sha256:");
        for b in Sha256::digest(s.as_bytes()) {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    #[test]
    fn copy_path_is_stable_and_distinct_per_host_and_path() {
        let f = fx();
        let a = copy_paths(&f.root, "h_aaaaaaaa", "/srv/a.txt").unwrap();
        assert_eq!(a, copy_paths(&f.root, "h_aaaaaaaa", "/srv/a.txt").unwrap());
        let b = copy_paths(&f.root, "h_bbbbbbbb", "/srv/a.txt").unwrap();
        let c = copy_paths(&f.root, "h_aaaaaaaa", "/srv/x/a.txt").unwrap();
        assert_ne!(a.dir, b.dir);
        assert_ne!(a.dir, c.dir);
        assert!(a.payload.ends_with("a.txt"));
        assert_eq!(
            Path::new(&a.payload).parent().unwrap(),
            Path::new(&a.dir).join("f")
        );
        assert_eq!(
            Path::new(&a.incoming).parent().unwrap(),
            Path::new(&a.dir).join("dl")
        );
        // host "a" + path "b\0c" must not collide with host "a\0b" + path "c".
        assert_ne!(copy_key("a", "/b"), copy_key("a/", "b"));
        assert_eq!(copy_key("h", "/p").len(), 20);
    }

    #[cfg(unix)]
    #[test]
    fn copy_dirs_are_private_0700() {
        use std::os::unix::fs::PermissionsExt;
        let f = fx();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&f.root), 0o700);
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        for sub in ["", "f", "snap", "dl"] {
            assert_eq!(mode(&Path::new(&a.dir).join(sub)), 0o700, "{sub}");
        }
        // A loosened folder is made private again.
        fs::set_permissions(&a.dir, fs::Permissions::from_mode(0o755)).unwrap();
        copy_paths(&f.root, "h", "/a.txt").unwrap();
        assert_eq!(mode(Path::new(&a.dir)), 0o700);
    }

    #[test]
    fn invalid_basenames_are_refused() {
        let f = fx();
        for p in ["/srv/", "/srv/..", "/srv/.", "", "/srv/a\0b"] {
            assert!(copy_paths(&f.root, "h", p).is_err(), "{p:?}");
        }
        assert_eq!(copy_basename("/srv/a.txt").unwrap(), "a.txt");
        // A Windows Remote Host's path.
        assert_eq!(copy_basename("C:\\Users\\me\\a.txt").unwrap(), "a.txt");
        assert_eq!(copy_basename("\\\\srv\\share\\b.txt").unwrap(), "b.txt");
        assert!(copy_basename("C:\\Users\\me\\").is_err());
        // A backslash is part of a POSIX name (refused on a Windows Desktop, which cannot
        // store it).
        #[cfg(unix)]
        assert_eq!(copy_basename("/srv/a\\b.txt").unwrap(), "a\\b.txt");
        #[cfg(windows)]
        assert!(copy_basename("/srv/a:b").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_copy_dir_is_refused() {
        let f = fx();
        let elsewhere = f._tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let key = copy_key("h", "/a.txt");
        std::os::unix::fs::symlink(&elsewhere, f.root.join(&key)).unwrap();
        assert!(copy_paths(&f.root, "h", "/a.txt").is_err());
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
        // A symlinked sub-folder too.
        let key2 = copy_key("h", "/b.txt");
        fs::create_dir(f.root.join(&key2)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, f.root.join(&key2).join("f")).unwrap();
        assert!(copy_paths(&f.root, "h", "/b.txt").is_err());
        // And a symlinked root.
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        std::os::unix::fs::symlink(&elsewhere, cache.join(OPEN_DIR)).unwrap();
        assert!(root_in(&cache).is_err());
    }

    #[test]
    fn a_remote_file_named_meta_json_does_not_clash() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/etc/meta.json").unwrap();
        fs::write(&a.payload, "payload").unwrap();
        meta(&f.root, Path::new(&a.dir), Some(&json!({"pending": true}))).unwrap();
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "payload");
        assert_eq!(
            meta(&f.root, Path::new(&a.dir), None).unwrap(),
            Some(json!({"pending": true}))
        );
    }

    #[test]
    fn stat_refuses_outside_root_and_symlinks() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        assert_eq!(stat(&f.root, Path::new(&a.payload)).unwrap(), None);
        fs::write(&a.payload, "abc").unwrap();
        assert_eq!(
            stat(&f.root, Path::new(&a.payload)).unwrap().unwrap().size,
            3
        );
        let outside = f._tmp.path().join("outside.txt");
        fs::write(&outside, "x").unwrap();
        assert!(stat(&f.root, &outside).is_err());
        let dotdot = Path::new(&a.dir)
            .join("f")
            .join("..")
            .join("..")
            .join("..")
            .join("outside.txt");
        assert!(stat(&f.root, &dotdot).is_err());
        assert!(stat(&f.root, Path::new("relative/a.txt")).is_err());
        // Not the payload folder.
        assert!(stat(&f.root, &Path::new(&a.dir).join("meta.json")).is_err());
        #[cfg(unix)]
        {
            fs::remove_file(&a.payload).unwrap();
            std::os::unix::fs::symlink(&outside, &a.payload).unwrap();
            assert!(stat(&f.root, Path::new(&a.payload)).is_err());
            assert!(snapshot(&f.root, Path::new(&a.payload)).is_err());
        }
    }

    #[test]
    fn release_refuses_outside_root() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        let outside = f._tmp.path().join("keep");
        fs::create_dir(&outside).unwrap();
        assert!(release(&f.root, &outside, None).is_err());
        assert!(release(&f.root, &f.root, None).is_err());
        assert!(release(&f.root, Path::new(&a.payload), None).is_err());
        assert!(outside.exists());
        fs::write(&a.payload, "x").unwrap();
        release(&f.root, Path::new(&a.dir), None).unwrap();
        assert!(!Path::new(&a.dir).exists());
        release(&f.root, Path::new(&a.dir), None).unwrap();
    }

    #[test]
    fn snapshot_hashes_and_is_released() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        fs::write(&a.payload, "hello").unwrap();
        let s = snapshot(&f.root, Path::new(&a.payload)).unwrap();
        assert_eq!(s.version, sha("hello"));
        assert_eq!(s.size, 5);
        assert_eq!(fs::read_to_string(&s.path).unwrap(), "hello");
        assert_eq!(
            Path::new(&s.path).parent().unwrap(),
            Path::new(&a.dir).join("snap")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&s.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        release(&f.root, Path::new(&s.path), None).unwrap();
        assert!(!Path::new(&s.path).exists());
    }

    #[test]
    fn install_checks_the_payload_first() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        // First download: no payload expected.
        fs::write(&a.incoming, "v1").unwrap();
        let st = install(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Absent,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "v1");
        assert_eq!(stat(&f.root, Path::new(&a.payload)).unwrap(), Some(st));
        assert!(!Path::new(&a.incoming).exists());
        // Refresh with the expected content.
        fs::write(&a.incoming, "v2").unwrap();
        install(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Version(&sha("v1")),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "v2");
        // The payload was edited meanwhile: kept, the download discarded.
        fs::write(&a.payload, "local edit").unwrap();
        fs::write(&a.incoming, "v3").unwrap();
        let e = install(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Version(&sha("v2")),
        )
        .unwrap_err();
        assert!(e.starts_with(LOCAL_CHANGED), "{e}");
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "local edit");
        assert!(!Path::new(&a.incoming).exists());
        // "Download again" expects no payload: one that reappeared is kept.
        fs::write(&a.incoming, "v3").unwrap();
        assert!(install(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Absent
        )
        .is_err());
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "local edit");
        // Another copy's download is refused.
        let b = copy_paths(&f.root, "h", "/b.txt").unwrap();
        fs::write(&b.incoming, "b").unwrap();
        assert!(install(
            &f.root,
            Path::new(&b.incoming),
            Path::new(&a.payload),
            Expect::Absent
        )
        .is_err());
    }

    #[test]
    fn install_refuses_a_payload_replaced_after_it_was_hashed() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        fs::write(&a.payload, "v1").unwrap();
        // An editor's rename-save between the hash and the rename: same content even.
        fs::write(&a.incoming, "remote").unwrap();
        let other = Path::new(&a.dir).join("f").join(".tmp-save");
        let e = install_with(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Version(&sha("v1")),
            &mut || {},
            &mut || {
                fs::write(&other, "v2").unwrap();
                fs::rename(&other, &a.payload).unwrap();
            },
        )
        .unwrap_err();
        assert!(e.starts_with(LOCAL_CHANGED), "{e}");
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "v2");
        assert!(!Path::new(&a.incoming).exists());
        // An in-place write (new mtime) in the same window.
        fs::write(&a.incoming, "remote").unwrap();
        let e = install_with(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Version(&sha("v2")),
            &mut || {},
            &mut || {
                fs::write(&a.payload, "v3").unwrap();
                set_mtime(
                    Path::new(&a.payload),
                    SystemTime::now() + Duration::from_secs(5),
                );
            },
        )
        .unwrap_err();
        assert!(e.starts_with(LOCAL_CHANGED), "{e}");
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "v3");
        // A payload that appears while a first download installs.
        let b = copy_paths(&f.root, "h", "/b.txt").unwrap();
        fs::write(&b.incoming, "remote").unwrap();
        let e = install_with(
            &f.root,
            Path::new(&b.incoming),
            Path::new(&b.payload),
            Expect::Absent,
            &mut || {},
            &mut || fs::write(&b.payload, "new").unwrap(),
        )
        .unwrap_err();
        assert!(e.starts_with(LOCAL_CHANGED), "{e}");
        assert_eq!(fs::read_to_string(&b.payload).unwrap(), "new");
    }

    #[test]
    fn release_with_expect_keeps_unseen_edits() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        fs::write(&a.payload, "seen").unwrap();
        let dir = Path::new(&a.dir);
        let pl = Path::new(&a.payload);
        let e = release(&f.root, dir, Some((pl, Expect::Version(&sha("other"))))).unwrap_err();
        assert!(e.starts_with(LOCAL_CHANGED), "{e}");
        assert!(pl.exists());
        assert!(release(&f.root, dir, Some((pl, Expect::Absent))).is_err());
        // An editor's temp file next to it does not matter; only the payload does.
        fs::write(Path::new(&a.dir).join("f").join(".a.txt.swp"), "swap").unwrap();
        release(&f.root, dir, Some((pl, Expect::Version(&sha("seen"))))).unwrap();
        assert!(!dir.exists());
        // A missing payload is kept unless the caller saw it missing (a rename gap).
        let b = copy_paths(&f.root, "h", "/b.txt").unwrap();
        let (bd, bp) = (Path::new(&b.dir), Path::new(&b.payload));
        assert!(release(&f.root, bd, Some((bp, Expect::Version(&sha("x"))))).is_err());
        release(&f.root, bd, Some((bp, Expect::Absent))).unwrap();
        assert!(!bd.exists());
        // Another copy's payload is no guard.
        let c = copy_paths(&f.root, "h", "/c.txt").unwrap();
        assert!(release(&f.root, Path::new(&c.dir), Some((pl, Expect::Absent))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn linked_snap_and_dl_folders_are_never_used() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        fs::write(&a.payload, "x").unwrap();
        let elsewhere = f._tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let victim = elsewhere.join("victim");
        fs::write(&victim, "keep").unwrap();
        set_mtime(
            &victim,
            SystemTime::now() - SCRATCH_MAX_AGE - Duration::from_secs(60),
        );
        for sub in ["snap", "dl"] {
            let d = Path::new(&a.dir).join(sub);
            fs::remove_dir(&d).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &d).unwrap();
        }
        // A snapshot is not written through the link.
        assert!(snapshot(&f.root, Path::new(&a.payload)).is_err());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 1);
        // The sweep does not clean the link's target.
        sweep(&f.root, SystemTime::now());
        assert!(victim.exists());
        // Nor can a file be released or installed through it.
        assert!(release(
            &f.root,
            &Path::new(&a.dir).join("snap").join("victim"),
            None
        )
        .is_err());
        assert!(install(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Absent
        )
        .is_err());
        assert!(victim.exists());
        // copy_paths refuses the copy from now on.
        assert!(copy_paths(&f.root, "h", "/a.txt").is_err());
    }

    #[test]
    fn untrusted_leftovers_are_restored_and_never_swept() {
        let f = fx();
        let now = SystemTime::now();
        let old = now - OPEN_MAX_AGE - Duration::from_secs(60);
        let age_all = |c: &CopyPaths| {
            for p in [
                c.payload.clone(),
                format!("{}/meta.json", c.dir),
                c.dir.clone(),
            ] {
                if Path::new(&p).exists() {
                    set_mtime(Path::new(&p), old);
                }
            }
        };
        // A failed "pending" write left a clean meta behind an edited payload.
        let edited = copy_paths(&f.root, "h", "/edited.txt").unwrap();
        fs::write(&edited.payload, "edited after the sync").unwrap();
        let clean = json!({"pending": false, "host": "h", "remotePath": "/edited.txt", "localVersion": sha("synced")});
        meta(&f.root, Path::new(&edited.dir), Some(&clean)).unwrap();
        // A truly clean copy.
        let synced = copy_paths(&f.root, "h", "/synced.txt").unwrap();
        fs::write(&synced.payload, "synced").unwrap();
        let ok = json!({"pending": false, "host": "h", "remotePath": "/synced.txt", "localVersion": sha("synced")});
        meta(&f.root, Path::new(&synced.dir), Some(&ok)).unwrap();
        // A payload whose meta was never written.
        let bare = copy_paths(&f.root, "h", "/bare.txt").unwrap();
        fs::write(&bare.payload, "?").unwrap();
        for c in [&edited, &synced, &bare] {
            age_all(c);
        }

        let p = pending(&f.root);
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0]["remotePath"], "/edited.txt");
        assert_eq!(p[0]["pending"], true);
        assert_eq!(p[0]["untrusted"], true);
        assert_eq!(p[0]["dir"], edited.dir);

        sweep(&f.root, now);
        assert!(Path::new(&edited.payload).exists());
        assert!(Path::new(&bare.payload).exists());
        assert!(!Path::new(&synced.dir).exists());
    }

    #[test]
    fn a_payload_rewritten_during_the_release_hash_is_kept() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        let n = 200 * 1024;
        fs::write(&a.payload, vec![b'a'; n]).unwrap();
        // What a hash torn by the rewrite after the first 64 KiB chunk reads.
        let mut torn = vec![b'a'; 64 * 1024];
        torn.extend(vec![b'b'; n - 64 * 1024]);
        let mut h = Sha256::new();
        h.update(&torn);
        let torn_version = format!(
            "sha256:{}",
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let mut once = true;
        let e = release_with(
            &f.root,
            Path::new(&a.dir),
            Some((Path::new(&a.payload), Expect::Version(&torn_version))),
            &mut || {
                if std::mem::take(&mut once) {
                    // In place: same file, same size, a new mtime.
                    fs::write(&a.payload, vec![b'b'; n]).unwrap();
                    set_mtime(
                        Path::new(&a.payload),
                        SystemTime::now() + Duration::from_secs(5),
                    );
                }
            },
        )
        .unwrap_err();
        assert!(e.starts_with(LOCAL_CHANGED), "{e}");
        assert_eq!(fs::read(&a.payload).unwrap(), vec![b'b'; n]);
    }

    #[test]
    fn a_payload_replaced_during_the_install_hash_is_kept() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        let n = 200 * 1024;
        fs::write(&a.payload, vec![b'a'; n]).unwrap();
        fs::write(&a.incoming, "remote").unwrap();
        let other = Path::new(&a.dir).join("f").join(".save");
        let mut once = true;
        let e = install_with(
            &f.root,
            Path::new(&a.incoming),
            Path::new(&a.payload),
            Expect::Version(&sha(&"a".repeat(n))),
            &mut || {
                if std::mem::take(&mut once) {
                    // An editor's rename-save while the old file is being hashed.
                    fs::write(&other, "saved").unwrap();
                    fs::rename(&other, &a.payload).unwrap();
                }
            },
            &mut || {},
        )
        .unwrap_err();
        assert!(e.starts_with(LOCAL_CHANGED), "{e}");
        assert_eq!(fs::read_to_string(&a.payload).unwrap(), "saved");
        assert!(!Path::new(&a.incoming).exists());
    }

    #[test]
    fn meta_round_trip_is_atomic() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        let dir = Path::new(&a.dir);
        assert_eq!(meta(&f.root, dir, None).unwrap(), None);
        meta(&f.root, dir, Some(&json!({"pending": false, "n": 1}))).unwrap();
        meta(&f.root, dir, Some(&json!({"pending": true, "n": 2}))).unwrap();
        assert_eq!(meta(&f.root, dir, None).unwrap().unwrap()["n"], 2);
        let leftovers: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        assert!(meta(&f.root, &Path::new(&a.dir).join("f"), None).is_err());
    }

    #[test]
    fn pending_lists_meta_with_pending_true() {
        let f = fx();
        let a = copy_paths(&f.root, "h", "/a.txt").unwrap();
        let b = copy_paths(&f.root, "h", "/b.txt").unwrap();
        copy_paths(&f.root, "h", "/c.txt").unwrap();
        meta(
            &f.root,
            Path::new(&a.dir),
            Some(&json!({"pending": true, "host": "h", "remotePath": "/a.txt"})),
        )
        .unwrap();
        meta(&f.root, Path::new(&b.dir), Some(&json!({"pending": false}))).unwrap();
        let p = pending(&f.root);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0]["remotePath"], "/a.txt");
        assert_eq!(p[0]["dir"], a.dir);
    }

    #[test]
    fn sweep_removes_old_and_keeps_pending_and_fresh() {
        let f = fx();
        let now = SystemTime::now();
        let old = now - OPEN_MAX_AGE - Duration::from_secs(60);
        let make = |path: &str, pending: bool, t: SystemTime| {
            let c = copy_paths(&f.root, "h", path).unwrap();
            fs::write(&c.payload, "x").unwrap();
            meta(
                &f.root,
                Path::new(&c.dir),
                Some(&json!({"pending": pending, "host": "h", "remotePath": path, "localVersion": sha("x")})),
            )
            .unwrap();
            for p in [
                c.payload.as_str(),
                &format!("{}/meta.json", c.dir),
                c.dir.as_str(),
            ] {
                set_mtime(Path::new(p), t);
            }
            c
        };
        let stale = make("/old.txt", false, old);
        let kept = make("/pending.txt", true, old);
        let fresh = make("/fresh.txt", false, now);
        let snap = Path::new(&fresh.dir).join("snap").join("leftover");
        fs::write(&snap, "s").unwrap();
        set_mtime(&snap, now - SCRATCH_MAX_AGE - Duration::from_secs(60));
        let live = Path::new(&fresh.dir).join("snap").join("live");
        fs::write(&live, "s").unwrap();
        sweep(&f.root, now);
        assert!(!Path::new(&stale.dir).exists());
        assert!(Path::new(&kept.payload).exists());
        assert!(Path::new(&fresh.payload).exists());
        assert!(!snap.exists());
        assert!(live.exists());
    }

    #[test]
    fn open_local_path_refuses_relative_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(check_openable("relative.txt").is_err());
        assert!(check_openable(&tmp.path().join("missing.txt").to_string_lossy()).is_err());
        let p = tmp.path().join("there.txt");
        fs::write(&p, "x").unwrap();
        assert_eq!(check_openable(&p.to_string_lossy()).unwrap(), p);
    }

    #[test]
    fn drag_copies_are_private_confined_and_released() {
        let tmp = tempfile::tempdir().unwrap();
        let root = drag_root_in(&tmp.path().join("cache")).unwrap();
        assert_eq!(root.file_name().unwrap(), DRAG_DIR);
        let p = PathBuf::from(drag_copy_path_in(&root, "h_aaaaaaaa", "/srv/a b.txt").unwrap());
        assert_eq!(p.file_name().unwrap(), "a b.txt");
        assert_eq!(p.parent().unwrap().parent().unwrap(), root);
        // Stable per (Host, path), distinct across Hosts.
        assert_eq!(
            drag_copy_path_in(&root, "h_aaaaaaaa", "/srv/a b.txt").unwrap(),
            p.to_string_lossy()
        );
        assert_ne!(
            drag_copy_path_in(&root, "h_bbbbbbbb", "/srv/a b.txt").unwrap(),
            p.to_string_lossy()
        );
        assert!(drag_copy_path_in(&root, "h_aaaaaaaa", "/srv/..").is_err());
        assert!(drag_copy_path_in(&root, "h_aaaaaaaa", "/").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [&root, &p.parent().unwrap().to_path_buf()] {
                let mode = fs::metadata(d).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{}", d.display());
            }
        }
        fs::write(&p, "x").unwrap();
        // Outside the area, the area itself, and deeper paths are refused.
        let outside = tmp.path().join("other.txt");
        fs::write(&outside, "x").unwrap();
        assert!(drag_release(&root, &outside).is_err());
        assert!(drag_release(&root, &root).is_err());
        assert!(drag_release(&root, &p.join("deeper")).is_err());
        assert!(drag_release(&root, Path::new("relative")).is_err());
        assert!(outside.exists());
        drag_release(&root, &p).unwrap();
        assert!(!p.parent().unwrap().exists());
        // Already gone: fine.
        drag_release(&root, &p).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn drag_area_refuses_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = drag_root_in(&tmp.path().join("cache")).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep.txt"), "x").unwrap();
        // A symlinked copy folder is never created through nor released.
        let key = copy_key("h_aaaaaaaa", "/srv/keep.txt");
        std::os::unix::fs::symlink(&elsewhere, root.join(&key)).unwrap();
        assert!(drag_copy_path_in(&root, "h_aaaaaaaa", "/srv/keep.txt").is_err());
        assert!(drag_release(&root, &root.join(&key)).is_err());
        assert!(drag_release(&root, &root.join(&key).join("keep.txt")).is_err());
        assert!(elsewhere.join("keep.txt").exists());
        // A symlinked drag area is refused.
        let cache2 = tmp.path().join("cache2");
        fs::create_dir(&cache2).unwrap();
        std::os::unix::fs::symlink(&elsewhere, cache2.join(DRAG_DIR)).unwrap();
        assert!(drag_root_in(&cache2).is_err());
    }

    #[test]
    fn drag_sweep_removes_only_old_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let root = drag_root_in(&tmp.path().join("cache")).unwrap();
        let old = PathBuf::from(drag_copy_path_in(&root, "h_aaaaaaaa", "/srv/old.txt").unwrap());
        let fresh = PathBuf::from(drag_copy_path_in(&root, "h_aaaaaaaa", "/srv/new.txt").unwrap());
        fs::write(&old, "x").unwrap();
        fs::write(&fresh, "x").unwrap();
        let now = SystemTime::now();
        let long_ago = now - SCRATCH_MAX_AGE - Duration::from_secs(60);
        set_mtime(&old, long_ago);
        set_mtime(old.parent().unwrap(), long_ago);
        sweep_drag(&root, now);
        assert!(!old.parent().unwrap().exists());
        assert!(fresh.exists());
    }
}

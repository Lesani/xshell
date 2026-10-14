//! Chunked file transfers between a Desktop and a Host: `file_stat`, `file_read`, `make_dir`
//! and upload sessions (`upload_begin`, `upload_chunk`, `upload_commit`, `upload_abort`), plus
//! the pumps the Desktop runs a whole download or a path upload with, over any transport.
//!
//! Same trust as `list_dir`: any path the Host's user can reach, no new confinement. Part
//! files are created new (`0600`) next to their target, so the final rename never crosses a
//! filesystem. Desktops reach Remote Hosts over SSH only and Mobiles are refused (ADR-0004):
//! when a Desktop can reach a Host through the Relay, cap one transfer (e.g. 512 MiB) and
//! respect the quota notice before letting these methods through it.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The most bytes one `file_read` answers or one `upload_chunk` carries (decoded). About
/// 1.4 MiB of JSON: far below the SSH link's frame and queue caps.
pub const MAX_CHUNK: u64 = 1024 * 1024;

/// The start of the error when the Host's file is not the version the client expected:
/// `"{FILE_CHANGED}: {current version or `deleted`}"`, `"…: retargeted"` (the path now
/// resolves elsewhere) or `"…: during download"`.
pub const FILE_CHANGED: &str = "file changed on the host";
/// The start of the error when the target exists: `"{FILE_EXISTS}: {path}"`.
pub const FILE_EXISTS: &str = "file exists";
/// An upload id that is expired, aborted, finished or was never issued.
pub const UPLOAD_UNKNOWN: &str = "unknown upload";
/// An `upload_commit` whose answer never came (the call timed out): it may have published
/// the file or not; the client checks the target again.
pub const UPLOAD_OUTCOME_UNKNOWN: &str = "upload outcome unknown";
/// A pump stopped by its cancel flag.
pub const TRANSFER_CANCELLED: &str = "transfer cancelled";
/// A no-clobber publish on a filesystem with neither `RENAME_NOREPLACE` nor hard links.
pub const CANNOT_RENAME_SAFELY: &str = "this filesystem cannot rename safely; choose Replace";

/// Open uploads per process (the Desktop's, or the Daemon's).
pub const MAX_UPLOADS: usize = 64;
/// An upload untouched this long is dropped with its part file.
pub const UPLOAD_IDLE: Duration = Duration::from_secs(10 * 60);
/// A part file no live upload owns is removed once it is this old.
pub const STALE_PART_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Part files are `.<name>.xshell-part-<12 hex>`.
const PART_MARK: &str = ".xshell-part-";
const NOT_REGULAR: &str = "not a regular file";

/// What `upload_begin` (and a retried `upload_commit`) does when the target exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnClash {
    /// Refuse with [`FILE_EXISTS`].
    Fail,
    /// Overwrite it (through a symlink: the file the link points to, the link stays).
    Replace,
    /// Take the first free `name (n).ext`.
    KeepBoth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub size: u64,
    pub mtime_ms: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// `sha256:<hex>` of the content, only when asked for and never for a directory.
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadChunk {
    pub bytes_base64: String,
    /// The read reached the file's size as it was at the read.
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Begun {
    pub upload_id: String,
    pub final_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Written {
    pub written: u64,
}

/// A finished upload or download: where the file is now, its size and its version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transferred {
    pub path: String,
    pub size: u64,
    pub version: String,
}

// ── Reads ─────────────────────────────────────────────────────────────

/// Follows symlinks, like `list_dir`; `is_symlink` says whether `path` itself is one.
pub fn file_stat(path: &str, version: bool) -> Result<FileStat, String> {
    let p = Path::new(path);
    let stat_err = |e: io::Error| format!("Failed to stat {path}: {e}");
    let link = fs::symlink_metadata(p).map_err(stat_err)?;
    let meta = fs::metadata(p).map_err(stat_err)?;
    let version = if version && !meta.is_dir() {
        Some(hash_file(p)?)
    } else {
        None
    };
    Ok(FileStat {
        size: meta.len(),
        mtime_ms: mtime_ms(&meta),
        is_dir: meta.is_dir(),
        is_symlink: link.file_type().is_symlink(),
        version,
    })
}

/// Up to `len` (at most [`MAX_CHUNK`]) bytes of a regular file from `offset`.
pub fn file_read(path: &str, offset: u64, len: u64) -> Result<ReadChunk, String> {
    let p = Path::new(path);
    let (mut f, meta) = open_regular(p)?;
    let size = meta.len();
    let want = len.min(MAX_CHUNK).min(size.saturating_sub(offset));
    let mut buf = vec![0u8; want as usize];
    let mut n = 0;
    if want > 0 {
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("Failed to read {path}: {e}"))?;
        n = read_full(&mut f, &mut buf).map_err(|e| format!("Failed to read {path}: {e}"))?;
        buf.truncate(n);
    }
    Ok(ReadChunk {
        bytes_base64: B64.encode(&buf),
        eof: offset + n as u64 >= size,
    })
}

/// Create one directory (umask applies). An existing directory is fine; anything else there
/// is [`FILE_EXISTS`].
pub fn make_dir(path: &str) -> Result<(), String> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if Path::new(path).is_dir() {
                Ok(())
            } else {
                Err(format!("{FILE_EXISTS}: {path}"))
            }
        }
        Err(e) => Err(format!("Failed to create {path}: {e}")),
    }
}

/// `sha256:<hex>` of a regular file's content.
pub fn hash_file(p: &Path) -> Result<String, String> {
    let (mut f, _) = open_regular(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("Failed to read {}: {e}", p.display()))?;
        if n == 0 {
            return Ok(version_of(&h));
        }
        h.update(&buf[..n]);
    }
}

/// Open `p` for reading, refusing anything but a regular file by the handle's own metadata.
/// Unix: opened non-blocking, so a FIFO is refused instead of blocking the call's thread.
fn open_regular(p: &Path) -> Result<(File, fs::Metadata), String> {
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NONBLOCK);
    let f = o
        .open(p)
        .map_err(|e| format!("Failed to open {}: {e}", p.display()))?;
    let m = f
        .metadata()
        .map_err(|e| format!("Failed to stat {}: {e}", p.display()))?;
    if m.is_dir() {
        return Err(format!("is a directory: {}", p.display()));
    }
    if !m.is_file() {
        return Err(format!("{NOT_REGULAR}: {}", p.display()));
    }
    Ok((f, m))
}

fn read_full(f: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn mtime_ms(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn version_of(h: &Sha256) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("sha256:");
    for b in h.clone().finalize() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ── Uploads ───────────────────────────────────────────────────────────

/// The part file an upload writes. A seam so tests can fail a write or a sync.
pub(crate) trait PartFile: Send {
    fn put(&mut self, b: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
    fn file(&self) -> &File;
}

impl PartFile for File {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        Write::write_all(self, b)
    }
    fn sync(&mut self) -> io::Result<()> {
        File::sync_all(self)
    }
    fn file(&self) -> &File {
        self
    }
}

struct Session {
    /// The name asked for (keep_both numbers it again at commit when need be).
    name: String,
    /// The path the client sees: `dir/name`, or the number keep_both picked.
    shown: PathBuf,
    /// Where a `replace` writes: `shown`, or the file a symlink there resolved to at begin.
    target: PathBuf,
    on_clash: OnClash,
    out: Option<Box<dyn PartFile>>,
    size: u64,
    written: u64,
    hash: Sha256,
    /// Published: the part path is the target now and must not be removed.
    done: bool,
}

#[derive(Clone, Copy)]
struct Activity {
    /// In a chunk or a commit: never swept.
    busy: bool,
    last_active: Instant,
}

struct Entry {
    part: PathBuf,
    activity: Mutex<Activity>,
    aborted: AtomicBool,
    session: Mutex<Session>,
}

/// How a failed operation leaves its session.
enum Fail {
    /// The client can retry (commit again with other options, resend a chunk).
    Keep(String),
    /// The session is gone with its part file; the target is untouched.
    Abort(String),
}

type PartIds = Box<dyn FnMut() -> String + Send>;
type WrapPart = Box<dyn Fn(File) -> Box<dyn PartFile> + Send + Sync>;

/// The upload sessions of one process. Not bound to a connection: neither `dispatch` nor the
/// Desktop's local commands know one. Lock order: table → session → commit; an entry's
/// `activity` is a leaf.
pub struct Uploads {
    table: Mutex<HashMap<String, Arc<Entry>>>,
    /// Serializes the precondition check and the rename of every commit.
    commit: Mutex<()>,
    part_ids: Mutex<PartIds>,
    wrap: WrapPart,
    /// Called at [`Point`]s of a commit: lets tests act inside its windows.
    probe: Probe,
}

/// A place inside a commit, for [`Uploads::probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    /// The destination was checked against `if_version`.
    Hashed,
}

type Probe = Box<dyn Fn(Point) + Send + Sync>;

impl Default for Uploads {
    fn default() -> Self {
        Self::with(Box::new(random_hex12), Box::new(|f| Box::new(f)))
    }
}

impl Uploads {
    fn with(part_ids: PartIds, wrap: WrapPart) -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            commit: Mutex::new(()),
            part_ids: Mutex::new(part_ids),
            wrap,
            probe: Box::new(|_| {}),
        }
    }

    #[cfg(all(test, unix))]
    fn with_probe(mut self, probe: Probe) -> Self {
        self.probe = probe;
        self
    }

    pub fn begin(
        &self,
        dir: &str,
        name: &str,
        size: u64,
        on_clash: OnClash,
        now: Instant,
    ) -> Result<Begun, String> {
        self.sweep_idle(now);
        check_name(name)?;
        let d = Path::new(dir);
        let meta = fs::metadata(d).map_err(|e| format!("Failed to read {dir}: {e}"))?;
        if !meta.is_dir() {
            return Err(format!("not a directory: {dir}"));
        }
        if self.table.lock().unwrap().len() >= MAX_UPLOADS {
            return Err("too many uploads".into());
        }
        let requested = d.join(name);
        let (shown, target) = match on_clash {
            OnClash::Fail => {
                if exists(&requested) {
                    return Err(format!("{FILE_EXISTS}: {}", requested.display()));
                }
                (requested.clone(), requested)
            }
            OnClash::KeepBoth => {
                let p = free_name(d, name)?;
                (p.clone(), p)
            }
            OnClash::Replace => {
                let t = replace_target(&requested)?;
                (requested, t)
            }
        };
        let part_dir = target
            .parent()
            .ok_or_else(|| format!("no directory for {}", target.display()))?
            .to_path_buf();
        let live: HashSet<PathBuf> = self
            .table
            .lock()
            .unwrap()
            .values()
            .map(|e| e.part.clone())
            .collect();
        sweep_stale_parts(&part_dir, &live, SystemTime::now());
        let (part, file) = {
            let mut ids = self.part_ids.lock().unwrap();
            create_part(&part_dir, name, &mut *ids)?
        };
        let entry = Arc::new(Entry {
            part: part.clone(),
            activity: Mutex::new(Activity {
                busy: false,
                last_active: now,
            }),
            aborted: AtomicBool::new(false),
            session: Mutex::new(Session {
                name: name.to_string(),
                shown: shown.clone(),
                target,
                on_clash,
                out: Some((self.wrap)(file)),
                size,
                written: 0,
                hash: Sha256::new(),
                done: false,
            }),
        });
        let id = uuid::Uuid::new_v4().simple().to_string();
        {
            let mut table = self.table.lock().unwrap();
            if table.len() >= MAX_UPLOADS {
                drop(table);
                cleanup(&entry);
                return Err("too many uploads".into());
            }
            table.insert(id.clone(), entry);
        }
        Ok(Begun {
            upload_id: id,
            final_path: shown.to_string_lossy().into_owned(),
        })
    }

    pub fn chunk(
        &self,
        id: &str,
        offset: u64,
        bytes_base64: &str,
        now: Instant,
    ) -> Result<Written, String> {
        self.sweep_idle(now);
        let e = self.acquire(id, now)?;
        let r = (|| {
            let mut s = e.session.lock().unwrap();
            if e.aborted.load(Ordering::SeqCst) {
                return Err(Fail::Keep(format!("{UPLOAD_UNKNOWN}: {id}")));
            }
            if offset != s.written {
                return Err(Fail::Keep(format!(
                    "upload out of order: expected {}",
                    s.written
                )));
            }
            let bytes = B64
                .decode(bytes_base64)
                .map_err(|e| Fail::Keep(format!("invalid base64: {e}")))?;
            let n = bytes.len() as u64;
            if n > MAX_CHUNK {
                return Err(Fail::Keep(format!("chunk too large: {n} > {MAX_CHUNK}")));
            }
            if s.written + n > s.size {
                return Err(Fail::Keep(format!(
                    "upload larger than declared: {} bytes",
                    s.size
                )));
            }
            let part = e.part.display().to_string();
            let out = s
                .out
                .as_mut()
                .ok_or_else(|| Fail::Abort(UPLOAD_UNKNOWN.into()))?;
            out.put(&bytes)
                .map_err(|err| Fail::Abort(format!("Failed to write {part}: {err}")))?;
            s.hash.update(&bytes);
            s.written += n;
            Ok(Written { written: s.written })
        })();
        self.settle(id, &e, r, now)
    }

    /// Publish the upload: fsync, check the precondition, rename. `if_version`: the target
    /// must currently be this version (`sha256:…`, or `deleted` for missing), else
    /// [`FILE_CHANGED`]. `on_clash` overrides the policy given at begin. A [`FILE_CHANGED`]
    /// or [`FILE_EXISTS`] keeps the session for a retry; any other error ends it.
    pub fn commit(
        &self,
        id: &str,
        if_version: Option<String>,
        on_clash: Option<OnClash>,
        now: Instant,
    ) -> Result<Transferred, String> {
        let e = self.acquire(id, now)?;
        let r = self.publish(id, &e, if_version, on_clash);
        self.settle(id, &e, r, now)
    }

    fn publish(
        &self,
        id: &str,
        e: &Arc<Entry>,
        if_version: Option<String>,
        on_clash: Option<OnClash>,
    ) -> Result<Transferred, Fail> {
        let mut s = e.session.lock().unwrap();
        if e.aborted.load(Ordering::SeqCst) {
            return Err(Fail::Keep(format!("{UPLOAD_UNKNOWN}: {id}")));
        }
        if s.written != s.size {
            return Err(Fail::Abort(format!(
                "upload incomplete: {} of {} bytes",
                s.written, s.size
            )));
        }
        let clash = on_clash.unwrap_or(s.on_clash);
        let _one = self.commit.lock().unwrap();
        let part = e.part.clone();
        {
            let out = s
                .out
                .as_mut()
                .ok_or_else(|| Fail::Abort(UPLOAD_UNKNOWN.into()))?;
            out.sync()
                .map_err(|err| Fail::Abort(format!("Failed to write {}: {err}", part.display())))?;
        }
        // The path asked for must still lead where it led at begin.
        let target = if clash == OnClash::Replace {
            let t = replace_target(&s.shown).map_err(Fail::Abort)?;
            if s.on_clash == OnClash::Replace && t != s.target {
                return Err(Fail::Keep(format!("{FILE_CHANGED}: retargeted")));
            }
            t
        } else {
            s.shown.clone()
        };
        if let Some(want) = &if_version {
            let current = match fs::symlink_metadata(&target) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => "deleted".to_string(),
                _ => hash_file(&target).map_err(Fail::Abort)?,
            };
            if current != *want {
                return Err(Fail::Keep(format!("{FILE_CHANGED}: {current}")));
            }
            (self.probe)(Point::Hashed);
        }
        // Permissions through the handle, never by path: an existing target's (so `+x`
        // survives a replace), else 0644. Windows inherits the directory's ACL.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = match fs::metadata(&target) {
                Ok(m) if clash == OnClash::Replace => m.permissions(),
                _ => fs::Permissions::from_mode(0o644),
            };
            let out = s.out.as_ref().expect("checked above");
            out.file()
                .set_permissions(perms)
                .map_err(|err| Fail::Abort(format!("Failed to set permissions: {err}")))?;
        }
        if e.aborted.load(Ordering::SeqCst) {
            return Err(Fail::Keep(format!("{UPLOAD_UNKNOWN}: {id}")));
        }
        // Last, right before the rename (after the slow hash): the part path must still name
        // the file this upload wrote.
        let out = s.out.as_ref().expect("checked above");
        verify_part(&part, out.file()).map_err(Fail::Abort)?;
        // From here on the upload is published or it failed; nothing cancels it.
        let published = match clash {
            OnClash::Replace => {
                fs::rename(&part, &target).map_err(|err| {
                    Fail::Abort(format!("Failed to replace {}: {err}", target.display()))
                })?;
                s.shown.clone()
            }
            OnClash::Fail => match rename_noreplace(&part, &target) {
                Ok(()) => target,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(Fail::Keep(format!("{FILE_EXISTS}: {}", target.display())))
                }
                Err(err) => return Err(Fail::Abort(publish_error(&target, err))),
            },
            OnClash::KeepBoth => {
                let dir = s.shown.parent().map(Path::to_path_buf).unwrap_or_default();
                let mut t = target;
                let mut tries = 0;
                loop {
                    if tries > 0 {
                        let out = s.out.as_ref().expect("checked above");
                        verify_part(&part, out.file()).map_err(Fail::Abort)?;
                    }
                    match rename_noreplace(&part, &t) {
                        Ok(()) => break t,
                        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && tries < 16 => {
                            tries += 1;
                            t = free_name(&dir, &s.name).map_err(Fail::Abort)?;
                        }
                        Err(err) => return Err(Fail::Abort(publish_error(&t, err))),
                    }
                }
            }
        };
        #[cfg(unix)]
        if let Some(parent) = published.parent() {
            // Best effort: the rename is durable once the directory is.
            let _ = File::open(parent).and_then(|d| d.sync_all());
        }
        s.done = true;
        s.out = None;
        Ok(Transferred {
            path: published.to_string_lossy().into_owned(),
            size: s.size,
            version: version_of(&s.hash),
        })
    }

    /// Drop the upload and its part file. Unknown ids are fine.
    pub fn abort(&self, id: &str) {
        let e = self.table.lock().unwrap().remove(id);
        if let Some(e) = e {
            stop(&e);
        }
    }

    /// Drop every upload idle for more than [`UPLOAD_IDLE`] at `now`.
    pub fn sweep_idle(&self, now: Instant) {
        let gone: Vec<Arc<Entry>> = {
            let mut table = self.table.lock().unwrap();
            let ids: Vec<String> = table
                .iter()
                .filter(|(_, e)| {
                    let a = *e.activity.lock().unwrap();
                    !a.busy && now.saturating_duration_since(a.last_active) > UPLOAD_IDLE
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| table.remove(id)).collect()
        };
        for e in gone {
            stop(&e);
        }
    }

    /// Drop every upload (the Daemon is exiting).
    pub fn abort_all(&self) {
        let all: Vec<Arc<Entry>> = self.table.lock().unwrap().drain().map(|(_, e)| e).collect();
        for e in all {
            stop(&e);
        }
    }

    /// The live session `id`, marked busy and active at `now`.
    fn acquire(&self, id: &str, now: Instant) -> Result<Arc<Entry>, String> {
        let table = self.table.lock().unwrap();
        let e = table
            .get(id)
            .ok_or_else(|| format!("{UPLOAD_UNKNOWN}: {id}"))?;
        let mut a = e.activity.lock().unwrap();
        if a.busy {
            return Err(format!("upload busy: {id}"));
        }
        *a = Activity {
            busy: true,
            last_active: now,
        };
        drop(a);
        Ok(e.clone())
    }

    /// Finish an operation on an acquired session: keep it (idle again) or end it.
    fn settle<T>(
        &self,
        id: &str,
        e: &Arc<Entry>,
        r: Result<T, Fail>,
        now: Instant,
    ) -> Result<T, String> {
        let done = r.is_ok() && e.session.lock().unwrap().done;
        match r {
            Ok(v) if !done => {
                release(e, now);
                Ok(v)
            }
            Ok(v) => {
                self.remove(id, e);
                Ok(v)
            }
            Err(Fail::Keep(m)) => {
                release(e, now);
                Err(m)
            }
            Err(Fail::Abort(m)) => {
                self.remove(id, e);
                Err(m)
            }
        }
    }

    fn remove(&self, id: &str, e: &Arc<Entry>) {
        {
            let mut table = self.table.lock().unwrap();
            if table.get(id).is_some_and(|x| Arc::ptr_eq(x, e)) {
                table.remove(id);
            }
        }
        e.aborted.store(true, Ordering::SeqCst);
        cleanup(e);
    }
}

/// An entry just taken out of the table: clean it up now, or let its busy operation do it
/// when it ends ([`release`]).
fn stop(e: &Arc<Entry>) {
    e.aborted.store(true, Ordering::SeqCst);
    if !e.activity.lock().unwrap().busy {
        cleanup(e);
    }
}

fn release(e: &Arc<Entry>, now: Instant) {
    *e.activity.lock().unwrap() = Activity {
        busy: false,
        last_active: now,
    };
    if e.aborted.load(Ordering::SeqCst) {
        cleanup(e);
    }
}

/// Close and remove the part file, unless it was published.
fn cleanup(e: &Entry) {
    let mut s = e.session.lock().unwrap();
    if s.done {
        return;
    }
    s.out = None;
    s.done = true;
    let _ = fs::remove_file(&e.part);
}

fn publish_error(target: &Path, e: io::Error) -> String {
    if e.kind() == io::ErrorKind::Unsupported {
        CANNOT_RENAME_SAFELY.into()
    } else {
        format!("Failed to write {}: {e}", target.display())
    }
}

fn check_name(name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.contains(['/', '\\', '\0']);
    if bad {
        Err(format!("invalid file name {name:?}"))
    } else {
        Ok(())
    }
}

/// Anything at `p`, a dangling symlink included.
fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// Where a `replace` of `p` writes: the file a symlink at `p` resolves to, else `p`.
fn replace_target(p: &Path) -> Result<PathBuf, String> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => {
            let t = fs::canonicalize(p)
                .map_err(|e| format!("Failed to resolve {}: {e}", p.display()))?;
            if t.is_dir() {
                return Err(format!("is a directory: {}", p.display()));
            }
            Ok(t)
        }
        Ok(m) if m.is_dir() => Err(format!("is a directory: {}", p.display())),
        _ => Ok(p.to_path_buf()),
    }
}

/// `name (n).ext` (`.tar.gz` counts as `.gz`; a dotfile keeps its whole name as the stem).
fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

/// `dir/name` when free, else the first free `name (n).ext`, n = 1..999.
fn free_name(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let first = dir.join(name);
    if !exists(&first) {
        return Ok(first);
    }
    (1..=999)
        .map(|n| dir.join(numbered(name, n)))
        .find(|p| !exists(p))
        .ok_or_else(|| format!("{FILE_EXISTS}: {}", first.display()))
}

fn random_hex12() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// `name` cut to at most `max` bytes on a character boundary.
fn truncate_bytes(name: &str, max: usize) -> &str {
    let mut end = name.len().min(max);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn is_part_name(n: &str) -> bool {
    n.starts_with('.') && n.contains(PART_MARK)
}

/// A new part file `.<name>.xshell-part-<id>` in `dir`, owner-only, created new (never over
/// an existing file or symlink): a taken name draws another id, a few times.
fn create_part(
    dir: &Path,
    name: &str,
    next_id: &mut dyn FnMut() -> String,
) -> Result<(PathBuf, File), String> {
    let base = truncate_bytes(name, 120);
    for _ in 0..8 {
        let p = dir.join(format!(".{base}{PART_MARK}{}", next_id()));
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        match o.open(&p) {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Failed to create {}: {e}", p.display())),
        }
    }
    Err(format!("Failed to create a part file in {}", dir.display()))
}

/// Remove the part files in `dir` older than [`STALE_PART_AGE`] that no live upload owns.
fn sweep_stale_parts(dir: &Path, live: &HashSet<PathBuf>, now: SystemTime) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        if !is_part_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let p = entry.path();
        if live.contains(&p) {
            continue;
        }
        let Ok(m) = fs::symlink_metadata(&p) else {
            continue;
        };
        let old = m
            .modified()
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > STALE_PART_AGE);
        if m.file_type().is_file() && old {
            let _ = fs::remove_file(&p);
        }
    }
}

/// The part path still names the file this upload has open (not a symlink or another file
/// put there since).
#[cfg(unix)]
fn verify_part(part: &Path, f: &File) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let open = f
        .metadata()
        .map_err(|e| format!("Failed to stat the part file: {e}"))?;
    let at = fs::symlink_metadata(part).map_err(|e| format!("the part file is gone: {e}"))?;
    if at.file_type().is_symlink() || at.dev() != open.dev() || at.ino() != open.ino() {
        return Err(format!("the part file was replaced: {}", part.display()));
    }
    Ok(())
}

/// Windows: not checked. The part is created new in the target's directory and held open;
/// comparing file indexes needs a second, reparse-point handle on the path.
#[cfg(not(unix))]
fn verify_part(_part: &Path, _f: &File) -> Result<(), String> {
    Ok(())
}

/// Rename `from` to `to` unless `to` exists (`AlreadyExists` then). `Unsupported` when the
/// filesystem offers no atomic way: there is no check-then-rename fallback.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        noreplace_with(from, to, &native_noreplace, &|a, b| fs::hard_link(a, b))
    }
    #[cfg(windows)]
    {
        windows_noreplace(from, to)
    }
}

/// Whether the native no-clobber rename ran (`Ok(false)`: the filesystem lacks it).
#[cfg(target_os = "linux")]
fn native_noreplace(from: &Path, to: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    };
    let (a, b) = (c(from)?, c(to)?);
    // SAFETY: two valid NUL-terminated paths; renameat2 reads them only.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) => Ok(false),
        _ => Err(e),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn native_noreplace(_from: &Path, _to: &Path) -> io::Result<bool> {
    Ok(false)
}

/// How a filesystem without hard links refuses one.
#[cfg(unix)]
const NO_LINKS: [i32; 5] = [
    libc::EPERM,
    libc::EOPNOTSUPP,
    libc::ENOTSUP,
    libc::EXDEV,
    libc::EMLINK,
];

/// The native rename, else hard link (fails atomically on an existing name) and unlink.
#[cfg(unix)]
fn noreplace_with(
    from: &Path,
    to: &Path,
    native: &dyn Fn(&Path, &Path) -> io::Result<bool>,
    link: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if native(from, to)? {
        return Ok(());
    }
    match link(from, to) {
        Ok(()) => {
            // Published; a part name left behind is swept as stale.
            let _ = fs::remove_file(from);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(e) if e.raw_os_error().is_some_and(|c| NO_LINKS.contains(&c)) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            CANNOT_RENAME_SAFELY,
        )),
        Err(e) => Err(e),
    }
}

/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` is natively no-clobber.
#[cfg(windows)]
fn windows_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let w = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (a, b) = (w(from), w(to));
    // SAFETY: two valid NUL-terminated wide strings.
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// ── The process-wide sessions ────────────────────────────────────────

fn uploads() -> &'static Uploads {
    static UPLOADS: OnceLock<Uploads> = OnceLock::new();
    UPLOADS.get_or_init(Uploads::default)
}

pub fn upload_begin(dir: &str, name: &str, size: u64, on_clash: OnClash) -> Result<Begun, String> {
    uploads().begin(dir, name, size, on_clash, Instant::now())
}

pub fn upload_chunk(upload_id: &str, offset: u64, bytes_base64: &str) -> Result<Written, String> {
    uploads().chunk(upload_id, offset, bytes_base64, Instant::now())
}

pub fn upload_commit(
    upload_id: &str,
    if_version: Option<String>,
    on_clash: Option<OnClash>,
) -> Result<Transferred, String> {
    uploads().commit(upload_id, if_version, on_clash, Instant::now())
}

pub fn upload_abort(upload_id: &str) {
    uploads().abort(upload_id)
}

/// Drop the uploads idle at `now` (a Daemon's periodic sweep).
pub fn sweep_idle(now: Instant) {
    uploads().sweep_idle(now)
}

/// Drop every upload (a Daemon exiting).
pub fn abort_all() {
    uploads().abort_all()
}

// ── Pumps ─────────────────────────────────────────────────────────────
//
// A whole transfer as a loop of Host calls, one at a time, through `call` (a local dispatch
// or a Remote Host's link). `local_err` makes the transport's error from a local failure.

/// A Host call: the method and its params, the result or the transport's error.
pub type Call<'a, E> = dyn FnMut(&str, Value) -> Result<Value, E> + 'a;

fn parse<T: DeserializeOwned, E>(v: Value, local_err: &dyn Fn(String) -> E) -> Result<T, E> {
    serde_json::from_value(v).map_err(|e| local_err(format!("unexpected answer: {e}")))
}

fn cancelled<E>(cancel: &AtomicBool, local_err: &dyn Fn(String) -> E) -> Result<(), E> {
    if cancel.load(Ordering::SeqCst) {
        Err(local_err(TRANSFER_CANCELLED.into()))
    } else {
        Ok(())
    }
}

/// Download `remote_path` to `local_path` (replaced if it exists) through a part file next
/// to it. A file that changes during the download is fetched once more, then
/// [`FILE_CHANGED`]. `private`: the file stays `0600` (unix), else `0644`.
pub fn download_with<E: Display>(
    call: &mut Call<'_, E>,
    local_err: &dyn Fn(String) -> E,
    remote_path: &str,
    local_path: &Path,
    private: bool,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Transferred, E> {
    let parent = local_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = local_path
        .file_name()
        .map_or_else(|| "download".into(), |n| n.to_string_lossy().into_owned());
    for _attempt in 0..2 {
        cancelled(cancel, local_err)?;
        let stat = json!({ "path": remote_path });
        let before: FileStat = parse(call("file_stat", stat.clone())?, local_err)?;
        if before.is_dir {
            return Err(local_err(format!("is a directory: {remote_path}")));
        }
        let (part, mut file) = create_part(parent, &name, &mut random_hex12).map_err(local_err)?;
        let r = download_into(
            call,
            local_err,
            remote_path,
            &mut file,
            &part,
            &before,
            private,
            progress,
            cancel,
        );
        match r {
            Ok(Some(size_version)) => {
                if let Err(e) = cancelled(cancel, local_err) {
                    let _ = fs::remove_file(&part);
                    return Err(e);
                }
                // The handle stays open through the check, right before the rename.
                if let Err(e) = verify_part(&part, &file) {
                    let _ = fs::remove_file(&part);
                    return Err(local_err(e));
                }
                if let Err(e) = fs::rename(&part, local_path) {
                    let _ = fs::remove_file(&part);
                    return Err(local_err(format!(
                        "Failed to write {}: {e}",
                        local_path.display()
                    )));
                }
                return Ok(Transferred {
                    path: local_path.to_string_lossy().into_owned(),
                    size: size_version.0,
                    version: size_version.1,
                });
            }
            Ok(None) => {
                let _ = fs::remove_file(&part);
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                return Err(e);
            }
        }
    }
    Err(local_err(format!("{FILE_CHANGED}: during download")))
}

/// One attempt: `Some((size, version))`, or `None` when the file changed meanwhile.
#[allow(clippy::too_many_arguments)]
fn download_into<E: Display>(
    call: &mut Call<'_, E>,
    local_err: &dyn Fn(String) -> E,
    remote_path: &str,
    file: &mut File,
    part: &Path,
    before: &FileStat,
    private: bool,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Option<(u64, String)>, E> {
    let write_err = |e: io::Error| local_err(format!("Failed to write {}: {e}", part.display()));
    let mut hash = Sha256::new();
    let mut offset = 0u64;
    progress(0, before.size);
    loop {
        cancelled(cancel, local_err)?;
        let args = json!({ "path": remote_path, "offset": offset, "len": MAX_CHUNK });
        let chunk: ReadChunk = parse(call("file_read", args)?, local_err)?;
        let bytes = B64
            .decode(&chunk.bytes_base64)
            .map_err(|e| local_err(format!("invalid base64: {e}")))?;
        file.write_all(&bytes).map_err(write_err)?;
        hash.update(&bytes);
        offset += bytes.len() as u64;
        progress(offset, before.size.max(offset));
        if chunk.eof {
            break;
        }
        if bytes.is_empty() {
            return Err(local_err(format!("short read at {offset}: {remote_path}")));
        }
    }
    let after: FileStat = parse(
        call("file_stat", json!({ "path": remote_path }))?,
        local_err,
    )?;
    if after.size != before.size || after.mtime_ms != before.mtime_ms || offset != after.size {
        return Ok(None);
    }
    #[cfg(unix)]
    if !private {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(write_err)?;
    }
    #[cfg(not(unix))]
    let _ = private;
    file.sync_all().map_err(write_err)?;
    Ok(Some((offset, version_of(&hash))))
}

/// Upload the local file `local_path` as `dir/name` (begin, chunks, commit). Every failure
/// before the commit aborts the Host's session; a commit refused with [`FILE_CHANGED`] or
/// [`FILE_EXISTS`] too (the caller uploads again to retry). A cancel takes effect up to the
/// commit; after that the upload completes.
#[allow(clippy::too_many_arguments)]
pub fn upload_path_with<E: Display>(
    call: &mut Call<'_, E>,
    local_err: &dyn Fn(String) -> E,
    local_path: &Path,
    dir: &str,
    name: &str,
    on_clash: OnClash,
    if_version: Option<String>,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Transferred, E> {
    cancelled(cancel, local_err)?;
    // Non-blocking, checked by the handle: a FIFO is refused, never waited on.
    let (mut file, meta) = open_regular(local_path).map_err(local_err)?;
    let size = meta.len();
    let begin = json!({ "dir": dir, "name": name, "size": size, "onClash": on_clash });
    let begun: Begun = parse(call("upload_begin", begin)?, local_err)?;
    let id = begun.upload_id;
    let sent = send_chunks(call, local_err, &mut file, &id, size, progress, cancel)
        .and_then(|()| cancelled(cancel, local_err));
    if let Err(e) = sent {
        let _ = call("upload_abort", json!({ "uploadId": id }));
        return Err(e);
    }
    let commit = json!({ "uploadId": id, "ifVersion": if_version });
    match call("upload_commit", commit) {
        Ok(v) => parse(v, local_err),
        Err(e) => {
            let m = e.to_string();
            if m.starts_with(FILE_CHANGED) || m.starts_with(FILE_EXISTS) {
                let _ = call("upload_abort", json!({ "uploadId": id }));
            }
            Err(e)
        }
    }
}

fn send_chunks<E: Display>(
    call: &mut Call<'_, E>,
    local_err: &dyn Fn(String) -> E,
    file: &mut File,
    id: &str,
    size: u64,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<(), E> {
    let changed = || local_err("the local file changed during the upload".into());
    let mut buf = vec![0u8; MAX_CHUNK as usize];
    let mut sent = 0u64;
    progress(0, size);
    loop {
        cancelled(cancel, local_err)?;
        let n = read_full(file, &mut buf).map_err(|e| local_err(format!("Failed to read: {e}")))?;
        if n == 0 {
            break;
        }
        if sent + n as u64 > size {
            return Err(changed());
        }
        let args = json!({ "uploadId": id, "offset": sent, "bytesBase64": B64.encode(&buf[..n]) });
        call("upload_chunk", args)?;
        sent += n as u64;
        progress(sent, size);
    }
    if sent != size {
        return Err(changed());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::sync::Barrier;

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    fn sha(b: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(b);
        version_of(&h)
    }

    fn parts_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| is_part_name(n))
            .collect();
        v.sort();
        v
    }

    /// Upload `bytes` in one chunk to `dir/name` and return the session id.
    fn staged(u: &Uploads, dir: &Path, name: &str, bytes: &[u8], clash: OnClash) -> Begun {
        let now = Instant::now();
        let b = u
            .begin(&s(dir), name, bytes.len() as u64, clash, now)
            .unwrap();
        if !bytes.is_empty() {
            u.chunk(&b.upload_id, 0, &B64.encode(bytes), now).unwrap();
        }
        b
    }

    #[test]
    fn file_stat_cheap_and_versioned() {
        let fx = Fixture::new();
        let f = fx.write("d/a.txt", "hello");
        let st = file_stat(&s(&f), false).unwrap();
        assert_eq!((st.size, st.is_dir, st.is_symlink), (5, false, false));
        assert_eq!(st.version, None);
        assert!(st.mtime_ms > 0);
        assert_eq!(
            file_stat(&s(&f), true).unwrap().version,
            Some(sha(b"hello"))
        );
        assert!(sha(b"hello").starts_with("sha256:") && sha(b"hello").len() == 7 + 64);
        let d = file_stat(&s(f.parent().unwrap()), true).unwrap();
        assert!(d.is_dir);
        assert_eq!(d.version, None);
        assert!(file_stat(&s(&fx.dir.path().join("nope")), false).is_err());
        #[cfg(unix)]
        {
            let l = fx.dir.path().join("d/link");
            std::os::unix::fs::symlink(&f, &l).unwrap();
            let st = file_stat(&s(&l), true).unwrap();
            assert!(st.is_symlink);
            assert_eq!(st.version, Some(sha(b"hello")));
        }
    }

    #[test]
    fn file_read_chunks_and_eof() {
        let fx = Fixture::new();
        let big: Vec<u8> = (0..MAX_CHUNK + 10).map(|i| (i % 251) as u8).collect();
        let f = fx.write("big.bin", &big);
        let c = file_read(&s(&f), 0, MAX_CHUNK * 4).unwrap();
        let b = B64.decode(&c.bytes_base64).unwrap();
        assert_eq!(b.len() as u64, MAX_CHUNK);
        assert!(!c.eof);
        assert_eq!(&b[..], &big[..MAX_CHUNK as usize]);
        let c = file_read(&s(&f), MAX_CHUNK, MAX_CHUNK).unwrap();
        assert_eq!(
            B64.decode(&c.bytes_base64).unwrap(),
            &big[MAX_CHUNK as usize..]
        );
        assert!(c.eof);
        let c = file_read(&s(&f), MAX_CHUNK + 100, 5).unwrap();
        assert_eq!((c.bytes_base64.as_str(), c.eof), ("", true));
        assert!(file_read(&s(fx.dir.path()), 0, 5).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fifos_are_refused_without_blocking() {
        let fx = Fixture::new();
        let fifo = fx.dir.path().join("fifo");
        let c = std::ffi::CString::new(s(&fifo)).unwrap();
        // SAFETY: a valid path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = file_read(&s(&fifo), 0, 5).unwrap_err();
        assert!(e.starts_with(NOT_REGULAR), "{e}");
        let e = file_stat(&s(&fifo), true).unwrap_err();
        assert!(e.starts_with(NOT_REGULAR), "{e}");
        assert!(file_stat(&s(&fifo), false).is_ok());
        // A conditional commit over a FIFO hashes it: refused, the session ends.
        let u = Uploads::default();
        let b = staged(&u, fx.dir.path(), "fifo", b"x", OnClash::Replace);
        let e = u
            .commit(&b.upload_id, Some(sha(b"")), None, Instant::now())
            .unwrap_err();
        assert!(e.starts_with(NOT_REGULAR), "{e}");
        assert!(parts_in(fx.dir.path()).is_empty());
        // A path upload of a FIFO: refused before any call, without blocking.
        let mut calls = 0;
        let e = upload_path_with(
            &mut |_: &str, _| {
                calls += 1;
                Ok(Value::Null)
            },
            &|m| m,
            &fifo,
            &s(fx.dir.path()),
            "f",
            OnClash::Fail,
            None,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(e.starts_with(NOT_REGULAR), "{e}");
        assert_eq!(calls, 0);
    }

    #[test]
    fn make_dir_creates_merges_and_refuses_files() {
        let fx = Fixture::new();
        let d = fx.dir.path().join("new");
        make_dir(&s(&d)).unwrap();
        assert!(d.is_dir());
        make_dir(&s(&d)).unwrap();
        let f = fx.write("file", "x");
        assert_eq!(make_dir(&s(&f)), Err(format!("{FILE_EXISTS}: {}", s(&f))));
        assert!(make_dir(&s(&fx.dir.path().join("a/b"))).is_err());
    }

    #[test]
    fn upload_round_trip_new_file() {
        let fx = Fixture::new();
        let dir = fx.dir.path().join("up");
        fs::create_dir(&dir).unwrap();
        let u = Uploads::default();
        let now = Instant::now();
        let b = u.begin(&s(&dir), "a.bin", 9, OnClash::Fail, now).unwrap();
        assert_eq!(b.final_path, s(&dir.join("a.bin")));
        assert_eq!(parts_in(&dir).len(), 1);
        for (i, piece) in [b"abc", b"def", b"ghi"].iter().enumerate() {
            let w = u
                .chunk(&b.upload_id, i as u64 * 3, &B64.encode(piece), now)
                .unwrap();
            assert_eq!(w.written, (i as u64 + 1) * 3);
        }
        let t = u.commit(&b.upload_id, None, None, now).unwrap();
        assert_eq!(t.path, s(&dir.join("a.bin")));
        assert_eq!((t.size, t.version.clone()), (9, sha(b"abcdefghi")));
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"abcdefghi");
        assert!(parts_in(&dir).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("a.bin"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644);
        }
        // The session is gone.
        let e = u.commit(&b.upload_id, None, None, now).unwrap_err();
        assert!(e.starts_with(UPLOAD_UNKNOWN), "{e}");
        // An empty file needs no chunk.
        let b = u.begin(&s(&dir), "empty", 0, OnClash::Fail, now).unwrap();
        let t = u.commit(&b.upload_id, None, None, now).unwrap();
        assert_eq!((t.size, t.version), (0, sha(b"")));
    }

    #[test]
    fn upload_out_of_order_and_overflow_rejected() {
        let fx = Fixture::new();
        let u = Uploads::default();
        let now = Instant::now();
        let b = u
            .begin(&s(fx.dir.path()), "a", 4, OnClash::Fail, now)
            .unwrap();
        let id = &b.upload_id;
        assert_eq!(
            u.chunk(id, 2, &B64.encode(b"ab"), now),
            Err("upload out of order: expected 0".into())
        );
        u.chunk(id, 0, &B64.encode(b"ab"), now).unwrap();
        assert_eq!(
            u.chunk(id, 0, &B64.encode(b"ab"), now),
            Err("upload out of order: expected 2".into())
        );
        let e = u.chunk(id, 2, &B64.encode(b"abc"), now).unwrap_err();
        assert!(e.starts_with("upload larger than declared"), "{e}");
        let big = B64.encode(vec![0u8; MAX_CHUNK as usize + 1]);
        let b2 = u
            .begin(&s(fx.dir.path()), "b", MAX_CHUNK * 2, OnClash::Fail, now)
            .unwrap();
        assert!(u
            .chunk(&b2.upload_id, 0, &big, now)
            .unwrap_err()
            .starts_with("chunk too large"));
        // Incomplete at commit: the session ends with its part file.
        let e = u.commit(id, None, None, now).unwrap_err();
        assert_eq!(e, "upload incomplete: 2 of 4 bytes");
        assert!(u
            .chunk(id, 2, &B64.encode(b"cd"), now)
            .unwrap_err()
            .starts_with(UPLOAD_UNKNOWN));
        assert!(!fx.dir.path().join("a").exists());
    }

    #[test]
    fn bad_base64_rejected() {
        let fx = Fixture::new();
        let u = Uploads::default();
        let now = Instant::now();
        let b = u
            .begin(&s(fx.dir.path()), "a", 5, OnClash::Fail, now)
            .unwrap();
        // The lenient decoder of `save_dropped_file` would skip the newline; this one refuses.
        for bad in ["aGVs\nbG8=", "aGVsbG8", "!!!!"] {
            let e = u.chunk(&b.upload_id, 0, bad, now).unwrap_err();
            assert!(e.starts_with("invalid base64"), "{bad}: {e}");
        }
        u.chunk(&b.upload_id, 0, "aGVsbG8=", now).unwrap();
    }

    #[test]
    fn name_validation() {
        let fx = Fixture::new();
        let u = Uploads::default();
        let now = Instant::now();
        let long = "x".repeat(256);
        for bad in ["../x", "a/b", "a\\b", ".", "..", "", "a\0b", long.as_str()] {
            let e = u
                .begin(&s(fx.dir.path()), bad, 1, OnClash::Fail, now)
                .unwrap_err();
            assert!(e.starts_with("invalid file name"), "{bad:?}: {e}");
        }
        let f = fx.write("file", "x");
        assert!(u
            .begin(&s(&f), "a", 1, OnClash::Fail, now)
            .unwrap_err()
            .starts_with("not a directory"));
        assert!(parts_in(fx.dir.path()).is_empty());
    }

    #[test]
    fn clash_fail_begin_and_commit() {
        let fx = Fixture::new();
        let dir = fx.dir.path();
        let u = Uploads::default();
        let now = Instant::now();
        let a = fx.write("a.txt", "old");
        let e = u
            .begin(&s(dir), "a.txt", 3, OnClash::Fail, now)
            .unwrap_err();
        assert_eq!(e, format!("{FILE_EXISTS}: {}", s(&a)));
        let b = staged(&u, dir, "b.txt", b"new", OnClash::Fail);
        // Appears between begin and commit.
        fx.write("b.txt", "theirs");
        let e = u.commit(&b.upload_id, None, None, now).unwrap_err();
        assert_eq!(e, format!("{FILE_EXISTS}: {}", s(&dir.join("b.txt"))));
        assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"theirs");
        assert_eq!(parts_in(dir).len(), 1, "the session and part are kept");
        let t = u
            .commit(&b.upload_id, None, Some(OnClash::Replace), now)
            .unwrap();
        assert_eq!(t.path, s(&dir.join("b.txt")));
        assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"new");
        assert!(parts_in(dir).is_empty());
    }

    #[test]
    fn clash_keep_both_names() {
        let fx = Fixture::new();
        let dir = fx.dir.path();
        let u = Uploads::default();
        let now = Instant::now();
        fx.write("a.txt", "0");
        let b1 = staged(&u, dir, "a.txt", b"1", OnClash::KeepBoth);
        assert_eq!(b1.final_path, s(&dir.join("a (1).txt")));
        assert_eq!(
            u.commit(&b1.upload_id, None, None, now).unwrap().path,
            b1.final_path
        );
        let b2 = staged(&u, dir, "a.txt", b"2", OnClash::KeepBoth);
        assert_eq!(b2.final_path, s(&dir.join("a (2).txt")));
        // Taken by someone else before the commit: the next number.
        fx.write("a (2).txt", "theirs");
        let t = u.commit(&b2.upload_id, None, None, now).unwrap();
        assert_eq!(t.path, s(&dir.join("a (3).txt")));
        assert_eq!(fs::read(dir.join("a (3).txt")).unwrap(), b"2");
        assert_eq!(fs::read(dir.join("a (2).txt")).unwrap(), b"theirs");
        fx.write(".env", "0");
        let b = staged(&u, dir, ".env", b"e", OnClash::KeepBoth);
        assert_eq!(b.final_path, s(&dir.join(".env (1)")));
        fx.write("x.tar.gz", "0");
        let b = staged(&u, dir, "x.tar.gz", b"t", OnClash::KeepBoth);
        assert_eq!(b.final_path, s(&dir.join("x.tar (1).gz")));
        // Free: the name itself.
        let b = staged(&u, dir, "free.txt", b"f", OnClash::KeepBoth);
        assert_eq!(b.final_path, s(&dir.join("free.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn replace_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new();
        let f = fx.write("run.sh", "old");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        let u = Uploads::default();
        let b = staged(
            &u,
            fx.dir.path(),
            "run.sh",
            b"#!/bin/sh\n",
            OnClash::Replace,
        );
        u.commit(&b.upload_id, None, None, Instant::now()).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"#!/bin/sh\n");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_writes_through_symlink() {
        let fx = Fixture::new();
        let real = fx.write("repo/dot", "old");
        fs::create_dir(fx.dir.path().join("home2")).unwrap();
        let link = fx.dir.path().join("home2/.dot");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let u = Uploads::default();
        let b = staged(
            &u,
            &fx.dir.path().join("home2"),
            ".dot",
            b"new",
            OnClash::Replace,
        );
        assert_eq!(b.final_path, s(&link));
        let t = u.commit(&b.upload_id, None, None, Instant::now()).unwrap();
        assert_eq!(t.path, s(&link));
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"new");
        assert!(parts_in(&fx.dir.path().join("repo")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn retargeted_link_is_refused() {
        let fx = Fixture::new();
        let a = fx.write("a", "A");
        let b2 = fx.write("b", "B");
        let link = fx.dir.path().join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let u = Uploads::default();
        let b = staged(&u, fx.dir.path(), "link", b"new", OnClash::Replace);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b2, &link).unwrap();
        let e = u
            .commit(&b.upload_id, None, None, Instant::now())
            .unwrap_err();
        assert_eq!(e, format!("{FILE_CHANGED}: retargeted"));
        assert_eq!(fs::read(&a).unwrap(), b"A");
        assert_eq!(fs::read(&b2).unwrap(), b"B");
        // Pointed back: the commit goes through.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        u.commit(&b.upload_id, None, None, Instant::now()).unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"new");
    }

    #[test]
    fn if_version_match_and_changed() {
        let fx = Fixture::new();
        let dir = fx.dir.path();
        let f = fx.write("f", "v1");
        let u = Uploads::default();
        let now = Instant::now();
        let b = staged(&u, dir, "f", b"v2", OnClash::Replace);
        let t = u.commit(&b.upload_id, Some(sha(b"v1")), None, now).unwrap();
        assert_eq!(t.version, sha(b"v2"));
        // Changed behind our back.
        let b = staged(&u, dir, "f", b"v3", OnClash::Replace);
        fs::write(&f, "theirs").unwrap();
        let e = u
            .commit(&b.upload_id, Some(sha(b"v2")), None, now)
            .unwrap_err();
        assert_eq!(e, format!("{FILE_CHANGED}: {}", sha(b"theirs")));
        assert_eq!(fs::read(&f).unwrap(), b"theirs");
        // Deleted.
        fs::remove_file(&f).unwrap();
        let e = u
            .commit(&b.upload_id, Some(sha(b"v2")), None, now)
            .unwrap_err();
        assert_eq!(e, format!("{FILE_CHANGED}: deleted"));
        // "Overwrite": commit without the precondition.
        u.commit(&b.upload_id, None, None, now).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"v3");
        assert!(parts_in(dir).is_empty());
    }

    #[test]
    fn concurrent_commits_same_target_one_changed() {
        let fx = Fixture::new();
        let dir = fx.dir.path();
        fx.write("f", "v1");
        let u = Arc::new(Uploads::default());
        let a = staged(&u, dir, "f", b"A", OnClash::Replace);
        let b = staged(&u, dir, "f", b"B", OnClash::Replace);
        let go = Arc::new(Barrier::new(2));
        let ts: Vec<_> = [a.upload_id, b.upload_id]
            .into_iter()
            .map(|id| {
                let (u, go) = (u.clone(), go.clone());
                std::thread::spawn(move || {
                    go.wait();
                    u.commit(&id, Some(sha(b"v1")), None, Instant::now())
                })
            })
            .collect();
        let rs: Vec<_> = ts.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(rs.iter().filter(|r| r.is_ok()).count(), 1, "{rs:?}");
        let e = rs.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert!(e.starts_with(FILE_CHANGED), "{e}");
    }

    #[test]
    fn idle_sweep_removes_part() {
        let fx = Fixture::new();
        let u = Uploads::default();
        let t0 = Instant::now();
        let b = u
            .begin(&s(fx.dir.path()), "a", 2, OnClash::Fail, t0)
            .unwrap();
        u.chunk(
            &b.upload_id,
            0,
            &B64.encode(b"a"),
            t0 + Duration::from_secs(300),
        )
        .unwrap();
        // Ten minutes after the chunk, not after the begin.
        u.sweep_idle(t0 + Duration::from_secs(700));
        assert_eq!(parts_in(fx.dir.path()).len(), 1);
        u.sweep_idle(t0 + Duration::from_secs(300) + UPLOAD_IDLE + Duration::from_secs(1));
        assert!(parts_in(fx.dir.path()).is_empty());
        let e = u.chunk(&b.upload_id, 1, &B64.encode(b"b"), t0).unwrap_err();
        assert!(e.starts_with(UPLOAD_UNKNOWN), "{e}");
    }

    #[test]
    fn sweep_spares_a_commit_waiting_for_its_turn() {
        let fx = Fixture::new();
        let u = Arc::new(Uploads::default());
        let t0 = Instant::now();
        let b = staged(&u, fx.dir.path(), "a", b"x", OnClash::Fail);
        let held = u.commit.lock().unwrap();
        let (u2, id) = (u.clone(), b.upload_id.clone());
        let t = std::thread::spawn(move || u2.commit(&id, None, None, t0));
        // Wait until the commit holds its session and waits on the commit lock.
        let entry = u.table.lock().unwrap().get(&b.upload_id).unwrap().clone();
        while !entry.activity.lock().unwrap().busy {
            std::thread::yield_now();
        }
        u.sweep_idle(t0 + UPLOAD_IDLE * 3);
        drop(held);
        assert!(t.join().unwrap().is_ok());
        assert_eq!(fs::read(fx.dir.path().join("a")).unwrap(), b"x");
    }

    #[test]
    fn abort_idempotent_and_removes_part() {
        let fx = Fixture::new();
        let u = Uploads::default();
        let b = staged(&u, fx.dir.path(), "a", b"x", OnClash::Fail);
        assert_eq!(parts_in(fx.dir.path()).len(), 1);
        u.abort(&b.upload_id);
        assert!(parts_in(fx.dir.path()).is_empty());
        u.abort(&b.upload_id);
        u.abort("nonsense");
        assert!(!fx.dir.path().join("a").exists());
    }

    #[test]
    fn unknown_upload() {
        let u = Uploads::default();
        let now = Instant::now();
        assert_eq!(
            u.chunk("bogus", 0, "", now),
            Err(format!("{UPLOAD_UNKNOWN}: bogus"))
        );
        assert_eq!(
            u.commit("bogus", None, None, now),
            Err(format!("{UPLOAD_UNKNOWN}: bogus"))
        );
    }

    #[test]
    fn stale_part_files_swept_on_begin() {
        let fx = Fixture::new();
        let dir = fx.dir.path();
        let u = Uploads::default();
        let live = staged(&u, dir, "live", b"x", OnClash::Fail);
        let live_part = parts_in(dir)[0].clone();
        let old = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
        fx.set_mtime(&dir.join(&live_part), old);
        let stale = fx.write(".gone.xshell-part-aaaaaaaaaaaa", "x");
        fx.set_mtime(&stale, old);
        let fresh = fx.write(".new.xshell-part-bbbbbbbbbbbb", "x");
        let other = fx.write("keep.txt", "x");
        fx.set_mtime(&other, old);
        staged(&u, dir, "next", b"y", OnClash::Fail);
        assert!(!stale.exists());
        assert!(fresh.exists() && other.exists());
        assert!(
            dir.join(&live_part).exists(),
            "a live session's part is kept"
        );
        u.commit(&live.upload_id, None, None, Instant::now())
            .unwrap();
    }

    #[test]
    fn too_many_uploads() {
        let fx = Fixture::new();
        let u = Uploads::default();
        let now = Instant::now();
        for i in 0..MAX_UPLOADS {
            u.begin(&s(fx.dir.path()), &format!("f{i}"), 1, OnClash::Fail, now)
                .unwrap();
        }
        let e = u
            .begin(&s(fx.dir.path()), "one-more", 1, OnClash::Fail, now)
            .unwrap_err();
        assert_eq!(e, "too many uploads");
        u.abort_all();
        assert!(parts_in(fx.dir.path()).is_empty());
        u.begin(&s(fx.dir.path()), "again", 1, OnClash::Fail, now)
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn part_file_is_private_and_create_new() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new();
        let dir = fx.dir.path();
        let victim = fx.write("victim", "safe");
        std::os::unix::fs::symlink(&victim, dir.join(".a.xshell-part-aaaaaaaaaaaa")).unwrap();
        let mut ids = ["aaaaaaaaaaaa", "bbbbbbbbbbbb"]
            .into_iter()
            .map(String::from);
        let ids: PartIds = Box::new(move || ids.next().unwrap_or_else(|| "zzzzzzzzzzzz".into()));
        let u = Uploads::with(ids, Box::new(|f| Box::new(f)));
        let b = staged(&u, dir, "a", b"data", OnClash::Fail);
        let part = dir.join(".a.xshell-part-bbbbbbbbbbbb");
        assert_eq!(
            fs::metadata(&part).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(&victim).unwrap(), b"safe");
        u.commit(&b.upload_id, None, None, Instant::now()).unwrap();
        assert_eq!(fs::read(dir.join("a")).unwrap(), b"data");
        // Every id taken: the begin fails without touching anything.
        let u = Uploads::with(
            Box::new(|| "aaaaaaaaaaaa".into()),
            Box::new(|f| Box::new(f)),
        );
        assert!(u
            .begin(&s(dir), "a", 1, OnClash::Replace, Instant::now())
            .is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn swapped_part_file_is_refused() {
        let fx = Fixture::new();
        let dir = fx.dir.path();
        let secret = fx.write("secret", "s3cret");
        let u = Uploads::default();
        let b = staged(&u, dir, "out", b"mine", OnClash::Fail);
        let part = dir.join(&parts_in(dir)[0]);
        fs::remove_file(&part).unwrap();
        std::os::unix::fs::symlink(&secret, &part).unwrap();
        let e = u
            .commit(&b.upload_id, None, None, Instant::now())
            .unwrap_err();
        assert!(e.starts_with("the part file was replaced"), "{e}");
        assert!(!dir.join("out").exists());
        assert_eq!(fs::read(&secret).unwrap(), b"s3cret");
    }

    /// The part is swapped for a symlink while the commit hashes the destination: the check
    /// right before the rename catches it.
    #[cfg(unix)]
    #[test]
    fn part_swapped_during_conditional_commit_is_refused() {
        let fx = Fixture::new();
        let dir = fx.dir.path().to_path_buf();
        let secret = fx.write("secret", "s3cret");
        let target = fx.write("t", "v1");
        let (d, sec) = (dir.clone(), secret.clone());
        let probe: Probe = Box::new(move |p| {
            assert_eq!(p, Point::Hashed);
            let part = d.join(&parts_in(&d)[0]);
            fs::remove_file(&part).unwrap();
            std::os::unix::fs::symlink(&sec, &part).unwrap();
        });
        let u = Uploads::default().with_probe(probe);
        let b = staged(&u, &dir, "t", b"v2", OnClash::Replace);
        let e = u
            .commit(&b.upload_id, Some(sha(b"v1")), None, Instant::now())
            .unwrap_err();
        assert!(e.starts_with("the part file was replaced"), "{e}");
        assert_eq!(fs::read(&target).unwrap(), b"v1");
        assert!(!fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&secret).unwrap(), b"s3cret");
    }

    /// The local part is swapped for a symlink before the download publishes it.
    #[cfg(unix)]
    #[test]
    fn part_swapped_during_download_is_refused() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let src = fx.write("remote/f", "remote bytes");
        let victim = fx.write("victim", "safe");
        let local = fx.dir.path().join("local");
        fs::create_dir(&local).unwrap();
        let dst = local.join("f");
        let mut inner = dispatcher(&ctx);
        let mut stats = 0;
        let mut call = |m: &str, p: Value| {
            if m == "file_stat" {
                stats += 1;
                if stats == 2 {
                    // After the last read, before the rename.
                    let part = local.join(&parts_in(&local)[0]);
                    fs::remove_file(&part).unwrap();
                    std::os::unix::fs::symlink(&victim, &part).unwrap();
                }
            }
            inner(m, p)
        };
        let e = download_with(
            &mut call,
            &|m| m,
            &s(&src),
            &dst,
            false,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(e.starts_with("the part file was replaced"), "{e}");
        assert!(fs::symlink_metadata(&dst).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"safe");
        assert!(parts_in(&local).is_empty());
    }

    /// A part file whose writes or syncs fail on demand.
    struct Flaky {
        f: File,
        fail_write: bool,
        fail_sync: bool,
    }

    impl PartFile for Flaky {
        fn put(&mut self, b: &[u8]) -> io::Result<()> {
            if self.fail_write {
                return Err(io::Error::other("disk full"));
            }
            Write::write_all(&mut self.f, b)
        }
        fn sync(&mut self) -> io::Result<()> {
            if self.fail_sync {
                return Err(io::Error::other("io error"));
            }
            self.f.sync_all()
        }
        fn file(&self) -> &File {
            &self.f
        }
    }

    #[test]
    fn write_or_sync_failure_aborts_and_keeps_target() {
        for (fail_write, fail_sync) in [(true, false), (false, true)] {
            let fx = Fixture::new();
            let dir = fx.dir.path();
            let target = fx.write("t", "old");
            let wrap: WrapPart = Box::new(move |f| {
                Box::new(Flaky {
                    f,
                    fail_write,
                    fail_sync,
                })
            });
            let u = Uploads::with(Box::new(random_hex12), wrap);
            let now = Instant::now();
            let b = u.begin(&s(dir), "t", 3, OnClash::Replace, now).unwrap();
            let r = u
                .chunk(&b.upload_id, 0, &B64.encode(b"new"), now)
                .and_then(|_| u.commit(&b.upload_id, None, None, now));
            let e = r.unwrap_err();
            assert!(e.starts_with("Failed to write"), "{e}");
            assert_eq!(fs::read(&target).unwrap(), b"old");
            assert!(parts_in(dir).is_empty());
            assert!(u.table.lock().unwrap().is_empty(), "the slot is free");
        }
    }

    #[test]
    fn rename_noreplace_refuses_existing() {
        let fx = Fixture::new();
        let a = fx.write("a", "A");
        let b = fx.write("b", "B");
        let e = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"B");
        let c = fx.dir.path().join("c");
        rename_noreplace(&a, &c).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read(&c).unwrap(), b"A");
    }

    #[cfg(unix)]
    #[test]
    fn noreplace_without_native_or_links_refuses() {
        let fx = Fixture::new();
        let a = fx.write("a", "A");
        let to = fx.dir.path().join("b");
        let no_native = |_: &Path, _: &Path| Ok(false);
        // Hard links instead: works, and refuses an existing name.
        noreplace_with(&a, &to, &no_native, &|x, y| fs::hard_link(x, y)).unwrap();
        assert!(!a.exists() && to.exists());
        let a = fx.write("a", "A");
        let e = noreplace_with(&a, &to, &no_native, &|x, y| fs::hard_link(x, y)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        // Neither: a clear refusal, nothing moved.
        let to2 = fx.dir.path().join("c");
        let no_links = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(libc::EPERM));
        let e = noreplace_with(&a, &to2, &no_native, &no_links).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert_eq!(publish_error(&to2, e), CANNOT_RENAME_SAFELY);
        assert!(a.exists() && !to2.exists());
    }

    // ── Pumps through dispatch ──

    fn dispatcher(ctx: &crate::HostCtx) -> impl FnMut(&str, Value) -> Result<Value, String> + '_ {
        move |m, p| crate::dispatch(ctx, m, p)
    }

    #[test]
    fn download_with_via_dispatch() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let big: Vec<u8> = (0..MAX_CHUNK * 2 + 7).map(|i| (i % 253) as u8).collect();
        let src = fx.write("remote/big.bin", &big);
        fs::create_dir(fx.dir.path().join("local")).unwrap();
        let dst = fx.dir.path().join("local/big.bin");
        fs::write(&dst, "previous").unwrap();
        let mut seen = vec![];
        let t = download_with(
            &mut dispatcher(&ctx),
            &|m| m,
            &s(&src),
            &dst,
            false,
            &mut |d, t| seen.push((d, t)),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(&dst).unwrap(), big);
        assert_eq!((t.size, t.version), (big.len() as u64, sha(&big)));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "{seen:?}");
        assert_eq!(seen.last(), Some(&(big.len() as u64, big.len() as u64)));
        assert!(parts_in(&fx.dir.path().join("local")).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
                0o644
            );
            let p = fx.dir.path().join("local/private");
            download_with(
                &mut dispatcher(&ctx),
                &|m| m,
                &s(&src),
                &p,
                true,
                &mut |_, _| {},
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // A directory is refused.
        let e = download_with(
            &mut dispatcher(&ctx),
            &|m| m,
            &s(&fx.dir.path().join("remote")),
            &dst,
            false,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(e.starts_with("is a directory"), "{e}");
    }

    #[test]
    fn download_cancel_and_change() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let big = vec![7u8; MAX_CHUNK as usize * 2];
        let src = fx.write("remote/f", &big);
        fs::create_dir(fx.dir.path().join("local")).unwrap();
        let dst = fx.dir.path().join("local/f");
        // Cancelled before the start: no call at all.
        let mut calls = 0;
        let e = download_with(
            &mut |_: &str, _| {
                calls += 1;
                Ok(Value::Null)
            },
            &|m| m,
            &s(&src),
            &dst,
            false,
            &mut |_, _| {},
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert_eq!((e.as_str(), calls), (TRANSFER_CANCELLED, 0));
        // Cancelled mid-way: the local part file is removed.
        let cancel = AtomicBool::new(false);
        let mut inner = dispatcher(&ctx);
        let mut reads = 0;
        let mut call = |m: &str, p: Value| {
            if m == "file_read" {
                reads += 1;
                if reads == 2 {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            inner(m, p)
        };
        let e = download_with(
            &mut call,
            &|m| m,
            &s(&src),
            &dst,
            false,
            &mut |_, _| {},
            &cancel,
        )
        .unwrap_err();
        assert_eq!(e, TRANSFER_CANCELLED);
        assert!(!dst.exists());
        assert!(parts_in(&fx.dir.path().join("local")).is_empty());
        // The file keeps changing while it is read: one retry, then FILE_CHANGED.
        let mut inner = dispatcher(&ctx);
        let mut n = 0u8;
        let mut call = |m: &str, p: Value| {
            if m == "file_read" {
                n += 1;
                fs::write(&src, vec![n; MAX_CHUNK as usize * 2 + n as usize]).unwrap();
            }
            inner(m, p)
        };
        let e = download_with(
            &mut call,
            &|m| m,
            &s(&src),
            &dst,
            false,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(e, format!("{FILE_CHANGED}: during download"));
        assert!(parts_in(&fx.dir.path().join("local")).is_empty());
    }

    #[test]
    fn upload_path_with_via_dispatch() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let big: Vec<u8> = (0..MAX_CHUNK + 3).map(|i| (i % 249) as u8).collect();
        let local = fx.write("local/f.bin", &big);
        let remote = fx.dir.path().join("remote");
        fs::create_dir(&remote).unwrap();
        let mut seen = vec![];
        let t = upload_path_with(
            &mut dispatcher(&ctx),
            &|m| m,
            &local,
            &s(&remote),
            "f.bin",
            OnClash::Fail,
            None,
            &mut |d, t| seen.push((d, t)),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(remote.join("f.bin")).unwrap(), big);
        assert_eq!(
            (t.path, t.version.clone()),
            (s(&remote.join("f.bin")), sha(&big))
        );
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(seen.last(), Some(&(big.len() as u64, big.len() as u64)));
        // A stale precondition: FILE_CHANGED, the Host's session is aborted.
        let mut aborted = 0;
        let mut inner = dispatcher(&ctx);
        let mut call = |m: &str, p: Value| {
            if m == "upload_abort" {
                aborted += 1;
            }
            inner(m, p)
        };
        let e = upload_path_with(
            &mut call,
            &|m| m,
            &local,
            &s(&remote),
            "f.bin",
            OnClash::Replace,
            Some(sha(b"other")),
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(e, format!("{FILE_CHANGED}: {}", t.version));
        assert_eq!(aborted, 1);
        assert!(parts_in(&remote).is_empty());
    }

    #[test]
    fn upload_cancel_before_start_and_before_commit() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let local = fx.write("local/f", vec![1u8; MAX_CHUNK as usize + 1]);
        let remote = fx.dir.path().join("remote");
        fs::create_dir(&remote).unwrap();
        let mut calls = vec![];
        let e = upload_path_with(
            &mut |m: &str, _| {
                calls.push(m.to_string());
                Ok(Value::Null)
            },
            &|m| m,
            &local,
            &s(&remote),
            "f",
            OnClash::Fail,
            None,
            &mut |_, _| {},
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert_eq!(e, TRANSFER_CANCELLED);
        assert!(calls.is_empty());
        // Cancelled during the last chunk: no commit, the session is aborted.
        let cancel = AtomicBool::new(false);
        let mut inner = dispatcher(&ctx);
        let mut calls = vec![];
        let mut chunks = 0;
        let mut call = |m: &str, p: Value| {
            calls.push(m.to_string());
            if m == "upload_chunk" {
                chunks += 1;
                if chunks == 2 {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            inner(m, p)
        };
        let e = upload_path_with(
            &mut call,
            &|m| m,
            &local,
            &s(&remote),
            "f",
            OnClash::Fail,
            None,
            &mut |_, _| {},
            &cancel,
        )
        .unwrap_err();
        assert_eq!(e, TRANSFER_CANCELLED);
        assert_eq!(
            calls,
            [
                "upload_begin",
                "upload_chunk",
                "upload_chunk",
                "upload_abort"
            ]
        );
        assert!(!remote.join("f").exists());
        assert!(parts_in(&remote).is_empty());
    }
}

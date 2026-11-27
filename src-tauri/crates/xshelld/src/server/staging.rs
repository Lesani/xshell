//! Receiving a build over a link (`daemon.stage-*`, capability `daemon.stage`; ADR-0019).
//!
//! One stage per Daemon. Its bytes go to a private part file in the build's install directory
//! (`~/.xshell/server/<version>+<build>/.xshelld.stage-<uuid>.part`, `O_EXCL|O_NOFOLLOW`,
//! 0700) through a worker thread with a bounded queue, so the connection's reader never
//! waits on the disk. Commit checks, in order: every byte arrived and the SHA-256 matches;
//! the build stamp in the bytes names the announced build and directory; the requester is
//! still connected and still a Desktop of this Ring (C8); the file runs and `--version`
//! reports the same version and build. Only then is it renamed to `<dir>/xshelld`. Nothing
//! is ever selected or started here.
//!
//! Bounds (C7): the announced length is at most [`STAGE_MAX_BYTES`]; a chunk at most
//! [`STAGE_MAX_CHUNK`] decoded bytes and never past the announced length (refused before
//! any write); a write or sync error aborts the stage and removes its part file. A stage that
//! gets nothing for the idle time, or whose connection closes, is removed too.
// Staging over the link is Linux/macOS only: Windows never offers `daemon.stage`, so its
// machinery is compiled there but unused.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use super::registry::Daemon;
use super::{ConnId, TestPoint};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};
use uuid::Uuid;
use xshell_protocol::build::{install_dir_name, scan_stamp, BuildId};
use xshell_protocol::msg::{
    StageBegun, StageCommitted, StageReceived, UpdateLatest, STAGE_BUSY, STAGE_MAX_BYTES,
    STAGE_MAX_CHUNK, UPDATE_LATEST_BAD_BUILD,
};
use xshell_protocol::ring::SignKey;

/// Answers one request.
pub(crate) type Reply = Box<dyn FnOnce(Result<Value, String>) + Send>;

/// Part files older than this are swept when a stage begins.
const PART_MAX_AGE: Duration = Duration::from_secs(3600);
/// Chunks waiting for the worker; a sender with more in flight waits for the replies.
const QUEUE: usize = 4;

/// Who asked: the connection, and (over the Relay) the Ring key it authenticated as.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Requester {
    pub conn: ConnId,
    /// `None`: the local socket (this machine's user) or a test's in-process connection.
    pub peer: Option<SignKey>,
}

/// C8: the requester is still connected, and a Relay requester's key is still a Desktop in
/// this Host's current Roster.
pub(crate) fn authorized(d: &Daemon, who: Requester) -> Result<(), String> {
    let reg = d.reg.lock().unwrap();
    authorized_locked(d, &reg, who)
}

/// [`authorized`] with the registry locked by the caller: a disconnect waits for it.
fn authorized_locked(
    d: &Daemon,
    reg: &super::registry::Registry,
    who: Requester,
) -> Result<(), String> {
    if !reg.conns.contains_key(&who.conn) {
        return Err("the requesting connection is gone".into());
    }
    let Some(key) = who.peer else {
        return Ok(());
    };
    let desktop = d.ring.trusted_chain().is_some_and(|c| {
        c.head()
            .member(&key)
            .is_some_and(|m| m.role == xshell_protocol::ring::Role::Desktop)
    });
    if desktop {
        Ok(())
    } else {
        Err("only a Desktop in this Ring may update this machine".into())
    }
}

enum Job {
    Write(Vec<u8>, Reply),
    Commit(Reply),
    Abort(Reply),
}

struct Active {
    id: Uuid,
    conn: ConnId,
    /// Bytes accepted for writing (queued or written).
    queued: u64,
    length: u64,
    tx: SyncSender<Job>,
    /// Commit runs: the reservation stays until it has fully finished (P2-10).
    committing: bool,
}

/// Test seam (C7): homes whose stage writes fail as on a full disk.
#[doc(hidden)]
pub static FAIL_WRITES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[derive(Default)]
pub(crate) struct Staging {
    slot: Arc<Mutex<Option<Active>>>,
}

impl Staging {
    /// `daemon.stage-begin`. Linux and macOS only: Windows does not offer `daemon.stage`
    /// (the link refuses it before this is reached) and keeps its own update path.
    pub fn begin(
        &self,
        d: &Arc<Daemon>,
        who: Requester,
        version: &str,
        build: &BuildId,
        sha256: &str,
        length: u64,
    ) -> Result<Value, String> {
        #[cfg(unix)]
        {
            self.begin_unix(d, who, version, build, sha256, length)
        }
        #[cfg(not(unix))]
        {
            let _ = (d, who, version, build, sha256, length);
            Err("this xshelld cannot be updated over its link".into())
        }
    }

    #[cfg(unix)]
    fn begin_unix(
        &self,
        d: &Arc<Daemon>,
        who: Requester,
        version: &str,
        build: &BuildId,
        sha256: &str,
        length: u64,
    ) -> Result<Value, String> {
        let verifier = crate::stage::Verifier::new(sha256, length).map_err(|e| e.to_string())?;
        let target = semver::Version::parse(version).map_err(|e| e.to_string())?;
        let current =
            semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())?;
        if target.cmp_precedence(&current).is_lt() {
            return Err(format!(
                "xshell {version} is older than the {current} running here"
            ));
        }
        let dir = install_dir_name(version, Some(build));
        if !xshell_core::server::valid_dir_name(&dir) {
            return Err("invalid install directory".into());
        }
        let home = &d.cfg.home;
        let root = xshell_core::server::server_root(home);
        if let Some(file) = xshell_core::server::valid_install(&root, &dir) {
            if same_bytes(&file, sha256, length) {
                return Ok(json!(StageBegun {
                    stage: None,
                    installed: true
                }));
            }
        }
        // The running build and the selected build are never replaced (P1-6).
        let selected = fs::read_to_string(root.join(xshell_core::server::SELECTED))
            .ok()
            .and_then(|t| t.lines().next().map(str::to_string));
        if dir == crate::upgrade::own_dir() || selected.as_deref() == Some(dir.as_str()) {
            return Ok(json!(StageBegun {
                stage: None,
                installed: true
            }));
        }
        let mut slot = self.slot.lock().unwrap();
        match slot.as_ref() {
            Some(a) if a.conn != who.conn || a.committing => return Err(STAGE_BUSY.into()),
            // The same connection starts over: its earlier stage goes.
            _ => drop(slot.take()),
        }
        let id = Uuid::new_v4();
        let worker = self.open_worker(d, id, who, version, build, dir, verifier)?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let idle = d.cfg.stage_idle;
        std::thread::Builder::new()
            .name("daemon-stage".into())
            .spawn(move || worker.run(rx, idle))
            .map_err(|e| e.to_string())?;
        *slot = Some(Active {
            id,
            conn: who.conn,
            queued: 0,
            length,
            tx,
            committing: false,
        });
        Ok(json!(StageBegun {
            stage: Some(id),
            installed: false
        }))
    }

    /// The part file and the worker that receives build `version`+`build` into install
    /// directory `dir` as stage `id`.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn open_worker(
        &self,
        d: &Arc<Daemon>,
        id: Uuid,
        who: Requester,
        version: &str,
        build: &BuildId,
        dir: String,
        verifier: crate::stage::Verifier,
    ) -> Result<Worker, String> {
        let home = &d.cfg.home;
        sweep_parts(&xshell_core::server::server_root(home));
        let dir_path =
            xshell_hostlink::install::ensure_server_dir(home, &dir).map_err(|e| e.to_string())?;
        let part = dir_path.join(format!(".xshelld.stage-{}.part", id.simple()));
        let file = create_part(&part).map_err(|e| e.to_string())?;
        Ok(Worker {
            d: Arc::downgrade(d),
            slot: Arc::downgrade(&self.slot),
            id,
            who,
            dir,
            dir_path,
            home: home.clone(),
            version: version.to_string(),
            build: build.clone(),
            part,
            file: Some(file),
            verifier: Some(verifier),
            published: false,
            finished: false,
        })
    }

    /// `daemon.update-latest` (capability `daemon.update-latest`): ask the release server for
    /// the latest release and, when it is newer than this Daemon, download its build for this
    /// machine and install it as a `daemon.stage-commit` would. Answered with
    /// [`UpdateLatest`] from a worker thread, so the connection's reader never waits on the
    /// network. Shares the one slot with staging: a stage or another fetch in progress is
    /// [`STAGE_BUSY`], and while this runs a stage begin is too. Selects nothing.
    ///
    /// Failures of the checksum, length, build stamp and reported identity answer with the
    /// [`UPDATE_LATEST_BAD_BUILD`] prefix; saving and authorization failures pass through.
    pub fn update_latest(&self, d: &Arc<Daemon>, who: Requester, reply: Reply) {
        #[cfg(unix)]
        {
            self.update_latest_unix(d, who, reply)
        }
        #[cfg(not(unix))]
        {
            let _ = (d, who);
            reply(Err("this xshelld cannot be updated over its link".into()))
        }
    }

    #[cfg(unix)]
    fn update_latest_unix(&self, d: &Arc<Daemon>, who: Requester, reply: Reply) {
        if let Err(e) = authorized(d, who) {
            return reply(Err(e));
        }
        let id = Uuid::new_v4();
        {
            let mut slot = self.slot.lock().unwrap();
            if slot.is_some() {
                drop(slot);
                return reply(Err(STAGE_BUSY.into()));
            }
            // A sender nobody receives from: `committing` keeps chunks, commits and a
            // disconnect away from the reservation (P2-10); only the fetch frees it.
            let (tx, _) = mpsc::sync_channel(1);
            *slot = Some(Active {
                id,
                conn: who.conn,
                queued: 0,
                length: 0,
                tx,
                committing: true,
            });
        }
        let guard = Reservation {
            slot: Arc::downgrade(&self.slot),
            id,
        };
        let reply = Arc::new(Mutex::new(Some(reply)));
        let r2 = reply.clone();
        let (staging, d2) = (
            Staging {
                slot: self.slot.clone(),
            },
            Arc::downgrade(d),
        );
        let base = d.cfg.get_base.clone();
        let spawned = std::thread::Builder::new()
            .name("daemon-update-latest".into())
            .spawn(move || {
                let r = staging.fetch_and_install(&d2, id, who, &base);
                if let Err(e) = &r {
                    crate::log!("WARN", "update from the release server: {e}");
                }
                // The slot is free before the requester hears back.
                drop(guard);
                if let Some(reply) = r2.lock().unwrap().take() {
                    reply(r.map(|u| json!(u)));
                }
            });
        if let Err(e) = spawned {
            // The closure (and with it the reservation) is gone already.
            if let Some(reply) = reply.lock().unwrap().take() {
                reply(Err(e.to_string()));
            }
        }
    }

    #[cfg(unix)]
    fn fetch_and_install(
        &self,
        d: &Weak<Daemon>,
        id: Uuid,
        who: Requester,
        base: &str,
    ) -> Result<UpdateLatest, String> {
        let own_text = env!("CARGO_PKG_VERSION");
        let own = semver::Version::parse(own_text).map_err(|e| e.to_string())?;
        let triple =
            xshell_protocol::build::host_triple(crate::cli::uname_os(), std::env::consts::ARCH)?;
        let (version, sha256, bytes) = match crate::latest::fetch(base, &own, triple)? {
            crate::latest::Latest::Current => {
                return Ok(UpdateLatest {
                    current: true,
                    version: own_text.into(),
                    build: None,
                    dir: None,
                })
            }
            crate::latest::Latest::Download {
                version,
                sha256,
                bytes,
            } => (version.to_string(), sha256, bytes),
        };
        let bad = |e: &str| format!("{UPDATE_LATEST_BAD_BUILD}: {e}");
        let stamp = scan_stamp(&bytes)
            .map_err(|e| bad(&format!("not an xshell build: {e}")))?
            .ok_or_else(|| bad("the build has no build stamp"))?;
        let dir = install_dir_name(&version, Some(&stamp));
        if !xshell_core::server::valid_dir_name(&dir) {
            return Err(bad("invalid install directory"));
        }
        let length = bytes.len() as u64;
        let verifier =
            crate::stage::Verifier::new(&sha256, length).map_err(|e| bad(&e.to_string()))?;
        let installed = UpdateLatest {
            current: false,
            version: version.clone(),
            build: Some(stamp.clone()),
            dir: Some(dir.clone()),
        };
        let d = d.upgrade().ok_or("xshelld is exiting")?;
        let root = xshell_core::server::server_root(&d.cfg.home);
        // An install directory of this release that exists already is reused only when it
        // holds exactly these bytes and they report the release's identity; anything else
        // there is never overwritten (amendment 6).
        if let Some(file) = xshell_core::server::valid_install(&root, &dir) {
            if !same_bytes(&file, &sha256, length) {
                return Err(bad(&format!("{dir} holds another build")));
            }
            let (v, b) = crate::stage::native_identity(&file).map_err(|e| bad(&e.to_string()))?;
            if v != version || !b.is_some_and(|b| b.same(&stamp)) {
                return Err(bad("the build reports another version or build"));
            }
            authorized(&d, who)?;
            return Ok(installed);
        }
        if fs::symlink_metadata(root.join(&dir).join("xshelld")).is_ok() {
            return Err(bad(&format!("{dir} holds another file")));
        }
        let mut worker = self.open_worker(&d, id, who, &version, &stamp, dir, verifier)?;
        drop(d);
        worker.write(&bytes).map_err(|e| e.to_string())?;
        drop(bytes);
        let r = worker.commit();
        worker.finish();
        match r {
            Ok(_) => Ok(installed),
            Err(CommitError::Bad(e)) => Err(bad(&e)),
            Err(CommitError::Other(e)) => Err(e),
        }
    }

    /// `daemon.stage-chunk`. `Some(result)`: answered now; `None`: the worker answers.
    pub fn chunk(
        &self,
        who: Requester,
        stage: Uuid,
        offset: u64,
        data: &str,
        reply: Reply,
    ) -> Option<Reply> {
        let mut slot = self.slot.lock().unwrap();
        let Some(a) = slot
            .as_mut()
            .filter(|a| a.id == stage && a.conn == who.conn && !a.committing)
        else {
            reply(Err("unknown update stage".into()));
            return None;
        };
        if offset != a.queued {
            // The stage stays: the sender resumes from what it has.
            reply(Err(format!(
                "wrong offset {offset}: {} bytes received",
                a.queued
            )));
            return None;
        }
        // A chunk's base64 is at most this long; anything longer is refused undecoded.
        let max_encoded = STAGE_MAX_CHUNK.div_ceil(3) * 4;
        let bytes = if data.len() as u64 > max_encoded {
            Err("update chunk too large".to_string())
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| format!("invalid update chunk: {e}"))
        }
        .and_then(|b| {
            if b.is_empty() || b.len() as u64 > STAGE_MAX_CHUNK {
                Err("invalid update chunk size".into())
            } else if a.queued + b.len() as u64 > a.length {
                Err("more bytes than announced".into())
            } else {
                Ok(b)
            }
        });
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                // Dropping the sender ends the worker, which removes the part file.
                *slot = None;
                reply(Err(format!("{e}; the update was stopped")));
                return None;
            }
        };
        a.queued += bytes.len() as u64;
        let tx = a.tx.clone();
        drop(slot);
        match tx.try_send(Job::Write(bytes, reply)) {
            Ok(()) => None,
            Err(TrySendError::Full(job)) => {
                // The disk is slow: wait for a place (bounded by the queue).
                if let Err(mpsc::SendError(Job::Write(_, r) | Job::Commit(r) | Job::Abort(r))) =
                    tx.send(job)
                {
                    self.forget(stage);
                    r(Err("the update was stopped".into()));
                }
                None
            }
            Err(TrySendError::Disconnected(Job::Write(_, r) | Job::Commit(r) | Job::Abort(r))) => {
                self.forget(stage);
                r(Err("the update was stopped".into()));
                None
            }
        }
    }

    /// `daemon.stage-commit`: the worker answers once the queued chunks are written.
    pub fn commit(&self, who: Requester, stage: Uuid, reply: Reply) {
        let tx = {
            let mut slot = self.slot.lock().unwrap();
            match slot.as_mut() {
                Some(a) if a.id == stage && a.conn == who.conn && !a.committing => {
                    a.committing = true;
                    Some(a.tx.clone())
                }
                _ => None,
            }
        };
        let Some(tx) = tx else {
            return reply(Err("unknown update stage".into()));
        };
        if let Err(mpsc::SendError(Job::Write(_, r) | Job::Commit(r) | Job::Abort(r))) =
            tx.send(Job::Commit(reply))
        {
            self.forget(stage);
            r(Err("the update was stopped".into()));
        }
    }

    /// `daemon.stage-abort`: answered `null` also for an unknown stage.
    /// The answer comes once the stage's reservation is released and its part file is gone
    /// (after a commit in progress, when there is one), so a client that got it can always
    /// begin again.
    pub fn abort(&self, who: Requester, stage: Uuid, reply: Reply) {
        let tx = {
            let slot = self.slot.lock().unwrap();
            slot.as_ref()
                .filter(|a| a.id == stage && a.conn == who.conn)
                .map(|a| a.tx.clone())
        };
        let Some(tx) = tx else {
            return reply(Ok(Value::Null));
        };
        // A worker that is gone already finished its cleanup.
        if let Err(mpsc::SendError(Job::Write(_, r) | Job::Commit(r) | Job::Abort(r))) =
            tx.send(Job::Abort(reply))
        {
            r(Ok(Value::Null));
        }
    }

    /// The connection closed: its stage goes.
    pub fn drop_conn(&self, conn: ConnId) {
        let mut slot = self.slot.lock().unwrap();
        // A commit in progress finishes; its worker releases the reservation (P2-10).
        if slot
            .as_ref()
            .is_some_and(|a| a.conn == conn && !a.committing)
        {
            *slot = None;
        }
    }

    fn forget(&self, stage: Uuid) {
        let mut slot = self.slot.lock().unwrap();
        if slot.as_ref().is_some_and(|a| a.id == stage) {
            *slot = None;
        }
    }

    /// Test hook: whether a stage is open.
    pub fn active(&self) -> bool {
        self.slot.lock().unwrap().is_some()
    }
}

/// `file` has exactly `length` bytes with SHA-256 `sha256`.
fn same_bytes(file: &Path, sha256: &str, length: u64) -> bool {
    let Ok(f) = fs::File::open(file) else {
        return false;
    };
    if f.metadata().map(|m| m.len()).ok() != Some(length) {
        return false;
    }
    let mut h = Sha256::new();
    let mut buf = vec![0; 256 * 1024];
    let mut r = f.take(length + 1);
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    format!("{:x}", h.finalize()).eq_ignore_ascii_case(sha256)
}

fn create_part(path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o700)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::other("staging over a link is Unix only"))
    }
}

/// Remove part files older than [`PART_MAX_AGE`] from every build directory.
fn sweep_parts(root: &Path) {
    let Ok(dirs) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for dir in dirs.flatten() {
        let Ok(files) = fs::read_dir(dir.path()) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with(".xshelld.stage-") && name.ends_with(".part")) {
                continue;
            }
            let old = f
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age >= PART_MAX_AGE);
            if old {
                let _ = fs::remove_file(f.path());
            }
        }
    }
}

struct Worker {
    d: Weak<Daemon>,
    slot: Weak<Mutex<Option<Active>>>,
    id: Uuid,
    who: Requester,
    dir: String,
    dir_path: PathBuf,
    home: PathBuf,
    version: String,
    build: BuildId,
    part: PathBuf,
    file: Option<fs::File>,
    verifier: Option<crate::stage::Verifier>,
    published: bool,
    finished: bool,
}

impl Worker {
    /// Every final answer is sent after [`Worker::finish`]: the reservation is released and
    /// the part file gone before the client hears back.
    fn run(mut self, rx: mpsc::Receiver<Job>, idle: Duration) {
        loop {
            match rx.recv_timeout(idle) {
                Ok(Job::Write(bytes, reply)) => match self.write(&bytes) {
                    Ok(received) => reply(Ok(json!(StageReceived { received }))),
                    Err(e) => {
                        crate::log!("WARN", "update stage {}: {e}", self.id);
                        self.finish();
                        reply(Err(format!("{e}; the update was stopped")));
                        return drain(&rx);
                    }
                },
                Ok(Job::Commit(reply)) => {
                    let r = self.commit().map_err(CommitError::into_message);
                    if let Err(e) = &r {
                        crate::log!("WARN", "update stage {}: {e}", self.id);
                    }
                    self.finish();
                    reply(r.map(|dir| json!(StageCommitted { dir })));
                    return drain(&rx);
                }
                Ok(Job::Abort(reply)) => {
                    self.finish();
                    reply(Ok(Value::Null));
                    return drain(&rx);
                }
                Err(RecvTimeoutError::Timeout) => {
                    crate::log!("INFO", "update stage {}: idle; removed", self.id);
                    return self.finish();
                }
                // Replaced, or its connection closed.
                Err(RecvTimeoutError::Disconnected) => return self.finish(),
            }
        }
    }

    /// Release the reservation and remove the part file (unless published). Once.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.published {
            let _ = fs::remove_file(&self.part);
        }
        if let Some(slot) = self.slot.upgrade() {
            let mut slot = slot.lock().unwrap();
            if slot.as_ref().is_some_and(|a| a.id == self.id) {
                *slot = None;
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<u64> {
        let v = self.verifier.as_mut().expect("open stage");
        v.check(bytes.len() as u64)?;
        if FAIL_WRITES.lock().unwrap().contains(&self.home) {
            return Err(io::Error::from_raw_os_error(28));
        }
        self.file.as_mut().expect("open stage").write_all(bytes)?;
        v.update(bytes)?;
        Ok(v.received())
    }

    fn commit(&mut self) -> Result<String, CommitError> {
        use CommitError::{Bad, Other};
        self.verifier
            .take()
            .expect("open stage")
            .finish()
            .map_err(|e| Bad(e.to_string()))?;
        let file = self.file.take().expect("open stage");
        file.sync_all()
            .map_err(|e| Other(format!("cannot save the build: {e}")))?;
        drop(file);
        let mut bytes = Vec::new();
        fs::File::open(&self.part)
            .and_then(|f| f.take(STAGE_MAX_BYTES + 1).read_to_end(&mut bytes))
            .map_err(|e| Other(e.to_string()))?;
        let stamp = scan_stamp(&bytes)
            .map_err(|e| Bad(format!("not an xshell build: {e}")))?
            .ok_or_else(|| Bad("the build has no build stamp".into()))?;
        drop(bytes);
        if !stamp.same(&self.build) || install_dir_name(&self.version, Some(&stamp)) != self.dir {
            return Err(Bad(
                "the build does not match its announced version and build".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.part, fs::Permissions::from_mode(0o700))
                .map_err(|e| Other(e.to_string()))?;
        }
        let d = self
            .d
            .upgrade()
            .ok_or_else(|| Other("xshelld is exiting".into()))?;
        d.test_point(self.id, TestPoint::StageBeforeVerify);
        authorized(&d, self.who).map_err(Other)?;
        drop(d);
        #[cfg(unix)]
        {
            let (version, build) =
                crate::stage::native_identity(&self.part).map_err(|e| Bad(e.to_string()))?;
            if version != self.version || !build.is_some_and(|b| b.same(&self.build)) {
                return Err(Bad("the build reports another version or build".into()));
            }
        }
        let d = self
            .d
            .upgrade()
            .ok_or_else(|| Other("xshelld is exiting".into()))?;
        d.test_point(self.id, TestPoint::StageBeforePublish);
        let dest = self.dir_path.join("xshelld");
        {
            // Publication with the registry locked: a disconnect waits for it, or wins
            // before it (P1-6).
            let reg = d.reg.lock().unwrap();
            authorized_locked(&d, &reg, self.who).map_err(Other)?;
            fs::rename(&self.part, &dest).map_err(|e| Other(e.to_string()))?;
            self.published = true;
            let synced =
                sync(&self.dir_path).and_then(|()| self.dir_path.parent().map_or(Ok(()), sync));
            if let Err(e) = synced {
                // Not durable: nothing may be selectable (P2-8).
                self.unpublish(&dest);
                return Err(Other(format!("cannot save the build: {e}")));
            }
        }
        // A Roster revocation is not ordered by that lock: one that won meanwhile undoes it.
        if let Err(e) = authorized(&d, self.who) {
            self.unpublish(&dest);
            return Err(Other(e));
        }
        crate::log!("INFO", "update stage {}: installed {}", self.id, self.dir);
        Ok(self.dir.clone())
    }
}

/// Why a commit failed: the bytes failed a check (length and SHA-256, build stamp, reported
/// identity), or anything else (saving, authorization, exiting).
enum CommitError {
    Bad(String),
    Other(String),
}

impl CommitError {
    fn into_message(self) -> String {
        match self {
            CommitError::Bad(e) | CommitError::Other(e) => e,
        }
    }
}

/// Holds the staging slot for a `daemon.update-latest` fetch; frees it when dropped.
struct Reservation {
    slot: Weak<Mutex<Option<Active>>>,
    id: Uuid,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.upgrade() {
            let mut slot = slot.lock().unwrap();
            if slot.as_ref().is_some_and(|a| a.id == self.id) {
                *slot = None;
            }
        }
    }
}

fn sync(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

impl Worker {
    fn unpublish(&mut self, dest: &Path) {
        let _ = fs::remove_file(dest);
        let _ = sync(&self.dir_path);
        self.published = false;
    }
}

/// Answer what was queued behind a final job: the stage is over.
fn drain(rx: &mpsc::Receiver<Job>) {
    while let Ok(job) = rx.try_recv() {
        match job {
            Job::Abort(r) => r(Ok(Value::Null)),
            Job::Write(_, r) | Job::Commit(r) => r(Err("the update was stopped".into())),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.finish();
    }
}

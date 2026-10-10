//! Dropped files a `term.submit` types the paths of. The files are reserved from the moment
//! they are checked until a grace period after the reply's outcome is known, so the drop
//! directory's sweep never removes a file the agent has not read yet.

use super::terminal::typed_path;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use xshell_core::HostCtx;
use xshell_protocol::msg::{SUBMIT_MAX_FILES, SUBMIT_NOT_DROPPED, SUBMIT_TOO_MANY_FILES};

/// The reserved files, by canonical path: how many replies hold each, and until when the
/// last one's grace lasts.
pub(crate) struct Drops {
    held: Mutex<HashMap<PathBuf, Held>>,
    grace: Duration,
}

#[derive(Default)]
struct Held {
    count: usize,
    until: Option<Instant>,
}

impl Held {
    fn kept(&self, now: Instant) -> bool {
        self.count > 0 || self.until.is_some_and(|u| now < u)
    }
}

/// A reply's hold on its files: released when dropped, after which the files are kept for
/// the grace period.
pub(crate) struct Reservation {
    drops: Arc<Drops>,
    paths: Vec<PathBuf>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let until = Instant::now() + self.drops.grace;
        let mut held = self.drops.held.lock().unwrap();
        for p in &self.paths {
            if let Some(h) = held.get_mut(p) {
                h.count = h.count.saturating_sub(1);
                h.until = Some(h.until.map_or(until, |u| u.max(until)));
            }
        }
    }
}

impl Drops {
    pub fn new(grace: Duration) -> Arc<Self> {
        Arc::new(Drops {
            held: Mutex::new(HashMap::new()),
            grace,
        })
    }

    /// Check `files` (a `term.submit`'s) and reserve them, in one step under the lock the
    /// sweep takes: each must be a file `save_dropped_file` saved
    /// ([`xshell_core::files::dropped_file`]), typed by its canonical path ([`typed_path`]).
    /// The typed paths and the hold, or [`SUBMIT_TOO_MANY_FILES`] / [`SUBMIT_NOT_DROPPED`]
    /// with nothing reserved.
    pub fn reserve(
        self: &Arc<Self>,
        ctx: &HostCtx,
        files: &[String],
    ) -> Result<(Vec<String>, Reservation), String> {
        if files.len() > SUBMIT_MAX_FILES {
            return Err(SUBMIT_TOO_MANY_FILES.into());
        }
        let mut held = self.held.lock().unwrap();
        let mut typed = Vec::with_capacity(files.len());
        let mut paths = Vec::with_capacity(files.len());
        for f in files {
            let canon = xshell_core::files::dropped_file(ctx, f)
                .ok_or_else(|| SUBMIT_NOT_DROPPED.to_string())?;
            typed.push(
                canon
                    .to_str()
                    .and_then(typed_path)
                    .ok_or_else(|| SUBMIT_NOT_DROPPED.to_string())?,
            );
            paths.push(canon);
        }
        for p in &paths {
            held.entry(p.clone()).or_default().count += 1;
        }
        let r = Reservation {
            drops: self.clone(),
            paths,
        };
        Ok((typed, r))
    }

    /// Sweep the drop directory of files older than `max_age`, except the reserved ones, and
    /// forget holds whose grace is over.
    pub fn sweep(&self, ctx: &HostCtx, max_age: Duration, now: SystemTime) {
        let mut held = self.held.lock().unwrap();
        let at = Instant::now();
        held.retain(|_, h| h.kept(at));
        let keep = |p: &Path| held.contains_key(p);
        xshell_core::files::sweep_dropped_files_except(ctx, max_age, now, &keep);
    }

    /// Whether `path` (canonical) is reserved now (tests).
    #[cfg(test)]
    fn is_held(&self, path: &Path) -> bool {
        let held = self.held.lock().unwrap();
        held.get(path).is_some_and(|h| h.kept(Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> HostCtx {
        HostCtx::with_home(dir.join("home"), dir.join("tmp"))
    }

    fn old(p: &Path) {
        fs_set_old(p, Duration::from_secs(3600));
    }

    fn fs_set_old(p: &Path, age: Duration) {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    /// A reserved file outlives the sweep through the reply and its grace; then it goes.
    #[test]
    fn reserved_files_are_not_swept() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let p = xshell_core::files::save_dropped_file(&ctx, "aGk=".into(), "a.jpg".into()).unwrap();
        let other =
            xshell_core::files::save_dropped_file(&ctx, "aGk=".into(), "b.jpg".into()).unwrap();
        let drops = Drops::new(Duration::from_millis(200));
        let (typed, r) = drops.reserve(&ctx, std::slice::from_ref(&p)).unwrap();
        let canon = std::fs::canonicalize(&p).unwrap();
        // Typed as the agents read it (quoted on Windows, where the path has backslashes).
        assert_eq!(typed, [typed_path(&canon.to_string_lossy()).unwrap()]);
        old(Path::new(&p));
        old(Path::new(&other));
        let max = Duration::from_secs(60);
        drops.sweep(&ctx, max, SystemTime::now());
        assert!(Path::new(&p).exists());
        assert!(!Path::new(&other).exists());
        // Two replies hold it; the first one's end does not release it.
        let (_, r2) = drops.reserve(&ctx, std::slice::from_ref(&p)).unwrap();
        drop(r);
        drops.sweep(&ctx, max, SystemTime::now());
        assert!(Path::new(&p).exists() && drops.is_held(&canon));
        drop(r2);
        // In its grace period.
        drops.sweep(&ctx, max, SystemTime::now());
        assert!(Path::new(&p).exists());
        std::thread::sleep(Duration::from_millis(300));
        assert!(!drops.is_held(&canon));
        drops.sweep(&ctx, max, SystemTime::now());
        assert!(!Path::new(&p).exists());
        assert!(drops.held.lock().unwrap().is_empty());
    }

    /// A refused check reserves nothing.
    #[test]
    fn a_refusal_reserves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let p = xshell_core::files::save_dropped_file(&ctx, "aGk=".into(), "a.jpg".into()).unwrap();
        let drops = Drops::new(Duration::from_secs(600));
        let bad = [p.clone(), "/etc/hosts".into()];
        assert_eq!(
            drops.reserve(&ctx, &bad).err().as_deref(),
            Some(SUBMIT_NOT_DROPPED)
        );
        let five = vec![p.clone(); SUBMIT_MAX_FILES + 1];
        assert_eq!(
            drops.reserve(&ctx, &five).err().as_deref(),
            Some(SUBMIT_TOO_MANY_FILES)
        );
        assert!(drops.held.lock().unwrap().is_empty());
    }
}

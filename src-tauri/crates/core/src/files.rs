use crate::ctx::HostCtx;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub fn read_text_file(path: String) -> Result<String, String> {
    fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {}", e))
}

pub fn decode_base64(s: &str) -> Vec<u8> {
    let mut val = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for c in s.chars() {
        let sextet = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => break,
            _ => continue, // ignore whitespace/newlines
        };
        val = (val << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((val >> bits) & 0xFF) as u8);
        }
    }
    out
}

pub const MAX_DROPPED_FILE_BYTES: usize = 25 * 1024 * 1024; // ponytail: flat cap, revisit if legit >25MB drops show up

/// How long a dropped file is kept: [`cleanup_old_dropped_files`] removes older ones.
pub const DROPPED_FILE_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// The drop directory: where [`save_dropped_file`] saves, inside the context's private temp
/// directory.
pub fn drop_dir(ctx: &HostCtx) -> PathBuf {
    ctx.temp_dir.join("xshell-clipboard")
}

// Ctrl+V with an image on the clipboard, or a file dragged in from Explorer — WebView2 (unlike
// a native app) never gives us a real filesystem path for either one, so we save the bytes to
// a real temp file and hand back that path instead, since shells/CLIs take a path, not raw bytes.
// Bytes travel as base64 (not a JSON number array) to keep the IPC payload small.
//
// The file is `<unix ms>-<random>-<sanitized name>`, created new (never over another drop,
// even one saved in the same millisecond under the same name), readable by its owner only.
pub fn save_dropped_file(
    ctx: &HostCtx,
    bytes_base64: String,
    name: String,
) -> Result<String, String> {
    use std::time::UNIX_EPOCH;
    let bytes = decode_base64(&bytes_base64);
    if bytes.len() > MAX_DROPPED_FILE_BYTES {
        return Err(format!(
            "File too large ({} bytes, max {})",
            bytes.len(),
            MAX_DROPPED_FILE_BYTES
        ));
    }
    let dir = drop_dir(ctx);
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create temp dir: {}", e))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .collect();
    let safe_name = if safe_name.is_empty() {
        "file".to_string()
    } else {
        safe_name
    };
    let path = create_drop(&dir, ts, &safe_name, &bytes, &mut || {
        uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
    })?;
    Ok(path.to_string_lossy().to_string())
}

/// Write `bytes` to a new file `<ts>-<id>-<name>` in `dir`, drawing another `id` when the
/// name is taken (a few times).
fn create_drop(
    dir: &Path,
    ts: u128,
    name: &str,
    bytes: &[u8],
    next_id: &mut dyn FnMut() -> String,
) -> Result<PathBuf, String> {
    use std::io::Write;
    let mut tries = 0;
    let (path, mut file) = loop {
        let path = dir.join(format!("{}-{}-{}", ts, next_id(), name));
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        match o.open(&path) {
            Ok(f) => break (path, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
            Err(e) => return Err(format!("Failed to write file: {}", e)),
        }
    };
    if let Err(e) = file.write_all(bytes).and_then(|()| file.flush()) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(format!("Failed to write file: {}", e));
    }
    Ok(path)
}

/// `path` as a file [`save_dropped_file`] saved: an absolute path to a regular file (not a
/// symlink, not a directory) directly in the drop directory, both compared canonically. The
/// canonical path, or `None`. Unix: a file with other hard links is refused too (its name in
/// the drop directory could alias a file elsewhere).
pub fn dropped_file(ctx: &HostCtx, path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return None;
    }
    let regular = |p: &Path| {
        fs::symlink_metadata(p).is_ok_and(|m| {
            #[cfg(unix)]
            let single = std::os::unix::fs::MetadataExt::nlink(&m) == 1;
            #[cfg(not(unix))]
            let single = true;
            m.file_type().is_file() && single
        })
    };
    if !regular(p) {
        return None;
    }
    let canon = fs::canonicalize(p).ok()?;
    let dir = fs::canonicalize(drop_dir(ctx)).ok()?;
    (canon.parent() == Some(dir.as_path()) && regular(&canon)).then_some(canon)
}

/// Called at startup, and periodically by a Daemon: removes dropped files older than
/// [`DROPPED_FILE_MAX_AGE`] — the dir only ever grows otherwise (screenshots, dropped images...).
pub fn cleanup_old_dropped_files(ctx: &HostCtx) {
    sweep_dropped_files(ctx, DROPPED_FILE_MAX_AGE, SystemTime::now());
}

/// Remove the files of the drop directory last modified more than `max_age` before `now`.
pub fn sweep_dropped_files(ctx: &HostCtx, max_age: Duration, now: SystemTime) {
    sweep_dropped_files_except(ctx, max_age, now, &|_| false);
}

/// [`sweep_dropped_files`], keeping every file for which `keep` (given the file's path in the
/// canonical drop directory, as [`dropped_file`] answers it) is true.
pub fn sweep_dropped_files_except(
    ctx: &HostCtx,
    max_age: Duration,
    now: SystemTime,
    keep: &dyn Fn(&Path) -> bool,
) {
    let Ok(dir) = fs::canonicalize(drop_dir(ctx)) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() || keep(&dir.join(entry.file_name())) {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() > max_age {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub fn get_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string())
}

pub fn get_home_dir(ctx: &HostCtx) -> String {
    ctx.home
        .clone()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ── File explorer ─────────────────────────────────────────────────────
//
// Single-level directory listing for the terminal's file-explorer panel. Lazy by design:
// the frontend calls this once per folder as the user expands it (mirroring the git panel's
// lazy-polling philosophy) rather than walking the whole tree up front. Returns folders
// first then files, each case-insensitively alphabetical — the order VS Code's explorer uses.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DirItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

pub fn list_dir(path: String) -> Result<Vec<DirItem>, String> {
    let rd = fs::read_dir(&path).map_err(|e| format!("Failed to read {}: {}", path, e))?;
    let mut items: Vec<DirItem> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // file_type() avoids a stat syscall on most platforms; for symlinks we follow to
        // decide tree-vs-leaf so a symlinked dir still gets an expand chevron.
        let is_dir = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => entry.path().is_dir(),
            Ok(ft) => ft.is_dir(),
            Err(_) => false,
        };
        items.push(DirItem {
            name,
            path: entry.path().to_string_lossy().into_owned(),
            is_dir,
        });
    }
    items.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(items)
}

// Recursive name search under `root` for the file-explorer's search box. Case-insensitive
// substring match on the entry name. Bounded on both axes — at most `limit` matches and a
// hard ceiling on entries visited — so searching a deep tree (or one with node_modules)
// stays responsive rather than walking millions of paths. Symlinked dirs aren't followed,
// which both avoids cycles and keeps the walk bounded.
//
// `async` so Tauri runs it on the async runtime rather than the main thread — a big tree can
// take a moment to walk, and doing it on the main thread would freeze the UI until it returns.
pub fn search_dir(root: String, query: String, limit: Option<usize>) -> Vec<DirItem> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let cap = limit.unwrap_or(300).min(2000);
    const MAX_VISIT: usize = 200_000;
    let mut out: Vec<DirItem> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(&root)];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        if out.len() >= cap || visited >= MAX_VISIT {
            break;
        }
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            visited += 1;
            if visited >= MAX_VISIT {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Don't follow symlinks: treat them as leaves so the walk can't loop or escape.
            let is_dir = matches!(entry.file_type(), Ok(ft) if ft.is_dir() && !ft.is_symlink());
            if out.len() < cap && name.to_lowercase().contains(&q) {
                out.push(DirItem {
                    name,
                    path: entry.path().to_string_lossy().into_owned(),
                    is_dir,
                });
            }
            if is_dir {
                stack.push(entry.path());
            }
        }
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_base64_handles_padding_and_whitespace() {
        assert_eq!(decode_base64("aGVs\nbG8="), b"hello");
    }

    use crate::testutil::Fixture;

    fn names(v: &[DirItem]) -> Vec<(String, bool)> {
        v.iter().map(|i| (i.name.clone(), i.is_dir)).collect()
    }

    #[test]
    fn list_dir_sorts_dirs_first_case_insensitive() {
        let fx = Fixture::new();
        fx.write("d/b.txt", "");
        fx.write("d/A.txt", "");
        fx.write("d/zdir/x", "");
        fx.write("d/Cdir/x", "");
        let dir = fx.dir.path().join("d");
        let items = list_dir(dir.to_string_lossy().to_string()).unwrap();
        assert_eq!(
            names(&items),
            [
                ("Cdir".to_string(), true),
                ("zdir".to_string(), true),
                ("A.txt".to_string(), false),
                ("b.txt".to_string(), false)
            ]
        );
        assert_eq!(std::path::PathBuf::from(&items[2].path), dir.join("A.txt"));

        let missing = fx.dir.path().join("nope").to_string_lossy().to_string();
        let err = list_dir(missing.clone()).unwrap_err();
        assert!(
            err.starts_with(&format!("Failed to read {}: ", missing)),
            "{err}"
        );
    }

    #[test]
    fn search_dir_matches_case_insensitive_and_caps_limit() {
        let fx = Fixture::new();
        fx.write("r/Report.md", "");
        fx.write("r/sub/report-old.txt", "");
        fx.write("r/sub/deeper/REPORTS/x.txt", "");
        fx.write("r/other.txt", "");
        let root = fx.dir.path().join("r").to_string_lossy().to_string();

        let mut all = names(&search_dir(root.clone(), " rePort ".into(), None));
        all.sort();
        assert_eq!(
            all,
            [
                ("REPORTS".to_string(), true),
                ("Report.md".to_string(), false),
                ("report-old.txt".to_string(), false)
            ]
        );
        // Results come back directories first, then by name.
        let out = search_dir(root.clone(), "report".into(), None);
        assert!(out[0].is_dir);

        assert_eq!(search_dir(root.clone(), "report".into(), Some(2)).len(), 2);
        assert!(search_dir(root.clone(), "   ".into(), None).is_empty());
        assert!(search_dir(root, "zzz".into(), None).is_empty());
    }

    #[test]
    fn save_dropped_file_sanitizes_name_into_ctx_temp_dir() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        // "hello" in base64, with a line break as some encoders emit.
        let p = save_dropped_file(&ctx, "aGVs\nbG8=".into(), "my shot:<1>/é.png".into()).unwrap();
        let p = std::path::PathBuf::from(p);
        assert_eq!(p.parent().unwrap(), ctx.temp_dir.join("xshell-clipboard"));
        let file_name = p.file_name().unwrap().to_string_lossy().to_string();
        let (ts, rest) = file_name.split_once('-').unwrap();
        assert!(ts.chars().all(|c| c.is_ascii_digit()), "{file_name}");
        let (id, rest) = rest.split_once('-').unwrap();
        assert!(
            id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit()),
            "{file_name}"
        );
        assert_eq!(rest, "my shot1.png");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");

        // A name with nothing safe left becomes "file".
        let p = save_dropped_file(&ctx, "".into(), "::".into()).unwrap();
        assert!(p.ends_with("-file"), "{p}");
    }

    #[test]
    fn cleanup_old_dropped_files_removes_files_older_than_three_days() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let old = fx.write("tmp/xshell-clipboard/old.png", "x");
        let recent = fx.write("tmp/xshell-clipboard/recent.png", "x");
        let now = SystemTime::now();
        fx.set_mtime(&old, now - Duration::from_secs(4 * 24 * 60 * 60));
        fx.set_mtime(&recent, now - Duration::from_secs(2 * 24 * 60 * 60));
        cleanup_old_dropped_files(&ctx);
        assert!(!old.exists());
        assert!(recent.exists());

        // No directory yet: nothing to do, no panic.
        cleanup_old_dropped_files(&Fixture::new().ctx());
    }

    #[test]
    fn drop_dir_is_under_temp() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        assert_eq!(drop_dir(&ctx), ctx.temp_dir.join("xshell-clipboard"));
    }

    #[test]
    fn sweep_dropped_files_takes_max_age_and_now() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let f = fx.write("tmp/xshell-clipboard/a.png", "x");
        let sub = fx.dir.path().join("tmp/xshell-clipboard/sub");
        std::fs::create_dir(&sub).unwrap();
        let now = SystemTime::now();
        sweep_dropped_files(&ctx, Duration::from_secs(60), now);
        assert!(f.exists());
        sweep_dropped_files(
            &ctx,
            Duration::from_secs(60),
            now + Duration::from_secs(120),
        );
        assert!(!f.exists());
        // Directories are left alone.
        assert!(sub.exists());
        // A kept file stays, however old; `keep` sees its canonical path.
        let kept = fx.write("tmp/xshell-clipboard/kept.png", "x");
        let canon = std::fs::canonicalize(&kept).unwrap();
        let later = now + Duration::from_secs(120);
        sweep_dropped_files_except(&ctx, Duration::from_secs(60), later, &|p| p == canon);
        assert!(kept.exists());
        sweep_dropped_files_except(&ctx, Duration::from_secs(60), later, &|_| false);
        assert!(!kept.exists());
    }

    /// Two drops of the same name in the same millisecond, from many threads at once, each
    /// get a file of their own, with their own bytes.
    #[test]
    fn same_ms_same_name_distinct_paths() {
        let fx = Fixture::new();
        let ctx = std::sync::Arc::new(fx.ctx());
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let (ctx, start) = (ctx.clone(), start.clone());
                std::thread::spawn(move || {
                    let b64 = ["MA==", "MQ==", "Mg==", "Mw=="][i % 4];
                    start.wait();
                    (
                        i,
                        save_dropped_file(&ctx, b64.into(), "photo.jpg".into()).unwrap(),
                    )
                })
            })
            .collect();
        let saved: Vec<(usize, String)> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let paths: std::collections::HashSet<&String> = saved.iter().map(|(_, p)| p).collect();
        assert_eq!(paths.len(), 16);
        for (i, p) in &saved {
            assert_eq!(std::fs::read(p).unwrap(), (i % 4).to_string().as_bytes());
        }
    }

    /// The same millisecond and the same random id: the second drop draws another id and
    /// never overwrites the first; one whose ids keep colliding fails without writing.
    #[test]
    fn a_taken_name_draws_another_id() {
        let fx = Fixture::new();
        let dir = drop_dir(&fx.ctx());
        std::fs::create_dir_all(&dir).unwrap();
        let mut ids = ["aaa", "aaa", "aaa", "bbb"].into_iter().map(String::from);
        let mut next = || ids.next().unwrap();
        let a = create_drop(&dir, 7, "p.jpg", b"one", &mut next).unwrap();
        let b = create_drop(&dir, 7, "p.jpg", b"two", &mut next).unwrap();
        assert_eq!(a, dir.join("7-aaa-p.jpg"));
        assert_eq!(b, dir.join("7-bbb-p.jpg"));
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        assert_eq!(std::fs::read(&b).unwrap(), b"two");
        let e = create_drop(&dir, 7, "p.jpg", b"three", &mut || "aaa".into()).unwrap_err();
        assert!(e.starts_with("Failed to write file"), "{e}");
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
    }

    #[test]
    fn dropped_file_accepts_saved() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let p = save_dropped_file(&ctx, "aGk=".into(), "photo.jpg".into()).unwrap();
        let canon = std::fs::canonicalize(&p).unwrap();
        assert_eq!(dropped_file(&ctx, &p), Some(canon));
    }

    #[cfg(unix)]
    #[test]
    fn dropped_file_rejects_outside_symlink_and_dir() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let drop = drop_dir(&ctx);
        let inside = save_dropped_file(&ctx, "aGk=".into(), "a.png".into()).unwrap();
        let outside = fx.write("project/b.png", "x");
        let s = |p: &Path| p.to_string_lossy().into_owned();
        // A file elsewhere, and one reached through `..`.
        assert_eq!(dropped_file(&ctx, &s(&outside)), None);
        let dotdot = drop.join("../../project/b.png");
        assert_eq!(dropped_file(&ctx, &s(&dotdot)), None);
        // A symlink in the drop directory, to a file outside it and to a drop.
        let link = drop.join("link.png");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(dropped_file(&ctx, &s(&link)), None);
        let link2 = drop.join("link2.png");
        std::os::unix::fs::symlink(&inside, &link2).unwrap();
        assert_eq!(dropped_file(&ctx, &s(&link2)), None);
        // A directory inside it, and a file in a subdirectory.
        let sub = drop.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(dropped_file(&ctx, &s(&sub)), None);
        std::fs::write(sub.join("c.png"), "x").unwrap();
        assert_eq!(dropped_file(&ctx, &s(&sub.join("c.png"))), None);
        // A subdirectory that is a symlink to elsewhere.
        let alias_out = drop.join("out");
        std::os::unix::fs::symlink(outside.parent().unwrap(), &alias_out).unwrap();
        assert_eq!(dropped_file(&ctx, &s(&alias_out.join("b.png"))), None);
        // A relative path, a missing file, and a hard link to a file elsewhere.
        let name = Path::new(&inside).file_name().unwrap().to_string_lossy();
        assert_eq!(dropped_file(&ctx, &name), None);
        assert_eq!(dropped_file(&ctx, &s(&drop.join("gone.png"))), None);
        let hard = drop.join("hard.png");
        std::fs::hard_link(&outside, &hard).unwrap();
        assert_eq!(dropped_file(&ctx, &s(&hard)), None);
        // The drop itself, also through a symlinked alias of its directory: the canonical
        // path.
        let alias = fx.dir.path().join("alias");
        std::os::unix::fs::symlink(&drop, &alias).unwrap();
        let via = alias.join(Path::new(&inside).file_name().unwrap());
        let canon = std::fs::canonicalize(&inside).unwrap();
        assert_eq!(dropped_file(&ctx, &s(&via)), Some(canon.clone()));
        assert_eq!(dropped_file(&ctx, &inside), Some(canon));
    }
}

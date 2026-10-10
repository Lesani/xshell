use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// Case-insensitive Windows-friendly path equality.
pub fn paths_equal(a: &str, b: &str) -> bool {
    a.replace('\\', "/").to_lowercase() == b.replace('\\', "/").to_lowercase()
}

// Walk up from the project path to find the git repository root. Returns None if we never
// encounter a .git entry — in which case Claude uses the project dir itself for memory storage.
pub fn find_git_root(start: &std::path::Path) -> Option<PathBuf> {
    let mut cur: Option<&std::path::Path> = Some(start);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Whether `path`, below `root`, resolves inside `root`: the path itself or, where it does
/// not exist, its nearest existing ancestor. Only "not found" counts as absent; any other
/// error, a dangling symlink included, refuses.
pub fn stays_inside(root: &Path, path: &Path) -> bool {
    let absent = |p: &Path| {
        p.symlink_metadata()
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
    };
    let real_root = match fs::canonicalize(root) {
        Ok(r) => r,
        // No storage at all: nothing below it to read.
        Err(_) => return absent(root),
    };
    let mut p = path;
    while absent(p) {
        match p.parent() {
            Some(up) if up.starts_with(root) => p = up,
            _ => return false,
        }
    }
    fs::canonicalize(p).is_ok_and(|r| r.starts_with(&real_root))
}

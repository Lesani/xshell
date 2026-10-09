use std::path::PathBuf;

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

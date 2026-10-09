//! Fixture trees for unit tests: a temp dir holding `home/` and `tmp/`.

use crate::ctx::HostCtx;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;

pub(crate) struct Fixture {
    pub dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("home")).unwrap();
        fs::create_dir_all(dir.path().join("tmp")).unwrap();
        Fixture { dir }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub fn ctx(&self) -> HostCtx {
        HostCtx::with_home(self.home(), self.dir.path().join("tmp"))
    }

    /// Write `contents` to `rel` under the fixture root (creating parents) and return its path.
    /// Write `contents` to `rel` under the fixture root. `rel` uses `/` separators; each
    /// component is joined separately so the returned path uses the platform separator,
    /// matching paths the code under test builds with `Path::join`.
    pub fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PathBuf {
        let p = rel
            .as_ref()
            .to_string_lossy()
            .split('/')
            .fold(self.dir.path().to_path_buf(), |acc, c| acc.join(c));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, contents).unwrap();
        p
    }

    /// Write one JSON value per line to `rel` under the fixture root.
    pub fn write_jsonl(&self, rel: impl AsRef<Path>, lines: &[serde_json::Value]) -> PathBuf {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        self.write(rel, body)
    }

    pub fn set_mtime(&self, path: &Path, t: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
}

/// One context item as `(name, detail, path)`.
pub(crate) type ItemView = (String, String, String);

/// Context sections as plain tuples for `assert_eq!`: `(title, [(name, detail, path)])`.
pub(crate) fn sections_view(
    sections: &[crate::agent_context::AgentContextSection],
) -> Vec<(String, Vec<ItemView>)> {
    sections
        .iter()
        .map(|s| {
            let items = s
                .items
                .iter()
                .map(|i| (i.name.clone(), i.detail.clone(), i.path.clone()))
                .collect();
            (s.title.clone(), items)
        })
        .collect()
}

/// `path` as the lossy string the core functions report.
pub(crate) fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Run `git` in `dir` isolated from the developer's system and global config, with an
/// identity and signing off so commits work anywhere. Returns whether it succeeded.
pub(crate) fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
        ])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A fresh repository at `dir` (created if missing) with one commit of `README.md`, or
/// `None` when no usable `git` is on PATH (the caller should then skip). The repo-local
/// config pins what core's own `git` calls depend on, since those inherit the global config:
/// no hooks, no signing, no global excludes file.
pub(crate) fn git_repo(dir: &Path) -> Option<PathBuf> {
    if !std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return None;
    }
    fs::create_dir_all(dir).unwrap();
    let hooks = dir.join(".git").join("no-hooks");
    let ok = git(dir, &["init", "-q"])
        && git(dir, &["config", "commit.gpgsign", "false"])
        && git(dir, &["config", "core.excludesFile", ""])
        && git(dir, &["config", "core.hooksPath", &hooks.to_string_lossy()])
        && {
            fs::write(dir.join("README.md"), "hello\n").unwrap();
            git(dir, &["add", "README.md"])
        }
        && git(dir, &["commit", "-q", "-m", "init"]);
    assert!(ok, "git is installed but setting up a test repo failed");
    Some(dir.to_path_buf())
}

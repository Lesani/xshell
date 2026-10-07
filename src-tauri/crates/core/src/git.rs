use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GitFile {
    pub path: String,
    pub staged: String,   // single char: "M", "A", "D", "R", "C", " ", "?"
    pub unstaged: String, // single char: "M", "D", " ", "?"
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GitStatus {
    pub is_repo: bool,
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub has_upstream: bool,
    pub files: Vec<GitFile>,
}

pub fn parse_porcelain(output: &str) -> GitStatus {
    let mut status = GitStatus {
        is_repo: true,
        branch: String::new(),
        ahead: 0,
        behind: 0,
        has_upstream: false,
        files: vec![],
    };
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // e.g. "main...origin/main [ahead 1, behind 2]", "main", "HEAD (no branch)"
            let (branch_part, tracking_part) = match rest.find(" [") {
                Some(idx) => (&rest[..idx], &rest[idx + 2..rest.len().saturating_sub(1)]),
                None => (rest, ""),
            };
            let branch_name = branch_part
                .split("...")
                .next()
                .unwrap_or(branch_part)
                .to_string();
            status.branch = branch_name;
            status.has_upstream = branch_part.contains("...");
            for token in tracking_part.split(", ") {
                if let Some(n) = token.strip_prefix("ahead ") {
                    status.ahead = n.parse().unwrap_or(0);
                }
                if let Some(n) = token.strip_prefix("behind ") {
                    status.behind = n.parse().unwrap_or(0);
                }
            }
            continue;
        }
        if line.len() < 3 {
            continue;
        }
        let bytes = line.as_bytes();
        let staged = (bytes[0] as char).to_string();
        let unstaged = (bytes[1] as char).to_string();
        let path = line[3..].to_string();
        if path.is_empty() {
            continue;
        }
        status.files.push(GitFile {
            path,
            staged,
            unstaged,
        });
    }
    status
}

pub fn get_git_status(cwd: String) -> GitStatus {
    use std::process::Command;
    let mut cmd = Command::new("git");
    // core.quotePath=false: emit non-ASCII paths verbatim (UTF-8) instead of octal-escaped and
    // wrapped in quotes. Without it, a file like `Prüflast.cs` comes back as `"Pr\303\274..."`,
    // and that quoted string is then passed to `git diff -- <path>`, which finds nothing — so
    // the diff (and the displayed name) breaks for any path with special characters.
    // --untracked-files=all: list each untracked file individually instead of collapsing a
    // wholly-untracked directory into one `dir/` entry (which rendered as an empty-named row
    // in the changes tree). Matches what VS Code's source-control view shows.
    cmd.arg("-c")
        .arg("core.quotePath=false")
        .arg("status")
        .arg("--porcelain=v1")
        .arg("-b")
        .arg("--untracked-files=all")
        .current_dir(&cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    } // CREATE_NO_WINDOW
    match cmd.output() {
        Ok(o) if o.status.success() => parse_porcelain(&String::from_utf8_lossy(&o.stdout)),
        _ => GitStatus {
            is_repo: false,
            branch: String::new(),
            ahead: 0,
            behind: 0,
            has_upstream: false,
            files: vec![],
        },
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GitCommit {
    pub hash: String,
    pub short_hash: String,
    pub subject: String,
    pub author: String,
    pub relative_time: String,
}

pub fn git_cmd(cwd: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd
}

// Repository top-level for `cwd`, or `cwd` itself if it isn't inside a repo. Path-taking git
// commands (diff/add/reset) must run from here because `git status --porcelain` reports paths
// relative to the repo root, while those commands resolve paths relative to the process cwd —
// so a subdirectory cwd would otherwise never match the root-relative paths the UI passes back.
pub fn git_root(cwd: &str) -> String {
    let mut cmd = git_cmd(cwd);
    cmd.arg("rev-parse").arg("--show-toplevel");
    match cmd.output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                cwd.to_string()
            } else {
                s
            }
        }
        _ => cwd.to_string(),
    }
}

pub fn get_git_log(cwd: String, limit: Option<u32>) -> Vec<GitCommit> {
    let n = limit.unwrap_or(20).clamp(1, 200);
    // Use a rare-in-normal-text separator between fields so we don't collide with subjects.
    let mut cmd = git_cmd(&cwd);
    cmd.arg("log")
        .arg(format!("-{}", n))
        .arg("--pretty=format:%H\x1f%h\x1f%s\x1f%an\x1f%cr");
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut commits = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(5, '\x1f');
        let hash = parts.next().unwrap_or("").to_string();
        let short_hash = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        let author = parts.next().unwrap_or("").to_string();
        let relative_time = parts.next().unwrap_or("").to_string();
        if !hash.is_empty() {
            commits.push(GitCommit {
                hash,
                short_hash,
                subject,
                author,
                relative_time,
            });
        }
    }
    commits
}

// Take a path as shown in `git status --porcelain` and return the post-rename path when
// it's a rename entry ("old -> new"); otherwise the path unchanged.
pub fn normalize_git_path(p: &str) -> &str {
    match p.find(" -> ") {
        Some(idx) => &p[idx + 4..],
        None => p,
    }
}

// Unified diff for a single changed file, for the git panel's Diff tab. Mode-specific so a file
// that is both staged and further modified shows two distinct diffs depending on the row clicked:
//   staged    → `git diff --cached` (index vs HEAD)
//   unstaged  → `git diff`          (working tree vs index)
//   untracked → `git diff --no-index /dev/null <file>` (whole file as additions)
// Runs from the repo top-level so the root-relative paths from `git status` resolve even when the
// terminal cwd is a subdirectory. --no-ext-diff ignores external diff drivers (diff.external /
// `.gitattributes` diff=<driver>) that would otherwise print nothing; --no-color keeps ANSI out.
pub fn git_diff(cwd: String, path: String, mode: String) -> Result<String, String> {
    let p = normalize_git_path(&path).to_string();
    let root = git_root(&cwd);
    let mut cmd = git_cmd(&root);
    cmd.arg("diff").arg("--no-ext-diff").arg("--no-color");
    if mode == "untracked" {
        cmd.arg("--no-index").arg("--").arg("/dev/null").arg(&p);
        let out = cmd
            .output()
            .map_err(|e| format!("git diff failed: {}", e))?;
        // --no-index exits 1 when the two inputs differ — the normal "found a diff" signal.
        return match out.status.code() {
            Some(0) | Some(1) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            _ => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        };
    }
    if mode == "staged" {
        cmd.arg("--cached");
    }
    cmd.arg("--").arg(&p);
    let out = cmd
        .output()
        .map_err(|e| format!("git diff failed: {}", e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

pub fn git_stage(cwd: String, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut cmd = git_cmd(&git_root(&cwd)); // root-relative paths from status; run from the top-level
    cmd.arg("add").arg("--");
    for p in &paths {
        cmd.arg(normalize_git_path(p));
    }
    let out = cmd.output().map_err(|e| format!("git add failed: {}", e))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(())
}

pub fn git_unstage(cwd: String, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut cmd = git_cmd(&git_root(&cwd)); // root-relative paths from status; run from the top-level
    cmd.arg("reset").arg("HEAD").arg("--");
    for p in &paths {
        cmd.arg(normalize_git_path(p));
    }
    let out = cmd
        .output()
        .map_err(|e| format!("git reset failed: {}", e))?;
    // `git reset HEAD` exits non-zero when the repo has no commits yet; surface as success
    // with whatever it wrote, because the state change is still effective for staged files.
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("ambiguous argument 'HEAD'") {
            return Err(stderr.to_string());
        }
    }
    Ok(())
}

// Discard a file's changes, scoped to the section it was invoked from so it never clobbers more
// than intended:
//   unstaged  → `git checkout -- <path>`      (restore working tree from the INDEX — drops only
//               the unstaged edits, keeps anything already staged)
//   staged    → `git checkout HEAD -- <path>` (restore index + working tree to HEAD — drops the
//               file's changes entirely)
//   untracked → delete the file
// Destructive and irreversible, so the UI confirms first. Runs from the repo root.
pub fn git_discard(cwd: String, path: String, mode: String) -> Result<(), String> {
    let root = git_root(&cwd);
    let p = normalize_git_path(&path).to_string();
    if mode == "untracked" {
        let full = std::path::Path::new(&root).join(&p);
        return fs::remove_file(&full).map_err(|e| format!("Failed to delete {}: {}", p, e));
    }
    let mut cmd = git_cmd(&root);
    cmd.arg("checkout");
    if mode == "staged" {
        cmd.arg("HEAD");
    } // staged: revert index + worktree to HEAD
      // unstaged (no ref): restore the working tree from the index, leaving staged changes intact
    cmd.arg("--").arg(&p);
    let out = cmd
        .output()
        .map_err(|e| format!("git checkout failed: {}", e))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GitBranch {
    pub name: String,     // short name: "main", "feature/foo"
    pub full_ref: String, // "refs/heads/main" or "refs/remotes/origin/foo"
    pub is_current: bool,
    pub is_remote: bool,
    pub upstream: String, // empty when none
    pub last_commit_subject: String,
    pub last_commit_relative: String,
}

pub fn list_git_branches(cwd: String) -> Vec<GitBranch> {
    // ASCII unit-separator (\x1f) between fields keeps subjects with spaces / tabs intact.
    // Sorted by committer date desc so the dropdown opens with the most-relevant branches up
    // top — current branch + recent feature branches before stale ones.
    let format = "%(refname)\x1f%(refname:short)\x1f%(HEAD)\x1f%(upstream:short)\x1f%(committerdate:relative)\x1f%(subject)";
    let mut cmd = git_cmd(&cwd);
    cmd.arg("for-each-ref")
        .arg("--sort=-committerdate")
        .arg(format!("--format={}", format))
        .arg("refs/heads/")
        .arg("refs/remotes/");
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut branches = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(6, '\x1f');
        let full_ref = parts.next().unwrap_or("").to_string();
        let name = parts.next().unwrap_or("").to_string();
        let head_marker = parts.next().unwrap_or("");
        let upstream = parts.next().unwrap_or("").to_string();
        let last_commit_relative = parts.next().unwrap_or("").to_string();
        let last_commit_subject = parts.next().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        // Skip `origin/HEAD` symbolic ref — it's a pointer, not a real branch the user can switch to.
        if full_ref.ends_with("/HEAD") {
            continue;
        }
        let is_remote = full_ref.starts_with("refs/remotes/");
        let is_current = head_marker == "*";
        branches.push(GitBranch {
            name,
            full_ref,
            is_current,
            is_remote,
            upstream,
            last_commit_subject,
            last_commit_relative,
        });
    }
    branches
}

pub fn git_checkout(cwd: String, branch: String) -> Result<(), String> {
    // For remote refs we hand `git switch` the bare branch name (e.g. "feature/foo" not
    // "origin/feature/foo"). Since git 2.23, `switch <name>` does DWIM: if no local branch
    // exists but exactly one remote tracks it, git creates the local branch + sets upstream.
    let target = branch
        .strip_prefix("origin/")
        .unwrap_or(&branch)
        .to_string();
    let mut cmd = git_cmd(&cwd);
    cmd.arg("switch").arg(&target);
    let out = cmd
        .output()
        .map_err(|e| format!("git switch failed: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Empty stderr can happen on truly-fatal git errors; give the user *something* useful.
        return Err(if stderr.is_empty() {
            format!("git switch exited with status {}", out.status)
        } else {
            stderr
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_reads_branch_tracking_and_files() {
        let out = "## main...origin/main [ahead 2, behind 1]\n M src/a.rs\nA  b.rs\n?? new file.txt\nR  old.rs -> new.rs\n";
        let s = parse_porcelain(out);
        assert!(s.is_repo);
        assert_eq!(s.branch, "main");
        assert!(s.has_upstream);
        assert_eq!(s.ahead, 2);
        assert_eq!(s.behind, 1);
        let files: Vec<(&str, &str, &str)> = s
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.staged.as_str(), f.unstaged.as_str()))
            .collect();
        assert_eq!(
            files,
            vec![
                ("src/a.rs", " ", "M"),
                ("b.rs", "A", " "),
                ("new file.txt", "?", "?"),
                // The rename target is extracted later, by normalize_git_path.
                ("old.rs -> new.rs", "R", " "),
            ]
        );
    }

    #[test]
    fn parse_porcelain_without_upstream() {
        let s = parse_porcelain("## HEAD (no branch)");
        assert_eq!(s.branch, "HEAD (no branch)");
        assert!(!s.has_upstream);
        assert_eq!((s.ahead, s.behind), (0, 0));
        assert!(s.files.is_empty());
    }

    #[test]
    fn normalize_git_path_takes_rename_target() {
        assert_eq!(normalize_git_path("a -> b"), "b");
        assert_eq!(normalize_git_path("c"), "c");
    }

    #[test]
    fn git_status_stage_unstage_round_trip() {
        let fx = crate::testutil::Fixture::new();
        let Some(repo) = crate::testutil::git_repo(&fx.dir.path().join("repo")) else {
            eprintln!("git not available; skipping");
            return;
        };
        let cwd = repo.to_string_lossy().to_string();
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("new.txt"), "new\n").unwrap();
        std::fs::write(repo.join("README.md"), "changed\n").unwrap();
        let files = |s: &GitStatus| -> Vec<(String, String, String)> {
            let mut v: Vec<_> = s
                .files
                .iter()
                .map(|f| (f.path.clone(), f.staged.clone(), f.unstaged.clone()))
                .collect();
            v.sort();
            v
        };
        let row = |p: &str, a: &str, b: &str| (p.to_string(), a.to_string(), b.to_string());

        let s = get_git_status(cwd.clone());
        assert!(s.is_repo);
        assert_eq!(s.branch, "main");
        assert!(!s.has_upstream);
        assert_eq!(
            files(&s),
            [row("README.md", " ", "M"), row("sub/new.txt", "?", "?")]
        );

        // Stage from a subdirectory cwd: paths are repo-root-relative, as status reports them.
        let sub_cwd = sub.to_string_lossy().to_string();
        git_stage(
            sub_cwd.clone(),
            vec!["sub/new.txt".into(), "README.md".into()],
        )
        .unwrap();
        assert_eq!(
            files(&get_git_status(cwd.clone())),
            [row("README.md", "M", " "), row("sub/new.txt", "A", " ")]
        );
        let staged = git_diff(cwd.clone(), "README.md".into(), "staged".into()).unwrap();
        assert!(staged.contains("+changed"), "{staged}");

        git_unstage(sub_cwd, vec!["sub/new.txt".into(), "README.md".into()]).unwrap();
        assert_eq!(
            files(&get_git_status(cwd.clone())),
            [row("README.md", " ", "M"), row("sub/new.txt", "?", "?")]
        );

        let log = get_git_log(cwd.clone(), None);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].subject, "init");

        // Not a repo.
        let plain = fx.dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // The fixture lives in the system temp dir, which is not inside a repository.
        let s = get_git_status(plain.to_string_lossy().to_string());
        assert!(!s.is_repo);
        assert!(s.files.is_empty());
    }
}

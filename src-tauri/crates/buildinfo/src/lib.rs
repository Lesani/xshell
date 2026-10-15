//! The build identity of xshell and xshelld (Lesani/xshell#49), computed by their build
//! scripts. A build is the commit it was built from, a digest of the uncommitted changes (if
//! any) and the commit time. The Desktop and xshelld built from one tree get the same identity.
//!
//! Each build script calls [`emit`], which sets `XSHELL_BUILDINFO_STAMP` for the crate:
//! `@@XSHELL-BUILD-V1:<display>:<time>@@`, or `@@XSHELL-BUILD-V1:unknown@@` when there is no
//! git checkout. `xshell_protocol::build` parses it, also out of a compiled binary.
//!
//! **What reruns the build scripts.** A build script that reruns on every build makes cargo
//! recompile its crate on every build, even when the identity did not change (checked for both
//! crates in #49). So both scripts watch one identical list instead: the git files that move
//! with a commit or checkout (`HEAD`, the branch ref, `packed-refs`, `index`) and the source
//! paths in [`WATCHED`]. The digest of uncommitted changes covers exactly [`WATCHED`] too, so
//! any change that alters the identity reruns both scripts on their next build. The trade-off:
//! uncommitted changes outside [`WATCHED`] (docs, for example) are not part of the identity.
//!
//! **Inputs besides the tree.** git runs without the user's and the system's configuration and
//! global ignore/attribute files, with the diff format pinned and line endings normalised
//! (`core.autocrlf=input`; untracked text files are hashed with CRLF turned into LF), so one
//! tree gives one identity across clones, users and platforms. What remains is watched: the
//! repository's `config` (and `config.worktree`), `info/exclude`, `info/attributes`, and the
//! `.gitignore`/`.gitattributes` files at the root and in `src-tauri/`. Of these, only existing
//! files can be watched (the root and `src-tauri/` hold the build output), so creating a new
//! `src-tauri/.gitignore` or `.gitattributes` goes unnoticed until another watched input
//! changes.
//!
//! **Overrides** (CI, source tarballs): `XSHELL_BUILD_COMMIT` (12 lowercase hex, or `unknown`),
//! `XSHELL_BUILD_TIME` (commit unix seconds) and `XSHELL_BUILD_DIRTY` (absent or empty for a
//! clean tree, else 12 lowercase hex). With them no git is run. An invalid value fails the
//! build.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The paths (relative to the repository root) that make up a build: watched by both build
/// scripts, and the only paths the digest of uncommitted changes looks at.
pub const WATCHED: &[&str] = &[
    "index.html",
    "package.json",
    "src",
    "src-tauri/Cargo.lock",
    "src-tauri/Cargo.toml",
    "src-tauri/build.rs",
    "src-tauri/capabilities",
    "src-tauri/crates",
    "src-tauri/src",
    "src-tauri/tauri.conf.json",
    "src-tauri/tauri.daemon.conf.json",
    "src-tauri/vendor",
];

pub const ENV_COMMIT: &str = "XSHELL_BUILD_COMMIT";
pub const ENV_DIRTY: &str = "XSHELL_BUILD_DIRTY";
pub const ENV_TIME: &str = "XSHELL_BUILD_TIME";
/// The variable [`emit`] sets for the crate. Its name differs from the overrides, so `env!`
/// never picks up a user-set `XSHELL_BUILD_COMMIT` by mistake.
pub const OUT_ENV: &str = "XSHELL_BUILDINFO_STAMP";
pub const STAMP_PREFIX: &str = "@@XSHELL-BUILD-V1:";

/// One build's identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// The first 12 lowercase hex digits of the commit.
    pub commit: String,
    /// 12 lowercase hex digits of the digest of the uncommitted changes; `None` when clean.
    pub dirty: Option<String>,
    /// The commit's time, unix seconds. Diagnostic only.
    pub time: u64,
}

/// `85ee920a1b2c`, or `85ee920a1b2c+d.3f9a01c2b4e5` with uncommitted changes.
pub fn display(s: &Stamp) -> String {
    match &s.dirty {
        Some(d) => format!("{}+d.{d}", s.commit),
        None => s.commit.clone(),
    }
}

/// The string embedded in the binary: `@@XSHELL-BUILD-V1:<display>:<time>@@`, or
/// `@@XSHELL-BUILD-V1:unknown@@`.
pub fn stamp_string(s: Option<&Stamp>) -> String {
    match s {
        Some(s) => format!("{STAMP_PREFIX}{}:{}@@", display(s), s.time),
        None => format!("{STAMP_PREFIX}unknown@@"),
    }
}

/// What [`resolve_full`] found: the identity (or why it is unknown) and the files to watch.
#[derive(Debug)]
pub struct Resolution {
    pub stamp: Option<Stamp>,
    /// Why `stamp` is `None`.
    pub unknown: Option<String>,
    /// Existing files and directories whose change can change the identity.
    pub watch: Vec<PathBuf>,
}

/// The identity of the tree that contains `manifest_dir`. `Err` only for an invalid override.
pub fn resolve(
    manifest_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Stamp>, String> {
    resolve_full(manifest_dir, env).map(|r| r.stamp)
}

pub fn resolve_full(
    manifest_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Resolution, String> {
    if let Some(stamp) = from_overrides(env)? {
        return Ok(Resolution {
            unknown: stamp.is_none().then(|| format!("{ENV_COMMIT}=unknown")),
            stamp,
            watch: Vec::new(),
        });
    }
    Ok(from_git(manifest_dir))
}

/// `Ok(None)`: no override set. `Ok(Some(None))`: `XSHELL_BUILD_COMMIT=unknown`.
fn from_overrides(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<Option<Stamp>>, String> {
    let (commit, dirty, time) = (env(ENV_COMMIT), env(ENV_DIRTY), env(ENV_TIME));
    let Some(commit) = commit else {
        for (name, v) in [(ENV_DIRTY, &dirty), (ENV_TIME, &time)] {
            if let Some(v) = v {
                return Err(format!("{name}={v:?} is set without {ENV_COMMIT}"));
            }
        }
        return Ok(None);
    };
    if commit == "unknown" {
        // Nothing else is used, but a malformed value is still a mistake worth failing on.
        if let Some(d) = dirty.as_deref().filter(|d| !d.is_empty() && !is_hex12(d)) {
            return Err(format!(
                "{ENV_DIRTY}={d:?}: expected nothing or 12 lowercase hex digits"
            ));
        }
        if let Some(t) = time.as_deref().filter(|t| parse_time(t).is_none()) {
            return Err(format!("{ENV_TIME}={t:?}: expected unix seconds"));
        }
        return Ok(Some(None));
    }
    if !is_hex12(&commit) {
        return Err(format!(
            "{ENV_COMMIT}={commit:?}: expected 12 lowercase hex digits or `unknown`"
        ));
    }
    let dirty = match dirty.as_deref() {
        None | Some("") => None,
        Some(d) if is_hex12(d) => Some(d.to_string()),
        Some(d) => {
            return Err(format!(
                "{ENV_DIRTY}={d:?}: expected nothing or 12 lowercase hex digits"
            ))
        }
    };
    let time = match time {
        None => return Err(format!("{ENV_TIME} is required with {ENV_COMMIT}")),
        Some(t) => {
            parse_time(&t).ok_or_else(|| format!("{ENV_TIME}={t:?}: expected unix seconds"))?
        }
    };
    Ok(Some(Some(Stamp {
        commit,
        dirty,
        time,
    })))
}

fn is_hex12(s: &str) -> bool {
    s.len() == 12 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_time(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Configuration that would otherwise come from outside the repository (or vary by user):
/// no global ignore/attribute files, line endings normalised to LF on the way in (as on a
/// default Linux or macOS checkout, so a Windows CRLF checkout of the same tree agrees), no
/// fsmonitor hook, plain paths.
const GIT_CONFIG: &[&str] = &[
    "core.excludesFile=/dev/null",
    "core.attributesFile=/dev/null",
    "core.autocrlf=input",
    "core.safecrlf=false",
    "core.fsmonitor=false",
    "core.quotePath=off",
];

/// The pinned diff format: the digest hashes these bytes.
const DIFF_ARGS: &[&str] = &[
    "--binary",
    "--full-index",
    "--unified=3",
    "--diff-algorithm=myers",
    "--no-indent-heuristic",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for kv in GIT_CONFIG {
        c.args(["-c", kv]);
    }
    // Never rewrite the index (a `git status` style refresh); stable output.
    c.env("GIT_OPTIONAL_LOCKS", "0").env("LC_ALL", "C");
    // Only the repository's own configuration (watched): not the user's or the system's.
    // `/dev/null` works with Git for Windows too.
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    // A build run from a git hook inherits these; discovery from `-C` must win. Config
    // injected through the environment is not an input either.
    for v in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_ATTR_NOSYSTEM",
    ] {
        c.env_remove(v);
    }
    c
}

fn run(mut c: Command) -> Result<Vec<u8>, String> {
    let out = c.output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git: {}", err.trim()));
    }
    Ok(out.stdout)
}

fn run_line(c: Command) -> Result<String, String> {
    let out = run(c)?;
    Ok(String::from_utf8_lossy(&out)
        .trim_end_matches(['\n', '\r'])
        .to_string())
}

fn from_git(manifest_dir: &Path) -> Resolution {
    let mut c = git(manifest_dir);
    c.args(["rev-parse", "--show-toplevel"]);
    let top = match run_line(c) {
        Ok(t) => PathBuf::from(t),
        Err(e) => {
            return Resolution {
                stamp: None,
                unknown: Some(format!("not a git checkout ({e})")),
                watch: Vec::new(),
            }
        }
    };
    let watch = watch_list(&top);
    match identity(&top) {
        Ok(stamp) => Resolution {
            stamp: Some(stamp),
            unknown: None,
            watch,
        },
        Err(e) => Resolution {
            stamp: None,
            unknown: Some(e),
            watch,
        },
    }
}

fn identity(top: &Path) -> Result<Stamp, String> {
    let mut c = git(top);
    c.args(["rev-parse", "HEAD"]);
    let head = run_line(c)?;
    if head.len() < 12 || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("unexpected HEAD {head:?}"));
    }
    let commit = head[..12].to_ascii_lowercase();
    let mut c = git(top);
    c.args(["log", "-1", "--format=%ct", "HEAD"]);
    let t = run_line(c)?;
    let time = parse_time(&t).ok_or_else(|| format!("unexpected commit time {t:?}"))?;

    let mut c = git(top);
    c.args(["--no-pager", "diff", "HEAD"])
        .args(DIFF_ARGS)
        .arg("--")
        .args(WATCHED);
    let diff = run(c)?;
    let mut c = git(top);
    c.args(["ls-files", "--others", "--exclude-standard", "-z", "--"])
        .args(WATCHED);
    let listed = run(c)?;
    let mut untracked: Vec<(Vec<u8>, Vec<u8>)> = listed
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| (p.to_vec(), entry_content(&top.join(path_of(p)))))
        .collect();
    untracked.sort();
    let dirty =
        (!diff.is_empty() || !untracked.is_empty()).then(|| dirty_digest(&diff, &untracked));
    Ok(Stamp {
        commit,
        dirty,
        time,
    })
}

#[cfg(unix)]
fn path_of(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// An untracked entry as the digest sees it: a tag byte, then the data. `F` + contents for a
/// file (CRLF turned into LF when it has no NUL byte, like git's text detection; binary files
/// as they are), `L` + target for a symlink, `D` for a directory (an untracked nested
/// repository), `E` when unreadable.
fn entry_content(p: &Path) -> Vec<u8> {
    let tagged = |tag: u8, data: &[u8]| {
        let mut v = Vec::with_capacity(data.len() + 1);
        v.push(tag);
        v.extend_from_slice(data);
        v
    };
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => match std::fs::read_link(p) {
            Ok(t) => tagged(b'L', t.to_string_lossy().as_bytes()),
            Err(_) => vec![b'E'],
        },
        Ok(m) if m.is_dir() => vec![b'D'],
        Ok(_) => match std::fs::read(p) {
            Ok(data) => tagged(b'F', &normalize_text(data)),
            Err(_) => vec![b'E'],
        },
        Err(_) => vec![b'E'],
    }
}

/// A file without a NUL byte is text: CRLF becomes LF. Others stay as they are.
pub fn normalize_text(data: Vec<u8>) -> Vec<u8> {
    if data.contains(&0) || !data.windows(2).any(|w| w == b"\r\n") {
        return data;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut it = data.iter().peekable();
    while let Some(&b) = it.next() {
        if b == b'\r' && it.peek() == Some(&&b'\n') {
            continue;
        }
        out.push(b);
    }
    out
}

/// 12 hex digits of sha256 over `xshell-dirty-v1\0`, the length-prefixed diff, then each
/// untracked entry (sorted by path bytes) as path, `\0`, length-prefixed tagged content.
/// Depends only on the tree, so both build scripts compute the same value.
pub fn dirty_digest(diff: &[u8], untracked: &[(Vec<u8>, Vec<u8>)]) -> String {
    let mut sorted: Vec<&(Vec<u8>, Vec<u8>)> = untracked.iter().collect();
    sorted.sort();
    let mut h = Sha256::new();
    h.update(b"xshell-dirty-v1\0");
    h.update((diff.len() as u64).to_le_bytes());
    h.update(diff);
    for (path, content) in sorted {
        h.update(path);
        h.update([0]);
        h.update((content.len() as u64).to_le_bytes());
        h.update(content);
    }
    h.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The files to watch: the git files a commit or checkout changes, and the existing paths of
/// [`WATCHED`]. Only existing paths: cargo reruns a script whose watched path is missing on
/// every build. Git files that may appear later (a loose branch ref) are watched through their
/// nearest existing parent directory.
pub fn watch_list(top: &Path) -> Vec<PathBuf> {
    let git_path = |name: &str| -> Option<PathBuf> {
        let mut c = git(top);
        c.args(["rev-parse", "--git-path", name]);
        let p = PathBuf::from(run_line(c).ok()?);
        Some(if p.is_absolute() { p } else { top.join(p) })
    };
    let mut out = Vec::new();
    // HEAD, index: per worktree, and they exist in any checkout with a commit.
    for name in ["HEAD", "index"] {
        if let Some(p) = git_path(name).filter(|p| p.exists()) {
            out.push(p);
        }
    }
    // The branch ref: loose (a file) or packed. When it is loose, a later `git pack-refs`
    // deletes it, which cargo notices; when it is packed, a commit writes the loose file,
    // which shows in its parent directory. Detached HEAD: `HEAD` holds the commit itself.
    let mut c = git(top);
    c.args(["symbolic-ref", "-q", "HEAD"]);
    if let Ok(r) = run_line(c) {
        if !r.is_empty() {
            if let Some(p) = git_path(&r) {
                out.extend(existing_or_parent(&p));
            }
        }
    }
    // What else decides the digest: the repository's configuration, its ignore and attribute
    // files. `info/` files that do not exist yet are watched through `info/` (small) when it
    // exists; the config files always exist when they matter.
    for name in ["packed-refs", "config", "config.worktree"] {
        if let Some(p) = git_path(name).filter(|p| p.exists()) {
            out.push(p);
        }
    }
    for name in ["info/exclude", "info/attributes"] {
        if let Some(p) = git_path(name) {
            if p.exists() {
                out.push(p);
            } else if let Some(info) = p.parent().filter(|i| i.is_dir()) {
                out.push(info.to_path_buf());
            }
        }
    }
    for f in [
        ".gitignore",
        ".gitattributes",
        "src-tauri/.gitignore",
        "src-tauri/.gitattributes",
    ] {
        let p = top.join(f);
        if p.exists() {
            out.push(p);
        }
    }
    for w in WATCHED {
        let p = top.join(w);
        if p.exists() {
            out.push(p);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `p`, or its nearest existing ancestor.
fn existing_or_parent(p: &Path) -> Option<PathBuf> {
    p.ancestors().find(|a| a.exists()).map(Path::to_path_buf)
}

/// The whole body of a build script: resolve the identity of the crate's tree, set
/// `XSHELL_BUILDINFO_STAMP`, and print the rerun rules. Panics on an invalid override.
pub fn emit() {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    for v in [ENV_COMMIT, ENV_DIRTY, ENV_TIME] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    let env = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
    let r = match resolve_full(Path::new(&manifest), &env) {
        Ok(r) => r,
        Err(e) => panic!("invalid build identity override: {e}"),
    };
    for p in &r.watch {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    if let Some(why) = &r.unknown {
        println!("cargo:warning=build identity unknown: {why}");
    }
    println!(
        "cargo:rustc-env={OUT_ENV}={}",
        stamp_string(r.stamp.as_ref())
    );
}

#[cfg(test)]
mod tests;

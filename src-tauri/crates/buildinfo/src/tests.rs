use super::*;
use std::collections::HashMap;
use std::fs;

fn no_env(_: &str) -> Option<String> {
    None
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// An empty file standing in for the user's git config (portable, unlike `/dev/null`).
fn empty_config() -> PathBuf {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        // One fixed name: always empty, so concurrent runs agree and nothing piles up.
        let p = std::env::temp_dir().join("xshell-buildinfo-empty-gitconfig");
        fs::write(&p, "").unwrap();
        p
    })
    .clone()
}

/// git with a neutral identity and no user or system config that could sign or run hooks.
fn g(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", empty_config())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_COMMITTER_DATE", "1760000000 +0000")
        .env("GIT_AUTHOR_DATE", "1760000000 +0000")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

/// A repository shaped like xshell, with one commit.
fn repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    g(dir, &["init", "-q"]);
    write(
        &dir.join(".gitignore"),
        "node_modules/\nsrc-tauri/target/\n",
    );
    write(&dir.join("package.json"), "{}\n");
    write(&dir.join("src/main.ts"), "export {};\n");
    write(&dir.join("src-tauri/src/main.rs"), "fn main() {}\n");
    write(
        &dir.join("src-tauri/crates/xshelld/src/main.rs"),
        "fn main() {}\n",
    );
    write(&dir.join("docs/readme.md"), "docs\n");
    g(dir, &["add", "-A"]);
    g(dir, &["commit", "-q", "-m", "init"]);
}

fn stamp(dir: &Path) -> Stamp {
    resolve(dir, &no_env).unwrap().expect("a known build")
}

#[test]
fn clean_tree_has_no_digest_and_the_commit_time() {
    if !have_git() {
        return;
    }
    let t = tempfile_dir();
    repo(&t);
    let s = stamp(&t);
    assert_eq!(s.dirty, None);
    assert_eq!(s.time, 1760000000);
    assert_eq!(s.commit.len(), 12);
    assert!(is_hex12(&s.commit));
}

#[test]
fn edits_change_the_digest_deterministically() {
    if !have_git() {
        return;
    }
    let (a, b) = (tempfile_dir(), tempfile_dir());
    repo(&a);
    // Same content and dates: the same commit.
    repo(&b);
    assert_eq!(stamp(&a), stamp(&b));
    write(&a.join("src-tauri/src/main.rs"), "fn main() { 1; }\n");
    write(&b.join("src-tauri/src/main.rs"), "fn main() { 1; }\n");
    let (sa, sb) = (stamp(&a), stamp(&b));
    assert!(sa.dirty.is_some());
    assert_eq!(sa, sb, "the same edit in two clones");
    write(&b.join("src-tauri/src/main.rs"), "fn main() { 2; }\n");
    assert_ne!(stamp(&b).dirty, sa.dirty, "different edits");
    assert_eq!(stamp(&b).commit, sa.commit);
}

#[test]
fn untracked_files_count_ignored_and_unwatched_ones_do_not() {
    if !have_git() {
        return;
    }
    let t = tempfile_dir();
    repo(&t);
    write(&t.join("src/node_modules/x.js"), "ignored\n");
    write(&t.join("src-tauri/target/debug/x"), "ignored\n");
    write(&t.join("docs/new.md"), "outside WATCHED\n");
    write(&t.join("docs/readme.md"), "edited outside WATCHED\n");
    assert_eq!(stamp(&t).dirty, None);
    write(&t.join("src-tauri/crates/new/lib.rs"), "new\n");
    let one = stamp(&t).dirty.expect("an untracked file is a change");
    write(&t.join("src-tauri/crates/new/lib.rs"), "other\n");
    let two = stamp(&t).dirty.unwrap();
    assert_ne!(one, two, "the untracked file's content counts");
}

#[test]
fn subdirectories_of_one_tree_agree() {
    if !have_git() {
        return;
    }
    let t = tempfile_dir();
    repo(&t);
    write(&t.join("src/edit.ts"), "x\n");
    let desktop = resolve_full(&t.join("src-tauri"), &no_env).unwrap();
    let daemon = resolve_full(&t.join("src-tauri/crates/xshelld"), &no_env).unwrap();
    assert_eq!(desktop.stamp, daemon.stamp);
    assert!(desktop.stamp.unwrap().dirty.is_some());
    assert_eq!(desktop.watch, daemon.watch, "both scripts watch one list");
}

#[test]
fn watch_list_has_the_git_files_and_existing_sources_only() {
    if !have_git() {
        return;
    }
    let t = tempfile_dir();
    repo(&t);
    let w = watch_list(&t);
    let has = |s: &str| w.iter().any(|p| p.ends_with(s));
    assert!(has(".git/HEAD"), "{w:?}");
    assert!(has(".git/index"), "{w:?}");
    assert!(has(".git/refs/heads/main"), "{w:?}");
    assert!(has("src-tauri/src") && has("src") && has("package.json"));
    assert!(w.iter().all(|p| p.exists()), "{w:?}");
    assert!(!has("index.html"), "missing paths are not watched");
    // Packed: the loose ref is gone, so its existing parent directory is watched instead.
    g(&t, &["pack-refs", "--all"]);
    let w = watch_list(&t);
    let has = |s: &str| w.iter().any(|p| p.ends_with(s));
    assert!(has(".git/packed-refs"), "{w:?}");
    assert!(has(".git/refs/heads"), "{w:?}");
    assert!(w.iter().all(|p| p.exists()), "{w:?}");
}

#[test]
fn a_worktree_resolves() {
    if !have_git() {
        return;
    }
    let t = tempfile_dir();
    repo(&t);
    let holder = tempfile_dir();
    let wt = holder.join("wt");
    g(
        &t,
        &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
    );
    assert_eq!(stamp(&wt), stamp(&t));
    let w = watch_list(&wt);
    assert!(w.iter().any(|p| p.ends_with("refs/heads/side")), "{w:?}");
    assert!(w.iter().all(|p| p.exists()), "{w:?}");
}

/// A tracked file long enough that the diff context matters.
fn long_file() -> String {
    (0..40).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn local_diff_and_ignore_config_does_not_change_the_digest() {
    if !have_git() {
        return;
    }
    let (a, b) = (tempfile_dir(), tempfile_dir());
    for t in [&a, &b] {
        repo(t);
        write(&t.join("src/long.ts"), &long_file());
        g(t, &["add", "-A"]);
        g(t, &["commit", "-q", "-m", "long"]);
        write(
            &t.join("src/long.ts"),
            &long_file().replace("line 20\n", "line 20 changed\n"),
        );
        write(&t.join("src/new.ts"), "new\n");
    }
    // Settings that change `git diff` output or the ignore rules, in one clone only.
    for kv in [
        "diff.context=0",
        "diff.algorithm=patience",
        "diff.indentHeuristic=false",
        "diff.noprefix=true",
        "diff.mnemonicPrefix=true",
        "color.diff=always",
        "core.autocrlf=true",
    ] {
        let (k, v) = kv.split_once('=').unwrap();
        g(&b, &["config", k, v]);
    }
    write(&b.join("excludes"), "new.ts\n");
    g(
        &b,
        &[
            "config",
            "core.excludesFile",
            b.join("excludes").to_str().unwrap(),
        ],
    );
    let (sa, sb) = (stamp(&a), stamp(&b));
    assert!(sa.dirty.is_some());
    assert_eq!(sa.dirty, sb.dirty);
}

#[test]
fn line_endings_do_not_change_the_digest() {
    if !have_git() {
        return;
    }
    let (a, b) = (tempfile_dir(), tempfile_dir());
    repo(&a);
    repo(&b);
    // Untracked text, LF vs CRLF; a binary file (NUL byte) keeps its bytes.
    write(&a.join("src/new.ts"), "a\nb\n");
    write(&b.join("src/new.ts"), "a\r\nb\r\n");
    // A tracked edit, LF vs CRLF (a Windows checkout).
    write(&a.join("src/main.ts"), "export const x = 1;\n");
    write(&b.join("src/main.ts"), "export const x = 1;\r\n");
    assert_eq!(stamp(&a).dirty, stamp(&b).dirty);
    fs::write(a.join("src/bin.dat"), b"a\r\n\0").unwrap();
    fs::write(b.join("src/bin.dat"), b"a\n\0").unwrap();
    assert_ne!(
        stamp(&a).dirty,
        stamp(&b).dirty,
        "binary bytes count as they are"
    );
    assert_eq!(normalize_text(b"a\r\nb\rc\r\n".to_vec()), b"a\nb\rc\n");
    assert_eq!(normalize_text(b"a\r\n\0".to_vec()), b"a\r\n\0");
}

#[test]
fn ignore_rules_are_watched_inputs() {
    if !have_git() {
        return;
    }
    let t = tempfile_dir();
    repo(&t);
    write(&t.join(".gitattributes"), "*.png binary\n");
    write(&t.join("src/new.ts"), "new\n");
    let before = stamp(&t).dirty;
    assert!(before.is_some());
    // Ignoring the file through the root .gitignore changes the identity...
    let gi = fs::read_to_string(t.join(".gitignore")).unwrap();
    write(&t.join(".gitignore"), &format!("{gi}new.ts\n"));
    assert_eq!(stamp(&t).dirty, None);
    // ...so every file that can do that is watched.
    let w = watch_list(&t);
    let has = |s: &str| w.iter().any(|p| p.ends_with(s));
    for f in [".gitignore", ".gitattributes", ".git/config", ".git/info"] {
        assert!(has(f) || has(&format!("{f}/exclude")), "{f}: {w:?}");
    }
    assert!(w.iter().all(|p| p.exists()), "{w:?}");
    let mut sorted = w.clone();
    sorted.dedup();
    assert_eq!(sorted, w, "no duplicates");
}

#[test]
fn no_repository_is_unknown() {
    let t = tempfile_dir();
    let r = resolve_full(&t, &no_env).unwrap();
    assert_eq!(r.stamp, None);
    assert!(r.unknown.is_some());
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| m.get(k).cloned()
}

#[test]
fn overrides() {
    let nowhere = Path::new("/nonexistent-xshell-buildinfo");
    let ok = |pairs: &[(&str, &str)]| resolve(nowhere, &env_of(pairs)).unwrap();
    let err = |pairs: &[(&str, &str)]| resolve(nowhere, &env_of(pairs)).unwrap_err();
    assert_eq!(
        ok(&[(ENV_COMMIT, "85ee920a1b2c"), (ENV_TIME, "1760000000")]),
        Some(Stamp {
            commit: "85ee920a1b2c".into(),
            dirty: None,
            time: 1760000000
        })
    );
    assert_eq!(
        ok(&[
            (ENV_COMMIT, "85ee920a1b2c"),
            (ENV_TIME, "1"),
            (ENV_DIRTY, "")
        ])
        .unwrap()
        .dirty,
        None
    );
    assert_eq!(
        ok(&[
            (ENV_COMMIT, "85ee920a1b2c"),
            (ENV_TIME, "1"),
            (ENV_DIRTY, "3f9a01c2b4e5")
        ])
        .unwrap()
        .dirty
        .as_deref(),
        Some("3f9a01c2b4e5")
    );
    assert_eq!(ok(&[(ENV_COMMIT, "unknown")]), None);
    assert_eq!(
        ok(&[(ENV_COMMIT, "unknown"), (ENV_TIME, "1"), (ENV_DIRTY, "")]),
        None
    );
    // `unknown` uses nothing else, but still refuses malformed companions.
    assert!(err(&[(ENV_COMMIT, "unknown"), (ENV_DIRTY, "xyz")]).contains(ENV_DIRTY));
    assert!(err(&[(ENV_COMMIT, "unknown"), (ENV_TIME, "soon")]).contains(ENV_TIME));
    assert!(err(&[(ENV_COMMIT, "unknown"), (ENV_TIME, "")]).contains(ENV_TIME));
    assert!(err(&[(ENV_COMMIT, "85EE920A1B2C"), (ENV_TIME, "1")]).contains(ENV_COMMIT));
    assert!(err(&[(ENV_COMMIT, "85ee920a1b"), (ENV_TIME, "1")]).contains(ENV_COMMIT));
    assert!(err(&[(ENV_COMMIT, ""), (ENV_TIME, "1")]).contains(ENV_COMMIT));
    assert!(err(&[(ENV_COMMIT, "85ee920a1b2c"), (ENV_TIME, "x1")]).contains(ENV_TIME));
    assert!(err(&[(ENV_COMMIT, "85ee920a1b2c"), (ENV_TIME, "")]).contains(ENV_TIME));
    assert!(err(&[(ENV_COMMIT, "85ee920a1b2c")]).contains(ENV_TIME));
    assert!(err(&[
        (ENV_COMMIT, "85ee920a1b2c"),
        (ENV_TIME, "1"),
        (ENV_DIRTY, "xyz")
    ])
    .contains(ENV_DIRTY));
    assert!(err(&[(ENV_DIRTY, "3f9a01c2b4e5")]).contains(ENV_COMMIT));
    assert!(err(&[(ENV_TIME, "1")]).contains(ENV_COMMIT));
}

#[test]
fn display_and_stamp_string() {
    let s = Stamp {
        commit: "85ee920a1b2c".into(),
        dirty: None,
        time: 1760000000,
    };
    assert_eq!(display(&s), "85ee920a1b2c");
    assert_eq!(
        stamp_string(Some(&s)),
        "@@XSHELL-BUILD-V1:85ee920a1b2c:1760000000@@"
    );
    let d = Stamp {
        dirty: Some("3f9a01c2b4e5".into()),
        ..s
    };
    assert_eq!(display(&d), "85ee920a1b2c+d.3f9a01c2b4e5");
    assert_eq!(
        stamp_string(Some(&d)),
        "@@XSHELL-BUILD-V1:85ee920a1b2c+d.3f9a01c2b4e5:1760000000@@"
    );
    assert_eq!(stamp_string(None), "@@XSHELL-BUILD-V1:unknown@@");
}

#[test]
fn dirty_digest_known_answer() {
    // sha256("xshell-dirty-v1\0" ‖ u64le(4) ‖ "diff" ‖ "a\0" ‖ u64le(2) ‖ "Fx"), first 6 bytes.
    let d = dirty_digest(b"diff", &[(b"a".to_vec(), b"Fx".to_vec())]);
    let mut h = Sha256::new();
    h.update(b"xshell-dirty-v1\0");
    h.update(4u64.to_le_bytes());
    h.update(b"diff");
    h.update(b"a\0");
    h.update(2u64.to_le_bytes());
    h.update(b"Fx");
    let want: String = h.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(d, want);
    assert_eq!(
        d, "678cc8224a01",
        "pinned: the digest is part of the identity"
    );
    // Order of the untracked list does not matter; contents and paths do.
    let a = (b"a".to_vec(), b"F1".to_vec());
    let b = (b"b".to_vec(), b"F2".to_vec());
    assert_eq!(
        dirty_digest(b"", &[a.clone(), b.clone()]),
        dirty_digest(b"", &[b, a])
    );
    assert_ne!(
        dirty_digest(b"", &[(b"a".to_vec(), b"F1".to_vec())]),
        dirty_digest(b"", &[(b"a".to_vec(), b"L1".to_vec())])
    );
}

/// A fresh directory under the system temp dir, removed on drop (no tempfile dependency in
/// this build-dependency crate).
struct Dir(PathBuf);

impl std::ops::Deref for Dir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tempfile_dir() -> Dir {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "xshell-buildinfo-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    Dir(p)
}

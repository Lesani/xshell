//! Where Daemon builds are installed on a Host, and which one starts next (Lesani/xshell#49).
//!
//! Each build lives in `~/.xshell/server/<dir>/xshelld`, where `<dir>` is
//! `xshell_protocol::build::install_dir_name`. The Host-wide `selected` record
//! (`~/.xshell/server/selected`, one line: a directory name) names the build that `xshelld
//! connect` and a Persistent Local Daemon start when no Daemon runs. Only a Desktop's
//! upgrade or replace writes it, just before it asks the running Daemon to exit; installing
//! never does. The one-Daemon lock and socket do not depend on the build.

use std::path::{Path, PathBuf};

/// The record's file name, in [`server_root`].
pub const SELECTED: &str = "selected";

/// The longest directory name accepted.
const MAX_DIR_NAME: usize = 128;

/// `home/.xshell/server`.
pub fn server_root(home: &Path) -> PathBuf {
    home.join(".xshell").join("server")
}

/// An install directory name: the version alphabet (`[A-Za-z0-9.+-]`), not starting with
/// `.`, at most 128 bytes. Such a name is safe in a path and in a shell word.
pub fn valid_dir_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_DIR_NAME
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
}

/// The version part of a directory name: everything before the first `+`.
#[cfg(any(unix, test))]
fn dir_version(name: &str) -> &str {
    name.split_once('+').map_or(name, |(v, _)| v)
}

#[cfg(unix)]
pub use unix::{resolve_selected, valid_install, write_selected};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io::{self, Error, ErrorKind, Read, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

    fn uid() -> u32 {
        unsafe { libc::getuid() }
    }

    /// Ours, not a symlink, and not writable by group or others.
    fn private(m: &std::fs::Metadata) -> bool {
        m.uid() == uid() && m.mode() & 0o022 == 0
    }

    /// `root/name/xshelld` when it is a real, private, executable file in a real, private
    /// directory.
    pub fn valid_install(root: &Path, name: &str) -> Option<PathBuf> {
        if !valid_dir_name(name) {
            return None;
        }
        let dir = root.join(name);
        let m = std::fs::symlink_metadata(&dir).ok()?;
        if !m.file_type().is_dir() || !private(&m) {
            return None;
        }
        let file = dir.join("xshelld");
        let m = std::fs::symlink_metadata(&file).ok()?;
        (m.file_type().is_file() && private(&m) && m.mode() & 0o100 != 0).then_some(file)
    }

    /// The binary the `selected` record in `root` names, if it is a valid install
    /// ([`valid_install`]) and not of an older version than `own_version` (both parsed as
    /// semver; precedence only). A newer version therefore takes over at the next natural
    /// restart (ADR-0003), and between builds of one version the record always wins.
    pub fn resolve_selected(root: &Path, own_version: &str) -> Option<PathBuf> {
        let m = std::fs::symlink_metadata(root).ok()?;
        if !m.file_type().is_dir() || !private(&m) {
            return None;
        }
        let record = root.join(SELECTED);
        let m = std::fs::symlink_metadata(&record).ok()?;
        if !m.file_type().is_file() || !private(&m) {
            return None;
        }
        let mut raw = Vec::new();
        std::fs::File::open(&record)
            .ok()?
            .take(256)
            .read_to_end(&mut raw)
            .ok()?;
        let text = String::from_utf8(raw).ok()?;
        let name = text.strip_suffix('\n').unwrap_or(&text);
        let file = valid_install(root, name)?;
        if let (Ok(sel), Ok(own)) = (
            semver::Version::parse(dir_version(name)),
            semver::Version::parse(own_version),
        ) {
            if sel.cmp_precedence(&own) == std::cmp::Ordering::Less {
                return None;
            }
        }
        Some(file)
    }

    /// Record `name` as the build to start next on this machine: written to a new private
    /// temp file, synced, and renamed over `selected`. Every directory on the way is created
    /// 0700 and must be a real directory owned by this user; `name` must be a valid install.
    pub fn write_selected(home: &Path, name: &str) -> io::Result<()> {
        if !valid_dir_name(name) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid install directory name {name:?}"),
            ));
        }
        let mut dir = home.to_path_buf();
        for part in [".xshell", "server"] {
            dir.push(part);
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            let m = std::fs::symlink_metadata(&dir)?;
            if !m.file_type().is_dir() {
                return Err(Error::other(format!(
                    "{} is not a directory",
                    dir.display()
                )));
            }
            if m.uid() != uid() {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{} is owned by uid {}, not by us", dir.display(), m.uid()),
                ));
            }
            if m.mode() & 0o077 != 0 {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        if valid_install(&dir, name).is_none() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("{} holds no valid xshelld", dir.join(name).display()),
            ));
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = dir.join(format!(
            ".selected.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let r = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(format!("{name}\n").as_bytes())?;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, dir.join(SELECTED))
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_names() {
        for ok in [
            "1.5.0",
            "1.5.0+85ee920a1b2c",
            "1.5.0+85ee920a1b2c+d.3f9a01c2b4e5",
        ] {
            assert!(valid_dir_name(ok), "{ok}");
        }
        let long = "1".repeat(129);
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "1/2",
            "1 2",
            "1;rm",
            "selected\n",
            &long,
        ] {
            assert!(!valid_dir_name(bad), "{bad:?}");
        }
        assert!(valid_dir_name(&"1".repeat(128)));
        assert_eq!(dir_version("1.5.0+abc+d.x"), "1.5.0");
        assert_eq!(dir_version("1.5.0"), "1.5.0");
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode(p: &Path) -> u32 {
            std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
        }

        /// A home with `server/<name>/xshelld` installed (0700).
        fn install(home: &Path, name: &str) -> PathBuf {
            let dir = server_root(home).join(name);
            std::fs::create_dir_all(&dir).unwrap();
            for d in [home.join(".xshell"), server_root(home), dir.clone()] {
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let f = dir.join("xshelld");
            std::fs::write(&f, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o700)).unwrap();
            f
        }

        #[test]
        fn write_then_resolve() {
            let t = tempfile::tempdir().unwrap();
            let home = t.path();
            let f = install(home, "1.5.0+85ee920a1b2c");
            write_selected(home, "1.5.0+85ee920a1b2c").unwrap();
            let rec = server_root(home).join(SELECTED);
            assert_eq!(
                std::fs::read_to_string(&rec).unwrap(),
                "1.5.0+85ee920a1b2c\n"
            );
            assert_eq!(mode(&rec), 0o600);
            assert_eq!(resolve_selected(&server_root(home), "1.5.0"), Some(f));
            // Replaced atomically, no temp files left.
            let g = install(home, "1.5.0+000000000000");
            write_selected(home, "1.5.0+000000000000").unwrap();
            assert_eq!(resolve_selected(&server_root(home), "1.5.0"), Some(g));
            let left: Vec<_> = std::fs::read_dir(server_root(home))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert!(left.is_empty(), "{left:?}");
        }

        #[test]
        fn write_refuses_bad_names_and_missing_installs() {
            let t = tempfile::tempdir().unwrap();
            let home = t.path();
            for bad in ["", "..", "a/b", ".x"] {
                let e = write_selected(home, bad).unwrap_err();
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
            }
            assert!(!home.join(".xshell").exists());
            let e = write_selected(home, "1.5.0+85ee920a1b2c").unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
            assert!(!server_root(home).join(SELECTED).exists());
            // The directories it made are private.
            assert_eq!(mode(&home.join(".xshell")), 0o700);
            assert_eq!(mode(&server_root(home)), 0o700);
        }

        #[test]
        fn write_refuses_symlinked_root() {
            let t = tempfile::tempdir().unwrap();
            let home = t.path().join("home");
            let elsewhere = t.path().join("elsewhere");
            std::fs::create_dir_all(home.join(".xshell")).unwrap();
            std::fs::create_dir(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, server_root(&home)).unwrap();
            let e = write_selected(&home, "1.5.0").unwrap_err();
            assert!(e.to_string().contains("not a directory"), "{e}");
        }

        #[test]
        fn concurrent_writes_leave_one_whole_record() {
            let t = tempfile::tempdir().unwrap();
            let home = t.path().to_path_buf();
            let names: Vec<String> = (0..8).map(|i| format!("1.5.0+00000000000{i}")).collect();
            for n in &names {
                install(&home, n);
            }
            let joins: Vec<_> = names
                .iter()
                .cloned()
                .map(|n| {
                    let home = home.clone();
                    std::thread::spawn(move || {
                        for _ in 0..20 {
                            write_selected(&home, &n).unwrap();
                        }
                    })
                })
                .collect();
            for j in joins {
                j.join().unwrap();
            }
            let got = std::fs::read_to_string(server_root(&home).join(SELECTED)).unwrap();
            assert!(names.iter().any(|n| got == format!("{n}\n")), "{got:?}");
        }

        type Breaker = Box<dyn Fn(&Path)>;

        /// Everything `resolve_selected` refuses falls back (`None`).
        #[test]
        fn resolve_validation_table() {
            let cases: Vec<(&str, Breaker)> = vec![
                ("no record", Box::new(|_| {})),
                (
                    "bad name",
                    Box::new(|r| std::fs::write(r.join(SELECTED), "../1.5.0\n").unwrap()),
                ),
                (
                    "two lines",
                    Box::new(|r| std::fs::write(r.join(SELECTED), "1.5.0+a\n\n").unwrap()),
                ),
                (
                    "missing dir",
                    Box::new(|r| std::fs::write(r.join(SELECTED), "1.5.0+missing\n").unwrap()),
                ),
                (
                    "group-writable binary",
                    Box::new(|r| {
                        std::fs::set_permissions(
                            r.join("1.5.0+a/xshelld"),
                            std::fs::Permissions::from_mode(0o720),
                        )
                        .unwrap()
                    }),
                ),
                (
                    "not executable",
                    Box::new(|r| {
                        std::fs::set_permissions(
                            r.join("1.5.0+a/xshelld"),
                            std::fs::Permissions::from_mode(0o600),
                        )
                        .unwrap()
                    }),
                ),
                (
                    "world-writable dir",
                    Box::new(|r| {
                        std::fs::set_permissions(
                            r.join("1.5.0+a"),
                            std::fs::Permissions::from_mode(0o702),
                        )
                        .unwrap()
                    }),
                ),
                (
                    "symlinked binary",
                    Box::new(|r| {
                        let f = r.join("1.5.0+a/xshelld");
                        std::fs::rename(&f, r.join("real")).unwrap();
                        std::os::unix::fs::symlink(r.join("real"), &f).unwrap();
                    }),
                ),
                (
                    "symlinked dir",
                    Box::new(|r| {
                        std::fs::rename(r.join("1.5.0+a"), r.join("real-dir")).unwrap();
                        std::os::unix::fs::symlink(r.join("real-dir"), r.join("1.5.0+a")).unwrap();
                    }),
                ),
                (
                    "symlinked record",
                    Box::new(|r| {
                        std::fs::rename(r.join(SELECTED), r.join("other")).unwrap();
                        std::os::unix::fs::symlink(r.join("other"), r.join(SELECTED)).unwrap();
                    }),
                ),
                (
                    "older version (S2)",
                    Box::new(|r| {
                        std::fs::rename(r.join("1.5.0+a"), r.join("1.4.9+a")).unwrap();
                        std::fs::write(r.join(SELECTED), "1.4.9+a\n").unwrap();
                    }),
                ),
                (
                    "group-writable root",
                    Box::new(|r| {
                        std::fs::set_permissions(r, std::fs::Permissions::from_mode(0o770)).unwrap()
                    }),
                ),
            ];
            for (what, break_it) in cases {
                let t = tempfile::tempdir().unwrap();
                let home = t.path();
                install(home, "1.5.0+a");
                let root = server_root(home);
                std::fs::write(root.join(SELECTED), "1.5.0+a\n").unwrap();
                std::fs::set_permissions(
                    root.join(SELECTED),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
                assert!(
                    resolve_selected(&root, "1.5.0").is_some(),
                    "{what}: baseline"
                );
                if what == "no record" {
                    std::fs::remove_file(root.join(SELECTED)).unwrap();
                }
                break_it(&root);
                assert_eq!(resolve_selected(&root, "1.5.0"), None, "{what}");
            }
        }

        /// Newer and equal versions win; versions that do not parse are not compared.
        #[test]
        fn resolve_precedence() {
            let t = tempfile::tempdir().unwrap();
            let home = t.path();
            let root = server_root(home);
            for (name, own, wins) in [
                ("1.6.0+a", "1.5.0", true),
                ("1.5.0+a", "1.5.0", true),
                ("1.5.0", "1.5.0+b", true),
                ("1.5.0-beta.1+a", "1.5.0", false),
                ("dev+a", "1.5.0", true),
                ("1.4.0", "dev", true),
            ] {
                install(home, name);
                write_selected(home, name).unwrap();
                assert_eq!(
                    resolve_selected(&root, own).is_some(),
                    wins,
                    "{name} vs {own}"
                );
            }
        }
    }
}

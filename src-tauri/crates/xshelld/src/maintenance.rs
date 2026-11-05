//! Explicit offline install maintenance, not a collector or a lock respected by launchers.
//! The operator owns distributed quiescence and complete reference-home coverage.
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cleanup {
    pub home: PathBuf,
    pub reference_homes: Vec<PathBuf>,
    pub keep: BTreeSet<String>,
    pub delete: bool,
}

fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|c| {
            matches!(
                c,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
        && !path
            .as_os_str()
            .to_string_lossy()
            .split(|c| c == '/' || (cfg!(windows) && c == '\\'))
            .any(|part| part == "." || part == "..")
}

pub fn parse(args: impl Iterator<Item = OsString>) -> Result<Cleanup, String> {
    let mut args = args;
    if args.next().as_deref() != Some("cleanup-installs".as_ref()) {
        return Err("expected maintenance cleanup-installs".into());
    }
    let mut home = None;
    let mut reference_homes = Vec::new();
    let mut keep = BTreeSet::new();
    let mut delete = false;
    let mut confirm = false;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--home" | "--reference-home") => {
                let path = PathBuf::from(args.next().ok_or("home option needs an absolute path")?);
                if !absolute(&path) {
                    return Err("home must be an absolute path without . or .. components".into());
                }
                if flag == "--home" {
                    if home.replace(path).is_some() {
                        return Err("duplicate --home".into());
                    }
                } else if reference_homes.contains(&path) {
                    return Err("duplicate --reference-home".into());
                } else {
                    reference_homes.push(path);
                }
            }
            Some("--keep") => {
                let name = args
                    .next()
                    .and_then(|s| s.into_string().ok())
                    .ok_or("--keep needs a directory name")?;
                if !xshell_core::server::valid_dir_name(&name) || !keep.insert(name) {
                    return Err("invalid or duplicate --keep".into());
                }
            }
            Some("--delete") if !delete => delete = true,
            Some("--confirm-quiescent") if !confirm => confirm = true,
            _ => return Err("unsupported or duplicate maintenance option".into()),
        }
    }
    if delete != confirm {
        return Err("--delete and --confirm-quiescent must be supplied together".into());
    }
    let home = home.ok_or("maintenance requires explicit --home ABS")?;
    if reference_homes.contains(&home) {
        return Err("--reference-home duplicates --home".into());
    }
    Ok(Cleanup {
        home,
        reference_homes,
        keep,
        delete,
    })
}

pub fn run(opts: Cleanup) -> i32 {
    #[cfg(target_os = "linux")]
    {
        linux::run(opts)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = opts;
        eprintln!("blocked: install maintenance is supported only on Linux; nothing removed");
        1
    }
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    fn parse_args(s: &str) -> Result<Cleanup, String> {
        parse(s.split_whitespace().map(OsString::from))
    }
    #[cfg(not(windows))]
    #[test]
    fn explicit_home_paired_contract_and_additive_keeps() {
        let opts =
            parse_args("cleanup-installs --home /a --reference-home /b --keep 1.0.0").unwrap();
        assert!(!opts.delete);
        assert_eq!(opts.reference_homes, [PathBuf::from("/b")]);
        assert!(opts.keep.contains("1.0.0"));
        assert!(
            parse_args("cleanup-installs --home /a --delete --confirm-quiescent")
                .unwrap()
                .delete
        );
        for args in [
            "cleanup-installs",
            "cleanup-installs --home relative",
            "cleanup-installs --home /a/../b",
            "cleanup-installs --home /a/./b",
            "cleanup-installs --home /a --home /a",
            "cleanup-installs --home /a --delete",
            "cleanup-installs --home /a --confirm-quiescent",
            "cleanup-installs --home /a --keep ../bad",
            "cleanup-installs --home /a --delete --confirm-quiescent --delete",
            "cleanup-installs --home /a --socket /s",
            "cleanup-installs --home /a --reference-home /b --reference-home /b",
        ] {
            assert!(parse_args(args).is_err(), "{args}");
        }
    }
    #[cfg(windows)]
    #[test]
    fn windows_absolute_home_reaches_unsupported_dispatch() {
        let opts = parse(
            [
                "cleanup-installs",
                "--home",
                r"C:\fixture",
                "--delete",
                "--confirm-quiescent",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap();
        assert_eq!(run(opts), 1);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_platform_no_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let sentinel = temp.path().join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        assert_eq!(
            run(Cleanup {
                home: temp.path().to_owned(),
                reference_homes: vec![],
                keep: BTreeSet::new(),
                delete: true
            }),
            1
        );
        assert_eq!(std::fs::read(sentinel).unwrap(), b"unchanged");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}

//! Windows: the stable console command `xshelld` (#118). The Local Host's Daemon is installed
//! at `%USERPROFILE%\.xshell\server\<version+build>\xshelld.exe`, a path that changes with
//! every update and is on no PATH, and `xshell.exe` is a GUI program no shell waits for. So
//! at every start the Desktop writes `%USERPROFILE%\.xshell\bin\xshelld.cmd`, which runs the
//! Daemon it resolved; `install.ps1` puts that folder on the user's PATH.
//!
//! Not written on Unix: there `~/.xshell/bin/xshelld` can be a real binary an installer put.

#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};

/// The batch file's text for the Daemon at `daemon`, written into `<xshell_dir>\bin`: two
/// lines (CRLF) that run it with the caller's arguments; the batch file ends with the
/// Daemon's exit code. A Daemon under `xshell_dir` is named relative to the batch file, so a
/// profile path (which may not be ASCII) never appears in it; any other path is written out.
/// `%` is doubled: in a batch file a single one starts a variable. `None`: the path cannot
/// be quoted in a batch file.
#[cfg(any(windows, test))]
pub fn content(xshell_dir: &str, daemon: &str) -> Option<String> {
    if daemon.contains(['"', '\r', '\n']) {
        return None;
    }
    let relative = daemon
        .strip_prefix(xshell_dir.trim_end_matches('\\'))
        .filter(|rest| rest.starts_with('\\') && !xshell_dir.is_empty());
    let target = match relative {
        // `%~dp0` is the batch file's own folder, with a trailing backslash.
        Some(rest) => format!("%~dp0..{}", rest.replace('%', "%%")),
        None => daemon.replace('%', "%%"),
    };
    Some(format!("@echo off\r\n\"{target}\" %*\r\n"))
}

/// Write `<home>\.xshell\bin\xshelld.cmd` for `daemon`, replacing an existing one atomically
/// (a temporary file beside it, then a rename). Nothing is written when the Daemon itself is
/// in that folder: `xshelld.exe` there is already the command.
#[cfg(any(windows, test))]
pub fn write(home: &Path, daemon: &Path) -> Result<Option<PathBuf>, String> {
    let xshell_dir = home.join(".xshell");
    let bin_dir = xshell_dir.join("bin");
    if daemon.parent() == Some(bin_dir.as_path()) {
        return Ok(None);
    }
    let text = content(
        xshell_dir.to_str().ok_or("the home path is not Unicode")?,
        daemon.to_str().ok_or("the Daemon path is not Unicode")?,
    )
    .ok_or("the Daemon path cannot be written in a batch file")?;
    std::fs::create_dir_all(&bin_dir).map_err(|e| format!("{}: {e}", bin_dir.display()))?;
    let path = bin_dir.join("xshelld.cmd");
    let tmp = bin_dir.join(format!("xshelld.cmd.tmp-{}", std::process::id()));
    let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_under_the_xshell_dir_is_named_relative_to_the_batch_file() {
        let text = content(
            r"C:\Users\Zoë\.xshell",
            r"C:\Users\Zoë\.xshell\server\1.6.0-rjw.6+abc123\xshelld.exe",
        )
        .unwrap();
        assert_eq!(
            text,
            "@echo off\r\n\"%~dp0..\\server\\1.6.0-rjw.6+abc123\\xshelld.exe\" %*\r\n"
        );
        assert!(text.is_ascii(), "the profile path must not appear");
        // A trailing separator on the directory changes nothing.
        assert_eq!(
            content(
                r"C:\Users\Zoë\.xshell\",
                r"C:\Users\Zoë\.xshell\server\1.6.0-rjw.6+abc123\xshelld.exe",
            )
            .unwrap(),
            text
        );
    }

    #[test]
    fn another_path_is_written_out_with_percent_doubled() {
        assert_eq!(
            content(r"C:\Users\a\.xshell", r"D:\100% builds\xshelld.exe").unwrap(),
            "@echo off\r\n\"D:\\100%% builds\\xshelld.exe\" %*\r\n"
        );
        // A sibling directory that only shares the prefix is not "under" it.
        assert_eq!(
            content(r"C:\Users\a\.xshell", r"C:\Users\a\.xshell-old\xshelld.exe").unwrap(),
            "@echo off\r\n\"C:\\Users\\a\\.xshell-old\\xshelld.exe\" %*\r\n"
        );
    }

    #[test]
    fn percent_is_doubled_in_the_relative_form_too() {
        assert_eq!(
            content(r"C:\h\.xshell", r"C:\h\.xshell\server\50%\xshelld.exe").unwrap(),
            "@echo off\r\n\"%~dp0..\\server\\50%%\\xshelld.exe\" %*\r\n"
        );
    }

    #[test]
    fn a_path_that_cannot_be_quoted_is_refused() {
        assert_eq!(content(r"C:\h\.xshell", "C:\\a\"b\\xshelld.exe"), None);
        assert_eq!(content(r"C:\h\.xshell", "C:\\a\r\nb\\xshelld.exe"), None);
    }

    #[test]
    fn write_replaces_an_existing_file_and_leaves_no_temporary_file() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let bin = home.join(".xshell").join("bin");
        let first = home.join("first-daemon").join("xshelld.exe");
        let path = write(home, &first).unwrap().unwrap();
        assert_eq!(path, bin.join("xshelld.cmd"));
        let second = home.join("second-daemon").join("xshelld.exe");
        write(home, &second).unwrap().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("second-daemon"), "{text}");
        assert!(!text.contains("first-daemon"), "{text}");
        let names: Vec<_> = std::fs::read_dir(&bin)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["xshelld.cmd"]);
    }

    #[test]
    fn nothing_is_written_beside_a_daemon_in_the_bin_folder() {
        let t = tempfile::tempdir().unwrap();
        let daemon = t.path().join(".xshell").join("bin").join("xshelld.exe");
        assert_eq!(write(t.path(), &daemon).unwrap(), None);
        assert!(!t.path().join(".xshell").join("bin").exists());
    }
}

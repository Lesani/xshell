//! Diagnostics go to stderr as `<rfc3339> <LEVEL> <message>`. `serve` runs with stderr
//! redirected to `~/.xshell/log/xshelld.log`; for `connect`, stderr is the ssh error text.

#[doc(hidden)]
pub fn write(level: &str, msg: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let now = xshell_core::time::system_time_to_iso(std::time::SystemTime::now());
    // One write per line, so lines from concurrent writers never interleave.
    let line = format!("{now} {level} {msg}\n");
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[macro_export]
macro_rules! log {
    ($level:literal, $($arg:tt)*) => {
        $crate::log::write($level, format_args!($($arg)*))
    };
}

/// Rotated once it is this big: the old log becomes `xshelld.log.1`.
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Open the Daemon log for appending (0600, its directory private), rotating it first when
/// it has grown past 5 MiB. `serve`'s stdout and stderr go here.
pub fn open_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(path, old);
    }
    crate::paths::private_file(std::fs::OpenOptions::new().create(true).append(true)).open(path)
}

/// Point this process's stdout and stderr at the Daemon log (see [`open_log`]).
#[cfg(unix)]
pub fn redirect_to_log(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let f = open_log(path)?;
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(f.as_raw_fd(), fd) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Point this process's stdout and stderr at the Daemon log (see [`open_log`]). The log's
/// handle becomes both standard handles and stays open for the process's life; std looks
/// the handle up on every write.
#[cfg(windows)]
pub fn redirect_to_log(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    let h = open_log(path)?.into_raw_handle();
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if unsafe { SetStdHandle(which, h as _) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

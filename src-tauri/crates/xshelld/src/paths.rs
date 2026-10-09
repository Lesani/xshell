//! Where the Daemon keeps its socket, lock, state, log and temp files.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub socket: PathBuf,
    /// `daemon.lock` next to the socket: held by the one running `serve`.
    pub lock: PathBuf,
    /// `daemon.pid` next to the socket, written by `serve`.
    pub pid: PathBuf,
    /// `home/.xshell/daemon/terminals.json`.
    pub state: PathBuf,
    /// `home/.xshell/log/xshelld.log`.
    pub log: PathBuf,
    /// Per-user private temp dir for `HostCtx.temp_dir` (dropped files):
    /// `$XDG_RUNTIME_DIR/xshell/tmp`, else `home/.xshell/tmp`.
    pub tmp: PathBuf,
}

impl Paths {
    pub fn socket_dir(&self) -> &Path {
        self.socket.parent().unwrap_or(Path::new("/"))
    }
}

/// Pure: environment values are passed in.
pub fn resolve(
    home: &Path,
    xdg_runtime_dir: Option<&Path>,
    socket_override: Option<&Path>,
) -> Paths {
    let xdg = xdg_runtime_dir.filter(|p| p.is_absolute());
    let base = match xdg {
        Some(x) => x.join("xshell"),
        None => home.join(".xshell").join("run"),
    };
    let socket = socket_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| base.join("daemon.sock"));
    let sdir = socket.parent().unwrap_or(Path::new("/")).to_path_buf();
    let tmp = match xdg {
        Some(x) => x.join("xshell").join("tmp"),
        None => home.join(".xshell").join("tmp"),
    };
    Paths {
        lock: sdir.join("daemon.lock"),
        pid: sdir.join("daemon.pid"),
        socket,
        state: home.join(".xshell").join("daemon").join("terminals.json"),
        log: home.join(".xshell").join("log").join("xshelld.log"),
        tmp,
    }
}

/// Create `dir` (and parents) as 0700. An existing dir must be owned by us; a mode broader
/// than 0700 is tightened.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {}, not by us (uid {uid})",
                dir.display(),
                meta.uid()
            ),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const SUN_PATH_MAX: usize = 108;
#[cfg(not(target_os = "linux"))]
const SUN_PATH_MAX: usize = 104;

/// `sun_path` holds the path plus a NUL; a longer one fails with an unhelpful error later.
pub fn check_socket_path_len(p: &Path) -> io::Result<()> {
    let len = p.as_os_str().len();
    if len >= SUN_PATH_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path is {len} bytes, the limit is {}: {} (set XSHELLD_SOCKET or XDG_RUNTIME_DIR)",
                SUN_PATH_MAX - 1,
                p.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_preferred() {
        let p = resolve(Path::new("/h"), Some(Path::new("/r")), None);
        assert_eq!(p.socket, PathBuf::from("/r/xshell/daemon.sock"));
        assert_eq!(p.lock, PathBuf::from("/r/xshell/daemon.lock"));
        assert_eq!(p.pid, PathBuf::from("/r/xshell/daemon.pid"));
        assert_eq!(p.tmp, PathBuf::from("/r/xshell/tmp"));
    }

    #[test]
    fn home_fallback() {
        let p = resolve(Path::new("/h"), None, None);
        assert_eq!(p.socket, PathBuf::from("/h/.xshell/run/daemon.sock"));
        assert_eq!(p.lock, PathBuf::from("/h/.xshell/run/daemon.lock"));
        assert_eq!(p.state, PathBuf::from("/h/.xshell/daemon/terminals.json"));
        assert_eq!(p.log, PathBuf::from("/h/.xshell/log/xshelld.log"));
        assert_eq!(p.tmp, PathBuf::from("/h/.xshell/tmp"));
        // A relative XDG_RUNTIME_DIR is invalid per the spec and ignored.
        assert_eq!(resolve(Path::new("/h"), Some(Path::new("r")), None), p);
    }

    #[test]
    fn socket_override_moves_lock() {
        let p = resolve(
            Path::new("/h"),
            Some(Path::new("/r")),
            Some(Path::new("/s/d.sock")),
        );
        assert_eq!(p.socket, PathBuf::from("/s/d.sock"));
        assert_eq!(p.lock, PathBuf::from("/s/daemon.lock"));
    }

    #[test]
    fn private_dir_mode() {
        let t = tempfile::tempdir().unwrap();
        let wide = t.path().join("wide");
        fs::create_dir(&wide).unwrap();
        fs::set_permissions(&wide, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&wide).unwrap();
        assert_eq!(fs::metadata(&wide).unwrap().mode() & 0o777, 0o700);
        let new = t.path().join("a/b");
        ensure_private_dir(&new).unwrap();
        assert_eq!(fs::metadata(&new).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn socket_path_too_long() {
        let p = PathBuf::from(format!("/{}", "x".repeat(199)));
        let e = check_socket_path_len(&p).unwrap_err();
        assert!(e.to_string().contains("200 bytes"), "{e}");
        check_socket_path_len(Path::new("/tmp/x.sock")).unwrap();
    }
}

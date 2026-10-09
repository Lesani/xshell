//! Where the Daemon keeps its socket, lock, state, log and temp files.
//!
//! On Windows the endpoint is a named pipe (`\\.\pipe\xshelld-<user SID>`), which lives in
//! no directory: the lock and the pidfile go to `home\.xshell\run` instead.

use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The socket (a pipe name on Windows).
    pub socket: PathBuf,
    /// `daemon.lock`, next to the socket (Windows: in `home/.xshell/run`): held by the one
    /// running `serve`.
    pub lock: PathBuf,
    /// `daemon.pid` next to the lock, written by `serve`.
    pub pid: PathBuf,
    /// `home/.xshell/daemon/terminals.json`.
    pub state: PathBuf,
    /// `home/.xshell/daemon/claude-hooks.json`: the Claude Code settings naming the agent
    /// hooks, passed to each Claude Terminal with `--settings`.
    pub claude_hooks: PathBuf,
    /// `home/.xshell/daemon/mode`: how the last Daemon that held the lock was started
    /// ([`Mode`]). `connect` never starts a Daemon while it says `gui-bound`.
    pub mode: PathBuf,
    /// `home/.xshell/log/xshelld.log`.
    pub log: PathBuf,
    /// `home/.xshell/daemon/ring`: this Host's Ring keys and Roster chain (0700).
    pub ring_dir: PathBuf,
    /// Per-user private temp dir for `HostCtx.temp_dir` (dropped files):
    /// `$XDG_RUNTIME_DIR/xshell/tmp`, else `home/.xshell/tmp`.
    pub tmp: PathBuf,
}

/// The paths of this platform. Environment values are passed in; on Windows
/// `xdg_runtime_dir` is ignored and the default pipe is this user's.
pub fn resolve(
    home: &Path,
    xdg_runtime_dir: Option<&Path>,
    socket_override: Option<&Path>,
) -> Paths {
    #[cfg(windows)]
    {
        let _ = xdg_runtime_dir;
        let pipe = xshell_core::pipe::default_pipe_name()
            .unwrap_or_else(|_| PathBuf::from(r"\\.\pipe\xshelld"));
        resolve_windows(home, &pipe, socket_override)
    }
    #[cfg(not(windows))]
    resolve_unix(home, xdg_runtime_dir, socket_override)
}

/// Unix: the socket in `$XDG_RUNTIME_DIR/xshell`, else `home/.xshell/run`; the lock and the
/// pidfile next to it. Pure.
pub fn resolve_unix(
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
    with_home(home, socket, &sdir, tmp)
}

/// Windows: the pipe `default_pipe` unless overridden; the lock and the pidfile in
/// `home/.xshell/run`, whatever the pipe. Pure.
pub fn resolve_windows(home: &Path, default_pipe: &Path, socket_override: Option<&Path>) -> Paths {
    let socket = socket_override.unwrap_or(default_pipe).to_path_buf();
    let run = home.join(".xshell").join("run");
    with_home(home, socket, &run, home.join(".xshell").join("tmp"))
}

fn with_home(home: &Path, socket: PathBuf, lock_dir: &Path, tmp: PathBuf) -> Paths {
    Paths {
        lock: lock_dir.join("daemon.lock"),
        pid: lock_dir.join("daemon.pid"),
        socket,
        state: home.join(".xshell").join("daemon").join("terminals.json"),
        claude_hooks: home
            .join(".xshell")
            .join("daemon")
            .join("claude-hooks.json"),
        mode: home.join(".xshell").join("daemon").join("mode"),
        log: home.join(".xshell").join("log").join("xshelld.log"),
        ring_dir: home.join(".xshell").join("daemon").join("ring"),
        tmp,
    }
}

/// How the Daemon on this machine is run: by the xshell app (GUI-bound, ADR-0005) or on its
/// own (Persistent). Recorded in [`Paths::mode`] by each `serve` that takes the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    GuiBound,
    Persistent,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::GuiBound => "gui-bound",
            Mode::Persistent => "persistent",
        }
    }
}

/// The recorded mode; `None` when there is no marker or it is unreadable.
pub fn read_mode(path: &Path) -> Option<Mode> {
    match fs::read_to_string(path).ok()?.trim() {
        "gui-bound" => Some(Mode::GuiBound),
        "persistent" => Some(Mode::Persistent),
        _ => None,
    }
}

/// Record `mode` atomically (a 0600 temp file renamed over the marker).
pub fn write_mode(path: &Path, mode: Mode) -> io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let r = (|| {
        let mut f = private_file(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
        )
        .open(&tmp)?;
        writeln!(f, "{}", mode.as_str())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// `o` creating files readable by us only (0600; Windows: the directory's inherited ACL).
pub fn private_file(o: &mut fs::OpenOptions) -> &mut fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600)
    }
    #[cfg(not(unix))]
    o
}

/// Create `dir` (and parents) as 0700. An existing dir must be owned by us; a mode broader
/// than 0700 is tightened. Windows: created, and it must be a directory; the user profile's
/// ACL is inherited.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    #[cfg(not(unix))]
    fs::create_dir_all(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
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
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const SUN_PATH_MAX: usize = 108;
#[cfg(not(target_os = "linux"))]
const SUN_PATH_MAX: usize = 104;

/// A usable endpoint: a socket path short enough for `sun_path` (Windows: a pipe name).
pub fn check_endpoint(p: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        xshell_core::pipe::check_pipe_name(p)
    }
    #[cfg(not(windows))]
    check_socket_path_len(p)
}

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

    #[cfg(unix)]
    #[test]
    fn xdg_preferred() {
        let p = resolve_unix(Path::new("/h"), Some(Path::new("/r")), None);
        assert_eq!(p.socket, PathBuf::from("/r/xshell/daemon.sock"));
        assert_eq!(p.lock, PathBuf::from("/r/xshell/daemon.lock"));
        assert_eq!(p.pid, PathBuf::from("/r/xshell/daemon.pid"));
        assert_eq!(p.tmp, PathBuf::from("/r/xshell/tmp"));
    }

    #[cfg(unix)]
    #[test]
    fn home_fallback() {
        let p = resolve_unix(Path::new("/h"), None, None);
        assert_eq!(p.socket, PathBuf::from("/h/.xshell/run/daemon.sock"));
        assert_eq!(p.lock, PathBuf::from("/h/.xshell/run/daemon.lock"));
        assert_eq!(p.state, PathBuf::from("/h/.xshell/daemon/terminals.json"));
        assert_eq!(
            p.claude_hooks,
            PathBuf::from("/h/.xshell/daemon/claude-hooks.json")
        );
        assert_eq!(p.log, PathBuf::from("/h/.xshell/log/xshelld.log"));
        assert_eq!(p.ring_dir, PathBuf::from("/h/.xshell/daemon/ring"));
        assert_eq!(p.tmp, PathBuf::from("/h/.xshell/tmp"));
        // A relative XDG_RUNTIME_DIR is invalid per the spec and ignored.
        assert_eq!(resolve_unix(Path::new("/h"), Some(Path::new("r")), None), p);
    }

    #[cfg(unix)]
    #[test]
    fn mode_path() {
        let p = resolve_unix(Path::new("/h"), Some(Path::new("/r")), None);
        assert_eq!(p.mode, PathBuf::from("/h/.xshell/daemon/mode"));
        assert_eq!(p.mode.parent(), p.state.parent());
        let t = tempfile::tempdir().unwrap();
        let m = t.path().join("mode");
        assert_eq!(read_mode(&m), None);
        write_mode(&m, Mode::GuiBound).unwrap();
        assert_eq!(read_mode(&m), Some(Mode::GuiBound));
        #[cfg(unix)]
        assert_eq!(fs::metadata(&m).unwrap().mode() & 0o777, 0o600);
        write_mode(&m, Mode::Persistent).unwrap();
        assert_eq!(read_mode(&m), Some(Mode::Persistent));
        fs::write(&m, "bogus\n").unwrap();
        assert_eq!(read_mode(&m), None);
        assert_eq!(
            fs::read_dir(t.path()).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_override_moves_lock() {
        let p = resolve_unix(
            Path::new("/h"),
            Some(Path::new("/r")),
            Some(Path::new("/s/d.sock")),
        );
        assert_eq!(p.socket, PathBuf::from("/s/d.sock"));
        assert_eq!(p.lock, PathBuf::from("/s/daemon.lock"));
    }

    #[cfg(unix)]
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
    fn mode_marker_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        let m = t.path().join("mode");
        assert_eq!(read_mode(&m), None);
        write_mode(&m, Mode::GuiBound).unwrap();
        assert_eq!(read_mode(&m), Some(Mode::GuiBound));
        write_mode(&m, Mode::Persistent).unwrap();
        assert_eq!(read_mode(&m), Some(Mode::Persistent));
        assert_eq!(
            fs::read_dir(t.path()).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[test]
    fn windows_layout() {
        let pipe = Path::new(r"\\.\pipe\xshelld-S-1-5-21-1");
        let h = Path::new("/h");
        let p = resolve_windows(h, pipe, None);
        assert_eq!(p.socket, pipe);
        // The lock and pidfile never sit next to a pipe name.
        assert_eq!(p.lock, h.join(".xshell").join("run").join("daemon.lock"));
        assert_eq!(p.pid, h.join(".xshell").join("run").join("daemon.pid"));
        assert_eq!(p.tmp, h.join(".xshell").join("tmp"));
        assert_eq!(p.mode, resolve_unix(h, None, None).mode);
        assert_eq!(p.state, resolve_unix(h, None, None).state);
        let other = Path::new(r"\\.\pipe\xshelld-test-1");
        let o = resolve_windows(h, pipe, Some(other));
        assert_eq!(o.socket, other);
        assert_eq!(o.lock, p.lock);
    }

    #[test]
    fn private_dir_created() {
        let t = tempfile::tempdir().unwrap();
        let new = t.path().join("a").join("b");
        ensure_private_dir(&new).unwrap();
        assert!(new.is_dir());
        let file = t.path().join("f");
        fs::write(&file, "x").unwrap();
        assert!(ensure_private_dir(&file).is_err());
    }

    #[test]
    fn socket_path_too_long() {
        let p = PathBuf::from(format!("/{}", "x".repeat(199)));
        let e = check_socket_path_len(&p).unwrap_err();
        assert!(e.to_string().contains("200 bytes"), "{e}");
        check_socket_path_len(Path::new("/tmp/x.sock")).unwrap();
        #[cfg(windows)]
        assert!(check_endpoint(Path::new("/tmp/x.sock")).is_err());
        #[cfg(not(windows))]
        check_endpoint(Path::new("/tmp/x.sock")).unwrap();
    }
}

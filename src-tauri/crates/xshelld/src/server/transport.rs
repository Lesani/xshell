//! The Daemon's endpoint per platform: a Unix socket (0600) or, on Windows, the per-user
//! named pipe of `xshell_core::pipe`. Both streams offer what connections use: `try_clone`,
//! read and write timeouts, `shutdown`, and `Read`/`Write` on a shared reference.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
#[cfg(unix)]
pub type Listener = std::os::unix::net::UnixListener;

#[cfg(windows)]
pub type Stream = xshell_core::pipe::PipeStream;
#[cfg(windows)]
pub type Listener = xshell_core::pipe::PipeListener;

/// Listen on `path`. Unix: the socket file is made 0600 (removed again if that fails); the
/// caller removed a stale one. Windows: fails when another process holds the pipe name.
pub(crate) fn bind(path: &Path) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let l = Listener::bind(path)?;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        Ok(l)
    }
    #[cfg(windows)]
    {
        Listener::bind(path)
    }
}

/// The next connection.
pub(crate) fn accept(l: &Listener) -> io::Result<Stream> {
    #[cfg(unix)]
    {
        l.accept().map(|(s, _)| s)
    }
    #[cfg(windows)]
    {
        l.accept()
    }
}

/// A connected pair, for a connection served in this process.
pub fn pair() -> io::Result<(Stream, Stream)> {
    #[cfg(unix)]
    {
        Stream::pair()
    }
    #[cfg(windows)]
    {
        xshell_core::pipe::pair()
    }
}

/// Connect once to `path`, so a blocked accept returns.
pub(crate) fn wake(path: &Path) {
    #[cfg(unix)]
    let _ = Stream::connect(path);
    #[cfg(windows)]
    let _ = xshell_core::pipe::connect(
        path,
        std::time::Instant::now() + std::time::Duration::from_secs(1),
        &|| false,
    );
}

/// Remove a socket file. A pipe has none: it is gone with the last handle.
pub(crate) fn remove_endpoint(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

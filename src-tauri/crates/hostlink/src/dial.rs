//! How the supervisor reaches a Daemon: a [`Dialer`] yields a byte stream ([`LinkIo`]) and
//! the [`Connection`] behind it. A Remote Host runs `xshelld connect` through its transport
//! ([`CommandDialer`]); a Daemon on this machine is reached directly over its local socket
//! ([`UnixSocketDialer`]), on Windows over its named pipe ([`NamedPipeDialer`]).
//! [`connect_local`] and [`LocalStream`] name whichever this platform uses.

use crate::cancel::CancelToken;
use crate::errors::{classify_ssh_failure, HostErrorHint};
use crate::link::LinkIo;
use crate::process::{self, Proc};
use crate::transport::Transport;
use std::sync::Arc;
use std::time::Duration;

/// A stream to a Daemon, and what owns it.
pub struct Dialed {
    pub io: LinkIo,
    pub conn: Box<dyn Connection>,
}

#[derive(Debug)]
pub enum DialError {
    Cancelled,
    Failed {
        message: String,
        hint: Option<HostErrorHint>,
        /// The OS error behind a local socket failure, so a dialer can tell a missing
        /// Daemon (`NotFound`, `ConnectionRefused`) from any other failure.
        kind: Option<std::io::ErrorKind>,
    },
}

impl DialError {
    /// A failure with no OS error kind attached.
    pub fn failed(message: impl Into<String>, hint: Option<HostErrorHint>) -> Self {
        DialError::Failed {
            message: message.into(),
            hint,
            kind: None,
        }
    }

    /// Whether this is a local socket nobody listens on: missing, or refusing connections.
    pub fn no_listener(&self) -> bool {
        matches!(
            self,
            DialError::Failed {
                kind: Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused),
                ..
            }
        )
    }
}

/// Opens one connection per attempt. `dial` returns promptly once `cancel` fires, and is
/// bounded otherwise: a named-pipe dialer must be equally cancellable (`WaitNamedPipe` with
/// a timeout, re-checking the token).
pub trait Dialer: Send + Sync {
    fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError>;
    fn describe(&self) -> String;

    /// Whether the Desktop upgrades the Daemon this reaches (ADR-0003), although it is not
    /// installed over a transport: an older one is reported as upgrade pending. Read at each
    /// check, so it may change while the Host runs.
    fn upgradable(&self) -> bool {
        false
    }

    /// Stop the incompatible Daemon this reaches, so the next dial starts this Desktop's
    /// version ("Upgrade now"). `None`: this dialer cannot, and the upgrade is refused.
    fn stop_incompatible(
        &self,
        _cancel: &CancelToken,
    ) -> Option<Result<(), crate::errors::HostError>> {
        None
    }
}

/// What carries a link: the transport child, or a socket. Dropping it ends it too.
pub trait Connection: Send {
    /// The transport child's pid; `None` for a socket.
    fn pid(&self) -> Option<u32>;
    /// Kill and reap the child, or shut the socket down, so the link's reader ends. Bounded.
    fn end(&self);
    /// Why a handshake failed, after the link reported `link_message`.
    fn diagnose(&self, link_message: String) -> Diagnosis;
}

pub struct Diagnosis {
    pub message: String,
    pub hint: Option<HostErrorHint>,
    /// The installed binary looks missing: probe again next time.
    pub reinstall: bool,
}

/// Runs `remote_cmd` through the Host's transport; the child's stdio is the stream.
pub struct CommandDialer {
    pub transport: Arc<dyn Transport>,
    pub remote_cmd: String,
}

impl Dialer for CommandDialer {
    fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError> {
        let cmd = self.transport.command(&self.remote_cmd);
        let mut proc = match process::spawn(&cmd, cancel) {
            Ok(p) => p,
            Err(_) if cancel.is_cancelled() => return Err(DialError::Cancelled),
            Err(e) => {
                return Err(DialError::failed(
                    format!("cannot run {}: {e}", self.transport.describe()),
                    classify_ssh_failure("", Some(&e), None),
                ))
            }
        };
        let io = LinkIo {
            read: Box::new(proc.stdout.take().expect("piped stdout")),
            write: Box::new(proc.stdin.take().expect("piped stdin")),
        };
        Ok(Dialed {
            io,
            conn: Box::new(proc),
        })
    }

    fn describe(&self) -> String {
        self.transport.describe()
    }
}

impl Connection for Proc {
    fn pid(&self) -> Option<u32> {
        Some(Proc::pid(self))
    }

    fn end(&self) {
        self.child.kill_and_wait(Duration::from_secs(2));
    }

    fn diagnose(&self, m: String) -> Diagnosis {
        // Let ssh finish so its stderr and exit code are complete.
        let status = self.child.wait_timeout(Duration::from_secs(1));
        self.child.kill_and_wait(Duration::from_secs(2));
        self.wait_stderr(Duration::from_millis(500));
        let stderr = self.stderr_text();
        let code = status.and_then(|s| s.code());
        let reinstall = code == Some(127) || stderr.contains("No such file");
        Diagnosis {
            hint: classify_ssh_failure(&stderr, None, code),
            message: if stderr.is_empty() { m } else { stderr },
            reinstall,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) use unix::{connect_nb, Pending};
#[cfg(unix)]
pub use unix::{connect_unix, UnixSocketDialer, CONNECT_TIMEOUT};
#[cfg(windows)]
pub use win::{connect_pipe, NamedPipeDialer, CONNECT_TIMEOUT};

/// The stream to a Daemon on this machine: a Unix socket, or a named pipe on Windows.
#[cfg(unix)]
pub type LocalStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type LocalStream = xshell_core::pipe::PipeStream;

/// Connect to the Daemon endpoint `path` on this machine (see [`connect_unix`] and
/// `connect_pipe`): bounded by `timeout`, prompt on `cancel`. A missing Daemon fails with
/// [`DialError::no_listener`].
#[cfg(unix)]
pub fn connect_local(
    path: &std::path::Path,
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<LocalStream, DialError> {
    connect_unix(path, cancel, timeout)
}

#[cfg(windows)]
pub fn connect_local(
    path: &std::path::Path,
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<LocalStream, DialError> {
    connect_pipe(path, cancel, timeout)
}

impl Dialed {
    /// A connected local stream as a link stream, shut down when `cancel` fires, when the
    /// connection ends, and when it is dropped.
    pub fn from_local_stream(s: LocalStream, cancel: &CancelToken) -> std::io::Result<Dialed> {
        #[cfg(unix)]
        {
            Dialed::from_unix_stream(s, cancel)
        }
        #[cfg(windows)]
        {
            Dialed::from_pipe_stream(s, cancel)
        }
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use crate::cancel::CancelHook;
    use std::io;
    use std::net::Shutdown;
    use std::path::{Path, PathBuf};
    use std::time::Instant;
    use xshell_core::pipe::PipeStream;

    /// A pipe that exists answers at once; this only bounds every instance being busy.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

    /// A Daemon's named pipe on this machine (`\\.\pipe\xshelld-<SID>`).
    pub struct NamedPipeDialer {
        pub name: PathBuf,
    }

    impl Dialer for NamedPipeDialer {
        fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError> {
            let stream = connect_pipe(&self.name, cancel, CONNECT_TIMEOUT)?;
            Dialed::from_pipe_stream(stream, cancel).map_err(|e| failed(&self.name, e))
        }

        fn describe(&self) -> String {
            self.name.display().to_string()
        }
    }

    fn failed(name: &Path, e: io::Error) -> DialError {
        DialError::Failed {
            message: format!("cannot connect to {}: {e}", name.display()),
            hint: None,
            kind: Some(e.kind()),
        }
    }

    /// Connect to the pipe `name`, never blocking past `timeout` or a cancel: while every
    /// instance is busy it waits in short steps, re-checking the token. The pipe must be
    /// owned by this user.
    pub fn connect_pipe(
        name: &Path,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<PipeStream, DialError> {
        let deadline = Instant::now() + timeout;
        match xshell_core::pipe::connect(name, deadline, &|| cancel.is_cancelled()) {
            Ok(s) => Ok(s),
            Err(_) if cancel.is_cancelled() => Err(DialError::Cancelled),
            Err(e) => Err(failed(name, e)),
        }
    }

    /// A pipe has no process to kill: shutting it down ends the link's reader and writer.
    struct PipeConn {
        stream: PipeStream,
        _hook: CancelHook,
    }

    impl Connection for PipeConn {
        fn pid(&self) -> Option<u32> {
            None
        }

        fn end(&self) {
            let _ = self.stream.shutdown(Shutdown::Both);
        }

        fn diagnose(&self, link_message: String) -> Diagnosis {
            Diagnosis {
                message: link_message,
                hint: None,
                reinstall: false,
            }
        }
    }

    impl Drop for PipeConn {
        fn drop(&mut self) {
            self.end();
        }
    }

    impl Dialed {
        /// A connected pipe as a link stream, shut down when `cancel` fires (at once when it
        /// already has), when the connection ends, and when it is dropped.
        pub fn from_pipe_stream(stream: PipeStream, cancel: &CancelToken) -> io::Result<Dialed> {
            let read = stream.try_clone()?;
            let write = stream.try_clone()?;
            let on_cancel = stream.try_clone()?;
            let hook = cancel.on_cancel(move || {
                let _ = on_cancel.shutdown(Shutdown::Both);
            });
            Ok(Dialed {
                io: LinkIo {
                    read: Box::new(read),
                    write: Box::new(write),
                },
                conn: Box::new(PipeConn {
                    stream,
                    _hook: hook,
                }),
            })
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::cancel::CancelHook;
    use std::io;
    use std::net::Shutdown;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    /// A local socket answers at once or not at all; this only bounds a full backlog.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
    const POLL_MS: i32 = 10;

    /// The Daemon's own socket on this machine (`xshelld`'s `server.sock`).
    pub struct UnixSocketDialer {
        pub path: PathBuf,
    }

    impl Dialer for UnixSocketDialer {
        fn dial(&self, cancel: &CancelToken) -> Result<Dialed, DialError> {
            let stream = connect_unix(&self.path, cancel, CONNECT_TIMEOUT)?;
            Dialed::from_unix_stream(stream, cancel).map_err(|e| failed(&self.path, e))
        }

        fn describe(&self) -> String {
            self.path.display().to_string()
        }
    }

    fn failed(path: &Path, e: io::Error) -> DialError {
        DialError::Failed {
            message: format!("cannot connect to {}: {e}", path.display()),
            hint: None,
            kind: Some(e.kind()),
        }
    }

    /// Connect to `path` without ever blocking past `timeout` or a cancel: the connect is
    /// non-blocking and re-checks the token while the listener's backlog is full.
    pub fn connect_unix(
        path: &Path,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<UnixStream, DialError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.is_cancelled() {
                return Err(DialError::Cancelled);
            }
            match connect_nb(path) {
                Ok(Pending::Done(s)) => return Ok(s),
                Ok(Pending::InProgress(fd)) => return finish(fd, path, cancel, deadline),
                // Linux: the backlog is full. Nothing to wait on but time.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(failed(path, io::Error::from(io::ErrorKind::TimedOut)));
                    }
                    cancel.sleep(Duration::from_millis(POLL_MS as u64));
                }
                Err(e) => return Err(failed(path, e)),
            }
        }
    }

    pub(crate) enum Pending {
        Done(UnixStream),
        InProgress(OwnedFd),
    }

    /// One non-blocking `connect`. The stream is blocking again once connected.
    pub(crate) fn connect_nb(path: &Path) -> io::Result<Pending> {
        let bytes = path.as_os_str().as_bytes();
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path too long",
            ));
        }
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
            *d = *s as libc::c_char;
        }
        let fd = nonblocking_socket()?;
        let raw = fd.as_raw_fd();
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let r = unsafe { libc::connect(raw, &addr as *const _ as *const libc::sockaddr, len) };
        if r == 0 {
            return Ok(Pending::Done(into_blocking(fd)?));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINPROGRESS) => Ok(Pending::InProgress(fd)),
            Some(libc::EAGAIN) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            _ => Err(e),
        }
    }

    /// A close-on-exec, non-blocking unix stream socket: atomically where the platform
    /// allows it, otherwise through checked `fcntl` calls.
    fn nonblocking_socket() -> io::Result<OwnedFd> {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        {
            let ty = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
            let fd = unsafe { libc::socket(libc::AF_UNIX, ty, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        )))]
        {
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let raw = fd.as_raw_fd();
            let check = |r: libc::c_int| {
                if r < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(r)
                }
            };
            check(unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) })?;
            let fl = check(unsafe { libc::fcntl(raw, libc::F_GETFL) })?;
            check(unsafe { libc::fcntl(raw, libc::F_SETFL, fl | libc::O_NONBLOCK) })?;
            Ok(fd)
        }
    }

    /// Back to blocking mode for the link's reader and writer (a checked call).
    fn into_blocking(fd: OwnedFd) -> io::Result<UnixStream> {
        let s = UnixStream::from(fd);
        s.set_nonblocking(false)?;
        Ok(s)
    }

    /// Wait for an in-progress connect (not Linux: its unix sockets never report one).
    fn finish(
        fd: OwnedFd,
        path: &Path,
        cancel: &CancelToken,
        deadline: Instant,
    ) -> Result<UnixStream, DialError> {
        loop {
            if cancel.is_cancelled() {
                return Err(DialError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(failed(path, io::Error::from(io::ErrorKind::TimedOut)));
            }
            let mut p = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut p, 1, POLL_MS) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(failed(path, e));
            }
            if r == 0 {
                continue;
            }
            let mut err: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let r = unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut err as *mut _ as *mut libc::c_void,
                    &mut len,
                )
            };
            if r < 0 {
                return Err(failed(path, io::Error::last_os_error()));
            }
            if err != 0 {
                return Err(failed(path, io::Error::from_raw_os_error(err)));
            }
            return into_blocking(fd).map_err(|e| failed(path, e));
        }
    }

    /// A socket has no process to kill: shutting it down ends the link's reader and writer.
    struct SocketConn {
        stream: UnixStream,
        _hook: CancelHook,
    }

    impl Connection for SocketConn {
        fn pid(&self) -> Option<u32> {
            None
        }

        fn end(&self) {
            let _ = self.stream.shutdown(Shutdown::Both);
        }

        fn diagnose(&self, link_message: String) -> Diagnosis {
            Diagnosis {
                message: link_message,
                hint: None,
                reinstall: false,
            }
        }
    }

    impl Drop for SocketConn {
        fn drop(&mut self) {
            self.end();
        }
    }

    impl Dialed {
        /// A connected socket as a link stream, shut down when `cancel` fires (at once when
        /// it already has), when the connection ends, and when it is dropped.
        pub fn from_unix_stream(stream: UnixStream, cancel: &CancelToken) -> io::Result<Dialed> {
            let read = stream.try_clone()?;
            let write = stream.try_clone()?;
            let on_cancel = stream.try_clone()?;
            let hook = cancel.on_cancel(move || {
                let _ = on_cancel.shutdown(Shutdown::Both);
            });
            Ok(Dialed {
                io: LinkIo {
                    read: Box::new(read),
                    write: Box::new(write),
                },
                conn: Box::new(SocketConn {
                    stream,
                    _hook: hook,
                }),
            })
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[cfg(target_os = "linux")]
    use super::unix::{connect_nb, Pending};
    use super::*;
    use crate::link::testpeer::{hello_frame, terminals_frame, Ev, Rec};
    use crate::link::Link;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Instant;
    use xshell_protocol::msg::ProtocolRange;

    const R11: ProtocolRange = ProtocolRange { min: 1, max: 1 };
    const T5: Duration = Duration::from_secs(5);

    /// Accepts once and greets like a Daemon; hands the server side back.
    fn greeter(path: &Path) -> std::thread::JoinHandle<UnixStream> {
        let l = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut b = hello_frame(1, 1, "1.5.0");
            b.extend(terminals_frame(vec![]));
            s.write_all(&b).unwrap();
            s
        })
    }

    fn dial(path: &Path, cancel: &CancelToken) -> Dialed {
        UnixSocketDialer { path: path.into() }
            .dial(cancel)
            .unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// Read until EOF (bounded); bytes the Desktop sent (its hello) are skipped. Only a
    /// read of 0 bytes is EOF; any other error fails.
    fn reads_eof(s: &mut UnixStream, within: Duration) -> Result<(), String> {
        use std::io::ErrorKind::{Interrupted, TimedOut, WouldBlock};
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let deadline = Instant::now() + within;
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            match s.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), WouldBlock | TimedOut | Interrupted) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Err(format!("no EOF within {within:?}"))
    }

    #[test]
    fn socket_dial_establishes_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let peer = greeter(&path);
        let d = dial(&path, &CancelToken::new());
        assert_eq!(d.conn.pid(), None);
        let rec = Rec::new();
        let (link, hello, _) = Link::establish(d.io, R11, "1.5.0", rec.clone(), T5).unwrap();
        assert_eq!(hello.version, "1.5.0");
        link.close();
        drop(peer.join().unwrap());
    }

    #[test]
    fn socket_dial_missing_path_fails_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        match (UnixSocketDialer { path: path.clone() }).dial(&CancelToken::new()) {
            Err(DialError::Failed {
                message,
                hint,
                kind,
            }) => {
                assert!(message.contains(&path.display().to_string()), "{message}");
                assert_eq!(hint, None);
                assert_eq!(kind, Some(std::io::ErrorKind::NotFound));
            }
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("connected to nothing"),
        }
    }

    #[test]
    fn socket_cancel_shuts_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let peer = greeter(&path);
        let cancel = CancelToken::new();
        let d = dial(&path, &cancel);
        let rec = Rec::new();
        let (_link, _, _) = Link::establish(d.io, R11, "1.5.0", rec.clone(), T5).unwrap();
        let _server = peer.join().unwrap();
        let start = Instant::now();
        cancel.cancel();
        rec.wait_for(|e| e.iter().any(|e| matches!(e, Ev::Closed(_))));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        drop(d.conn);
    }

    #[test]
    fn socket_end_and_drop_shut_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let peer = greeter(&path);
        let d = dial(&path, &CancelToken::new());
        let mut server = peer.join().unwrap();
        d.conn.end();
        reads_eof(&mut server, Duration::from_secs(2)).expect("EOF after end");

        let peer = {
            std::fs::remove_file(&path).unwrap();
            greeter(&path)
        };
        let d = dial(&path, &CancelToken::new());
        let mut server = peer.join().unwrap();
        // The link halves are still open: only dropping the connection shuts the socket.
        let io = d.io;
        drop(d.conn);
        reads_eof(&mut server, Duration::from_secs(2)).expect("EOF after drop");
        drop(io);
    }

    /// A listener that never accepts, its backlog full: the dial must not block on it.
    #[cfg(target_os = "linux")]
    #[test]
    fn socket_dial_with_full_backlog_returns_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let l = UnixListener::bind(&path).unwrap();
        // Shrink the backlog, then fill it.
        use std::os::fd::AsRawFd;
        assert_eq!(unsafe { libc::listen(l.as_raw_fd(), 0) }, 0);
        let mut queued = Vec::new();
        loop {
            match connect_nb(&path) {
                Ok(Pending::Done(s)) => queued.push(s),
                Ok(Pending::InProgress(_)) => panic!("unexpected in-progress connect"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
            assert!(queued.len() < 1000, "the backlog never filled");
        }
        let cancel = CancelToken::new();
        let c = cancel.clone();
        let p = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = (UnixSocketDialer { path: p }).dial(&c);
            let _ = tx.send(r.err().map(|e| matches!(e, DialError::Cancelled)));
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            rx.try_recv().is_err(),
            "the dial did not wait for the backlog"
        );
        let start = Instant::now();
        cancel.cancel();
        let cancelled = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("dial returned");
        assert_eq!(cancelled, Some(true));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(l);
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    use crate::link::testpeer::{hello_frame, terminals_frame, Ev, Rec};
    use crate::link::Link;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::time::Instant;
    use xshell_core::pipe::{PipeListener, PipeStream};
    use xshell_protocol::msg::ProtocolRange;

    const R11: ProtocolRange = ProtocolRange { min: 1, max: 1 };
    const T5: Duration = Duration::from_secs(5);

    fn unique() -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\xshell-test-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// Accepts once and greets like a Daemon; hands the server side back.
    fn greeter(name: &Path) -> std::thread::JoinHandle<PipeStream> {
        let l = PipeListener::bind(name).unwrap();
        std::thread::spawn(move || {
            let mut s = l.accept().unwrap();
            let mut b = hello_frame(1, 1, "1.5.0");
            b.extend(terminals_frame(vec![]));
            s.write_all(&b).unwrap();
            s
        })
    }

    fn dial(name: &Path, cancel: &CancelToken) -> Dialed {
        NamedPipeDialer { name: name.into() }
            .dial(cancel)
            .unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// Read until end of file (bounded), skipping what the Desktop sent.
    fn reads_eof(s: &PipeStream, within: Duration) -> Result<(), String> {
        let deadline = Instant::now() + within;
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            match s.read_within(&mut buf, Some(Duration::from_millis(50))) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Err(format!("no EOF within {within:?}"))
    }

    #[test]
    fn pipe_dial_establishes_link() {
        let name = unique();
        let peer = greeter(&name);
        let d = dial(&name, &CancelToken::new());
        assert_eq!(d.conn.pid(), None);
        let rec = Rec::new();
        let (link, hello, _) = Link::establish(d.io, R11, "1.5.0", rec.clone(), T5).unwrap();
        assert_eq!(hello.version, "1.5.0");
        link.close();
        drop(peer.join().unwrap());
    }

    #[test]
    fn pipe_dial_missing_is_no_listener() {
        let name = unique();
        match (NamedPipeDialer { name: name.clone() }).dial(&CancelToken::new()) {
            Err(e) => {
                assert!(e.no_listener(), "{e:?}");
                let DialError::Failed { message, .. } = e else {
                    unreachable!()
                };
                assert!(message.contains(&name.display().to_string()), "{message}");
            }
            Ok(_) => panic!("connected to nothing"),
        }
    }

    #[test]
    fn pipe_cancel_shuts_stream() {
        let name = unique();
        let peer = greeter(&name);
        let cancel = CancelToken::new();
        let d = dial(&name, &cancel);
        let rec = Rec::new();
        let (_link, _, _) = Link::establish(d.io, R11, "1.5.0", rec.clone(), T5).unwrap();
        let _server = peer.join().unwrap();
        let start = Instant::now();
        cancel.cancel();
        rec.wait_for(|e| e.iter().any(|e| matches!(e, Ev::Closed(_))));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        drop(d.conn);
    }

    #[test]
    fn pipe_end_and_drop_shut_stream() {
        let name = unique();
        let peer = greeter(&name);
        let d = dial(&name, &CancelToken::new());
        let server = peer.join().unwrap();
        d.conn.end();
        reads_eof(&server, Duration::from_secs(2)).expect("EOF after end");

        let name = unique();
        let peer = greeter(&name);
        let d = dial(&name, &CancelToken::new());
        let server = peer.join().unwrap();
        // The link halves are still open: only dropping the connection shuts the pipe.
        let io = d.io;
        drop(d.conn);
        reads_eof(&server, Duration::from_secs(2)).expect("EOF after drop");
        drop(io);
        let mut rest = Vec::new();
        let _ = (&server).read_to_end(&mut rest);
    }
}

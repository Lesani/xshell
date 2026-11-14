//! Short, bounded user-private native IPC, shared by the counterpart client and Daemon.
//! Callers must hold their existing process-owner lock before binding or stale cleanup.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

/// The original endpoint request deadline plus its server's shutdown cancellation.
/// Store and in-process lock admission use this same budget, never a renewed timeout.
#[derive(Clone)]
pub struct RequestBudget {
    until: Instant,
    stop: Arc<AtomicBool>,
}
impl RequestBudget {
    /// Reuse bounded lock admission for an explicit in-process operation. This carries
    /// only its original deadline/cancellation, never local confirmation authority.
    pub fn for_operation(until: Instant, stop: Arc<AtomicBool>) -> Self {
        Self { until, stop }
    }

    pub fn check(&self) -> io::Result<()> {
        if self.stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "counterpart endpoint is stopping",
            ));
        }
        if Instant::now() >= self.until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "counterpart request deadline",
            ));
        }
        Ok(())
    }
    pub fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> io::Result<MutexGuard<'a, T>> {
        loop {
            self.check()?;
            match mutex.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

#[cfg(any(unix, windows))]
mod native {
    use super::RequestBudget;
    use serde::{de::DeserializeOwned, Serialize};
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    const IO: Duration = Duration::from_secs(5);
    const MAX: usize = xshell_protocol::ring::companion::MAX_COMPANION_FRAME;
    #[cfg(unix)]
    use platform::denied;
    pub use platform::{check_path, private_dir, protected_connect, ProtectedListener, Stream};
    use platform::{dial_pinned, endpoint_identity, peer, Identity};
    #[cfg(test)]
    thread_local! {
        static AFTER_PIN_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }
    /// Pins local routing identity across a command's exchanges; not wire authority.
    /// Unix-only: it pins the socket inode. Windows pipes have no comparable instance
    /// identity (the owner-SID check distinguishes users, not endpoint lifetimes), so a
    /// Windows version would not detect replacement and is deliberately not offered.
    #[cfg(unix)]
    pub struct ProtectedEndpoint {
        path: PathBuf,
        identity: Identity,
    }
    #[cfg(unix)]
    impl ProtectedEndpoint {
        pub fn pin(path: PathBuf) -> io::Result<Self> {
            let identity = endpoint_identity(&path)?;
            Ok(Self { path, identity })
        }
        pub fn exchange_until<Q: Serialize, R: DeserializeOwned>(
            &self,
            request: &Q,
            budget: &RequestBudget,
        ) -> io::Result<R> {
            budget.check()?;
            if endpoint_identity(&self.path)? != self.identity {
                return Err(denied("protected owner endpoint replaced"));
            }
            #[cfg(test)]
            {
                let hook = AFTER_PIN_CHECK.with(|slot| slot.borrow_mut().take());
                if let Some(hook) = hook {
                    hook();
                }
            }
            let reply =
                exchange_with_identity_until(&self.path, request, budget, Some(self.identity))?;
            if endpoint_identity(&self.path)? != self.identity {
                return Err(denied("protected owner endpoint replaced"));
            }
            Ok(reply)
        }
    }
    struct Bounded {
        stream: Stream,
        until: Instant,
    }
    impl Bounded {
        fn left(&self) -> io::Result<Duration> {
            self.until
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC deadline"))
        }
    }
    impl Read for Bounded {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            super::tolerate_closed_peer(self.stream.set_read_timeout(Some(self.left()?)))?;
            self.stream.read(bytes)
        }
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            super::tolerate_closed_peer(self.stream.set_write_timeout(Some(self.left()?)))?;
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }
    fn read<T: DeserializeOwned>(stream: &mut Bounded) -> io::Result<T> {
        let mut header = [0; 4];
        stream.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame bounds",
            ));
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
    fn write<T: Serialize>(stream: &mut Bounded, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.is_empty() || bytes.len() > MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame bounds",
            ));
        }
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)
    }
    pub fn exchange<Q: Serialize, R: DeserializeOwned>(path: &Path, request: &Q) -> io::Result<R> {
        let budget =
            RequestBudget::for_operation(Instant::now() + IO, Arc::new(AtomicBool::new(false)));
        exchange_until(path, request, &budget)
    }
    /// Preserve an accepted operation's original bounded install allowance at local ingress.
    pub fn exchange_until<Q: Serialize, R: DeserializeOwned>(
        path: &Path,
        request: &Q,
        budget: &RequestBudget,
    ) -> io::Result<R> {
        exchange_with_identity_until(path, request, budget, None)
    }
    /// [`exchange_until`] that refuses, before sending anything, an endpoint whose serving
    /// process is not `server` (a squatter pipe or a successor of the expected Daemon).
    #[cfg(windows)]
    pub fn exchange_with_server_until<Q: Serialize, R: DeserializeOwned>(
        path: &Path,
        request: &Q,
        budget: &RequestBudget,
        server: &super::ServerProcess,
    ) -> io::Result<R> {
        exchange_checked_until(path, request, budget, None, Some(server))
    }
    fn exchange_with_identity_until<Q: Serialize, R: DeserializeOwned>(
        path: &Path,
        request: &Q,
        budget: &RequestBudget,
        expected: Option<Identity>,
    ) -> io::Result<R> {
        exchange_checked_until(path, request, budget, expected, None)
    }
    fn exchange_checked_until<Q: Serialize, R: DeserializeOwned>(
        path: &Path,
        request: &Q,
        budget: &RequestBudget,
        expected: Option<Identity>,
        #[cfg(windows)] server: Option<&super::ServerProcess>,
        #[cfg(not(windows))] server: Option<&()>,
    ) -> io::Result<R> {
        budget.check()?;
        let before = match expected {
            Some(identity) => identity,
            None => endpoint_identity(path)?,
        };
        let until = budget.until.min(Instant::now() + IO);
        let stream = dial_pinned(path, until, before)?;
        #[cfg(windows)]
        if let Some(server) = server {
            server.check_serves(&stream)?;
        }
        #[cfg(not(windows))]
        let _ = server;
        let mut stream = Bounded { stream, until };
        write(&mut stream, request)?;
        let response = read(&mut stream)?;
        budget.check()?;
        Ok(response)
    }
    pub struct Server {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Server {
        /// Stop admission, cancel request budgets, drain workers and remove only our endpoint.
        /// Must run outside any lock the request handlers can acquire.
        pub fn shutdown(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.shutdown();
        }
    }
    pub fn serve<Q: DeserializeOwned + Send + 'static, R: Serialize + Send + 'static>(
        path: PathBuf,
        handler: impl Fn(Q, RequestBudget) -> R + Send + Sync + 'static,
    ) -> io::Result<Server> {
        let listener = ProtectedListener::bind(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let handler = Arc::new(handler);
        let thread = std::thread::Builder::new()
            .name("companion-ipc".into())
            .spawn(move || {
                let mut workers = Vec::<(std::thread::JoinHandle<()>, Stream)>::new();
                while !done.load(Ordering::Acquire) {
                    let mut index = 0;
                    while index < workers.len() {
                        if workers[index].0.is_finished() {
                            let (worker, _) = workers.swap_remove(index);
                            let _ = worker.join();
                        } else {
                            index += 1;
                        }
                    }
                    if let Ok(stream) = listener.accept() {
                        if workers.len() < 4 && peer(&stream).is_ok() {
                            let Ok(shutdown) = stream.try_clone() else {
                                continue;
                            };
                            let handler = handler.clone();
                            let budget = RequestBudget {
                                until: Instant::now() + IO,
                                stop: done.clone(),
                            };
                            let worker = std::thread::spawn(move || {
                                let mut stream = Bounded {
                                    stream,
                                    until: budget.until,
                                };
                                if let Ok(request) = read(&mut stream) {
                                    if budget.check().is_ok() {
                                        let _ = write(&mut stream, &handler(request, budget));
                                    }
                                }
                            });
                            workers.push((worker, shutdown));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                drop(listener);
                for (_, stream) in &workers {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
                for (worker, _) in workers {
                    let _ = worker.join();
                }
            })?;
        Ok(Server {
            stop,
            thread: Some(thread),
        })
    }

    #[cfg(unix)]
    mod platform {
        use super::*;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        use std::os::unix::net::{UnixListener, UnixStream};
        pub type Stream = UnixStream;
        pub(super) type Identity = (u64, u64);
        pub(super) fn endpoint_identity(path: &Path) -> io::Result<Identity> {
            socket_identity(path)
        }
        pub(super) fn dial_pinned(
            path: &Path,
            deadline: Instant,
            before: Identity,
        ) -> io::Result<Stream> {
            let stream = dial(path, deadline)?;
            peer(&stream)?;
            if socket_identity(path)? != before {
                return Err(denied("IPC endpoint replaced during connect"));
            }
            Ok(stream)
        }
        pub(super) fn denied(why: &str) -> io::Error {
            io::Error::new(io::ErrorKind::PermissionDenied, why)
        }
        pub fn private_dir(path: &Path) -> io::Result<()> {
            let meta = std::fs::symlink_metadata(path)?;
            if !meta.is_dir()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o077 != 0
                || path.canonicalize()? != path
            {
                return Err(denied("non-private or ambiguous IPC directory"));
            }
            Ok(())
        }
        fn socket_identity(path: &Path) -> io::Result<(u64, u64)> {
            private_dir(path.parent().ok_or_else(|| denied("socket parent"))?)?;
            let meta = std::fs::symlink_metadata(path)?;
            if !meta.file_type().is_socket()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o077 != 0
            {
                return Err(denied("foreign, symlink or non-private IPC socket"));
            }
            Ok((meta.dev(), meta.ino()))
        }
        pub(super) fn peer(stream: &UnixStream) -> io::Result<()> {
            #[cfg(target_os = "linux")]
            {
                let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_PEERCRED,
                        (&mut cred as *mut libc::ucred).cast(),
                        &mut len,
                    )
                } != 0
                    || len as usize != std::mem::size_of_val(&cred)
                    || cred.uid != unsafe { libc::geteuid() }
                {
                    return Err(denied("IPC peer uid mismatch"));
                }
                Ok(())
            }
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                let (mut uid, mut gid) = (0, 0);
                if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0
                    || uid != unsafe { libc::geteuid() }
                {
                    return Err(denied("IPC peer uid mismatch"));
                }
                Ok(())
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
            {
                let _ = stream;
                Err(denied("IPC peer credentials unsupported on this platform"))
            }
        }
        pub fn check_path(path: &Path) -> io::Result<()> {
            let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            let bytes = path.as_os_str().as_bytes();
            if bytes.len() >= address.sun_path.len() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "companion Unix socket path exceeds platform limit",
                ));
            }
            if bytes.is_empty() || bytes.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid IPC path",
                ));
            }
            Ok(())
        }
        pub(super) fn dial(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
            check_path(path)?;
            let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            let bytes = path.as_os_str().as_bytes();
            address.sun_family = libc::AF_UNIX as _;
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                address.sun_len = std::mem::size_of_val(&address) as _;
            }
            for (to, from) in address.sun_path.iter_mut().zip(bytes) {
                *to = *from as _;
            }
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
                || unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe {
                libc::connect(
                    fd,
                    (&address as *const libc::sockaddr_un).cast(),
                    std::mem::size_of_val(&address) as _,
                )
            } != 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINPROGRESS) {
                    return Err(error);
                }
                let mut event = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero()
                    || unsafe {
                        libc::poll(&mut event, 1, left.as_millis().min(i32::MAX as u128) as i32)
                    } <= 0
                {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "IPC connect deadline",
                    ));
                }
                let (mut error, mut len) = (0_i32, std::mem::size_of::<i32>() as libc::socklen_t);
                if unsafe {
                    libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut error as *mut i32).cast(),
                        &mut len,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if error != 0 {
                    return Err(io::Error::from_raw_os_error(error));
                }
            }
            let stream = UnixStream::from(owned);
            stream.set_nonblocking(false)?;
            Ok(stream)
        }
        /// A persistent user-private endpoint. The caller must hold its process-owner lock.
        /// This shares only protection with companion IPC, not its five-second request budget.
        pub struct ProtectedListener {
            listener: UnixListener,
            path: PathBuf,
            identity: (u64, u64),
        }
        impl ProtectedListener {
            pub fn bind(path: PathBuf) -> io::Result<Self> {
                private_dir(path.parent().ok_or_else(|| denied("socket parent"))?)?;
                check_path(&path)?;
                if std::fs::symlink_metadata(&path).is_ok() {
                    let old = socket_identity(&path)?;
                    match dial(&path, Instant::now() + IO) {
                        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::AddrInUse,
                                "IPC endpoint is live or ambiguous",
                            ))
                        }
                    }
                    if socket_identity(&path)? != old {
                        return Err(denied("stale IPC endpoint changed"));
                    }
                    std::fs::remove_file(&path)?;
                }
                let listener = UnixListener::bind(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                let identity = socket_identity(&path)?;
                listener.set_nonblocking(true)?;
                Ok(Self {
                    listener,
                    path,
                    identity,
                })
            }
            pub fn accept(&self) -> io::Result<UnixStream> {
                let (stream, _) = self.listener.accept()?;
                peer(&stream)?;
                // BSD/macOS accept() copies the listener's O_NONBLOCK; Linux does not. A
                // non-blocking stream ignores the read/write timeouts and fails with WouldBlock.
                stream.set_nonblocking(false)?;
                Ok(stream)
            }
        }
        impl Drop for ProtectedListener {
            fn drop(&mut self) {
                if socket_identity(&self.path).ok() == Some(self.identity) {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
        pub fn protected_connect(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
            let before = socket_identity(path)?;
            let stream = dial(path, Instant::now() + timeout)?;
            peer(&stream)?;
            if socket_identity(path)? != before {
                return Err(denied("IPC endpoint replaced during connect"));
            }
            Ok(stream)
        }
    }
    #[cfg(windows)]
    mod platform {
        use super::*;
        pub use crate::pipe::PipeStream as Stream;
        pub fn private_dir(path: &Path) -> io::Result<()> {
            crate::server::no_reparse(path)?;
            crate::private_fs::check_private_dir(path)?;
            if path.canonicalize()? != path {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "ambiguous IPC directory",
                ));
            }
            Ok(())
        }
        pub fn check_path(path: &Path) -> io::Result<()> {
            crate::pipe::check_pipe_name(path)
        }
        /// The connected client runs as this user (its token's user SID), not merely a
        /// process that could open the user-only pipe.
        pub(super) fn peer(stream: &Stream) -> io::Result<()> {
            if crate::pipe::client_user_is_current(stream)? {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "IPC peer user mismatch",
                ))
            }
        }
        /// Named pipes have no inode; the pinned identity is the validated name, and
        /// each connect re-verifies the server owner through `crate::pipe::connect`.
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(super) struct Identity;
        pub(super) fn endpoint_identity(path: &Path) -> io::Result<Identity> {
            check_path(path).map(|()| Identity)
        }
        pub(super) fn dial_pinned(
            path: &Path,
            deadline: Instant,
            _: Identity,
        ) -> io::Result<Stream> {
            crate::pipe::connect(path, deadline, &|| false)
        }
        pub fn protected_connect(path: &Path, timeout: Duration) -> io::Result<Stream> {
            crate::pipe::connect(path, Instant::now() + timeout, &|| false)
        }
        pub struct ProtectedListener(crate::pipe::PipeListener);
        impl ProtectedListener {
            pub fn bind(path: PathBuf) -> io::Result<Self> {
                crate::pipe::PipeListener::bind(&path).map(Self)
            }
            pub fn accept(&self) -> io::Result<Stream> {
                self.0
                    .accept_until(Instant::now() + Duration::from_millis(1), &|| false)
                    .map_err(|e| {
                        if e.kind() == io::ErrorKind::TimedOut {
                            io::Error::from(io::ErrorKind::WouldBlock)
                        } else {
                            e
                        }
                    })
            }
        }
    }
    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::{UnixListener, UnixStream};
        fn directory() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            dir
        }
        #[test]
        fn private_round_trip_live_endpoint_refusal_and_owner_stale_cleanup() {
            let dir = directory();
            let path = dir.path().canonicalize().unwrap().join("ipc.sock");
            let server = serve(path.clone(), |value: String, _| value).unwrap();
            assert_eq!(exchange::<_, String>(&path, &"hello").unwrap(), "hello");
            assert!(serve(path.clone(), |value: String, _| value).is_err());
            assert_eq!(
                exchange::<_, String>(&path, &"still live").unwrap(),
                "still live"
            );
            drop(server);
            assert!(!path.exists());
            let stale = UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            drop(stale);
            let server = serve(path.clone(), |value: String, _| value).unwrap();
            assert_eq!(
                exchange::<_, String>(&path, &"replaced stale").unwrap(),
                "replaced stale"
            );
            drop(server);
            assert!(!path.exists());
        }
        #[test]
        fn symlink_and_nonprivate_endpoint_are_denied_without_unlink() {
            let dir = directory();
            let path = dir.path().canonicalize().unwrap().join("ipc.sock");
            let target = dir.path().join("target");
            std::fs::write(&target, b"untouched").unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(serve(path.clone(), |value: String, _| value).is_err());
            assert!(exchange::<_, String>(&path, &"no").is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
            std::fs::remove_file(&path).unwrap();
            let server = serve(path.clone(), |value: String, _| value).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
            assert!(exchange::<_, String>(&path, &"no").is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            drop(server);
        }
        #[test]
        fn pinned_protected_endpoint_refuses_replacement_between_exchanges() {
            let dir = directory();
            let path = dir.path().canonicalize().unwrap().join("ipc.sock");
            let server = serve(path.clone(), |value: String, _| value).unwrap();
            let endpoint = ProtectedEndpoint::pin(path.clone()).unwrap();
            let budget =
                RequestBudget::for_operation(Instant::now() + IO, Arc::new(AtomicBool::new(false)));
            assert_eq!(
                endpoint
                    .exchange_until::<_, String>(&"first", &budget)
                    .unwrap(),
                "first"
            );
            // Keep the old inode allocated: this is replacement, not inode reuse.
            let retained = dir.path().join("retained.sock");
            std::fs::rename(&path, &retained).unwrap();
            drop(server);
            let _successor = serve(path.clone(), |value: String, _| value).unwrap();
            assert!(endpoint
                .exchange_until::<_, String>(&"second", &budget)
                .is_err());
            assert_eq!(
                exchange::<_, String>(&path, &"successor").unwrap(),
                "successor"
            );
        }
        #[test]
        fn pinned_protected_endpoint_refuses_successor_mutation_after_initial_check() {
            use std::sync::atomic::AtomicUsize;
            let dir = directory();
            let path = dir.path().canonicalize().unwrap().join("ipc.sock");
            let server = serve(path.clone(), |value: String, _| value).unwrap();
            let endpoint = ProtectedEndpoint::pin(path.clone()).unwrap();
            let requests = Arc::new(AtomicUsize::new(0));
            let mutations = Arc::new(AtomicUsize::new(0));
            let (send, receive) = std::sync::mpsc::channel();
            let successor_path = path.clone();
            let retained = dir.path().join("retained.sock");
            let successor_requests = requests.clone();
            let successor_mutations = mutations.clone();
            AFTER_PIN_CHECK.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    // Keep A's inode allocated and replace it exactly after the pin check.
                    std::fs::rename(&successor_path, retained).unwrap();
                    drop(server);
                    let successor = serve(successor_path, move |value: String, _| {
                        successor_requests.fetch_add(1, Ordering::SeqCst);
                        if value == "install" {
                            successor_mutations.fetch_add(1, Ordering::SeqCst);
                        }
                        value
                    })
                    .unwrap();
                    send.send(successor).unwrap();
                }));
            });
            let budget =
                RequestBudget::for_operation(Instant::now() + IO, Arc::new(AtomicBool::new(false)));
            let result = endpoint.exchange_until::<_, String>(&"install", &budget);
            let mut successor = receive.try_recv().expect("replacement hook ran");
            successor.shutdown();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(
                requests.load(Ordering::SeqCst),
                0,
                "successor received request"
            );
            assert_eq!(mutations.load(Ordering::SeqCst), 0, "successor mutated");
        }
        #[test]
        fn oversized_frame_is_closed_before_handler_and_next_request_still_works() {
            let dir = directory();
            let path = dir.path().canonicalize().unwrap().join("ipc.sock");
            let server = serve(path.clone(), |value: String, _| value).unwrap();
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.set_read_timeout(Some(IO)).unwrap();
            stream.write_all(&u32::MAX.to_be_bytes()).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
            assert_eq!(exchange::<_, String>(&path, &"bounded").unwrap(), "bounded");
            drop(server);
        }
    }
}
#[cfg(windows)]
pub use native::exchange_with_server_until;
#[cfg(unix)]
pub use native::ProtectedEndpoint;

/// The process expected to serve a protected Windows endpoint (BINDING Decision 4): the
/// Daemon the Desktop spawned, or the one its private PID record names. The process handle
/// is opened before any comparison and held for this value's life, so the PID cannot be
/// reused by another process while it is compared.
#[cfg(windows)]
pub struct ServerProcess {
    pid: u32,
    handle: std::os::windows::io::OwnedHandle,
}
/// The creation time (FILETIME, 100 ns units) of the process behind `process`.
#[cfg(windows)]
fn process_creation_time(process: windows_sys::Win32::Foundation::HANDLE) -> io::Result<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// This process's creation time, as [`process_creation_time`] reads it.
#[cfg(windows)]
pub fn current_process_creation_time() -> io::Result<u64> {
    process_creation_time(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
}

#[cfg(windows)]
impl ServerProcess {
    /// Open and hold `pid`. Fails when no such process exists or it has already exited.
    pub fn open(pid: u32) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        if pid == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "process id 0"));
        }
        let h = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if h.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "expected Daemon process {pid} is gone: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        let server = Self {
            pid,
            handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(h as _) },
        };
        if !server.alive() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("expected Daemon process {pid} has exited"),
            ));
        }
        Ok(server)
    }
    /// [`ServerProcess::open`], also requiring the process creation time a PID record
    /// stored: a recycled PID (another process generation) is refused.
    pub fn open_expected(pid: u32, creation_time: u64) -> io::Result<Self> {
        let server = Self::open(pid)?;
        let actual = server.creation_time()?;
        if actual != creation_time {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("process {pid} is another generation than the recorded Daemon"),
            ));
        }
        Ok(server)
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    /// The held process's creation time (FILETIME, 100 ns units).
    pub fn creation_time(&self) -> io::Result<u64> {
        use std::os::windows::io::AsRawHandle;
        process_creation_time(self.handle.as_raw_handle() as _)
    }
    /// The held process has not exited.
    pub fn alive(&self) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(self.handle.as_raw_handle() as _, 0) == WAIT_TIMEOUT }
    }
    /// `stream`'s serving process is this live process.
    pub fn check_serves(&self, stream: &crate::pipe::PipeStream) -> io::Result<()> {
        let served_by = stream.server_process_id()?;
        if served_by != self.pid || !self.alive() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "protected endpoint is served by process {served_by}, not the expected Daemon {}",
                    self.pid
                ),
            ));
        }
        Ok(())
    }
}
/// Applies a socket timeout result. On macOS and the BSDs, setting a timeout on a socket
/// whose peer has already closed fails with EINVAL even when its reply is still buffered;
/// the following read or write then reports the buffered data, EOF or EPIPE truthfully.
pub fn tolerate_closed_peer(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        #[cfg(all(unix, not(target_os = "linux")))]
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}
#[cfg(any(unix, windows))]
pub use native::{
    check_path, exchange, exchange_until, private_dir, protected_connect, serve, ProtectedListener,
    Server, Stream,
};

/// Validate traversed Windows ancestors before canonicalizing a protected store binding.
#[cfg(any(unix, windows))]
pub fn canonical_store(path: &std::path::Path) -> io::Result<std::path::PathBuf> {
    #[cfg(windows)]
    {
        crate::server::no_reparse(path)?;
        crate::private_fs::check_owned_dir(path)?;
    }
    let canonical = path.canonicalize()?;
    private_dir(&canonical)?;
    Ok(canonical)
}

#[cfg(any(unix, windows))]
pub fn companion_endpoint(store: &std::path::Path) -> io::Result<std::path::PathBuf> {
    endpoint(store, "companion", "companion.sock")
}
#[cfg(any(unix, windows))]
pub fn host_broker_endpoint(store: &std::path::Path) -> io::Result<std::path::PathBuf> {
    endpoint(store, "host-broker", "host-broker.sock")
}
#[cfg(any(unix, windows))]
fn endpoint(
    store: &std::path::Path,
    kind: &str,
    unix_name: &str,
) -> io::Result<std::path::PathBuf> {
    private_dir(store)?;
    #[cfg(unix)]
    {
        let _ = kind;
        Ok(store.join(unix_name))
    }
    #[cfg(windows)]
    {
        let _ = unix_name;
        let path = std::path::PathBuf::from(format!(
            r"\\.\pipe\xshell-{kind}-v1-{}",
            store_identity(store, kind)?
        ));
        crate::pipe::check_pipe_name(&path)?;
        Ok(path)
    }
}
/// The Unix spelling is retained; Windows binds Hello to the same SID/canonical UTF-16
/// root as the endpoint mapping, not HOME or a display/lossy path.
#[cfg(any(unix, windows))]
pub fn host_broker_profile(store: &std::path::Path) -> io::Result<String> {
    private_dir(store)?;
    #[cfg(unix)]
    {
        use sha2::{Digest, Sha256};
        Ok(format!(
            "{:x}",
            Sha256::digest(store.as_os_str().as_encoded_bytes())
        ))
    }
    #[cfg(windows)]
    {
        store_identity(store, "host-broker-profile")
    }
}
#[cfg(windows)]
fn store_identity(store: &std::path::Path, domain: &str) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::os::windows::ffi::OsStrExt;
    let sid = crate::pipe::current_user_sid()?.to_string_sid()?;
    let path: Vec<u8> = store
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut hash = Sha256::new();
    for field in [
        b"xshell-protected-ipc-v1".as_slice(),
        domain.as_bytes(),
        sid.as_bytes(),
        &path,
    ] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    fn store() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::private_fs::ensure_dir(dir.path()).unwrap();
        dir
    }
    #[test]
    fn endpoints_bind_kind_profile_and_lossless_utf16_root() {
        use std::os::windows::ffi::OsStringExt;
        let dir = store();
        let one = dir.path().join("one");
        let two = dir.path().join(std::ffi::OsString::from_wide(&[0xd800]));
        for p in [&one, &two] {
            crate::private_fs::ensure_dir(p).unwrap();
        }
        let one = canonical_store(&one).unwrap();
        let two = canonical_store(&two).unwrap();
        let a = companion_endpoint(&one).unwrap();
        assert_eq!(a, companion_endpoint(&one).unwrap());
        assert_ne!(a, companion_endpoint(&two).unwrap());
        assert_ne!(a, host_broker_endpoint(&one).unwrap());
        assert_ne!(
            host_broker_profile(&one).unwrap(),
            host_broker_profile(&two).unwrap()
        );
        crate::pipe::check_pipe_name(&a).unwrap();
    }
    #[test]
    fn private_directory_validation_refuses_broad_acl_without_rewriting_it() {
        let dir = store();
        let path = dir.path().join("caller-directory");
        let sid = crate::pipe::current_user_sid()
            .unwrap()
            .to_string_sid()
            .unwrap();
        crate::private_fs::create_dir_with_sddl(&path, &format!("O:{sid}D:P(A;OICI;FA;;;WD)"))
            .unwrap();
        let before = crate::private_fs::security_sddl(&path).unwrap();
        assert!(
            private_dir(&path.canonicalize().unwrap()).is_err(),
            "validation cannot make an arbitrary caller directory private"
        );
        assert_eq!(crate::private_fs::security_sddl(&path).unwrap(), before);
    }
    #[test]
    fn junction_ancestor_is_refused_before_canonicalization() {
        let dir = store();
        let target = dir.path().join("target");
        crate::private_fs::ensure_dir(&target.join("child")).unwrap();
        let junction = dir.path().join("junction");
        let status = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "junction fixture must execute, not skip");
        assert!(canonical_store(&junction.join("child")).is_err());
        std::fs::remove_dir(junction).unwrap();
    }
    #[test]
    fn private_round_trip_collision_and_successor_after_shutdown() {
        let dir = store();
        let path = companion_endpoint(&canonical_store(dir.path()).unwrap()).unwrap();
        let mut server = serve(path.clone(), |s: String, _| s).unwrap();
        assert_eq!(exchange::<_, String>(&path, &"hello").unwrap(), "hello");
        assert!(serve(path.clone(), |s: String, _| s).is_err());
        server.shutdown();
        let _successor = serve(path.clone(), |s: String, _| s).unwrap();
        assert_eq!(
            exchange::<_, String>(&path, &"successor").unwrap(),
            "successor"
        );
    }
    /// BINDING Decision 4: an exchange that names the expected serving process refuses,
    /// before sending anything, a pipe served by any other process, and an exited one.
    #[test]
    fn expected_server_process_is_checked_before_any_request() {
        let dir = store();
        let path = companion_endpoint(&canonical_store(dir.path()).unwrap()).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let _server = serve(path.clone(), move |s: String, _| {
            c.fetch_add(1, Ordering::SeqCst);
            s
        })
        .unwrap();
        let budget = || {
            RequestBudget::for_operation(
                Instant::now() + Duration::from_secs(5),
                Arc::new(AtomicBool::new(false)),
            )
        };
        let me = ServerProcess::open(std::process::id()).unwrap();
        assert_eq!(
            exchange_with_server_until::<_, String>(&path, &"mine", &budget(), &me).unwrap(),
            "mine"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Another live process of this user is not the server of this pipe.
        let mut other = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let expected = ServerProcess::open(other.id()).unwrap();
        let e = exchange_with_server_until::<_, String>(&path, &"squatted", &budget(), &expected)
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "nothing was sent");
        let _ = other.kill();
        let _ = other.wait();
        // The held handle pins the exited PID: it can never match a later server.
        assert!(!expected.alive());
        assert!(
            exchange_with_server_until::<_, String>(&path, &"late", &budget(), &expected).is_err()
        );
        assert!(ServerProcess::open(0).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn malformed_oversized_and_partial_frames_never_dispatch_and_shutdown_drains() {
        let dir = store();
        let path = companion_endpoint(&canonical_store(dir.path()).unwrap()).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let mut server = serve(path.clone(), move |s: String, _| {
            c.fetch_add(1, Ordering::SeqCst);
            s
        })
        .unwrap();
        for payload in [
            u32::MAX.to_be_bytes().to_vec(),
            [1_u32.to_be_bytes().as_slice(), b"!"].concat(),
        ] {
            let mut stream = protected_connect(&path, Duration::from_secs(1)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream.write_all(&payload).unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        }
        let mut partial = protected_connect(&path, Duration::from_secs(1)).unwrap();
        partial.write_all(&[0]).unwrap();
        let start = Instant::now();
        server.shutdown();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

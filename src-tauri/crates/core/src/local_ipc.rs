//! Short, bounded user-private Unix IPC, shared by the counterpart client and Daemon.
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

#[cfg(unix)]
mod unix {
    use super::RequestBudget;
    use serde::{de::DeserializeOwned, Serialize};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    const IO: Duration = Duration::from_secs(5);
    const MAX: usize = xshell_protocol::ring::companion::MAX_COMPANION_FRAME;
    fn denied(why: &str) -> io::Error {
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
    fn peer(stream: &UnixStream) -> io::Result<()> {
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
    fn dial(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
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
    struct Bounded {
        stream: UnixStream,
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
            self.stream.set_read_timeout(Some(self.left()?))?;
            self.stream.read(bytes)
        }
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.stream.set_write_timeout(Some(self.left()?))?;
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
        let before = socket_identity(path)?;
        let until = Instant::now() + IO;
        let stream = dial(path, until)?;
        peer(&stream)?;
        if socket_identity(path)? != before {
            return Err(denied("IPC endpoint replaced during connect"));
        }
        let mut stream = Bounded { stream, until };
        write(&mut stream, request)?;
        read(&mut stream)
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
                let mut workers = Vec::<(std::thread::JoinHandle<()>, UnixStream)>::new();
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

    #[cfg(test)]
    mod tests {
        use super::*;
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
#[cfg(unix)]
pub use unix::{
    check_path, exchange, private_dir, protected_connect, serve, ProtectedListener, Server,
};

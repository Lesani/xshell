//! Private pairing-only facade of the running Desktop's existing connection owner.
//! This implementation supports Linux; other targets fail closed, never use TCP fallback.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xshell_hostlink::ring::DesktopRing;
use xshell_protocol::pairing_access::{enrollment::*, AccessError};

pub fn endpoint() -> Result<PathBuf, AccessError> {
    let home = std::env::var_os("HOME").ok_or(AccessError::DesktopUnavailable)?;
    Ok(PathBuf::from(home).join(".xshell/pairing/desktop.sock"))
}
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use xshell_protocol::pairing_access::{read_frame, write_frame};
    const IO: Duration = Duration::from_secs(5);

    fn private(path: &Path, socket: bool) -> Result<(), AccessError> {
        let m = std::fs::symlink_metadata(path).map_err(|_| AccessError::DesktopUnavailable)?;
        if m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
            || if socket {
                !m.file_type().is_socket()
            } else {
                !m.file_type().is_dir()
            }
        {
            return Err(AccessError::PermissionRequired);
        }
        Ok(())
    }
    fn dial(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
        let bytes = path.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "endpoint"));
        }
        address.sun_family = libc::AF_UNIX as _;
        for (to, from) in address.sun_path.iter_mut().zip(bytes) {
            *to = *from as _;
        }
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as _,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            // AF_UNIX backlog saturation reports EAGAIN, not an in-flight connection.
            // Fail closed immediately rather than waiting on a blocking local connect.
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(error);
            }
            let mut event = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero()
                || unsafe {
                    libc::poll(
                        &mut event,
                        1,
                        remaining.as_millis().min(i32::MAX as u128) as i32,
                    )
                } <= 0
            {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
            }
            let mut error = 0_i32;
            let mut len = std::mem::size_of_val(&error) as libc::socklen_t;
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
    fn peer(stream: &UnixStream) -> Result<(), AccessError> {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } != 0
            || len as usize != std::mem::size_of::<libc::ucred>()
            || cred.uid != unsafe { libc::geteuid() }
        {
            return Err(AccessError::PermissionRequired);
        }
        Ok(())
    }
    struct Bounded {
        stream: UnixStream,
        deadline: Instant,
    }
    impl Bounded {
        fn remaining(&self) -> io::Result<Duration> {
            self.deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "deadline"))
        }
    }
    impl Read for Bounded {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.stream.set_read_timeout(Some(self.remaining()?))?;
            self.stream.read(out)
        }
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.stream.set_write_timeout(Some(self.remaining()?))?;
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }
    pub fn exchange(path: &Path, request: &DesktopRequest) -> Result<DesktopResponse, AccessError> {
        let dir = path.parent().ok_or(AccessError::PermissionRequired)?;
        private(dir.parent().ok_or(AccessError::PermissionRequired)?, false)?;
        private(dir, false)?;
        private(path, true)?;
        let deadline = Instant::now() + IO;
        let stream = dial(path, deadline).map_err(|_| AccessError::DesktopUnavailable)?;
        peer(&stream)?;
        let mut stream = Bounded { stream, deadline };
        write_frame(&mut stream, request)?;
        read_frame(&mut stream)
    }
    pub struct Server {
        stop: Arc<AtomicBool>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
        }
    }
    pub fn serve(ring: Arc<DesktopRing>, path: PathBuf) -> Result<Server, AccessError> {
        let dir = path.parent().ok_or(AccessError::PermissionRequired)?;
        // Reuse the Ring store's user-private directory handling, then strictly inspect.
        let parent = dir.parent().ok_or(AccessError::PermissionRequired)?;
        xshell_hostlink::ring::store::ensure_private_dir(parent)
            .map_err(|_| AccessError::PermissionRequired)?;
        xshell_hostlink::ring::store::ensure_private_dir(dir)
            .map_err(|_| AccessError::PermissionRequired)?;
        private(parent, false)?;
        private(dir, false)?;
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let ring = Arc::downgrade(&ring);
        std::thread::Builder::new()
            .name("desktop-pairing-ipc".into())
            .spawn(move || {
                let mut listener: Option<UnixListener> = None;
                let mut workers = Vec::<std::thread::JoinHandle<()>>::new();
                let mut bound_inode = None;
                while !done.load(Ordering::Acquire) {
                    let Some(ring) = ring.upgrade() else { break };
                    workers.retain(|w| !w.is_finished());
                    if !ring.pairing_ipc_owner() {
                        if listener.is_some() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    if listener.is_none() {
                        if std::fs::symlink_metadata(&path).is_ok() {
                            if private(&path, true).is_err() {
                                break;
                            }
                            match dial(&path, Instant::now() + IO) {
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                                    ) => {}
                                _ => {
                                    std::thread::sleep(Duration::from_millis(100));
                                    continue;
                                }
                            }
                            if std::fs::remove_file(&path).is_err() {
                                break;
                            }
                        }
                        let Ok(bound) = UnixListener::bind(&path) else {
                            break;
                        };
                        if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                            .is_err()
                        {
                            break;
                        }
                        bound_inode = std::fs::symlink_metadata(&path).ok().map(|m| m.ino());
                        if bound.set_nonblocking(true).is_err() {
                            break;
                        }
                        listener = Some(bound);
                    }
                    if let Ok((stream, _)) = listener.as_ref().unwrap().accept() {
                        if workers.len() < 4
                            && peer(&stream).is_ok()
                            && stream.set_nonblocking(false).is_ok()
                        {
                            workers.push(std::thread::spawn(move || {
                                let mut stream = Bounded {
                                    stream,
                                    deadline: Instant::now() + IO,
                                };
                                let response = match read_frame::<DesktopRequest>(&mut stream) {
                                    Ok(DesktopRequest::Scope {
                                        ring: id,
                                        device_name,
                                    }) => ring
                                        .enrollment_scope(&id, device_name)
                                        .map(|scope| DesktopResponse::Scope { scope }),
                                    Ok(DesktopRequest::Offer {
                                        task,
                                        operation,
                                        parameters,
                                    }) => ring
                                        .enrollment_offer(task, operation, *parameters)
                                        .map(|start| DesktopResponse::Offer { start }),
                                    Ok(DesktopRequest::Status { task, operation }) => {
                                        Ok(DesktopResponse::Operation {
                                            response: ring
                                                .enrollment_status(task, operation, false),
                                        })
                                    }
                                    Ok(DesktopRequest::Cancel { task, operation }) => {
                                        Ok(DesktopResponse::Operation {
                                            response: ring.enrollment_status(task, operation, true),
                                        })
                                    }
                                    Err(error) => Err(error),
                                }
                                .unwrap_or_else(|error| DesktopResponse::Error { error });
                                stream.deadline = Instant::now() + IO;
                                let _ = write_frame(&mut stream, &response);
                            }));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(listener);
                if bound_inode.is_some()
                    && std::fs::symlink_metadata(&path).ok().map(|m| m.ino()) == bound_inode
                {
                    let _ = std::fs::remove_file(path);
                }
            })
            .map_err(|_| AccessError::CoordinatorUnavailable)?;
        Ok(Server { stop })
    }
}
#[cfg(target_os = "linux")]
pub use linux::{exchange, serve, Server};
#[cfg(not(target_os = "linux"))]
pub struct Server;
#[cfg(not(target_os = "linux"))]
pub fn serve(_: Arc<DesktopRing>, _: PathBuf) -> Result<Server, AccessError> {
    Err(AccessError::PermissionRequired)
}
#[cfg(not(target_os = "linux"))]
pub fn exchange(_: &Path, _: &DesktopRequest) -> Result<DesktopResponse, AccessError> {
    Err(AccessError::PermissionRequired)
}

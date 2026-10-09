//! The Daemon's endpoint on Windows: a per-user named pipe, `\\.\pipe\xshelld-<user SID>`.
//!
//! Every instance is created with a security descriptor that names the user as owner and
//! grants access to the user only, rejects remote clients, and the first one with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name another process holds fails the start. A client
//! accepts a pipe only when its owner is the client's own user SID, so a pipe another user
//! created under our name first is refused.
//!
//! All I/O is overlapped: Windows serializes synchronous I/O on one file object, and the
//! Daemon and the Desktop read and write one pipe from two threads at once. Each operation
//! waits on its own event for at most its timeout, then cancels and waits for the
//! cancellation, so the buffer is never in use after the call returns.
//!
//! The pure helpers (names, the descriptor) compile everywhere, so their tests run on Linux.

use std::io;
use std::path::Path;

/// Every pipe name starts with this.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Pipe names are at most 256 characters, prefix included.
pub const PIPE_NAME_MAX: usize = 256;

/// The Daemon pipe of the user with this SID string (`S-1-5-21-…`).
pub fn pipe_name_for_sid(sid: &str) -> String {
    format!(r"{PIPE_PREFIX}xshelld-{sid}")
}

/// The security descriptor of every Daemon pipe instance: owned by the user, a protected
/// DACL that grants the user (and nobody else) full access.
pub fn user_only_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;;GA;;;{sid})")
}

/// The manual-reset event that stops the Daemon with process id `pid` in order (the Desktop
/// sets it at quit; Windows has no SIGTERM).
pub fn stop_event_name(pid: u32) -> String {
    format!(r"Local\xshelld-stop-{pid}")
}

/// A valid local pipe name: `\\.\pipe\` and at least one character without a backslash,
/// 256 characters at most.
pub fn check_pipe_name(p: &Path) -> io::Result<()> {
    let bad = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a pipe name: {why}", p.display()),
        ))
    };
    let Some(s) = p.to_str() else {
        return bad("not valid Unicode");
    };
    let Some(rest) = s.strip_prefix(PIPE_PREFIX) else {
        return bad(&format!("it must start with {PIPE_PREFIX}"));
    };
    if rest.is_empty() || rest.contains('\\') {
        return bad("the name after the prefix must be non-empty and contain no backslash");
    }
    if s.encode_utf16().count() > PIPE_NAME_MAX {
        return bad(&format!("longer than {PIPE_NAME_MAX} characters"));
    }
    Ok(())
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::OsStr;
    use std::net::Shutdown;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING,
        ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        CopySid, EqualSid, GetLengthSid, GetTokenInformation, TokenUser,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// In and out buffer size of every instance.
    const BUFFER: u32 = 64 * 1024;
    /// One `WaitNamedPipe` while every instance is busy, between deadline and cancel checks.
    const BUSY_WAIT: Duration = Duration::from_millis(50);
    /// The largest single read or write.
    const MAX_IO: usize = 1 << 30;

    /// `s` as a NUL-terminated UTF-16 string.
    pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    pub(crate) fn last_error() -> io::Error {
        io::Error::last_os_error()
    }

    pub(crate) fn os_error(code: u32) -> io::Error {
        io::Error::from_raw_os_error(code as i32)
    }

    pub(crate) fn owned(h: HANDLE) -> io::Result<OwnedHandle> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
    }

    pub(crate) fn raw(h: &OwnedHandle) -> HANDLE {
        h.as_raw_handle() as HANDLE
    }

    /// A timeout in milliseconds for a wait: at least 1 ms, never `INFINITE` by accident.
    fn wait_ms(d: Duration) -> u32 {
        d.as_millis().clamp(1, (INFINITE - 1) as u128) as u32
    }

    /// A manual-reset event, unsignalled.
    fn event() -> io::Result<OwnedHandle> {
        owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })
    }

    // ── The user's SID ────────────────────────────────────────────────────

    /// A SID, in a buffer aligned for it.
    pub struct Sid(Vec<u32>);

    impl Sid {
        pub(crate) fn as_psid(&self) -> PSID {
            self.0.as_ptr() as PSID
        }

        /// Copy the SID at `p`.
        fn copy(p: PSID) -> io::Result<Sid> {
            let len = unsafe { GetLengthSid(p) };
            let mut buf = vec![0u32; (len as usize).div_ceil(4)];
            if unsafe { CopySid(len, buf.as_mut_ptr() as PSID, p) } == 0 {
                return Err(last_error());
            }
            Ok(Sid(buf))
        }

        /// The `S-1-…` string form.
        pub fn to_string_sid(&self) -> io::Result<String> {
            let mut s: *mut u16 = std::ptr::null_mut();
            if unsafe { ConvertSidToStringSidW(self.as_psid(), &mut s) } == 0 {
                return Err(last_error());
            }
            let len = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
            let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, len) });
            unsafe { LocalFree(s as _) };
            Ok(out)
        }

        pub(crate) fn equals(&self, other: PSID) -> bool {
            unsafe { EqualSid(self.as_psid(), other) != 0 }
        }
    }

    /// This process's user (`TokenUser`).
    pub fn current_user_sid() -> io::Result<Sid> {
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_error());
        }
        let token = owned(token)?;
        let mut len = 0u32;
        unsafe { GetTokenInformation(raw(&token), TokenUser, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(last_error());
        }
        // u64s: aligned for TOKEN_USER.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if unsafe {
            GetTokenInformation(
                raw(&token),
                TokenUser,
                buf.as_mut_ptr().cast(),
                len,
                &mut len,
            )
        } == 0
        {
            return Err(last_error());
        }
        let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        Sid::copy(user.User.Sid)
    }

    /// This user's Daemon pipe.
    pub fn default_pipe_name() -> io::Result<PathBuf> {
        Ok(PathBuf::from(pipe_name_for_sid(
            &current_user_sid()?.to_string_sid()?,
        )))
    }

    // ── Security descriptors ──────────────────────────────────────────────

    /// A security descriptor parsed from SDDL, freed on drop.
    pub(crate) struct SecDesc(pub(crate) PSECURITY_DESCRIPTOR);

    unsafe impl Send for SecDesc {}
    unsafe impl Sync for SecDesc {}

    impl SecDesc {
        pub(crate) fn from_sddl(sddl: &str) -> io::Result<SecDesc> {
            let w = wide(OsStr::new(sddl));
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    w.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(last_error());
            }
            Ok(SecDesc(sd))
        }

        pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for SecDesc {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0 as _) };
        }
    }

    // ── Streams ───────────────────────────────────────────────────────────

    struct Shared {
        /// `None` once shut down. Every operation holds a read guard while it uses the
        /// handle, so `shutdown` closes it only after the last one has returned.
        handle: RwLock<Option<OwnedHandle>>,
        /// The handle's value, for cancelling operations without the lock. Valid until
        /// `shutdown` takes the handle.
        raw: usize,
        closed: AtomicBool,
        /// Serializes `shutdown`, so only one caller ever cancels on the handle's value.
        shutting: Mutex<()>,
        read_timeout: Mutex<Option<Duration>>,
        write_timeout: Mutex<Option<Duration>>,
    }

    /// One end of a connected pipe, like a `UnixStream`: clones share the handle and the
    /// timeouts, and [`PipeStream::shutdown`] ends every clone's I/O at once.
    #[derive(Clone)]
    pub struct PipeStream(Arc<Shared>);

    impl std::fmt::Debug for PipeStream {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "PipeStream({:#x})", self.0.raw)
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Dir {
        Read,
        Write,
    }

    impl PipeStream {
        fn from_handle(h: OwnedHandle) -> PipeStream {
            PipeStream(Arc::new(Shared {
                raw: raw(&h) as usize,
                handle: RwLock::new(Some(h)),
                closed: AtomicBool::new(false),
                shutting: Mutex::new(()),
                read_timeout: Mutex::new(None),
                write_timeout: Mutex::new(None),
            }))
        }

        /// Another handle on the same stream (it shares the timeouts).
        pub fn try_clone(&self) -> io::Result<PipeStream> {
            Ok(self.clone())
        }

        fn check_timeout(d: Option<Duration>) -> io::Result<()> {
            if d == Some(Duration::ZERO) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot set a 0 duration timeout",
                ));
            }
            Ok(())
        }

        pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
            Self::check_timeout(d)?;
            *self.0.read_timeout.lock().unwrap() = d;
            Ok(())
        }

        pub fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
            Self::check_timeout(d)?;
            *self.0.write_timeout.lock().unwrap() = d;
            Ok(())
        }

        /// Close the stream for every clone: blocked reads return 0, blocked writes fail, and
        /// the peer reads what was written, then end of file. A pipe has no half-close, so
        /// `how` is ignored. Bytes already written are never discarded (no
        /// `DisconnectNamedPipe`).
        pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
            let _one = self.0.shutting.lock().unwrap();
            self.0.closed.store(true, Ordering::SeqCst);
            loop {
                if let Ok(mut g) = self.0.handle.try_write() {
                    g.take();
                    return Ok(());
                }
                // Operations in progress hold read guards: cancel them, then retry. One that
                // started after `closed` was set returns at once. The handle is still open:
                // only this (serialized) call closes it.
                unsafe { CancelIoEx(self.0.raw as HANDLE, std::ptr::null()) };
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// The pid of the process serving this pipe.
        pub fn server_process_id(&self) -> io::Result<u32> {
            let g = self.0.handle.read().unwrap();
            let h = g
                .as_ref()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
            let mut pid = 0u32;
            if unsafe { GetNamedPipeServerProcessId(raw(h), &mut pid) } == 0 {
                return Err(last_error());
            }
            Ok(pid)
        }

        /// Read, waiting at most `timeout` (`None`: no limit). 0 at end of file and after a
        /// shutdown.
        pub fn read_within(&self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
            self.op(Dir::Read, buf.as_mut_ptr(), buf.len(), timeout)
        }

        /// Write, waiting at most `timeout` (`None`: no limit).
        pub fn write_within(&self, buf: &[u8], timeout: Option<Duration>) -> io::Result<usize> {
            self.op(Dir::Write, buf.as_ptr() as *mut u8, buf.len(), timeout)
        }

        fn closed_result(dir: Dir) -> io::Result<usize> {
            match dir {
                Dir::Read => Ok(0),
                Dir::Write => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            }
        }

        /// The outcome of a failed operation.
        fn failed(&self, dir: Dir, code: u32, timed_out: bool) -> io::Result<usize> {
            let closed = self.0.closed.load(Ordering::SeqCst);
            match code {
                ERROR_OPERATION_ABORTED if timed_out && !closed => {
                    Err(io::Error::from(io::ErrorKind::TimedOut))
                }
                ERROR_OPERATION_ABORTED if closed => Self::closed_result(dir),
                ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA => {
                    Self::closed_result(dir)
                }
                _ => Err(os_error(code)),
            }
        }

        fn op(
            &self,
            dir: Dir,
            ptr: *mut u8,
            len: usize,
            timeout: Option<Duration>,
        ) -> io::Result<usize> {
            if len == 0 {
                return Ok(0);
            }
            let len = len.min(MAX_IO) as u32;
            let g = self.0.handle.read().unwrap();
            let h = match g.as_ref() {
                Some(h) if !self.0.closed.load(Ordering::SeqCst) => raw(h),
                _ => return Self::closed_result(dir),
            };
            let ev = event()?;
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = raw(&ev);
            let started = unsafe {
                match dir {
                    Dir::Read => ReadFile(h, ptr, len, std::ptr::null_mut(), &mut ov),
                    Dir::Write => WriteFile(h, ptr, len, std::ptr::null_mut(), &mut ov),
                }
            };
            let mut timed_out = false;
            if started == 0 {
                let code = unsafe { GetLastError() };
                if code != ERROR_IO_PENDING {
                    return self.failed(dir, code, false);
                }
                let ms = timeout.map_or(INFINITE, wait_ms);
                if unsafe { WaitForSingleObject(raw(&ev), ms) } != WAIT_OBJECT_0 {
                    timed_out = true;
                    unsafe { CancelIoEx(h, &ov) };
                }
            }
            // Always waits for the operation to finish: `buf` and `ov` stay in use until then.
            let mut n = 0u32;
            if unsafe { GetOverlappedResult(h, &ov, &mut n, 1) } == 0 {
                let code = unsafe { GetLastError() };
                return self.failed(dir, code, timed_out);
            }
            Ok(n as usize)
        }
    }

    impl io::Read for &PipeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let t = *self.0.read_timeout.lock().unwrap();
            self.read_within(buf, t)
        }
    }

    impl io::Write for &PipeStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let t = *self.0.write_timeout.lock().unwrap();
            self.write_within(buf, t)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl io::Read for PipeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&*self).read(buf)
        }
    }

    impl io::Write for PipeStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&*self).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ── Server ────────────────────────────────────────────────────────────

    fn create_instance(name: &[u16], sd: &SecDesc, first: bool) -> io::Result<OwnedHandle> {
        let mut flags = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let sa = sd.attributes();
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                &sa,
            )
        };
        owned(h)
    }

    /// Wait until a client connects to the pending instance `h`.
    fn wait_connected(h: &OwnedHandle) -> io::Result<()> {
        let ev = event()?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = raw(&ev);
        if unsafe { ConnectNamedPipe(raw(h), &mut ov) } != 0 {
            return Ok(());
        }
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED => Ok(()),
            // The client came and went already: it reads as an empty connection.
            ERROR_NO_DATA => Ok(()),
            ERROR_IO_PENDING => {
                let mut n = 0u32;
                if unsafe { GetOverlappedResult(raw(h), &ov, &mut n, 1) } == 0 {
                    return Err(last_error());
                }
                Ok(())
            }
            code => Err(os_error(code)),
        }
    }

    /// A named pipe server. While it lives a pending instance always exists, so a client
    /// finds the name busy, never missing.
    pub struct PipeListener {
        name: Vec<u16>,
        display: String,
        sd: SecDesc,
        pending: Mutex<Option<OwnedHandle>>,
    }

    impl PipeListener {
        /// Serve `name` for this user only (see the module docs). Fails when the name exists
        /// already, whoever created it.
        pub fn bind(name: &Path) -> io::Result<PipeListener> {
            let sid = current_user_sid()?.to_string_sid()?;
            Self::bind_with_sddl(name, &user_only_sddl(&sid))
        }

        /// [`PipeListener::bind`] with another security descriptor (tests).
        #[doc(hidden)]
        pub fn bind_with_sddl(name: &Path, sddl: &str) -> io::Result<PipeListener> {
            check_pipe_name(name)?;
            let sd = SecDesc::from_sddl(sddl)?;
            let w = wide(name.as_os_str());
            let first = create_instance(&w, &sd, true).map_err(|e| {
                if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "the pipe {} exists already (another process serves it)",
                            name.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
            Ok(PipeListener {
                name: w,
                display: name.display().to_string(),
                sd,
                pending: Mutex::new(Some(first)),
            })
        }

        /// Wait for the next client. The next pending instance is created before this one is
        /// handed out.
        pub fn accept(&self) -> io::Result<PipeStream> {
            let mut pending = self.pending.lock().unwrap();
            let h = match pending.take() {
                Some(h) => h,
                None => create_instance(&self.name, &self.sd, false)?,
            };
            // On failure this instance is dropped; the next call makes a fresh one.
            wait_connected(&h)?;
            match create_instance(&self.name, &self.sd, false) {
                Ok(next) => *pending = Some(next),
                // The connected instance keeps the name alive; the next accept retries.
                Err(e) => eprintln!("cannot create the next instance of {}: {e}", self.display),
            }
            Ok(PipeStream::from_handle(h))
        }
    }

    impl std::fmt::Debug for PipeListener {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "PipeListener({})", self.display)
        }
    }

    // ── Client ────────────────────────────────────────────────────────────

    /// The owner of the object behind `h` equals `sid`.
    fn owner_is(h: &OwnedHandle, sid: &Sid) -> io::Result<bool> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let r = unsafe {
            GetSecurityInfo(
                raw(h),
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if r != 0 {
            return Err(os_error(r));
        }
        let same = !owner.is_null() && sid.equals(owner);
        unsafe { LocalFree(sd as _) };
        Ok(same)
    }

    /// Open `name` once; `None` while every instance is busy.
    fn open(name: &[u16]) -> io::Result<Option<OwnedHandle>> {
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return owned(h).map(Some);
        }
        match unsafe { GetLastError() } {
            ERROR_PIPE_BUSY => Ok(None),
            code => Err(os_error(code)),
        }
    }

    /// Connect to `name` before `deadline`, re-checking `cancelled` while every instance is
    /// busy (then `Interrupted`). A missing pipe is `NotFound`; a pipe whose owner is not
    /// this process's user is `PermissionDenied`.
    pub fn connect(
        name: &Path,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<PipeStream> {
        check_pipe_name(name)?;
        let w = wide(name.as_os_str());
        let me = current_user_sid()?;
        loop {
            if cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            if let Some(h) = open(&w)? {
                if !owner_is(&h, &me)? {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{} is not owned by this user", name.display()),
                    ));
                }
                return Ok(PipeStream::from_handle(h));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            // Fails at once when the pipe vanished meanwhile; the next open tells.
            unsafe { WaitNamedPipeW(w.as_ptr(), wait_ms(left.min(BUSY_WAIT))) };
        }
    }

    /// Set the stop event of the Daemon with process id `pid` (see [`stop_event_name`]).
    /// `NotFound` when it has none (yet).
    pub fn set_stop_event(pid: u32) -> io::Result<()> {
        use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
        let w = wide(OsStr::new(&stop_event_name(pid)));
        let h = owned(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, w.as_ptr()) })?;
        if unsafe { SetEvent(raw(&h)) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// A connected pair over a fresh, uniquely named pipe (in-process connections, tests).
    pub fn pair() -> io::Result<(PipeStream, PipeStream)> {
        let name = format!("{PIPE_PREFIX}xshell-pair-{}", uuid::Uuid::new_v4().simple());
        let sid = current_user_sid()?.to_string_sid()?;
        let sd = SecDesc::from_sddl(&user_only_sddl(&sid))?;
        let w = wide(OsStr::new(&name));
        let server = create_instance(&w, &sd, true)?;
        let client = open(&w)?.ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        wait_connected(&server)?;
        Ok((
            PipeStream::from_handle(client),
            PipeStream::from_handle(server),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_from_sid() {
        assert_eq!(
            pipe_name_for_sid("S-1-5-21-1-2-3-1001"),
            r"\\.\pipe\xshelld-S-1-5-21-1-2-3-1001"
        );
        check_pipe_name(Path::new(&pipe_name_for_sid("S-1-5-21-1-2-3-1001"))).unwrap();
    }

    #[test]
    fn sddl_user_only() {
        assert_eq!(
            user_only_sddl("S-1-5-21-9"),
            "O:S-1-5-21-9D:P(A;;GA;;;S-1-5-21-9)"
        );
    }

    #[test]
    fn stop_event_name_per_pid() {
        assert_eq!(stop_event_name(4242), r"Local\xshelld-stop-4242");
    }

    #[test]
    fn pipe_name_checks() {
        check_pipe_name(Path::new(r"\\.\pipe\x")).unwrap();
        for bad in [
            r"\\.\pipe\",
            r"\\.\pipe\a\b",
            "/tmp/daemon.sock",
            r"\\host\pipe\x",
        ] {
            assert!(check_pipe_name(Path::new(bad)).is_err(), "{bad}");
        }
        let long = format!(r"\\.\pipe\{}", "x".repeat(PIPE_NAME_MAX));
        let e = check_pipe_name(Path::new(&long)).unwrap_err();
        assert!(e.to_string().contains("256"), "{e}");
        let max = format!(
            r"\\.\pipe\{}",
            "x".repeat(PIPE_NAME_MAX - PIPE_PREFIX.len())
        );
        check_pipe_name(Path::new(&max)).unwrap();
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn unique() -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\xshell-test-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn full_duplex_concurrent_no_deadlock() {
        let (a, b) = pair().unwrap();
        // A reader blocked on `a` must not hold up a 1 MiB write on `a`.
        let ar = a.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 5];
            (&ar).read_exact(&mut buf).unwrap();
            buf
        });
        std::thread::sleep(Duration::from_millis(100));
        let big = vec![7u8; 1 << 20];
        let bw = b.try_clone().unwrap();
        let drain = std::thread::spawn(move || {
            let mut got = vec![0u8; 1 << 20];
            (&bw).read_exact(&mut got).unwrap();
            got
        });
        (&a).write_all(&big).unwrap();
        assert_eq!(drain.join().unwrap(), big);
        (&b).write_all(b"hello").unwrap();
        assert_eq!(&reader.join().unwrap(), b"hello");
    }

    #[test]
    fn read_timeout_times_out() {
        let (a, _b) = pair().unwrap();
        a.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let t = Instant::now();
        let e = (&a).read(&mut [0u8; 4]).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            t.elapsed() >= Duration::from_millis(90),
            "{:?}",
            t.elapsed()
        );
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        // Still usable after a timeout.
        a.set_read_timeout(None).unwrap();
        (&_b).write_all(b"x").unwrap();
        let mut one = [0u8; 1];
        (&a).read_exact(&mut one).unwrap();
        assert_eq!(&one, b"x");
    }

    #[test]
    fn shutdown_unblocks_reader_and_writer() {
        let (a, _b) = pair().unwrap();
        let ar = a.clone();
        let reader = std::thread::spawn(move || (&ar).read(&mut [0u8; 16]).unwrap());
        let aw = a.clone();
        // Fills the peer's buffer: blocks, nobody reads `_b`.
        let writer = std::thread::spawn(move || (&aw).write_all(&vec![1u8; 4 << 20]));
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        a.shutdown(Shutdown::Both).unwrap();
        assert_eq!(reader.join().unwrap(), 0);
        assert!(writer.join().unwrap().is_err());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!((&a).write(b"x").is_err());
        assert_eq!((&a).read(&mut [0u8; 1]).unwrap(), 0);
    }

    #[test]
    fn close_delivers_buffered_bytes_then_eof() {
        let (a, b) = pair().unwrap();
        (&a).write_all(b"last words").unwrap();
        a.shutdown(Shutdown::Both).unwrap();
        let mut got = Vec::new();
        (&b).read_to_end(&mut got).unwrap();
        assert_eq!(got, b"last words");
    }

    #[test]
    fn first_instance_refuses_existing_name() {
        let name = unique();
        let _l = PipeListener::bind(&name).unwrap();
        let e = PipeListener::bind(&name).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{e}");
    }

    #[test]
    fn connect_missing_is_not_found() {
        let e = connect(&unique(), soon(), &|| false).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
    }

    #[test]
    fn listener_serves_clients_in_turn() {
        let name = unique();
        let l = PipeListener::bind(&name).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let s = l.accept().unwrap();
                let mut b = [0u8; 1];
                (&s).read_exact(&mut b).unwrap();
                (&s).write_all(&[b[0] + 1]).unwrap();
            }
        });
        for i in 0..3u8 {
            let c = connect(&name, soon(), &|| false).unwrap();
            (&c).write_all(&[i]).unwrap();
            let mut b = [0u8; 1];
            (&c).read_exact(&mut b).unwrap();
            assert_eq!(b[0], i + 1);
        }
        server.join().unwrap();
    }

    #[test]
    fn busy_connect_bounded_and_cancellable() {
        let name = unique();
        let l = PipeListener::bind(&name).unwrap();
        // Takes the only pending instance; the listener never accepts again, so every later
        // client finds the pipe busy.
        let _first = connect(&name, soon(), &|| false).unwrap();
        let t = Instant::now();
        let e = connect(&name, Instant::now() + Duration::from_millis(300), &|| {
            false
        })
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut, "{e}");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            f.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let t = Instant::now();
        let e = connect(&name, Instant::now() + Duration::from_secs(30), &|| {
            flag.load(std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Interrupted, "{e}");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        drop(l);
    }

    #[test]
    fn owner_check_accepts_own_server() {
        let name = unique();
        let l = PipeListener::bind(&name).unwrap();
        let server = std::thread::spawn(move || l.accept().map(drop));
        let c = connect(&name, soon(), &|| false).unwrap();
        assert_eq!(c.server_process_id().unwrap(), std::process::id());
        server.join().unwrap().unwrap();
    }

    /// A pipe owned by someone else (Administrators, where this token may assign that owner)
    /// is refused even though its DACL lets us in.
    #[test]
    fn owner_check_refuses_foreign_owner() {
        let name = unique();
        let sid = current_user_sid().unwrap().to_string_sid().unwrap();
        let sddl = format!("O:BAD:P(A;;GA;;;{sid})");
        let l = match PipeListener::bind_with_sddl(&name, &sddl) {
            Ok(l) => l,
            // CI runs elevated, where it can: there the check must really run.
            Err(e) if std::env::var_os("GITHUB_ACTIONS").is_some() => {
                panic!("cannot create a pipe owned by Administrators: {e}")
            }
            Err(e) => {
                eprintln!("skipped: this token cannot make Administrators the owner: {e}");
                return;
            }
        };
        let server = std::thread::spawn(move || l.accept().map(drop));
        let e = connect(&name, soon(), &|| false).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
        let _ = server.join();
    }
}

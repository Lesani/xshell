//! The GUI-bound Daemon's parent watch (ADR-0005): the Daemon ends when the xshell process
//! that started it does, however that process ends.
//!
//! Linux: the kernel sends SIGTERM on the parent's death (`PR_SET_PDEATHSIG`, handled like
//! any SIGTERM); a `getppid` poll backs it up, which also covers a subreaper adopting us.
//! macOS: `kqueue` reports the parent's `NOTE_EXIT`, re-checking `getppid` on every wake.
//! Windows: a wait on a handle to the parent process, opened while it is still our parent
//! (its creation time no later than ours, so a reused pid is never mistaken for it). The
//! Desktop's Job Object ends the Daemon as well (ADR-0005); this watch makes it orderly.

#[cfg(unix)]
use std::time::Duration;

/// This process's parent pid (the one it was started by, on Windows, even after that one
/// exited).
#[cfg(unix)]
pub fn parent_pid() -> Option<u32> {
    Some(unsafe { libc::getppid() } as u32)
}

#[cfg(windows)]
pub fn parent_pid() -> Option<u32> {
    win::parent_pid()
}

/// Whether `pid` is this process's parent.
pub fn is_parent(pid: u32) -> bool {
    parent_pid() == Some(pid)
}

/// Arm the watch: `on_death` runs once, from a thread of its own, when `parent` is no longer
/// this process's parent. Fails when it is not the parent now.
pub fn watch(parent: u32, on_death: impl FnOnce() + Send + 'static) -> Result<(), String> {
    let gone = || format!("parent {parent} is not running");
    if !is_parent(parent) {
        return Err(gone());
    }
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) } != 0 {
            crate::log!(
                "WARN",
                "PR_SET_PDEATHSIG failed: {}",
                std::io::Error::last_os_error()
            );
        }
        // The parent may have died before the request took effect.
        if !is_parent(parent) {
            return Err(gone());
        }
    }
    let wait = Waiter::new(parent)?;
    std::thread::Builder::new()
        .name("parent".into())
        .spawn(move || {
            wait.until_gone(parent);
            crate::log!("INFO", "parent {parent} ended: shutting down");
            on_death();
        })
        .map_err(|e| format!("cannot start the parent watch: {e}"))?;
    Ok(())
}

/// Returns once `parent` is no longer our parent.
#[cfg(unix)]
impl Waiter {
    fn until_gone(&self, parent: u32) {
        while is_parent(parent) {
            self.next();
        }
    }
}

/// One wait between `getppid` checks.
#[cfg(all(unix, not(target_os = "macos")))]
struct Waiter;

#[cfg(all(unix, not(target_os = "macos")))]
impl Waiter {
    fn new(_parent: u32) -> Result<Self, String> {
        Ok(Waiter)
    }

    fn next(&self) {
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A kqueue with `EVFILT_PROC`/`NOTE_EXIT` registered for the parent; waits at most 1 s.
#[cfg(target_os = "macos")]
struct Waiter(std::os::fd::OwnedFd);

#[cfg(target_os = "macos")]
impl Waiter {
    fn new(parent: u32) -> Result<Self, String> {
        use std::os::fd::FromRawFd;
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(format!("kqueue: {}", std::io::Error::last_os_error()));
        }
        let kq = unsafe { std::os::fd::OwnedFd::from_raw_fd(kq) };
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = parent as libc::uintptr_t;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        let r = unsafe {
            libc::kevent(
                std::os::fd::AsRawFd::as_raw_fd(&kq),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            // ESRCH: it is already gone.
            return Err(format!("parent {parent} is not running ({e})"));
        }
        Ok(Waiter(kq))
    }

    fn next(&self) {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        let r = unsafe {
            libc::kevent(
                std::os::fd::AsRawFd::as_raw_fd(&self.0),
                std::ptr::null(),
                0,
                &mut ev,
                1,
                &timeout,
            )
        };
        if r < 0 {
            // Interrupted or broken: fall back to polling at the same pace.
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(windows)]
use win::Waiter;

#[cfg(windows)]
mod win {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, OpenProcess, WaitForSingleObject, INFINITE,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    /// Our `th32ParentProcessID` from a process snapshot.
    pub(super) fn parent_pid() -> Option<u32> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let snap = unsafe { OwnedHandle::from_raw_handle(snap as _) };
        let me = std::process::id();
        let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let h = snap.as_raw_handle() as HANDLE;
        let mut ok = unsafe { Process32FirstW(h, &mut e) } != 0;
        while ok {
            if e.th32ProcessID == me {
                return Some(e.th32ParentProcessID);
            }
            ok = unsafe { Process32NextW(h, &mut e) } != 0;
        }
        None
    }

    fn created(h: HANDLE) -> Option<u64> {
        let z = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut c, mut x, mut k, mut u) = (z, z, z, z);
        if unsafe { GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u) } == 0 {
            return None;
        }
        Some(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
    }

    /// A handle on the parent process, to wait for its exit.
    pub(super) struct Waiter(OwnedHandle);

    impl Waiter {
        pub(super) fn new(parent: u32) -> Result<Self, String> {
            let gone = |why: String| format!("parent {parent} is not running ({why})");
            let h = unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    parent,
                )
            };
            if h.is_null() {
                return Err(gone(std::io::Error::last_os_error().to_string()));
            }
            let h = unsafe { OwnedHandle::from_raw_handle(h as _) };
            // A process started after us cannot be our parent: the pid was reused.
            match (
                created(h.as_raw_handle() as HANDLE),
                created(unsafe { GetCurrentProcess() }),
            ) {
                (Some(p), Some(me)) if p <= me => Ok(Waiter(h)),
                (Some(_), Some(_)) => Err(gone("its pid now names a newer process".into())),
                _ => Err(gone(std::io::Error::last_os_error().to_string())),
            }
        }

        pub(super) fn until_gone(&self, _parent: u32) {
            unsafe { WaitForSingleObject(self.0.as_raw_handle() as HANDLE, INFINITE) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_process_that_is_not_the_parent() {
        let me = std::process::id();
        assert!(!is_parent(me));
        assert!(watch(me, || {}).unwrap_err().contains("is not running"));
        let parent = parent_pid().expect("a parent");
        assert!(is_parent(parent));
    }
}

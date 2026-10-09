//! The GUI-bound Daemon's parent watch (ADR-0005): the Daemon ends when the xshell process
//! that started it does, however that process ends.
//!
//! Linux: the kernel sends SIGTERM on the parent's death (`PR_SET_PDEATHSIG`, handled like
//! any SIGTERM); a `getppid` poll backs it up, which also covers a subreaper adopting us.
//! macOS: `kqueue` reports the parent's `NOTE_EXIT`, re-checking `getppid` on every wake.

use std::time::Duration;

/// Whether `pid` is this process's parent.
pub fn is_parent(pid: u32) -> bool {
    (unsafe { libc::getppid() }) as u32 == pid
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
            while is_parent(parent) {
                wait.next();
            }
            crate::log!("INFO", "parent {parent} ended: shutting down");
            on_death();
        })
        .map_err(|e| format!("cannot start the parent watch: {e}"))?;
    Ok(())
}

/// One wait between `getppid` checks.
#[cfg(not(target_os = "macos"))]
struct Waiter;

#[cfg(not(target_os = "macos"))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_process_that_is_not_the_parent() {
        let me = std::process::id();
        assert!(!is_parent(me));
        assert!(watch(me, || {}).unwrap_err().contains("is not running"));
        assert!(is_parent(unsafe { libc::getppid() } as u32));
    }
}

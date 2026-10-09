//! What stops `serve` from outside, in order: SIGTERM, SIGINT or SIGHUP on Unix; on Windows
//! the named stop event the Desktop sets ([`xshell_core::pipe::stop_event_name`]) and, when
//! a console is attached, its close and Ctrl+C events.

use super::StopLatch;
use std::io;
use std::sync::Arc;

/// Fire `stop` on SIGTERM, SIGINT or SIGHUP, from a thread of its own. The caller must have
/// blocked those signals (see [`block_exit_signals`]) before starting any thread.
#[cfg(unix)]
pub fn watch_exit_signals(stop: Arc<StopLatch>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || loop {
            let sig = wait_exit_signal();
            crate::log!("INFO", "signal {sig}: shutting down");
            stop.trigger();
        })?;
    Ok(())
}

#[cfg(unix)]
fn exit_signal_set() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

/// Block the exit signals in this thread and every thread it spawns afterwards, so only the
/// `sigwait` thread sees them. Children get a clean mask: std and portable-pty reset it.
#[cfg(unix)]
pub fn block_exit_signals() {
    let set = exit_signal_set();
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

#[cfg(unix)]
fn wait_exit_signal() -> i32 {
    let set = exit_signal_set();
    let mut sig: libc::c_int = 0;
    loop {
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
            return sig;
        }
    }
}

/// Nothing to block on Windows.
#[cfg(windows)]
pub fn block_exit_signals() {}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    use windows_sys::Win32::System::Threading::{SetEvent, Sleep};

    /// The event [`on_ctrl`] sets (a raw handle, never closed).
    pub(super) static CTRL_EVENT: AtomicUsize = AtomicUsize::new(0);

    /// Console control handler: request the orderly shutdown. For a console close, logoff
    /// or shutdown Windows ends the process once this returns, so it waits instead; the
    /// shutdown's `exit` ends the process first, or Windows' own timeout does.
    pub(super) unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
        let h = CTRL_EVENT.load(Ordering::SeqCst);
        if h != 0 {
            unsafe { SetEvent(h as HANDLE) };
        }
        if matches!(
            kind,
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
        ) {
            unsafe { Sleep(20_000) };
        }
        1
    }
}

/// Fire `stop` when the named stop event of this process is set (the Desktop's orderly
/// quit), or on a console close or Ctrl+C when a console is attached. The stop event is
/// required; the console handler is best effort.
#[cfg(windows)]
pub fn watch_exit_signals(stop: Arc<StopLatch>) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Console::{GetConsoleCP, SetConsoleCtrlHandler};
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects, INFINITE};
    let name = xshell_core::pipe::stop_event_name(std::process::id());
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let owned = |h: windows_sys::Win32::Foundation::HANDLE| {
        if h.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedHandle::from_raw_handle(h as _) })
        }
    };
    unsafe { SetLastError(0) };
    let named = owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, wide.as_ptr()) })?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("the stop event {name} exists already"),
        ));
    }
    let console = owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
    win::CTRL_EVENT.store(console.as_raw_handle() as usize, Ordering::SeqCst);
    if unsafe { GetConsoleCP() } != 0
        && unsafe { SetConsoleCtrlHandler(Some(win::on_ctrl), 1) } == 0
    {
        crate::log!(
            "WARN",
            "no console control handler: {}",
            io::Error::last_os_error()
        );
    }
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let handles = [named.as_raw_handle() as _, console.as_raw_handle() as _];
            let why = match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } {
                0 => "stop event",
                1 => "console event",
                _ => {
                    crate::log!(
                        "ERROR",
                        "cannot wait for the stop event: {}",
                        io::Error::last_os_error()
                    );
                    return;
                }
            };
            crate::log!("INFO", "{why}: shutting down");
            stop.trigger();
            // Both events stay open (the handler may still use the second).
            std::mem::forget((named, console));
        })?;
    Ok(())
}

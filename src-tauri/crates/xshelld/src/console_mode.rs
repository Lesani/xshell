//! `xshelld console-mode <pid>`: reads the input mode of the console process `pid` runs on
//! (Windows), for the Daemon's reply gate (Lesani/xshell#40). A Chat View reply is a
//! bracketed paste, which only an agent that reads terminal (VT) input gets as a paste; an
//! agent that reads console key events gets it as keystrokes, each newline a key of its own.
//! The console's `ENABLE_VIRTUAL_TERMINAL_INPUT` bit tells the two apart.
//!
//! The Daemon cannot attach to an agent's console itself: a process has one console, and
//! every process the Daemon starts would inherit the swapped one. So it starts this helper
//! (detached, with no console of its own), which attaches to the console, reads the mode,
//! detaches, and prints one line on its standard output:
//!
//! - `xshelld-console-mode 1 mode=0x%08x procs=%u` (exit 0): the mode, and how many
//!   processes the console had attached (the helper included);
//! - `xshelld-console-mode 1 error=<stage>:<win32 error>` (exit 1), stage `attach`, `conin`,
//!   `mode` or `output`.
//!
//! A reading can only be as new as the helper's run: the agent may change its mode right
//! after it, and nothing serializes the agent's `SetConsoleMode` with the Daemon's writes.
//! The Daemon narrows that window (a reading allows one write, within 100 ms of the
//! helper's start and with no wait for the agent's locks; the mode is read again before
//! Enter) but cannot close it. That residual race blocks offering replies on Windows (Lesani/xshell#40, PR2b)
//! until a measured argument says agents do not change their mode while they show their
//! composer, or the risk is accepted.
//!
//! The helper only reads. The console calls it makes are `FreeConsole`, `AttachConsole`,
//! `SetConsoleCtrlHandler`, `GetConsoleMode` and `GetConsoleProcessList`: never a read or
//! peek of the console's input, never a write to its screen, never a mode change.

/// `ENABLE_VIRTUAL_TERMINAL_INPUT`: the console gives its reader terminal input (VT
/// sequences, a bracketed paste as it was typed) instead of key events.
pub const VT_INPUT: u32 = 0x0200;

/// What every line of the helper starts with: its name and the line format's version.
pub const PREFIX: &str = "xshelld-console-mode 1 ";

/// One reading of a console's input mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// The console's input mode (`GetConsoleMode` on its `CONIN$`).
    pub mode: u32,
    /// How many processes were attached to the console, the helper included.
    pub procs: u32,
}

impl Reading {
    /// Whether the console gives its reader terminal (VT) input.
    pub fn vt(&self) -> bool {
        self.mode & VT_INPUT != 0
    }
}

/// The line the helper prints for a reading.
pub fn ok_line(r: Reading) -> String {
    format!("{PREFIX}mode=0x{:08x} procs={}\n", r.mode, r.procs)
}

/// The line the helper prints when `stage` failed with Win32 error `code`.
pub fn error_line(stage: &str, code: u32) -> String {
    format!("{PREFIX}error={stage}:{code}\n")
}

/// The helper's whole standard output, as a reading or as why there is none. Exactly one
/// line, with its newline.
pub fn parse_reading(out: &str) -> Result<Reading, String> {
    let line = out
        .strip_suffix('\n')
        .filter(|l| !l.contains('\n'))
        .ok_or_else(|| format!("not one line: {out:?}"))?;
    let rest = line
        .strip_prefix(PREFIX)
        .ok_or_else(|| format!("not a reading: {line:?}"))?;
    if let Some(e) = rest.strip_prefix("error=") {
        return Err(format!("the helper failed: {e}"));
    }
    let (mode, procs) = rest
        .strip_prefix("mode=0x")
        .and_then(|r| r.split_once(" procs="))
        .ok_or_else(|| format!("not a reading: {line:?}"))?;
    let hex = |s: &str| {
        (s.len() == 8)
            .then(|| u32::from_str_radix(s, 16).ok())
            .flatten()
    };
    let dec = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    match (hex(mode), dec(procs)) {
        (Some(mode), Some(procs)) => Ok(Reading { mode, procs }),
        _ => Err(format!("not a reading: {line:?}")),
    }
}

/// Test hook, debug builds only: how the helper misbehaves, from `XSHELLD_CONSOLE_MODE_SIM`
/// (which only the Daemon's test hook `XSHELLD_TEST_CONSOLE_MODE` sets): `garbage` prints a
/// line that is no reading, `fail` reports an attach error, `hang:<file>` attaches, writes
/// its pid to `<file>` and never answers, and `slow:<ms>` waits that long after it read the
/// mode before it answers.
/// A release build's helper ignores it, and its Daemon never sets it.
pub const SIM_ENV: &str = "XSHELLD_CONSOLE_MODE_SIM";

/// Run the helper; returns the exit code.
#[cfg(windows)]
pub fn run(pid: u32) -> i32 {
    win::run(pid)
}

/// Elsewhere a terminal has no console.
#[cfg(not(windows))]
pub fn run(_pid: u32) -> i32 {
    eprintln!("xshelld: console-mode is a Windows command");
    2
}

/// The Daemon's side: read the input mode of the console the Terminal process `leader`
/// (a member of the Terminal's `job`) runs on, through the helper `exe`, within `timeout`.
/// Any failure is an `Err` with what failed: the Terminal's identity cannot be pinned, the
/// helper cannot start, does not answer in time (it is then ended), or reports an error.
#[cfg(windows)]
pub(crate) fn read(
    exe: &std::path::Path,
    leader: u32,
    job: &xshell_core::job::Job,
    timeout: std::time::Duration,
    sim: Option<&str>,
) -> Result<Reading, String> {
    win::read(exe, leader, job, timeout, sim)
}

#[cfg(windows)]
mod win {
    use super::{error_line, ok_line, parse_reading, Reading};
    use std::io::Read;
    use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        GetLastError, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, WriteFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GetConsoleMode, GetConsoleProcessList, SetConsoleCtrlHandler,
        CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, DETACHED_PROCESS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    unsafe extern "system" fn ignore_break(kind: u32) -> windows_sys::core::BOOL {
        // Ctrl+C and Ctrl+Break are the agent's; a close or logoff ends the helper.
        (kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT) as windows_sys::core::BOOL
    }

    fn owned(h: HANDLE) -> Option<OwnedHandle> {
        (!h.is_null() && h != INVALID_HANDLE_VALUE)
            .then(|| unsafe { OwnedHandle::from_raw_handle(h as _) })
    }

    /// Write `line` whole to `out` (never the console: `out` is the pipe the Daemon reads).
    fn say(out: &OwnedHandle, line: &str) -> bool {
        let mut b = line.as_bytes();
        while !b.is_empty() {
            let mut n = 0u32;
            let ok = unsafe {
                WriteFile(
                    out.as_raw_handle() as HANDLE,
                    b.as_ptr(),
                    b.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || n == 0 {
                return false;
            }
            b = &b[n as usize..];
        }
        true
    }

    pub(super) fn run(pid: u32) -> i32 {
        // Nothing of a panic reaches the console it may be attached to.
        std::panic::set_hook(Box::new(|_| {}));
        // Our output pipe, before attaching: afterwards the standard handles may be the
        // agent's console, which must never be written.
        let Ok(out) = std::io::stdout().as_handle().try_clone_to_owned() else {
            return 1;
        };
        #[cfg(debug_assertions)]
        let sim = std::env::var(super::SIM_ENV).ok();
        #[cfg(debug_assertions)]
        match sim.as_deref() {
            Some("garbage") => return if say(&out, "garbage\n") { 0 } else { 1 },
            Some("fail") => {
                say(&out, &error_line("attach", 5));
                return 1;
            }
            _ => {}
        }
        let fail = |stage: &str| {
            let code = unsafe { GetLastError() };
            say(&out, &error_line(stage, code));
            1
        };
        unsafe { FreeConsole() };
        if unsafe { AttachConsole(pid) } == 0 {
            return fail("attach");
        }
        // After attaching: AttachConsole starts a fresh handler list.
        unsafe { SetConsoleCtrlHandler(Some(ignore_break), 1) };
        #[cfg(debug_assertions)]
        if let Some(file) = sim.as_deref().and_then(|s| s.strip_prefix("hang:")) {
            let _ = std::fs::write(file, std::process::id().to_string());
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        let name: Vec<u16> = "CONIN$".encode_utf16().chain([0]).collect();
        let conin = owned(unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        });
        let Some(conin) = conin else {
            let r = fail("conin");
            unsafe { FreeConsole() };
            return r;
        };
        let mut mode = 0u32;
        if unsafe { GetConsoleMode(conin.as_raw_handle() as HANDLE, &mut mode) } == 0 {
            let r = fail("mode");
            drop(conin);
            unsafe { FreeConsole() };
            return r;
        }
        #[cfg(debug_assertions)]
        if let Some(ms) = sim
            .as_deref()
            .and_then(|s| s.strip_prefix("slow:"))
            .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let mut list = [0u32; 64];
        let procs = unsafe { GetConsoleProcessList(list.as_mut_ptr(), list.len() as u32) };
        drop(conin);
        unsafe { FreeConsole() };
        if say(&out, &ok_line(Reading { mode, procs })) {
            0
        } else {
            1
        }
    }

    pub(super) fn read(
        exe: &Path,
        leader: u32,
        job: &xshell_core::job::Job,
        timeout: Duration,
        sim: Option<&str>,
    ) -> Result<Reading, String> {
        // The leader's identity, pinned until the helper is done: a handle keeps its pid from
        // being reused, and the job says it is still this Terminal's process.
        let h = owned(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                leader,
            )
        })
        .ok_or_else(|| {
            format!(
                "cannot open the terminal's process {leader}: {}",
                std::io::Error::last_os_error()
            )
        })?;
        match job.contains(h.as_raw_handle()) {
            Ok(true) => {}
            Ok(false) => return Err(format!("process {leader} is not the terminal's")),
            Err(e) => return Err(format!("cannot check process {leader}: {e}")),
        }
        let mut cmd = Command::new(exe);
        cmd.arg("console-mode")
            .arg(leader.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // No console of its own: it attaches straight to the Terminal's.
            .creation_flags(DETACHED_PROCESS);
        cmd.env_remove(super::SIM_ENV);
        #[cfg(debug_assertions)]
        if let Some(s) = sim {
            cmd.env(super::SIM_ENV, s);
        }
        #[cfg(not(debug_assertions))]
        let _ = sim;
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        let mut stdout = child.stdout.take().expect("piped");
        let reader = std::thread::Builder::new()
            .name("console-mode".into())
            .spawn(move || {
                let mut s = String::new();
                // At most a line's worth: a helper that prints more is not one.
                let _ = (&mut stdout).take(512).read_to_string(&mut s);
                s
            });
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) if Instant::now() >= deadline => break Err("did not answer in time"),
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break Err("cannot be waited for"),
            }
        };
        let status = match status {
            Ok(st) => st,
            Err(why) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the console-mode helper {why}"));
            }
        };
        drop(h);
        let out = match reader {
            Ok(r) => r.join().unwrap_or_default(),
            Err(e) => return Err(format!("cannot read the helper's answer: {e}")),
        };
        let reading = parse_reading(&out);
        match (status.code(), reading) {
            (Some(0), Ok(r)) => Ok(r),
            (_, Err(e)) => Err(e),
            (code, Ok(_)) => Err(format!("the helper exited with {code:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_lines_parse() {
        let r = Reading {
            mode: 0x2a7,
            procs: 3,
        };
        assert_eq!(
            ok_line(r),
            "xshelld-console-mode 1 mode=0x000002a7 procs=3\n"
        );
        assert_eq!(parse_reading(&ok_line(r)), Ok(r));
        assert!(r.vt());
        let records = Reading {
            mode: 0x0018,
            procs: 2,
        };
        assert!(!records.vt());
        assert_eq!(parse_reading(&ok_line(records)), Ok(records));
        let e = parse_reading(&error_line("attach", 87)).unwrap_err();
        assert!(e.contains("attach:87"), "{e}");
        for bad in [
            "",
            "garbage\n",
            "xshelld-console-mode 1 mode=0x000002a7 procs=3",
            "xshelld-console-mode 2 mode=0x000002a7 procs=3\n",
            "xshelld-console-mode 1 mode=0x2a7 procs=3\n",
            "xshelld-console-mode 1 mode=0x0000zza7 procs=3\n",
            "xshelld-console-mode 1 mode=0x000002a7 procs=\n",
            "xshelld-console-mode 1 mode=0x000002a7 procs=-1\n",
            "xshelld-console-mode 1 mode=0x000002a7 procs=3 \n",
            "xshelld-console-mode 1 mode=0x000002a7 procs=3\nmore\n",
            "xshelld-console-mode 1 mode=0x000002a7 procs=3\n\n",
        ] {
            assert!(parse_reading(bad).is_err(), "{bad:?}");
        }
    }
}

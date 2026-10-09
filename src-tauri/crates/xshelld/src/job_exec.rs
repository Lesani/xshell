//! `xshelld job-exec <job> -- <program> [args…]`: how the Daemon starts each Terminal on
//! Windows. The launcher joins the Terminal's Job Object first, then runs the program as its
//! child on the same console, so the program and everything it starts are in the job from
//! their first instruction. A job assigned from outside after the start would race the
//! program's own children.
//!
//! The launcher exits with the program's code. It ignores Ctrl+C and Ctrl+Break (the
//! program handles them); a console close ends it along with the program.

use std::ffi::{OsStr, OsString};

#[cfg(windows)]
unsafe extern "system" fn ignore_break(kind: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    // Handled (ignored here) for Ctrl+C and Ctrl+Break; anything else ends the launcher.
    (kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT) as windows_sys::core::BOOL
}

/// Run the launcher; returns the exit code.
#[cfg(windows)]
pub fn run(job: &str, program: &OsStr, args: &[OsString]) -> i32 {
    if let Err(e) = xshell_core::job::join_named(job) {
        eprintln!("xshelld: {e}");
        return 1;
    }
    // A handler, not `SetConsoleCtrlHandler(NULL, TRUE)`: that would be inherited, and the
    // program would ignore Ctrl+C too.
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ignore_break), 1) };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    // The console's own handles, explicitly: ours may be unset (the ConPTY host passes none).
    let console = |name: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .ok()
    };
    if let (Some(input), Some(output)) = (console("CONIN$"), console("CONOUT$")) {
        if let Ok(err) = output.try_clone() {
            cmd.stdin(input).stdout(output).stderr(err);
        }
    }
    match cmd.status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(e) => {
            eprintln!("xshelld: cannot start {}: {e}", program.to_string_lossy());
            // cmd.exe's code for a command it cannot find.
            9009
        }
    }
}

/// Elsewhere the Daemon starts Terminals in a session of their own instead.
#[cfg(not(windows))]
pub fn run(_job: &str, _program: &OsStr, _args: &[OsString]) -> i32 {
    eprintln!("xshelld: job-exec is a Windows command");
    2
}

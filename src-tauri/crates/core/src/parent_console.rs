//! Windows: running a console program from a GUI-subsystem program (`xshell.exe pair`, #118).
//! A GUI program has no console even when a terminal started it, so a console child would
//! open a window of its own, and with `CREATE_NO_WINDOW` it would be cut off from the
//! terminal: no Ctrl+C, no output. So the caller's console is attached first and the child
//! shares it; only without one does the child run windowless on the inherited handles.

use std::ffi::c_void;
use std::os::windows::process::CommandExt as _;
use std::process::{Command, Stdio};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
    fn GetStdHandle(std_handle: u32) -> *mut c_void;
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

/// Ctrl+C and Ctrl+Break belong to the child, which shares the console and handles them:
/// this process only waits for it and returns its exit code. (A handler is not inherited;
/// ignoring Ctrl+C with a null handler would be.)
unsafe extern "system" fn leave_to_child(_kind: u32) -> i32 {
    1
}

/// Whether this process's standard stream is missing (a GUI program started from a
/// terminal without redirection has none).
fn missing(std_handle: u32) -> bool {
    let handle = unsafe { GetStdHandle(std_handle) };
    handle.is_null() || handle as isize == -1
}

/// Prepare `cmd`, a console program, to run for the terminal that started this GUI program.
/// With a parent console (`true`): the child shares it, gets the console for every standard
/// stream that was not redirected, and receives its Ctrl+C. Without one (`false`): the child
/// gets no window and uses the inherited handles.
pub fn share_with_child(cmd: &mut Command) -> bool {
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        cmd.creation_flags(CREATE_NO_WINDOW);
        return false;
    }
    unsafe { SetConsoleCtrlHandler(Some(leave_to_child), 1) };
    let console = |name: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .map(Stdio::from)
    };
    if missing(STD_INPUT_HANDLE) {
        if let Ok(stdin) = console("CONIN$") {
            cmd.stdin(stdin);
        }
    }
    if missing(STD_OUTPUT_HANDLE) {
        if let Ok(stdout) = console("CONOUT$") {
            cmd.stdout(stdout);
        }
    }
    if missing(STD_ERROR_HANDLE) {
        if let Ok(stderr) = console("CONOUT$") {
            cmd.stderr(stderr);
        }
    }
    true
}

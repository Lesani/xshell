//! Windows Job Objects that end a whole process tree: every job here is kill-on-close, so
//! when its last handle closes (its owner exits, however) the OS ends every process in it,
//! nested jobs included (Windows 8 and later).
//!
//! The Desktop puts its GUI-bound Daemon in one (ADR-0005), and the Daemon puts each
//! Terminal in one. A process joins a named job itself, as its first act, before it starts
//! any child: a job assigned from outside after the start would race the child's own
//! children.

use std::ffi::OsStr;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use windows_sys::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// `JOB_OBJECT_ASSIGN_PROCESS` (winnt.h): all a process needs to join a job.
const JOB_OBJECT_ASSIGN_PROCESS: u32 = 0x0001;

fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn owned(h: HANDLE) -> io::Result<OwnedHandle> {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
}

/// A kill-on-close job: dropping the last handle ends every process in it.
#[derive(Debug)]
pub struct Job(OwnedHandle);

impl Job {
    /// An anonymous job.
    pub fn new() -> io::Result<Job> {
        let h = owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
        Job::kill_on_close(h)
    }

    /// A new job under `name` (`Local\…`), for a process to join with [`join_named`]. Fails
    /// with `AlreadyExists` when the name is taken, so nobody else's job is ever used.
    pub fn new_named(name: &str) -> io::Result<Job> {
        let w = wide(name);
        unsafe { SetLastError(0) };
        let h = unsafe { CreateJobObjectW(std::ptr::null(), w.as_ptr()) };
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let h = owned(h)?;
        if existed {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("the job {name} exists already"),
            ));
        }
        Job::kill_on_close(h)
    }

    fn kill_on_close(h: OwnedHandle) -> io::Result<Job> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let r = unsafe {
            SetInformationJobObject(
                h.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Job(h))
    }

    /// Put the process `process` (a process handle) in this job.
    pub fn assign(&self, process: RawHandle) -> io::Result<()> {
        if unsafe { AssignProcessToJobObject(self.raw(), process as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// How many processes run in the job, nested jobs included.
    pub fn active_processes(&self) -> io::Result<u32> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        let r = unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if r == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }

    /// End every process in the job with exit code `code`.
    pub fn terminate(&self, code: u32) -> io::Result<()> {
        if unsafe { TerminateJobObject(self.raw(), code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

/// Put this process in the job `name` and close the handle: the job's creator keeps it
/// alive. Call it before starting any child, so every descendant is in the job too.
pub fn join_named(name: &str) -> io::Result<()> {
    let w = wide(name);
    let h = owned(unsafe { OpenJobObjectW(JOB_OBJECT_ASSIGN_PROCESS, 0, w.as_ptr()) })
        .map_err(|e| io::Error::new(e.kind(), format!("cannot open the job {name}: {e}")))?;
    if unsafe { AssignProcessToJobObject(h.as_raw_handle() as HANDLE, GetCurrentProcess()) } == 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            e.kind(),
            format!("cannot join the job {name}: {e}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// `CREATE_NO_WINDOW`, as the Desktop starts its Daemon.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    #[test]
    fn kill_on_close_ends_tree() {
        let job = Job::new().unwrap();
        // A child that starts its own child: both are in the job (the grandchild by
        // inheritance), so closing the job ends both.
        let mut c = Command::new("cmd.exe")
            .args([
                "/C",
                "ping -n 600 127.0.0.1 >NUL & ping -n 600 127.0.0.1 >NUL",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        job.assign(c.as_raw_handle()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while job.active_processes().unwrap() < 2 {
            assert!(Instant::now() < deadline, "ping never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(job);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if c.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the job's process survived its close"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn named_job_is_exclusive_and_joinable() {
        let name = format!(r"Local\xshell-test-{}", uuid::Uuid::new_v4().simple());
        let job = Job::new_named(&name).unwrap();
        let e = Job::new_named(&name).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(join_named(&format!("{name}-missing")).is_err());
        assert_eq!(job.active_processes().unwrap(), 0);
        job.terminate(1).unwrap();
    }
}

//! Attach-only Windows byte bridge. No server startup, protocol interpretation or flush.
use super::{Opts, Paths};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows_sys::Wdk::Storage::FileSystem::{
    FileModeInformation, NtQueryInformationFile, FILE_MODE_INFORMATION, FILE_SYNCHRONOUS_IO_ALERT,
    FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING,
    ERROR_NOT_FOUND, ERROR_NO_DATA, ERROR_OPERATION_ABORTED, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileType, ReadFile, WriteFile, FILE_TYPE_PIPE, FILE_TYPE_UNKNOWN,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, WaitForSingleObject};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CancelSynchronousIo, GetOverlappedResult, IO_STATUS_BLOCK, OVERLAPPED,
};
use xshell_core::pipe::PipeStream;
use xshell_protocol::{NOT_RUNNING_EXIT, NOT_RUNNING_MESSAGE};

const TICK: Duration = Duration::from_millis(10);
const CONNECT_WAIT: Duration = Duration::from_secs(15);
const BUFFER: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct BridgeLimits {
    stall: Duration,
}

impl Default for BridgeLimits {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(60),
        }
    }
}

impl BridgeLimits {
    fn from_env() -> io::Result<Self> {
        Self::with_probe(std::env::var_os("XSHELLD_TEST_CONNECT_STALL_MS").as_deref())
    }

    fn with_probe(value: Option<&std::ffi::OsStr>) -> io::Result<Self> {
        if cfg!(debug_assertions) {
            if let Some(value) = value {
                let ms = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "XSHELLD_TEST_CONNECT_STALL_MS must be positive milliseconds",
                        )
                    })?;
                return Ok(Self {
                    stall: Duration::from_millis(ms),
                });
            }
        }
        Ok(Self::default())
    }
}

pub fn run_connect(_opts: &Opts, paths: &Paths) -> i32 {
    let pipe =
        match xshell_core::pipe::connect(&paths.socket, Instant::now() + CONNECT_WAIT, &|| false) {
            Ok(pipe) => pipe,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                eprintln!("xshelld: {NOT_RUNNING_MESSAGE}");
                return NOT_RUNNING_EXIT;
            }
            Err(e) => {
                eprintln!("xshelld: {e}");
                return 1;
            }
        };
    let result = (|| {
        let limits = BridgeLimits::from_env()?;
        let stdin = StdioHandle::duplicate(unsafe { GetStdHandle(STD_INPUT_HANDLE) })?;
        let stdout = StdioHandle::duplicate(unsafe { GetStdHandle(STD_OUTPUT_HANDLE) })?;
        bridge(pipe, stdin, stdout, limits, Hooks::default())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("xshelld: {e}");
            1
        }
    }
}

fn raw(h: &OwnedHandle) -> HANDLE {
    h.as_raw_handle()
}

fn owned(h: HANDLE) -> io::Result<OwnedHandle> {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: callers supply only freshly created, uniquely owned handles.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoMode {
    Synchronous,
    Overlapped,
}

struct StdioHandle {
    handle: OwnedHandle,
    mode: IoMode,
}

impl StdioHandle {
    fn duplicate(original: HANDLE) -> io::Result<Self> {
        if original.is_null() || original == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "connect needs valid redirected stdin and stdout (SSH -T)",
            ));
        }
        let mut copy = std::ptr::null_mut();
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                original,
                GetCurrentProcess(),
                &mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let handle = owned(copy)?;
        let kind = unsafe { GetFileType(raw(&handle)) };
        if kind == FILE_TYPE_UNKNOWN {
            return Err(io::Error::other(
                "cannot classify connect stdio; use ordinary redirected stdio or an SSH -T session",
            ));
        }
        let mut status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let mut info = FILE_MODE_INFORMATION::default();
        let size = std::mem::size_of_val(&info);
        let result = unsafe {
            NtQueryInformationFile(
                raw(&handle),
                &mut status,
                (&mut info as *mut FILE_MODE_INFORMATION).cast(),
                size as u32,
                FileModeInformation,
            )
        };
        if result < 0 || unsafe { status.Anonymous.Status } < 0 || status.Information != size {
            return Err(io::Error::other(format!("cannot query connect stdio I/O mode (NTSTATUS {result:#x}); use ordinary redirected stdio or an SSH -T session")));
        }
        let synchronous =
            info.Mode & (FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT) != 0;
        let mode = classify_mode(synchronous, kind)?;
        Ok(Self { handle, mode })
    }

    fn read(&self, buf: &mut [u8], state: &State) -> io::Result<usize> {
        self.op(buf.as_mut_ptr(), buf.len(), false, state)
    }

    fn write(&self, buf: &[u8], state: &State) -> io::Result<usize> {
        self.op(buf.as_ptr().cast_mut(), buf.len(), true, state)
    }

    // The caller keeps the buffer and this handle live until op has collected completion.
    fn op(&self, ptr: *mut u8, len: usize, write: bool, state: &State) -> io::Result<usize> {
        if state.stopping() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        #[cfg(test)]
        state.hooks.before_stdio(write);
        let h = raw(&self.handle);
        let mut n = 0;
        if self.mode == IoMode::Synchronous {
            let ok = unsafe {
                if write {
                    WriteFile(h, ptr, len as u32, &mut n, std::ptr::null_mut())
                } else {
                    ReadFile(h, ptr, len as u32, &mut n, std::ptr::null_mut())
                }
            };
            return stdio_result(ok, n, write);
        }
        let event = owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = raw(&event);
        let ok = unsafe {
            if write {
                WriteFile(h, ptr, len as u32, std::ptr::null_mut(), &mut ov)
            } else {
                ReadFile(h, ptr, len as u32, std::ptr::null_mut(), &mut ov)
            }
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return stdio_error(e, write);
            }
        }
        #[cfg(test)]
        if ok == 0 {
            if let Some(tx) = &state.hooks.test.overlapped_pending {
                let _ = tx.send(write);
            }
        }
        let mut wait_error = None;
        loop {
            if state.stopping() {
                wait_error = cancel_overlapped(h, &ov).err();
                break;
            }
            match unsafe { WaitForSingleObject(raw(&event), TICK.as_millis() as u32) } {
                WAIT_OBJECT_0 => break,
                WAIT_TIMEOUT => continue,
                _ => {
                    wait_error = Some(io::Error::last_os_error());
                    let _ = cancel_overlapped(h, &ov); // retain the original wait failure
                    break;
                }
            }
        }
        // Cancel is only a request. Always drain, even if completion won the cancel race.
        // No early return may release ov, event, buffer or handle before this call.
        let completed = unsafe { GetOverlappedResult(h, &ov, &mut n, 1) };
        let result = stdio_result(completed, n, write);
        #[cfg(test)]
        state.hooks.test.drained.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = wait_error {
            return Err(e);
        }
        result
    }
}

fn cancel_overlapped(handle: HANDLE, overlapped: &OVERLAPPED) -> io::Result<()> {
    if unsafe { CancelIoEx(handle, overlapped) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
        Ok(())
    } else {
        Err(error)
    }
}

fn stopped_result(result: io::Result<()>, state: &State) -> io::Result<()> {
    match result {
        Err(error)
            if state.stopping()
                && (error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32)
                    || matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::NotConnected
                    )) =>
        {
            Ok(())
        }
        other => other,
    }
}

fn classify_mode(synchronous: bool, kind: u32) -> io::Result<IoMode> {
    if synchronous {
        Ok(IoMode::Synchronous)
    } else if kind == FILE_TYPE_PIPE {
        Ok(IoMode::Overlapped)
    } else {
        Err(io::Error::new(io::ErrorKind::Unsupported,
        "connect does not support asynchronous non-pipe stdio; use ordinary redirected stdio or an SSH -T session"))
    }
}

fn stdio_result(ok: i32, n: u32, write: bool) -> io::Result<usize> {
    if ok != 0 {
        Ok(n as usize)
    } else {
        stdio_error(io::Error::last_os_error(), write)
    }
}

fn stdio_error(e: io::Error, write: bool) -> io::Result<usize> {
    if !write
        && matches!(e.raw_os_error(), Some(c) if c == ERROR_BROKEN_PIPE as i32 || c == ERROR_HANDLE_EOF as i32 || c == ERROR_NO_DATA as i32)
    {
        Ok(0)
    } else {
        Err(e)
    }
}

#[derive(Default, Clone)]
struct Hooks {
    #[cfg(test)]
    test: Arc<TestHooks>,
}

struct State {
    stop: AtomicBool,
    stdout_pending: Mutex<Option<Instant>>,
    #[cfg(test)]
    hooks: Hooks,
}

impl State {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

fn write_all(
    mut bytes: &[u8],
    state: &State,
    mut write: impl FnMut(&[u8]) -> io::Result<usize>,
) -> io::Result<()> {
    while !bytes.is_empty() && !state.stopping() {
        match write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted && !state.stopping() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn pump(pipe: PipeStream, stdio: Arc<StdioHandle>, state: &State, input: bool) -> io::Result<()> {
    let mut pipe = pipe;
    let mut buf = vec![0; BUFFER];
    while !state.stopping() {
        let n = match if input {
            stdio.read(&mut buf, state)
        } else {
            pipe.read(&mut buf)
        } {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted && !state.stopping() => continue,
            Err(e) => return Err(e),
        };
        write_all(&buf[..n], state, |bytes| {
            if input {
                pipe.write(bytes)
            } else {
                stdout_write(state, || stdio.write(bytes, state))
            }
        })?;
    }
    Ok(())
}

fn stdout_write(state: &State, write: impl FnOnce() -> io::Result<usize>) -> io::Result<usize> {
    *state.stdout_pending.lock().unwrap() = Some(Instant::now());
    let result = write();
    *state.stdout_pending.lock().unwrap() = None;
    result
}

fn cancel_worker(worker: &JoinHandle<()>) -> io::Result<()> {
    if unsafe { CancelSynchronousIo(worker.as_raw_handle()) } != 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
        Ok(())
    } else {
        Err(e)
    }
}

fn bridge(
    pipe: PipeStream,
    stdin: StdioHandle,
    stdout: StdioHandle,
    limits: BridgeLimits,
    hooks: Hooks,
) -> io::Result<()> {
    pipe.set_write_timeout(Some(limits.stall))?;
    let state = Arc::new(State {
        stop: AtomicBool::new(false),
        stdout_pending: Mutex::new(None),
        #[cfg(test)]
        hooks: hooks.clone(),
    });
    let stdin = Arc::new(stdin);
    let stdout = Arc::new(stdout);
    let (tx, rx) = mpsc::channel();
    let mut workers = Vec::with_capacity(2);
    let mut failure = None;
    for (input, stdio) in [(true, stdin.clone()), (false, stdout.clone())] {
        let state = state.clone();
        let pipe = pipe.clone();
        let tx = tx.clone();
        #[cfg(test)]
        let worker_hooks = hooks.clone();
        let worker = spawn_worker(input, &hooks, move || {
            #[cfg(test)]
            let _finished = WorkerFinished(worker_hooks.clone());
            #[cfg(test)]
            assert_ne!(
                worker_hooks.test.panic_worker,
                Some(input),
                "injected worker panic"
            );
            let result = pump(pipe, stdio, &state, input);
            // A cancellation caused by the other pump is not another transport error.
            let result = stopped_result(result, &state);
            let _ = tx.send(result);
        });
        match worker {
            Ok(worker) => workers.push(worker),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    drop(tx);
    if failure.is_none() {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(result) => {
                    failure = result.err();
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let stalled = state
                .stdout_pending
                .lock()
                .unwrap()
                .is_some_and(|at| at.elapsed() >= limits.stall);
            if stalled {
                failure = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connect stdout write made no progress before the stall deadline",
                ));
                break;
            }
            if workers.iter().any(JoinHandle::is_finished) {
                break;
            }
        }
    }
    state.stop.store(true, Ordering::SeqCst);
    if let Err(e) = pipe.shutdown(Shutdown::Both) {
        failure.get_or_insert(e);
    }
    // Repeat on BOTH threads until finished: ERROR_NOT_FOUND can race syscall start.
    while workers.iter().any(|w| !w.is_finished()) {
        for worker in &workers {
            if !worker.is_finished() {
                let result = cancel_worker(worker);
                #[cfg(test)]
                hooks.test.cancel_ticks
                    [usize::from(worker.thread().name() == Some("connect-stdout"))]
                .fetch_add(1, Ordering::SeqCst);
                if let Err(e) = result {
                    failure.get_or_insert(e);
                }
            }
        }
        thread::sleep(TICK);
    }
    for worker in workers {
        if worker.join().is_err() {
            failure.get_or_insert_with(|| io::Error::other("connect worker panicked"));
        }
    }
    for result in rx.try_iter() {
        if let Err(e) = result {
            failure.get_or_insert(e);
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn spawn_worker(
    input: bool,
    _hooks: &Hooks,
    f: impl FnOnce() + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    #[cfg(test)]
    if !input && _hooks.test.fail_second_spawn {
        return Err(io::Error::other("injected worker start failure"));
    }
    thread::Builder::new()
        .name(
            if input {
                "connect-stdin"
            } else {
                "connect-stdout"
            }
            .into(),
        )
        .spawn(f)
}

#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    fail_second_spawn: bool,
    panic_worker: Option<bool>,
    cancel_ticks: [std::sync::atomic::AtomicUsize; 2],
    drained: std::sync::atomic::AtomicUsize,
    overlapped_pending: Option<mpsc::Sender<bool>>,
    finished: std::sync::atomic::AtomicUsize,
    before_read: Option<Arc<StartGate>>,
    before_write: Option<Arc<StartGate>>,
}

#[cfg(test)]
struct WorkerFinished(Hooks);
#[cfg(test)]
impl Drop for WorkerFinished {
    fn drop(&mut self) {
        self.0.test.finished.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
struct StartGate {
    entered: mpsc::SyncSender<Instant>,
    release: Mutex<mpsc::Receiver<()>>,
    once: AtomicBool,
}

#[cfg(test)]
impl Hooks {
    fn before_stdio(&self, write: bool) {
        let gate = if write {
            &self.test.before_write
        } else {
            &self.test.before_read
        };
        if let Some(gate) = gate {
            if !gate.once.swap(true, Ordering::SeqCst) {
                gate.entered.send(Instant::now()).unwrap();
                gate.release.lock().unwrap().recv().unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, GENERIC_READ, GENERIC_WRITE,
        HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_TYPE_DISK, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, CreatePipe, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use xshell_core::pipe::PipeListener;

    const SHORT: Duration = Duration::from_millis(250);
    const WAIT: Duration = Duration::from_secs(5);

    // An independent process watchdog bounds even a broken cancellation/join implementation.
    // No bridge subprocesses are started in these unit tests: all workers live in this child.
    fn isolated(name: &str) -> bool {
        let name = format!("connect::windows::tests::{name}");
        if std::env::var("XSHELLD_CONNECT_UNIT_CHILD").as_deref() == Ok(name.as_str()) {
            return true;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env("XSHELLD_CONNECT_UNIT_CHILD", &name)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name}: {status}");
                return false;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{name}: watchdog timeout");
            }
            thread::sleep(TICK);
        }
    }

    fn unique() -> std::path::PathBuf {
        format!(r"\\.\pipe\xshelld-connect-unit-{}", uuid::Uuid::new_v4()).into()
    }

    fn pair() -> (PipeStream, PipeStream) {
        let name = unique();
        let listener = PipeListener::bind(&name).unwrap();
        let peer = thread::spawn(move || listener.accept().unwrap());
        let client = xshell_core::pipe::connect(&name, Instant::now() + WAIT, &|| false).unwrap();
        let peer = peer.join().unwrap();
        peer.set_read_timeout(Some(WAIT)).unwrap();
        peer.set_write_timeout(Some(WAIT)).unwrap();
        (client, peer)
    }

    fn anonymous() -> (File, File) {
        let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_ne!(
            unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 4096) },
            0
        );
        (File::from(owned(r).unwrap()), File::from(owned(w).unwrap()))
    }

    // The stdio side is overlapped; its peer is synchronous and may safely use File I/O.
    fn overlapped() -> (OwnedHandle, File) {
        let name = unique();
        let wide: Vec<u16> = name.as_os_str().encode_wide().chain(Some(0)).collect();
        let server = owned(unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        })
        .unwrap();
        let event =
            owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) }).unwrap();
        let mut ov = OVERLAPPED {
            hEvent: raw(&event),
            ..Default::default()
        };
        assert_eq!(unsafe { ConnectNamedPipe(raw(&server), &mut ov) }, 0);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(ERROR_IO_PENDING as i32)
        );
        let client = owned(unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        })
        .unwrap();
        let mut n = 0;
        assert_ne!(
            unsafe { GetOverlappedResult(raw(&server), &ov, &mut n, 1) },
            0
        );
        (server, File::from(client))
    }

    fn duplicate(h: &impl AsRawHandle) -> StdioHandle {
        StdioHandle::duplicate(h.as_raw_handle()).unwrap()
    }
    fn state(hooks: Hooks) -> Arc<State> {
        Arc::new(State {
            stop: AtomicBool::new(false),
            stdout_pending: Mutex::new(None),
            hooks,
        })
    }
    fn limits() -> BridgeLimits {
        BridgeLimits { stall: SHORT }
    }
    fn hooks(test: TestHooks) -> Hooks {
        Hooks {
            test: Arc::new(test),
        }
    }

    fn binary_roundtrip(async_stdio: bool) {
        let (pipe, peer) = pair();
        let (input, mut writer, output, mut reader) = if async_stdio {
            let (input, writer) = overlapped();
            let (output, reader) = overlapped();
            (
                StdioHandle::duplicate(raw(&input)).unwrap(),
                writer,
                StdioHandle::duplicate(raw(&output)).unwrap(),
                reader,
            )
        } else {
            let (input, writer) = anonymous();
            let (reader, output) = anonymous();
            (duplicate(&input), writer, duplicate(&output), reader)
        };
        assert_eq!(
            input.mode,
            if async_stdio {
                IoMode::Overlapped
            } else {
                IoMode::Synchronous
            }
        );
        let (pending_tx, pending_rx) = mpsc::channel();
        let hook = hooks(TestHooks {
            overlapped_pending: async_stdio.then_some(pending_tx),
            ..Default::default()
        });
        let observed = hook.clone();
        let bridge =
            thread::spawn(move || bridge(pipe, input, output, BridgeLimits { stall: WAIT }, hook));
        if async_stdio {
            assert!(
                !pending_rx.recv_timeout(WAIT).unwrap(),
                "stdin read must really pend"
            );
        }
        let bytes: Vec<u8> = (0..(2 << 20)).map(|i| (i % 256) as u8).collect();
        let sent = bytes.clone();
        let (hold_tx, hold_rx) = mpsc::channel();
        let producer = thread::spawn(move || {
            writer.write_all(&sent).unwrap();
            hold_rx.recv_timeout(WAIT).unwrap();
        });
        let mut got = vec![0; bytes.len()];
        (&peer).read_exact(&mut got).unwrap();
        assert_eq!(got, bytes);
        let sent = bytes.clone();
        let p = peer.clone();
        let producer2 = thread::spawn(move || (&p).write_all(&sent).unwrap());
        if async_stdio {
            while !pending_rx.recv_timeout(WAIT).unwrap() {} // stdout write must really pend
        }
        reader.read_exact(&mut got).unwrap();
        assert_eq!(got, bytes);
        producer2.join().unwrap();
        hold_tx.send(()).unwrap();
        producer.join().unwrap();
        bridge.join().unwrap().unwrap();
        assert_eq!(observed.test.finished.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn synchronous_stdio_preserves_binary_bytes() {
        if isolated("synchronous_stdio_preserves_binary_bytes") {
            binary_roundtrip(false);
        }
    }
    #[test]
    fn overlapped_pipe_stdio_preserves_binary_bytes() {
        if isolated("overlapped_pipe_stdio_preserves_binary_bytes") {
            binary_roundtrip(true);
        }
    }
    #[test]
    fn stdio_duplicates_are_non_inheritable_and_do_not_close_originals() {
        if !isolated("stdio_duplicates_are_non_inheritable_and_do_not_close_originals") {
            return;
        }
        let (mut original, mut writer) = anonymous();
        let (mut reader, mut output) = anonymous();
        for original in [original.as_raw_handle(), output.as_raw_handle()] {
            assert_ne!(
                unsafe { SetHandleInformation(original, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) },
                0
            );
        }
        let input_copy = duplicate(&original);
        let output_copy = duplicate(&output);
        for copy in [&input_copy, &output_copy] {
            let mut flags = 0;
            assert_ne!(
                unsafe { GetHandleInformation(raw(&copy.handle), &mut flags) },
                0
            );
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        }
        let (pipe, peer) = pair();
        let hook = Hooks::default();
        let seen = hook.clone();
        let b = thread::spawn(move || bridge(pipe, input_copy, output_copy, limits(), hook));
        peer.shutdown(Shutdown::Both).unwrap();
        b.join().unwrap().unwrap();
        assert_eq!(seen.test.finished.load(Ordering::SeqCst), 2);
        for original in [original.as_raw_handle(), output.as_raw_handle()] {
            let mut flags = 0;
            assert_ne!(unsafe { GetHandleInformation(original, &mut flags) }, 0);
            assert_eq!(flags & HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
        }
        writer.write_all(b"x").unwrap();
        let mut one = [0];
        original.read_exact(&mut one).unwrap();
        assert_eq!(one, *b"x");
        output.write_all(b"y").unwrap();
        reader.read_exact(&mut one).unwrap();
        assert_eq!(one, *b"y");
    }
    #[test]
    fn unsupported_async_non_pipe_stdio_is_actionable() {
        use std::os::windows::fs::OpenOptionsExt;
        if !isolated("unsupported_async_non_pipe_stdio_is_actionable") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(dir.path().join("async-stdio"))
            .unwrap();
        let e = StdioHandle::duplicate(file.as_raw_handle()).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(e
            .to_string()
            .contains("ordinary redirected stdio or an SSH -T session"));
        assert_eq!(
            classify_mode(false, FILE_TYPE_DISK).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
    #[test]
    fn stdio_write_all_advances_after_partial_writes() {
        let s = state(Hooks::default());
        let mut got = Vec::new();
        let mut attempt = 0;
        write_all(b"0123456789", &s, |bytes| {
            attempt += 1;
            if attempt == 2 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = bytes.len().min(attempt);
            got.extend_from_slice(&bytes[..n]);
            Ok(n)
        })
        .unwrap();
        assert_eq!(got, b"0123456789");
        assert_eq!(
            write_all(b"x", &s, |_| Ok(0)).unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        let mut attempts = 0;
        assert_eq!(
            write_all(b"x", &s, |_| {
                attempts += 1;
                s.stop.store(true, Ordering::SeqCst);
                Err(io::ErrorKind::Interrupted.into())
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(attempts, 1);
        write_all(b"x", &s, |_| panic!("no write after stop")).unwrap();
    }

    fn gate() -> (
        Arc<StartGate>,
        mpsc::Receiver<Instant>,
        mpsc::SyncSender<()>,
    ) {
        let (entered, rx) = mpsc::sync_channel(1);
        let (tx, release) = mpsc::sync_channel(1);
        (
            Arc::new(StartGate {
                entered,
                release: Mutex::new(release),
                once: AtomicBool::new(false),
            }),
            rx,
            tx,
        )
    }

    fn fill_stdout_pipe(output: &mut File) {
        use windows_sys::Wdk::Storage::FileSystem::{
            FilePipeLocalInformation, FILE_PIPE_LOCAL_INFORMATION,
        };
        let mut status = IO_STATUS_BLOCK::default();
        let mut info = FILE_PIPE_LOCAL_INFORMATION::default();
        let size = std::mem::size_of_val(&info);
        let result = unsafe {
            NtQueryInformationFile(
                output.as_raw_handle(),
                &mut status,
                (&mut info as *mut FILE_PIPE_LOCAL_INFORMATION).cast(),
                size as u32,
                FilePipeLocalInformation,
            )
        };
        assert!(result >= 0 && unsafe { status.Anonymous.Status } >= 0);
        assert_eq!(status.Information, size);
        assert!(info.WriteQuotaAvailable > 0);
        output
            .write_all(&vec![0; info.WriteQuotaAvailable as usize])
            .unwrap();
    }

    fn synchronous_start_race(write: bool) {
        let (gate, entered, release) = gate();
        let hook = hooks(TestHooks {
            before_read: (!write).then(|| gate.clone()),
            before_write: write.then_some(gate),
            ..Default::default()
        });
        let seen = hook.clone();
        let (pipe, peer) = pair();
        let (input, _writer) = anonymous();
        let (_reader, mut output) = anonymous();
        if write {
            fill_stdout_pipe(&mut output);
        }
        let b = thread::spawn(move || {
            bridge(pipe, duplicate(&input), duplicate(&output), limits(), hook)
        });
        let p = peer.clone();
        // The stdout fixture is already quota-full: even a one-byte write must block.
        let producer = write.then(|| {
            thread::spawn(move || {
                let _ = (&p).write_all(&vec![1; 4 << 20]);
            })
        });
        entered.recv_timeout(WAIT).unwrap();
        if write {
            // Stop through input EOF while output is poised to enter WriteFile.
            drop(_writer);
        } else {
            peer.shutdown(Shutdown::Both).unwrap();
        }
        let by = Instant::now() + WAIT;
        while seen.test.cancel_ticks[usize::from(write)].load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < by);
            thread::sleep(TICK);
        }
        // The first CancelSynchronousIo has already seen no outstanding call.
        release.send(()).unwrap();
        b.join().unwrap().unwrap();
        if let Some(producer) = producer {
            producer.join().unwrap();
        }
        assert_eq!(seen.test.finished.load(Ordering::SeqCst), 2);
        assert!(seen.test.cancel_ticks[usize::from(write)].load(Ordering::SeqCst) >= 2);
    }
    #[test]
    fn cancel_before_synchronous_read_starts_is_not_lost() {
        if isolated("cancel_before_synchronous_read_starts_is_not_lost") {
            synchronous_start_race(false);
        }
    }
    #[test]
    fn cancel_before_synchronous_write_starts_is_not_lost() {
        if isolated("cancel_before_synchronous_write_starts_is_not_lost") {
            synchronous_start_race(true);
        }
    }

    fn cancel_overlapped(write: bool) {
        let (handle, _peer) = overlapped();
        let stdio = duplicate(&handle);
        let (tx, rx) = mpsc::channel();
        let hook = hooks(TestHooks {
            overlapped_pending: Some(tx),
            ..Default::default()
        });
        let s = state(hook.clone());
        let worker_state = s.clone();
        let worker = thread::spawn(move || {
            if write {
                stdio.write(&vec![2; 4 << 20], &worker_state)
            } else {
                stdio.read(&mut [0; 64], &worker_state)
            }
        });
        rx.recv_timeout(WAIT).unwrap();
        assert!(!worker.is_finished());
        s.stop.store(true, Ordering::SeqCst);
        assert!(worker.join().unwrap().is_err());
        assert_eq!(hook.test.drained.load(Ordering::SeqCst), 1);
        // The server handle and peer are released only after the drain assertion.
        drop(handle);
    }
    #[test]
    fn cancel_pending_overlapped_read_drains_before_return() {
        if isolated("cancel_pending_overlapped_read_drains_before_return") {
            cancel_overlapped(false);
        }
    }
    #[test]
    fn cancel_pending_overlapped_write_drains_before_return() {
        if isolated("cancel_pending_overlapped_write_drains_before_return") {
            cancel_overlapped(true);
        }
    }

    fn blocked_stdout(close_peer: bool) {
        let (pipe, peer) = pair();
        let (input, _writer) = anonymous();
        let (_reader, output) = anonymous();
        let (gate, entered, release) = gate();
        let hook = hooks(TestHooks {
            before_write: Some(gate),
            ..Default::default()
        });
        let seen = hook.clone();
        let b = thread::spawn(move || {
            bridge(pipe, duplicate(&input), duplicate(&output), limits(), hook)
        });
        // No deadline while stdin and Daemon reads are idle.
        thread::sleep(SHORT * 2);
        assert!(!b.is_finished());
        let p = peer.clone();
        let producer = thread::spawn(move || {
            let _ = (&p).write_all(&vec![3; 4 << 20]);
        });
        let at = entered.recv_timeout(WAIT).unwrap();
        release.send(()).unwrap();
        if close_peer {
            peer.shutdown(Shutdown::Both).unwrap();
        }
        let e = b.join().unwrap().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(e.to_string().contains("stdout"));
        assert!(at.elapsed() >= SHORT - TICK * 2);
        assert!(at.elapsed() < WAIT);
        producer.join().unwrap();
        assert_eq!(seen.test.finished.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn stdout_stall_deadline_only_runs_during_pending_write() {
        if !isolated("stdout_stall_deadline_only_runs_during_pending_write") {
            return;
        }
        // Force partial suffixes through the real publication/write-all path. No
        // pending interval survives a positive return or the idle gap between attempts.
        let (mut reader, output) = anonymous();
        let writer = duplicate(&output);
        let s = state(Hooks::default());
        let mut previous = None;
        let mut got = Vec::new();
        write_all(b"abc", &s, |bytes| {
            assert!(s.stdout_pending.lock().unwrap().is_none());
            let result = stdout_write(&s, || {
                let current = s.stdout_pending.lock().unwrap().unwrap();
                assert!(previous.is_none_or(|at| current > at));
                previous = Some(current);
                writer.write(&bytes[..1], &s)
            });
            assert!(s.stdout_pending.lock().unwrap().is_none());
            let mut one = [0];
            reader.read_exact(&mut one).unwrap();
            got.push(one[0]);
            thread::sleep(SHORT + TICK);
            result
        })
        .unwrap();
        assert_eq!(got, b"abc");
        blocked_stdout(false);
    }
    #[test]
    fn daemon_close_with_stdout_blocked_exits_after_stall_bound() {
        if isolated("daemon_close_with_stdout_blocked_exits_after_stall_bound") {
            blocked_stdout(true);
        }
    }
    #[test]
    fn bridge_returns_only_after_all_workers_joined() {
        if !isolated("bridge_returns_only_after_all_workers_joined") {
            return;
        }
        for direction in 0..4 {
            let (pipe, peer) = pair();
            let (input, writer) = anonymous();
            let (reader, output) = anonymous();
            let hook = hooks(TestHooks {
                panic_worker: (direction == 3).then_some(true),
                ..Default::default()
            });
            let seen = hook.clone();
            let b = thread::spawn(move || {
                bridge(pipe, duplicate(&input), duplicate(&output), limits(), hook)
            });
            match direction {
                0 => drop(writer),
                1 => {
                    // Keep the synchronous stdin writer open throughout direct return.
                    peer.shutdown(Shutdown::Both).unwrap();
                }
                2 => {
                    drop(reader);
                    (&peer).write_all(b"broken stdout").unwrap();
                    assert!(b.join().unwrap().is_err());
                    assert_eq!(seen.test.finished.load(Ordering::SeqCst), 2);
                    continue;
                }
                _ => {
                    assert!(b
                        .join()
                        .unwrap()
                        .unwrap_err()
                        .to_string()
                        .contains("panicked"));
                    assert_eq!(seen.test.finished.load(Ordering::SeqCst), 2);
                    continue;
                }
            }
            b.join().unwrap().unwrap();
            assert_eq!(seen.test.finished.load(Ordering::SeqCst), 2);
        }
    }
    #[test]
    fn partial_worker_start_failure_cleans_up_started_worker() {
        if !isolated("partial_worker_start_failure_cleans_up_started_worker") {
            return;
        }
        let (pipe, _peer) = pair();
        let (input, _writer) = anonymous();
        let (_reader, output) = anonymous();
        let hook = hooks(TestHooks {
            fail_second_spawn: true,
            ..Default::default()
        });
        let seen = hook.clone();
        assert!(
            bridge(pipe, duplicate(&input), duplicate(&output), limits(), hook)
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        assert_eq!(seen.test.finished.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn release_stall_limit_is_sixty_seconds() {
        assert_eq!(BridgeLimits::default().stall, Duration::from_secs(60));
        assert_eq!(
            BridgeLimits::with_probe(None).unwrap().stall,
            Duration::from_secs(60)
        );
        for value in ["0", "bad", "", "-1"] {
            let result = BridgeLimits::with_probe(Some(value.as_ref()));
            if cfg!(debug_assertions) {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().stall, Duration::from_secs(60));
            }
        }
        let limit = BridgeLimits::with_probe(Some("123".as_ref())).unwrap();
        assert_eq!(
            limit.stall,
            if cfg!(debug_assertions) {
                Duration::from_millis(123)
            } else {
                Duration::from_secs(60)
            }
        );
    }
}

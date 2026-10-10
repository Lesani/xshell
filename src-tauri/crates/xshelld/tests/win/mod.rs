//! A lean harness for the Windows tests: temp homes, a unique pipe per Daemon, `.cmd` fake
//! agents, process checks, and a protocol client over a named pipe. The Unix harness
//! (`common`) is left untouched.
#![allow(dead_code)]

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_core::pipe::PipeStream;
use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
use xshell_protocol::msg::{
    decode_server, encode_msg, ClientMsg, Hello, OpenSpec, ProtocolRange, ServerMsg, TerminalInfo,
};

/// Generous: ConPTY, cmd.exe and PowerShell start slowly on CI runners.
pub const T: Duration = Duration::from_secs(30);

/// `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`, as the Desktop starts its Daemon.
pub const DAEMON_FLAGS: u32 = 0x0800_0000 | 0x0000_0200;

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_xshelld")
}

pub fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ── Processes ─────────────────────────────────────────────────────────────

/// Whether process `pid` runs.
pub fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return false;
    }
    let running = unsafe { WaitForSingleObject(h, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(h) };
    running
}

pub fn wait_dead(pid: u32, within: Duration) -> bool {
    wait_until(within, || !alive(pid))
}

/// End `pid` now (cleanup of a process a failing test may leave).
pub fn kill_pid(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if !h.is_null() {
        unsafe {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

/// Kills the listed pids on drop.
#[derive(Default)]
pub struct PidReaper(pub Vec<u32>);

impl Drop for PidReaper {
    fn drop(&mut self) {
        for &p in &self.0 {
            kill_pid(p);
        }
    }
}

// ── Temp homes ────────────────────────────────────────────────────────────

pub struct TestHome {
    pub dir: tempfile::TempDir,
    /// This home's Daemon pipe.
    pub pipe: PathBuf,
}

impl Default for TestHome {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHome {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("xd").tempdir().unwrap();
        fs::create_dir(dir.path().join("home")).unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        TestHome {
            dir,
            pipe: unique_pipe(),
        }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    pub fn log(&self) -> PathBuf {
        self.home().join(".xshell").join("log").join("xshelld.log")
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(self.log()).unwrap_or_default()
    }

    pub fn mode(&self) -> Option<String> {
        fs::read_to_string(self.home().join(".xshell").join("daemon").join("mode"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    pub fn state_ids(&self) -> Vec<Uuid> {
        let p = self
            .home()
            .join(".xshell")
            .join("daemon")
            .join("terminals.json");
        let Ok(b) = fs::read(p) else {
            return Vec::new();
        };
        let v: Value = serde_json::from_slice(&b).unwrap();
        v["terminals"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| t["terminal"].as_str().unwrap().parse().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A project directory (a Terminal cwd).
    pub fn project(&self, name: &str) -> PathBuf {
        let p = self.dir.path().join("projects").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// A batch file `bin\<name>.cmd`, found on the Daemon's PATH.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let p = self.bin_dir().join(format!("{name}.cmd"));
        fs::write(
            &p,
            format!("@echo off\r\n{}\r\n", body.replace('\n', "\r\n")),
        )
        .unwrap();
        p
    }

    /// PATH for the Daemon: the fake agents first.
    pub fn path_env(&self) -> String {
        format!(
            "{};{}",
            self.bin_dir().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// `xshelld` with this home, pipe and PATH, started as the Desktop starts it.
    pub fn cmd(&self) -> Command {
        let mut c = Command::new(bin());
        c.env("XSHELLD_HOME", self.home())
            .env("XSHELLD_SOCKET", &self.pipe)
            .env("PATH", self.path_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir(self.home())
            .creation_flags(DAEMON_FLAGS);
        c
    }

    /// The env a hostlink `LocalDaemon` passes to the Daemons it starts.
    pub fn daemon_env(&self) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        vec![
            ("XSHELLD_HOME".into(), self.home().into()),
            ("XSHELLD_SOCKET".into(), self.pipe.clone().into()),
            ("PATH".into(), self.path_env().into()),
        ]
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "── xshelld.log of {} ──\n{}",
                self.home().display(),
                self.log_text()
            );
        }
    }
}

pub fn unique_pipe() -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\xshelld-test-{}",
        Uuid::new_v4().simple()
    ))
}

// ── Daemons ───────────────────────────────────────────────────────────────

/// A `serve --gui-bound` this test process is the parent of. Dropping it stops it (stop
/// event, then kill).
pub struct Daemon {
    pub child: Child,
}

impl Daemon {
    pub fn start(h: &TestHome) -> Daemon {
        Self::start_with(h, &[])
    }

    pub fn start_with(h: &TestHome, env: &[(&str, &str)]) -> Daemon {
        let mut c = h.cmd();
        c.args(["serve", "--gui-bound", "--parent-pid"])
            .arg(std::process::id().to_string());
        for (k, v) in env {
            c.env(k, v);
        }
        Daemon {
            child: c.spawn().unwrap(),
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The stop event, as the Desktop's quit sets it.
    pub fn stop(&self) {
        xshell_core::pipe::set_stop_event(self.pid()).expect("the Daemon's stop event");
    }

    pub fn wait_exit(&mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return Some(st.code().unwrap_or(-1));
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = xshell_core::pipe::set_stop_event(self.pid());
            if self.wait_exit(Duration::from_secs(15)).is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

/// Connect to `pipe`, retrying while it does not exist yet.
pub fn connect_pipe(pipe: &Path) -> PipeStream {
    let deadline = Instant::now() + T;
    loop {
        match xshell_core::pipe::connect(pipe, deadline, &|| false) {
            Ok(s) => return s,
            Err(e) if Instant::now() >= deadline => panic!("connect {}: {e}", pipe.display()),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

// ── Specs ─────────────────────────────────────────────────────────────────

/// A raw `cmd.exe` Terminal.
pub fn cmd_spec(cwd: &Path) -> LaunchSpec {
    LaunchSpec {
        cwd: cwd.to_string_lossy().into_owned(),
        shell_mode: Some("raw".into()),
        shell_command: Some("cmd.exe".into()),
        ..Default::default()
    }
}

/// A Claude Code Terminal (the fake `claude.cmd`, through `cmd.exe /C`).
pub fn claude_spec(cwd: &Path) -> LaunchSpec {
    LaunchSpec {
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

pub fn open_msg(id: Uuid, launch: LaunchSpec) -> ClientMsg {
    ClientMsg::TermOpen {
        spec: OpenSpec {
            terminal: id,
            launch,
            meta: Map::new(),
            // Wide, so ConPTY never wraps a marker.
            cols: 200,
            rows: 40,
            first_message: None,
        },
    }
}

// ── Protocol client ───────────────────────────────────────────────────────

/// A protocol client with a background reader: messages are logged in order and consumed
/// by `expect_msg`; output accumulates per Terminal.
pub struct Client {
    w: Box<dyn Write + Send>,
    rx: Receiver<Option<Frame>>,
    pub log: Vec<ServerMsg>,
    consumed: HashSet<usize>,
    pub out: HashMap<Uuid, Vec<u8>>,
    marks: HashMap<Uuid, usize>,
    eof: bool,
    next_id: u64,
    keep: Option<PipeStream>,
}

impl Client {
    pub fn connect(pipe: &Path) -> Client {
        let s = connect_pipe(pipe);
        let mut c = Client::from_io(s.clone(), s.clone());
        c.keep = Some(s);
        c
    }

    pub fn from_io(r: impl Read + Send + 'static, w: impl Write + Send + 'static) -> Client {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(r);
            loop {
                match read_frame(&mut r, MAX_FRAME_LEN) {
                    Ok(Some(f)) => {
                        if tx.send(Some(f)).is_err() {
                            return;
                        }
                    }
                    _ => {
                        let _ = tx.send(None);
                        return;
                    }
                }
            }
        });
        Client {
            w: Box::new(w),
            rx,
            log: Vec::new(),
            consumed: HashSet::new(),
            out: HashMap::new(),
            marks: HashMap::new(),
            eof: false,
            next_id: 1000,
            keep: None,
        }
    }

    pub fn send(&mut self, msg: &ClientMsg, id: Option<u64>) {
        let f = encode_msg(msg, id).unwrap();
        let _ = self.w.write_all(&f).and_then(|_| self.w.flush());
    }

    fn pump(&mut self, deadline: Instant) -> bool {
        if self.eof {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(left) {
            Ok(Some(Frame::Json(j))) => {
                let m = decode_server(&j).unwrap_or_else(|e| panic!("bad server message: {e}"));
                self.log.push(m);
                true
            }
            Ok(Some(Frame::Output { terminal, data })) => {
                self.out
                    .entry(terminal)
                    .or_default()
                    .extend_from_slice(&data);
                true
            }
            Ok(Some(Frame::Unknown { .. })) => true,
            Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                self.eof = true;
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
        }
    }

    pub fn try_msg(
        &mut self,
        within: Duration,
        pred: impl Fn(&ServerMsg) -> bool,
    ) -> Option<ServerMsg> {
        let deadline = Instant::now() + within;
        let mut i = 0;
        loop {
            while i < self.log.len() {
                if !self.consumed.contains(&i) && pred(&self.log[i]) {
                    self.consumed.insert(i);
                    return Some(self.log[i].clone());
                }
                i += 1;
            }
            if Instant::now() >= deadline || (!self.pump(deadline) && self.eof) {
                return None;
            }
        }
    }

    pub fn expect_msg(&mut self, what: &str, pred: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        self.try_msg(T, pred).unwrap_or_else(|| {
            panic!(
                "no {what} within {T:?}; log: {:?}; output: {:?}",
                self.log,
                self.out
                    .values()
                    .map(|o| String::from_utf8_lossy(o).into_owned())
                    .collect::<Vec<_>>()
            )
        })
    }

    pub fn hello(&mut self) -> (Hello, Vec<TerminalInfo>) {
        self.send(
            &ClientMsg::Hello(Hello {
                protocol: ProtocolRange { min: 1, max: 1 },
                version: "test".into(),
                capabilities: vec!["call".into(), "term".into()],
            }),
            None,
        );
        let ServerMsg::Hello(h) = self.expect_msg("hello", |m| matches!(m, ServerMsg::Hello(_)))
        else {
            unreachable!()
        };
        let list = self.terminals();
        (h, list)
    }

    pub fn terminals(&mut self) -> Vec<TerminalInfo> {
        match self.expect_msg("terminals", |m| matches!(m, ServerMsg::Terminals { .. })) {
            ServerMsg::Terminals { list } => list,
            _ => unreachable!(),
        }
    }

    pub fn terminals_where(&mut self, pred: impl Fn(&[TerminalInfo]) -> bool) -> Vec<TerminalInfo> {
        match self.expect_msg(
            "matching terminals list",
            |m| matches!(m, ServerMsg::Terminals { list } if pred(list)),
        ) {
            ServerMsg::Terminals { list } => list,
            _ => unreachable!(),
        }
    }

    pub fn request(&mut self, msg: &ClientMsg) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(msg, Some(id));
        match self.expect_msg(
            &format!("res {id}"),
            |m| matches!(m, ServerMsg::Res(r) if r.id == id),
        ) {
            ServerMsg::Res(r) => r.outcome.into_result(),
            _ => unreachable!(),
        }
    }

    /// Open a Terminal; returns its pid.
    pub fn open(&mut self, id: Uuid, launch: LaunchSpec) -> u32 {
        let v = self
            .request(&open_msg(id, launch))
            .unwrap_or_else(|e| panic!("term.open failed: {e}"));
        v["pid"].as_u64().expect("a pid") as u32
    }

    pub fn attach(&mut self, t: Uuid) {
        self.request(&ClientMsg::TermAttach { terminal: t })
            .unwrap_or_else(|e| panic!("term.attach failed: {e}"));
    }

    pub fn close(&mut self, t: Uuid) {
        self.request(&ClientMsg::TermClose { terminal: t })
            .unwrap_or_else(|e| panic!("term.close failed: {e}"));
    }

    pub fn input(&mut self, t: Uuid, data: &str) {
        self.send(
            &ClientMsg::TermInput {
                terminal: t,
                data: data.into(),
            },
            None,
        );
    }

    /// Output of `t` since the last match, up to and including `needle`.
    pub fn output_until(&mut self, t: Uuid, needle: &str) -> Vec<u8> {
        let deadline = Instant::now() + T;
        loop {
            let mark = *self.marks.get(&t).unwrap_or(&0);
            if let Some(buf) = self.out.get(&t) {
                if let Some(i) = find(&buf[mark..], needle.as_bytes()) {
                    let end = mark + i + needle.len();
                    let got = buf[mark..end].to_vec();
                    self.marks.insert(t, end);
                    return got;
                }
            }
            if Instant::now() >= deadline || (!self.pump(deadline) && self.eof) {
                let buf = self.out.get(&t).cloned().unwrap_or_default();
                panic!(
                    "no {needle:?} from {t} within {T:?}; got {:?}",
                    String::from_utf8_lossy(&buf)
                );
            }
        }
    }

    /// Type a `set /a` sum and wait for its result, which the echoed input never contains.
    pub fn marker(&mut self, t: Uuid, n: u32) {
        self.input(t, &format!("set /a {n}*1000+{n}\r"));
        self.output_until(t, &(n * 1000 + n).to_string());
    }

    pub fn expect_eof(&mut self) {
        let deadline = Instant::now() + T;
        while !self.eof {
            if !self.pump(deadline) && Instant::now() >= deadline {
                panic!("connection still open after {T:?}");
            }
        }
    }
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The first line of `path` once it exists (a pid a fake agent wrote).
pub fn read_pid_file(path: &Path) -> u32 {
    let mut pid = None;
    assert!(
        wait_until(T, || {
            pid = fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().trim_start_matches('\u{feff}').parse().ok());
            pid.is_some()
        }),
        "{} never named a pid",
        path.display()
    );
    pid.unwrap()
}

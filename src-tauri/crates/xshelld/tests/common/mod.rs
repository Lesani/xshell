//! Shared harness for the xshelld integration tests: temp homes, in-process servers, the
//! binary, and a protocol client built only from xshell-core's codec.
#![allow(dead_code)]

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_core::protocol::frame::{read_frame, write_frame, Frame, FrameDecoder, MAX_FRAME_LEN};
use xshell_core::protocol::msg::{
    decode_server, encode_msg, ClientMsg, Hello, OpenSpec, ProtocolRange, ServerMsg, TerminalInfo,
};
use xshelld::paths::{resolve, Paths};
use xshelld::server::{Config, Server, ServerHandle};

pub const T: Duration = Duration::from_secs(5);

pub fn range(min: u32, max: u32) -> ProtocolRange {
    ProtocolRange { min, max }
}

// ── Temp homes ────────────────────────────────────────────────────────────

pub struct TestHome {
    pub dir: TempDir,
    /// `dir` with symlinks resolved (macOS `/tmp` is `/private/tmp`), so paths compare equal
    /// to what processes started in them report.
    root: PathBuf,
}

impl Default for TestHome {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHome {
    /// Under /tmp so socket paths stay well inside `sun_path`.
    pub fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("xd")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir(root.join("home")).unwrap();
        fs::create_dir(root.join("run")).unwrap();
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { dir, root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn paths(&self) -> Paths {
        resolve(&self.home(), Some(&self.run()), None)
    }

    /// A project directory (a Terminal cwd).
    pub fn project(&self, name: &str) -> PathBuf {
        let p = self.root.join("projects").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// An executable `#!/bin/sh` script in the temp dir's `bin/`.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let dir = self.root.join("bin");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    pub fn state_json(&self) -> Value {
        match fs::read(self.paths().state) {
            Ok(b) => serde_json::from_slice(&b).unwrap(),
            Err(_) => Value::Null,
        }
    }

    pub fn state_ids(&self) -> Vec<Uuid> {
        self.state_json()["terminals"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| t["terminal"].as_str().unwrap().parse().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A raw Terminal running `program` directly (raw mode spawns the shell command as is).
pub fn raw_spec(cwd: &Path, program: &Path) -> LaunchSpec {
    LaunchSpec {
        cwd: cwd.to_string_lossy().into_owned(),
        shell_mode: Some("raw".into()),
        shell_command: Some(program.to_string_lossy().into_owned()),
        ..Default::default()
    }
}

/// An interactive `/bin/sh`.
pub fn sh_spec(cwd: &Path) -> LaunchSpec {
    raw_spec(cwd, Path::new("/bin/sh"))
}

pub fn open_msg(id: Uuid, launch: LaunchSpec) -> ClientMsg {
    ClientMsg::TermOpen {
        spec: OpenSpec {
            terminal: id,
            launch,
            cols: 80,
            rows: 24,
            meta: Map::new(),
        },
    }
}

// ── In-process server ─────────────────────────────────────────────────────

pub fn config(h: &TestHome) -> Config {
    let mut c = Config::new(h.home(), h.paths());
    c.kill_grace = Duration::from_millis(300);
    c.write_stall_timeout = Duration::from_secs(1);
    c.resize_persist_delay = Duration::from_millis(200);
    c
}

pub fn start(h: &TestHome, tweak: impl FnOnce(&mut Config)) -> ServerHandle {
    let mut c = config(h);
    tweak(&mut c);
    Server::start(c).expect("server starts")
}

// ── Processes ─────────────────────────────────────────────────────────────

pub fn alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // A zombie is dead for our purposes.
    #[cfg(target_os = "linux")]
    if let Ok(s) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(rest) = s.rfind(')').map(|i| &s[i + 1..]) {
            if rest.split_whitespace().next() == Some("Z") {
                return false;
            }
        }
    }
    true
}

pub fn wait_dead(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

/// Kills the listed pids when dropped (tests that leave HUP-ignoring processes behind on
/// purpose clean them up even when an assertion fails first).
#[derive(Default)]
pub struct PidReaper(pub Vec<i32>);

impl Drop for PidReaper {
    fn drop(&mut self) {
        for &p in &self.0 {
            if p > 1 {
                unsafe { libc::kill(p, libc::SIGKILL) };
            }
        }
    }
}

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_xshelld")
}

/// `xshelld` with the test home's environment and a short idle timeout, so any Daemon a
/// test leaves behind exits by itself.
pub fn bin_cmd(h: &TestHome) -> Command {
    let mut c = Command::new(bin());
    c.env("HOME", h.home())
        .env("XDG_RUNTIME_DIR", h.run())
        .env("XSHELLD_LOGIN_ENV", "0")
        .env("XSHELLD_IDLE_TIMEOUT_MS", "3000")
        .env_remove("XSHELLD_HOME")
        .env_remove("XSHELLD_SOCKET");
    c
}

/// Ends the Daemon named by the test home's pidfile when dropped: SIGTERM (an orderly
/// shutdown), then SIGKILL after 5 s. Runs on assertion-failure paths too.
pub struct DaemonGuard {
    pidfile: PathBuf,
}

impl DaemonGuard {
    pub fn new(h: &TestHome) -> Self {
        Self {
            pidfile: h.paths().pid,
        }
    }

    pub fn pid(&self) -> Option<i32> {
        fs::read_to_string(&self.pidfile).ok()?.trim().parse().ok()
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            if alive(pid) {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                if !wait_dead(pid, T) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }
}

/// `xshelld serve` as a child. Dropping it SIGTERMs, then SIGKILLs after 5 s, and reaps.
pub struct ServeProc {
    pub child: Child,
}

impl ServeProc {
    pub fn start(h: &TestHome, env: &[(&str, &str)]) -> ServeProc {
        let mut c = bin_cmd(h);
        c.arg("serve").stdin(Stdio::null()).stdout(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        ServeProc {
            child: c.spawn().unwrap(),
        }
    }

    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    pub fn wait_exit(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return Some(s);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ServeProc {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.pid(), libc::SIGTERM) };
            if self.wait_exit(T).is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

/// Connect to the socket, retrying while the server comes up.
pub fn connect_socket(path: &Path) -> UnixStream {
    let deadline = Instant::now() + T;
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return s,
            Err(e) if Instant::now() >= deadline => panic!("connect {}: {e}", path.display()),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

// ── Protocol client ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Ev {
    Msg(ServerMsg),
    Out(Uuid, Vec<u8>),
}

/// A protocol client with a background reader. Everything received is kept in `log`, in
/// order; `expect_msg` consumes messages, output accumulates per Terminal.
pub struct Client {
    w: Box<dyn Write + Send>,
    rx: Receiver<Option<Frame>>,
    pub log: Vec<Ev>,
    consumed: HashSet<usize>,
    pub out: HashMap<Uuid, Vec<u8>>,
    marks: HashMap<Uuid, usize>,
    eof: bool,
    next_id: u64,
    /// Keep only a short tail of output and leave it out of `log` (for floods).
    pub quiet: bool,
    _keep: Option<UnixStream>,
}

impl Client {
    pub fn connect(socket: &Path) -> Client {
        let s = connect_socket(socket);
        let r = s.try_clone().unwrap();
        let mut c = Client::from_io(r, s.try_clone().unwrap());
        c._keep = Some(s);
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
            quiet: false,
            _keep: None,
        }
    }

    pub fn shutdown(&self) {
        if let Some(s) = &self._keep {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn send(&mut self, msg: &ClientMsg, id: Option<u64>) {
        let f = encode_msg(msg, id).unwrap();
        let _ = self.w.write_all(&f).and_then(|_| self.w.flush());
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        let _ = self.w.write_all(bytes).and_then(|_| self.w.flush());
    }

    pub fn send_frame(&mut self, f: &Frame) {
        let _ = write_frame(&mut self.w, f);
    }

    /// Receive one frame into the log. `false` on timeout or EOF.
    fn pump(&mut self, deadline: Instant) -> bool {
        if self.eof {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(left) {
            Ok(Some(Frame::Json(j))) => {
                let m = decode_server(&j).unwrap_or_else(|e| panic!("bad server message: {e}"));
                self.log.push(Ev::Msg(m));
                true
            }
            Ok(Some(Frame::Output { terminal, data })) => {
                let buf = self.out.entry(terminal).or_default();
                buf.extend_from_slice(&data);
                if self.quiet {
                    if buf.len() > 1 << 20 {
                        let cut = buf.len() - 4096;
                        buf.drain(..cut);
                        let m = self.marks.entry(terminal).or_default();
                        *m = m.saturating_sub(cut);
                    }
                } else {
                    self.log.push(Ev::Out(terminal, data));
                }
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

    /// The first unconsumed message matching `pred`, waiting up to `within`.
    pub fn try_msg(
        &mut self,
        within: Duration,
        pred: impl Fn(&ServerMsg) -> bool,
    ) -> Option<ServerMsg> {
        let deadline = Instant::now() + within;
        let mut i = 0;
        loop {
            while i < self.log.len() {
                if !self.consumed.contains(&i) {
                    if let Ev::Msg(m) = &self.log[i] {
                        if pred(m) {
                            self.consumed.insert(i);
                            return Some(m.clone());
                        }
                    }
                }
                i += 1;
            }
            // Check the deadline even while frames keep coming.
            if Instant::now() >= deadline || (!self.pump(deadline) && self.eof) {
                return None;
            }
        }
    }

    pub fn expect_msg(&mut self, what: &str, pred: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        self.try_msg(T, pred)
            .unwrap_or_else(|| panic!("no {what} within {T:?}; log: {:?}", self.summary()))
    }

    /// A compact view of the log for failure messages.
    pub fn summary(&self) -> Vec<String> {
        self.log
            .iter()
            .map(|e| match e {
                Ev::Msg(m) => format!("{m:?}"),
                Ev::Out(t, d) => format!(
                    "out {} {:?}",
                    &t.to_string()[..8],
                    String::from_utf8_lossy(d)
                ),
            })
            .collect()
    }

    pub fn hello(&mut self, r: ProtocolRange) -> (Hello, Vec<TerminalInfo>) {
        self.send(
            &ClientMsg::Hello(Hello {
                protocol: r,
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

    /// The next `terminals` list.
    pub fn terminals(&mut self) -> Vec<TerminalInfo> {
        match self.expect_msg("terminals", |m| matches!(m, ServerMsg::Terminals { .. })) {
            ServerMsg::Terminals { list } => list,
            _ => unreachable!(),
        }
    }

    /// Wait for a `terminals` list satisfying `pred`, skipping older ones.
    pub fn terminals_where(&mut self, pred: impl Fn(&[TerminalInfo]) -> bool) -> Vec<TerminalInfo> {
        let m = self.expect_msg(
            "matching terminals list",
            |m| matches!(m, ServerMsg::Terminals { list } if pred(list)),
        );
        match m {
            ServerMsg::Terminals { list } => list,
            _ => unreachable!(),
        }
    }

    pub fn request_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Send with an id and wait for its `res`.
    pub fn request(&mut self, msg: &ClientMsg) -> Result<Value, String> {
        let id = self.request_id();
        self.send(msg, Some(id));
        self.wait_res(id)
    }

    pub fn wait_res(&mut self, id: u64) -> Result<Value, String> {
        match self.expect_msg(
            &format!("res {id}"),
            |m| matches!(m, ServerMsg::Res(r) if r.id == id),
        ) {
            ServerMsg::Res(r) => r.outcome.into_result(),
            _ => unreachable!(),
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.request(&ClientMsg::Call {
            method: method.into(),
            params,
        })
    }

    pub fn open(&mut self, id: Uuid, launch: LaunchSpec) -> Value {
        self.request(&open_msg(id, launch))
            .unwrap_or_else(|e| panic!("term.open failed: {e}"))
    }

    pub fn attach(&mut self, t: Uuid) -> Value {
        self.request(&ClientMsg::TermAttach { terminal: t })
            .unwrap_or_else(|e| panic!("term.attach failed: {e}"))
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

    pub fn resize(&mut self, t: Uuid, cols: u16, rows: u16) {
        self.request(&ClientMsg::TermResize {
            terminal: t,
            cols,
            rows,
        })
        .unwrap();
    }

    /// Output of `t` since the last match, up to and including `needle` (waits up to 5 s).
    pub fn try_output_until(
        &mut self,
        t: Uuid,
        needle: &[u8],
        within: Duration,
    ) -> Option<Vec<u8>> {
        let deadline = Instant::now() + within;
        loop {
            let mark = *self.marks.get(&t).unwrap_or(&0);
            if let Some(buf) = self.out.get(&t) {
                if let Some(i) = find(&buf[mark..], needle) {
                    let end = mark + i + needle.len();
                    let got = buf[mark..end].to_vec();
                    self.marks.insert(t, end);
                    return Some(got);
                }
            }
            // Check the deadline even while frames keep coming.
            if Instant::now() >= deadline || (!self.pump(deadline) && self.eof) {
                return None;
            }
        }
    }

    /// Forget output received so far: later matches only see newer bytes.
    pub fn skip_output(&mut self, t: Uuid) {
        let n = self.out.get(&t).map_or(0, |b| b.len());
        self.marks.insert(t, n);
    }

    pub fn output_until(&mut self, t: Uuid, needle: &str) -> Vec<u8> {
        let mark = *self.marks.get(&t).unwrap_or(&0);
        self.try_output_until(t, needle.as_bytes(), T)
            .unwrap_or_else(|| {
                let buf = self
                    .out
                    .get(&t)
                    .map(|b| &b[mark.min(b.len())..])
                    .unwrap_or(&[]);
                panic!(
                    "no {needle:?} from {t} within {T:?}; got {:?}",
                    String::from_utf8_lossy(buf)
                )
            })
    }

    /// Type `echo ab""c`-style input and wait for the joined marker, which only the
    /// command's output (not the echoed input line) contains.
    pub fn marker(&mut self, t: Uuid, word: &str) {
        let (a, b) = word.split_at(word.len() / 2);
        self.input(t, &format!("echo {a}\"\"{b}\n"));
        self.output_until(t, word);
    }

    /// Poll `stty size` until it reports `rows cols`. Polling, because an attach's redraw
    /// nudge briefly shrinks the Terminal by a row.
    pub fn expect_size(&mut self, t: Uuid, rows: u16, cols: u16) {
        let want = format!("{rows} {cols}\r\n");
        let deadline = Instant::now() + T;
        while Instant::now() < deadline {
            self.input(t, "stty size\n");
            if self
                .try_output_until(t, want.as_bytes(), Duration::from_millis(300))
                .is_some()
            {
                return;
            }
        }
        panic!("size never became {rows}x{cols}: {:?}", self.summary());
    }

    pub fn expect_eof(&mut self) {
        let deadline = Instant::now() + T;
        while !self.eof {
            if !self.pump(deadline) && Instant::now() >= deadline {
                panic!("connection still open after {T:?}");
            }
        }
    }

    /// Drain whatever arrives within `d`.
    pub fn drain_for(&mut self, d: Duration) {
        let deadline = Instant::now() + d;
        while Instant::now() < deadline {
            if !self.pump(deadline) && self.eof {
                break;
            }
        }
    }

    pub fn is_eof(&self) -> bool {
        self.eof
    }
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// What a [`RawClient`] read produced.
#[derive(Debug)]
pub enum Got {
    Frame(Frame),
    /// Nothing arrived in time; the connection is still open.
    Timeout,
    /// The Daemon closed (or reset) the connection.
    Eof,
}

/// A client that only reads when asked: nothing drains its socket in the background, so the
/// Daemon really sees a stalled peer. Reads wait with `poll` and feed a frame decoder, so a
/// timeout never loses a partial frame and needs no socket option (macOS refuses
/// `setsockopt` on a socket the peer has shut down).
pub struct RawClient {
    pub sock: UnixStream,
    dec: FrameDecoder,
    eof: bool,
    next_id: u64,
}

impl RawClient {
    pub fn connect(socket: &Path) -> RawClient {
        RawClient {
            sock: connect_socket(socket),
            dec: FrameDecoder::new(),
            eof: false,
            next_id: 1,
        }
    }

    pub fn send(&mut self, msg: &ClientMsg, id: Option<u64>) {
        self.sock.write_all(&encode_msg(msg, id).unwrap()).unwrap();
    }

    /// The next frame, a timeout, or EOF.
    pub fn read(&mut self, within: Duration) -> Got {
        let deadline = Instant::now() + within;
        loop {
            match self.dec.next_frame() {
                Ok(Some(f)) => return Got::Frame(f),
                Ok(None) => {}
                Err(e) => panic!("bad frame from the Daemon: {e}"),
            }
            if self.eof {
                return Got::Eof;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd {
                fd: self.sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n == 0 {
                return Got::Timeout;
            }
            if n < 0 {
                continue; // EINTR
            }
            let mut buf = [0u8; 64 * 1024];
            match (&self.sock).read(&mut buf) {
                Ok(0) | Err(_) => self.eof = true,
                Ok(k) => self.dec.feed(&buf[..k]),
            }
        }
    }

    /// Hello and wait for the first `terminals` list.
    pub fn hello(&mut self) {
        self.send(
            &ClientMsg::Hello(Hello {
                protocol: range(1, 1),
                version: "test".into(),
                capabilities: vec![],
            }),
            None,
        );
        loop {
            match self.read(T) {
                Got::Frame(Frame::Json(j)) => {
                    if matches!(decode_server(&j).unwrap(), ServerMsg::Terminals { .. }) {
                        return;
                    }
                }
                g => panic!("unexpected {g:?}"),
            }
        }
    }

    pub fn attach(&mut self, t: Uuid) {
        self.next_id += 1;
        self.send(&ClientMsg::TermAttach { terminal: t }, Some(self.next_id));
    }

    /// Read until EOF or until `within` passes. Returns `(eof, needle seen in any output)`.
    pub fn drain_until_eof(&mut self, within: Duration, needle: &[u8]) -> (bool, bool) {
        let deadline = Instant::now() + within;
        let mut seen = false;
        let mut tail: Vec<u8> = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.read(left) {
                Got::Frame(Frame::Output { data, .. }) => {
                    tail.extend_from_slice(&data);
                    if find(&tail, needle).is_some() {
                        seen = true;
                    }
                    let keep = tail.len().saturating_sub(needle.len());
                    tail.drain(..keep);
                }
                Got::Frame(_) => {}
                Got::Eof => return (true, seen),
                Got::Timeout => return (false, seen),
            }
        }
    }
}

/// `xshelld connect` as a child, with a protocol client over its stdio.
pub struct ConnectProc {
    pub child: Child,
    pub client: Client,
}

impl ConnectProc {
    pub fn start(h: &TestHome) -> ConnectProc {
        let mut child = bin_cmd(h)
            .arg("connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin: ChildStdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        ConnectProc {
            child,
            client: Client::from_io(stdout, stdin),
        }
    }

    /// Close stdin (EOF to the Daemon) and wait for `connect` to exit.
    pub fn finish(mut self) -> std::process::ExitStatus {
        self.client.w = Box::new(std::io::sink());
        let deadline = Instant::now() + T;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("connect did not exit after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ConnectProc {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn ok_pid(v: &Value) -> i32 {
    v["pid"].as_i64().expect("pid") as i32
}

pub fn get_home(c: &mut Client) -> Value {
    c.call("get_home_dir", json!({})).unwrap()
}

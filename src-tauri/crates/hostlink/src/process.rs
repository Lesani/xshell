//! Child processes (ssh, or `sh` in tests): spawned in their own process group, registered on
//! a cancel token, killed as a group, and reaped. stderr is kept as a short ANSI-free tail.

use crate::cancel::{CancelHook, CancelToken};
use crate::transport::{CommandSpec, Transport};
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TAIL_BYTES: usize = 4096;
const POLL: Duration = Duration::from_millis(10);

/// The last `TAIL_BYTES` of a stream.
#[derive(Debug, Default)]
pub struct Tail {
    buf: VecDeque<u8>,
    done: bool,
}

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        let over = self.buf.len().saturating_sub(TAIL_BYTES);
        self.buf.drain(..over);
    }

    /// The tail as text, ANSI escapes stripped and trimmed.
    pub fn text(&self) -> String {
        let bytes: Vec<u8> = self.buf.iter().copied().collect();
        strip_ansi(&String::from_utf8_lossy(&bytes))
            .trim()
            .to_string()
    }
}

/// Drop CSI/OSC escape sequences and other control characters except `\n` and `\t`.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.peek() {
                Some('[') => {
                    it.next();
                    // Parameters and intermediates, then one final byte in @..~.
                    for c in it.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    it.next();
                    // OSC ends at BEL or ESC \.
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    it.next();
                }
                None => {}
            }
        } else if c == '\r' {
            // ssh prints "\r\n"; keep just the newline.
        } else if c.is_control() && c != '\n' && c != '\t' {
        } else {
            out.push(c);
        }
    }
    out
}

struct Cell {
    child: Child,
    reaped: Option<ExitStatus>,
}

/// A shared, killable child.
#[derive(Clone)]
pub struct ChildCell {
    inner: Arc<Mutex<Cell>>,
    pid: u32,
}

impl ChildCell {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// SIGKILL the child's process group (unix), or the child. A reaped child is left
    /// alone: its pid may belong to someone else by now.
    pub fn kill(&self) {
        let mut c = self.inner.lock().unwrap();
        if c.reaped.is_some() {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
        let _ = c.child.kill();
    }

    pub fn try_wait(&self) -> Option<ExitStatus> {
        let mut c = self.inner.lock().unwrap();
        if let Some(s) = c.reaped {
            return Some(s);
        }
        match c.child.try_wait() {
            Ok(Some(s)) => {
                c.reaped = Some(s);
                Some(s)
            }
            _ => None,
        }
    }

    pub fn wait_timeout(&self, d: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + d;
        loop {
            if let Some(s) = self.try_wait() {
                return Some(s);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Kill, then reap (bounded).
    pub fn kill_and_wait(&self, d: Duration) -> Option<ExitStatus> {
        self.kill();
        self.wait_timeout(d)
    }
}

pub struct Proc {
    pub child: ChildCell,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    stderr: Arc<Mutex<Tail>>,
    _hook: CancelHook,
}

impl Proc {
    pub fn pid(&self) -> u32 {
        self.child.pid()
    }

    pub fn stderr_text(&self) -> String {
        self.stderr.lock().unwrap().text()
    }

    /// Wait (bounded) until stderr reached EOF, so the tail is complete.
    pub fn wait_stderr(&self, d: Duration) {
        let deadline = Instant::now() + d;
        while !self.stderr.lock().unwrap().done && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.child.kill_and_wait(Duration::from_secs(2));
    }
}

fn build(spec: &CommandSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own group: a stop kills ssh and anything a test shell started, and a Ctrl-C in
        // the terminal that launched the Desktop does not reach it.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Spawn `spec`, killed when `cancel` fires (or right away if it already has).
pub fn spawn(spec: &CommandSpec, cancel: &CancelToken) -> io::Result<Proc> {
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let mut child = build(spec).spawn()?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let pid = child.id();
    let cell = ChildCell {
        inner: Arc::new(Mutex::new(Cell {
            child,
            reaped: None,
        })),
        pid,
    };
    let stderr = Arc::new(Mutex::new(Tail::default()));
    if let Some(mut e) = stderr_pipe {
        let tail = stderr.clone();
        std::thread::Builder::new()
            .name("child-stderr".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match e.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => tail.lock().unwrap().push(&buf[..n]),
                    }
                }
                tail.lock().unwrap().done = true;
            })?;
    } else {
        stderr.lock().unwrap().done = true;
    }
    let killer = cell.clone();
    let hook = cancel.on_cancel(move || killer.kill());
    Ok(Proc {
        child: cell,
        stdin,
        stdout,
        stderr,
        _hook: hook,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum ProcError {
    Spawn(io::Error),
    Timeout,
    Cancelled,
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcError::Spawn(e) => write!(f, "cannot start the command: {e}"),
            ProcError::Timeout => write!(f, "the command timed out"),
            ProcError::Cancelled => write!(f, "cancelled"),
        }
    }
}

const MAX_STDOUT: usize = 1024 * 1024;

/// Run `remote_cmd` through `t` to completion, feeding `stdin` from its own thread while
/// stdout and stderr drain concurrently (no pipe deadlock).
pub fn run_script(
    t: &dyn Transport,
    remote_cmd: &str,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Output, ProcError> {
    let mut p = match spawn(&t.command(remote_cmd), cancel) {
        Ok(p) => p,
        Err(_) if cancel.is_cancelled() => return Err(ProcError::Cancelled),
        Err(e) => return Err(ProcError::Spawn(e)),
    };
    if let Some(mut w) = p.stdin.take() {
        let data = stdin.unwrap_or_default();
        std::thread::Builder::new()
            .name("child-stdin".into())
            .spawn(move || {
                let _ = w.write_all(&data);
                // Dropping `w` closes the pipe: EOF for the remote `cat`.
            })
            .map_err(ProcError::Spawn)?;
    }
    let (tx, rx) = mpsc::channel();
    if let Some(mut out) = p.stdout.take() {
        std::thread::Builder::new()
            .name("child-stdout".into())
            .spawn(move || {
                let mut v = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match out.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if v.len() < MAX_STDOUT {
                                v.extend_from_slice(&buf[..n]);
                            }
                        }
                    }
                }
                let _ = tx.send(v);
            })
            .map_err(ProcError::Spawn)?;
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        // Checked first: the cancel hook's kill also makes the child exit.
        if cancel.is_cancelled() {
            p.child.kill_and_wait(Duration::from_secs(2));
            return Err(ProcError::Cancelled);
        }
        if let Some(s) = p.child.try_wait() {
            break s;
        }
        if Instant::now() >= deadline {
            p.child.kill_and_wait(Duration::from_secs(2));
            return Err(ProcError::Timeout);
        }
        std::thread::sleep(POLL);
    };
    // A grandchild may keep the pipes open; do not wait for it forever.
    let stdout = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    p.wait_stderr(Duration::from_millis(500));
    Ok(Output {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: p.stderr_text(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::transport::LocalShellTransport;

    #[test]
    fn strip_ansi_and_tail() {
        assert_eq!(
            strip_ansi("\x1b[31mred\x1b[0m\r\n\x1b]0;t\x07x\x01"),
            "red\nx"
        );
        let mut t = Tail::default();
        t.push(&vec![b'a'; 5000]);
        t.push(b"end ");
        assert_eq!(t.buf.len(), TAIL_BYTES);
        assert!(t.text().ends_with("aend"));
    }

    #[test]
    fn run_script_feeds_stdin_and_drains_both() {
        let t = LocalShellTransport::default();
        let big = vec![b'x'; 3 * 1024 * 1024];
        let out = run_script(
            &t,
            "wc -c; echo oops >&2; exit 3",
            Some(big),
            Duration::from_secs(10),
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "3145728");
        assert_eq!(out.stderr, "oops");
        assert_eq!(out.code, Some(3));
    }

    #[test]
    fn run_script_timeout_and_cancel() {
        let t = LocalShellTransport::default();
        let start = Instant::now();
        let r = run_script(
            &t,
            "exec sleep 100",
            None,
            Duration::from_millis(100),
            &CancelToken::new(),
        );
        assert!(matches!(r, Err(ProcError::Timeout)));
        let c = CancelToken::new();
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c2.cancel();
        });
        let r = run_script(&t, "sleep 100", None, Duration::from_secs(60), &c);
        assert!(matches!(r, Err(ProcError::Cancelled)));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

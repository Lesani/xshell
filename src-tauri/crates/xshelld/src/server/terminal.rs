//! One Terminal: a PTY process plus its reader, waiter and input threads, its replay buffer
//! and the connections attached to it.

use super::outbox::Outbox;
use super::prompt_cell::{Outcome, PromptCell, Read as PromptRead};
use super::registry::{frame, now_ms, overflowed, Daemon, Overflow, Registry};
use super::size::SizeArbiter;
use super::{ConnId, TestPoint};
use portable_pty::{native_pty_system, MasterPty, PtySize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::{AgentStatus, HookAgent, TerminalHooks, Tracker};
use xshell_core::launch::{relaunch_spec, LaunchSpec};
use xshell_core::prompt::{composer, composer_images, extract, screen_tail, ScreenModel};
use xshell_core::terminal::replay::ReplayBuffer;
use xshell_core::terminal::state::{Leader, PersistedTerminal, ProcIdentity};
use xshell_protocol::msg::{
    encode_res, LastLine, ServerMsg, TerminalInfo, PROMPT_ANSWERED, SUBMIT_NEEDS_YOU,
    SUBMIT_NOT_CHAT, SUBMIT_NOT_READY, SUBMIT_NO_NONBLOCK, SUBMIT_STUCK, SUBMIT_UNCONFIRMED,
    SUBMIT_UNSUPPORTED,
};

const READ_BUF: usize = 16 * 1024;
const INPUT_BACKLOG: usize = 1024;
/// Replay is queued in pieces so the per-connection output cap applies to it as well.
const REPLAY_CHUNK: usize = 256 * 1024;
const OVERFLOW_NUDGE_EVERY: Duration = Duration::from_secs(1);

struct Record {
    spec: LaunchSpec,
    meta: Map<String, Value>,
    created_at_ms: u64,
    /// The `skipPermissions` a Relaunch in progress will start with. Persisted in its place,
    /// so a restart during the Relaunch restores what the user asked for.
    pending_skip: Option<bool>,
    /// The process a Relaunch started to replace this Terminal's, persisted as this
    /// Terminal's leader from the moment it exists until it is listed in its own right or
    /// confirmed gone: a restart in between ends it instead of running a second agent.
    replacement: Option<Leader>,
}

struct TermIo {
    /// `None` for a Terminal restored without a process (see [`unresolved`]).
    master: Option<Box<dyn MasterPty + Send>>,
    arb: SizeArbiter,
}

struct TermOutput {
    replay: ReplayBuffer,
    subs: HashMap<ConnId, Arc<Outbox>>,
    /// Set when `term.exit` is published; no output follows it.
    exit_code: Option<i32>,
}

#[derive(Default)]
struct Life {
    closing: bool,
    /// A Relaunch is ending the process to start a replacement: its exit is held back.
    relaunching: bool,
    reader_done: bool,
    exited: Option<i32>,
    /// An exit held back for a Relaunch, published only if the Relaunch fails.
    held_exit: Option<i32>,
}

/// How a listed Terminal holds the agent session its spec names (capability
/// `term.open-existing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionHold {
    /// Running, or in the middle of a Relaunch (its exit is held back): it is the session.
    Live,
    /// `term.close` is ending it: a new agent on the session would run next to it.
    Closing,
    /// Restored without a process, leftovers of its previous run maybe still running.
    Unresolved,
    /// Its process ended: it holds nothing.
    Ended,
}

impl Life {
    /// How a Terminal in this state holds its session; `unresolved` says it was restored
    /// without a process while its previous run's leader is kept.
    fn hold(&self, unresolved: bool) -> SessionHold {
        if self.closing {
            SessionHold::Closing
        } else if self.exited.is_none() || self.relaunching {
            SessionHold::Live
        } else if unresolved {
            SessionHold::Unresolved
        } else {
            SessionHold::Ended
        }
    }

    /// Whether a Relaunch may start.
    fn check_relaunch(&self) -> Result<(), String> {
        if self.exited.is_some() {
            Err("terminal has exited".into())
        } else if self.closing {
            Err("terminal is closing".into())
        } else if self.relaunching {
            Err("terminal is already relaunching".into())
        } else {
            Ok(())
        }
    }
}

/// An agent Terminal's screen model, for its Permission Prompts.
struct Screen {
    model: ScreenModel,
    agent: HookAgent,
}

impl Screen {
    /// The prompt the screen shows, if any, and its rows.
    fn read(&self) -> (Option<xshell_core::prompt::Found>, Vec<String>) {
        let rows = self.model.rows();
        (extract(self.agent, &rows), rows)
    }
}

/// How far the input thread got: inputs written, and the screen revision at the last write.
/// One lock, taken last and alone, so the two are read together.
type Written = Mutex<(u64, u64)>;

/// One item for a Terminal's input thread.
pub(crate) enum Input {
    /// Bytes written as they are.
    Bytes(Vec<u8>),
    /// A reply from the Chat View (`term.submit`).
    Submit(Box<Submit>),
}

/// Which write of a reply the gate runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitStep {
    /// A piece of the paste, after checking the agent still accepts the reply.
    Paste,
    /// The first piece of a file's paste: as [`SubmitStep::Paste`], and the images
    /// attached in the composer are counted under the same check, as the baseline the file
    /// must add one to.
    PasteFile,
    /// Enter, after the pause, after checking again.
    Enter,
    /// The rest of a paste that was stopped: whatever completes the bracketed paste frame
    /// (`ESC [201~`), written whatever the readiness, so the agent never stays inside a
    /// paste that swallows the next key.
    Close,
}

/// One write of a reply: what the PTY takes now, without blocking.
pub(crate) type WriteNow<'a> = dyn FnMut() -> std::io::Result<usize> + 'a;
/// Runs a write of a reply with the agent's readiness held (checked, except to
/// [`SubmitStep::Close`]): the refusal, or the write's result.
type SubmitGate =
    Box<dyn FnMut(SubmitStep, &mut WriteNow<'_>) -> Result<std::io::Result<usize>, String> + Send>;
type SubmitDone = Box<dyn FnOnce(Result<(), String>) + Send>;
/// Whether a reply's `n`-th file (from 1) shows attached in the agent's composer: `Ok(false)`
/// not yet, an `Err` when the reply must stop (the agent needs you, the Terminal ended).
pub(crate) type SubmitAttach = Box<dyn FnMut(usize) -> Result<bool, String> + Send>;
/// How long a reply waits for a file to show attached (`Config::submit_attach_timeout`).
pub(crate) const SUBMIT_ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// A paste is written in pieces of at most this many bytes, each under its own check.
const SUBMIT_CHUNK: usize = 1024;
/// How long the input thread waits (holding nothing) when the PTY takes no more input.
const SUBMIT_WAIT: Duration = Duration::from_millis(10);
/// A PTY that takes nothing of a reply for this long ends it (`Config::submit_stall`).
pub(crate) const SUBMIT_STALL: Duration = Duration::from_secs(10);
/// The end of a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";
/// The start of a bracketed paste.
const PASTE_START: &[u8] = b"\x1b[200~";

/// A reply typed by the input thread: the paste, a pause, then Enter. Every write runs inside
/// the gate, which holds the agent's readiness while it checks it and writes, so nothing the
/// check did not see lands before the write. A paste stopped part way is completed with
/// [`PASTE_END`] (never with more of the text, never with Enter); if even that cannot be
/// written within `stall`, `stuck` is called. `done` gets the outcome exactly once: `Ok` once
/// Enter was written; the refusal when nothing was written; [`SUBMIT_UNCONFIRMED`] when (part
/// of) the paste was written but Enter was not (also when the Terminal ends with the reply
/// half typed).
pub(crate) struct Submit {
    /// Everything typed before Enter: one or more bracketed pastes (`frames`), with a space
    /// typed after each file's.
    pub paste: Vec<u8>,
    frames: Vec<Range<usize>>,
    /// Offsets in `paste` after which `attach` must answer `true` before more is typed.
    waits: Vec<usize>,
    /// Whether the `n`-th file (from 1) shows attached; an `Err` stops the reply. Called
    /// with nothing held, every [`SUBMIT_WAIT`], for at most `attach_timeout`.
    attach: Option<SubmitAttach>,
    pub attach_timeout: Duration,
    pub enter_after: Duration,
    pub stall: Duration,
    pub gate: SubmitGate,
    done: Option<SubmitDone>,
    stuck: Option<Box<dyn FnOnce() + Send>>,
    /// Some of the paste was written.
    pasted: bool,
    /// Each bracketed paste's start and end marker is a piece of its own, and no piece ends
    /// inside a UTF-8 character (Windows: the ConPTY's input decoder is not trusted with a
    /// marker or a character split across two reads).
    pub markers_alone: bool,
    /// The most bytes one piece of the paste has ([`SUBMIT_CHUNK`]; a probe makes it less).
    pub max_piece: usize,
}

impl Submit {
    pub fn new(typing: Typing, enter_after: Duration, gate: SubmitGate, done: SubmitDone) -> Self {
        Self {
            paste: typing.bytes,
            frames: typing.frames,
            waits: typing.waits,
            attach: None,
            attach_timeout: SUBMIT_ATTACH_TIMEOUT,
            enter_after,
            stall: SUBMIT_STALL,
            gate,
            done: Some(done),
            stuck: None,
            pasted: false,
            markers_alone: false,
            max_piece: SUBMIT_CHUNK,
        }
    }

    /// Whether a file's paste starts at `off`.
    fn file_starts_at(&self, off: usize) -> bool {
        self.frames
            .iter()
            .any(|f| f.start == off && self.waits.contains(&f.end))
    }

    /// Wait at each file with `attach`.
    pub fn on_attach(mut self, attach: SubmitAttach) -> Self {
        self.attach = Some(attach);
        self
    }

    /// Call `stuck` when a stopped paste cannot be completed.
    pub fn on_stuck(mut self, stuck: Box<dyn FnOnce() + Send>) -> Self {
        self.stuck = Some(stuck);
        self
    }

    fn finish(&mut self, r: Result<(), String>) {
        if let Some(done) = self.done.take() {
            done(r);
        }
    }

    /// Nothing written yet: the refusal `e`; else the outcome is unknown.
    fn stop(&mut self, e: String) {
        let r = if self.pasted {
            SUBMIT_UNCONFIRMED.to_string()
        } else {
            e
        };
        self.finish(Err(r));
    }
}

impl Drop for Submit {
    /// Never written to the end: the input thread stopped (the Terminal ended).
    fn drop(&mut self) {
        self.stop("terminal has exited".into());
    }
}

impl Submit {
    /// Where the next piece of the paste after its first `off` bytes ends: at most
    /// `max_piece` on, and never across the start or end of a bracketed paste. With
    /// `markers_alone`, never across the end of a start marker or the start of an end marker
    /// either, and never inside a UTF-8 character.
    fn piece_end(&self, off: usize) -> usize {
        let alone = self.markers_alone;
        if alone {
            // A marker (or what is left of one) is a piece of its own, whatever its length.
            if let Some(f) = self.frames.iter().find(|f| f.start <= off && off < f.end) {
                let (body, tail) = (f.start + PASTE_START.len(), f.end - PASTE_END.len());
                if off < body {
                    return body;
                }
                if off >= tail {
                    return f.end;
                }
            }
        }
        let end = self
            .frames
            .iter()
            .flat_map(|f| {
                let (body, tail) = (f.start + PASTE_START.len(), f.end - PASTE_END.len());
                if alone {
                    vec![f.start, body, tail, f.end]
                } else {
                    vec![f.start, f.end]
                }
            })
            .filter(|&b| b > off)
            .fold(
                (off + self.max_piece.max(1)).min(self.paste.len()),
                usize::min,
            );
        if !alone {
            return end;
        }
        // Back to the start of a character cut at `end`; on to its end when the piece is
        // shorter than the character.
        let inside = |i: usize| self.paste.get(i).is_some_and(|b| (b & 0xC0) == 0x80);
        let mut cut = end;
        while cut > off + 1 && inside(cut) {
            cut -= 1;
        }
        if inside(cut) {
            cut = end;
            while inside(cut) {
                cut += 1;
            }
        }
        cut
    }

    /// What completes the paste once its first `off` bytes were written: the bracketed
    /// paste `off` is inside of completed ([`paste_close`]); nothing between two.
    fn close_at(&self, off: usize) -> Vec<u8> {
        self.frames
            .iter()
            .find(|f| f.start < off && off < f.end)
            .map(|f| paste_close(&self.paste[f.clone()], off - f.start))
            .unwrap_or_default()
    }
}

/// The bracketed paste `text` is typed as.
fn bracketed(text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len() + 12);
    v.extend_from_slice(PASTE_START);
    v.extend_from_slice(text.as_bytes());
    v.extend_from_slice(PASTE_END);
    v
}

/// What a reply types before Enter: `bytes`, made of the bracketed pastes at `frames` and
/// what is typed between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Typing {
    bytes: Vec<u8>,
    frames: Vec<Range<usize>>,
    /// Where a file's paste ends: nothing more is typed until the agent shows that file
    /// attached.
    waits: Vec<usize>,
}

impl Typing {
    /// Each of `paths` (as [`typed_path`] gives them) as a bracketed paste of its own
    /// followed by a space, then `text` as one if there is one. Codex attaches an image
    /// only when a paste is exactly its path; Claude Code reads a pasted image path too, but
    /// asynchronously, and drops an Enter that comes while it reads: after each path's
    /// paste the reply waits until the image shows attached ([`SubmitAttach`]).
    pub fn reply(paths: &[String], text: Option<&str>) -> Self {
        let mut t = Typing {
            bytes: Vec::new(),
            frames: Vec::new(),
            waits: Vec::new(),
        };
        let paste = |t: &mut Typing, s: &str| {
            let start = t.bytes.len();
            t.bytes.extend(bracketed(s));
            t.frames.push(start..t.bytes.len());
        };
        for p in paths {
            paste(&mut t, p);
            t.waits.push(t.bytes.len());
            t.bytes.push(b' ');
        }
        if let Some(text) = text {
            paste(&mut t, text);
        }
        t
    }

    /// `text` alone.
    #[cfg(test)]
    pub fn text(text: &str) -> Self {
        Self::reply(&[], Some(text))
    }
}

/// How the path of a dropped file is typed: as it is when every character is one no agent
/// reads specially (letters, digits, `/._-+,@:%=~`); otherwise in double quotes with `"` and
/// `\` escaped by `\`. Claude Code strips the outer quotes and then the backslash escapes;
/// Codex reads a pasted path as one shell word. `None` for a path with a control character
/// (ESC, a newline…), which is never typed.
pub(crate) fn typed_path(path: &str) -> Option<String> {
    if path.chars().any(char::is_control) {
        return None;
    }
    let plain = |c: char| c.is_alphanumeric() || "/._-+,@:%=~".contains(c);
    if path.chars().all(plain) {
        return Some(path.to_string());
    }
    let mut q = String::with_capacity(path.len() + 2);
    q.push('"');
    for c in path.chars() {
        if c == '"' || c == '\\' {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    Some(q)
}

/// What completes `paste` (a bracketed paste) once its first `off` bytes were written:
/// the rest of its start marker, the rest of a character cut in two, and its end marker.
fn paste_close(paste: &[u8], off: usize) -> Vec<u8> {
    let end_at = paste.len() - PASTE_END.len();
    if off >= end_at {
        return paste[off..].to_vec();
    }
    let mut to = off.max(PASTE_START.len());
    while to < end_at && (paste[to] & 0xC0) == 0x80 {
        to += 1;
    }
    let mut v = paste[off..to].to_vec();
    v.extend_from_slice(PASTE_END);
    v
}

/// The PTY's input end, as the input thread writes it.
pub(crate) trait PtyIn: Write {
    /// Write what the PTY takes now, without blocking: `WouldBlock` when it takes nothing,
    /// `Unsupported` when it cannot write without blocking (a reply is then never written).
    fn write_now(&mut self, data: &[u8]) -> std::io::Result<usize>;
}

/// A Terminal's PTY writer. Unix: with its own descriptor of the PTY master for writes that
/// must not block (`O_NONBLOCK` is set only for that one write; the reader, which shares the
/// open file, retries a read that would block meanwhile). Windows: with its own handle of the
/// ConPTY's input pipe, set to `PIPE_NOWAIT` only for that one write.
struct PtyWriter {
    w: Box<dyn Write + Send>,
    #[cfg(unix)]
    fd: Option<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    pipe: Option<std::os::windows::io::OwnedHandle>,
}

impl Write for PtyWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.w.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

impl PtyIn for PtyWriter {
    fn write_now(&mut self, data: &[u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        if let Some(fd) = &self.fd {
            use std::os::fd::AsRawFd;
            return nonblocking_write(fd.as_raw_fd(), data);
        }
        #[cfg(windows)]
        if let Some(pipe) = &self.pipe {
            return win::nowait_write(pipe, data);
        }
        // Never a blocking write in its place: it could block holding the agent's state.
        let _ = data;
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

/// One `write(2)` to `fd` with `O_NONBLOCK` set for it.
#[cfg(unix)]
fn nonblocking_write(fd: std::os::fd::RawFd, data: &[u8]) -> std::io::Result<usize> {
    // SAFETY: `fd` is a descriptor this Terminal owns; `data` is valid for its length.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = libc::write(fd, data.as_ptr().cast(), data.len());
        let err = std::io::Error::last_os_error();
        libc::fcntl(fd, libc::F_SETFL, flags);
        if n < 0 {
            Err(err)
        } else {
            Ok(n as usize)
        }
    }
}

/// What one gated write of a reply came to.
enum Step {
    Wrote(usize),
    /// The PTY took nothing.
    Full,
    Refused(String),
}

fn gated(
    s: &mut Submit,
    step: SubmitStep,
    w: &mut dyn PtyIn,
    data: &[u8],
) -> std::io::Result<Step> {
    match (s.gate)(step, &mut || w.write_now(data)) {
        Err(e) => Ok(Step::Refused(e)),
        Ok(Ok(0)) => Ok(Step::Full),
        Ok(Ok(n)) => Ok(Step::Wrote(n)),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
            Ok(Step::Refused(SUBMIT_NO_NONBLOCK.into()))
        }
        Ok(Err(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(Step::Full)
        }
        Ok(Err(e)) => Err(e),
    }
}

/// Complete a paste stopped after its first `off` bytes ([`paste_close`]), under the gate's
/// locks but whatever the readiness, then report the unknown outcome. If the PTY takes
/// nothing of it for `stall`, the Terminal's replies are stuck.
fn close_paste(
    mut s: Box<Submit>,
    w: &mut dyn PtyIn,
    off: usize,
    sleep: &dyn Fn(Duration),
) -> std::io::Result<()> {
    let rest = s.close_at(off);
    let (mut done, mut waited) = (0, Duration::ZERO);
    while done < rest.len() {
        match gated(&mut s, SubmitStep::Close, w, &rest[done..])? {
            Step::Wrote(n) => {
                done += n;
                waited = Duration::ZERO;
            }
            Step::Full if waited < s.stall => {
                sleep(SUBMIT_WAIT);
                waited += SUBMIT_WAIT;
            }
            Step::Full | Step::Refused(_) => {
                if let Some(stuck) = s.stuck.take() {
                    stuck();
                }
                break;
            }
        }
    }
    s.finish(Err(SUBMIT_UNCONFIRMED.into()));
    Ok(())
}

/// Wait until the reply's `n`-th file shows attached: polled holding nothing, every
/// [`SUBMIT_WAIT`], for at most `attach_timeout`. `false` when it did not, or the reply must
/// stop.
fn wait_attached(s: &mut Submit, n: usize, sleep: &dyn Fn(Duration)) -> bool {
    let timeout = s.attach_timeout;
    let Some(attach) = s.attach.as_mut() else {
        return true;
    };
    let mut waited = Duration::ZERO;
    loop {
        match attach(n) {
            Ok(true) => return true,
            Ok(false) if waited < timeout => {
                sleep(SUBMIT_WAIT);
                waited += SUBMIT_WAIT;
            }
            Ok(false) | Err(_) => return false,
        }
    }
}

/// Write one input item. A [`Submit`] is the paste, in pieces, and `enter_after` later
/// (`sleep`) a separate `\r`: Enter must not arrive in the same read as the paste (the agent
/// would take it as part of it), nor after the agent stopped accepting the reply (it would
/// answer whatever took the composer's place). Each write runs in the gate, with readiness
/// checked and held, and never blocks: when the PTY takes nothing the thread waits holding
/// nothing and checks again. A paste stopped part way is completed ([`close_paste`]). An
/// error stops the input thread.
pub(crate) fn write_item(
    w: &mut dyn PtyIn,
    item: Input,
    sleep: &dyn Fn(Duration),
) -> std::io::Result<()> {
    let mut s = match item {
        Input::Bytes(data) => return w.write_all(&data).and_then(|_| w.flush()),
        Input::Submit(s) => s,
    };
    let (mut off, mut waited) = (0, Duration::ZERO);
    while off < s.paste.len() {
        let piece = s.paste[off..s.piece_end(off)].to_vec();
        let step = if s.file_starts_at(off) {
            SubmitStep::PasteFile
        } else {
            SubmitStep::Paste
        };
        match gated(&mut s, step, w, &piece)? {
            Step::Wrote(n) => {
                off += n;
                s.pasted = true;
                waited = Duration::ZERO;
                if let Some(k) = s.waits.iter().position(|&w| w == off) {
                    if !wait_attached(&mut s, k + 1, sleep) {
                        // At the end of a paste: nothing to complete, nothing more typed.
                        return close_paste(s, w, off, sleep);
                    }
                }
                continue;
            }
            Step::Full if waited < s.stall => {
                sleep(SUBMIT_WAIT);
                waited += SUBMIT_WAIT;
                continue;
            }
            Step::Refused(e) if off == 0 => s.stop(e),
            Step::Full if off == 0 => s.stop("input backlog full".into()),
            Step::Refused(_) | Step::Full => return close_paste(s, w, off, sleep),
        }
        return Ok(());
    }
    sleep(s.enter_after);
    let mut waited = Duration::ZERO;
    loop {
        match gated(&mut s, SubmitStep::Enter, w, b"\r")? {
            Step::Wrote(_) => break,
            Step::Full if waited < s.stall => {
                sleep(SUBMIT_WAIT);
                waited += SUBMIT_WAIT;
            }
            Step::Refused(_) | Step::Full => {
                s.finish(Err(SUBMIT_UNCONFIRMED.into()));
                return Ok(());
            }
        }
    }
    s.finish(Ok(()));
    Ok(())
}

pub(crate) struct Terminal {
    pub id: Uuid,
    record: Mutex<Record>,
    io: Mutex<TermIo>,
    out: Mutex<TermOutput>,
    input: Mutex<Option<SyncSender<Input>>>,
    /// The session leader; portable-pty runs it under `setsid`, so it is also the pgid.
    pid: Option<u32>,
    start_time: Option<u64>,
    life: Mutex<Life>,
    life_cv: Condvar,
    nudge_pending: AtomicBool,
    last_overflow_nudge: Mutex<Option<Instant>>,
    persist_pending: AtomicBool,
    /// The most a Mobile is replayed (`Config::mobile_replay_cap`).
    mobile_replay_cap: usize,
    /// For a Terminal restored without a process: the previous run's leader, kept in the
    /// state file so a later start retries ending it.
    kept_leader: Option<Leader>,
    /// This process's run: unique per Daemon start and process, so a hook of a process a
    /// Relaunch or restart replaced never reports for its successor.
    pub run: u64,
    /// The Agent Status of this run and when it changed. Locked last, never across another
    /// lock.
    status: Mutex<StatusCell>,
    /// The newest text message of the agent's session, as the last-line worker last read
    /// it. Locked last, never across another lock.
    last_line: Mutex<Option<LastLine>>,
    /// The session the agent reported itself (Codex's notify `thread-id`): linked, and still
    /// to be checked. Locked last, never across another lock.
    link: Mutex<LinkCell>,
    /// Claude and Codex run directly: a model of the screen, fed with the output and resized
    /// with the PTY. Locked alone, or after `io`; `prompt` and `input` may be taken under it.
    screen: Mutex<Option<Screen>>,
    /// The screen's revision: bumped under `screen` by every feed and resize.
    screen_rev: Arc<AtomicU64>,
    /// The Permission Prompt. Locked after the other Terminal locks (`status` and `last_line`
    /// aside), `screen` included; only `input` and `written` may be taken under it.
    prompt: Mutex<PromptCell>,
    /// Inputs queued for the PTY (counted under `input`) and written by the input thread.
    queued: AtomicU64,
    written: Arc<Written>,
    /// The Daemon's pending SIGKILLs, which [`Terminal::kill`] adds to.
    escalations: Arc<super::orphans::Escalations>,
    /// The input thread can write a reply without blocking (`term.submit` needs it).
    reply_writes: bool,
    /// A reply's stopped paste could not be completed: replies are refused from now on.
    reply_stuck: AtomicBool,
    /// Windows: the kill-on-close Job Object the process runs in, with everything it
    /// starts. Closing it (the Terminal and its escalation dropped) ends them all.
    #[cfg(windows)]
    job: Option<Arc<xshell_core::job::Job>>,
}

/// The session an agent reported for its Terminal, and a report the last-line worker has
/// still to check (see `codex_link`).
#[derive(Default)]
struct LinkCell {
    /// The session the agent reported and the Daemon checked and linked: a `term.update`
    /// naming another one is refused. Not persisted (the spec keeps the link).
    agent_session: Option<String>,
    /// The newest report not yet checked.
    pending: Option<PendingLink>,
    /// The last generation drawn: every report gets a new one.
    gen: u64,
}

/// A session report waiting to be checked: only the newest one (its generation still
/// current) may be linked, kept for a retry or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingLink {
    pub gen: u64,
    pub sid: String,
    /// When it was reported: a missing rollout is looked for again until a retry later.
    pub at: Instant,
}

/// A run's Agent Status and the time of its last change.
pub(crate) struct StatusCell {
    tracker: Tracker,
    /// When the status last changed (Unix ms); `None` before the first change.
    at_ms: Option<u64>,
    /// Every stamp is above this: the last stamp of the run this one replaced.
    floor: u64,
}

impl StatusCell {
    pub(crate) fn new(tracker: Tracker, floor: u64) -> Self {
        Self {
            tracker,
            at_ms: None,
            floor,
        }
    }

    /// Pass on a [`Tracker`] method's result; a change is stamped with `now`, or just after
    /// the previous stamp when the clock has not moved past it (equal or set back), so the
    /// stamps of one Terminal strictly increase.
    pub(crate) fn stamp(&mut self, changed: bool, now: u64) -> bool {
        if changed {
            self.at_ms = Some(now.max(self.last_stamp() + 1));
        }
        changed
    }

    /// Continue after `floor` (the replaced run's last stamp): a stamp made before, with a
    /// clock below it, moves to just after it.
    pub(crate) fn raise_floor(&mut self, floor: u64) {
        self.floor = self.floor.max(floor);
        if self.at_ms.is_some_and(|at| at <= floor) {
            self.at_ms = Some(floor + 1);
        }
    }

    /// The latest stamp this Terminal has had, the replaced run's included.
    pub(crate) fn last_stamp(&self) -> u64 {
        self.at_ms.unwrap_or(0).max(self.floor)
    }

    /// The status and its stamp, as listed.
    pub(crate) fn listed(&self) -> (Option<AgentStatus>, Option<u64>) {
        let status = self.tracker.status();
        (status, status.and(self.at_ms))
    }
}

/// Fixed per-entry cost in a `terminals` list on top of the spec and metadata (UUID, pid,
/// exit code, timestamps, keys).
const ENTRY_OVERHEAD: usize = 256;

/// Reserved per entry for the fields the Daemon fills in after admission, at their largest
/// serialized: `agentStatus`, `statusAtMs`, a `lastLine` of [`LAST_LINE_MAX_CHARS`]
/// four-byte characters (control characters never reach it; `"` and `\` escape to two bytes)
/// and a `permissionPrompt` of [`PROMPT_TEXT_MAX_CHARS`] and [`PROMPT_OPTIONS_MAX`] labels
/// of [`PROMPT_OPTION_MAX_CHARS`] such characters (about 5.9 KB). Counted in every entry's
/// budget, so filling them never grows a list past its limit.
///
/// [`LAST_LINE_MAX_CHARS`]: xshell_protocol::msg::LAST_LINE_MAX_CHARS
/// [`PROMPT_TEXT_MAX_CHARS`]: xshell_protocol::msg::PROMPT_TEXT_MAX_CHARS
/// [`PROMPT_OPTIONS_MAX`]: xshell_protocol::msg::PROMPT_OPTIONS_MAX
/// [`PROMPT_OPTION_MAX_CHARS`]: xshell_protocol::msg::PROMPT_OPTION_MAX_CHARS
pub const OPTIONAL_FIELDS_BYTES: usize = 7168;

/// The size a Terminal with this spec and metadata adds to a serialized `terminals` list.
pub(crate) fn entry_bytes(spec: &LaunchSpec, meta: &Map<String, Value>) -> usize {
    let len = |v: serde_json::Result<Vec<u8>>| v.map_or(usize::MAX / 4, |b| b.len());
    len(serde_json::to_vec(spec))
        + len(serde_json::to_vec(meta))
        + ENTRY_OVERHEAD
        + OPTIONAL_FIELDS_BYTES
}

fn short(id: &Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// Why [`spawn_with`] failed.
pub(crate) struct SpawnError {
    pub message: String,
    /// Set when the process started but some of its threads did not: it is being ended, and
    /// its exit is published on this Terminal (never on a listed one).
    pub started: Option<Arc<Terminal>>,
}

impl From<String> for SpawnError {
    fn from(message: String) -> Self {
        Self {
            message,
            started: None,
        }
    }
}

/// Start a Terminal. Unlike the Desktop, a cwd that is not a directory is an error: the PTY
/// library would silently fall back to `$HOME`, and a restore must fail visibly instead.
pub(crate) fn spawn(
    d: &Arc<Daemon>,
    id: Uuid,
    spec: LaunchSpec,
    meta: Map<String, Value>,
    cols: u16,
    rows: u16,
    created_at_ms: u64,
) -> Result<Arc<Terminal>, String> {
    spawn_with(d, id, spec, meta, (cols, rows), created_at_ms, None, |_| {}).map_err(|e| e.message)
}

/// [`spawn`], calling `spawned` with the new process's identity as soon as it exists, before
/// any of its threads start. `first` is a new chat's first message (`term.open` only): it
/// goes to this launch's argv and nowhere else, so a restore or Relaunch never sends it again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_with(
    d: &Arc<Daemon>,
    id: Uuid,
    spec: LaunchSpec,
    meta: Map<String, Value>,
    (cols, rows): (u16, u16),
    created_at_ms: u64,
    first: Option<&str>,
    spawned: impl FnOnce(&Leader),
) -> Result<Arc<Terminal>, SpawnError> {
    if !spec.cwd.is_empty() && !Path::new(&spec.cwd).is_dir() {
        return Err(format!("working directory does not exist: {}", spec.cwd).into());
    }
    let run = d.next_run.fetch_add(1, Ordering::SeqCst);
    let hooks = d.hooks.as_ref().map(|hooks| TerminalHooks {
        hooks,
        terminal: id,
        run,
    });
    let plan = xshell_core::plan_command_first(&d.ctx, &spec, hooks, first)?;
    let tracker = Tracker::new(&spec);
    // Only an agent run directly draws its own dialogs on the whole screen; a wrapping shell
    // or launch prefix may print anything around it (and a Mobile never sees those).
    let screen_agent = tracker.agent().filter(|_| spec.is_direct_agent());
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("failed to open PTY: {e}"))?;
    // Windows: until the Terminal exists, a failure hands the console to a cleanup thread
    // that drains it (closing it may wait for that) and ends whatever started, so nothing
    // here blocks under the registry lock.
    #[cfg(windows)]
    let mut pair = win::SpawnGuard::new(pair, id);
    #[cfg(unix)]
    let cmd = plan.to_command_builder();
    #[cfg(windows)]
    let (job, cmd) = {
        let name = format!(
            r"Local\xshelld-term-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        );
        let job = xshell_core::job::Job::new_named(&name)
            .map_err(|e| format!("failed to create the terminal's job: {e}"))?;
        let job = Arc::new(job);
        pair.job = Some(job.clone());
        let launcher = d.cfg.job_launcher.as_deref();
        (job, win::command(&plan, launcher, &name))
    };
    // A first message runs the resolved executable by its exact path: only the launcher
    // starts it that way (portable-pty would search again, PATHEXT included), it must still
    // be there, and the whole command line, the launcher's words included, must fit
    // CreateProcess's limit.
    #[cfg(windows)]
    if first.is_some() {
        use xshell_core::direct_exec as de;
        if d.cfg.job_launcher.is_none() {
            return Err(de::NO_LAUNCHER.to_string().into());
        }
        if !de::runnable_file(Path::new(&plan.program)) {
            let agent = xshell_core::launch::agent_binary(spec.agent.as_deref());
            return Err(de::not_direct(agent).into());
        }
        if !de::fits_command_line(cmd.get_argv()) {
            return Err(de::TOO_LONG.to_string().into());
        }
    }
    #[cfg(unix)]
    let spawned_child = pair.slave.spawn_command(cmd);
    #[cfg(windows)]
    let spawned_child = pair.slave().spawn_command(cmd);
    let mut child = spawned_child.map_err(|e| format!("failed to start {}: {e}", plan.program))?;
    // Without the launcher (an in-process server), the job is assigned from here: a process
    // the child starts before that escapes it.
    #[cfg(windows)]
    if d.cfg.job_launcher.is_none() {
        let assigned = child
            .as_raw_handle()
            .ok_or_else(|| std::io::Error::other("no process handle"))
            .and_then(|h| job.assign(h));
        if let Err(e) = assigned {
            let _ = child.kill();
            return Err(format!("failed to put {} in its job: {e}", plan.program).into());
        }
    }
    let pid = child.process_id();
    let start_time = pid.and_then(|p| super::orphans::start_time(p as i32));
    if let Some(pid) = pid {
        spawned(&Leader {
            pid,
            start_time,
            groups: vec![ProcIdentity {
                pid: pid as i32,
                start_time,
            }],
        });
    }
    #[cfg(unix)]
    drop(pair.slave);
    #[cfg(windows)]
    pair.release_slave();
    #[cfg(unix)]
    let pty_master = &pair.master;
    #[cfg(windows)]
    let pty_master = pair.master();
    // The input pipe for replies' writes that must not block: taken before the writer, which
    // owns it afterwards.
    #[cfg(windows)]
    let pipe = if d.test_point(id, TestPoint::ReplyDescriptor) {
        None
    } else {
        win::input_pipe(pty_master)
    };
    let pty = pty_master
        .try_clone_reader()
        .map_err(|e| format!("failed to clone PTY reader: {e}"))
        .and_then(|r| {
            pty_master
                .take_writer()
                .map(|w| (r, w))
                .map_err(|e| format!("failed to take PTY writer: {e}"))
        });
    let (reader, writer) = pty?;
    let writer = PtyWriter {
        w: writer,
        // A descriptor of the master's open file, for replies' writes that must not block.
        #[cfg(unix)]
        fd: pty_master
            .as_raw_fd()
            .filter(|_| !d.test_point(id, TestPoint::ReplyDescriptor))
            .and_then(|fd| {
                // SAFETY: the master is open for as long as this borrow; the clone is owned.
                unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
                    .try_clone_to_owned()
                    .ok()
            }),
        #[cfg(windows)]
        pipe,
    };
    #[cfg(unix)]
    let reply_writes = writer.fd.is_some();
    #[cfg(windows)]
    let reply_writes = writer.pipe.is_some();
    #[cfg(windows)]
    let master = pair.into_master();
    #[cfg(unix)]
    let master = pair.master;
    let (tx, rx) = sync_channel::<Input>(INPUT_BACKLOG);
    let t = Arc::new(Terminal {
        id,
        record: Mutex::new(Record {
            spec,
            meta,
            created_at_ms,
            pending_skip: None,
            replacement: None,
        }),
        io: Mutex::new(TermIo {
            master: Some(master),
            arb: SizeArbiter::new(cols, rows),
        }),
        out: Mutex::new(TermOutput {
            replay: ReplayBuffer::new(d.cfg.replay_capacity),
            subs: HashMap::new(),
            exit_code: None,
        }),
        input: Mutex::new(Some(tx)),
        pid,
        start_time,
        life: Mutex::new(Life::default()),
        life_cv: Condvar::new(),
        nudge_pending: AtomicBool::new(false),
        last_overflow_nudge: Mutex::new(None),
        persist_pending: AtomicBool::new(false),
        mobile_replay_cap: d.cfg.mobile_replay_cap,
        kept_leader: None,
        run,
        screen: Mutex::new(screen_agent.map(|agent| Screen {
            model: ScreenModel::new(cols, rows),
            agent,
        })),
        screen_rev: Arc::new(AtomicU64::new(0)),
        prompt: Mutex::new(PromptCell::new(0)),
        queued: AtomicU64::new(0),
        written: Arc::new(Mutex::new((0, 0))),
        status: Mutex::new(StatusCell::new(tracker, 0)),
        last_line: Mutex::new(None),
        link: Mutex::new(LinkCell::default()),
        escalations: d.escalations.clone(),
        reply_writes,
        reply_stuck: AtomicBool::new(false),
        #[cfg(windows)]
        job: Some(job),
    });
    let tag = short(&id);

    let (tr, dr) = (t.clone(), d.clone());
    let reader_thread = std::thread::Builder::new()
        .name(format!("pty-read-{tag}"))
        .spawn(move || tr.read_loop(&dr, reader));

    let (tw, dw) = (t.clone(), d.clone());
    let waiter = std::thread::Builder::new()
        .name(format!("pty-wait-{tag}"))
        .spawn(move || {
            let code = match child.wait() {
                Ok(st) => st.exit_code() as i32,
                Err(e) => {
                    crate::log!("WARN", "wait for terminal {} failed: {e}", tw.id);
                    -1
                }
            };
            // A ConPTY's output never ends while the console lives: close it now, while the
            // reader drains what is left. Whatever the process left in its job (a detached
            // descendant) ends after the grace, listed Terminal or not.
            #[cfg(windows)]
            {
                if let Some(job) = &tw.job {
                    tw.kill_after(vec![job.clone()], dw.cfg.kill_grace);
                }
                let master = tw.io.lock().unwrap().master.take();
                drop(master);
            }
            // `term.exit` follows the last output: wait for the reader to see EOF. Windows:
            // bounded, in case the closed console never ends its output.
            #[cfg(windows)]
            let drained_by = Some(Instant::now() + win::DRAIN_WAIT);
            #[cfg(not(windows))]
            let drained_by: Option<Instant> = None;
            let mut l = tw.life.lock().unwrap();
            while !l.reader_done {
                match drained_by {
                    None => l = tw.life_cv.wait(l).unwrap(),
                    Some(by) => {
                        let left = by.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            crate::log!("WARN", "output of terminal {} never ended", tw.id);
                            break;
                        }
                        l = tw.life_cv.wait_timeout(l, left).unwrap().0;
                    }
                }
            }
            drop(l);
            tw.publish_exit(&dw, code);
        });

    let input_thread = if d.test_point(id, TestPoint::StartInput) {
        Err(std::io::Error::other("refused by a test hook"))
    } else {
        std::thread::Builder::new()
            .name(format!("pty-in-{tag}"))
            .spawn({
                let (written, rev) = (t.written.clone(), t.screen_rev.clone());
                move || {
                    let mut writer = writer;
                    for item in rx {
                        if write_item(&mut writer, item, &std::thread::sleep).is_err() {
                            break;
                        }
                        let mut w = written.lock().unwrap();
                        w.0 += 1;
                        w.1 = rev.load(Ordering::SeqCst);
                    }
                }
            })
    };
    if let Some(e) = [reader_thread.err(), waiter.err(), input_thread.err()]
        .into_iter()
        .flatten()
        .next()
    {
        t.kill(d.cfg.kill_grace);
        return Err(SpawnError {
            message: format!("failed to start terminal threads: {e}"),
            started: Some(t),
        });
    }
    Ok(t)
}

impl Terminal {
    fn read_loop(&self, d: &Arc<Daemon>, mut reader: Box<dyn Read + Send>) {
        let mut buf = vec![0u8; READ_BUF];
        #[cfg(windows)]
        let mut startup = win::StartupQuery::default();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                #[cfg(windows)]
                Ok(n) => {
                    let (out, answer) = startup.feed(&buf[..n]);
                    if answer {
                        if let Some(tx) = self.input.lock().unwrap().as_ref() {
                            let reply = Input::Bytes(win::CURSOR_AT_ORIGIN.to_vec());
                            if tx.try_send(reply).is_ok() {
                                self.queued.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                    if !out.is_empty() {
                        self.on_output(d, &out);
                    }
                }
                #[cfg(not(windows))]
                Ok(n) => self.on_output(d, &buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // A reply's write set `O_NONBLOCK` on the shared open file for a moment.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                // EIO once every slave fd is closed.
                Err(_) => break,
            }
        }
        self.life.lock().unwrap().reader_done = true;
        self.life_cv.notify_all();
    }

    fn on_output(&self, d: &Arc<Daemon>, bytes: &[u8]) {
        let chunk: Arc<[u8]> = Arc::from(bytes);
        let mut dropped = Vec::new();
        {
            let mut o = self.out.lock().unwrap();
            o.replay.push(bytes);
            for ob in o.subs.values() {
                dropped.extend(overflowed(ob, ob.push_output(self.id, chunk.clone())));
            }
        }
        d.nudge_overflowed(dropped);
        self.feed_screen(d, bytes);
        let changed = {
            let mut c = self.status.lock().unwrap();
            let changed = c.tracker.on_output(bytes);
            c.stamp(changed, now_ms()).then(|| c.listed())
        };
        if let Some((status, stamp)) = changed {
            self.follow_status(d, status, stamp);
            super::agent::changed(d, self);
            // Codex's OSC 9 notification: the agent itself says it needs you.
            d.push.notify(self.id, self.run, AgentStatus::NeedsYou);
        }
    }

    /// Feed the screen model. A listed prompt the screen no longer shows becomes
    /// unanswerable at once, before anything else sees the new screen; the worker reads the
    /// screen once it settles.
    fn feed_screen(&self, d: &Daemon, bytes: &[u8]) {
        let mut g = self.screen.lock().unwrap();
        let Some(s) = g.as_mut() else {
            return;
        };
        s.model.feed(bytes);
        self.screen_rev.fetch_add(1, Ordering::SeqCst);
        let mut p = self.prompt.lock().unwrap();
        if p.watching().is_some() {
            p.observe(s.read().0.map(|f| f.fingerprint));
        }
        let interested = p.interested();
        drop(p);
        drop(g);
        if interested {
            d.prompts.request(self.id);
        }
    }

    /// The Agent Status changed to `status` (stamped `stamp`): a needs-you starts a prompt
    /// episode; anything else ends it and unlists the prompt, in the list that carries the
    /// new status.
    fn follow_status(&self, d: &Daemon, status: Option<AgentStatus>, stamp: Option<u64>) {
        if self.screen.lock().unwrap().is_none() {
            return;
        }
        let mut p = self.prompt.lock().unwrap();
        match status {
            Some(AgentStatus::NeedsYou) => {
                let now = Instant::now();
                p.needs_you(stamp.unwrap_or(0), now);
                drop(p);
                d.prompts.request(self.id);
                d.prompts.at(self.id, now + d.cfg.prompt_grace);
            }
            Some(AgentStatus::Ended) => {
                p.end();
            }
            _ => {
                p.left_needs_you();
            }
        }
    }

    /// The prompt worker's read: snapshot the screen, extract without a lock, then commit
    /// unless the prompt changed meanwhile (an answer, typing, a status change: the result
    /// is dropped, and whatever changed it asked for its own read). The commit holds the
    /// screen: a screen that changed since the snapshot is read again, and none changes
    /// before the commit is done.
    pub fn read_prompt(&self, d: &Daemon, recheck: bool) -> Outcome {
        let gen = self.prompt.lock().unwrap().gen();
        let snap = {
            let g = self.screen.lock().unwrap();
            let Some(s) = g.as_ref() else {
                return Outcome::default();
            };
            (
                s.model.rows(),
                self.screen_rev.load(Ordering::SeqCst),
                s.agent,
            )
        };
        let (rows, rev, agent) = snap;
        let found = extract(agent, &rows);
        d.test_point(self.id, super::TestPoint::PromptRead);
        let g = self.screen.lock().unwrap();
        let Some(s) = g.as_ref() else {
            return Outcome::default();
        };
        let mut p = self.prompt.lock().unwrap();
        if p.gen() != gen {
            return Outcome {
                recheck_again: recheck,
                ..Outcome::default()
            };
        }
        let now_rev = self.screen_rev.load(Ordering::SeqCst);
        let (found, rows, rev) = if now_rev != rev {
            let (f, r) = s.read();
            (f, r, now_rev)
        } else {
            (found, rows, rev)
        };
        // The writer's progress as one snapshot, then the inputs queued so far (never fewer
        // than it has written).
        let written = *self.written.lock().unwrap();
        let queued = self.queued.load(Ordering::SeqCst);
        let tail = || screen_tail(&rows);
        p.commit(&PromptRead {
            found: found.as_ref(),
            tail: &tail,
            rev,
            recheck,
            written,
            queued,
            now: Instant::now(),
            now_ms: d.cfg.prompt_clock.as_ref().map_or_else(now_ms, |c| (c.0)()),
            grace: d.cfg.prompt_grace,
            recheck_delay: d.cfg.prompt_recheck,
        })
    }

    /// The prompt-id floor `floor` was persisted (`ok`) or could not be: list the prompt
    /// waiting for it, or drop it. `true` when the listed prompt changed.
    pub fn confirm_prompt(&self, floor: u64, ok: bool) -> bool {
        self.prompt.lock().unwrap().confirm(floor, ok)
    }

    /// Answer the listed prompt `id` with `option`: type that option's key, without taking
    /// the size. Refused with [`PROMPT_ANSWERED`] unless `id` is listed and the screen still
    /// shows it, with `PROMPT_NO_OPTION` for an option it does not have. The screen is held
    /// from that check until the key is queued and the prompt spent: no output lands in
    /// between.
    pub fn answer(&self, d: &Daemon, id: u64, option: u32) -> Result<(), String> {
        let keys = {
            let g = self.screen.lock().unwrap();
            let mut p = self.prompt.lock().unwrap();
            let (keys, fp) = p.check_answer(id, option)?;
            let shows = g.as_ref().and_then(|s| s.read().0).map(|f| f.fingerprint) == Some(fp);
            if !shows {
                p.set_gone();
                drop(p);
                drop(g);
                d.prompts.request(self.id);
                return Err(PROMPT_ANSWERED.into());
            }
            d.test_point(self.id, super::TestPoint::AnswerChecked);
            let input = self.input.lock().unwrap();
            let tx = input.as_ref().ok_or("terminal has exited")?;
            match tx.try_send(Input::Bytes(keys.clone())) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => return Err("input backlog full".into()),
                Err(TrySendError::Disconnected(_)) => return Err("terminal has exited".into()),
            }
            let seq = self.queued.fetch_add(1, Ordering::SeqCst) + 1;
            p.spend(seq, self.screen_rev.load(Ordering::SeqCst), Instant::now());
            keys
        };
        // A Codex answer key also says the turn goes on.
        self.note_input(d, &keys);
        self.publish_prompt(d);
        d.prompts.recheck(self.id);
        Ok(())
    }

    /// Whether a reply may be typed now: the agent does not need you (no needs-you status,
    /// no Permission Prompt current or on the screen), and its screen ends with its chat
    /// composer with bracketed paste on. The caller holds `screen`, `prompt` and `status`.
    fn reply_ready(screen: &Option<Screen>, p: &PromptCell, c: &StatusCell) -> Result<(), String> {
        if p.is_current() || c.tracker.status() == Some(AgentStatus::NeedsYou) {
            return Err(SUBMIT_NEEDS_YOU.into());
        }
        // An agent not run directly has no screen model: its composer cannot be seen.
        let s = screen.as_ref().ok_or(SUBMIT_NOT_READY)?;
        let rows = s.model.rows();
        if extract(s.agent, &rows).is_some() {
            return Err(SUBMIT_NEEDS_YOU.into());
        }
        if !s.model.bracketed_paste() || !composer(s.agent, &rows) {
            return Err(SUBMIT_NOT_READY.into());
        }
        Ok(())
    }

    /// Whether the process may get a reply: not ended or ending, not being relaunched.
    fn reply_life(&self) -> Result<(), String> {
        let l = self.life.lock().unwrap();
        if l.exited.is_some() || l.closing {
            Err("terminal has exited".into())
        } else if l.relaunching {
            Err(SUBMIT_NOT_READY.into())
        } else {
            Ok(())
        }
    }

    /// Whether the agent's composer shows at least `want` attached images (a reply's file
    /// was taken), with the screen, prompt and status held. `Err` when the reply must stop:
    /// the agent needs you or shows a prompt, or the Terminal ended.
    fn images_attached(&self, base: usize) -> Result<bool, String> {
        self.reply_life()?;
        let g = self.screen.lock().unwrap();
        let p = self.prompt.lock().unwrap();
        let c = self.status.lock().unwrap();
        if p.is_current() || c.tracker.status() == Some(AgentStatus::NeedsYou) {
            return Err(SUBMIT_NEEDS_YOU.into());
        }
        let Some(s) = g.as_ref() else {
            return Ok(false);
        };
        let rows = s.model.rows();
        if extract(s.agent, &rows).is_some() {
            return Err(SUBMIT_NEEDS_YOU.into());
        }
        match composer_images(s.agent, &rows) {
            // An image went away (the composer was cleared or edited): the file's chip
            // cannot be told apart any more.
            Some(n) if n < base => Err(SUBMIT_NOT_READY.into()),
            Some(n) => Ok(n > base),
            None => Ok(false),
        }
    }

    /// [`Self::reply_ready`], after [`Self::reply_life`].
    pub fn check_reply(&self) -> Result<(), String> {
        self.reply_life()?;
        let g = self.screen.lock().unwrap();
        let p = self.prompt.lock().unwrap();
        let c = self.status.lock().unwrap();
        Self::reply_ready(&g, &p, &c)
    }

    /// One write of a reply (the input thread's [`write_item`]): check readiness and, if the
    /// agent is ready, `write`, holding `screen`, `prompt` and `status` throughout. Output,
    /// a hook's report or a prompt the check did not see therefore lands only after the
    /// write (the write never blocks). Enter, once written, starts the turn it starts for a
    /// Desktop's Enter (Codex), under the same locks: a report that comes after it is applied
    /// after it, never overwritten by it.
    ///
    /// For a file's paste ([`SubmitStep::PasteFile`]), `images` gets the count of images
    /// attached in the composer as the check saw it, right before the write.
    fn write_if_ready(
        &self,
        d: &Daemon,
        step: SubmitStep,
        write: &mut WriteNow<'_>,
        images: &AtomicUsize,
    ) -> Result<std::io::Result<usize>, String> {
        // Completing a stopped paste is written whatever the readiness: its end marker is
        // what keeps the next key out of the paste.
        let check = step != SubmitStep::Close;
        if check {
            self.reply_life()?;
        }
        let g = self.screen.lock().unwrap();
        let mut p = self.prompt.lock().unwrap();
        let mut c = self.status.lock().unwrap();
        if check {
            Self::reply_ready(&g, &p, &c)?;
            d.test_point(self.id, TestPoint::SubmitChecked);
        }
        if step == SubmitStep::PasteFile {
            let n = g
                .as_ref()
                .and_then(|s| composer_images(s.agent, &s.model.rows()))
                .unwrap_or(0);
            images.store(n, Ordering::SeqCst);
        }
        let r = write();
        let mut changed = None;
        if step == SubmitStep::Enter && matches!(r, Ok(n) if n > 0) {
            let ch = c.tracker.on_input(b"\r");
            if c.stamp(ch, now_ms()) {
                changed = Some(c.listed().0);
            }
        }
        drop(c);
        // As `follow_status` does, with the prompt still held.
        match changed {
            Some(Some(AgentStatus::Ended)) => {
                p.end();
            }
            Some(Some(AgentStatus::NeedsYou)) | None => {}
            Some(_) => {
                p.left_needs_you();
            }
        }
        drop(p);
        drop(g);
        if changed.is_some() {
            super::agent::changed(d, self);
        }
        Ok(r)
    }

    /// Reply `typing` (a checked text, and the checked paths of dropped files, as
    /// [`Typing::reply`] makes them) to the agent's chat (`term.submit`): queue its bracketed
    /// pastes and, after `Config::submit_enter_delay`, Enter, as one item. It never touches
    /// the size. Refused now (`Err`,
    /// `done` never called) unless this is a direct Claude Code or Codex chat whose reply
    /// is ready ([`Self::reply_ready`], checked with the screen held until it is queued);
    /// otherwise `done` gets the outcome from the input thread, which writes each piece of
    /// the paste and Enter only with readiness checked and held ([`Self::write_if_ready`]).
    pub fn submit(
        self: &Arc<Self>,
        d: &Arc<Daemon>,
        typing: Typing,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) -> Result<(), String> {
        if !d.cfg.submit {
            return Err(SUBMIT_UNSUPPORTED.into());
        }
        let agent = self.status.lock().unwrap().tracker.agent();
        if agent.is_none() || xshell_core::chat::chat_agent(&self.spec()).is_none() {
            return Err(SUBMIT_NOT_CHAT.into());
        }
        self.reply_life()?;
        if !self.reply_writes {
            return Err(SUBMIT_NO_NONBLOCK.into());
        }
        if self.reply_stuck.load(Ordering::SeqCst) {
            return Err(SUBMIT_STUCK.into());
        }
        let id = self.id;
        // The images in the composer right before the current file's paste (delivery, not
        // admission: input queued ahead of this reply may change the composer).
        let images = Arc::new(AtomicUsize::new(0));
        let (weak, dg, base) = (Arc::downgrade(self), d.clone(), images.clone());
        // A probe's refusal once this many bytes of the paste were written.
        let (stop_after, mut sent) = (d.cfg.submit_probe.stop_after, 0usize);
        let gate: SubmitGate = Box::new(move |step, write| {
            dg.test_point(
                id,
                match step {
                    SubmitStep::Paste | SubmitStep::PasteFile => TestPoint::SubmitPaste,
                    SubmitStep::Enter => TestPoint::SubmitEnter,
                    SubmitStep::Close => TestPoint::SubmitClose,
                },
            );
            if step != SubmitStep::Close && stop_after.is_some_and(|k| sent >= k) {
                return Err(SUBMIT_NOT_READY.into());
            }
            let r = weak
                .upgrade()
                .ok_or_else(|| "terminal has exited".to_string())?
                .write_if_ready(&dg, step, write, &base);
            if let Ok(Ok(n)) = &r {
                sent += n;
            }
            r
        });
        let dd = d.clone();
        let done = Box::new(move |r: Result<(), String>| {
            dd.test_point(id, TestPoint::Submitted);
            done(r);
        });
        let g = self.screen.lock().unwrap();
        let p = self.prompt.lock().unwrap();
        {
            let c = self.status.lock().unwrap();
            Self::reply_ready(&g, &p, &c)?;
        }
        // Each file must add one image to the composer's count before its paste.
        let (weak, da) = (Arc::downgrade(self), d.clone());
        let attach: SubmitAttach = Box::new(move |_| {
            da.test_point(id, TestPoint::SubmitAttach);
            weak.upgrade()
                .ok_or_else(|| "terminal has exited".to_string())?
                .images_attached(images.load(Ordering::SeqCst))
        });
        let input = self.input.lock().unwrap();
        let tx = input.as_ref().ok_or("terminal has exited")?;
        // Made only now: once made, its outcome is reported.
        let weak = Arc::downgrade(self);
        let stuck = Box::new(move || {
            crate::log!(
                "WARN",
                "terminal {id}: a stopped reply's paste could not be completed; replies are refused"
            );
            if let Some(t) = weak.upgrade() {
                t.reply_stuck.store(true, Ordering::SeqCst);
            }
        });
        let mut item = Submit::new(typing, d.cfg.submit_enter_delay, gate, done)
            .on_stuck(stuck)
            .on_attach(attach);
        item.stall = d.cfg.submit_stall;
        item.attach_timeout = d.cfg.submit_attach_timeout;
        item.markers_alone = d.cfg.submit_markers_alone;
        if let Some(n) = d.cfg.submit_probe.max_piece {
            item.max_piece = n;
        }
        let refused = match tx.try_send(Input::Submit(Box::new(item))) {
            Ok(()) => None,
            Err(TrySendError::Full(i)) => Some((i, "input backlog full")),
            Err(TrySendError::Disconnected(i)) => Some((i, "terminal has exited")),
        };
        if let Some((item, e)) = refused {
            // Not queued: refused here, so its outcome is never reported.
            if let Input::Submit(mut s) = item {
                s.done = None;
            }
            return Err(e.into());
        }
        self.queued.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Publish the list after the prompt changed, if this Terminal is the one listed.
    fn publish_prompt(&self, d: &Daemon) {
        let reg = d.reg.lock().unwrap();
        let listed = reg
            .terminals
            .get(&self.id)
            .is_some_and(|c| std::ptr::eq(Arc::as_ptr(c), self));
        if listed && !reg.frozen {
            d.broadcast_terminals(&reg);
        }
    }

    /// Continue prompt ids above `floor`, persisted by the run before a restart.
    pub fn restore_prompt_floor(&self, floor: u64) {
        self.prompt.lock().unwrap().restored(floor);
    }

    /// Test hook: the screen model's revision.
    pub fn screen_rev(&self) -> u64 {
        self.screen_rev.load(Ordering::SeqCst)
    }

    /// Start this run's Agent Status afresh, as a running process would (tests only: a
    /// Terminal restored without a process has ended).
    #[cfg(test)]
    pub(crate) fn reset_status_for_test(&self) {
        let tracker = Tracker::new(&self.spec());
        *self.status.lock().unwrap() = StatusCell::new(tracker, 0);
    }

    /// The Agent Status of this run.
    pub fn agent_status(&self) -> Option<AgentStatus> {
        self.status.lock().unwrap().tracker.status()
    }

    /// A hook's report for process `run`. A change reaches the prompt before the list that
    /// carries it is published.
    pub fn on_agent_event(
        &self,
        d: &Daemon,
        run: u64,
        status: AgentStatus,
    ) -> Result<bool, String> {
        let changed = {
            let mut c = self.status.lock().unwrap();
            if c.tracker.agent().is_none() {
                return Err("not an agent terminal".into());
            }
            if run != self.run {
                return Err("stale run".into());
            }
            let changed = c.tracker.on_event(status)?;
            c.stamp(changed, now_ms()).then(|| c.listed())
        };
        if let Some((status, stamp)) = changed {
            self.follow_status(d, status, stamp);
        }
        Ok(changed.is_some())
    }

    /// Input just queued (or refused): it may answer a prompt or interrupt a turn.
    pub fn note_input(&self, d: &Daemon, data: &[u8]) {
        let changed = {
            let mut c = self.status.lock().unwrap();
            let changed = c.tracker.on_input(data);
            c.stamp(changed, now_ms()).then(|| c.listed())
        };
        if let Some((status, stamp)) = changed {
            self.follow_status(d, status, stamp);
            super::agent::changed(d, self);
        }
    }

    /// Runs once the process has exited and its output is drained. During a Relaunch the
    /// exit is held back: no `term.exit`, and the attached connections stay for the
    /// replacement.
    fn publish_exit(self: &Arc<Self>, d: &Arc<Daemon>, code: i32) {
        let mut reg = d.reg.lock().unwrap();
        let held = {
            let mut l = self.life.lock().unwrap();
            let hold = l.relaunching && !l.closing && !reg.frozen;
            if hold {
                l.exited = Some(code);
                l.held_exit = Some(code);
            }
            hold
        };
        if held {
            self.input.lock().unwrap().take();
            self.life_cv.notify_all();
        } else {
            self.finish_exit(d, &mut reg, code);
        }
        drop(reg);
        d.test_point(self.id, TestPoint::ExitHandled { pid: self.pid });
    }

    /// Mark the Terminal ended and tell every connection, unless the Daemon is upgrading or
    /// shutting down or this Terminal is not the one listed under its UUID.
    fn finish_exit(self: &Arc<Self>, d: &Arc<Daemon>, reg: &mut Registry, code: i32) {
        // Published by the `terminals` list below.
        {
            let mut c = self.status.lock().unwrap();
            let changed = c.tracker.on_exit();
            c.stamp(changed, now_ms());
        }
        self.prompt.lock().unwrap().end();
        {
            let mut o = self.out.lock().unwrap();
            o.exit_code = Some(code);
            o.subs.clear();
        }
        // Ends the input thread, which drops the PTY writer.
        self.input.lock().unwrap().take();
        let closing = {
            let mut l = self.life.lock().unwrap();
            l.exited = Some(code);
            l.closing
        };
        self.life_cv.notify_all();
        if reg.frozen {
            // Upgrade/shutdown: the Terminal stays in the state file and on the Desktops.
            return;
        }
        if !d.is_current(reg, self) {
            // A Relaunch replacement whose start failed: the listed Terminal is not this one.
            return;
        }
        if let Some(f) = frame(&ServerMsg::TermExit {
            terminal: self.id,
            code,
        }) {
            d.broadcast_about(reg, self.id, &self.spec(), f);
        }
        if closing {
            reg.terminals.remove(&self.id);
            d.touch_idle(reg);
        }
        // Also when it stays listed: the state file must stop naming its process, whose pid
        // may be reused from now on.
        d.persist(reg);
        d.broadcast_terminals(reg);
    }

    /// The current launch spec.
    pub fn spec(&self) -> LaunchSpec {
        self.record.lock().unwrap().spec.clone()
    }

    /// Whether this Terminal's current spec runs `agent`'s session `sid`
    /// (`LaunchSpec::agent_session`).
    pub fn runs_session(&self, agent: &str, sid: &str) -> bool {
        self.record.lock().unwrap().spec.agent_session() == Some((agent, sid))
    }

    /// How this Terminal holds its session now. Locks `life` alone.
    pub fn session_hold(&self) -> SessionHold {
        let unresolved = self.pid.is_none() && self.kept_leader.is_some();
        self.life.lock().unwrap().hold(unresolved)
    }

    /// When this Terminal was first opened (kept across Relaunches and restarts).
    pub fn created_at_ms(&self) -> u64 {
        self.record.lock().unwrap().created_at_ms
    }

    /// Whether a Relaunch may start now.
    pub fn check_relaunch(&self) -> Result<(), String> {
        self.life.lock().unwrap().check_relaunch()
    }

    /// Reserve the Terminal for a Relaunch to `skip`: from now on its exit is held back and
    /// the state file names the pending spec. Refused while it has exited, is closing or is
    /// already relaunching.
    pub fn begin_relaunch(&self, skip: bool) -> Result<(), String> {
        let mut r = self.record.lock().unwrap();
        let mut l = self.life.lock().unwrap();
        l.check_relaunch()?;
        l.relaunching = true;
        r.pending_skip = Some(skip);
        Ok(())
    }

    /// Undo [`Terminal::begin_relaunch`] after a failed Relaunch: publish an exit held back
    /// in the meantime, exactly once, and drop the pending spec from the state file.
    pub fn abort_relaunch(self: &Arc<Self>, d: &Arc<Daemon>, reg: &mut Registry) {
        self.record.lock().unwrap().pending_skip = None;
        let held = {
            let mut l = self.life.lock().unwrap();
            l.relaunching = false;
            l.held_exit.take()
        };
        match held {
            Some(code) => self.finish_exit(d, reg, code),
            None => d.persist(reg),
        }
    }

    /// Name (or stop naming) the process a Relaunch started to replace this one's.
    pub fn set_replacement(&self, leader: Option<Leader>) {
        self.record.lock().unwrap().replacement = leader;
    }

    /// What a Relaunch restarts with: spec, metadata, creation time and the current size.
    pub fn relaunch_parts(&self) -> (LaunchSpec, Map<String, Value>, u64, (u16, u16)) {
        let r = self.record.lock().unwrap();
        let size = self.io.lock().unwrap().arb.current();
        (r.spec.clone(), r.meta.clone(), r.created_at_ms, size)
    }

    /// Move the attached connections and the size arbiter to `next`, which replaces this
    /// Terminal under its UUID, and send them `next`'s replay: a reset, then whatever `next`
    /// printed so far (a Mobile: its tail). A size this Terminal took after `next` started
    /// is applied to `next`. Returns the overflows to recover.
    pub fn hand_over(&self, next: &Terminal) -> Vec<Overflow> {
        let (subs, arb) = {
            let mut io = self.io.lock().unwrap();
            let mut o = self.out.lock().unwrap();
            let (cols, rows) = io.arb.current();
            (
                std::mem::take(&mut o.subs),
                std::mem::replace(&mut io.arb, SizeArbiter::new(cols, rows)),
            )
        };
        // The replacement resumes the same session, and its stamps continue after this run's,
        // also one it made already (its reader runs before the hand-over).
        let floor = self.status.lock().unwrap().last_stamp();
        next.status.lock().unwrap().raise_floor(floor);
        // Prompt ids too: an answer to this run's prompt never matches the next run's.
        let (ids, reserved) = {
            let p = self.prompt.lock().unwrap();
            (p.high_water(), p.reserved())
        };
        next.prompt.lock().unwrap().inherit(ids, reserved);
        let line = self.last_line.lock().unwrap().clone();
        *next.last_line.lock().unwrap() = line;
        // The replacement resumes the session the agent linked: a `term.update` naming
        // another one is still refused before the new run reports. A report still waiting
        // was the old run's and stays behind.
        let resumes = next.spec().session_id;
        let agent = self.agent_session().filter(|a| resumes.as_ref() == Some(a));
        next.link.lock().unwrap().agent_session = agent;
        let mut dropped = Vec::new();
        let mut io = next.io.lock().unwrap();
        let spawned = io.arb.current();
        io.arb = arb;
        let size = io.arb.current();
        let mut o = next.out.lock().unwrap();
        let (full, tail) = (
            o.replay.snapshot(),
            o.replay.snapshot_tail(next.mobile_replay_cap),
        );
        for (conn, ob) in subs {
            let snap = if ob.is_mobile() { &tail } else { &full };
            for piece in snap.chunks(REPLAY_CHUNK) {
                dropped.extend(overflowed(&ob, ob.push_replay(self.id, Arc::from(piece))));
            }
            o.subs.insert(conn, ob);
        }
        // A size taken while `next` was starting (it started at the size read then).
        if size != spawned && next.apply_size(&io, size).is_ok() {
            next.notify_size(&o, size);
        }
        dropped
    }

    pub fn is_exited(&self) -> bool {
        self.life.lock().unwrap().exited.is_some()
    }

    pub fn wait_exited(&self, deadline: Instant) -> bool {
        let mut l = self.life.lock().unwrap();
        while l.exited.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            l = self.life_cv.wait_timeout(l, left).unwrap().0;
        }
        true
    }

    /// Subscribe `conn`: queue the reply (`{exitCode, cols, rows}`), then the replay (a
    /// Mobile's is a shorter tail), atomically with respect to new output and size changes
    /// (both happen under the size and output locks). For an exited Terminal, `term.exit`
    /// follows the replay. Returns the overflows to recover, and whether no other connection
    /// is attached.
    pub fn attach(&self, conn: ConnId, ob: &Arc<Outbox>, id: Option<u64>) -> (Vec<Overflow>, bool) {
        let mut dropped = Vec::new();
        let io = self.io.lock().unwrap();
        let mut o = self.out.lock().unwrap();
        if let Some(id) = id {
            let (cols, rows) = io.arb.current();
            let res = json!({ "exitCode": o.exit_code, "cols": cols, "rows": rows });
            ob.push_about(self.id, Arc::from(encode_res(id, Ok(res))));
        }
        let snap = if ob.is_mobile() {
            o.replay.snapshot_tail(self.mobile_replay_cap)
        } else {
            o.replay.snapshot()
        };
        for piece in snap.chunks(REPLAY_CHUNK) {
            dropped.extend(overflowed(ob, ob.push_replay(self.id, Arc::from(piece))));
        }
        let alone = o.subs.keys().all(|c| *c == conn);
        match o.exit_code {
            Some(code) => {
                if let Some(f) = frame(&ServerMsg::TermExit {
                    terminal: self.id,
                    code,
                }) {
                    ob.push_about(self.id, f);
                }
            }
            None => {
                o.subs.insert(conn, ob.clone());
            }
        }
        (dropped, alone)
    }

    /// Unsubscribe `conn` and forget its size. A Mobile's output still queued is dropped.
    /// Returns whether the size changed (an owning Mobile left; see [`SizeArbiter::forget`]).
    pub fn detach(&self, conn: ConnId) -> bool {
        let mut io = self.io.lock().unwrap();
        let mut o = self.out.lock().unwrap();
        if let Some(ob) = o.subs.remove(&conn) {
            if ob.is_mobile() {
                ob.cancel_output(self.id);
            }
        }
        match io.arb.forget(conn) {
            Some(sz) if self.apply_size(&io, sz).is_ok() => {
                self.notify_size(&o, sz);
                true
            }
            _ => false,
        }
    }

    /// Tell the attached Mobiles the size now applied.
    fn notify_size(&self, o: &TermOutput, (cols, rows): (u16, u16)) {
        let mut mobiles = o.subs.values().filter(|ob| ob.is_mobile()).peekable();
        if mobiles.peek().is_none() {
            return;
        }
        if let Some(f) = frame(&ServerMsg::TermSize {
            terminal: self.id,
            cols,
            rows,
        }) {
            for ob in mobiles {
                ob.push_size(self.id, f.clone());
            }
        }
    }

    /// Recover a Mobile connection's dropped output: the redraw nudge when nobody else is
    /// attached, else (a nudge would reflow everyone's screen) a fresh tail of the replay in
    /// place of what is still queued.
    pub fn recover_mobile_overflow(self: &Arc<Self>, ob: &Arc<Outbox>, delay: Duration) {
        let more = {
            let o = self.out.lock().unwrap();
            if !o.subs.values().any(|s| Arc::ptr_eq(s, ob)) {
                return;
            }
            if o.subs.len() > 1 {
                ob.drop_output(self.id);
                let tail = o.replay.snapshot_tail(self.mobile_replay_cap);
                let mut more = Vec::new();
                for piece in tail.chunks(REPLAY_CHUNK) {
                    more.extend(ob.push_output(self.id, Arc::from(piece)));
                }
                Some(more)
            } else {
                None
            }
        };
        match more {
            None => self.overflow_nudge(delay),
            Some(more) if !more.is_empty() => {
                crate::log!("WARN", "a Mobile's send queue overflowed again on recovery")
            }
            Some(_) => {}
        }
    }

    /// Resize the PTY (`io` locked), and the screen model with it.
    fn apply_size(&self, io: &TermIo, (cols, rows): (u16, u16)) -> Result<(), String> {
        if let Some(master) = &io.master {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| format!("resize failed: {e}"))?;
        }
        if let Some(s) = self.screen.lock().unwrap().as_mut() {
            s.model.resize(cols, rows);
            self.screen_rev.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    /// `conn` (a Mobile when `mobile`) resized its view; see [`SizeArbiter::on_resize`].
    /// Returns whether the PTY size changed.
    pub fn resize(&self, conn: ConnId, cols: u16, rows: u16, mobile: bool) -> Result<bool, String> {
        let mut io = self.io.lock().unwrap();
        match io.arb.on_resize(conn, cols, rows, mobile) {
            Some(sz) => {
                self.apply_size(&io, sz)?;
                self.notify_size(&self.out.lock().unwrap(), sz);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// `conn` (a Mobile when `mobile`) typed: it takes over the size. Input that
    /// `may_answer` the listed prompt makes it stale, atomically with queueing it, so an
    /// answer racing it either goes first or is refused. Returns whether the PTY size changed.
    pub fn write_input(
        &self,
        d: &Daemon,
        conn: ConnId,
        data: String,
        mobile: bool,
        may_answer: bool,
    ) -> Result<bool, String> {
        let changed = {
            let mut io = self.io.lock().unwrap();
            match io.arb.on_input(conn, mobile) {
                Some(sz) if self.apply_size(&io, sz).is_ok() => {
                    self.notify_size(&self.out.lock().unwrap(), sz);
                    true
                }
                _ => false,
            }
        };
        let spent = {
            let mut p = self.prompt.lock().unwrap();
            let g = self.input.lock().unwrap();
            let tx = g.as_ref().ok_or("terminal has exited")?;
            match tx.try_send(Input::Bytes(data.into_bytes())) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => return Err("input backlog full".into()),
                Err(TrySendError::Disconnected(_)) => return Err("terminal has exited".into()),
            }
            let seq = self.queued.fetch_add(1, Ordering::SeqCst) + 1;
            may_answer && p.spend(seq, self.screen_rev.load(Ordering::SeqCst), Instant::now())
        };
        if spent {
            self.publish_prompt(d);
            d.prompts.recheck(self.id);
        }
        Ok(changed)
    }

    /// Force full-screen TUIs to repaint: shrink by a row, then restore the current size.
    /// The gap keeps the two SIGWINCHs from merging into one.
    pub fn nudge(self: &Arc<Self>, delay: Duration) {
        if self.is_exited() || self.nudge_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let t = self.clone();
        let r = std::thread::Builder::new()
            .name(format!("pty-nudge-{}", short(&self.id)))
            .spawn(move || {
                {
                    let io = t.io.lock().unwrap();
                    let (cols, rows) = io.arb.current();
                    if rows > 1 {
                        let _ = t.apply_size(&io, (cols, rows - 1));
                    }
                }
                std::thread::sleep(delay);
                {
                    // The current size, so a real resize during the gap wins.
                    let io = t.io.lock().unwrap();
                    let _ = t.apply_size(&io, io.arb.current());
                }
                t.nudge_pending.store(false, Ordering::SeqCst);
            });
        if r.is_err() {
            self.nudge_pending.store(false, Ordering::SeqCst);
        }
    }

    pub fn overflow_nudge(self: &Arc<Self>, delay: Duration) {
        {
            let mut last = self.last_overflow_nudge.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < OVERFLOW_NUDGE_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        self.nudge(delay);
    }

    /// Persist the size after it settles, at most once per `delay`.
    pub fn schedule_persist(self: &Arc<Self>, d: &Arc<Daemon>) {
        if self.persist_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let (t, d) = (self.clone(), d.clone());
        let r = std::thread::Builder::new()
            .name(format!("persist-{}", short(&self.id)))
            .spawn(move || {
                std::thread::sleep(d.cfg.resize_persist_delay);
                t.persist_pending.store(false, Ordering::SeqCst);
                let reg = d.reg.lock().unwrap();
                if reg.terminals.contains_key(&t.id) {
                    d.persist(&reg);
                }
            });
        if r.is_err() {
            self.persist_pending.store(false, Ordering::SeqCst);
        }
    }

    /// End the Terminal: [`Terminal::signal_groups`], marked as closing so that its exit
    /// removes it from the list.
    pub fn kill(self: &Arc<Self>, grace: Duration) {
        self.hang_up(grace, true);
    }

    /// End the process: SIGHUP the session leader's group and the foreground job's group
    /// (job control puts an agent under `bash -i` in its own group), then SIGKILL each group
    /// that still exists after `grace`. A Terminal that already exited is never signalled.
    pub fn signal_groups(self: &Arc<Self>, grace: Duration) {
        self.hang_up(grace, false);
    }

    /// The process groups ending the Terminal signals: the session leader's and the
    /// foreground job's.
    #[cfg(unix)]
    fn groups(&self) -> Vec<i32> {
        let mut groups: Vec<i32> = self.pid.map(|p| p as i32).into_iter().collect();
        let fg = self
            .io
            .lock()
            .unwrap()
            .master
            .as_ref()
            .and_then(|m| m.process_group_leader());
        if let Some(g) = fg {
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
        groups
    }

    /// What ending the Terminal kills after the grace: its process groups (Windows: its job).
    #[cfg(unix)]
    fn kill_groups(&self) -> Vec<super::orphans::Group> {
        self.groups()
    }

    #[cfg(windows)]
    fn kill_groups(&self) -> Vec<super::orphans::Group> {
        self.job.iter().cloned().collect()
    }

    /// [`Terminal::kill`], then wait until the process has exited and every group it
    /// signalled is gone, or until `deadline`. Answers whether they are gone.
    pub fn end_and_wait(self: &Arc<Self>, grace: Duration, deadline: Instant) -> bool {
        let groups = self.kill_groups();
        self.kill(grace);
        if !self.wait_exited(deadline) {
            return false;
        }
        loop {
            if !groups.iter().any(super::orphans::group_alive) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn hang_up(self: &Arc<Self>, grace: Duration, closing: bool) {
        let groups = self.groups();
        {
            let mut l = self.life.lock().unwrap();
            if l.exited.is_some() {
                return;
            }
            l.closing |= closing;
            for &g in &groups {
                unsafe { libc::killpg(g, libc::SIGHUP) };
            }
        }
        self.kill_after(groups, grace);
    }

    /// Windows: closing the ConPTY is the hangup (every process attached to it gets
    /// `CTRL_CLOSE_EVENT`); after `grace` the Terminal's job ends whatever is left of its
    /// tree, detached processes included.
    #[cfg(windows)]
    fn hang_up(self: &Arc<Self>, grace: Duration, closing: bool) {
        {
            let mut l = self.life.lock().unwrap();
            if l.exited.is_some() {
                return;
            }
            l.closing |= closing;
        }
        // Closing may wait for the console's output to drain: never under a lock.
        let master = self.io.lock().unwrap().master.take();
        if let Some(m) = master {
            win::close_console(m, &self.id);
        }
        if let Some(job) = &self.job {
            self.kill_after(vec![job.clone()], grace);
        }
    }

    /// Kill what is left of `groups` after `grace`. Recorded until the timer ran, so an exit
    /// in between, or the Daemon's own exit, still kills them.
    fn kill_after(&self, groups: Vec<super::orphans::Group>, grace: Duration) {
        let esc = self.escalations.clone();
        let entry = esc.add(groups.clone(), Instant::now() + grace);
        let r = std::thread::Builder::new()
            .name(format!("pty-kill-{}", short(&self.id)))
            .spawn(move || {
                std::thread::sleep(grace);
                super::orphans::kill_remaining(&groups);
                esc.done(entry);
            });
        if let Err(e) = r {
            crate::log!("ERROR", "cannot start kill timer for {}: {e}", self.id);
        }
    }

    /// The spec and metadata a `term.update` would produce, without applying it.
    pub fn updated(
        &self,
        session_id: Option<String>,
        meta: Option<Map<String, Value>>,
    ) -> (LaunchSpec, Map<String, Value>) {
        let r = self.record.lock().unwrap();
        let (mut spec, mut m) = (r.spec.clone(), r.meta.clone());
        if let Some(s) = session_id {
            spec.session_id = Some(s);
        }
        for (k, v) in meta.into_iter().flatten() {
            if v.is_null() {
                m.remove(&k);
            } else {
                m.insert(k, v);
            }
        }
        (spec, m)
    }

    pub fn set_record(&self, spec: LaunchSpec, meta: Map<String, Value>) {
        let mut r = self.record.lock().unwrap();
        r.spec = spec;
        r.meta = meta;
    }

    /// The agent reported session `sid`: it waits for the last-line worker to check it,
    /// replacing any earlier report still waiting.
    pub fn request_link(&self, sid: String) {
        let mut l = self.link.lock().unwrap();
        l.gen += 1;
        l.pending = Some(PendingLink {
            gen: l.gen,
            sid,
            at: Instant::now(),
        });
    }

    /// The session report waiting to be checked.
    pub fn pending_link(&self) -> Option<PendingLink> {
        self.link.lock().unwrap().pending.clone()
    }

    /// Drop report `gen`, if it is still the one waiting; `true` when it was.
    pub fn drop_link(&self, gen: u64) -> bool {
        let mut l = self.link.lock().unwrap();
        let current = l.pending.as_ref().is_some_and(|p| p.gen == gen);
        if current {
            l.pending = None;
        }
        current
    }

    /// Report `gen` was checked and its session is linked: it becomes the agent's session.
    /// Call under the registry lock that linked it; `false` (nothing changes) when a newer
    /// report replaced it.
    pub fn commit_link(&self, gen: u64) -> bool {
        let mut l = self.link.lock().unwrap();
        match l.pending.take_if(|p| p.gen == gen) {
            Some(p) => {
                l.agent_session = Some(p.sid);
                true
            }
            None => false,
        }
    }

    /// The session the agent reported and the Daemon linked, if any.
    pub fn agent_session(&self) -> Option<String> {
        self.link.lock().unwrap().agent_session.clone()
    }

    /// Store the last line the worker read; `true` when it changed.
    pub fn set_last_line(&self, line: Option<LastLine>) -> bool {
        let mut l = self.last_line.lock().unwrap();
        if *l == line {
            return false;
        }
        *l = line;
        true
    }

    /// Connections attached to the output.
    pub fn attached(&self) -> usize {
        self.out.lock().unwrap().subs.len()
    }

    /// Connections the size arbiter still remembers.
    pub fn size_tracked(&self) -> usize {
        self.io.lock().unwrap().arb.tracked()
    }

    /// This Terminal's share of the `terminals` list budget.
    pub fn entry_bytes(&self) -> usize {
        let r = self.record.lock().unwrap();
        entry_bytes(&r.spec, &r.meta)
    }

    pub fn info(&self) -> TerminalInfo {
        let r = self.record.lock().unwrap();
        let exit_code = self.out.lock().unwrap().exit_code;
        let (agent_status, status_at_ms) = self.status.lock().unwrap().listed();
        TerminalInfo {
            terminal: self.id,
            spec: r.spec.clone(),
            meta: r.meta.clone(),
            created_at_ms: r.created_at_ms,
            pid: self.pid,
            exit_code,
            agent_status,
            status_at_ms,
            last_line: self.last_line.lock().unwrap().clone(),
            permission_prompt: self.prompt.lock().unwrap().listed(),
        }
    }

    pub fn persisted(&self) -> PersistedTerminal {
        let r = self.record.lock().unwrap();
        let io = self.io.lock().unwrap();
        let (cols, rows) = io.arb.current();
        // An exited Terminal has no process left to end; its pid may already be reused.
        let pid = self.pid.filter(|_| !self.is_exited());
        let leader = pid.map(|pid| {
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut groups = vec![ProcIdentity {
                pid: pid as i32,
                start_time: self.start_time,
            }];
            #[cfg(unix)]
            if let Some(g) = io.master.as_ref().and_then(|m| m.process_group_leader()) {
                if g != pid as i32 {
                    groups.push(super::orphans::identity(g));
                }
            }
            Leader {
                pid,
                start_time: self.start_time,
                groups,
            }
        });
        let spec = r
            .pending_skip
            .and_then(|skip| relaunch_spec(&r.spec, skip).ok())
            .unwrap_or_else(|| r.spec.clone());
        PersistedTerminal {
            terminal: self.id,
            spec,
            meta: r.meta.clone(),
            cols,
            rows,
            created_at_ms: r.created_at_ms,
            leader: leader
                .or_else(|| r.replacement.clone())
                .or_else(|| self.kept_leader.clone()),
            prompt_id_floor: self.prompt.lock().unwrap().persist_floor(),
        }
    }
}

/// Exit code listed for a Terminal that was not relaunched because leftovers of its previous
/// run could not be ended or confirmed gone.
pub(crate) const UNRESOLVED_EXIT: i32 = -1;

/// A restored Terminal without a process: listed as exited (`UNRESOLVED_EXIT`), its record
/// and previous leader kept in the state file until `term.close` removes it.
pub(crate) fn unresolved(d: &Arc<Daemon>, p: PersistedTerminal) -> Arc<Terminal> {
    let mut status = StatusCell::new(Tracker::new(&p.spec), 0);
    let ended = status.tracker.on_exit();
    status.stamp(ended, now_ms());
    let t = Terminal {
        id: p.terminal,
        record: Mutex::new(Record {
            spec: p.spec,
            meta: p.meta,
            created_at_ms: p.created_at_ms,
            pending_skip: None,
            replacement: None,
        }),
        io: Mutex::new(TermIo {
            master: None,
            arb: SizeArbiter::new(p.cols, p.rows),
        }),
        out: Mutex::new(TermOutput {
            replay: ReplayBuffer::new(d.cfg.replay_capacity),
            subs: HashMap::new(),
            exit_code: Some(UNRESOLVED_EXIT),
        }),
        input: Mutex::new(None),
        pid: None,
        start_time: None,
        life: Mutex::new(Life {
            exited: Some(UNRESOLVED_EXIT),
            reader_done: true,
            ..Life::default()
        }),
        life_cv: Condvar::new(),
        nudge_pending: AtomicBool::new(false),
        last_overflow_nudge: Mutex::new(None),
        persist_pending: AtomicBool::new(false),
        mobile_replay_cap: d.cfg.mobile_replay_cap,
        kept_leader: p.leader,
        run: d.next_run.fetch_add(1, Ordering::SeqCst),
        screen: Mutex::new(None),
        screen_rev: Arc::new(AtomicU64::new(0)),
        prompt: Mutex::new(PromptCell::new(p.prompt_id_floor.unwrap_or(0))),
        queued: AtomicU64::new(0),
        written: Arc::new(Mutex::new((0, 0))),
        status: Mutex::new(status),
        last_line: Mutex::new(None),
        link: Mutex::new(LinkCell::default()),
        escalations: d.escalations.clone(),
        reply_writes: false,
        reply_stuck: AtomicBool::new(false),
        #[cfg(windows)]
        job: None,
    };
    Arc::new(t)
}

/// This Windows's build number (`RtlGetVersion`, which no compatibility shim changes).
#[cfg(windows)]
pub(crate) fn windows_build() -> Option<u32> {
    win::build()
}

#[cfg(windows)]
mod win {
    use portable_pty::{CommandBuilder, MasterPty, PtyPair, SlavePty};
    use std::io::{Read, Write};
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use std::sync::Arc;
    use xshell_core::job::Job;

    /// See [`super::windows_build`].
    pub(super) fn build() -> Option<u32> {
        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
        let mut v = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `v` is a valid OSVERSIONINFOW with its size set.
        (unsafe { RtlGetVersion(&mut v) } == 0).then_some(v.dwBuildNumber)
    }

    /// A handle of the ConPTY's input pipe of its own (`None` if it cannot have one), for
    /// [`nowait_write`]. Must be taken before the master's writer.
    pub(super) fn input_pipe(m: &(dyn MasterPty + Send)) -> Option<OwnedHandle> {
        let m: &dyn MasterPty = m;
        let con = m.downcast_ref::<portable_pty::win::conpty::ConPtyMasterPty>()?;
        match con.try_clone_input() {
            Ok(h) => Some(h),
            Err(e) => {
                crate::log!("WARN", "no handle of the console's input for replies: {e}");
                None
            }
        }
    }

    /// One `WriteFile` to the input pipe `h` with `PIPE_NOWAIT` set for it, as Unix sets
    /// `O_NONBLOCK` for one write: what the pipe takes now, `WouldBlock` when it takes
    /// nothing. The pipe is shared with the Terminal's (blocking) writer: if its mode cannot
    /// be put back, the write fails, and with it the input thread.
    pub(super) fn nowait_write(h: &OwnedHandle, data: &[u8]) -> std::io::Result<usize> {
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use windows_sys::Win32::System::Pipes::{SetNamedPipeHandleState, PIPE_NOWAIT, PIPE_WAIT};
        let raw = h.as_raw_handle();
        let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
        // Measured (Lesani/xshell#40, P5, builds 20348 and 26100): while the pipe holds
        // unread data, setting PIPE_NOWAIT fails with ERROR_PIPE_BUSY (the mode stays
        // PIPE_WAIT), and a write larger than the pipe takes nothing. The pipe takes nothing
        // now: WouldBlock.
        let busy = |e: std::io::Error| {
            if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
                std::io::ErrorKind::WouldBlock.into()
            } else {
                e
            }
        };
        // SAFETY: `raw` is a pipe handle this Terminal owns; `data` is valid for `len`.
        unsafe {
            if SetNamedPipeHandleState(raw, &PIPE_NOWAIT, std::ptr::null(), std::ptr::null()) == 0 {
                return Err(busy(std::io::Error::last_os_error()));
            }
            let mut n = 0u32;
            let ok = WriteFile(raw, data.as_ptr(), len, &mut n, std::ptr::null_mut());
            let err = std::io::Error::last_os_error();
            if SetNamedPipeHandleState(raw, &PIPE_WAIT, std::ptr::null(), std::ptr::null()) == 0 {
                return Err(std::io::Error::other(format!(
                    "cannot put the console's input back in blocking mode: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if ok == 0 {
                Err(busy(err))
            } else if n == 0 && !data.is_empty() {
                Err(std::io::ErrorKind::WouldBlock.into())
            } else {
                Ok(n as usize)
            }
        }
    }

    /// How long a Terminal's output may go on after its process exited and its console was
    /// closed.
    pub(super) const DRAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    /// What a ConPTY created to inherit the cursor (as portable-pty creates it) prints first.
    const CURSOR_QUERY: &[u8] = b"\x1b[6n";
    /// The answer: the cursor is at the top left.
    pub(super) const CURSOR_AT_ORIGIN: &[u8] = b"\x1b[1;1R";

    /// The ConPTY's startup cursor query. It renders nothing until the query is answered, and
    /// a Terminal may have no Desktop attached (a restore), so the Daemon answers it and keeps
    /// it out of the output: a Desktop replaying the output must not answer it again.
    #[derive(Default)]
    pub(super) struct StartupQuery {
        head: Vec<u8>,
        done: bool,
    }

    impl StartupQuery {
        /// The output to pass on, and whether the query was seen (answer it now).
        pub(super) fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, bool) {
            if self.done {
                return (bytes.to_vec(), false);
            }
            self.head.extend_from_slice(bytes);
            if self.head.starts_with(CURSOR_QUERY) {
                self.done = true;
                return (self.head.split_off(CURSOR_QUERY.len()), true);
            }
            if CURSOR_QUERY.starts_with(&self.head) {
                return (Vec::new(), false);
            }
            self.done = true;
            (std::mem::take(&mut self.head), false)
        }
    }

    use std::path::Path;
    use uuid::Uuid;
    use xshell_core::CommandPlan;

    /// The Terminal's command. With a `launcher` (this `xshelld`), the process is
    /// `xshelld job-exec <job> -- <program> <args…>`: it joins the job itself before it starts
    /// the program, so nothing the program starts escapes the job.
    ///
    /// `CommandBuilder::new` reloads the registry's environment over this process's: ours is
    /// put back, PATH included (followed by registry entries it lacks, such as a tool
    /// installed since the Desktop started), then the plan's variables.
    pub(super) fn command(
        plan: &CommandPlan,
        launcher: Option<&Path>,
        job: &str,
    ) -> CommandBuilder {
        let mut cmd = match launcher {
            Some(exe) => {
                let mut c = CommandBuilder::new(exe);
                c.args(["job-exec", job, "--"]);
                c.arg(&plan.program);
                c
            }
            None => CommandBuilder::new(&plan.program),
        };
        for a in &plan.args {
            cmd.arg(a);
        }
        // Shared with the first-message search, so it finds the agent this Terminal runs.
        xshell_core::direct_exec::terminal_env(&mut cmd, std::env::vars_os());
        for (k, v) in &plan.env {
            cmd.env(k, v);
        }
        cmd.cwd(&plan.cwd);
        cmd
    }

    /// A ConPTY and the Terminal's job while the Terminal is being started. Dropped before
    /// [`SpawnGuard::into_master`] (a failed start), it ends the job's processes and hands
    /// the console to [`abandon_console`].
    pub(super) struct SpawnGuard {
        master: Option<Box<dyn MasterPty + Send>>,
        slave: Option<Box<dyn SlavePty + Send>>,
        pub(super) job: Option<Arc<Job>>,
        id: Uuid,
    }

    impl SpawnGuard {
        pub(super) fn new(pair: PtyPair, id: Uuid) -> SpawnGuard {
            SpawnGuard {
                master: Some(pair.master),
                slave: Some(pair.slave),
                job: None,
                id,
            }
        }

        pub(super) fn master(&self) -> &(dyn MasterPty + Send) {
            self.master.as_deref().expect("held until handed on")
        }

        pub(super) fn slave(&self) -> &(dyn SlavePty + Send) {
            self.slave.as_deref().expect("held until released")
        }

        /// Drop the slave (the started process holds the console now). It shares the
        /// console with the master, so this closes nothing yet.
        pub(super) fn release_slave(&mut self) {
            self.slave.take();
        }

        /// The start succeeded: the Terminal owns the console from now on.
        pub(super) fn into_master(mut self) -> Box<dyn MasterPty + Send> {
            self.master.take().expect("held until handed on")
        }
    }

    impl Drop for SpawnGuard {
        fn drop(&mut self) {
            let Some(master) = self.master.take() else {
                return;
            };
            if let Some(job) = &self.job {
                let _ = job.terminate(1);
            }
            abandon_console(master, self.slave.take(), self.id);
        }
    }

    /// Close a console nobody reads, off this thread: its output is drained (and its startup
    /// cursor query answered) so the close can complete.
    pub(super) fn abandon_console(
        master: Box<dyn MasterPty + Send>,
        slave: Option<Box<dyn SlavePty + Send>>,
        id: Uuid,
    ) {
        let r = std::thread::Builder::new()
            .name(format!("pty-abandon-{}", &id.simple().to_string()[..8]))
            .spawn(move || {
                let reader = master.try_clone_reader().ok();
                let mut writer = master.take_writer().ok();
                if let Some(mut reader) = reader {
                    let _ = std::thread::Builder::new()
                        .name("pty-drain".into())
                        .spawn(move || {
                            let mut q = StartupQuery::default();
                            let mut buf = [0u8; 4096];
                            while let Ok(n) = reader.read(&mut buf) {
                                if n == 0 {
                                    break;
                                }
                                if q.feed(&buf[..n]).1 {
                                    if let Some(w) = writer.as_mut() {
                                        let _ = w.write_all(CURSOR_AT_ORIGIN);
                                    }
                                }
                            }
                        });
                }
                drop(slave);
                drop(master);
            });
        if let Err(e) = r {
            crate::log!("ERROR", "cannot close the console of {id} off-thread: {e}");
        }
    }

    /// Close a ConPTY on a thread of its own: `ClosePseudoConsole` may wait until the
    /// console's output is drained.
    pub(super) fn close_console(m: Box<dyn MasterPty + Send>, id: &Uuid) {
        let r = std::thread::Builder::new()
            .name(format!("pty-close-{}", &id.simple().to_string()[..8]))
            .spawn(move || drop(m));
        if let Err(e) = r {
            crate::log!("ERROR", "cannot close the console of {id} off-thread: {e}");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn startup_query_is_answered_and_dropped() {
            let mut q = StartupQuery::default();
            assert_eq!(q.feed(b"\x1b["), (vec![], false));
            assert_eq!(q.feed(b"6nhello"), (b"hello".to_vec(), true));
            assert_eq!(q.feed(b"\x1b[6n"), (b"\x1b[6n".to_vec(), false));
            let mut q = StartupQuery::default();
            assert_eq!(q.feed(b"\x1b[2J"), (b"\x1b[2J".to_vec(), false));
            assert_eq!(q.feed(b"\x1b[6n"), (b"\x1b[6n".to_vec(), false));
        }

        /// A measured fact for the CI log (Lesani/xshell#40): printed, and appended to
        /// `$XSHELL_PROBE_FACTS` when it is set.
        fn fact(line: &str) {
            eprintln!("FACT {line}");
            if let Some(p) = std::env::var_os("XSHELL_PROBE_FACTS") {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                {
                    let _ = writeln!(f, "{line}");
                }
            }
        }

        /// An anonymous pipe of `size` bytes, as ConPTY's input pipe is made.
        fn pipe(size: u32) -> (OwnedHandle, OwnedHandle) {
            use std::os::windows::io::FromRawHandle;
            use windows_sys::Win32::System::Pipes::CreatePipe;
            let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
            // SAFETY: out-pointers to two handles, owned from here on.
            unsafe {
                assert_ne!(CreatePipe(&mut r, &mut w, std::ptr::null(), size), 0);
                (
                    OwnedHandle::from_raw_handle(r),
                    OwnedHandle::from_raw_handle(w),
                )
            }
        }

        /// The probe itself, in the child [`pipe_nowait_write_semantics`] starts: one
        /// `PROBE key=value` line per measurement on stdout.
        fn pipe_probe() {
            let say = |k: &str, v: String| println!("PROBE {k}={v}");
            let show = |r: &std::io::Result<usize>| match r {
                Ok(n) => format!("ok:{n}"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => "wouldblock".into(),
                Err(e) => format!("err:{e}"),
            };
            // Filling a 4 KiB pipe 1000 bytes at a time: how much each write takes.
            let (r, w) = pipe(4096);
            let mut seq = Vec::new();
            for _ in 0..64 {
                let res = nowait_write(&w, &[b'x'; 1000]);
                let stop = !matches!(res, Ok(n) if n > 0);
                seq.push(show(&res));
                if stop {
                    break;
                }
            }
            say("fill_4k_by_1000", seq.join(","));
            // One write larger than the pipe, into an empty one.
            let (r2, w2) = pipe(4096);
            say(
                "big_64k_into_empty_4k",
                show(&nowait_write(&w2, &[b'y'; 65536])),
            );
            // The handle is back in blocking mode: a blocking write of 64 KiB through a
            // duplicate waits for the reader and arrives whole.
            let reader = std::thread::spawn(move || {
                use std::io::Read;
                let mut f = std::fs::File::from(r2);
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                while got.len() < 4096 + 65536 + 65536 {
                    match f.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                got.len()
            });
            let mut blocking = std::fs::File::from(w2.try_clone().unwrap());
            say(
                "blocking_after_nowait",
                match blocking.write_all(&[b'z'; 65536]) {
                    Ok(()) => "ok".into(),
                    Err(e) => format!("err:{e}"),
                },
            );
            drop(blocking);
            drop(w2);
            say("read_back", reader.join().unwrap().to_string());
            drop((r, w));
            say("done", "1".into());
        }

        /// P5: `PIPE_NOWAIT` writes to an anonymous pipe never block, a full pipe takes
        /// nothing, and the pipe is blocking again afterwards. Run in a child process killed
        /// after a deadline, so a write that blocks fails the test instead of hanging CI.
        #[test]
        fn pipe_nowait_write_semantics() {
            if std::env::var_os("XSHELLD_PIPE_PROBE").is_some() {
                pipe_probe();
                return;
            }
            use std::process::{Command, Stdio};
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::terminal::win::tests::pipe_nowait_write_semantics",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("XSHELLD_PIPE_PROBE", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut out = child.stdout.take().unwrap();
            let reader = std::thread::spawn(move || {
                let mut s = String::new();
                let _ = out.read_to_string(&mut s);
                s
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let status = loop {
                if let Some(st) = child.try_wait().unwrap() {
                    break Some(st);
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            let out = reader.join().unwrap();
            let probes: std::collections::HashMap<&str, &str> = out
                .lines()
                // libtest's `test … ... ` has no newline before the test's own output.
                .filter_map(|l| l[l.find("PROBE ")? + 6..].split_once('='))
                .collect();
            for (k, v) in &probes {
                fact(&format!(
                    "P5 build={} {k}={v}",
                    build().map_or("?".into(), |b| b.to_string())
                ));
            }
            assert!(status.is_some(), "the probe blocked: {out}");
            assert!(status.unwrap().success(), "{out}");
            assert_eq!(probes.get("done"), Some(&"1"), "{out}");
            // Measured on builds 20348 and 26100: a write to a pipe that holds unread data
            // takes nothing (ERROR_PIPE_BUSY, which `nowait_write` reports as WouldBlock),
            // and one larger than the pipe takes nothing even when it is empty. Never part
            // of a write, never a blocking one.
            assert_eq!(probes["fill_4k_by_1000"], "ok:1000,wouldblock", "{out}");
            assert_eq!(probes["big_64k_into_empty_4k"], "wouldblock", "{out}");
            assert_eq!(probes["blocking_after_nowait"], "ok", "{out}");
            let back: usize = probes["read_back"].parse().unwrap();
            assert_eq!(back, 65536, "only the blocking write arrived: {out}");
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    fn cell(floor: u64) -> StatusCell {
        StatusCell::new(
            Tracker::new(&LaunchSpec {
                agent: Some("claude".into()),
                ..Default::default()
            }),
            floor,
        )
    }

    fn event(c: &mut StatusCell, s: AgentStatus, now: u64) -> bool {
        let changed = c.tracker.on_event(s).unwrap();
        c.stamp(changed, now)
    }

    #[test]
    fn life_hold_states() {
        let running = Life::default();
        assert_eq!(running.hold(false), SessionHold::Live);
        // In the middle of a Relaunch the old process's exit is held: still the session.
        let relaunching = Life {
            relaunching: true,
            exited: Some(0),
            held_exit: Some(0),
            ..Life::default()
        };
        assert_eq!(relaunching.hold(false), SessionHold::Live);
        let closing = Life {
            closing: true,
            ..Life::default()
        };
        assert_eq!(closing.hold(false), SessionHold::Closing);
        let closed = Life {
            closing: true,
            exited: Some(0),
            ..Life::default()
        };
        assert_eq!(closed.hold(false), SessionHold::Closing);
        let exited = Life {
            exited: Some(0),
            ..Life::default()
        };
        assert_eq!(exited.hold(false), SessionHold::Ended);
        assert_eq!(exited.hold(true), SessionHold::Unresolved);
    }

    #[test]
    fn optional_fields_fit_their_reserve() {
        use xshell_protocol::msg::{
            PermissionPrompt, PromptOption, Speaker, LAST_LINE_MAX_CHARS, PROMPT_OPTIONS_MAX,
            PROMPT_OPTION_MAX_CHARS, PROMPT_TEXT_MAX_CHARS,
        };
        let bare = TerminalInfo {
            terminal: Uuid::new_v4(),
            spec: LaunchSpec::default(),
            meta: Map::new(),
            created_at_ms: u64::MAX,
            pid: Some(u32::MAX),
            exit_code: Some(i32::MIN),
            agent_status: None,
            status_at_ms: None,
            last_line: None,
            permission_prompt: None,
        };
        let size = |i: &TerminalInfo| serde_json::to_vec(i).unwrap().len();
        // The widest character in JSON: four UTF-8 bytes, or two for an escaped `"`.
        for widest in ["\u{1D11E}", "\"", "\\"] {
            let text =
                xshell_core::last_line::normalize(&widest.repeat(LAST_LINE_MAX_CHARS * 2)).unwrap();
            let full = TerminalInfo {
                agent_status: Some(AgentStatus::NeedsYou),
                status_at_ms: Some(u64::MAX),
                last_line: Some(LastLine {
                    from: Speaker::Agent,
                    text,
                }),
                permission_prompt: Some(PermissionPrompt {
                    id: u64::MAX,
                    text: widest.repeat(PROMPT_TEXT_MAX_CHARS),
                    options: vec![
                        PromptOption {
                            label: widest.repeat(PROMPT_OPTION_MAX_CHARS),
                        };
                        PROMPT_OPTIONS_MAX
                    ],
                }),
                ..bare.clone()
            };
            let extra = size(&full) - size(&bare);
            assert!(extra <= OPTIONAL_FIELDS_BYTES, "{widest:?}: {extra}");
        }
        // The fixed fields of the bare entry fit the per-entry overhead.
        let fixed = size(&bare)
            - serde_json::to_vec(&bare.spec).unwrap().len()
            - serde_json::to_vec(&bare.meta).unwrap().len();
        assert!(fixed <= ENTRY_OVERHEAD, "{fixed}");
    }

    #[test]
    fn status_stamps_strictly_increase() {
        let mut c = cell(0);
        assert_eq!(c.listed(), (None, None));
        assert!(event(&mut c, AgentStatus::Working, 1000));
        assert_eq!(c.listed(), (Some(AgentStatus::Working), Some(1000)));
        // The same status again: no change, no new stamp.
        assert!(!event(&mut c, AgentStatus::Working, 2000));
        assert_eq!(c.listed().1, Some(1000));
        // The clock did not move: just after the previous stamp.
        assert!(event(&mut c, AgentStatus::Finished, 1000));
        assert_eq!(c.listed(), (Some(AgentStatus::Finished), Some(1001)));
        // The clock went back.
        assert!(event(&mut c, AgentStatus::Working, 10));
        assert_eq!(c.listed().1, Some(1002));
        // And forward again.
        assert!(event(&mut c, AgentStatus::NeedsYou, 5000));
        assert_eq!(c.listed().1, Some(5000));
        assert_eq!(c.last_stamp(), 5000);
    }

    #[test]
    fn hand_over_moves_an_early_replacement_stamp_after_the_previous_run() {
        let dir = tempfile::tempdir().unwrap();
        let d = super::super::role::tests::daemon(dir.path());
        let persisted = || PersistedTerminal {
            terminal: Uuid::new_v4(),
            spec: LaunchSpec {
                agent: Some("codex".into()),
                ..Default::default()
            },
            meta: Map::new(),
            cols: 80,
            rows: 24,
            created_at_ms: 0,
            leader: None,
            prompt_id_floor: None,
        };
        let prev = unresolved(&d, persisted());
        let next = unresolved(&d, persisted());
        // The previous run stamped at 5000; the replacement's reader stamped an OSC 9 at
        // 100 (a clock set back), before the hand-over.
        prev.status.lock().unwrap().at_ms = Some(5000);
        {
            let mut c = next.status.lock().unwrap();
            c.at_ms = None;
            c.tracker = Tracker::new(&next.spec());
            let changed = c.tracker.on_output(b"\x1b]9;Approval requested\x07");
            assert!(c.stamp(changed, 100));
            assert_eq!(c.listed().1, Some(100));
        }
        prev.hand_over(&next);
        assert_eq!(
            next.info().status_at_ms,
            Some(5001),
            "listed after the previous run's stamp"
        );
        // Later stamps continue from there, the clock still behind.
        let mut c = next.status.lock().unwrap();
        let changed = c.tracker.on_event(AgentStatus::Working).unwrap();
        assert!(c.stamp(changed, 200));
        assert_eq!(c.listed().1, Some(5002));
        drop(c);
        // A replacement stamped after the floor keeps its stamp.
        let mut c = cell(0);
        assert!(event(&mut c, AgentStatus::Working, 9000));
        c.raise_floor(5000);
        assert_eq!(c.listed().1, Some(9000));
    }

    /// A Relaunch's replacement starts at the size read before it started; a size taken on
    /// the old Terminal after that (late input) is applied to it at the hand-over, and the
    /// attached Mobiles are told. The replacement is a real process: its PTY's size is read
    /// back.
    #[cfg(unix)]
    #[test]
    fn hand_over_applies_a_size_taken_while_the_replacement_started() {
        use super::super::outbox::{Out, PaceCfg};
        let dir = tempfile::tempdir().unwrap();
        let d = super::super::role::tests::daemon(dir.path());
        let id = Uuid::new_v4();
        let persisted = || PersistedTerminal {
            terminal: id,
            spec: LaunchSpec {
                agent: Some("claude".into()),
                ..Default::default()
            },
            meta: Map::new(),
            cols: 80,
            rows: 24,
            created_at_ms: 0,
            leader: None,
            prompt_id_floor: None,
        };
        let prev = unresolved(&d, persisted());
        let prog = dir.path().join("quiet");
        std::fs::write(&prog, "#!/bin/sh\nexec sleep 100\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = LaunchSpec {
            cwd: dir.path().to_string_lossy().into_owned(),
            shell_mode: Some("raw".into()),
            shell_command: Some(prog.to_string_lossy().into_owned()),
            ..Default::default()
        };
        // Started at the size read before the late input.
        let next = spawn(&d, id, spec, Map::new(), 80, 24, 0).unwrap();
        let pty_size = |t: &Terminal| {
            let io = t.io.lock().unwrap();
            let s = io.master.as_ref().unwrap().get_size().unwrap();
            (s.cols, s.rows)
        };
        assert_eq!(pty_size(&next), (80, 24));
        let desk = Outbox::with_pace(1 << 20, 1 << 21, None, None);
        let pace = PaceCfg {
            idle: Duration::from_secs(1),
            burst: Duration::from_millis(100),
            window: Duration::from_secs(3),
        };
        let mob = Outbox::with_pace(1 << 20, 1 << 21, None, Some(pace));
        {
            let mut o = prev.out.lock().unwrap();
            o.subs.insert(1, desk.clone());
            o.subs.insert(2, mob.clone());
        }
        // The Mobile typed after the replacement started at 80×24.
        prev.io.lock().unwrap().arb.on_resize(2, 40, 20, true);
        prev.io.lock().unwrap().arb.on_input(2, true);
        prev.hand_over(&next);
        assert_eq!(next.io.lock().unwrap().arb.current(), (40, 20));
        assert_eq!(
            pty_size(&next),
            (40, 20),
            "the replacement's PTY was not resized"
        );
        assert_eq!(next.attached(), 2);
        let kinds = |ob: &Outbox| -> Vec<String> {
            ob.take_all()
                .into_iter()
                .map(|o| match o {
                    Out::Output { urgent, .. } => format!("output urgent={urgent}"),
                    Out::Size { frame, .. } => String::from_utf8_lossy(&frame[5..]).into(),
                    _ => "other".into(),
                })
                .collect()
        };
        assert_eq!(kinds(&desk), vec!["output urgent=true"]);
        assert_eq!(
            kinds(&mob),
            vec![
                "output urgent=true".to_string(),
                format!(r#"{{"t":"term.size","terminal":"{id}","cols":40,"rows":20}}"#)
            ]
        );
        // Unchanged size: no notice.
        let third = unresolved(&d, persisted());
        third.io.lock().unwrap().arb = SizeArbiter::new(40, 20);
        next.hand_over(&third);
        assert_eq!(kinds(&mob), vec!["output urgent=true"]);
        next.kill(Duration::from_millis(100));
        assert!(next.wait_exited(Instant::now() + Duration::from_secs(5)));
    }

    /// An overflow recovered with a fresh replay keeps the size notice still queued.
    #[test]
    fn mobile_overflow_recovery_keeps_the_size_notice() {
        use super::super::outbox::{Out, PaceCfg};
        let dir = tempfile::tempdir().unwrap();
        let d = super::super::role::tests::daemon(dir.path());
        let t = unresolved(
            &d,
            PersistedTerminal {
                terminal: Uuid::new_v4(),
                spec: LaunchSpec::default(),
                meta: Map::new(),
                cols: 80,
                rows: 24,
                created_at_ms: 0,
                leader: None,
                prompt_id_floor: None,
            },
        );
        let desk = Outbox::with_pace(1 << 20, 1 << 21, None, None);
        let pace = PaceCfg {
            idle: Duration::from_secs(1),
            burst: Duration::from_millis(100),
            window: Duration::from_secs(3),
        };
        let mob = Outbox::with_pace(1 << 20, 1 << 21, None, Some(pace));
        {
            let mut o = t.out.lock().unwrap();
            o.subs.insert(1, desk.clone());
            o.subs.insert(2, mob.clone());
            mob.push_output(t.id, Arc::from(&b"stale"[..]));
            t.notify_size(&o, (40, 20));
            mob.push_output(t.id, Arc::from(&b"more"[..]));
        }
        t.recover_mobile_overflow(&mob, Duration::from_millis(10));
        let got: Vec<String> = mob
            .take_all()
            .into_iter()
            .map(|o| match o {
                Out::Output { data, .. } => format!("out {}", String::from_utf8_lossy(&data)),
                Out::Size { frame, .. } => String::from_utf8_lossy(&frame[5..]).into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                format!(
                    r#"{{"t":"term.size","terminal":"{}","cols":40,"rows":20}}"#,
                    t.id
                ),
                "out \u{1b}c".to_string(),
            ]
        );
    }

    #[test]
    fn replacement_run_stamps_after_the_previous_run() {
        // A Relaunch's run starts unstamped, above the replaced run's last stamp.
        let mut c = cell(5000);
        assert_eq!(c.listed(), (None, None));
        assert_eq!(c.last_stamp(), 5000);
        assert!(event(&mut c, AgentStatus::Working, 4000));
        assert_eq!(c.listed().1, Some(5001));
        let mut c = cell(5000);
        assert!(event(&mut c, AgentStatus::Working, 9000));
        assert_eq!(c.listed().1, Some(9000));
        // Non-agent Terminals have no status and so no stamp.
        let mut shell = StatusCell::new(
            Tracker::new(&LaunchSpec {
                shell_mode: Some("raw".into()),
                ..Default::default()
            }),
            0,
        );
        let ended = shell.tracker.on_exit();
        assert!(!shell.stamp(ended, 1));
        assert_eq!(shell.listed(), (None, None));
    }

    /// A PTY that records each write, takes at most `take` bytes per write, answers
    /// `WouldBlock` `full` times first, and fails on demand.
    #[derive(Default)]
    struct Recorder {
        ops: Arc<Mutex<Vec<String>>>,
        input: Vec<u8>,
        take: Option<usize>,
        full: usize,
        fail_on: Option<&'static [u8]>,
        /// Cannot write without blocking (no descriptor).
        blocking_only: bool,
    }

    impl Write for Recorder {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let s = String::from_utf8_lossy(b).into_owned();
            self.ops.lock().unwrap().push(format!("write {s:?}"));
            self.input.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.ops.lock().unwrap().push("flush".into());
            Ok(())
        }
    }

    impl PtyIn for Recorder {
        fn write_now(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.fail_on == Some(b) {
                return Err(std::io::Error::other("closed"));
            }
            if self.blocking_only {
                return Err(std::io::ErrorKind::Unsupported.into());
            }
            if self.full > 0 {
                self.full -= 1;
                self.ops.lock().unwrap().push("full".into());
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let b = &b[..b.len().min(self.take.unwrap_or(usize::MAX))];
            let s = String::from_utf8_lossy(b).into_owned();
            self.ops.lock().unwrap().push(format!("write {s:?}"));
            self.input.extend_from_slice(b);
            Ok(b.len())
        }
    }

    type Outcome = Arc<Mutex<Vec<Result<(), String>>>>;

    /// A submit of `text` whose gate answers from `gates` (one per call, then `Ok`) and
    /// records its steps and the outcomes in `ops` and `out`. The write runs inside the gate.
    fn submit_of(
        text: &str,
        ops: &Arc<Mutex<Vec<String>>>,
        out: &Outcome,
        mut gates: Vec<Result<(), String>>,
    ) -> Input {
        let (o, r) = (ops.clone(), out.clone());
        gates.reverse();
        let gate: SubmitGate = Box::new(move |step, write| {
            o.lock().unwrap().push(format!("gate {step:?}"));
            gates.pop().unwrap_or(Ok(()))?;
            Ok(write())
        });
        let done = Box::new(move |res| r.lock().unwrap().push(res));
        Input::Submit(Box::new(Submit::new(
            Typing::text(text),
            Duration::from_millis(50),
            gate,
            done,
        )))
    }

    fn submit(
        ops: &Arc<Mutex<Vec<String>>>,
        out: &Outcome,
        gates: Vec<Result<(), String>>,
    ) -> Input {
        submit_of("fix it\nplease", ops, out, gates)
    }

    fn sleeper(ops: &Arc<Mutex<Vec<String>>>) -> impl Fn(Duration) {
        let o = ops.clone();
        move |d| o.lock().unwrap().push(format!("sleep {}ms", d.as_millis()))
    }

    #[test]
    fn submit_item_writes_paste_pause_enter() {
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        let item = submit(&ops, &out, vec![]);
        write_item(&mut w, item, &sleeper(&ops)).unwrap();
        assert_eq!(
            *ops.lock().unwrap(),
            [
                "gate Paste",
                "write \"\\u{1b}[200~fix it\\nplease\\u{1b}[201~\"",
                "sleep 50ms",
                "gate Enter",
                "write \"\\r\"",
            ]
        );
        assert_eq!(*out.lock().unwrap(), [Ok(())]);
        // Plain bytes are written as they are.
        ops.lock().unwrap().clear();
        write_item(&mut w, Input::Bytes(b"ab".to_vec()), &sleeper(&ops)).unwrap();
        assert_eq!(*ops.lock().unwrap(), ["write \"ab\"", "flush"]);
    }

    /// A long paste goes in pieces, a PTY that takes part of a piece gets the rest next, and
    /// one that takes nothing is waited for holding nothing: every write is inside its gate.
    #[test]
    fn submit_item_writes_in_gated_pieces() {
        let text = "x".repeat(SUBMIT_CHUNK * 2 + 10);
        let mut w = Recorder {
            take: Some(700),
            full: 2,
            ..Default::default()
        };
        let ops = w.ops.clone();
        let out = Outcome::default();
        write_item(&mut w, submit_of(&text, &ops, &out, vec![]), &sleeper(&ops)).unwrap();
        let mut want = bracketed(&text);
        want.push(b'\r');
        assert_eq!(w.input, want);
        assert_eq!(*out.lock().unwrap(), [Ok(())]);
        let ops = ops.lock().unwrap();
        for (i, op) in ops.iter().enumerate() {
            if op.starts_with("write") || op == "full" {
                assert!(ops[i - 1].starts_with("gate"), "{i}: {ops:?}");
            }
        }
        assert_eq!(ops.iter().filter(|o| *o == "full").count(), 2);
        assert!(ops.contains(&format!("sleep {}ms", SUBMIT_WAIT.as_millis())));
        let pieces = ops.iter().filter(|o| *o == "gate Paste").count();
        assert!(pieces >= 5, "{pieces}: {ops:?}");
    }

    #[test]
    fn submit_item_stops_where_the_gate_refuses() {
        let enter = "write \"\\r\"".to_string();
        // Before the paste: nothing is written, and the refusal is the outcome.
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        let item = submit(&ops, &out, vec![Err(SUBMIT_NEEDS_YOU.into())]);
        write_item(&mut w, item, &sleeper(&ops)).unwrap();
        assert_eq!(*ops.lock().unwrap(), ["gate Paste"]);
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_NEEDS_YOU.to_string())]);
        // In the middle of the paste: unknown, and no Enter.
        let mut w = Recorder {
            take: Some(5),
            ..Default::default()
        };
        let ops = w.ops.clone();
        let out = Outcome::default();
        let item = submit(&ops, &out, vec![Ok(()), Err(SUBMIT_NOT_READY.into())]);
        write_item(&mut w, item, &sleeper(&ops)).unwrap();
        // The paste's frame is completed, nothing more of the text.
        assert_eq!(w.input, b"\x1b[200~\x1b[201~");
        assert!(!ops.lock().unwrap().contains(&enter));
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
        // Before Enter: the paste is out, Enter is not; the outcome is unknown.
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        let item = submit(&ops, &out, vec![Ok(()), Err(SUBMIT_NEEDS_YOU.into())]);
        write_item(&mut w, item, &sleeper(&ops)).unwrap();
        let ops = ops.lock().unwrap();
        assert_eq!(ops.last().unwrap(), "gate Enter");
        assert!(!ops.contains(&enter), "{ops:?}");
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
    }

    #[test]
    fn a_stopped_paste_is_closed() {
        let p = bracketed("aé🦀b");
        let n = p.len();
        // Inside the start marker, inside the text, inside a character, inside the end.
        assert_eq!(paste_close(&p, 3), b"00~\x1b[201~");
        assert_eq!(paste_close(&p, 6), b"\x1b[201~");
        assert_eq!(paste_close(&p, 7), b"\x1b[201~");
        assert_eq!(
            paste_close(&p, 8),
            [&p[8..9], PASTE_END].concat(),
            "é cut in two"
        );
        assert_eq!(
            paste_close(&p, 10),
            [&p[10..13], PASTE_END].concat(),
            "🦀 cut"
        );
        assert_eq!(paste_close(&p, n - 3), &p[n - 3..]);
        // Refused after the first piece of a long reply: the end marker, under the gate
        // (a Close step), then nothing; the outcome is unknown.
        let text = "y".repeat(SUBMIT_CHUNK * 2);
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        let item = submit_of(
            &text,
            &ops,
            &out,
            vec![Ok(()), Err(SUBMIT_NEEDS_YOU.into())],
        );
        write_item(&mut w, item, &sleeper(&ops)).unwrap();
        let mut want = PASTE_START.to_vec();
        want.extend(std::iter::repeat_n(b'y', SUBMIT_CHUNK - PASTE_START.len()));
        want.extend_from_slice(PASTE_END);
        assert_eq!(w.input, want);
        let ops = ops.lock().unwrap();
        assert_eq!(
            &ops[ops.len() - 2..],
            ["gate Close", "write \"\\u{1b}[201~\""]
        );
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
    }

    /// A submit of `typing`, gated as [`submit_of`].
    fn submit_typing(
        typing: Typing,
        ops: &Arc<Mutex<Vec<String>>>,
        out: &Outcome,
        mut gates: Vec<Result<(), String>>,
    ) -> Input {
        let (o, r) = (ops.clone(), out.clone());
        gates.reverse();
        let gate: SubmitGate = Box::new(move |step, write| {
            o.lock().unwrap().push(format!("gate {step:?}"));
            gates.pop().unwrap_or(Ok(()))?;
            Ok(write())
        });
        let done = Box::new(move |res| r.lock().unwrap().push(res));
        Input::Submit(Box::new(Submit::new(
            typing,
            Duration::from_millis(50),
            gate,
            done,
        )))
    }

    #[test]
    fn typed_paths_are_quoted_as_the_agents_read_them() {
        for (p, want) in [
            (
                "/run/user/1000/xshell/tmp/xshell-clipboard/1-ab-photo.jpg",
                Some("/run/user/1000/xshell/tmp/xshell-clipboard/1-ab-photo.jpg"),
            ),
            ("/home/é/x+y,z@h:1%=~.png", Some("/home/é/x+y,z@h:1%=~.png")),
            ("/home/a b/p.jpg", Some(r#""/home/a b/p.jpg""#)),
            (
                r#"/home/it's "x"/p.jpg"#,
                Some(r#""/home/it's \"x\"/p.jpg""#),
            ),
            (r"/home/a\b/p.jpg", Some(r#""/home/a\\b/p.jpg""#)),
            (
                "/home/$HOME/`x`;|&#*?[]<>(){}!/p.jpg",
                Some("\"/home/$HOME/`x`;|&#*?[]<>(){}!/p.jpg\""),
            ),
            ("/home/a\x1b[201~/p.jpg", None),
            ("/home/a\nb/p.jpg", None),
            ("/home/a\rb/p.jpg", None),
            ("/home/a\tb/p.jpg", None),
            ("/home/a\u{9b}b/p.jpg", None),
            ("/home/a\x7fb/p.jpg", None),
        ] {
            assert_eq!(typed_path(p).as_deref(), want, "{p:?}");
        }
        // What Claude Code does with a pasted path: outer quotes off, then `\x` → `x`
        // (`\\` → `\`). Every quoted path comes back as it was.
        let claude = |t: &str| {
            let t = t
                .strip_prefix('"')
                .and_then(|t| t.strip_suffix('"'))
                .unwrap_or(t);
            let mut out = String::new();
            let mut it = t.chars();
            while let Some(c) = it.next() {
                out.push(if c == '\\' { it.next().unwrap_or(c) } else { c });
            }
            out
        };
        for p in [
            "/a b/c.jpg",
            r#"/it's "x"/p.jpg"#,
            r"/a\b\\c/p.jpg",
            "/plain/p.png",
        ] {
            assert_eq!(claude(&typed_path(p).unwrap()), p);
        }
    }

    #[test]
    fn a_reply_with_files_types_each_path_as_its_own_paste() {
        let typing = Typing::reply(
            &["/d/1-a.jpg".into(), r#""/d d/2.png""#.into()],
            Some("what is this"),
        );
        let mut want = b"\x1b[200~/d/1-a.jpg\x1b[201~ ".to_vec();
        want.extend_from_slice(b"\x1b[200~\"/d d/2.png\"\x1b[201~ ");
        want.extend_from_slice(b"\x1b[200~what is this\x1b[201~");
        assert_eq!(typing.bytes, want);
        assert_eq!(typing.frames.len(), 3);
        for f in &typing.frames {
            assert!(typing.bytes[f.clone()].starts_with(PASTE_START));
            assert!(typing.bytes[f.clone()].ends_with(PASTE_END));
        }
        // No caption: the paths only.
        let only = Typing::reply(&["/d/1-a.jpg".into()], None);
        assert_eq!(only.bytes, b"\x1b[200~/d/1-a.jpg\x1b[201~ ");
        // Written in one item: each paste a gated piece of its own, then Enter after the
        // pause.
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        write_item(
            &mut w,
            submit_typing(typing, &ops, &out, vec![]),
            &sleeper(&ops),
        )
        .unwrap();
        want.push(b'\r');
        assert_eq!(w.input, want);
        assert_eq!(*out.lock().unwrap(), [Ok(())]);
        let ops = ops.lock().unwrap();
        assert_eq!(
            &ops[..],
            [
                "gate PasteFile",
                "write \"\\u{1b}[200~/d/1-a.jpg\\u{1b}[201~\"",
                "gate Paste",
                "write \" \"",
                "gate PasteFile",
                "write \"\\u{1b}[200~\\\"/d d/2.png\\\"\\u{1b}[201~\"",
                "gate Paste",
                "write \" \"",
                "gate Paste",
                "write \"\\u{1b}[200~what is this\\u{1b}[201~\"",
                "sleep 50ms",
                "gate Enter",
                "write \"\\r\"",
            ]
        );
    }

    /// After each file's paste the reply waits, holding nothing, until the file shows
    /// attached, before its space; and stops with no more typed when it never does or the
    /// agent needs you meanwhile.
    #[test]
    fn a_reply_waits_for_each_file_to_attach() {
        let typing = || Typing::reply(&["/d/1.jpg".into(), "/d/2.jpg".into()], Some("cap"));
        assert_eq!(typing().waits, [20, 41]);
        // Attached after two checks, then at once.
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        let o = ops.clone();
        let mut answers = vec![Ok(true), Ok(true), Ok(false), Ok(false)];
        let attach: SubmitAttach = Box::new(move |n| {
            o.lock().unwrap().push(format!("attach {n}"));
            answers.pop().unwrap()
        });
        let Input::Submit(s) = submit_typing(typing(), &ops, &out, vec![]) else {
            unreachable!()
        };
        let s = s.on_attach(attach);
        write_item(&mut w, Input::Submit(Box::new(s)), &sleeper(&ops)).unwrap();
        assert_eq!(*out.lock().unwrap(), [Ok(())]);
        let ops = ops.lock().unwrap();
        let first = ops.iter().position(|o| o == "attach 1").unwrap();
        assert_eq!(
            &ops[first - 1..first + 9],
            [
                "write \"\\u{1b}[200~/d/1.jpg\\u{1b}[201~\"",
                "attach 1",
                "sleep 10ms",
                "attach 1",
                "sleep 10ms",
                "attach 1",
                "gate Paste",
                "write \" \"",
                "gate PasteFile",
                "write \"\\u{1b}[200~/d/2.jpg\\u{1b}[201~\"",
            ]
        );
        assert_eq!(ops[first + 9], "attach 2");
        assert_eq!(ops[first + 10], "gate Paste");
        // Never attached within the timeout: no space, no caption, no Enter.
        for answer in [Ok(false), Err(SUBMIT_NEEDS_YOU.to_string())] {
            let mut w = Recorder::default();
            let ops = w.ops.clone();
            let out = Outcome::default();
            let Input::Submit(s) = submit_typing(typing(), &ops, &out, vec![]) else {
                unreachable!()
            };
            let a = answer.clone();
            let mut s = s.on_attach(Box::new(move |_| a.clone()));
            s.attach_timeout = Duration::from_millis(30);
            write_item(&mut w, Input::Submit(Box::new(s)), &sleeper(&ops)).unwrap();
            assert_eq!(w.input, b"\x1b[200~/d/1.jpg\x1b[201~", "{answer:?}");
            assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
            let sleeps = ops
                .lock()
                .unwrap()
                .iter()
                .filter(|o| *o == "sleep 10ms")
                .count();
            assert_eq!(sleeps, if answer.is_ok() { 3 } else { 0 });
        }
    }

    /// Stopped inside the second paste: that paste is closed, nothing else is typed (no
    /// caption, no Enter), and the outcome is unknown. Stopped between two pastes: nothing
    /// to close. Before the first: refused, nothing written.
    #[test]
    fn a_reply_with_files_stops_cleanly() {
        let typing = || Typing::reply(&["/d/1.jpg".into(), "/d/2.jpg".into()], Some("cap"));
        let first = b"\x1b[200~/d/1.jpg\x1b[201~ ".to_vec();
        // Refused at the second paste's second piece (it takes 4 bytes at a time).
        let mut w = Recorder {
            take: Some(4),
            ..Default::default()
        };
        let ops = w.ops.clone();
        let out = Outcome::default();
        // The first paste in 4-byte pieces, its space, one piece of the second.
        let pieces = (first.len() - 1).div_ceil(4) + 2;
        let mut gates = vec![Ok(()); pieces];
        gates.push(Err(SUBMIT_NEEDS_YOU.into()));
        write_item(
            &mut w,
            submit_typing(typing(), &ops, &out, gates),
            &sleeper(&ops),
        )
        .unwrap();
        let mut want = first.clone();
        want.extend_from_slice(b"\x1b[20");
        want.extend_from_slice(b"0~");
        want.extend_from_slice(PASTE_END);
        assert_eq!(
            String::from_utf8_lossy(&w.input),
            String::from_utf8_lossy(&want)
        );
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
        assert!(!ops.lock().unwrap().iter().any(|o| o.contains("\\r")));
        // Refused right after the first path and its space: nothing to close.
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        let gates = vec![Ok(()), Ok(()), Err(SUBMIT_NOT_READY.into())];
        write_item(
            &mut w,
            submit_typing(typing(), &ops, &out, gates),
            &sleeper(&ops),
        )
        .unwrap();
        assert_eq!(w.input, first);
        assert_eq!(ops.lock().unwrap().last().unwrap(), "gate PasteFile");
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
        // Refused before anything: the refusal.
        let mut w = Recorder::default();
        let ops = w.ops.clone();
        let out = Outcome::default();
        let gates = vec![Err(SUBMIT_NEEDS_YOU.into())];
        write_item(
            &mut w,
            submit_typing(typing(), &ops, &out, gates),
            &sleeper(&ops),
        )
        .unwrap();
        assert!(w.input.is_empty());
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_NEEDS_YOU.to_string())]);
    }

    /// The end marker cannot be written in time: the Terminal's replies are stuck.
    #[test]
    fn an_unclosable_paste_is_stuck() {
        let mut w = Recorder {
            take: Some(10),
            ..Default::default()
        };
        let ops = w.ops.clone();
        let out = Outcome::default();
        let stuck = Arc::new(Mutex::new(0));
        let st = stuck.clone();
        let Input::Submit(s) = submit(&ops, &out, vec![Ok(())]) else {
            unreachable!()
        };
        let mut s = s.on_stuck(Box::new(move || *st.lock().unwrap() += 1));
        // The first piece goes through; then the PTY takes nothing, the end marker neither.
        let mut inner = std::mem::replace(&mut s.gate, Box::new(|_, _| Ok(Ok(0))));
        let mut gates = 0;
        s.gate = Box::new(move |step, write| {
            gates += 1;
            if gates == 1 {
                inner(step, write)
            } else {
                Ok(Err(std::io::ErrorKind::WouldBlock.into()))
            }
        });
        write_item(&mut w, Input::Submit(Box::new(s)), &sleeper(&ops)).unwrap();
        assert_eq!(*stuck.lock().unwrap(), 1);
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
        assert_eq!(w.input.len(), 10);
    }

    /// A PTY that takes every write whole and keeps each one apart.
    #[derive(Default)]
    struct Pieces(Vec<Vec<u8>>);

    impl Write for Pieces {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.push(b.to_vec());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl PtyIn for Pieces {
        fn write_now(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.write(b)
        }
    }

    /// The writes a reply of `text` is typed in, pieces as `alone` and `max` say, with
    /// `gates` as [`submit_of`]; and its outcome.
    fn pieces_of(
        text: &str,
        alone: bool,
        max: usize,
        gates: Vec<Result<(), String>>,
    ) -> (Vec<Vec<u8>>, Vec<Result<(), String>>) {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let out = Outcome::default();
        let Input::Submit(mut s) = submit_of(text, &ops, &out, gates) else {
            unreachable!()
        };
        s.markers_alone = alone;
        s.max_piece = max;
        let mut w = Pieces::default();
        write_item(&mut w, Input::Submit(s), &|_| {}).unwrap();
        let out = out.lock().unwrap().clone();
        (w.0, out)
    }

    /// Windows' piece rule: each marker is a write of its own, and every other piece of the
    /// paste is whole characters; the bytes are the same as without the rule.
    #[test]
    fn markers_alone_pieces_never_split_a_marker_or_char() {
        // Two- to four-byte characters across every 1024-byte border.
        let text: String = "aé€🚀\n".chars().cycle().take(1400).collect();
        assert!(text.len() > 3000, "{}", text.len());
        let (plain, _) = pieces_of(&text, false, SUBMIT_CHUNK, vec![]);
        let (pieces, out) = pieces_of(&text, true, SUBMIT_CHUNK, vec![]);
        assert_eq!(out, [Ok(())]);
        assert_eq!(pieces.concat(), plain.concat(), "the same bytes");
        assert_eq!(pieces.first().unwrap(), PASTE_START);
        assert_eq!(pieces[pieces.len() - 2], PASTE_END);
        assert_eq!(pieces.last().unwrap(), b"\r");
        let body = &pieces[1..pieces.len() - 2];
        assert!(body.len() >= 3, "{}", body.len());
        for p in body {
            assert!(p.len() <= SUBMIT_CHUNK);
            let s = std::str::from_utf8(p).expect("whole characters");
            assert!(!s.contains('\x1b'));
        }
        // Without the rule some piece cuts a character (the test text is worth its name).
        assert!(plain.iter().any(|p| std::str::from_utf8(p).is_err()));
        // Pieces shorter than a character still carry whole ones.
        let (tiny, _) = pieces_of("🚀é", true, 1, vec![]);
        assert_eq!(
            tiny,
            [
                PASTE_START,
                "🚀".as_bytes(),
                "é".as_bytes(),
                PASTE_END,
                b"\r".as_slice()
            ]
        );
        // A probe's one-byte pieces without the rule split everything.
        let (bytes, out) = pieces_of("é", false, 1, vec![]);
        assert_eq!(bytes.len(), bracketed("é").len() + 1);
        assert!(bytes.iter().all(|p| p.len() == 1));
        assert_eq!(out, [Ok(())]);
    }

    /// A reply refused after its start marker alone: only the end marker follows.
    #[test]
    fn a_stopped_marker_alone_paste_is_closed() {
        let (pieces, out) = pieces_of(
            "fix it\nplease",
            true,
            SUBMIT_CHUNK,
            vec![Ok(()), Err(SUBMIT_NOT_READY.into())],
        );
        assert_eq!(pieces, [PASTE_START, PASTE_END]);
        assert_eq!(out, [Err(SUBMIT_UNCONFIRMED.to_string())]);
        // Refused inside the text: the end marker, never more text, never Enter.
        let (pieces, out) = pieces_of(
            &"é".repeat(SUBMIT_CHUNK),
            true,
            SUBMIT_CHUNK,
            vec![Ok(()), Ok(()), Err(SUBMIT_NEEDS_YOU.into())],
        );
        assert_eq!(pieces.len(), 3, "{pieces:?}");
        assert_eq!(pieces[2], PASTE_END);
        assert!(std::str::from_utf8(&pieces[1]).is_ok());
        assert_eq!(out, [Err(SUBMIT_UNCONFIRMED.to_string())]);
    }

    /// No descriptor for writes that cannot block: a reply is refused, and nothing is
    /// written in its place.
    #[test]
    fn a_reply_never_blocks() {
        let mut w = Recorder {
            blocking_only: true,
            ..Default::default()
        };
        let ops = w.ops.clone();
        let out = Outcome::default();
        write_item(&mut w, submit(&ops, &out, vec![]), &sleeper(&ops)).unwrap();
        assert!(w.input.is_empty());
        assert_eq!(*ops.lock().unwrap(), ["gate Paste"]);
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_NO_NONBLOCK.to_string())]);
        // The PtyWriter without one refuses too, never writing through its blocking writer.
        let rec = Recorder::default();
        let rops = rec.ops.clone();
        let mut pw = PtyWriter {
            w: Box::new(rec),
            #[cfg(unix)]
            fd: None,
            #[cfg(windows)]
            pipe: None,
        };
        let e = pw.write_now(b"x").unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
        assert!(rops.lock().unwrap().is_empty());
    }

    /// A PTY that takes nothing for `SUBMIT_STALL`: nothing written is a refusal, a paste
    /// without its Enter is unknown.
    #[test]
    fn submit_item_gives_up_on_a_stalled_pty() {
        let mut w = Recorder {
            full: usize::MAX,
            ..Default::default()
        };
        let ops = w.ops.clone();
        let out = Outcome::default();
        write_item(&mut w, submit(&ops, &out, vec![]), &sleeper(&ops)).unwrap();
        assert!(w.input.is_empty());
        assert_eq!(
            *out.lock().unwrap(),
            [Err("input backlog full".to_string())]
        );
        let waits = SUBMIT_STALL.as_millis() / SUBMIT_WAIT.as_millis();
        let slept = ops
            .lock()
            .unwrap()
            .iter()
            .filter(|o| o.starts_with("sleep"))
            .count();
        assert_eq!(slept as u128, waits);
    }

    #[test]
    fn submit_item_reports_once_whatever_ends_it() {
        // The PTY fails at Enter: unknown, reported once, and the thread stops.
        let mut w = Recorder {
            fail_on: Some(b"\r"),
            ..Default::default()
        };
        let ops = w.ops.clone();
        let out = Outcome::default();
        assert!(write_item(&mut w, submit(&ops, &out, vec![]), &sleeper(&ops)).is_err());
        assert_eq!(*out.lock().unwrap(), [Err(SUBMIT_UNCONFIRMED.to_string())]);
        // It fails before any of the paste: nothing was written.
        let mut w = Recorder {
            fail_on: Some(b"\x1b[200~fix it\nplease\x1b[201~"),
            ..Default::default()
        };
        let out = Outcome::default();
        assert!(write_item(&mut w, submit(&ops, &out, vec![]), &sleeper(&ops)).is_err());
        assert_eq!(
            *out.lock().unwrap(),
            [Err("terminal has exited".to_string())]
        );
        // Still queued when the input thread ends: nothing was written.
        let ops = Arc::new(Mutex::new(Vec::new()));
        let out = Outcome::default();
        drop(submit(&ops, &out, vec![]));
        assert_eq!(
            *out.lock().unwrap(),
            [Err("terminal has exited".to_string())]
        );
        assert!(ops.lock().unwrap().is_empty());
    }
}

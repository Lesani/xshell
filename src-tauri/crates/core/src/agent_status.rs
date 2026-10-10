//! Agent Status from agent hooks: what an agent Terminal is doing (working, needs you,
//! finished, ended), shared by the Daemon and the Desktop's Local Host Terminals.
//!
//! - [`Tracker`]: the per-run state machine. Hooks report through [`Tracker::on_event`];
//!   input, output and the process exit fill the gaps the hooks leave.
//! - [`AgentHooks`]: what a launch adds so the agent reports. Claude Code gets a
//!   `--settings` file with hooks; Codex gets `-c` overrides (`notify` for a finished turn,
//!   OSC 9 TUI notifications for approvals, which [`Tracker::on_output`] scans for).
//! - [`event_main`]: the hook client (`xshelld event …`, or the Desktop executable's
//!   `xshell event …`), which sends one `term.event` over the event socket.
//! - [`serve_event_conn`]: the Desktop's side of one event-socket connection.
//!
//! Nothing here writes the agents' own configuration (`~/.claude`, `~/.codex`): everything
//! is passed per launch.

use crate::chat::SESSION_ID_MAX;
use crate::launch::agent_binary;
use crate::sessions::valid_session_id;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
use xshell_protocol::msg::{decode_inbound, encode_msg, encode_res, ClientMsg, DecodeError, Hello};
use xshell_protocol::negotiate::negotiate;
use xshell_protocol::{LaunchSpec, PROTOCOL};

pub use xshell_protocol::msg::AgentStatus;

/// `<terminal>.<run>`: which Terminal and run a hook reports for.
pub const TERMINAL_ID_ENV: &str = "XSHELL_TERMINAL_ID";
/// The socket the hook client sends to.
pub const EVENT_SOCKET_ENV: &str = "XSHELL_EVENT_SOCKET";

/// The agents whose hooks report an Agent Status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAgent {
    Claude,
    Codex,
}

/// The hook-reporting agent a Terminal runs, if any: none for raw shells and for agents
/// without hooks (Cursor, opencode, Antigravity).
pub fn hook_agent(spec: &LaunchSpec) -> Option<HookAgent> {
    if spec.shell_mode.as_deref() == Some("raw") {
        return None;
    }
    match agent_binary(spec.agent.as_deref()) {
        "claude" => Some(HookAgent::Claude),
        "codex" => Some(HookAgent::Codex),
        _ => None,
    }
}

// ── State machine ─────────────────────────────────────────────────────────

/// One run's Agent Status. Pure: callers feed it hook reports, input, output and the exit,
/// and publish the status when a method returns `true` (changed).
///
/// Hooks drive Claude Code completely (`UserPromptSubmit`/`PostToolUse` working,
/// `Notification` needs you, `Stop` finished, `SessionEnd` ended). Codex has no hooks
/// without writing `~/.codex`, so its `notify` reports finished, OSC 9 approval
/// notifications in its output mean needs you, and input fills in working. Ended is final.
#[derive(Debug, Clone)]
pub struct Tracker {
    agent: Option<HookAgent>,
    status: Option<AgentStatus>,
    osc: OscScanner,
}

impl Tracker {
    pub fn new(spec: &LaunchSpec) -> Self {
        Self {
            agent: hook_agent(spec),
            status: None,
            osc: OscScanner::default(),
        }
    }

    pub fn agent(&self) -> Option<HookAgent> {
        self.agent
    }

    pub fn status(&self) -> Option<AgentStatus> {
        self.status
    }

    fn set(&mut self, s: AgentStatus) -> bool {
        if self.status == Some(AgentStatus::Ended) || self.status == Some(s) {
            return false;
        }
        self.status = Some(s);
        true
    }

    /// A hook's report. Refused for Terminals whose agent reports none.
    pub fn on_event(&mut self, s: AgentStatus) -> Result<bool, String> {
        if self.agent.is_none() {
            return Err("not an agent terminal".into());
        }
        Ok(self.set(s))
    }

    /// Input typed into the Terminal (one write, as the Desktop sent it).
    ///
    /// A bare Esc or ^C interrupts a turn, and interrupts fire no `Stop` hook: finished.
    /// Anything else matters only for Codex: an answer key (`y`, `n`, `a`, a digit, Enter)
    /// clears needs you, and Enter starts a turn. Typing, Tab, Backspace and escape
    /// sequences (arrows, focus reports, cursor-position replies) change nothing.
    pub fn on_input(&mut self, data: &[u8]) -> bool {
        let Some(agent) = self.agent else {
            return false;
        };
        if data == b"\x1b" || data == b"\x03" {
            return matches!(
                self.status,
                Some(AgentStatus::Working | AgentStatus::NeedsYou)
            ) && self.set(AgentStatus::Finished);
        }
        if agent != HookAgent::Codex {
            return false;
        }
        match self.status {
            Some(AgentStatus::NeedsYou) if is_answer_key(data) => self.set(AgentStatus::Working),
            None | Some(AgentStatus::Finished)
                if !data.starts_with(b"\x1b") && data.contains(&b'\r') =>
            {
                self.set(AgentStatus::Working)
            }
            _ => false,
        }
    }

    /// Output the Terminal printed. Only Codex's is scanned, for OSC 9 notifications.
    pub fn on_output(&mut self, data: &[u8]) -> bool {
        if self.agent != Some(HookAgent::Codex) {
            return false;
        }
        self.osc.feed(data) && self.set(AgentStatus::NeedsYou)
    }

    /// The run's process ended.
    pub fn on_exit(&mut self) -> bool {
        self.agent.is_some() && self.set(AgentStatus::Ended)
    }
}

fn is_answer_key(data: &[u8]) -> bool {
    matches!(data, [b'y' | b'n' | b'a' | b'\r'] | [b'0'..=b'9'])
}

/// Longest OSC payload the scanner follows; a longer one is abandoned, so the scanner's
/// state never grows and a runaway sequence cannot hide later ones for long.
const OSC_MAX: usize = 512;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum OscState {
    #[default]
    Ground,
    Esc,
    Osc,
    /// An ESC inside an OSC: `\` ends it (ST).
    OscEsc,
}

/// Finds OSC 9 desktop notifications (`ESC ] 9 ; text BEL` or `… ESC \`) in a byte stream,
/// across chunk boundaries. ConEmu's OSC 9;4 progress reports are not notifications. Keeps
/// only the first bytes of a payload: no buffer, no growth.
#[derive(Debug, Clone, Default)]
struct OscScanner {
    state: OscState,
    prefix: [u8; 4],
    len: usize,
}

impl OscScanner {
    /// Whether `data` completed at least one notification.
    fn feed(&mut self, data: &[u8]) -> bool {
        let mut hit = false;
        for &b in data {
            self.state = match self.state {
                OscState::Ground if b == 0x1b => OscState::Esc,
                OscState::Ground => OscState::Ground,
                OscState::Esc => match b {
                    b']' => {
                        self.len = 0;
                        OscState::Osc
                    }
                    0x1b => OscState::Esc,
                    _ => OscState::Ground,
                },
                OscState::Osc => match b {
                    0x07 => {
                        hit |= self.is_notification();
                        OscState::Ground
                    }
                    0x1b => OscState::OscEsc,
                    _ if self.len >= OSC_MAX => OscState::Ground,
                    _ => {
                        if self.len < self.prefix.len() {
                            self.prefix[self.len] = b;
                        }
                        self.len += 1;
                        OscState::Osc
                    }
                },
                OscState::OscEsc => match b {
                    b'\\' => {
                        hit |= self.is_notification();
                        OscState::Ground
                    }
                    b']' => {
                        self.len = 0;
                        OscState::Osc
                    }
                    0x1b => OscState::Esc,
                    _ => OscState::Ground,
                },
            };
        }
        hit
    }

    fn is_notification(&self) -> bool {
        let p = &self.prefix[..self.len.min(self.prefix.len())];
        p.starts_with(b"9;") && !p.starts_with(b"9;4;")
    }
}

// ── What a launch adds ────────────────────────────────────────────────────

/// How a Host's agents report: the hook client executable, the event socket it sends to,
/// and the Claude Code settings file that names the hooks (written by its owner, the Daemon
/// or the Desktop, under its own private directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHooks {
    pub exe: PathBuf,
    pub endpoint: String,
    pub claude_settings: PathBuf,
}

/// The hooks for one Terminal's run: `terminal` is what the hook client reports under (the
/// Terminal's UUID on a Daemon, a per-Terminal token on the Desktop).
#[derive(Debug, Clone, Copy)]
pub struct TerminalHooks<'a> {
    pub hooks: &'a AgentHooks,
    pub terminal: Uuid,
    pub run: u64,
}

impl TerminalHooks<'_> {
    /// The value of [`TERMINAL_ID_ENV`].
    pub fn id_value(&self) -> String {
        format!("{}.{}", self.terminal, self.run)
    }

    /// The variables the agent's hooks read.
    pub fn env(&self) -> [(String, String); 2] {
        [
            (TERMINAL_ID_ENV.into(), self.id_value()),
            (EVENT_SOCKET_ENV.into(), self.hooks.endpoint.clone()),
        ]
    }
}

/// `'…'` for POSIX shells.
pub fn posix_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A path as one word of a hook command, which Claude Code runs through the shell: POSIX
/// single quotes on Unix, double quotes on Windows (where paths cannot contain `"`).
fn shell_word(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        posix_quote(&s)
    }
}

/// The hook command reporting `status`. `-` reads the Terminal from [`TERMINAL_ID_ENV`], so
/// the command expands no variables and reads the same in every shell.
pub fn hook_command(exe: &Path, status: AgentStatus) -> String {
    format!("{} event - {}", shell_word(exe), status.as_str())
}

/// Claude Code `Notification` types that block the turn on the user. `idle_prompt` (waiting
/// for the next prompt after a turn) is left out: that is finished, not needs you.
/// Verified against Claude Code 2.1.295.
pub const CLAUDE_NEEDS_YOU_MATCHER: &str =
    "permission_prompt|elicitation_dialog|elicitation_url_dialog";

/// The Claude Code settings passed with `--settings`: only hooks. The hook events and their
/// shape are verified against Claude Code 2.1.295.
pub fn claude_settings_json(exe: &Path) -> String {
    let cmd = |s: AgentStatus| serde_json::json!([{ "type": "command", "command": hook_command(exe, s), "timeout": 5 }]);
    let v = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [{ "hooks": cmd(AgentStatus::Working) }],
            // A tool ran: any Permission Prompt before it was answered.
            "PostToolUse": [{ "matcher": "*", "hooks": cmd(AgentStatus::Working) }],
            "Notification": [{ "matcher": CLAUDE_NEEDS_YOU_MATCHER, "hooks": cmd(AgentStatus::NeedsYou) }],
            "Stop": [{ "hooks": cmd(AgentStatus::Finished) }],
            "SessionEnd": [{ "hooks": cmd(AgentStatus::Ended) }],
        }
    });
    serde_json::to_string_pretty(&v).expect("settings serialize")
}

/// Codex TUI notifications that mean needs you, shown as OSC 9. Verified against
/// codex-cli 0.154.0.
pub const CODEX_NOTIFICATIONS: &[&str] = &["approval-requested", "plan-mode-prompt"];

impl AgentHooks {
    /// Write [`claude_settings_json`] to `claude_settings`, atomically and private (0600).
    /// The directory must exist.
    pub fn write_claude_settings(&self) -> io::Result<()> {
        let tmp = self
            .claude_settings
            .with_extension(format!("json.tmp.{}", std::process::id()));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let r = o.open(&tmp).and_then(|mut f| {
            f.write_all(claude_settings_json(&self.exe).as_bytes())?;
            f.sync_all()
        });
        let r = r.and_then(|_| std::fs::rename(&tmp, &self.claude_settings));
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r
    }

    /// Codex's `-c` overrides: `notify` runs the hook client after each turn (Codex appends
    /// its JSON payload as the last argument), and the TUI shows approvals and plan prompts
    /// as OSC 9 notifications, also while focused. Both replace the user's own `notify` and
    /// `tui.notifications` for this launch. A JSON array of strings is valid TOML.
    pub fn codex_overrides(&self) -> Vec<String> {
        let exe = self.exe.to_string_lossy().into_owned();
        let notify = serde_json::to_string(&[exe.as_str(), "event", "-", "finished"])
            .expect("strings serialize");
        let kinds = serde_json::to_string(CODEX_NOTIFICATIONS).expect("strings serialize");
        vec![
            "-c".into(),
            format!("notify={notify}"),
            "-c".into(),
            format!("tui.notifications={kinds}"),
            "-c".into(),
            "tui.notification_method=\"osc9\"".into(),
            "-c".into(),
            "tui.notification_condition=\"always\"".into(),
        ]
    }

    /// What a POSIX shell wrapper runs once the agent exits (the shell itself stays).
    pub fn ended_command_posix(&self) -> String {
        format!(
            "{} event - {}",
            posix_quote(&self.exe.to_string_lossy()),
            AgentStatus::Ended.as_str()
        )
    }

    /// The same for a `cmd /K` wrapper: arguments chained after the agent's with `&`, so cmd
    /// runs the report once the agent exits.
    pub fn ended_args_cmd(&self) -> Vec<String> {
        vec![
            "&".into(),
            self.exe.to_string_lossy().into_owned(),
            "event".into(),
            "-".into(),
            AgentStatus::Ended.as_str().into(),
        ]
    }

    /// The same for a PowerShell wrapper.
    pub fn ended_command_powershell(&self) -> String {
        format!(
            "& '{}' 'event' '-' '{}'",
            self.exe.to_string_lossy().replace('\'', "''"),
            AgentStatus::Ended.as_str()
        )
    }
}

// ── The hook client ───────────────────────────────────────────────────────

/// One parsed `event` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EventArgs {
    socket: Option<PathBuf>,
    verbose: bool,
    terminal: String,
    status: String,
    payload: Option<String>,
}

fn parse_event_args(args: &[OsString]) -> Result<EventArgs, String> {
    let mut socket = None;
    let mut verbose = false;
    let mut pos: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let s = a.to_string_lossy().into_owned();
        // Options anywhere: before, between or after the positionals. Codex's JSON payload
        // is one argument and never one of these.
        if s == "--socket" {
            socket = Some(PathBuf::from(it.next().ok_or("--socket needs a value")?));
        } else if let Some(v) = s.strip_prefix("--socket=") {
            socket = Some(PathBuf::from(v));
        } else if s == "-v" || s == "--verbose" {
            verbose = true;
        } else {
            pos.push(s);
        }
    }
    let mut pos = pos.into_iter();
    let (Some(terminal), Some(status)) = (pos.next(), pos.next()) else {
        return Err("usage: event <terminal|-> <status> [payload]".into());
    };
    Ok(EventArgs {
        socket,
        verbose,
        terminal,
        status,
        payload: pos.next(),
    })
}

/// `<uuid>.<run>` (or a bare `<uuid>`, run 0).
fn parse_terminal_id(s: &str) -> Result<(Uuid, u64), String> {
    let (id, run) = match s.rsplit_once('.') {
        Some((id, run)) => (
            id,
            run.parse::<u64>()
                .map_err(|_| format!("bad terminal id {s:?}"))?,
        ),
        None => (s, 0),
    };
    let id = id
        .parse::<Uuid>()
        .map_err(|_| format!("bad terminal id {s:?}"))?;
    Ok((id, run))
}

/// What Codex's `notify` payload says: `None` when it is not a turn end (nothing is sent),
/// else the session it names. Codex sends only the turn-end type today; other types and
/// other JSON are ignored, so a future one never reads as finished. Text that is not JSON
/// is no payload: a turn end with no session.
fn payload_turn_end(payload: &str) -> Option<Option<String>> {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(v) if v.get("type").and_then(|t| t.as_str()) == Some("agent-turn-complete") => {
            Some(payload_thread(&v))
        }
        Ok(_) => None,
        Err(_) => Some(None),
    }
}

/// The session a turn-end payload names: its `thread-id`, when that is a valid session id
/// of at most [`SESSION_ID_MAX`] characters. Anything else is dropped here, so the Daemon
/// never sees an id that could look like an option or a path.
fn payload_thread(v: &serde_json::Value) -> Option<String> {
    v.get("thread-id")
        .and_then(|t| t.as_str())
        .filter(|s| s.len() <= SESSION_ID_MAX && valid_session_id(s))
        .map(str::to_owned)
}

/// One `term.event` to send.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedEvent {
    socket: PathBuf,
    terminal: Uuid,
    run: u64,
    status: AgentStatus,
    session: Option<String>,
}

/// What one `event` command does: `Ok(None)` when there is nothing to send.
fn plan_event(
    a: &EventArgs,
    env: &dyn Fn(&str) -> Option<OsString>,
    default_socket: Option<PathBuf>,
) -> Result<Option<PlannedEvent>, String> {
    let session = match a.payload.as_deref().map(payload_turn_end) {
        Some(None) => return Ok(None),
        Some(Some(s)) => s,
        None => None,
    };
    let status =
        AgentStatus::parse(&a.status).ok_or_else(|| format!("unknown status {:?}", a.status))?;
    let id = if a.terminal == "-" {
        env(TERMINAL_ID_ENV)
            .ok_or_else(|| format!("{TERMINAL_ID_ENV} is not set"))?
            .to_string_lossy()
            .into_owned()
    } else {
        a.terminal.clone()
    };
    let (terminal, run) = parse_terminal_id(&id)?;
    let socket = a
        .socket
        .clone()
        .or_else(|| env(EVENT_SOCKET_ENV).map(PathBuf::from))
        .or(default_socket)
        .ok_or_else(|| format!("{EVENT_SOCKET_ENV} is not set"))?;
    Ok(Some(PlannedEvent {
        socket,
        terminal,
        run,
        status,
        session,
    }))
}

/// How long the hook client may take in all, connecting included. A hook must never hold
/// the agent up for long: Claude Code waits for it.
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
const CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Send one `term.event` and wait for its `res`, all within [`CLIENT_TIMEOUT`]. `session`
/// is the agent's own session id, when its hook reported one.
#[cfg(unix)]
pub fn send_event(
    socket: &Path,
    terminal: Uuid,
    run: u64,
    status: AgentStatus,
    session: Option<String>,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + CLIENT_TIMEOUT;
    let mut s = deadline_io::connect(socket, deadline)
        .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
    exchange(&mut s, terminal, run, status, session, Some(deadline))
}

/// A unix stream bound by an absolute deadline: a non-blocking connect, then reads and
/// writes that wait with `poll` only for the time left. No socket timeouts to set (or fail).
#[cfg(unix)]
mod deadline_io {
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Retry interval while the listener's backlog is full (nothing to poll on).
    const BACKLOG_RETRY: Duration = Duration::from_millis(20);

    pub struct DeadlineStream {
        s: UnixStream,
        deadline: Instant,
    }

    fn timed_out() -> io::Error {
        io::Error::from(io::ErrorKind::TimedOut)
    }

    /// Wait until `fd` is ready for `events`, or the deadline passes.
    fn wait(fd: i32, events: libc::c_short, deadline: Instant) -> io::Result<()> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timed_out());
            }
            let mut p = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            let ms = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
            match unsafe { libc::poll(&mut p, 1, ms) } {
                0 => return Err(timed_out()),
                n if n > 0 => return Ok(()),
                _ => {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// A close-on-exec, non-blocking unix stream socket: atomically where the platform
    /// allows it, otherwise through checked `fcntl` calls.
    fn socket() -> io::Result<OwnedFd> {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        {
            let ty = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
            let fd = unsafe { libc::socket(libc::AF_UNIX, ty, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        )))]
        {
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let raw = fd.as_raw_fd();
            let check = |r: libc::c_int| {
                if r < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(r)
                }
            };
            check(unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) })?;
            let fl = check(unsafe { libc::fcntl(raw, libc::F_GETFL) })?;
            check(unsafe { libc::fcntl(raw, libc::F_SETFL, fl | libc::O_NONBLOCK) })?;
            Ok(fd)
        }
    }

    /// Connect to `path` before `deadline`. The stream stays non-blocking.
    pub fn connect(path: &Path, deadline: Instant) -> io::Result<DeadlineStream> {
        let bytes = path.as_os_str().as_bytes();
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path too long",
            ));
        }
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
            *d = *s as libc::c_char;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        loop {
            let fd = socket()?;
            let raw = fd.as_raw_fd();
            let r = unsafe { libc::connect(raw, &addr as *const _ as *const libc::sockaddr, len) };
            if r == 0 {
                return Ok(DeadlineStream {
                    s: UnixStream::from(fd),
                    deadline,
                });
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // Linux: the backlog is full. Nothing to wait on but time.
                Some(libc::EAGAIN) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(timed_out());
                    }
                    std::thread::sleep(left.min(BACKLOG_RETRY));
                }
                Some(libc::EINPROGRESS) | Some(libc::EINTR) => {
                    wait(raw, libc::POLLOUT, deadline)?;
                    let mut err: libc::c_int = 0;
                    let mut l = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                    let r = unsafe {
                        libc::getsockopt(
                            raw,
                            libc::SOL_SOCKET,
                            libc::SO_ERROR,
                            &mut err as *mut _ as *mut libc::c_void,
                            &mut l,
                        )
                    };
                    if r < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if err != 0 {
                        return Err(io::Error::from_raw_os_error(err));
                    }
                    return Ok(DeadlineStream {
                        s: UnixStream::from(fd),
                        deadline,
                    });
                }
                _ => return Err(e),
            }
        }
    }

    impl Read for DeadlineStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                match self.s.read(buf) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        wait(self.s.as_raw_fd(), libc::POLLIN, self.deadline)?
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    r => return r,
                }
            }
        }
    }

    impl Write for DeadlineStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            loop {
                match self.s.write(buf) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        wait(self.s.as_raw_fd(), libc::POLLOUT, self.deadline)?
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    r => return r,
                }
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

/// Send one `term.event` and wait for its `res`, all within [`CLIENT_TIMEOUT`]: over the
/// Daemon's named pipe, every read and write waiting only for the time left.
#[cfg(windows)]
pub fn send_event(
    socket: &Path,
    terminal: Uuid,
    run: u64,
    status: AgentStatus,
    session: Option<String>,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + CLIENT_TIMEOUT;
    let s = crate::pipe::connect(socket, deadline, &|| false)
        .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
    exchange(
        &mut PipeDeadline { s, deadline },
        terminal,
        run,
        status,
        session,
        Some(deadline),
    )
}

/// A pipe bound by an absolute deadline: each operation waits only for the time left.
#[cfg(windows)]
struct PipeDeadline {
    s: crate::pipe::PipeStream,
    deadline: std::time::Instant,
}

#[cfg(windows)]
impl PipeDeadline {
    fn left(&self) -> io::Result<std::time::Duration> {
        let left = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        Ok(left)
    }
}

#[cfg(windows)]
impl Read for PipeDeadline {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.left()?;
        self.s.read_within(buf, Some(left))
    }
}

#[cfg(windows)]
impl Write for PipeDeadline {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let left = self.left()?;
        self.s.write_within(buf, Some(left))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
pub fn send_event(
    socket: &Path,
    _: Uuid,
    _: u64,
    _: AgentStatus,
    _: Option<String>,
) -> Result<(), String> {
    Err(format!(
        "no event socket on this platform ({})",
        socket.display()
    ))
}

/// The client side of one event connection: hello, `term.event` with id 1, then frames
/// until its `res`.
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
fn exchange(
    s: &mut (impl Read + Write),
    terminal: Uuid,
    run: u64,
    status: AgentStatus,
    session_id: Option<String>,
    deadline: Option<std::time::Instant>,
) -> Result<(), String> {
    use xshell_protocol::msg::{decode_server, ServerMsg};
    let hello = ClientMsg::Hello(Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: vec![],
    });
    let ev = ClientMsg::TermEvent {
        terminal,
        run,
        status,
        session_id,
    };
    let mut out = encode_msg(&hello, None).map_err(|e| e.to_string())?;
    out.extend(encode_msg(&ev, Some(1)).map_err(|e| e.to_string())?);
    s.write_all(&out)
        .and_then(|_| s.flush())
        .map_err(|e| e.to_string())?;
    loop {
        // A peer that keeps sending other frames never holds the hook past its deadline.
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return Err("no answer in time".into());
        }
        match read_frame(s, MAX_FRAME_LEN).map_err(|e| e.to_string())? {
            None => return Err("connection closed before the answer".into()),
            Some(Frame::Json(j)) => match decode_server(&j) {
                Ok(ServerMsg::Res(r)) if r.id == 1 => return r.outcome.into_result().map(drop),
                Ok(ServerMsg::Error { message, .. }) => return Err(message),
                _ => {}
            },
            Some(_) => {}
        }
    }
}

/// The hook client: `event <terminal|-> <status> [payload] [--socket PATH] [-v]`.
///
/// `-` reads `<terminal>.<run>` from [`TERMINAL_ID_ENV`]; the socket comes from `--socket`,
/// [`EVENT_SOCKET_ENV`], then `default_socket`. A Codex payload that is not a turn end sends
/// nothing. Never reads stdin, gives up within seconds, and always exits 0: a failing
/// Claude Code `Stop` hook (exit 2) would keep the turn from ending. `-v` prints why
/// nothing was recorded.
pub fn event_main(
    args: &[OsString],
    env: &dyn Fn(&str) -> Option<OsString>,
    default_socket: Option<PathBuf>,
) -> i32 {
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    let r = parse_event_args(args).and_then(|a| match plan_event(&a, env, default_socket)? {
        Some(e) => send_event(&e.socket, e.terminal, e.run, e.status, e.session),
        None => Ok(()),
    });
    if let Err(e) = r {
        if verbose {
            eprintln!("xshell event: {e}");
        }
    }
    0
}

// ── The Desktop's event socket ────────────────────────────────────────────

/// Serve one event-socket connection: hello both ways, then answer each `term.event` with
/// what `on` says. Any other message is refused; the connection ends at EOF or a malformed
/// frame.
pub fn serve_event_conn(
    mut s: impl Read + Write,
    version: &str,
    on: impl Fn(Uuid, u64, AgentStatus) -> Result<(), String>,
) {
    let hello = xshell_protocol::msg::ServerMsg::Hello(Hello {
        protocol: PROTOCOL,
        version: version.into(),
        capabilities: vec!["agent.status".into()],
    });
    let Ok(f) = encode_msg(&hello, None) else {
        return;
    };
    if s.write_all(&f).is_err() {
        return;
    }
    let theirs = match read_frame(&mut s, MAX_FRAME_LEN) {
        Ok(Some(Frame::Json(j))) => match decode_inbound(&j) {
            Ok(xshell_protocol::msg::Inbound {
                msg: ClientMsg::Hello(h),
                ..
            }) => h,
            _ => return,
        },
        _ => return,
    };
    if negotiate(PROTOCOL, theirs.protocol).is_err() {
        return;
    }
    loop {
        let j = match read_frame(&mut s, MAX_FRAME_LEN) {
            Ok(Some(Frame::Json(j))) => j,
            Ok(Some(_)) => continue,
            _ => return,
        };
        let (id, r) = match decode_inbound(&j) {
            Ok(m) => match m.msg {
                ClientMsg::TermEvent {
                    terminal,
                    run,
                    status,
                    ..
                } => (m.id, on(terminal, run, status)),
                _ => (m.id, Err("only term.event is served here".to_string())),
            },
            Err(DecodeError::Malformed(_)) => return,
            Err(e @ (DecodeError::UnknownType { id, .. } | DecodeError::Invalid { id, .. })) => {
                (id, Err(e.to_string()))
            }
        };
        if let Some(id) = id {
            if s.write_all(&encode_res(id, r.map(|_| serde_json::Value::Null)))
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use AgentStatus::*;

    fn spec(agent: &str) -> LaunchSpec {
        LaunchSpec {
            agent: Some(agent.into()),
            cwd: "/w".into(),
            ..Default::default()
        }
    }

    fn claude() -> Tracker {
        Tracker::new(&spec("claude"))
    }

    fn codex() -> Tracker {
        Tracker::new(&spec("codex"))
    }

    #[test]
    fn tracker_claude_transitions() {
        let mut t = claude();
        assert_eq!(t.agent(), Some(HookAgent::Claude));
        assert_eq!(t.status(), None);
        // (report, changed, status after)
        let steps = [
            (Working, true, Working),
            (Working, false, Working),
            (NeedsYou, true, NeedsYou),
            (Working, true, Working),
            (Finished, true, Finished),
            (Working, true, Working),
            (Finished, true, Finished),
            (Ended, true, Ended),
        ];
        for (i, (s, changed, after)) in steps.into_iter().enumerate() {
            assert_eq!(t.on_event(s), Ok(changed), "step {i}");
            assert_eq!(t.status(), Some(after), "step {i}");
        }
    }

    #[test]
    fn tracker_ended_is_final() {
        for mut t in [claude(), codex()] {
            assert!(t.on_exit());
            assert!(!t.on_exit());
            for s in [Working, NeedsYou, Finished] {
                assert_eq!(t.on_event(s), Ok(false));
            }
            assert!(!t.on_input(b"\r"));
            assert!(!t.on_input(b"\x1b"));
            assert!(!t.on_output(b"\x1b]9;Approval requested: ls\x07"));
            assert_eq!(t.status(), Some(Ended));
        }
    }

    #[test]
    fn input_does_not_clear_claude_needs_you() {
        // Only hooks clear it for Claude: PostToolUse (the tool ran) or the next prompt.
        let mut t = claude();
        t.on_event(NeedsYou).unwrap();
        for input in [&b"1"[..], b"y", b"\r", b"abc", b"\x7f", b"\t"] {
            assert!(!t.on_input(input), "{input:?}");
        }
        assert_eq!(t.status(), Some(NeedsYou));
        // PostToolUse reports working.
        assert_eq!(t.on_event(Working), Ok(true));
    }

    #[test]
    fn input_answer_clears_codex_needs_you() {
        for key in [&b"y"[..], b"n", b"a", b"1", b"9", b"\r"] {
            let mut t = codex();
            assert!(t.on_output(b"\x1b]9;Approval requested: ls\x07"));
            assert!(t.on_input(key), "{key:?}");
            assert_eq!(t.status(), Some(Working));
        }
        // Text editing, Tab and Backspace leave it.
        let mut t = codex();
        t.on_output(b"\x1b]9;Approval requested: ls\x07");
        for input in [&b"h"[..], b"yes", b"\t", b"\x7f", b"Y"] {
            assert!(!t.on_input(input), "{input:?}");
        }
        assert_eq!(t.status(), Some(NeedsYou));
    }

    #[test]
    fn escape_led_input_does_not_clear_needs_you() {
        let focus = b"\x1b[I";
        let cpr = b"\x1b[12;40R";
        let arrows = [&b"\x1b[A"[..], b"\x1b[B", b"\x1bOA"];
        for mut t in [claude(), codex()] {
            if t.agent() == Some(HookAgent::Claude) {
                t.on_event(NeedsYou).unwrap();
            } else {
                t.on_output(b"\x1b]9;x\x07");
            }
            assert!(!t.on_input(focus));
            assert!(!t.on_input(cpr));
            for a in arrows {
                assert!(!t.on_input(a));
            }
            assert_eq!(t.status(), Some(NeedsYou));
        }
    }

    #[test]
    fn bare_esc_or_ctrl_c_finishes_turn() {
        for key in [&b"\x1b"[..], b"\x03"] {
            for start in [Working, NeedsYou] {
                for mut t in [claude(), codex()] {
                    t.on_event(start).unwrap();
                    assert!(t.on_input(key));
                    assert_eq!(t.status(), Some(Finished));
                }
            }
            // Nothing to interrupt: no status appears.
            let mut t = claude();
            assert!(!t.on_input(key));
            assert_eq!(t.status(), None);
        }
    }

    #[test]
    fn codex_enter_sets_working() {
        let mut t = codex();
        assert!(!t.on_input(b"hello"));
        assert!(t.on_input(b"\r"));
        assert_eq!(t.status(), Some(Working));
        assert!(!t.on_input(b"\r"));
        t.on_event(Finished).unwrap();
        assert!(t.on_input(b"next\r"));
        assert_eq!(t.status(), Some(Working));
        // Claude has a prompt hook: Enter means nothing by itself.
        let mut c = claude();
        assert!(!c.on_input(b"\r"));
        assert_eq!(c.status(), None);
    }

    #[test]
    fn osc9_sets_needs_you_for_codex_only() {
        let n = b"text \x1b]9;Approval requested: ls -la\x07 more";
        let mut t = codex();
        assert!(t.on_output(n));
        assert_eq!(t.status(), Some(NeedsYou));
        // ST-terminated too.
        let mut t = codex();
        assert!(t.on_output(b"\x1b]9;Codex wants to edit a.rs\x1b\\"));
        let mut c = claude();
        assert!(!c.on_output(n));
        assert_eq!(c.status(), None);
        // Other OSCs are not notifications.
        let mut t = codex();
        for o in [
            &b"\x1b]0;title\x07"[..],
            b"\x1b]99;x\x07",
            b"\x1b]777;notify;a;b\x07",
            b"\x1b]8;;http://x\x07",
        ] {
            assert!(!t.on_output(o), "{o:?}");
        }
        assert_eq!(t.status(), None);
    }

    #[test]
    fn osc9_split_across_chunks() {
        let seq = b"ab\x1b]9;Approval requested: ls\x1b\\cd";
        for cut in 1..seq.len() {
            let mut t = codex();
            let first = t.on_output(&seq[..cut]);
            let second = t.on_output(&seq[cut..]);
            assert!(first ^ second, "cut at {cut}");
            assert_eq!(t.status(), Some(NeedsYou), "cut at {cut}");
        }
        // Byte by byte.
        let mut t = codex();
        let hits: usize = seq.iter().map(|b| usize::from(t.on_output(&[*b]))).sum();
        assert_eq!(hits, 1);
    }

    #[test]
    fn osc9_progress_ignored() {
        let mut t = codex();
        assert!(!t.on_output(b"\x1b]9;4;1;50\x07\x1b]9;4;0;\x1b\\"));
        assert_eq!(t.status(), None);
    }

    #[test]
    fn osc_scanner_bounded() {
        let mut s = OscScanner::default();
        let mut long = b"\x1b]9;".to_vec();
        long.extend(std::iter::repeat_n(b'x', 100_000));
        long.push(0x07);
        // Abandoned past OSC_MAX: never a hit, and the scanner holds no payload.
        assert!(!s.feed(&long));
        assert_eq!(s.state, OscState::Ground);
        assert!(std::mem::size_of::<OscScanner>() <= 32);
        // At the limit it still counts.
        let mut edge = b"\x1b]9;".to_vec();
        edge.extend(std::iter::repeat_n(b'x', OSC_MAX - 2));
        edge.push(0x07);
        assert!(s.feed(&edge));
        assert!(s.feed(b"\x1b]9;again\x07"));
    }

    #[test]
    fn untracked_agents_reject_events() {
        let raw = LaunchSpec {
            shell_mode: Some("raw".into()),
            ..spec("claude")
        };
        for s in [raw, spec("cursor"), spec("opencode"), spec("antigravity")] {
            assert_eq!(hook_agent(&s), None, "{s:?}");
            let mut t = Tracker::new(&s);
            assert_eq!(t.on_event(Working), Err("not an agent terminal".into()));
            assert!(!t.on_input(b"\x1b"));
            assert!(!t.on_output(b"\x1b]9;x\x07"));
            assert!(!t.on_exit());
            assert_eq!(t.status(), None);
        }
        // Claude is the default agent.
        assert_eq!(hook_agent(&LaunchSpec::default()), Some(HookAgent::Claude));
    }

    fn hooks(exe: &str) -> AgentHooks {
        AgentHooks {
            exe: exe.into(),
            endpoint: "/run/xshell/daemon.sock".into(),
            claude_settings: "/h/.xshell/daemon/claude-hooks.json".into(),
        }
    }

    /// The settings shape Claude Code 2.1.295 reads: `hooks.<Event>[].{matcher?, hooks[]}`
    /// with `{type: "command", command, timeout}`.
    #[cfg(unix)]
    #[test]
    fn claude_settings_json_golden() {
        let v: serde_json::Value =
            serde_json::from_str(&claude_settings_json(Path::new("/opt/xshell/xshelld"))).unwrap();
        let cmd = |s: &str| serde_json::json!([{"type":"command","command":format!("'/opt/xshell/xshelld' event - {s}"),"timeout":5}]);
        assert_eq!(
            v,
            serde_json::json!({"hooks":{
                "UserPromptSubmit":[{"hooks":cmd("working")}],
                "PostToolUse":[{"matcher":"*","hooks":cmd("working")}],
                "Notification":[{"matcher":"permission_prompt|elicitation_dialog|elicitation_url_dialog",
                    "hooks":cmd("needs-you")}],
                "Stop":[{"hooks":cmd("finished")}],
                "SessionEnd":[{"hooks":cmd("ended")}],
            }})
        );
        // Only hooks: nothing else of the user's settings is overridden.
        assert_eq!(v.as_object().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn hook_command_quotes_path_with_spaces() {
        assert_eq!(
            hook_command(Path::new("/Apps/My xshell/it's"), NeedsYou),
            "'/Apps/My xshell/it'\\''s' event - needs-you"
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "set -- {}; printf '%s|' \"$@\"",
                posix_quote("/Apps/My xshell/it's")
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "/Apps/My xshell/it's|"
        );
        assert_eq!(
            hooks("/a b/x'y").ended_command_posix(),
            "'/a b/x'\\''y' event - ended"
        );
        assert_eq!(
            hooks("/a b/x'y").ended_command_powershell(),
            "& '/a b/x''y' 'event' '-' 'ended'"
        );
    }

    /// The overrides are what codex-cli 0.154.0 parses: `-c key=value`, the value TOML.
    #[test]
    fn codex_notify_override_is_json_and_toml() {
        let h = hooks("/opt/x \"q\" \\b/xshelld");
        let o = h.codex_overrides();
        assert_eq!(o.len(), 8);
        let mut table = toml::Table::new();
        for pair in o.chunks(2) {
            assert_eq!(pair[0], "-c");
            let (k, v) = pair[1].split_once('=').unwrap();
            let doc: toml::Table = toml::from_str(&format!("v = {v}")).unwrap();
            table.insert(k.to_string(), doc["v"].clone());
        }
        let strs = |k: &str| -> Vec<String> {
            table[k]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            strs("notify"),
            vec!["/opt/x \"q\" \\b/xshelld", "event", "-", "finished"]
        );
        // The same value is JSON.
        let json: Vec<String> =
            serde_json::from_str(o[1].strip_prefix("notify=").unwrap()).unwrap();
        assert_eq!(json, strs("notify"));
        assert_eq!(
            strs("tui.notifications"),
            vec!["approval-requested", "plan-mode-prompt"]
        );
        assert_eq!(table["tui.notification_method"].as_str(), Some("osc9"));
        assert_eq!(table["tui.notification_condition"].as_str(), Some("always"));
    }

    fn envmap(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn event_main_payload_filter() {
        let id = Uuid::new_v4();
        let env = envmap(&[
            (TERMINAL_ID_ENV, &format!("{id}.7")),
            (EVENT_SOCKET_ENV, "/s.sock"),
        ]);
        let plan = |v: &[&str]| plan_event(&parse_event_args(&args(v)).unwrap(), &env, None);
        let ev = |socket: &str, run, status, session: Option<&str>| {
            Some(PlannedEvent {
                socket: socket.into(),
                terminal: id,
                run,
                status,
                session: session.map(str::to_owned),
            })
        };
        let want = ev("/s.sock", 7, Finished, None);
        assert_eq!(plan(&["-", "finished"]), Ok(want.clone()));
        assert_eq!(
            plan(&[
                "-",
                "finished",
                r#"{"type":"agent-turn-complete","thread-id":"t"}"#
            ]),
            Ok(ev("/s.sock", 7, Finished, Some("t")))
        );
        assert_eq!(
            plan(&["-", "finished", r#"{"type":"something-else"}"#]),
            Ok(None)
        );
        assert_eq!(plan(&["-", "finished", "{}"]), Ok(None));
        assert_eq!(plan(&["-", "finished", "not json"]), Ok(want));
        // Explicit id and socket; a bare UUID is run 0.
        assert_eq!(
            plan(&["--socket", "/o.sock", &id.to_string(), "working"]),
            Ok(ev("/o.sock", 0, Working, None))
        );
        assert_eq!(
            plan_event(
                &parse_event_args(&args(&["-", "ended"])).unwrap(),
                &envmap(&[(TERMINAL_ID_ENV, &format!("{id}.1"))]),
                Some("/d.sock".into())
            ),
            Ok(ev("/d.sock", 1, Ended, None))
        );
        assert!(plan(&["-", "bogus"]).is_err());
        assert!(plan(&["nope", "working"]).is_err());
        assert!(plan_event(
            &parse_event_args(&args(&["-", "working"])).unwrap(),
            &envmap(&[]),
            None
        )
        .is_err());
        assert!(parse_event_args(&args(&["-"])).is_err());
    }

    #[test]
    fn plan_event_extracts_thread_id() {
        let id = Uuid::new_v4();
        let env = envmap(&[
            (TERMINAL_ID_ENV, &format!("{id}.2")),
            (EVENT_SOCKET_ENV, "/s.sock"),
        ]);
        let session = |payload: &str| {
            plan_event(
                &parse_event_args(&args(&["-", "finished", payload])).unwrap(),
                &env,
                None,
            )
            .map(|p| p.map(|p| p.session))
        };
        let turn = |tid: serde_json::Value| {
            serde_json::json!({"type": "agent-turn-complete", "thread-id": tid, "cwd": "/p"})
                .to_string()
        };
        // Valid ids are carried.
        for ok in ["019a2b3c-dead-beef", "a", "A_b-9", &"x".repeat(200)] {
            assert_eq!(
                session(&turn(ok.into())),
                Ok(Some(Some(ok.to_string()))),
                "{ok}"
            );
        }
        // Bad ids are dropped, the status is still sent.
        for bad in [
            serde_json::json!("../x"),
            serde_json::json!("-x"),
            serde_json::json!(""),
            serde_json::json!("a/b"),
            serde_json::json!("x".repeat(201)),
            serde_json::json!(42),
            serde_json::Value::Null,
        ] {
            assert_eq!(session(&turn(bad.clone())), Ok(Some(None)), "{bad}");
        }
        assert_eq!(session(r#"{"type":"agent-turn-complete"}"#), Ok(Some(None)));
        // A payload that is not a turn end sends nothing, even with a thread id.
        assert_eq!(session(r#"{"type":"other","thread-id":"abc"}"#), Ok(None));
        // Text that is not JSON: a status with no session.
        assert_eq!(session("thread-id abc"), Ok(Some(None)));
    }

    #[cfg(unix)]
    #[test]
    fn event_main_unreachable_socket_exits_zero_fast() {
        let dir = tempfile::tempdir().unwrap();
        let env = envmap(&[
            (TERMINAL_ID_ENV, &format!("{}.1", Uuid::new_v4())),
            (
                EVENT_SOCKET_ENV,
                &dir.path().join("none.sock").to_string_lossy(),
            ),
        ]);
        let t = std::time::Instant::now();
        assert_eq!(event_main(&args(&["-", "working"]), &env, None), 0);
        assert_eq!(event_main(&args(&["garbage"]), &env, None), 0);
        assert_eq!(event_main(&[], &envmap(&[]), None), 0);
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
        // A listener that never answers: given up after the client timeout.
        let sock = dir.path().join("mute.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let id = Uuid::new_v4();
        let t = std::time::Instant::now();
        assert_eq!(
            event_main(
                &args(&[
                    "--socket",
                    &sock.to_string_lossy(),
                    &id.to_string(),
                    "working"
                ]),
                &envmap(&[]),
                None
            ),
            0
        );
        assert!(
            t.elapsed() < CLIENT_TIMEOUT + Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
    }

    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
    fn event_to(sock: &Path) -> Duration {
        let id = Uuid::new_v4();
        let t = std::time::Instant::now();
        let a = args(&[
            "--socket",
            &sock.to_string_lossy(),
            &id.to_string(),
            "working",
        ]);
        assert_eq!(event_main(&a, &envmap(&[]), None), 0);
        t.elapsed()
    }

    /// The listener's backlog is full: the connect itself waits, within the deadline.
    #[cfg(target_os = "linux")]
    #[test]
    fn event_client_deadline_covers_a_full_backlog() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("full.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        // The smallest backlog, never accepted from.
        assert_eq!(unsafe { libc::listen(l.as_raw_fd(), 0) }, 0);
        let mut held = Vec::new();
        loop {
            let soon = std::time::Instant::now() + Duration::from_millis(50);
            match deadline_io::connect(&sock, soon) {
                Ok(s) => held.push(s),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(e) => panic!("{e}"),
            }
            assert!(held.len() < 512, "the backlog never filled");
        }
        let took = event_to(&sock);
        assert!(
            took >= CLIENT_TIMEOUT - Duration::from_millis(100),
            "{took:?}"
        );
        assert!(
            took < CLIENT_TIMEOUT + Duration::from_millis(500),
            "{took:?}"
        );
    }

    /// A peer that answers hello and then trickles other frames, never the `res`.
    #[cfg(unix)]
    #[test]
    fn event_client_deadline_covers_a_trickling_peer() {
        use xshell_protocol::msg::ServerMsg;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("trickle.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = l.accept() else { return };
            let hello = ServerMsg::Hello(Hello {
                protocol: PROTOCOL,
                version: "x".into(),
                capabilities: vec![],
            });
            let other = ServerMsg::Terminals { list: vec![] };
            if s.write_all(&encode_msg(&hello, None).unwrap()).is_err() {
                return;
            }
            let frame = encode_msg(&other, None).unwrap();
            loop {
                // Byte by byte, so the client is always mid-frame.
                for b in &frame {
                    if s.write_all(&[*b]).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        let took = event_to(&sock);
        assert!(
            took >= CLIENT_TIMEOUT - Duration::from_millis(100),
            "{took:?}"
        );
        assert!(
            took < CLIENT_TIMEOUT + Duration::from_millis(500),
            "{took:?}"
        );
    }

    #[test]
    fn event_args_options_after_positionals() {
        let p = |v: &[&str]| parse_event_args(&args(v)).unwrap();
        let want = EventArgs {
            socket: Some("/s.sock".into()),
            verbose: true,
            terminal: "-".into(),
            status: "finished".into(),
            payload: None,
        };
        assert_eq!(p(&["-", "finished", "--socket", "/s.sock", "-v"]), want);
        assert_eq!(p(&["-", "--socket=/s.sock", "finished", "--verbose"]), want);
        assert_eq!(p(&["-v", "--socket", "/s.sock", "-", "finished"]), want);
        // Codex's JSON payload stays one positional, options before or after it.
        let json = r#"{"type":"agent-turn-complete","input-messages":["a --socket b"]}"#;
        let with = EventArgs {
            payload: Some(json.into()),
            ..want.clone()
        };
        assert_eq!(
            p(&["-", "finished", json, "--socket", "/s.sock", "-v"]),
            with
        );
        assert_eq!(p(&["-", "finished", "--socket=/s.sock", json, "-v"]), with);
        assert!(parse_event_args(&args(&["-", "finished", "--socket"])).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn event_roundtrip_over_unix_pair() {
        use std::os::unix::net::UnixStream;
        use std::sync::{Arc, Mutex};
        let (mut a, b) = UnixStream::pair().unwrap();
        let got: Arc<Mutex<Vec<(Uuid, u64, AgentStatus)>>> = Arc::default();
        let g = got.clone();
        let known = Uuid::new_v4();
        let server = std::thread::spawn(move || {
            serve_event_conn(b, "1.0", move |t, run, s| {
                g.lock().unwrap().push((t, run, s));
                if t == known {
                    Ok(())
                } else {
                    Err(format!("unknown terminal {t}"))
                }
            })
        });
        assert_eq!(exchange(&mut a, known, 4, NeedsYou, None, None), Ok(()));
        drop(a);
        server.join().unwrap();
        // A second connection, for an unknown Terminal.
        let (mut a, b) = UnixStream::pair().unwrap();
        let g = got.clone();
        let server = std::thread::spawn(move || {
            serve_event_conn(b, "1.0", move |t, run, s| {
                g.lock().unwrap().push((t, run, s));
                Err("unknown terminal".to_string())
            })
        });
        let other = Uuid::new_v4();
        assert_eq!(
            exchange(&mut a, other, 1, Working, None, None),
            Err("unknown terminal".into())
        );
        drop(a);
        server.join().unwrap();
        assert_eq!(
            *got.lock().unwrap(),
            vec![(known, 4, NeedsYou), (other, 1, Working)]
        );
    }

    // ── The hook client over a named pipe (Windows) ──

    #[cfg(windows)]
    fn unique_pipe() -> PathBuf {
        PathBuf::from(format!(r"\\.\pipe\xshell-test-{}", Uuid::new_v4().simple()))
    }

    #[cfg(windows)]
    fn event_over(pipe: &Path) -> std::time::Duration {
        let id = Uuid::new_v4();
        let t = std::time::Instant::now();
        let a = args(&[
            "--socket",
            &pipe.to_string_lossy(),
            &id.to_string(),
            "working",
        ]);
        assert_eq!(event_main(&a, &envmap(&[]), None), 0);
        t.elapsed()
    }

    #[cfg(windows)]
    #[test]
    fn send_event_over_pipe() {
        use std::sync::{Arc, Mutex};
        let pipe = unique_pipe();
        let l = crate::pipe::PipeListener::bind(&pipe).unwrap();
        let got: Arc<Mutex<Vec<(Uuid, u64, AgentStatus)>>> = Arc::default();
        let g = got.clone();
        let known = Uuid::new_v4();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let s = l.accept().unwrap();
                let g = g.clone();
                serve_event_conn(s, "1.0", move |t, run, st| {
                    g.lock().unwrap().push((t, run, st));
                    if t == known {
                        Ok(())
                    } else {
                        Err("unknown terminal".into())
                    }
                });
            }
        });
        assert_eq!(send_event(&pipe, known, 4, NeedsYou, None), Ok(()));
        let other = Uuid::new_v4();
        assert_eq!(
            send_event(&pipe, other, 1, Working, None),
            Err("unknown terminal".into())
        );
        server.join().unwrap();
        assert_eq!(
            *got.lock().unwrap(),
            vec![(known, 4, NeedsYou), (other, 1, Working)]
        );
        // Nobody serves the name any more: the client gives up at once, exit 0.
        let t = std::time::Instant::now();
        assert_eq!(
            event_main(
                &args(&[
                    "--socket",
                    &pipe.to_string_lossy(),
                    &known.to_string(),
                    "working"
                ]),
                &envmap(&[]),
                None
            ),
            0
        );
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
    }

    /// A server that accepts and never answers: the first frame never completes.
    #[cfg(windows)]
    #[test]
    fn event_client_deadline_covers_a_mute_pipe() {
        let pipe = unique_pipe();
        let l = crate::pipe::PipeListener::bind(&pipe).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let s = l.accept().unwrap();
            let _ = rx.recv();
            drop(s);
        });
        let took = event_over(&pipe);
        let _ = tx.send(());
        assert!(
            took >= CLIENT_TIMEOUT - std::time::Duration::from_millis(100),
            "{took:?}"
        );
        assert!(
            took < CLIENT_TIMEOUT + std::time::Duration::from_millis(500),
            "{took:?}"
        );
    }

    /// A peer that answers hello and then trickles other frames, never the `res`.
    #[cfg(windows)]
    #[test]
    fn event_client_deadline_covers_a_trickling_pipe() {
        use xshell_protocol::msg::ServerMsg;
        let pipe = unique_pipe();
        let l = crate::pipe::PipeListener::bind(&pipe).unwrap();
        std::thread::spawn(move || {
            let Ok(mut s) = l.accept() else { return };
            let hello = ServerMsg::Hello(Hello {
                protocol: PROTOCOL,
                version: "x".into(),
                capabilities: vec![],
            });
            if s.write_all(&encode_msg(&hello, None).unwrap()).is_err() {
                return;
            }
            let frame = encode_msg(&ServerMsg::Terminals { list: vec![] }, None).unwrap();
            loop {
                for b in &frame {
                    if s.write_all(&[*b]).is_err() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });
        let took = event_over(&pipe);
        assert!(
            took >= CLIENT_TIMEOUT - std::time::Duration::from_millis(100),
            "{took:?}"
        );
        assert!(
            took < CLIENT_TIMEOUT + std::time::Duration::from_millis(500),
            "{took:?}"
        );
    }
}

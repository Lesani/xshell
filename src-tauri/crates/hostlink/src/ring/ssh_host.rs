//! Add to Ring for a saved SSH Host (#115, design rjw.5 B + amendments AM1-AM6): the Desktop
//! runs `xshelld pair` on the Host over the Host's SSH route, reads the Device Code from the
//! command's output and approves it itself. The user's click on a Host this Desktop already
//! reaches is the consent.
//!
//! - The approval is bound to the clicked Host: its `ring.identity` names the key, and only
//!   that standalone Daemon's enrollment is approved
//!   ([`DesktopRing::approve_device_code_for`]).
//! - Membership comes from that identity, never from the command's output: this Ring is
//!   success without running `pair`, another Ring is `other-ring`.
//! - Success is the approval plus the Host's own completion: the command exits 0 within
//!   [`Timeouts::exit`] of the approval, else `unfinished`.
//! - The remote `pair` ends when the SSH session's input closes ([`cancellable`]).
//!
//! The Device Code words never leave this module: not in an error, a log line or a reply.
//! Errors are `code: detail` ([`AddError`]); the detail is for the log only.

use super::adoption::{ApprovalOutcome, MembershipState, APPROVE_COMPUTER_INTENT};
use super::desktop::{DesktopRing, LocalIdentity};
use super::pair_cli::{ALREADY_RUNNING, EXPIRED_EXIT, OFFLINE, RATE_LIMITED_EXIT, UNFINISHED_EXIT};
use crate::cancel::CancelToken;
use crate::handle::RING_CAPABILITY;
use crate::process::{self, Proc};
use crate::status::HostStatus;
use crate::transport::{sh_quote, sh_wrap, Transport};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::{RingId, Roster};
use zeroize::Zeroizing;

/// The error codes, first in `code: detail`. The frontend has one sentence per code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddCode {
    /// This Desktop cannot approve now: no Ring, another window owns pairing, an approval
    /// or adoption is still running or finishing.
    NotReady,
    /// The Host is not connected.
    Offline,
    /// The Host's xshell cannot pair this way (no `ring` capability, no `pair` command).
    Unsupported,
    /// The Host runs Windows (no `xshelld pair` there).
    Windows,
    /// The Host runs its own xshell Desktop: it joins from that machine.
    OwnDesktop,
    /// The Host is in another Ring.
    OtherRing,
    RateLimited,
    /// An earlier or this attempt is still being finished on the Host.
    Unfinished,
    Expired,
    /// The Host could not reach the Ring's Relay.
    Service,
    /// Another Add to Ring runs on this Desktop.
    Busy,
    Failed,
}

impl AddCode {
    pub fn as_str(self) -> &'static str {
        match self {
            AddCode::NotReady => "not-ready",
            AddCode::Offline => "offline",
            AddCode::Unsupported => "unsupported",
            AddCode::Windows => "windows",
            AddCode::OwnDesktop => "own-desktop",
            AddCode::OtherRing => "other-ring",
            AddCode::RateLimited => "rate-limited",
            AddCode::Unfinished => "unfinished",
            AddCode::Expired => "expired",
            AddCode::Service => "service",
            AddCode::Busy => "busy",
            AddCode::Failed => "failed",
        }
    }
}

/// `code: detail`. The detail never holds the words (it is scrubbed where words exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddError {
    pub code: AddCode,
    pub detail: String,
}

impl AddError {
    pub fn new(code: AddCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for AddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

/// How long to wait for the words, and for the command's exit after the approval.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub words: Duration,
    pub exit: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            words: Duration::from_secs(30),
            exit: Duration::from_secs(30),
        }
    }
}

// ── The remote command ────────────────────────────────────────────────────

/// The `xshelld` the Host's connect command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairBinary {
    /// The managed install in this install directory (safe in a shell word).
    Managed(String),
    /// The Host's "Daemon command": the user's own shell text, not quoted (as for connect).
    Command(String),
}

/// Which `xshelld` runs `pair`: the same one the connect command runs
/// (`supervisor.rs`): the managed install in `install_dir`; with a "Daemon command" on a
/// Unix Host, the Host's `selected` build when it has one, else the command itself.
pub fn resolve_binary(
    transport: &dyn Transport,
    daemon_command: Option<&str>,
    install_dir: &str,
    os: Option<&str>,
    cancel: &CancelToken,
) -> PairBinary {
    match daemon_command {
        None => PairBinary::Managed(install_dir.to_string()),
        Some(cmd) => {
            let selected = if matches!(os, Some("linux" | "macos")) {
                crate::install::selected_remote(transport, cmd, cancel)
            } else {
                None
            };
            match selected {
                Some(dir) => PairBinary::Managed(dir),
                None => PairBinary::Command(cmd.to_string()),
            }
        }
    }
}

/// `xshelld pair --relay <relay> --name <name>` for `binary`, as one POSIX `sh` command.
pub fn pair_invocation(binary: &PairBinary, relay: &str, name: &str) -> String {
    let args = format!("pair --relay {} --name {}", sh_quote(relay), sh_quote(name));
    match binary {
        PairBinary::Managed(dir) => format!("\"$HOME/.xshell/server/{dir}/xshelld\" {args}"),
        PairBinary::Command(cmd) => format!("{cmd} {args}"),
    }
}

/// `xshelld service install --json` for `binary`, as one POSIX `sh` command (#119).
pub fn service_invocation(binary: &PairBinary) -> String {
    match binary {
        PairBinary::Managed(dir) => {
            format!("\"$HOME/.xshell/server/{dir}/xshelld\" service install --json")
        }
        PairBinary::Command(cmd) => format!("{cmd} service install --json"),
    }
}

/// Whether the report of `service install --json` names a mechanism that starts the Daemon
/// again by itself (`installed` is not empty).
pub fn service_installed(stdout: &str) -> bool {
    stdout.lines().any(|line| {
        serde_json::from_str::<Value>(line.trim())
            .ok()
            .and_then(|report| report.get("installed")?.as_array().map(|a| !a.is_empty()))
            .unwrap_or(false)
    })
}

/// How long `service install` on the Host may take.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);

/// Keeps the Host's Daemon running without an SSH session (#119): runs the existing
/// `xshelld service install` there. A Host the Desktop installed over SSH has a Daemon that
/// `connect` started; it outlives the session but not a restart of the machine. A Host that
/// leaves the SSH list has no SSH route to start it again, so this must succeed first.
pub fn keep_running(
    transport: &dyn Transport,
    binary: &PairBinary,
    cancel: &CancelToken,
) -> Result<(), AddError> {
    let script = sh_wrap(&service_invocation(binary));
    let out = process::run_script(transport, &script, None, SERVICE_TIMEOUT, cancel)
        .map_err(|e| AddError::new(AddCode::Failed, format!("service install: {e}")))?;
    if out.code != Some(0) {
        return Err(AddError::new(
            AddCode::Failed,
            format!("service install ended with {:?}", out.code),
        ));
    }
    if !service_installed(&out.stdout) {
        return Err(AddError::new(
            AddCode::Failed,
            "the Host has no way to start its Daemon by itself",
        ));
    }
    Ok(())
}

/// `cmd` wrapped so it ends, with every process it started, when the session's input closes
/// (AM2, and Sol's diff review 1): the SSH input is kept on fd 3 for a watcher that ends `cmd`
/// at its EOF; `cmd` itself reads `/dev/null`. With job control (`set -m`; a shell without a
/// terminal may refuse it, as dash does) `cmd` is its own process group and the group is
/// signalled; otherwise its process tree (`d`, from `ps`) is. A "Daemon command" may be a
/// script that runs `xshelld` without `exec`. When `cmd` ends first the watcher's tree is
/// ended, and the wrapper exits with `cmd`'s code. The shell prints nothing on stdout. Run
/// under `sh` whatever the login shell is.
pub fn cancellable(cmd: &str) -> String {
    sh_wrap(&cancellable_script(cmd))
}

/// The script [`cancellable`] runs under `sh`.
pub fn cancellable_script(cmd: &str) -> String {
    format!(
        "set -m 2>/dev/null; exec 3<&0; d(){{ echo \"$1\"; for c in $(ps -A -o pid= -o ppid= | awk -v p=\"$1\" '$2==p{{print $1}}'); do d \"$c\"; done; }}; {cmd} </dev/null & p=$!; (cat <&3 >/dev/null 2>&1; kill -s TERM -- -$p 2>/dev/null || kill -s TERM $(d $p) 2>/dev/null) >/dev/null 2>&1 & w=$!; wait $p; s=$?; kill $(d $w) 2>/dev/null; exit $s"
    )
}

/// The longest stdout line read; longer lines are discarded (the words are one short line).
pub const MAX_LINE: usize = 4096;
/// The lines queued between the reader and [`drive`]; more are dropped, never waited for.
const LINE_QUEUE: usize = 64;

/// The remote command line Add to Ring runs on the Host.
pub fn remote_pair_command(binary: &PairBinary, relay: &str, name: &str) -> String {
    cancellable(&pair_invocation(binary, relay, name))
}

// ── Classification (AM5) ──────────────────────────────────────────────────

/// An approval error (`code: detail` from [`DesktopRing::approve_device_code_for`]) as an
/// Add to Ring code. `finishing` is handled by the caller first (AM4).
pub fn approval_code(error: &str) -> AddCode {
    let code = error.split_once(':').map_or(error, |(c, _)| c).trim();
    match code {
        // `busy`: another adoption, approval or registration holds this Desktop's admission.
        "other_window" | "not_approver" | "busy" => AddCode::NotReady,
        "finishing" => AddCode::Unfinished,
        "rate_limited" => AddCode::RateLimited,
        "expired" => AddCode::Expired,
        c if c == APPROVE_COMPUTER_INTENT => AddCode::OwnDesktop,
        _ => AddCode::Failed,
    }
}

/// How the command ended without words that were approved: by its exit code, and for exit 1
/// by the message `pair` printed on stderr.
pub fn exit_code_class(code: Option<i32>, stderr: &str) -> AddCode {
    match code {
        Some(RATE_LIMITED_EXIT) => AddCode::RateLimited,
        Some(EXPIRED_EXIT) => AddCode::Expired,
        Some(UNFINISHED_EXIT) => AddCode::Unfinished,
        Some(1) if stderr.contains(OFFLINE) => AddCode::Service,
        Some(1) if stderr.contains(ALREADY_RUNNING) => AddCode::Unfinished,
        // The shell's "not found", or a usage error: an `xshelld` without `pair`.
        Some(127) | Some(2) => AddCode::Unsupported,
        _ => AddCode::Failed,
    }
}

/// Refusals decided from the Host's status before anything runs: Windows (AM6), not
/// connected, no `ring` capability.
pub fn check_host(status: &HostStatus) -> Result<(), AddError> {
    if status.os.as_deref() == Some("windows") {
        return Err(AddError::new(AddCode::Windows, "the Host runs Windows"));
    }
    if !status.usable() {
        return Err(AddError::new(AddCode::Offline, "the Host is not connected"));
    }
    if !status
        .daemon_capabilities
        .iter()
        .any(|c| c == RING_CAPABILITY)
    {
        return Err(AddError::new(
            AddCode::Unsupported,
            "the Host's Daemon has no ring capability",
        ));
    }
    Ok(())
}

/// What the Host's `ring.identity` says about its membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMembership {
    None,
    This,
    Other,
}

pub fn host_membership(identity: &Value, ours: &RingId) -> HostMembership {
    match identity
        .get("ring")
        .and_then(|r| r.get("ringId"))
        .and_then(Value::as_str)
    {
        None => HostMembership::None,
        Some(id) if id == ours.as_str() => HostMembership::This,
        Some(_) => HostMembership::Other,
    }
}

/// `text` with every whole word of `code` masked (defence in depth).
pub fn scrub(text: &str, code: &DeviceCode) -> String {
    let formatted = code.format();
    let words: HashSet<&str> = formatted.split('-').collect();
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if words.contains(word.to_ascii_lowercase().as_str()) {
            out.push_str("•••");
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphabetic() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The names of the members in `after` that `before` did not have.
pub fn new_member_names(before: &Roster, after: &Roster) -> Vec<String> {
    after
        .members
        .iter()
        .filter(|m| before.member(&m.sign_key).is_none())
        .map(|m| m.name.clone())
        .collect()
}

// ── Driving the command ───────────────────────────────────────────────────

/// How a driven command ended well.
#[derive(Debug)]
pub enum Driven<A> {
    /// The approval succeeded and the command exited 0.
    Approved(A),
    /// The approval answered `finishing` (its publication was uncertain) and the command
    /// exited 0 anyway: the Host confirmed its own Join.
    Finished,
}

/// Ends the command: its input closes first (the remote `pair` ends at that EOF), then the
/// local process group is killed.
fn end(proc: &mut Proc) {
    drop(proc.stdin.take());
    if proc.child.wait_timeout(Duration::from_secs(2)).is_none() {
        proc.child.kill_and_wait(Duration::from_secs(2));
    }
}

/// The remote command's stderr is classified, never repeated (Sol's diff review 3): only how
/// many lines it had goes into a detail.
fn stderr_text(proc: &Proc) -> Zeroizing<String> {
    proc.wait_stderr(Duration::from_secs(1));
    Zeroizing::new(proc.stderr_text())
}

fn stderr_lines(stderr: &str) -> usize {
    stderr.lines().filter(|l| !l.trim().is_empty()).count()
}

/// Reads `out` line by line into `tx`: a line longer than [`MAX_LINE`] is discarded, a line
/// that finds the queue full or closed is dropped, and reading goes on to EOF either way, so
/// the command never stalls on a full pipe.
fn read_lines(out: impl std::io::Read, tx: mpsc::SyncSender<Zeroizing<String>>) {
    let mut out = BufReader::new(out);
    let mut line = Zeroizing::new(Vec::with_capacity(256));
    let mut oversized = false;
    loop {
        let (consumed, newline) = match out.fill_buf() {
            Ok([]) | Err(_) => return,
            Ok(buf) => match buf.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    if !oversized && line.len() + i <= MAX_LINE {
                        line.extend_from_slice(&buf[..i]);
                    } else {
                        oversized = true;
                    }
                    (i + 1, true)
                }
                None => {
                    if !oversized && line.len() + buf.len() <= MAX_LINE {
                        line.extend_from_slice(buf);
                    } else {
                        oversized = true;
                    }
                    (buf.len(), false)
                }
            },
        };
        out.consume(consumed);
        if newline {
            if !oversized {
                let text = Zeroizing::new(String::from_utf8_lossy(&line).into_owned());
                let _ = tx.try_send(text);
            }
            line.clear();
            oversized = false;
        }
    }
}

/// Reads the command's stdout line by line, approves the first line that parses as a Device
/// Code with `approve`, and waits for the command's exit (see the module docs). The stdin
/// of `proc` stays open until the end, and is closed to cancel. The words are never in the
/// result.
pub fn drive<A>(
    mut proc: Proc,
    timeouts: Timeouts,
    approve: impl FnOnce(&DeviceCode) -> Result<A, String>,
) -> Result<Driven<A>, AddError> {
    let Some(stdout) = proc.stdout.take() else {
        end(&mut proc);
        return Err(AddError::new(AddCode::Failed, "the command has no output"));
    };
    let (tx, rx) = mpsc::sync_channel::<Zeroizing<String>>(LINE_QUEUE);
    let reader = std::thread::Builder::new()
        .name("ring-ssh-pair-out".into())
        .spawn(move || read_lines(stdout, tx));
    if let Err(e) = reader {
        end(&mut proc);
        return Err(AddError::new(
            AddCode::Failed,
            format!("cannot read the command: {e}"),
        ));
    }
    let deadline = Instant::now() + timeouts.words;
    let no_words = || {
        AddError::new(
            AddCode::Failed,
            format!("no Device Code within {} s", timeouts.words.as_secs()),
        )
    };
    let code = loop {
        // Absolute: also while non-code lines keep arriving.
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            end(&mut proc);
            return Err(no_words());
        }
        match rx.recv_timeout(left) {
            Ok(line) => {
                if let Ok(code) = DeviceCode::parse(line.trim()) {
                    break code;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                end(&mut proc);
                return Err(no_words());
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Its output closed without words: how it ended says why.
                let status = proc.child.wait_timeout(Duration::from_secs(5));
                let exit = status.and_then(|s| s.code());
                end(&mut proc);
                let stderr = stderr_text(&proc);
                return Err(AddError::new(
                    exit_code_class(exit, &stderr),
                    format!(
                        "exit {exit:?} without words ({} stderr lines)",
                        stderr_lines(&stderr)
                    ),
                ));
            }
        }
    };
    // The reader only drains from here on.
    drop(rx);
    let approved = match approve(&code) {
        Ok(a) => Some(a),
        Err(e) if approval_code(&e) == AddCode::Unfinished => None,
        Err(e) => {
            end(&mut proc);
            return Err(AddError::new(approval_code(&e), scrub(&e, &code)));
        }
    };
    // AM4: the Host's own completion.
    let status = proc.child.wait_timeout(timeouts.exit);
    if status.is_some_and(|s| s.success()) {
        return Ok(match approved {
            Some(a) => Driven::Approved(a),
            None => Driven::Finished,
        });
    }
    let detail = match status {
        Some(s) => format!("approved; the command ended with {s}"),
        None => format!(
            "approved; the command did not end within {} s",
            timeouts.exit.as_secs()
        ),
    };
    end(&mut proc);
    let detail = format!(
        "{detail} ({} stderr lines)",
        stderr_lines(&stderr_text(&proc))
    );
    Err(AddError::new(AddCode::Unfinished, detail))
}

// ── The whole action ──────────────────────────────────────────────────────

/// What Add to Ring needs once the Host passed [`check_host`].
pub struct AddRequest<'a> {
    pub ring: &'a DesktopRing,
    /// The Host's saved name: its member name (`--name`).
    pub name: &'a str,
    pub transport: &'a dyn Transport,
    pub binary: &'a PairBinary,
    pub cancel: &'a CancelToken,
    pub timeouts: Timeouts,
}

/// [`add_ssh_host`]'s success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    /// The members the approval added (the Host's member when it already was one).
    pub names: Vec<String>,
    /// The Roster version now.
    pub version: u64,
    /// `pair` ran (the Host was not a member yet).
    pub paired: bool,
}

/// This Desktop can approve now (the preconditions of Add a machine).
pub fn check_ready(ring: &DesktopRing) -> Result<(), AddError> {
    if !ring.pairing_ipc_owner() {
        return Err(AddError::new(
            AddCode::NotReady,
            "another window owns pairing",
        ));
    }
    match ring.membership_state() {
        Ok(MembershipState::Member) => {}
        Ok(state) => {
            return Err(AddError::new(
                AddCode::NotReady,
                format!("this computer's membership is {state:?}"),
            ))
        }
        Err(e) => return Err(AddError::new(AddCode::NotReady, e)),
    }
    if ring.adoption_busy() {
        return Err(AddError::new(
            AddCode::NotReady,
            "an adoption or approval is running",
        ));
    }
    Ok(())
}

/// The error when the saved Host changed during the action.
pub fn host_changed() -> AddError {
    AddError::new(AddCode::Failed, "the Host changed")
}

/// Adds the Host to this Desktop's Ring: `identity` asks the Host for `ring.identity` over
/// its usable link; then (AM3) this Ring is success, another Ring `other-ring`, none runs
/// `pair` on the Host and approves its words, bound to the identity's key (AM1).
///
/// `unchanged` says whether the saved Host still exists with the configuration and link
/// generation captured at the start. It is asked before the identity is used, right before
/// `pair` starts and right before the words are approved; a change ends the command and is
/// [`host_changed`], with nothing approved.
pub fn add_ssh_host(
    req: &AddRequest<'_>,
    identity: impl FnOnce() -> Result<Value, String>,
    unchanged: impl Fn() -> bool,
) -> Result<Added, AddError> {
    check_ready(req.ring)?;
    let chain = req
        .ring
        .chain()
        .ok_or_else(|| AddError::new(AddCode::NotReady, "this computer is in no Ring"))?;
    let reply =
        identity().map_err(|e| AddError::new(AddCode::Failed, format!("ring.identity: {e}")))?;
    if !unchanged() {
        return Err(host_changed());
    }
    let host = LocalIdentity::from_json(&reply)
        .map_err(|e| AddError::new(AddCode::Failed, format!("ring.identity: {e}")))?;
    match host_membership(&reply, chain.ring_id()) {
        HostMembership::Other => {
            return Err(AddError::new(
                AddCode::OtherRing,
                "the Host is in another Ring",
            ))
        }
        HostMembership::This => {
            if let Some(member) = chain.authorized_member(&host.sign_key) {
                return Ok(Added {
                    names: vec![member.name.clone()],
                    version: chain.head().version(),
                    paired: false,
                });
            }
            // Not authorized in this Desktop's head (removed, or not seen yet): its own
            // `pair` decides; an "already in your Ring" without words is `failed`.
        }
        HostMembership::None => {}
    }
    let before = chain.head().roster().clone();
    let relay = before.relay_url.clone();
    let remote = remote_pair_command(req.binary, &relay, req.name);
    if !unchanged() {
        return Err(host_changed());
    }
    let proc = process::spawn(&req.transport.command(&remote), req.cancel)
        .map_err(|e| AddError::new(AddCode::Failed, format!("cannot start the command: {e}")))?;
    let changed = std::cell::Cell::new(false);
    let driven = drive(proc, req.timeouts, |code| {
        if !unchanged() {
            changed.set(true);
            return Err("host_changed: the Host changed".to_string());
        }
        req.ring.approve_device_code_for(code, &host.sign_key)
    })
    .map_err(|e| if changed.get() { host_changed() } else { e })?;
    let after = match driven {
        Driven::Approved(ApprovalOutcome { chain, .. }) => chain,
        Driven::Finished => req.ring.chain().ok_or_else(|| {
            AddError::new(AddCode::Unfinished, "approved; this Ring is not readable")
        })?,
    };
    Ok(Added {
        names: new_member_names(&before, after.head().roster()),
        version: after.head().version(),
        paired: true,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::transport::LocalShellTransport;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fixed_code() -> DeviceCode {
        DeviceCode::from_bytes([
            0x5a, 0x13, 0xc7, 0x02, 0x9e, 0x41, 0xb8, 0x6d, 0x20, 0xf3, 0x77, 0x0c,
        ])
    }

    fn words_in(text: &str, code: &DeviceCode) -> bool {
        let formatted = code.format();
        let words: HashSet<&str> = formatted.split('-').collect();
        text.to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphabetic())
            .any(|token| words.contains(token))
    }

    fn spawn(script: &str) -> Proc {
        let t = LocalShellTransport::default();
        process::spawn(&t.command(script), &CancelToken::new()).unwrap()
    }

    fn quick() -> Timeouts {
        Timeouts {
            words: Duration::from_secs(5),
            exit: Duration::from_secs(5),
        }
    }

    /// A fake `pair`: other lines, the words, then `rest`.
    fn fake_pair(rest: &str) -> String {
        format!(
            "echo 'xshell 1.6.0 on this machine'; echo '{}'; echo 'Waiting for another device to add this machine...'; {rest}",
            fixed_code().format().as_str()
        )
    }

    fn approved_with(code: &DeviceCode) -> bool {
        code.format() == fixed_code().format()
    }

    #[test]
    fn words_are_approved_and_success_needs_exit_zero() {
        let seen = AtomicBool::new(false);
        let r = drive(spawn(&fake_pair("sleep 0.2; exit 0")), quick(), |code| {
            seen.store(approved_with(code), Ordering::SeqCst);
            Ok(7)
        });
        assert!(matches!(r, Ok(Driven::Approved(7))), "{r:?}");
        assert!(
            seen.load(Ordering::SeqCst),
            "the words reached the approval"
        );
    }

    #[test]
    fn command_killed_right_after_the_approval_is_unfinished() {
        // AM4: approved, but the Host's command dies instead of confirming.
        let r = drive(spawn(&fake_pair("sleep 0.3; kill -9 $$")), quick(), |_| {
            std::thread::sleep(Duration::from_millis(100));
            Ok(())
        });
        let e = r.unwrap_err();
        assert_eq!(e.code, AddCode::Unfinished, "{e}");
        assert!(!words_in(&e.to_string(), &fixed_code()), "{e}");
        // Exits nonzero after the approval: unfinished as well.
        let e = drive(spawn(&fake_pair("exit 1")), quick(), |_| Ok(())).unwrap_err();
        assert_eq!(e.code, AddCode::Unfinished);
    }

    #[test]
    fn command_not_ending_after_the_approval_is_unfinished_and_ended() {
        let pidfile = tempfile::NamedTempFile::new().unwrap();
        let script = fake_pair(&format!(
            "echo $$ > {}; exec cat >/dev/null",
            pidfile.path().display()
        ));
        let started = Instant::now();
        let e = drive(
            spawn(&script),
            Timeouts {
                words: Duration::from_secs(5),
                exit: Duration::from_millis(300),
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(e.code, AddCode::Unfinished);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: i32 = std::fs::read_to_string(pidfile.path())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the command was ended");
    }

    #[test]
    fn finishing_then_exit_zero_is_finished_else_unfinished() {
        let r = drive(spawn(&fake_pair("exit 0")), quick(), |_| {
            Err::<(), _>("finishing: publication unresolved".into())
        });
        assert!(matches!(r, Ok(Driven::Finished)), "{r:?}");
        let e = drive(spawn(&fake_pair("exit 6")), quick(), |_| {
            Err::<(), _>("finishing: publication unresolved".into())
        })
        .unwrap_err();
        assert_eq!(e.code, AddCode::Unfinished);
    }

    #[test]
    fn approval_refusals_map_to_codes_and_never_carry_words() {
        let code = fixed_code();
        let words = code.format().as_str().to_string();
        for (error, expected) in [
            (
                "other_window: another window owns approval",
                AddCode::NotReady,
            ),
            (
                "not_approver: approval requires an authorized Computer",
                AddCode::NotReady,
            ),
            (
                "busy: another adoption operation is active",
                AddCode::NotReady,
            ),
            ("rate_limited: too many attempts", AddCode::RateLimited),
            ("expired: the code expired", AddCode::Expired),
            (
                "computer_intent: the machine joins with its own Desktop",
                AddCode::OwnDesktop,
            ),
            (
                "substituted: the words belong to another machine",
                AddCode::Failed,
            ),
            ("not_found: nobody waits", AddCode::Failed),
            ("offline: relay unreachable", AddCode::Failed),
            ("refused: no", AddCode::Failed),
        ] {
            assert_eq!(approval_code(error), expected, "{error}");
            // Even an error that (wrongly) held the words is scrubbed.
            let leaky = format!("{error} {words}");
            let e = drive(spawn(&fake_pair("cat >/dev/null")), quick(), |_| {
                Err::<(), _>(leaky.clone())
            })
            .unwrap_err();
            assert_eq!(e.code, expected, "{error}");
            assert!(!words_in(&e.to_string(), &code), "{e}");
        }
    }

    #[test]
    fn exit_codes_without_words_are_classified() {
        // One fixture per class (AM5), through a real child.
        let cases: [(String, AddCode); 9] = [
            ("exit 7".into(), AddCode::RateLimited),
            (
                "echo 'These words have expired. Run xshelld pair again for new words.'; exit 5"
                    .into(),
                AddCode::Expired,
            ),
            ("exit 6".into(), AddCode::Unfinished),
            (format!("echo \"{OFFLINE}\" >&2; exit 1"), AddCode::Service),
            (
                format!("echo \"{ALREADY_RUNNING}\" >&2; exit 1"),
                AddCode::Unfinished,
            ),
            (
                "echo 'xshelld: something else' >&2; exit 1".into(),
                AddCode::Failed,
            ),
            ("/nonexistent/xshelld pair".into(), AddCode::Unsupported),
            (
                "echo 'xshelld: unknown command: pair' >&2; exit 2".into(),
                AddCode::Unsupported,
            ),
            // AM3: an "already in your Ring" line is not success: membership comes from
            // the identity, which said "no Ring".
            (
                "echo 'This machine is already in your Ring. Your Ring has 2 machines.'; exit 0"
                    .into(),
                AddCode::Failed,
            ),
        ];
        for (script, expected) in cases {
            let e = drive(spawn(&script), quick(), |_| -> Result<(), String> {
                panic!("no words, no approval")
            })
            .unwrap_err();
            assert_eq!(e.code, expected, "{script}: {e}");
        }
    }

    #[test]
    fn no_words_in_time_fails_and_ends_the_command() {
        let started = Instant::now();
        let e = drive(
            spawn("echo 'Waiting...'; cat >/dev/null"),
            Timeouts {
                words: Duration::from_millis(300),
                exit: Duration::from_secs(5),
            },
            |_| -> Result<(), String> { panic!("no words") },
        )
        .unwrap_err();
        assert_eq!(e.code, AddCode::Failed);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// A "Daemon command" script that does not `exec`: `outer` is the script, `inner` the
    /// `pair`-like descendant it waits for. Prints `first_line` (if any) on stdout.
    fn non_exec_script(dir: &std::path::Path, first_line: Option<&str>) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join("pair");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho $$ > {d}/outer\n{echo}sh -c 'echo $$ > {d}/inner; sleep 30'\necho after\n",
                d = dir.display(),
                echo = first_line.map_or(String::new(), |l| format!("echo '{l}'\n")),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake
    }

    fn pid_in(path: &std::path::Path) -> i32 {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
            }
            assert!(Instant::now() < until, "{} never written", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn gone_within(pid: i32, d: Duration) -> bool {
        let until = Instant::now() + d;
        while alive(pid) {
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn cancelling_closes_stdin_and_the_whole_pair_tree_ends() {
        // A refused approval cancels: the wrapper sees its input close and ends `pair` and
        // what it started, also through a script that does not `exec`.
        let dir = tempfile::tempdir().unwrap();
        let words = fixed_code().format().as_str().to_string();
        let fake = non_exec_script(dir.path(), Some(&words));
        let script = cancellable(&sh_quote(&fake.display().to_string()));
        let started = Instant::now();
        let e = drive(spawn(&script), quick(), |_| {
            Err::<(), _>("not_found: nobody waits".into())
        })
        .unwrap_err();
        assert_eq!(e.code, AddCode::Failed);
        for name in ["outer", "inner"] {
            assert!(
                gone_within(pid_in(&dir.path().join(name)), Duration::from_secs(1)),
                "{name} still runs"
            );
        }
        // Ended by its closed input, not by the kill after the grace period.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// The shells the wrapper is checked under: `sh`, and dash and bash in POSIX mode when
    /// they are installed.
    fn shells() -> Vec<Vec<&'static str>> {
        let mut out = vec![vec!["sh"]];
        for (bin, args) in [
            ("/bin/dash", vec!["dash"]),
            ("/bin/bash", vec!["bash", "--posix"]),
        ] {
            if std::path::Path::new(bin).exists() {
                out.push(args);
            }
        }
        out
    }

    /// AM2 and Sol's diff review 1, with a real child of each shell and no process-group kill
    /// from here: `pair` (a script that does not `exec`) runs while stdin is open; it and its
    /// descendant end within a second of stdin closing; the shell prints nothing on stdout;
    /// the wrapper exits with `pair`'s code when `pair` ends first.
    #[test]
    fn wrapper_ends_the_tree_at_eof_and_exits_with_the_commands_code() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        for shell in shells() {
            let dir = tempfile::tempdir().unwrap();
            let fake = non_exec_script(dir.path(), None);
            let mut child = Command::new(shell[0])
                .args(&shell[1..])
                .arg("-c")
                .arg(cancellable_script(&sh_quote(&fake.display().to_string())))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let outer = pid_in(&dir.path().join("outer"));
            let inner = pid_in(&dir.path().join("inner"));
            std::thread::sleep(Duration::from_millis(1000));
            assert!(
                child.try_wait().unwrap().is_none(),
                "{shell:?}: ended with stdin open"
            );
            assert!(
                alive(outer) && alive(inner),
                "{shell:?}: pair ended with stdin open"
            );
            drop(child.stdin.take());
            assert!(
                gone_within(inner, Duration::from_secs(1)),
                "{shell:?}: descendant survived"
            );
            assert!(
                gone_within(outer, Duration::from_secs(1)),
                "{shell:?}: pair survived"
            );
            let status = child.wait().unwrap();
            assert!(!status.success(), "{shell:?}");
            let mut out = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut out)
                .unwrap();
            assert_eq!(out, "", "{shell:?}: the shell printed on stdout");

            // `pair` ends first: the wrapper exits with its code, promptly, stdin still open.
            for code in [0, 3, 7] {
                let mut child = Command::new(shell[0])
                    .args(&shell[1..])
                    .arg("-c")
                    .arg(cancellable_script(&format!(
                        "sh -c 'sleep 0.2; exit {code}'"
                    )))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let started = Instant::now();
                let status = loop {
                    if let Some(s) = child.try_wait().unwrap() {
                        break s;
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(3),
                        "{shell:?}: hung"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert_eq!(status.code(), Some(code), "{shell:?}");
                drop(child.stdin.take());
                let mut out = String::new();
                child
                    .stdout
                    .take()
                    .unwrap()
                    .read_to_string(&mut out)
                    .unwrap();
                assert_eq!(out, "", "{shell:?}");
            }
        }
    }

    #[test]
    fn continuous_output_does_not_extend_the_deadline() {
        let started = Instant::now();
        let e = drive(
            spawn("while :; do echo 'Waiting for another device...'; done"),
            Timeouts {
                words: Duration::from_millis(300),
                exit: Duration::from_secs(5),
            },
            |_| -> Result<(), String> { panic!("no words") },
        )
        .unwrap_err();
        assert_eq!(e.code, AddCode::Failed);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_oversized_line_is_discarded_and_reading_goes_on() {
        let words = fixed_code().format().as_str().to_string();
        // Words hidden at the end of a 5000-byte line are not words; the next line is.
        let long = "x ".repeat(2500);
        let seen = AtomicBool::new(false);
        let r = drive(
            spawn(&format!(
                "printf '%s%s\\n' '{long}' '{words}'; echo '{words}'; exit 0"
            )),
            quick(),
            |code| {
                seen.store(approved_with(code), Ordering::SeqCst);
                Ok(())
            },
        );
        assert!(matches!(r, Ok(Driven::Approved(()))), "{r:?}");
        assert!(seen.load(Ordering::SeqCst));
        // Only an oversized line: no words.
        let e = drive(
            spawn(&format!("printf '%s%s\\n' '{long}' '{words}'; exit 1")),
            quick(),
            |_| -> Result<(), String> { panic!("an oversized line is no code") },
        )
        .unwrap_err();
        assert_eq!(e.code, AddCode::Failed);
        assert!(!words_in(&e.to_string(), &fixed_code()));
    }

    #[test]
    fn stderr_never_crosses_into_the_error() {
        let code = fixed_code();
        let e = drive(
            spawn(&format!(
                "echo '{}' >&2; echo 'xshelld: secret detail' >&2; exit 1",
                code.format().as_str()
            )),
            quick(),
            |_| -> Result<(), String> { panic!("stderr words are not approved") },
        )
        .unwrap_err();
        assert_eq!(e.code, AddCode::Failed);
        let text = e.to_string();
        assert!(!words_in(&text, &code), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert_eq!(text, "failed: exit Some(1) without words (2 stderr lines)");
        // After an approval too.
        let e = drive(
            spawn(&fake_pair(&format!(
                "echo '{}' >&2; exit 1",
                code.format().as_str()
            ))),
            quick(),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(e.code, AddCode::Unfinished);
        assert!(!words_in(&e.to_string(), &code), "{e}");
    }

    #[test]
    fn a_full_queue_never_blocks_the_reader() {
        // The consumer reads nothing: the reader keeps draining to EOF.
        let (tx, rx) = mpsc::sync_channel(1);
        let data = "line\n".repeat(10_000);
        let reader = std::thread::spawn(move || read_lines(std::io::Cursor::new(data), tx));
        let until = Instant::now() + Duration::from_secs(5);
        while !reader.is_finished() {
            assert!(Instant::now() < until, "the reader blocked");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn pair_command_for_a_managed_install_and_a_daemon_command() {
        assert_eq!(
            pair_invocation(
                &PairBinary::Managed("1.6.0".into()),
                "ws://127.0.0.1:8787",
                "dev box"
            ),
            r#""$HOME/.xshell/server/1.6.0/xshelld" pair --relay 'ws://127.0.0.1:8787' --name 'dev box'"#
        );
        assert_eq!(
            pair_invocation(&PairBinary::Command("~/bin/xd".into()), "wss://r", "it's"),
            r#"~/bin/xd pair --relay 'wss://r' --name 'it'\''s'"#
        );
        // Through the wrapper and a real shell, the arguments arrive as single words.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(remote_pair_command(
                &PairBinary::Command("printf '%s|'".into()),
                "wss://r?x=1&y=$HOME",
                "it's \"mine\"",
            ))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "pair|--relay|wss://r?x=1&y=$HOME|--name|it's \"mine\"|"
        );
        // A managed install runs `$HOME/.xshell/server/<dir>/xshelld`.
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".xshell/server/9.9.9");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("xshelld"), "#!/bin/sh\nprintf '%s|' \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("xshelld"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(remote_pair_command(
                &PairBinary::Managed("9.9.9".into()),
                "ws://l",
                "n",
            ))
            .env("HOME", home.path())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "pair|--relay|ws://l|--name|n|"
        );
    }

    #[test]
    fn service_install_command_and_its_report() {
        assert_eq!(
            service_invocation(&PairBinary::Managed("1.6.0".into())),
            r#""$HOME/.xshell/server/1.6.0/xshelld" service install --json"#
        );
        assert_eq!(
            service_invocation(&PairBinary::Command("~/bin/xd".into())),
            "~/bin/xd service install --json"
        );
        assert!(service_installed(
            r#"{"mechanism":"profile","installed":["profile"],"active":false,"linger":null,"running":true,"launcher":"/h/.xshell/service/xshelld","changed":true,"warnings":[]}"#
        ));
        assert!(!service_installed(
            r#"{"mechanism":"none","installed":[],"active":false,"linger":null,"running":true,"launcher":"l","warnings":[]}"#
        ));
        assert!(!service_installed("xshell is running."));
        assert!(!service_installed(""));
    }

    /// Runs the remote command with a scratch HOME: never this user's own `~/.xshell`.
    struct HomeSh(std::path::PathBuf);
    impl Transport for HomeSh {
        fn command(&self, remote_cmd: &str) -> crate::transport::CommandSpec {
            crate::transport::CommandSpec {
                program: "sh".into(),
                args: vec!["-c".into(), remote_cmd.into()],
                env: vec![("HOME".into(), self.0.clone().into())],
            }
        }
        fn describe(&self) -> String {
            "sh".into()
        }
    }

    #[test]
    fn keep_running_needs_exit_zero_and_an_installed_mechanism() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".xshell/server/9.9.9");
        std::fs::create_dir_all(&dir).unwrap();
        let fake = |body: &str| {
            std::fs::write(dir.join("xshelld"), format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(dir.join("xshelld"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        };
        let t = HomeSh(home.path().to_path_buf());
        let bin = PairBinary::Managed("9.9.9".into());
        let cancel = CancelToken::new();
        fake(r#"[ "$1 $2 $3" = "service install --json" ] && echo '{"installed":["systemd"]}'"#);
        assert_eq!(keep_running(&t, &bin, &cancel), Ok(()));
        fake(r#"echo '{"installed":[]}'"#);
        assert_eq!(
            keep_running(&t, &bin, &cancel).unwrap_err().code,
            AddCode::Failed
        );
        fake(r#"echo '{"installed":["systemd"]}'; exit 1"#);
        assert_eq!(
            keep_running(&t, &bin, &cancel).unwrap_err().code,
            AddCode::Failed
        );
        std::fs::remove_file(dir.join("xshelld")).unwrap();
        assert_eq!(
            keep_running(&t, &bin, &cancel).unwrap_err().code,
            AddCode::Failed
        );
    }

    #[test]
    fn resolve_binary_follows_the_connect_command() {
        // A scratch HOME: never this user's own `~/.xshell`.
        let home = tempfile::tempdir().unwrap();
        let t = LocalShellTransport {
            env: vec![("HOME".into(), home.path().into())],
        };
        let cancel = CancelToken::new();
        assert_eq!(
            resolve_binary(&t, None, "1.6.0", Some("linux"), &cancel),
            PairBinary::Managed("1.6.0".into())
        );
        // A Daemon command whose Host has no selected build: the command itself.
        assert_eq!(
            resolve_binary(&t, Some("false"), "1.6.0", Some("linux"), &cancel),
            PairBinary::Command("false".into())
        );
        assert_eq!(
            resolve_binary(&t, Some("~/bin/xd"), "1.6.0", None, &cancel),
            PairBinary::Command("~/bin/xd".into())
        );
    }

    #[test]
    fn host_checks_and_membership() {
        let mut s = HostStatus::initial("h_1", "1.6.0", None, 0);
        s.status = crate::status::StatusKind::Connected;
        s.daemon_capabilities = vec!["ring".into()];
        assert_eq!(check_host(&s), Ok(()));
        s.os = Some("windows".into());
        assert_eq!(check_host(&s).unwrap_err().code, AddCode::Windows);
        s.os = Some("linux".into());
        s.daemon_capabilities.clear();
        assert_eq!(check_host(&s).unwrap_err().code, AddCode::Unsupported);
        s.status = crate::status::StatusKind::Offline;
        assert_eq!(check_host(&s).unwrap_err().code, AddCode::Offline);

        let ours = RingId::derive(
            &xshell_protocol::ring::DeviceKeys::generate()
                .unwrap()
                .sign_key(),
        );
        let theirs = RingId::derive(
            &xshell_protocol::ring::DeviceKeys::generate()
                .unwrap()
                .sign_key(),
        );
        assert_eq!(
            host_membership(&serde_json::json!({"ring": null}), &ours),
            HostMembership::None
        );
        assert_eq!(
            host_membership(
                &serde_json::json!({"ring": {"ringId": ours.as_str()}}),
                &ours
            ),
            HostMembership::This
        );
        assert_eq!(
            host_membership(
                &serde_json::json!({"ring": {"ringId": theirs.as_str()}}),
                &ours
            ),
            HostMembership::Other
        );
    }

    #[test]
    fn error_codes_are_the_designs() {
        let all = [
            (AddCode::NotReady, "not-ready"),
            (AddCode::Offline, "offline"),
            (AddCode::Unsupported, "unsupported"),
            (AddCode::Windows, "windows"),
            (AddCode::OwnDesktop, "own-desktop"),
            (AddCode::OtherRing, "other-ring"),
            (AddCode::RateLimited, "rate-limited"),
            (AddCode::Unfinished, "unfinished"),
            (AddCode::Expired, "expired"),
            (AddCode::Service, "service"),
            (AddCode::Busy, "busy"),
            (AddCode::Failed, "failed"),
        ];
        for (code, text) in all {
            assert_eq!(code.as_str(), text);
            assert_eq!(AddError::new(code, "d").to_string(), format!("{text}: d"));
        }
    }
}

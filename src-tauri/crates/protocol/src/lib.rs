//! The Desktop ↔ Daemon protocol: framing, messages, version negotiation and request
//! correlation. Sans-IO apart from the blocking `read_frame`/`write_frame` helpers, so any
//! runtime can drive it. With the `ring` feature (on by default) also the [`ring`] module: device
//! keys, the Roster and the Relay protocol; `relay-client` adds the Relay client.

pub mod backoff;
pub mod correlate;
pub mod frame;
pub mod launch;
pub mod msg;
pub mod negotiate;
#[cfg(feature = "ring")]
pub mod ring;

pub use launch::LaunchSpec;
use msg::ProtocolRange;

/// Support window (PRD, Lesani/xshell#1): a Mobile release speaks every protocol version that
/// any Desktop release from the last 12 months could install. The Mobile pins this crate, so
/// raising `PROTOCOL_MIN` (or narrowing the range in any other way) drops older Daemons for the
/// Mobile too: before doing so, check the protocol ranges of the Desktop releases from the last
/// 12 months.
pub const PROTOCOL_MIN: u32 = 1;
pub const PROTOCOL_MAX: u32 = 1;
pub const PROTOCOL: ProtocolRange = ProtocolRange {
    min: PROTOCOL_MIN,
    max: PROTOCOL_MAX,
};
/// Feature gates advertised in `hello` (additive; peers ignore unknown entries). Each is
/// documented at its constant in [`cap`].
pub const CAPABILITIES: &[&str] = &[
    cap::CALL,
    cap::TERM,
    cap::DAEMON_UPGRADE,
    cap::TERM_RELAUNCH,
    cap::LAUNCH_PREFIX,
    cap::AGENT_STATUS,
    cap::AGENT_LAST_LINE,
    cap::RING,
    cap::RING_CJOIN,
    cap::PUSH,
    cap::SESSION_STREAM,
    cap::TERM_MOBILE,
    cap::AGENT_PROMPT,
    cap::TERM_FIRST_MESSAGE,
    cap::PROJECT_SESSIONS,
    #[cfg(unix)]
    cap::TERM_SUBMIT,
];

/// The names in [`CAPABILITIES`], so peers gate on a constant instead of a string literal.
pub mod cap {
    /// `call` requests.
    pub const CALL: &str = "call";
    /// Terminal requests (`term.*`).
    pub const TERM: &str = "term";
    /// `daemon.upgrade` (Desktop only).
    pub const DAEMON_UPGRADE: &str = "daemon.upgrade";
    /// `term.relaunch`.
    pub const TERM_RELAUNCH: &str = "term.relaunch";
    /// Launch prefixes in `term.open`.
    pub const LAUNCH_PREFIX: &str = "launch.prefix";
    /// `TerminalInfo.agentStatus`.
    pub const AGENT_STATUS: &str = "agent.status";
    /// `TerminalInfo.lastLine`: the newest text message of an agent Terminal's session.
    pub const AGENT_LAST_LINE: &str = "agent.last-line";
    /// Ring identity and join (Desktop only).
    pub const RING: &str = "ring";
    /// `ring.join` takes `expect` (a conditional join).
    pub const RING_CJOIN: &str = "ring.cjoin";
    /// `push.register` and `push.unregister` (from a Mobile).
    pub const PUSH: &str = "push";
    /// `session.subscribe`, `session.page`, `session.unsubscribe` and `session.append`: an
    /// agent Terminal's conversation for the Chat View.
    pub const SESSION_STREAM: &str = "session.stream";
    /// The Mobile's Terminal View: a Mobile's `term.resize` records its size without applying
    /// it, and its `term.input` claims it; `term.size` notices to attached Mobiles; output to a
    /// Mobile paced (at most one frame per Terminal per second, ten per second for three
    /// seconds after its input); the attach `res` carries `{exitCode, cols, rows}`; a Mobile's
    /// attach nudges only when nobody else is attached; a Mobile's replay is a shorter tail;
    /// the size goes back to the last Desktop that held it when an owning Mobile leaves.
    pub const TERM_MOBILE: &str = "term.mobile";
    /// Permission Prompt buttons: `TerminalInfo.permissionPrompt`, the prompt an agent
    /// Terminal shows, read from a screen model of its output; and `term.answer`, which answers
    /// it. The Daemon types the option's key (its index digit) into the Terminal without taking
    /// the Terminal's size. Input that may answer the prompt (anything but focus reports,
    /// cursor-position replies and cursor keys) makes it stale first: an answer to a prompt
    /// someone answered or typed at is refused with `already answered`.
    pub const AGENT_PROMPT: &str = "agent.prompt";
    /// `term.open` takes `firstMessage`: a new Claude Code or Codex chat starts with that
    /// prompt. On Windows the agent then starts without `cmd.exe` (an `.exe` or an npm package),
    /// and an open is refused when it cannot.
    pub const TERM_FIRST_MESSAGE: &str = "term.first-message";
    /// `call get_project_sessions`: one Project's Claude Code and Codex sessions, newest
    /// first, paged (`msg::PastSessionsPage`).
    pub const PROJECT_SESSIONS: &str = "project.sessions";
    /// `term.submit`: a reply from the Chat View, typed into the agent's chat composer as one
    /// bracketed paste and Enter, never taking the Terminal's size; refused while the agent
    /// needs you or its composer is not on screen (`msg::ClientMsg::TermSubmit`). Unix Hosts
    /// only: a ConPTY re-renders the agent's output, so the Daemon cannot see whether the
    /// agent turned bracketed paste on.
    #[cfg(unix)]
    pub const TERM_SUBMIT: &str = "term.submit";
}

/// `xshelld connect`'s exit code when the Daemon on that machine is run by xshell there (it
/// is GUI-bound, ADR-0005) and xshell is closed: nothing was started.
pub const NOT_RUNNING_EXIT: i32 = 4;

/// Prefix of a `term.open` error after which the Terminal may still be running: the Daemon
/// could not save it, ended it, and could not confirm in time that its processes are gone.
/// Every other `term.open` error is a refusal (no Terminal exists).
pub const OPEN_INDETERMINATE: &str = "open-indeterminate:";
/// What `connect` prints after `xshelld: ` with [`NOT_RUNNING_EXIT`]. It avoids the phrases
/// ssh failures are recognized by ("No such file", "Connection refused").
pub const NOT_RUNNING_MESSAGE: &str =
    "xshell is not running here; terminals are reachable only while xshell is open";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_include_term_relaunch() {
        assert!(CAPABILITIES.contains(&"term.relaunch"));
    }

    #[test]
    fn capabilities_include_agent_status() {
        assert!(CAPABILITIES.contains(&"agent.status"));
    }

    #[test]
    fn capabilities_include_agent_last_line() {
        assert!(CAPABILITIES.contains(&"agent.last-line"));
    }

    #[test]
    fn capabilities_include_ring() {
        assert!(CAPABILITIES.contains(&"ring"));
        assert!(CAPABILITIES.contains(&"ring.cjoin"));
    }

    #[test]
    fn capabilities_include_push() {
        assert!(CAPABILITIES.contains(&"push"));
    }

    #[test]
    fn capabilities_include_session_stream() {
        assert!(CAPABILITIES.contains(&"session.stream"));
    }

    #[test]
    fn capabilities_include_term_mobile() {
        assert!(CAPABILITIES.contains(&"term.mobile"));
    }

    #[test]
    fn capabilities_include_term_first_message() {
        assert!(CAPABILITIES.contains(&"term.first-message"));
    }

    #[test]
    fn capabilities_include_project_sessions() {
        assert!(CAPABILITIES.contains(&"project.sessions"));
        assert_eq!(cap::PROJECT_SESSIONS, "project.sessions");
    }

    #[test]
    fn capabilities_include_agent_prompt() {
        assert!(CAPABILITIES.contains(&"agent.prompt"));
        assert_eq!(cap::AGENT_PROMPT, "agent.prompt");
    }

    #[cfg(unix)]
    #[test]
    fn capabilities_include_term_submit() {
        assert!(CAPABILITIES.contains(&"term.submit"));
        assert_eq!(cap::TERM_SUBMIT, "term.submit");
    }

    #[cfg(windows)]
    #[test]
    fn capabilities_exclude_term_submit_on_windows() {
        assert!(!CAPABILITIES.contains(&"term.submit"));
    }

    #[test]
    fn capabilities_include_launch_prefix() {
        assert!(CAPABILITIES.contains(&"launch.prefix"));
    }
}

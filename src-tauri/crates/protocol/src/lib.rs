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

pub const PROTOCOL_MIN: u32 = 1;
pub const PROTOCOL_MAX: u32 = 1;
pub const PROTOCOL: ProtocolRange = ProtocolRange {
    min: PROTOCOL_MIN,
    max: PROTOCOL_MAX,
};
/// Feature gates advertised in `hello` (additive; peers ignore unknown entries).
pub const CAPABILITIES: &[&str] = &[
    "call",
    "term",
    "daemon.upgrade",
    "term.relaunch",
    "launch.prefix",
    "agent.status",
    // `TerminalInfo.lastLine`: the newest text message of an agent Terminal's session.
    "agent.last-line",
    "ring",
    // `ring.join` takes `expect` (a conditional join).
    "ring.cjoin",
    // `push.register` and `push.unregister` (from a Mobile).
    "push",
    // `session.subscribe`, `session.page`, `session.unsubscribe` and `session.append`: an
    // agent Terminal's conversation for the Chat View.
    "session.stream",
    // The Mobile's Terminal View: a Mobile's `term.resize` records its size without applying
    // it, and its `term.input` claims it; `term.size` notices to attached Mobiles; output to a
    // Mobile paced (at most one frame per Terminal per second, ten per second for three
    // seconds after its input); the attach `res` carries `{exitCode, cols, rows}`; a Mobile's
    // attach nudges only when nobody else is attached; a Mobile's replay is a shorter tail;
    // the size goes back to the last Desktop that held it when an owning Mobile leaves.
    "term.mobile",
    // `term.open` takes `firstMessage`: a new Claude Code or Codex chat starts with that
    // prompt. Unix only: on Windows the agent runs through `cmd.exe /C`, which would parse the
    // text as a command line.
    #[cfg(unix)]
    "term.first-message",
];

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

    #[cfg(unix)]
    #[test]
    fn capabilities_include_term_first_message() {
        assert!(CAPABILITIES.contains(&"term.first-message"));
    }

    #[cfg(windows)]
    #[test]
    fn capabilities_exclude_term_first_message_on_windows() {
        assert!(!CAPABILITIES.contains(&"term.first-message"));
    }

    #[test]
    fn capabilities_include_launch_prefix() {
        assert!(CAPABILITIES.contains(&"launch.prefix"));
    }
}

//! The Desktop ↔ Daemon protocol: framing, messages, version negotiation and request
//! correlation. Sans-IO apart from the blocking `read_frame`/`write_frame` helpers, so any
//! runtime can drive it.

pub mod correlate;
pub mod frame;
pub mod launch;
pub mod msg;
pub mod negotiate;

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
];

/// `xshelld connect`'s exit code when the Daemon on that machine is run by xshell there (it
/// is GUI-bound, ADR-0005) and xshell is closed: nothing was started.
pub const NOT_RUNNING_EXIT: i32 = 4;
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
    fn capabilities_include_launch_prefix() {
        assert!(CAPABILITIES.contains(&"launch.prefix"));
    }
}

//! The Desktop ↔ Daemon protocol: framing, messages, version negotiation and request
//! correlation. Sans-IO apart from the blocking `read_frame`/`write_frame` helpers, so any
//! runtime can drive it.

pub mod correlate;
pub mod frame;
pub mod msg;
pub mod negotiate;

use msg::ProtocolRange;

pub const PROTOCOL_MIN: u32 = 1;
pub const PROTOCOL_MAX: u32 = 1;
pub const PROTOCOL: ProtocolRange = ProtocolRange {
    min: PROTOCOL_MIN,
    max: PROTOCOL_MAX,
};
/// Feature gates advertised in `hello` (additive; peers ignore unknown entries).
pub const CAPABILITIES: &[&str] = &["call", "term", "daemon.upgrade"];

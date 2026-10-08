//! Terminal helpers shared by hosts that keep Terminals alive across connections: the replay
//! buffer and the persisted Terminal list. Command building lives in [`crate::launch`].

pub mod replay;
pub mod state;

/// Injected in place of output dropped under backpressure. Starts with RIS so the Tab is
/// reset rather than left with a CSI sequence cut in half.
pub const OVERFLOW_NOTICE: &[u8] =
    b"\x1bc\x1b[2m[xshell: dropped output due to backpressure]\x1b[0m\r\n";

use super::relay::wire::{CloseReason, ErrorCode};
use super::{RosterError, SignError};
use std::fmt;

/// Everything that can go wrong building Rosters or talking to a Relay.
#[derive(Debug, Clone, PartialEq)]
pub enum RingError {
    /// The device's [`Signer`](super::Signer) failed.
    Sign(SignError),
    /// A Roster (built, received or uploaded) failed verification.
    Roster(RosterError),
    /// A bad argument: an invalid relay URL, payload, reason or token.
    Invalid(String),
    /// Dialing, TLS or the WebSocket upgrade failed.
    Connect(String),
    /// The Relay refused the request.
    Relay {
        code: ErrorCode,
        detail: Option<String>,
    },
    /// The Relay broke the protocol.
    Protocol(String),
    /// A deadline passed.
    Timeout,
    /// The outbound queue is full; the Relay or the network is not keeping up.
    Backpressure,
    /// The connection is gone.
    Closed(CloseReason),
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RingError::Sign(e) => write!(f, "{e}"),
            RingError::Roster(e) => write!(f, "roster refused: {e}"),
            RingError::Invalid(m) => write!(f, "invalid: {m}"),
            RingError::Connect(m) => write!(f, "cannot reach the relay: {m}"),
            RingError::Relay { code, detail } => match detail {
                Some(d) => write!(f, "relay refused: {} ({d})", code.as_str()),
                None => write!(f, "relay refused: {}", code.as_str()),
            },
            RingError::Protocol(m) => write!(f, "relay protocol error: {m}"),
            RingError::Timeout => f.write_str("timed out"),
            RingError::Backpressure => f.write_str("outbound queue full"),
            RingError::Closed(r) => write!(f, "connection closed: {r}"),
        }
    }
}

impl std::error::Error for RingError {}

impl From<SignError> for RingError {
    fn from(e: SignError) -> Self {
        RingError::Sign(e)
    }
}

impl From<RosterError> for RingError {
    fn from(e: RosterError) -> Self {
        RingError::Roster(e)
    }
}

#![doc = include_str!("../../../RELAY.md")]

pub mod wire;

#[cfg(feature = "relay-client")]
mod client;
#[cfg(feature = "relay-client")]
mod io;
#[cfg(feature = "relay-client")]
pub mod transport;

#[cfg(feature = "test-relay")]
pub mod contract;
#[cfg(feature = "test-relay")]
pub mod test_relay;

#[cfg(feature = "relay-client")]
pub use client::{
    MemberStatus, RingClient, RingClientConfig, RingEvents, RingLimits, RingTimeouts,
};
pub use wire::{ByeReason, CloseReason, ErrorCode, MemberPresence, Presence};

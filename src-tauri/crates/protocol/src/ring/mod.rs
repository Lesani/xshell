//! The Ring: device keys, the signed and versioned Roster with its signature chain, and the
//! Relay protocol. Behind the `ring` feature (pure Rust, no network code); the blocking Relay
//! client needs `relay-client`, the in-process test Relay and the contract scenarios
//! `test-relay`.
//!
//! Every signature is domain-separated: the signed bytes start with one of the `*_CONTEXT`
//! strings below, so a signature made for one purpose never verifies for another. All
//! verification is strict (`verify_strict`: canonical signatures, no small-order keys), and
//! every decoder refuses non-canonical base64, padding, duplicate JSON keys and oversize input.
//!
//! **Security boundary.** The Roster chain stops a Relay from forging membership or rolling a
//! device back behind the head it already holds. It cannot prove freshness: a Relay can hide a
//! newer version, and a fork signed by a removed or compromised Desktop is detectable (by the
//! `prev` hash) only once two devices compare heads. Envelope `from` and presence are Relay
//! assertions; the Noise sessions ([`noise`], `relay::sessions`) authenticate peers and
//! payloads end to end and bind `from` into each handshake.

pub mod b64;
pub mod chain;
pub mod entitlement;
mod error;
mod json;
pub mod keys;
pub mod noise;
pub mod pairing;
pub mod push;
pub mod relay;
pub mod ring_id;
pub mod roster;
pub mod url;

pub use chain::{verify_genesis, verify_successor, Accepted, RosterChain};
pub use error::RingError;
pub use keys::{verify, DeviceKeys, NoiseKey, SecretSeed, SignError, SignKey, Signature, Signer};
pub use ring_id::RingId;
pub use roster::{member_name, Member, Role, Roster, RosterDraft, RosterError, SignedRoster};

/// Prefix of the bytes hashed into a [`RingId`].
pub const RING_ID_CONTEXT: &str = "xshell-ring-v1\n";
/// Prefix of the bytes a Desktop signs for a Roster token.
pub const ROSTER_CONTEXT: &str = "xshell-roster-v1\n";
/// Prefix of the bytes a device signs to answer a Relay challenge.
pub const AUTH_CONTEXT: &str = "xshell-relay-auth-v1\n";
/// Prefix of the bytes a joining device signs over the pairing handshake hash.
pub const PAIR_POP_CONTEXT: &str = pairing::POP_CONTEXT;
/// Prefix of a session handshake's Noise prologue.
pub const NOISE_CONTEXT: &str = noise::SESSION_PROLOGUE;
/// Prefix of a sealed push's Noise prologue.
pub const PUSH_CONTEXT: &str = push::PUSH_PROLOGUE;
/// Every Roster token starts with this.
pub const ROSTER_PREFIX: &str = "xro1.";
/// Every Push Gateway entitlement token starts with this (see `push/core/entitlement.ts`).
pub const ENTITLEMENT_PREFIX: &str = "xet1.";

/// The largest integer every JSON implementation reads exactly (`Number.MAX_SAFE_INTEGER`).
pub const MAX_SAFE_INT: u64 = (1 << 53) - 1;

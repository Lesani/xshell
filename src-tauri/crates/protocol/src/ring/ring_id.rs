//! A Ring's id: the SHA-256 of its creator's sign key under [`RING_ID_CONTEXT`], so the
//! genesis Roster proves who created the Ring and no one can claim another Ring's id.
//!
//! [`RING_ID_CONTEXT`]: super::RING_ID_CONTEXT

use super::{b64, SignKey, RING_ID_CONTEXT};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RingId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRingId;

impl fmt::Display for InvalidRingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a ring id is 16 to 128 characters of [A-Za-z0-9_-]")
    }
}

impl std::error::Error for InvalidRingId {}

impl RingId {
    /// `b64u(SHA-256(ASCII("xshell-ring-v1\n") || creatorSignKey))`: 43 characters.
    pub fn derive(creator: &SignKey) -> Self {
        let mut h = Sha256::new();
        h.update(RING_ID_CONTEXT.as_bytes());
        h.update(creator.as_bytes());
        RingId(b64::encode(&h.finalize()))
    }

    /// Accepts what the Push Gateway accepts (`/^[A-Za-z0-9_-]{16,128}$/`). Whether an id was
    /// derived from its creator is a genesis check, not a parse check.
    pub fn parse(s: &str) -> Result<Self, InvalidRingId> {
        let ok = (16..=128).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if ok {
            Ok(RingId(s.to_string()))
        } else {
            Err(InvalidRingId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RingId({})", self.0)
    }
}

impl Serialize for RingId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RingId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        RingId::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_matches_the_gateway_regex() {
        assert!(RingId::parse(&"a".repeat(16)).is_ok());
        assert!(RingId::parse(&"a".repeat(128)).is_ok());
        assert!(RingId::parse(&"a".repeat(15)).is_err());
        assert!(RingId::parse(&"a".repeat(129)).is_err());
        assert!(RingId::parse("aaaaaaaaaaaaaaa=").is_err());
        assert!(RingId::parse("aaaaaaaaaaaaaaa/").is_err());
        assert!(RingId::parse("aaaaaaaaaaaaaaaé").is_err());
    }
}

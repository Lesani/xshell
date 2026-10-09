//! The Roster: a Ring's signed, versioned list of member keys and roles.
//!
//! Token: `"xro1." + b64u(payloadJSON) + "." + b64u(signature)`, where the signature is
//! Ed25519 by `signedBy` over `ASCII("xshell-roster-v1\n") || ASCII(b64u(payloadJSON))`. It
//! covers the encoded string, so nobody needs canonical JSON: devices and Relays store and
//! forward the exact token and never re-encode it. Fields this version does not know are
//! kept (and signed) but ignored.
//!
//! [`SignedRoster::parse`] checks a single version: its encoding, structure and signature.
//! Whether that signer was allowed to sign it is a chain question, answered in
//! [`chain`](super::chain).

use super::json::strict_object;
use super::url::RelayUrl;
use super::{
    b64, chain, verify, NoiseKey, RingError, RingId, SignKey, Signature, Signer, MAX_SAFE_INT,
    ROSTER_CONTEXT, ROSTER_PREFIX,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;

/// A whole token (prefix, payload, signature) is at most this many bytes.
pub const MAX_ROSTER_TOKEN: usize = 64 * 1024;
pub const MAX_MEMBERS: usize = 64;
pub const MAX_NAME_BYTES: usize = 64;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A computer Desktop: the only role that may sign a new Roster version.
    Desktop,
    /// A headless Daemon.
    Daemon,
    Mobile,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    /// The device's display name: 1 to 64 bytes of UTF-8, no control or invisible formatting
    /// characters.
    pub name: String,
    pub role: Role,
    pub sign_key: SignKey,
    pub noise_key: NoiseKey,
    /// Unix seconds.
    pub added_at: u64,
    /// Fields from newer versions, kept so a re-signed Roster does not drop them.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Member {
    pub fn new(name: &str, role: Role, sign_key: SignKey, noise_key: NoiseKey, now: u64) -> Self {
        Member {
            name: name.to_string(),
            role,
            sign_key,
            noise_key,
            added_at: now,
            extra: Map::new(),
        }
    }
}

/// One Roster version's payload.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    /// Format version, always 1.
    pub v: u32,
    pub ring_id: RingId,
    /// 1 for the genesis version, then +1 per change.
    pub version: u64,
    /// `null` in version 1, else `b64u(SHA-256(ASCII(previous token)))`.
    pub prev: Option<String>,
    /// The Ring's one Relay.
    pub relay_url: String,
    /// Who signed this version; a Desktop of the previous version (of this one, for v1).
    pub signed_by: SignKey,
    /// Unix seconds.
    pub issued_at: u64,
    pub members: Vec<Member>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

const REQUIRED_FIELDS: &[&str] = &[
    "v", "ringId", "version", "prev", "relayUrl", "signedBy", "issuedAt", "members",
];
const REQUIRED_MEMBER_FIELDS: &[&str] = &["name", "role", "signKey", "noiseKey", "addedAt"];

/// Why a Roster version was refused. [`RosterError::as_code`] is the `detail` the Relay puts
/// on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterError {
    /// Not a decodable token: prefix, base64url, JSON, missing or mistyped fields.
    Malformed(String),
    /// The signature does not verify under `signedBy`.
    BadSignature,
    /// A different `ringId` than the chain's (or, for a genesis, than its creator's).
    RingMismatch,
    /// A chain must start at version 1 with `prev: null`.
    NotGenesis,
    /// The signer is not a member of the version it extends (of itself, for a genesis).
    SignerNotMember,
    /// The signer is a member but not a Desktop.
    SignerNotDesktop,
    /// Not newer than the version it would replace.
    Stale,
    /// Skips a version.
    Gap,
    /// `prev` does not hash the version it extends: a fork.
    PrevMismatch,
    /// Over the size or count limits.
    TooLarge,
    /// Decodable but breaks a structural rule.
    Invalid(String),
}

impl RosterError {
    pub fn as_code(&self) -> &'static str {
        match self {
            RosterError::Malformed(_) => "malformed",
            RosterError::BadSignature => "bad_signature",
            RosterError::RingMismatch => "ring_mismatch",
            RosterError::NotGenesis => "not_genesis",
            RosterError::SignerNotMember => "signer_not_member",
            RosterError::SignerNotDesktop => "signer_not_desktop",
            RosterError::Stale => "stale",
            RosterError::Gap => "gap",
            RosterError::PrevMismatch => "prev_mismatch",
            RosterError::TooLarge => "too_large",
            RosterError::Invalid(_) => "invalid",
        }
    }

    /// Reads a wire `detail` back; unknown details become `Invalid`.
    pub fn from_code(code: &str) -> Self {
        match code {
            "malformed" => RosterError::Malformed(String::new()),
            "bad_signature" => RosterError::BadSignature,
            "ring_mismatch" => RosterError::RingMismatch,
            "not_genesis" => RosterError::NotGenesis,
            "signer_not_member" => RosterError::SignerNotMember,
            "signer_not_desktop" => RosterError::SignerNotDesktop,
            "stale" => RosterError::Stale,
            "gap" => RosterError::Gap,
            "prev_mismatch" => RosterError::PrevMismatch,
            "too_large" => RosterError::TooLarge,
            other => RosterError::Invalid(other.to_string()),
        }
    }
}

impl fmt::Display for RosterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RosterError::Malformed(m) | RosterError::Invalid(m) if !m.is_empty() => {
                write!(f, "{}: {m}", self.as_code())
            }
            _ => f.write_str(self.as_code()),
        }
    }
}

impl std::error::Error for RosterError {}

fn invalid(m: impl Into<String>) -> RosterError {
    RosterError::Invalid(m.into())
}

/// Zero-width and bidirectional formatting characters, which could make one name look like
/// another. Control characters are refused separately.
fn is_invisible_format(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

fn check_name(name: &str) -> Result<(), RosterError> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(invalid("name must be 1 to 64 bytes"));
    }
    if name
        .chars()
        .any(|c| c.is_control() || is_invisible_format(c))
    {
        return Err(invalid("name contains a control or formatting character"));
    }
    Ok(())
}

fn check_int(what: &str, n: u64) -> Result<(), RosterError> {
    if n > MAX_SAFE_INT {
        return Err(invalid(format!("{what} above 2^53-1")));
    }
    Ok(())
}

impl Roster {
    /// The rules every version must meet on its own.
    pub fn validate(&self) -> Result<(), RosterError> {
        if self.v != 1 {
            return Err(invalid("unsupported roster format"));
        }
        if self.version == 0 {
            return Err(invalid("version must be at least 1"));
        }
        check_int("version", self.version)?;
        check_int("issuedAt", self.issued_at)?;
        if let Some(prev) = &self.prev {
            b64::decode_array::<32>(prev).map_err(|_| invalid("prev is not a SHA-256"))?;
        }
        RelayUrl::parse(&self.relay_url).map_err(|e| invalid(e.to_string()))?;
        if self.members.is_empty() || self.members.len() > MAX_MEMBERS {
            return Err(invalid("a roster has 1 to 64 members"));
        }
        let mut signs = HashSet::new();
        let mut noises = HashSet::new();
        for m in &self.members {
            check_name(&m.name)?;
            check_int("addedAt", m.added_at)?;
            if !signs.insert(m.sign_key) {
                return Err(invalid("duplicate signKey"));
            }
            if !noises.insert(m.noise_key) {
                return Err(invalid("duplicate noiseKey"));
            }
        }
        if !self.members.iter().any(|m| m.role == Role::Desktop) {
            return Err(invalid("a roster needs at least one desktop"));
        }
        Ok(())
    }

    pub fn member(&self, key: &SignKey) -> Option<&Member> {
        self.members.iter().find(|m| &m.sign_key == key)
    }

    /// Signs this payload as it stands, with no chain checks, and parses the result back.
    /// Building blocks for [`SignedRoster::genesis`] and [`SignedRoster::next`]; tests use it
    /// to build tokens the chain must refuse.
    pub fn sign(&self, signer: &dyn Signer) -> Result<SignedRoster, RingError> {
        let json = serde_json::to_vec(self).map_err(|e| RingError::Invalid(e.to_string()))?;
        let payload = b64::encode(&json);
        let sig = signer.sign(&signed_bytes(&payload))?;
        let token = format!("{ROSTER_PREFIX}{payload}.{}", b64::encode(&sig));
        Ok(SignedRoster::parse(&token)?)
    }
}

fn signed_bytes(payload_part: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(ROSTER_CONTEXT.len() + payload_part.len());
    v.extend_from_slice(ROSTER_CONTEXT.as_bytes());
    v.extend_from_slice(payload_part.as_bytes());
    v
}

/// What a Desktop may change in the next version.
#[derive(Clone, PartialEq, Debug)]
pub struct RosterDraft {
    pub relay_url: String,
    pub members: Vec<Member>,
    pub extra: Map<String, Value>,
}

impl RosterDraft {
    pub fn add(&mut self, member: Member) {
        self.members.push(member);
    }

    /// Removes the member with `key`; false when there was none.
    pub fn remove(&mut self, key: &SignKey) -> bool {
        let before = self.members.len();
        self.members.retain(|m| &m.sign_key != key);
        self.members.len() != before
    }
}

/// One verified Roster version: the exact token and what it says.
#[derive(Clone, PartialEq, Debug)]
pub struct SignedRoster {
    token: String,
    roster: Roster,
}

impl SignedRoster {
    /// Decodes and checks one version: size, encoding, structure, and the signature under its
    /// own `signedBy`.
    pub fn parse(token: &str) -> Result<Self, RosterError> {
        if token.len() > MAX_ROSTER_TOKEN {
            return Err(RosterError::TooLarge);
        }
        let malformed = |m: &str| RosterError::Malformed(m.to_string());
        let rest = token
            .strip_prefix(ROSTER_PREFIX)
            .ok_or_else(|| malformed("no xro1. prefix"))?;
        let (payload_part, sig_part) = rest
            .split_once('.')
            .ok_or_else(|| malformed("not payload.signature"))?;
        let payload = b64::decode(payload_part).map_err(|_| malformed("payload encoding"))?;
        let sig = Signature::parse(sig_part).map_err(|_| malformed("signature encoding"))?;
        let obj = strict_object(&payload).map_err(RosterError::Malformed)?;
        for f in REQUIRED_FIELDS {
            if !obj.contains_key(*f) {
                return Err(RosterError::Malformed(format!("missing {f}")));
            }
        }
        if let Some(Value::Array(members)) = obj.get("members") {
            for m in members {
                for f in REQUIRED_MEMBER_FIELDS {
                    if m.get(f).is_none() {
                        return Err(RosterError::Malformed(format!("member missing {f}")));
                    }
                }
            }
        }
        let roster: Roster = serde_json::from_value(Value::Object(obj))
            .map_err(|e| RosterError::Malformed(e.to_string()))?;
        roster.validate()?;
        if !verify(&roster.signed_by, &signed_bytes(payload_part), &sig) {
            return Err(RosterError::BadSignature);
        }
        Ok(SignedRoster {
            token: token.to_string(),
            roster,
        })
    }

    /// Version 1 of a new Ring, created and signed by a Desktop. The Ring's id derives from
    /// the signer's key.
    pub fn genesis(
        signer: &dyn Signer,
        noise_key: NoiseKey,
        name: &str,
        relay_url: &str,
        now: u64,
    ) -> Result<Self, RingError> {
        let key = signer.sign_key();
        let roster = Roster {
            v: 1,
            ring_id: RingId::derive(&key),
            version: 1,
            prev: None,
            relay_url: relay_url.to_string(),
            signed_by: key,
            issued_at: now,
            members: vec![Member::new(name, Role::Desktop, key, noise_key, now)],
            extra: Map::new(),
        };
        let signed = roster.sign(signer)?;
        chain::verify_genesis(&signed)?;
        Ok(signed)
    }

    /// The next version, signed by `signer`, with the changes `edit` makes. Fails unless the
    /// result is a valid successor (so a Mobile or Daemon signer fails here, not at the Relay).
    pub fn next(
        &self,
        signer: &dyn Signer,
        now: u64,
        edit: impl FnOnce(&mut RosterDraft),
    ) -> Result<Self, RingError> {
        let mut draft = RosterDraft {
            relay_url: self.roster.relay_url.clone(),
            members: self.roster.members.clone(),
            extra: self.roster.extra.clone(),
        };
        edit(&mut draft);
        let roster = Roster {
            v: 1,
            ring_id: self.roster.ring_id.clone(),
            version: self.roster.version + 1,
            prev: Some(self.hash()),
            relay_url: draft.relay_url,
            signed_by: signer.sign_key(),
            issued_at: now,
            members: draft.members,
            extra: draft.extra,
        };
        let signed = roster.sign(signer)?;
        chain::verify_successor(self, &signed)?;
        Ok(signed)
    }

    /// The exact token, as stored and forwarded.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn roster(&self) -> &Roster {
        &self.roster
    }

    pub fn version(&self) -> u64 {
        self.roster.version
    }

    pub fn ring_id(&self) -> &RingId {
        &self.roster.ring_id
    }

    pub fn member(&self, key: &SignKey) -> Option<&Member> {
        self.roster.member(key)
    }

    /// `b64u(SHA-256(ASCII(token)))`: the next version's `prev`.
    pub fn hash(&self) -> String {
        b64::encode(&Sha256::digest(self.token.as_bytes()))
    }
}

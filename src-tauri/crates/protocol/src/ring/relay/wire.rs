//! The Relay's frames: one JSON object per WebSocket text frame, tagged by `"t"`, camelCase
//! fields, unknown fields ignored. See the [module docs](super) for the protocol.

use super::super::json::strict_object;
use super::super::{b64, RingId, SignKey, Signature, AUTH_CONTEXT};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fmt;

/// Protocol version in `challenge.v`.
pub const RELAY_PROTOCOL: u32 = 1;
/// A Relay → client frame, and an `auth.chain` or `roster.chain` frame, is at most this long.
pub const MAX_FRAME: usize = 1024 * 1024;
/// An envelope's decoded payload is at most this long (the Hosted per-message limit).
pub const MAX_ENVELOPE_PAYLOAD: usize = 64 * 1024;
/// An `env` frame from a client is at most this long: a full payload plus its addressing.
pub const MAX_ENV_FRAME: usize = 96 * 1024;
/// A `roster.put` frame is at most this long: one Roster token plus framing.
pub const MAX_ROSTER_PUT_FRAME: usize = 72 * 1024;
/// Every other client frame is at most this long.
pub const MAX_CONTROL_FRAME: usize = 8 * 1024;
/// An `auth.chain` candidate is at most this many bytes of tokens in all.
pub const MAX_CANDIDATE_BYTES: usize = 16 * 1024 * 1024;
/// A Relay stores a staged candidate in physical chunks of at most this many bytes each
/// (under the Durable Object's 128 KiB value limit).
pub const MAX_STAGE_CHUNK: usize = 96 * 1024;
/// A staged candidate is at most this many physical chunks …
pub const MAX_STAGE_CHUNKS: usize = 4096;
/// … and at most this many serialized bytes in all.
pub const MAX_STAGE_BYTES: usize = MAX_CANDIDATE_BYTES + 64 * 1024;
/// An entitlement token is at most this long.
pub const MAX_ENTITLEMENT_TOKEN: usize = 4 * 1024;
/// A Hosted Relay's default daily quota of client frames per Ring (section 14).
pub const HOSTED_QUOTA_FRAMES_PER_DAY: u64 = 2_000_000;
/// A socket is closed with 4029 after this many quota refusals in a row (section 14).
pub const QUOTA_REFUSALS_BEFORE_CLOSE: usize = 32;
/// The exact keepalive frames.
pub const PING: &str = r#"{"t":"ping"}"#;
pub const PONG: &str = r#"{"t":"pong"}"#;

/// A pairing pipe frame, either way, is at most this long (section 16).
pub const MAX_PAIR_FRAME: usize = 16 * 1024;
/// A pairing pipe socket sends at most this many `pair.msg` frames.
pub const MAX_PAIR_MSGS: usize = 8;
/// A pairing slot lives this long after its first open.
pub const PAIR_SLOT_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// A Relay keeps a slot's expiry state at most this long.
pub const PAIR_STATE_TTL: std::time::Duration = std::time::Duration::from_secs(900);
/// Suggested Relay limits on the pipe: slot opens per client address per minute, and slots
/// outstanding at once.
pub const PAIR_OPENS_PER_MINUTE: u32 = 10;
pub const PAIR_MAX_SLOTS: usize = 1000;

/// The largest frame a client may send of type `t`.
pub fn client_frame_cap(t: &str) -> usize {
    match t {
        "auth.chain" => MAX_FRAME,
        "env" => MAX_ENV_FRAME,
        "roster.put" => MAX_ROSTER_PUT_FRAME,
        _ => MAX_CONTROL_FRAME,
    }
}

/// WebSocket close codes the Relay uses.
pub mod close {
    pub const NORMAL: u16 = 1000;
    /// A frame over its size cap.
    pub const TOO_LARGE: u16 = 1009;
    /// Malformed, binary or out-of-order frames.
    pub const BAD_REQUEST: u16 = 4000;
    pub const BAD_SIGNATURE: u16 = 4001;
    /// Authentication refused: no Roster, an invalid candidate, or not a member.
    pub const REFUSED: u16 = 4003;
    /// A new Roster version no longer lists this device.
    pub const REMOVED: u16 = 4004;
    pub const AUTH_TIMEOUT: u16 = 4008;
    /// The same key connected again.
    pub const REPLACED: u16 = 4009;
    pub const QUOTA: u16 = 4029;
    /// A third socket on a pairing slot, or a slot already used (section 16).
    pub const PAIR_BUSY: u16 = 4010;
    /// A pairing slot's time is up (section 16).
    pub const PAIR_EXPIRED: u16 = 4008;
}

/// An `error` frame's `code`.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum ErrorCode {
    BadRequest,
    Unsupported,
    UnknownType,
    AuthTimeout,
    NoRoster,
    NotMember,
    BadSignature,
    RosterInvalid,
    RosterStale,
    RosterConflict,
    Removed,
    Replaced,
    Offline,
    UnknownRecipient,
    TooLarge,
    Quota,
    RateLimited,
    EntitlementRequired,
    EntitlementInvalid,
    Internal,
    /// Pairing pipe: a third socket, or a slot already used.
    PairBusy,
    /// Pairing pipe: the slot expired.
    PairExpired,
    /// Pairing pipe: more than [`MAX_PAIR_MSGS`] messages in one direction.
    TooMany,
    /// A code this version does not know.
    Other(String),
}

const ERROR_CODES: &[(ErrorCode, &str)] = &[
    (ErrorCode::BadRequest, "bad_request"),
    (ErrorCode::Unsupported, "unsupported"),
    (ErrorCode::UnknownType, "unknown_type"),
    (ErrorCode::AuthTimeout, "auth_timeout"),
    (ErrorCode::NoRoster, "no_roster"),
    (ErrorCode::NotMember, "not_member"),
    (ErrorCode::BadSignature, "bad_signature"),
    (ErrorCode::RosterInvalid, "roster_invalid"),
    (ErrorCode::RosterStale, "roster_stale"),
    (ErrorCode::RosterConflict, "roster_conflict"),
    (ErrorCode::Removed, "removed"),
    (ErrorCode::Replaced, "replaced"),
    (ErrorCode::Offline, "offline"),
    (ErrorCode::UnknownRecipient, "unknown_recipient"),
    (ErrorCode::TooLarge, "too_large"),
    (ErrorCode::Quota, "quota"),
    (ErrorCode::RateLimited, "rate_limited"),
    (ErrorCode::EntitlementRequired, "entitlement_required"),
    (ErrorCode::EntitlementInvalid, "entitlement_invalid"),
    (ErrorCode::Internal, "internal"),
    (ErrorCode::PairBusy, "pair_busy"),
    (ErrorCode::PairExpired, "pair_expired"),
    (ErrorCode::TooMany, "too_many"),
];

impl ErrorCode {
    pub fn as_str(&self) -> &str {
        if let ErrorCode::Other(s) = self {
            return s;
        }
        ERROR_CODES
            .iter()
            .find(|(c, _)| c == self)
            .map(|(_, s)| *s)
            .unwrap_or("internal")
    }

    pub fn parse(s: &str) -> Self {
        ERROR_CODES
            .iter()
            .find(|(_, name)| *name == s)
            .map(|(c, _)| c.clone())
            .unwrap_or_else(|| ErrorCode::Other(s.to_string()))
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Ok(ErrorCode::parse(&s))
    }
}

/// The `lastReason` a Relay records when a socket closed without a `bye`.
pub const DROPPED: &str = "dropped";

/// Why a device said goodbye: `^[a-z][a-z0-9_.-]{0,31}$`, never `dropped`.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct ByeReason(String);

impl ByeReason {
    /// The Desktop quit.
    pub fn quit() -> Self {
        ByeReason("quit".into())
    }
    /// The Daemon exited after being idle.
    pub fn idle() -> Self {
        ByeReason("idle".into())
    }
    /// The Daemon is replacing itself (`daemon.upgrade`).
    pub fn upgrade() -> Self {
        ByeReason("upgrade".into())
    }

    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        let ok = (1..=32).contains(&b.len())
            && b[0].is_ascii_lowercase()
            && b[1..]
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(c))
            && s != DROPPED;
        ok.then(|| ByeReason(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for ByeReason {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByeReason {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        ByeReason::parse(&s).ok_or_else(|| serde::de::Error::custom("bad bye reason"))
    }
}

/// One member's presence as the Relay records it (amendment A4).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    pub sign_key: SignKey,
    pub online: bool,
    /// Unix seconds of the last connect or disconnect; `null` if never seen.
    #[serde(default, deserialize_with = "safe::opt")]
    pub last_seen: Option<u64>,
    /// The last `bye` reason, or `dropped`; `null` while online or never seen.
    #[serde(default)]
    pub last_reason: Option<String>,
}

/// What a client makes of a [`Presence`] record. A Relay assertion, not a proof.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MemberPresence {
    Online {
        since: Option<u64>,
    },
    /// A Roster member the Relay has not seen yet.
    NeverConnected,
    /// It said goodbye: "xshell closed".
    Closed {
        reason: String,
        at: Option<u64>,
    },
    /// The socket dropped without a goodbye: "unreachable".
    Unreachable {
        at: Option<u64>,
    },
}

impl From<&Presence> for MemberPresence {
    fn from(p: &Presence) -> Self {
        if p.online {
            return MemberPresence::Online { since: p.last_seen };
        }
        match (&p.last_reason, p.last_seen) {
            (None, None) => MemberPresence::NeverConnected,
            (Some(r), at) if r != DROPPED => MemberPresence::Closed {
                reason: r.clone(),
                at,
            },
            (_, at) => MemberPresence::Unreachable { at },
        }
    }
}

/// Why a Ring client's connection ended.
#[derive(Clone, PartialEq, Debug)]
pub enum CloseReason {
    /// After this side's `bye`.
    Bye,
    /// This side dropped the connection.
    Local,
    /// The Relay closed it, after `error` with `code` if it sent one.
    Relay {
        close_code: Option<u16>,
        error: Option<ErrorCode>,
    },
    /// No frame from the Relay within the keepalive deadline.
    Dead,
    /// The stream broke.
    Io(String),
    /// The Relay broke the protocol.
    Protocol(String),
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CloseReason::Bye => f.write_str("said goodbye"),
            CloseReason::Local => f.write_str("closed locally"),
            CloseReason::Relay { close_code, error } => {
                write!(f, "closed by the relay")?;
                if let Some(e) = error {
                    write!(f, " ({e})")?;
                }
                if let Some(c) = close_code {
                    write!(f, " [{c}]")?;
                }
                Ok(())
            }
            CloseReason::Dead => f.write_str("relay stopped answering"),
            CloseReason::Io(e) => write!(f, "{e}"),
            CloseReason::Protocol(e) => write!(f, "protocol error: {e}"),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Every wire integer is at most 2^53−1, so JavaScript reads it exactly.
mod safe {
    use crate::ring::MAX_SAFE_INT;
    use serde::{Deserialize, Deserializer};

    pub fn int<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let n = u64::deserialize(d)?;
        if n > MAX_SAFE_INT {
            return Err(serde::de::Error::custom("integer above 2^53-1"));
        }
        Ok(n)
    }

    pub fn opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        match Option::<u64>::deserialize(d)? {
            Some(n) if n > MAX_SAFE_INT => Err(serde::de::Error::custom("integer above 2^53-1")),
            o => Ok(o),
        }
    }
}

/// Client → Relay.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "t")]
pub enum ClientFrame {
    /// Before `auth`: Roster versions to stage as the authentication candidate.
    #[serde(rename = "auth.chain")]
    AuthChain { rosters: Vec<String> },
    #[serde(rename = "auth", rename_all = "camelCase")]
    Auth {
        sign_key: SignKey,
        sig: Signature,
        #[serde(default)]
        caps: Vec<String>,
    },
    #[serde(rename = "env")]
    Env { to: SignKey, payload: String },
    #[serde(rename = "bye")]
    Bye { reason: ByeReason },
    #[serde(rename = "roster.put")]
    RosterPut {
        #[serde(deserialize_with = "safe::int")]
        id: u64,
        roster: String,
    },
    #[serde(rename = "roster.get")]
    RosterGet {
        #[serde(deserialize_with = "safe::int")]
        id: u64,
        #[serde(deserialize_with = "safe::int")]
        since: u64,
    },
    #[serde(rename = "entitlement.put")]
    EntitlementPut {
        #[serde(deserialize_with = "safe::int")]
        id: u64,
        token: String,
    },
    #[serde(rename = "ping")]
    Ping,
    /// A type this version does not know (decode only).
    #[serde(skip)]
    Unknown { t: String },
}

const CLIENT_TYPES: &[&str] = &[
    "auth.chain",
    "auth",
    "env",
    "bye",
    "roster.put",
    "roster.get",
    "entitlement.put",
    "ping",
];

/// Relay → client.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "t")]
pub enum RelayFrame {
    #[serde(rename = "challenge", rename_all = "camelCase")]
    Challenge {
        v: u32,
        nonce: String,
        /// The Relay's head version for this Ring, 0 when it holds none.
        #[serde(default, deserialize_with = "safe::int")]
        roster_version: u64,
        #[serde(default)]
        caps: Vec<String>,
    },
    #[serde(rename = "welcome", rename_all = "camelCase")]
    Welcome {
        you: SignKey,
        #[serde(deserialize_with = "safe::int")]
        roster_version: u64,
        presence: Vec<Presence>,
        #[serde(default)]
        entitlement: Option<String>,
        /// A Hosted Relay without a valid Hosted entitlement for this Ring: a limited session
        /// that does not route envelopes (section 12 of the protocol).
        #[serde(default, skip_serializing_if = "is_false")]
        limited: bool,
        #[serde(default)]
        caps: Vec<String>,
    },
    #[serde(rename = "presence")]
    Presence(Presence),
    #[serde(rename = "env")]
    Env { from: SignKey, payload: String },
    #[serde(rename = "ok")]
    Ok {
        #[serde(deserialize_with = "safe::int")]
        id: u64,
    },
    #[serde(rename = "error")]
    Error {
        code: ErrorCode,
        #[serde(
            default,
            deserialize_with = "safe::opt",
            skip_serializing_if = "Option::is_none"
        )]
        id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<SignKey>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "roster")]
    Roster { roster: String },
    #[serde(rename = "roster.chain")]
    RosterChain {
        #[serde(deserialize_with = "safe::int")]
        id: u64,
        rosters: Vec<String>,
        more: bool,
    },
    #[serde(rename = "entitlement")]
    Entitlement {
        token: Option<String>,
        /// On a Hosted Relay: whether sessions of this Ring are still limited.
        #[serde(default, skip_serializing_if = "is_false")]
        limited: bool,
    },
    #[serde(rename = "pong")]
    Pong,
    #[serde(skip)]
    Unknown { t: String },
}

const RELAY_TYPES: &[&str] = &[
    "challenge",
    "welcome",
    "presence",
    "env",
    "ok",
    "error",
    "roster",
    "roster.chain",
    "entitlement",
    "pong",
];

/// Pairing pipe, client → Relay (section 16).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "t")]
pub enum PairClientFrame {
    /// `payload`: b64u, at most 8192 bytes decoded; forwarded to the other socket.
    #[serde(rename = "pair.msg")]
    Msg { payload: String },
    #[serde(rename = "ping")]
    Ping,
}

/// Pairing pipe, Relay → client (section 16).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "t")]
pub enum PairRelayFrame {
    /// The first socket on the slot: waiting for the other side.
    #[serde(rename = "pair.wait")]
    Wait { v: u32 },
    /// Both sides are here.
    #[serde(rename = "pair.peer")]
    Peer,
    #[serde(rename = "pair.msg")]
    Msg { payload: String },
    #[serde(rename = "error")]
    Error {
        code: ErrorCode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "pong")]
    Pong,
    #[serde(skip)]
    Unknown { t: String },
}

const PAIR_CLIENT_TYPES: &[&str] = &["pair.msg", "ping"];
const PAIR_RELAY_TYPES: &[&str] = &["pair.wait", "pair.peer", "pair.msg", "error", "pong"];

/// Decodes a pairing pipe frame from a client: at most [`MAX_PAIR_FRAME`]; an unknown `t`
/// decodes as `Err(Invalid)` (the Relay refuses it).
pub fn decode_pair_client(text: &str) -> Result<PairClientFrame, WireError> {
    let (v, t) = split(text, MAX_PAIR_FRAME)?;
    if !PAIR_CLIENT_TYPES.contains(&t.as_str()) {
        return Err(WireError::Invalid {
            t,
            error: "not a pairing frame".into(),
        });
    }
    typed(v, t)
}

/// Decodes a pairing pipe frame from the Relay.
pub fn decode_pair_relay(text: &str) -> Result<PairRelayFrame, WireError> {
    let (v, t) = split(text, MAX_PAIR_FRAME)?;
    if !PAIR_RELAY_TYPES.contains(&t.as_str()) {
        return Ok(PairRelayFrame::Unknown { t });
    }
    typed(v, t)
}

impl PairClientFrame {
    pub fn encode(&self) -> String {
        encode_tagged(self, None)
    }
}

impl PairRelayFrame {
    pub fn encode(&self) -> String {
        let unknown = match self {
            PairRelayFrame::Unknown { t } => Some(t.as_str()),
            _ => None,
        };
        encode_tagged(self, unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// Not a JSON object with a string `t`, or duplicate keys.
    Malformed(String),
    /// Over the cap for its type.
    TooLarge,
    /// A known type with bad fields.
    Invalid { t: String, error: String },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Malformed(e) => write!(f, "malformed frame: {e}"),
            WireError::TooLarge => f.write_str("frame too large"),
            WireError::Invalid { t, error } => write!(f, "invalid {t}: {error}"),
        }
    }
}

impl std::error::Error for WireError {}

fn split(text: &str, max: usize) -> Result<(Value, String), WireError> {
    if text.len() > max {
        return Err(WireError::TooLarge);
    }
    let obj = strict_object(text.as_bytes()).map_err(WireError::Malformed)?;
    let t = obj
        .get("t")
        .and_then(Value::as_str)
        .ok_or_else(|| WireError::Malformed("missing string \"t\"".into()))?
        .to_string();
    Ok((Value::Object(obj), t))
}

fn typed<T: serde::de::DeserializeOwned>(v: Value, t: String) -> Result<T, WireError> {
    serde_json::from_value(v).map_err(|e| WireError::Invalid {
        t,
        error: e.to_string(),
    })
}

/// Decodes a client frame, enforcing its type's size cap.
pub fn decode_client(text: &str) -> Result<ClientFrame, WireError> {
    let (v, t) = split(text, MAX_FRAME)?;
    if text.len() > client_frame_cap(&t) {
        return Err(WireError::TooLarge);
    }
    if !CLIENT_TYPES.contains(&t.as_str()) {
        return Ok(ClientFrame::Unknown { t });
    }
    typed(v, t)
}

/// Decodes a Relay frame.
pub fn decode_relay(text: &str) -> Result<RelayFrame, WireError> {
    let (v, t) = split(text, MAX_FRAME)?;
    if !RELAY_TYPES.contains(&t.as_str()) {
        return Ok(RelayFrame::Unknown { t });
    }
    typed(v, t)
}

fn encode_tagged<T: Serialize>(frame: &T, unknown: Option<&str>) -> String {
    if let Some(t) = unknown {
        return serde_json::json!({ "t": t }).to_string();
    }
    // Every frame is plain data with string keys: serializing cannot fail.
    serde_json::to_string(frame).unwrap_or_default()
}

impl ClientFrame {
    pub fn encode(&self) -> String {
        let unknown = match self {
            ClientFrame::Unknown { t } => Some(t.as_str()),
            _ => None,
        };
        encode_tagged(self, unknown)
    }
}

impl RelayFrame {
    pub fn encode(&self) -> String {
        let unknown = match self {
            RelayFrame::Unknown { t } => Some(t.as_str()),
            _ => None,
        };
        encode_tagged(self, unknown)
    }

    pub fn error(code: ErrorCode) -> Self {
        RelayFrame::Error {
            code,
            id: None,
            to: None,
            detail: None,
        }
    }
}

/// The bytes a device signs to answer a challenge:
/// `"xshell-relay-auth-v1\n" + origin + "\n" + ringId + "\n" + nonce + "\n" + signKey`.
pub fn auth_message(origin: &str, ring: &RingId, nonce: &str, key: &SignKey) -> Vec<u8> {
    format!("{AUTH_CONTEXT}{origin}\n{ring}\n{nonce}\n{key}").into_bytes()
}

/// A fresh 32-byte challenge nonce.
pub fn new_nonce() -> Result<String, getrandom::Error> {
    let mut n = [0u8; 32];
    getrandom::getrandom(&mut n)?;
    Ok(b64::encode(&n))
}

/// Checks an envelope payload's encoding and size without allocating for an oversize one.
pub fn check_payload(payload: &str) -> Result<(), ErrorCode> {
    match b64::decoded_len(payload.len()) {
        Some(n) if n > MAX_ENVELOPE_PAYLOAD => Err(ErrorCode::TooLarge),
        None => Err(ErrorCode::BadRequest),
        Some(_) => b64::decode(payload)
            .map(|_| ())
            .map_err(|_| ErrorCode::BadRequest),
    }
}

/// The well-formedness a Relay without the gateway key checks on an entitlement token:
/// `xet1.` + two canonical base64url parts, a 64-byte signature, at most 4 KiB.
pub fn entitlement_well_formed(token: &str) -> bool {
    if token.len() > MAX_ENTITLEMENT_TOKEN {
        return false;
    }
    let Some(rest) = token.strip_prefix(super::super::ENTITLEMENT_PREFIX) else {
        return false;
    };
    let Some((payload, sig)) = rest.split_once('.') else {
        return false;
    };
    let payload_ok = b64::decode(payload)
        .ok()
        .and_then(|p| strict_object(&p).ok())
        .is_some();
    payload_ok && b64::decode_array::<64>(sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::DeviceKeys;

    fn key() -> SignKey {
        DeviceKeys::from_seeds(&[9; 32], &[8; 32]).sign_key()
    }

    #[test]
    fn ping_is_the_exact_auto_response_bytes() {
        assert_eq!(ClientFrame::Ping.encode(), PING);
        assert_eq!(RelayFrame::Pong.encode(), PONG);
    }

    #[test]
    fn unknown_types_decode_as_unknown() {
        assert_eq!(
            decode_relay(r#"{"t":"future.thing","x":1}"#),
            Ok(RelayFrame::Unknown {
                t: "future.thing".into()
            })
        );
        assert_eq!(
            decode_client(r#"{"t":"state","foreground":true}"#),
            Ok(ClientFrame::Unknown { t: "state".into() })
        );
    }

    #[test]
    fn malformed_frames_are_refused_without_panicking() {
        let k = key().to_b64();
        let cases: Vec<String> = vec![
            "".into(),
            "null".into(),
            "[]".into(),
            "\"t\"".into(),
            "{".into(),
            r#"{"t":1}"#.into(),
            r#"{"x":"env"}"#.into(),
            r#"{"t":"env","t":"bye"}"#.into(),
            r#"{"t":"env"}"#.into(),
            format!(r#"{{"t":"env","to":"{k}x","payload":""}}"#),
            r#"{"t":"env","to":"AAAA","payload":""}"#.into(),
            r#"{"t":"bye","reason":"dropped"}"#.into(),
            r#"{"t":"bye","reason":"Quit"}"#.into(),
            r#"{"t":"roster.get","id":-1,"since":0}"#.into(),
            r#"{"t":"roster.get","id":1.5,"since":0}"#.into(),
            format!(r#"{{"t":"auth","signKey":"{k}","sig":"AA"}}"#),
            "\u{0}".into(),
        ];
        for c in &cases {
            assert!(decode_client(c).is_err(), "{c:?} decoded");
            let _ = decode_relay(c);
        }
        let big = format!(
            r#"{{"t":"bye","reason":"quit","pad":"{}"}}"#,
            "x".repeat(9000)
        );
        assert_eq!(decode_client(&big), Err(WireError::TooLarge));
        assert_eq!(
            decode_relay(&"x".repeat(MAX_FRAME + 1)),
            Err(WireError::TooLarge)
        );
    }

    #[test]
    fn frames_round_trip() {
        let k = key();
        let frames = vec![
            ClientFrame::AuthChain {
                rosters: vec!["xro1.a.b".into()],
            },
            ClientFrame::Env {
                to: k,
                payload: b64::encode(b"hi"),
            },
            ClientFrame::Bye {
                reason: ByeReason::quit(),
            },
            ClientFrame::RosterGet { id: 3, since: 1 },
            ClientFrame::Ping,
        ];
        for f in frames {
            assert_eq!(decode_client(&f.encode()).unwrap(), f);
        }
        let e = RelayFrame::Error {
            code: ErrorCode::Other("future_code".into()),
            id: Some(1),
            to: Some(k),
            detail: None,
        };
        assert_eq!(decode_relay(&e.encode()).unwrap(), e);
    }

    #[test]
    fn error_codes_round_trip() {
        for (c, s) in ERROR_CODES {
            assert_eq!(c.as_str(), *s);
            assert_eq!(&ErrorCode::parse(s), c);
        }
        assert_eq!(ErrorCode::parse("nope"), ErrorCode::Other("nope".into()));
    }

    #[test]
    fn presence_maps_to_host_status() {
        let p = |online, seen: Option<u64>, reason: Option<&str>| Presence {
            sign_key: key(),
            online,
            last_seen: seen,
            last_reason: reason.map(str::to_string),
        };
        assert_eq!(
            MemberPresence::from(&p(true, Some(5), None)),
            MemberPresence::Online { since: Some(5) }
        );
        assert_eq!(
            MemberPresence::from(&p(false, None, None)),
            MemberPresence::NeverConnected
        );
        assert_eq!(
            MemberPresence::from(&p(false, Some(6), Some("dropped"))),
            MemberPresence::Unreachable { at: Some(6) }
        );
        assert_eq!(
            MemberPresence::from(&p(false, Some(7), Some("quit"))),
            MemberPresence::Closed {
                reason: "quit".into(),
                at: Some(7)
            }
        );
    }

    #[test]
    fn payload_bounds() {
        assert!(check_payload(&b64::encode(&vec![0; MAX_ENVELOPE_PAYLOAD])).is_ok());
        assert_eq!(
            check_payload(&b64::encode(&vec![0; MAX_ENVELOPE_PAYLOAD + 1])),
            Err(ErrorCode::TooLarge)
        );
        assert_eq!(check_payload("A"), Err(ErrorCode::BadRequest));
        assert_eq!(check_payload("AB"), Err(ErrorCode::BadRequest));
        assert_eq!(check_payload("+/"), Err(ErrorCode::BadRequest));
        let frame = ClientFrame::Env {
            to: key(),
            payload: b64::encode(&vec![0; MAX_ENVELOPE_PAYLOAD]),
        }
        .encode();
        assert!(frame.len() <= MAX_ENV_FRAME);
    }

    #[test]
    fn integers_above_2_53_are_refused() {
        let max = crate::ring::MAX_SAFE_INT;
        assert!(decode_client(&format!(r#"{{"t":"roster.get","id":{max},"since":0}}"#)).is_ok());
        assert!(decode_client(&format!(
            r#"{{"t":"roster.get","id":{},"since":0}}"#,
            max + 1
        ))
        .is_err());
        assert!(decode_relay(&format!(r#"{{"t":"ok","id":{}}}"#, max + 1)).is_err());
        assert!(decode_relay(&format!(r#"{{"t":"error","code":"x","id":{}}}"#, max + 1)).is_err());
        assert!(decode_relay(r#"{"t":"error","code":"x","id":null}"#).is_ok());
        assert!(decode_relay(r#"{"t":"error","code":"x"}"#).is_ok());
    }

    #[test]
    fn pair_frames_round_trip() {
        assert_eq!(
            PairRelayFrame::Wait { v: 1 }.encode(),
            r#"{"t":"pair.wait","v":1}"#
        );
        assert_eq!(PairRelayFrame::Peer.encode(), r#"{"t":"pair.peer"}"#);
        assert_eq!(PairClientFrame::Ping.encode(), PING);
        let m = PairClientFrame::Msg {
            payload: b64::encode(b"x"),
        };
        assert_eq!(decode_pair_client(&m.encode()).unwrap(), m);
        assert!(decode_pair_client(r#"{"t":"env","to":"x","payload":""}"#).is_err());
        assert!(decode_pair_client(&format!(
            r#"{{"t":"pair.msg","payload":"{}"}}"#,
            "A".repeat(MAX_PAIR_FRAME)
        ))
        .is_err());
        let e = PairRelayFrame::Error {
            code: ErrorCode::PairBusy,
            detail: None,
        };
        assert_eq!(decode_pair_relay(&e.encode()).unwrap(), e);
        assert_eq!(
            decode_pair_relay(r#"{"t":"later"}"#).unwrap(),
            PairRelayFrame::Unknown { t: "later".into() }
        );
    }

    #[test]
    fn bye_reasons() {
        for ok in ["quit", "idle", "upgrade", "a", "x.y-z_9"] {
            assert!(ByeReason::parse(ok).is_some(), "{ok}");
        }
        for bad in ["", "dropped", "Quit", "9x", "a b", &"a".repeat(33)] {
            assert!(ByeReason::parse(bad).is_none(), "{bad}");
        }
    }
}

//! Sealed push payloads (`PUSH.md`, included below): what a Daemon tells a Mobile through the
//! Push Gateway, sealed so that only that Mobile can open it.
//!
//! The Daemon seals with the one-way `Noise_X_25519_ChaChaPoly_BLAKE2s` handshake from its own
//! Noise static key to the Mobile's **seal key**, a separate X25519 key the Mobile registers
//! over its session (`push.register`). The Daemon's static key travels encrypted inside the
//! message, so the Mobile learns who sealed it and maps it to a `daemon` member of its Roster.
//! The plaintext is padded to one of four buckets, so the ciphertext length tells only a
//! coarse length class.
//!
//! **Limits** (ADR-0011): the sender is authenticated relative to the recipient's seal key,
//! not by a signature: whoever holds a seal secret can open every push sealed to it and can
//! forge pushes to that recipient that appear to come from any Daemon. Nothing is
//! forward-secret, and registering a new seal key does not protect pushes recorded earlier.
//! A replayed push opens again: the Mobile refuses it by its `seq` and `at`
//! ([`check_fresh`]).
//!
#![doc = include_str!("../../PUSH.md")]

use super::json::strict_object;
use super::{b64, DeviceKeys, NoiseKey, RingId, SignKey, MAX_SAFE_INT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snow::params::NoiseParams;
use snow::Builder;
use std::fmt;
use uuid::Uuid;
use zeroize::Zeroizing;

/// The sealing handshake: one message, initiator = the Daemon.
pub const PUSH_PATTERN: &str = "Noise_X_25519_ChaChaPoly_BLAKE2s";
/// Prefix of the Noise prologue; the Ring id follows it.
pub const PUSH_PROLOGUE: &str = "xshell-push-v1\n";
/// The sealed form's first byte.
pub const SEAL_VERSION: u8 = 1;
/// The padded plaintext is exactly one of these lengths: the smallest that fits.
pub const BUCKETS: [usize; 4] = [512, 1024, 1536, 2048];
/// The longest JSON body: the largest bucket minus the 2-byte length.
pub const MAX_JSON: usize = 2046;
/// A `sealedPayload` is at most this many b64u characters (the Push Gateway's limit).
pub const MAX_SEALED_PAYLOAD: usize = 3072;
/// A push blob (`xpb1.…`) is at most this long, so a `push` frame stays under 8 KiB.
pub const MAX_BLOB: usize = 4096;
/// A push blob is at least this long.
pub const MIN_BLOB: usize = 8;
/// Every push blob starts with this.
pub const BLOB_PREFIX: &str = "xpb1.";
/// A `collapseId` is 1 to this many bytes.
pub const MAX_COLLAPSE_ID: usize = 64;
/// Prefix of the bytes hashed into a [`collapse_id`].
pub const COLLAPSE_CONTEXT: &str = "xshell-push-collapse-v1\n";
/// `title` is cut to at most this many bytes.
pub const MAX_TITLE: usize = 120;
/// A Mobile refuses a push whose `at` is older than this (ms) …
pub const MAX_AGE_MS: u64 = 24 * 3600 * 1000;
/// … or further than this in the future (ms).
pub const MAX_SKEW_MS: u64 = 5 * 60 * 1000;
/// What a shortened `project` starts with.
pub const ELLIPSIS: &str = "…";

/// `e (32) ‖ encrypted s (32 + 16)` before the encrypted payload, and its tag after it.
const OVERHEAD: usize = 32 + 48 + 16;

/// What a push reports.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum PushStatus {
    NeedsYou,
    Finished,
}

/// The agent of the Terminal a push is about.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum PushAgent {
    Claude,
    Codex,
}

/// A push's plaintext.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PushPayload {
    /// The Daemon (its sign key) whose Terminal this is.
    pub host: SignKey,
    pub terminal: Uuid,
    pub status: PushStatus,
    pub agent: PushAgent,
    /// The Terminal's working directory (its Project).
    pub project: String,
    /// The Terminal's title, at most [`MAX_TITLE`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Unix ms of the status change.
    pub at: u64,
    /// Strictly increasing per Daemon and Mobile: the Mobile refuses one at or below the
    /// highest it has seen from this Daemon.
    pub seq: u64,
    /// How many of this Host's Terminals need you now.
    pub needs_you: u32,
}

/// The JSON body as sealed: `v` first.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Body<'a> {
    v: u32,
    #[serde(flatten)]
    p: &'a PushPayload,
}

#[derive(Deserialize)]
struct Version {
    v: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    /// Over [`MAX_SEALED_PAYLOAD`], or a payload too long to fit even when shortened.
    TooLarge,
    /// Not canonical b64u.
    Encoding,
    /// A version byte or `v` other than 1.
    Version,
    /// Bad keys, a failed handshake or AEAD tag.
    Crypto,
    /// A bad plaintext: the bucket, `len`, padding or JSON.
    Malformed(String),
    /// `seq` at or below the highest seen ([`check_fresh`]).
    Replayed,
    /// `at` outside the accepted window ([`check_fresh`]).
    Stale,
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PushError::TooLarge => f.write_str("push payload too large"),
            PushError::Encoding => f.write_str("not canonical base64url"),
            PushError::Version => f.write_str("unknown push version"),
            PushError::Crypto => f.write_str("the push does not open"),
            PushError::Malformed(m) => write!(f, "malformed push: {m}"),
            PushError::Replayed => f.write_str("push replayed"),
            PushError::Stale => f.write_str("push outside its time window"),
        }
    }
}

impl std::error::Error for PushError {}

fn params() -> NoiseParams {
    // A constant that parses.
    PUSH_PATTERN.parse().expect("X pattern")
}

/// `"xshell-push-v1\n" ‖ ringId`.
pub fn prologue(ring: &RingId) -> Vec<u8> {
    format!("{PUSH_PROLOGUE}{ring}").into_bytes()
}

fn cut_end(s: &mut String, max: usize) {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
}

/// Drops at least `n` bytes from the start of `s` (whole characters).
fn cut_start(s: &str, n: usize) -> String {
    let mut i = n.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    s[i..].to_string()
}

/// The JSON body `p` is sealed as: `title` cut to [`MAX_TITLE`], then, while it exceeds
/// [`MAX_JSON`] bytes, `project` shortened from the left (starting with [`ELLIPSIS`]), then
/// `title` from the right.
pub fn body(p: &PushPayload) -> Result<Vec<u8>, PushError> {
    if p.at > MAX_SAFE_INT || p.seq > MAX_SAFE_INT {
        return Err(PushError::Malformed("integer above 2^53-1".into()));
    }
    let mut p = p.clone();
    if let Some(t) = p.title.as_mut() {
        cut_end(t, MAX_TITLE);
    }
    let encode = |p: &PushPayload| serde_json::to_vec(&Body { v: 1, p }).unwrap_or_default();
    let mut json = encode(&p);
    while json.len() > MAX_JSON {
        let over = json.len() - MAX_JSON;
        if !p.project.is_empty() && p.project != ELLIPSIS {
            let rest = p.project.strip_prefix(ELLIPSIS).unwrap_or(&p.project);
            let rest = cut_start(rest, over.max(1));
            p.project = format!("{ELLIPSIS}{rest}");
        } else if let Some(t) = p.title.as_mut().filter(|t| !t.is_empty()) {
            let keep = t.len().saturating_sub(over.max(1));
            cut_end(t, keep);
        } else {
            return Err(PushError::TooLarge);
        }
        json = encode(&p);
    }
    Ok(json)
}

/// The padded plaintext: `len (u16 BE) ‖ json ‖ zeros`, as long as the smallest bucket that
/// holds it.
pub fn plaintext(p: &PushPayload) -> Result<Zeroizing<Vec<u8>>, PushError> {
    let json = body(p)?;
    let size = BUCKETS
        .iter()
        .copied()
        .find(|b| 2 + json.len() <= *b)
        .ok_or(PushError::TooLarge)?;
    let mut out = Zeroizing::new(Vec::with_capacity(size));
    out.extend_from_slice(&(json.len() as u16).to_be_bytes());
    out.extend_from_slice(&json);
    out.resize(size, 0);
    Ok(out)
}

/// Seals `p` from this device (a Daemon) to the Mobile's `seal_key` for `ring`: the
/// `sealedPayload` (b64u of `0x01 ‖ Noise X message`).
pub fn seal(
    keys: &DeviceKeys,
    ring: &RingId,
    seal_key: &NoiseKey,
    p: &PushPayload,
) -> Result<String, PushError> {
    let plain = plaintext(p)?;
    let secret = keys.noise_secret();
    let pro = prologue(ring);
    let mut hs = Builder::new(params())
        .local_private_key(&secret[..])
        .and_then(|b| b.remote_public_key(seal_key.as_bytes()))
        .and_then(|b| b.prologue(&pro))
        .and_then(|b| b.build_initiator())
        .map_err(|_| PushError::Crypto)?;
    let mut out = vec![0u8; 1 + OVERHEAD + plain.len()];
    out[0] = SEAL_VERSION;
    let n = hs
        .write_message(&plain, &mut out[1..])
        .map_err(|_| PushError::Crypto)?;
    out.truncate(1 + n);
    Ok(b64::encode(&out))
}

/// Opens a `sealedPayload` with the Mobile's seal secret: the sender's Noise static key and
/// the payload. The caller must check that the key is the `noiseKey` of the Roster member
/// whose sign key is `host`, a `daemon`, and then [`check_fresh`].
pub fn open(
    seal_secret: &[u8; 32],
    ring: &RingId,
    sealed: &str,
) -> Result<(NoiseKey, PushPayload), PushError> {
    if sealed.len() > MAX_SEALED_PAYLOAD {
        return Err(PushError::TooLarge);
    }
    let bytes = b64::decode(sealed).map_err(|_| PushError::Encoding)?;
    match bytes.first() {
        Some(&SEAL_VERSION) => {}
        Some(_) => return Err(PushError::Version),
        None => return Err(PushError::Malformed("empty".into())),
    }
    let msg = &bytes[1..];
    let body_len = msg
        .len()
        .checked_sub(OVERHEAD)
        .ok_or_else(|| PushError::Malformed("short".into()))?;
    if !BUCKETS.contains(&body_len) {
        return Err(PushError::Malformed("not a bucket size".into()));
    }
    // A small-order ephemeral would make `es` predictable.
    let e: [u8; 32] = msg[..32].try_into().map_err(|_| PushError::Crypto)?;
    NoiseKey::from_bytes(e).map_err(|_| PushError::Crypto)?;
    let pro = prologue(ring);
    let mut hs = Builder::new(params())
        .local_private_key(&seal_secret[..])
        .and_then(|b| b.prologue(&pro))
        .and_then(|b| b.build_responder())
        .map_err(|_| PushError::Crypto)?;
    let mut plain = Zeroizing::new(vec![0u8; body_len]);
    let n = hs
        .read_message(msg, &mut plain)
        .map_err(|_| PushError::Crypto)?;
    let rs: [u8; 32] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or(PushError::Crypto)?;
    let sender = NoiseKey::from_bytes(rs).map_err(|_| PushError::Crypto)?;
    let plain = &plain[..n];
    let len = u16::from_be_bytes([plain[0], plain[1]]) as usize;
    if 2 + len > plain.len() {
        return Err(PushError::Malformed("len past the body".into()));
    }
    if plain[2 + len..].iter().any(|b| *b != 0) {
        return Err(PushError::Malformed("non-zero padding".into()));
    }
    let json = &plain[2..2 + len];
    let obj = strict_object(json).map_err(PushError::Malformed)?;
    let v: Version = serde_json::from_value(serde_json::Value::Object(obj.clone()))
        .map_err(|e| PushError::Malformed(e.to_string()))?;
    if v.v != 1 {
        return Err(PushError::Version);
    }
    let p: PushPayload = serde_json::from_value(serde_json::Value::Object(obj))
        .map_err(|e| PushError::Malformed(e.to_string()))?;
    if p.at > MAX_SAFE_INT || p.seq > MAX_SAFE_INT {
        return Err(PushError::Malformed("integer above 2^53-1".into()));
    }
    Ok((sender, p))
}

/// The freshness rule a Mobile applies after [`open`]: `seq` must exceed the highest it has
/// seen from this Daemon (`highest`, kept per Daemon where the notification extension can
/// read it), and `at` must lie within [`MAX_AGE_MS`] before and [`MAX_SKEW_MS`] after
/// `now_ms`. A push that passes raises the highest seen to its `seq`.
pub fn check_fresh(p: &PushPayload, now_ms: u64, highest: Option<u64>) -> Result<(), PushError> {
    if highest.is_some_and(|h| p.seq <= h) {
        return Err(PushError::Replayed);
    }
    if p.at.saturating_add(MAX_AGE_MS) < now_ms || p.at > now_ms.saturating_add(MAX_SKEW_MS) {
        return Err(PushError::Stale);
    }
    Ok(())
}

/// `b64u(SHA-256("xshell-push-collapse-v1\n" ‖ hostSignKey))`, first 22 characters: an opaque,
/// stable id per Host, so a newer push from a Host replaces the older notification. APNs and
/// FCM never see a Ring key.
pub fn collapse_id(host: &SignKey) -> String {
    let mut h = Sha256::new();
    h.update(COLLAPSE_CONTEXT.as_bytes());
    h.update(host.to_b64().as_bytes());
    let mut s = b64::encode(&h.finalize());
    s.truncate(22);
    s
}

/// The seal key of a seal secret (an X25519 scalar, clamped as X25519 does).
pub fn seal_key_of(secret: &[u8; 32]) -> Result<NoiseKey, PushError> {
    let s = x25519_dalek::StaticSecret::from(*secret);
    NoiseKey::from_bytes(x25519_dalek::PublicKey::from(&s).to_bytes())
        .map_err(|_| PushError::Crypto)
}

/// Whether `blob` has a push blob's shape: [`BLOB_PREFIX`], [`MIN_BLOB`] to [`MAX_BLOB`]
/// bytes of `[A-Za-z0-9_.-]`. Only the Push Gateway can open one.
pub fn blob_well_formed(blob: &str) -> bool {
    (MIN_BLOB..=MAX_BLOB).contains(&blob.len())
        && blob.starts_with(BLOB_PREFIX)
        && blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Whether `id` is a valid `collapseId`: 1 to [`MAX_COLLAPSE_ID`] bytes.
pub fn collapse_id_well_formed(id: &str) -> bool {
    (1..=MAX_COLLAPSE_ID).contains(&id.len())
}

/// Whether `sealed` is canonical b64u of at most [`MAX_SEALED_PAYLOAD`] characters.
pub fn sealed_well_formed(sealed: &str) -> bool {
    !sealed.is_empty() && sealed.len() <= MAX_SEALED_PAYLOAD && b64::decode(sealed).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon() -> DeviceKeys {
        DeviceKeys::from_seeds(&[0x31; 32], &[0x32; 32])
    }

    fn ring() -> RingId {
        RingId::derive(&DeviceKeys::from_seeds(&[0x11; 32], &[0x12; 32]).sign_key())
    }

    const SECRET: [u8; 32] = [0x77; 32];

    fn payload() -> PushPayload {
        PushPayload {
            host: daemon().sign_key(),
            terminal: Uuid::from_u128(7),
            status: PushStatus::NeedsYou,
            agent: PushAgent::Claude,
            project: "/home/me/app".into(),
            title: Some("fix the build".into()),
            at: 1_767_225_600_000,
            seq: 1,
            needs_you: 1,
        }
    }

    #[test]
    fn round_trips_and_names_the_sender() {
        let p = payload();
        let s = seal(&daemon(), &ring(), &seal_key_of(&SECRET).unwrap(), &p).unwrap();
        let (who, back) = open(&SECRET, &ring(), &s).unwrap();
        assert_eq!(who, daemon().noise_key());
        assert_eq!(back, p);
        // Random ephemeral: two seals differ.
        let s2 = seal(&daemon(), &ring(), &seal_key_of(&SECRET).unwrap(), &p).unwrap();
        assert_ne!(s, s2);
    }

    #[test]
    fn buckets_bound_the_length() {
        let mut p = payload();
        for (project, size) in [(10, 512), (700, 1024), (1200, 1536), (1700, 2048)] {
            p.project = "p".repeat(project);
            assert_eq!(plaintext(&p).unwrap().len(), size, "{project}");
            let s = seal(&daemon(), &ring(), &seal_key_of(&SECRET).unwrap(), &p).unwrap();
            assert_eq!(b64::decode(&s).unwrap().len(), 1 + OVERHEAD + size);
            assert!(s.len() <= MAX_SEALED_PAYLOAD);
        }
    }

    #[test]
    fn long_fields_are_shortened() {
        let mut p = payload();
        p.project = format!("/{}", "é".repeat(3000));
        p.title = Some("t".repeat(500));
        let json = body(&p).unwrap();
        assert!(json.len() <= MAX_JSON);
        let back: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let proj = back["project"].as_str().unwrap();
        assert!(proj.starts_with(ELLIPSIS) && proj.ends_with('é'), "{proj}");
        assert_eq!(back["title"].as_str().unwrap().len(), MAX_TITLE);
        let s = seal(&daemon(), &ring(), &seal_key_of(&SECRET).unwrap(), &p).unwrap();
        assert!(open(&SECRET, &ring(), &s).is_ok());
    }

    #[test]
    fn refusals() {
        let p = payload();
        let s = seal(&daemon(), &ring(), &seal_key_of(&SECRET).unwrap(), &p).unwrap();
        // Wrong secret, another Ring.
        assert_eq!(open(&[0x78; 32], &ring(), &s), Err(PushError::Crypto));
        let other = RingId::derive(&daemon().sign_key());
        assert_eq!(open(&SECRET, &other, &s), Err(PushError::Crypto));
        // A flipped tag byte, another version.
        let mut b = b64::decode(&s).unwrap();
        let last = b.len() - 1;
        b[last] ^= 1;
        assert_eq!(
            open(&SECRET, &ring(), &b64::encode(&b)),
            Err(PushError::Crypto)
        );
        b[last] ^= 1;
        b[0] = 2;
        assert_eq!(
            open(&SECRET, &ring(), &b64::encode(&b)),
            Err(PushError::Version)
        );
        // Too long, padded, empty.
        assert_eq!(
            open(&SECRET, &ring(), &"A".repeat(3073)),
            Err(PushError::TooLarge)
        );
        assert_eq!(
            open(&SECRET, &ring(), &format!("{s}=")),
            Err(PushError::Encoding)
        );
        assert!(open(&SECRET, &ring(), "").is_err());
    }

    #[test]
    fn freshness() {
        let p = payload();
        let now = p.at + 1000;
        assert_eq!(check_fresh(&p, now, None), Ok(()));
        assert_eq!(check_fresh(&p, now, Some(0)), Ok(()));
        assert_eq!(check_fresh(&p, now, Some(1)), Err(PushError::Replayed));
        assert_eq!(
            check_fresh(&p, p.at + MAX_AGE_MS + 1, None),
            Err(PushError::Stale)
        );
        assert_eq!(
            check_fresh(&p, p.at - MAX_SKEW_MS - 1, None),
            Err(PushError::Stale)
        );
    }

    #[test]
    fn collapse_id_is_stable_and_short() {
        let a = collapse_id(&daemon().sign_key());
        assert_eq!(a.len(), 22);
        assert_eq!(a, collapse_id(&daemon().sign_key()));
        let other = DeviceKeys::from_seeds(&[1; 32], &[2; 32]).sign_key();
        assert_ne!(a, collapse_id(&other));
    }

    #[test]
    fn shapes() {
        assert!(blob_well_formed("xpb1.kid.AAAA"));
        assert!(!blob_well_formed("xpb1."));
        assert!(!blob_well_formed("xpb2.kid.AAAA"));
        assert!(!blob_well_formed("xpb1.kid.AA=A"));
        assert!(!blob_well_formed(&format!("xpb1.{}", "a".repeat(MAX_BLOB))));
        assert!(collapse_id_well_formed("a") && !collapse_id_well_formed(""));
        assert!(!collapse_id_well_formed(&"a".repeat(65)));
        assert!(sealed_well_formed("AAAA") && !sealed_well_formed("AA=="));
    }
}

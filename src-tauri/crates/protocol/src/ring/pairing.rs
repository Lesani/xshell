//! Pairing (sans-IO): the one-time secret a Desktop shows (a QR payload for a phone, a code
//! for a computer), and the `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` handshake that proves
//! both sides know it and carries the new member's keys to the Desktop. The Relay's pairing
//! pipe (`/v1/pair/{slot}`, `RELAY.md` section 16) only joins the two sockets; the driver is
//! `ring::relay::pair`.
//!
//! Concrete types only, no generics or lifetimes in the API, so the Mobile can bind it
//! through UniFFI.
#![doc = include_str!("../../SESSIONS.md")]

#[path = "adoption.rs"]
pub mod adoption;

use super::json::strict_object;
use super::url::RelayUrl;
use super::{
    b64, member_name, verify, DeviceKeys, NoiseKey, RingId, Role, SignKey, Signature, Signer,
    MAX_SAFE_INT,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, StatelessTransportState};
use std::fmt;
use zeroize::Zeroizing;

/// The pairing handshake.
pub const PAIR_PATTERN: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
/// Prefix of the bytes hashed into a pipe slot.
pub const SLOT_CONTEXT: &str = "xshell-pair-slot-v1\n";
/// Prefix of the bytes hashed into the pre-shared key.
pub const PSK_CONTEXT: &str = "xshell-pair-psk-v1\n";
/// Prefix of the pairing handshake's prologue (the slot follows).
pub const PAIR_PROLOGUE: &str = "xshell-pair-v1\n";
/// Prefix of the bytes a joining device signs over the handshake hash.
pub const POP_CONTEXT: &str = "xshell-pair-pop-v1\n";
/// Every QR payload starts with this.
pub const OFFER_PREFIX: &str = "xsp1.";
/// A QR payload is at most this long.
pub const MAX_OFFER: usize = 2048;
/// A pairing message (one `pair.msg` payload, decoded) is at most this long.
pub const MAX_PAIR_MESSAGE: usize = 8192;
/// A QR secret's length.
pub const OFFER_SECRET_BYTES: usize = 32;
/// A pair code's length: 80 bits, so an attacker who records the handshake (a malicious
/// Relay posing as the other side) faces 2^80 offline guesses whatever the expiry.
pub const CODE_BYTES: usize = 10;
/// How long a Desktop keeps a secret valid.
pub const PAIR_TTL_SECS: u64 = 600;

/// Crockford base32.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Why pairing failed. [`PairError::as_code`] is what a UI maps to its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// The Desktop refused (its answer, authenticated by the secret).
    Refused(PairRefusal),
    /// A handshake message failed: the wrong secret, or a message altered on the way.
    Crypto,
    /// The Desktop's key is not the one in the QR payload.
    Pin,
    /// A decrypted message this version does not accept.
    Payload(String),
    /// A QR payload or a code that does not parse.
    Invalid(String),
    /// Nobody waits on this code's slot.
    NotFound,
    /// Nobody came before the secret expired.
    Expired,
    /// A step did not finish in time.
    Timeout,
    Cancelled,
    /// The Relay (or the network) failed, or broke the pipe's protocol.
    Relay(String),
}

impl PairError {
    pub fn as_code(&self) -> &'static str {
        match self {
            PairError::Refused(r) => r.as_code(),
            PairError::Crypto => "crypto",
            PairError::Pin => "pin",
            PairError::Payload(_) => "protocol",
            PairError::Invalid(_) => "invalid_code",
            PairError::NotFound => "not_found",
            PairError::Expired => "expired",
            PairError::Timeout => "timeout",
            PairError::Cancelled => "cancelled",
            PairError::Relay(_) => "relay",
        }
    }
}

impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairError::Refused(r) => write!(f, "the desktop refused: {}", r.as_code()),
            PairError::Crypto => f.write_str("the pairing handshake failed (wrong code?)"),
            PairError::Pin => f.write_str("the desktop's key does not match the pairing code"),
            PairError::Payload(m) => write!(f, "bad pairing message: {m}"),
            PairError::Invalid(m) => write!(f, "invalid pairing code: {m}"),
            PairError::NotFound => f.write_str("nobody is waiting with this code"),
            PairError::Expired => f.write_str("the code expired"),
            PairError::Timeout => f.write_str("timed out"),
            PairError::Cancelled => f.write_str("cancelled"),
            PairError::Relay(m) => write!(f, "relay: {m}"),
        }
    }
}

impl std::error::Error for PairError {}

/// The Desktop's refusal in message 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRefusal {
    /// The secret expired before the join.
    Expired,
    /// The secret was used already.
    Used,
    /// The device asked for a role this flow does not add (a QR adds a `mobile`, a code a
    /// `daemon`).
    Role,
    /// One of its keys is a member already, under other keys or another role.
    Duplicate,
    /// The Roster holds the most members it can.
    Full,
    /// The Desktop could not get the new Roster version to the Relay.
    PublishFailed,
    /// v2: the Desktop's accepted head moved since the offer (another change landed); scan a
    /// fresh offer. No version naming this device was committed.
    Stale,
    /// v2: the Desktop cancelled the offer (or quit) before any commit.
    Cancelled,
    /// v2: the Desktop proved that it committed nothing for this operation (it stopped
    /// before sending the candidate to the Relay).
    NotCommitted,
    /// v2: the Desktop sent the candidate to the Relay but has no acknowledgment: the
    /// enrollment may or may not have landed. Retrying with the same keys recovers it as an
    /// authenticated "already enrolled" completion if it did.
    PublicationUncertain,
}

const REFUSALS: &[(PairRefusal, &str)] = &[
    (PairRefusal::Expired, "expired"),
    (PairRefusal::Used, "used"),
    (PairRefusal::Role, "role"),
    (PairRefusal::Duplicate, "duplicate"),
    (PairRefusal::Full, "full"),
    (PairRefusal::PublishFailed, "publish_failed"),
    (PairRefusal::Stale, "stale"),
    (PairRefusal::Cancelled, "cancelled_by_desktop"),
    (PairRefusal::NotCommitted, "not_committed"),
    (PairRefusal::PublicationUncertain, "publication_uncertain"),
];

impl PairRefusal {
    pub fn as_code(&self) -> &'static str {
        REFUSALS
            .iter()
            .find(|(r, _)| r == self)
            .map(|(_, s)| *s)
            .unwrap_or("expired")
    }

    pub fn from_code(s: &str) -> Option<Self> {
        REFUSALS.iter().find(|(_, c)| *c == s).map(|(r, _)| *r)
    }
}

/// A pairing secret: 32 random bytes in a QR payload, the 10 bytes of a pair code. Wiped
/// when dropped.
#[derive(Clone)]
pub struct PairSecret(Zeroizing<Vec<u8>>);

impl fmt::Debug for PairSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairSecret(..)")
    }
}

fn random(n: usize) -> Result<Zeroizing<Vec<u8>>, PairError> {
    let mut v = Zeroizing::new(vec![0u8; n]);
    getrandom::getrandom(&mut v[..])
        .map_err(|e| PairError::Relay(format!("no randomness: {e}")))?;
    Ok(v)
}

impl PairSecret {
    /// A fresh 32-byte secret for a QR payload.
    pub fn generate() -> Result<Self, PairError> {
        Ok(PairSecret(random(OFFER_SECRET_BYTES)?))
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        PairSecret(Zeroizing::new(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn hash(&self, context: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(context.as_bytes());
        h.update(&self.0[..]);
        h.finalize().into()
    }

    /// `b64u(SHA-256("xshell-pair-slot-v1\n" ‖ secret))`: where both sides meet on the
    /// Relay, 43 characters.
    pub fn slot(&self) -> String {
        b64::encode(&self.hash(SLOT_CONTEXT))
    }

    /// `SHA-256("xshell-pair-psk-v1\n" ‖ secret)`: the handshake's pre-shared key.
    pub fn psk(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.hash(PSK_CONTEXT))
    }
}

/// Whether `s` is a well-formed slot (`^[A-Za-z0-9_-]{43}$`).
pub fn valid_slot(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The accepted head a v2 offer was issued at (QR anchor, BINDING AMENDMENT 1 of the #87
/// phone plan): the phone refuses a proposal whose verified predecessor is any other head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferHead {
    pub version: u64,
    /// `b64u(SHA-256(token))` of that version.
    pub hash: String,
}

/// What a Desktop's QR code carries: `"xsp1." + b64u(JSON)` with
/// `{"v":1,"ringId","relayUrl","signKey","noiseKey","secret","expiresAt"}`. A Desktop on a
/// v2 (membership) head emits `"v":2` with the head anchor `"headVersion","headHash"` as
/// well; the phone then joins through the v2 adoption transport
/// (`relay::pair::adopt_mobile_as_guest_with_ready`). An old app refuses v2 with
/// "unsupported version".
#[derive(Debug, Clone)]
pub struct PairingOffer {
    pub ring_id: RingId,
    pub relay_url: String,
    /// The Desktop's keys; the phone refuses any other Noise key in the handshake.
    pub sign_key: SignKey,
    pub noise_key: NoiseKey,
    pub secret: PairSecret,
    /// Unix seconds, shown to the user only: the Desktop enforces the expiry itself.
    pub expires_at: u64,
    /// `Some` exactly for a v2 offer.
    pub head: Option<OfferHead>,
}

impl PairingOffer {
    /// The offer's format version: 2 when it carries a head anchor.
    pub fn version(&self) -> u8 {
        if self.head.is_some() {
            2
        } else {
            1
        }
    }

    pub fn encode(&self) -> String {
        let secret = Zeroizing::new(b64::encode(self.secret.as_bytes()));
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            v: u8,
            ring_id: &'a RingId,
            relay_url: &'a str,
            sign_key: SignKey,
            noise_key: NoiseKey,
            secret: &'a str,
            expires_at: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            head_version: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            head_hash: Option<&'a str>,
        }
        // Borrow the credential, never clone it into a serde_json::Value.
        let body = Zeroizing::new(
            serde_json::to_string(&Body {
                v: self.version(),
                ring_id: &self.ring_id,
                relay_url: &self.relay_url,
                sign_key: self.sign_key,
                noise_key: self.noise_key,
                secret: &secret,
                expires_at: self.expires_at,
                head_version: self.head.as_ref().map(|h| h.version),
                head_hash: self.head.as_ref().map(|h| h.hash.as_str()),
            })
            .expect("pairing offer fields are serializable"),
        );
        format!("{OFFER_PREFIX}{}", b64::encode(body.as_bytes()))
    }

    /// Strict: the prefix, canonical b64u, one JSON object without duplicate keys, every
    /// field present and typed, `v == 1` or `v == 2` (then with `headVersion` ≥ 1 and a
    /// 32-byte `headHash`), a valid Relay URL and keys, a 32-byte secret. Unknown fields are
    /// ignored. Surrounding whitespace (a pasted line) is trimmed.
    pub fn parse(s: &str) -> Result<Self, PairError> {
        let bad = |m: &str| PairError::Invalid(m.to_string());
        let s = s.trim();
        if s.len() > MAX_OFFER {
            return Err(bad("too long"));
        }
        let body = s
            .strip_prefix(OFFER_PREFIX)
            .ok_or_else(|| bad("not a pairing code"))?;
        let bytes = Zeroizing::new(b64::decode(body).map_err(|_| bad("encoding"))?);
        let obj =
            PrivateOfferObject(strict_object(&bytes).map_err(|_| bad("invalid pairing object"))?);
        let text = |k: &str| {
            obj.0
                .get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| PairError::Invalid(format!("missing {k}")))
        };
        let v = obj.0.get("v").and_then(Value::as_u64);
        if v != Some(1) && v != Some(2) {
            return Err(bad("unsupported version"));
        }
        let head = if v == Some(2) {
            let version = obj
                .0
                .get("headVersion")
                .and_then(Value::as_u64)
                .filter(|n| (1..=MAX_SAFE_INT).contains(n))
                .ok_or_else(|| bad("headVersion"))?;
            let hash = text("headHash")?.to_string();
            let raw = b64::decode_array::<32>(&hash).map_err(|_| bad("headHash"))?;
            if b64::encode(&raw) != hash {
                return Err(bad("headHash"));
            }
            Some(OfferHead { version, hash })
        } else {
            None
        };
        let ring_id = RingId::parse(text("ringId")?).map_err(|_| bad("ringId"))?;
        let relay_url = text("relayUrl")?.to_string();
        RelayUrl::parse(&relay_url).map_err(|e| PairError::Invalid(e.to_string()))?;
        let sign_key = SignKey::parse(text("signKey")?).map_err(|_| bad("signKey"))?;
        let noise_key = NoiseKey::parse(text("noiseKey")?).map_err(|_| bad("noiseKey"))?;
        let mut secret = Zeroizing::new([0u8; OFFER_SECRET_BYTES]);
        b64::decode_into(text("secret")?, &mut secret).map_err(|_| bad("secret"))?;
        let expires_at = obj
            .0
            .get("expiresAt")
            .and_then(Value::as_u64)
            .filter(|n| *n <= MAX_SAFE_INT)
            .ok_or_else(|| bad("expiresAt"))?;
        Ok(PairingOffer {
            ring_id,
            relay_url,
            sign_key,
            noise_key,
            secret: PairSecret::from_bytes(&secret[..]),
            expires_at,
            head,
        })
    }
}

/// Parser-owned allocations may contain credentials even in malformed/unknown fields.
/// The initial serde parser and allocator scratch copies are not an erasure guarantee.
struct PrivateOfferObject(serde_json::Map<String, Value>);
impl Drop for PrivateOfferObject {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        fn wipe(value: &mut Value) {
            match value {
                Value::String(s) => s.zeroize(),
                Value::Array(values) => values.iter_mut().for_each(wipe),
                Value::Object(values) => {
                    for (mut key, mut value) in std::mem::take(values) {
                        key.zeroize();
                        wipe(&mut value);
                    }
                }
                _ => {}
            }
        }
        for (mut key, mut value) in std::mem::take(&mut self.0) {
            key.zeroize();
            wipe(&mut value);
        }
    }
}

/// The code `xshelld pair` shows: 10 random bytes as 16 Crockford base32 characters in groups
/// of four (`7KQ4-M2XW-9PJR-H3CT`).
#[derive(Clone)]
pub struct PairCode(Zeroizing<[u8; CODE_BYTES]>);

impl fmt::Debug for PairCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairCode(..)")
    }
}

impl PairCode {
    pub fn generate() -> Result<Self, PairError> {
        let r = random(CODE_BYTES)?;
        let mut b = Zeroizing::new([0u8; CODE_BYTES]);
        b.copy_from_slice(&r);
        Ok(PairCode(b))
    }

    pub fn from_bytes(bytes: [u8; CODE_BYTES]) -> Self {
        PairCode(Zeroizing::new(bytes))
    }

    /// Reads what a person typed: case, hyphens and spaces are ignored, `O` reads as `0` and
    /// `I`/`L` as `1`. Exactly 16 characters must remain.
    pub fn parse(input: &str) -> Result<Self, PairError> {
        let mut bits: u128 = 0;
        let mut n = 0;
        for c in input.chars() {
            let c = match c.to_ascii_uppercase() {
                '-' | ' ' => continue,
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            let v = CROCKFORD
                .iter()
                .position(|x| *x as char == c)
                .ok_or_else(|| PairError::Invalid("a code has only letters and digits".into()))?;
            n += 1;
            if n > 16 {
                return Err(PairError::Invalid("a code has 16 characters".into()));
            }
            bits = (bits << 5) | v as u128;
        }
        if n != 16 {
            return Err(PairError::Invalid("a code has 16 characters".into()));
        }
        let mut b = Zeroizing::new([0u8; CODE_BYTES]);
        b.copy_from_slice(&bits.to_be_bytes()[6..]);
        Ok(PairCode(b))
    }

    /// `XXXX-XXXX-XXXX-XXXX`.
    pub fn format(&self) -> String {
        let mut v = [0u8; 16];
        v[6..].copy_from_slice(&self.0[..]);
        let bits = u128::from_be_bytes(v);
        let mut out = String::with_capacity(19);
        for i in 0..16 {
            if i > 0 && i % 4 == 0 {
                out.push('-');
            }
            let shift = 5 * (15 - i);
            out.push(CROCKFORD[((bits >> shift) & 31) as usize] as char);
        }
        out
    }

    pub fn secret(&self) -> PairSecret {
        PairSecret::from_bytes(&self.0[..])
    }
}

/// What the joining device sends in message 3, as the Desktop read and checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    pub role: Role,
    pub sign_key: SignKey,
    /// The handshake's static key (not a payload field).
    pub noise_key: NoiseKey,
    /// Cleaned up with [`member_name`].
    pub name: String,
}

/// What the Desktop says in message 4 when it added the device: the Roster version that
/// lists it, which the device then fetches from the Relay and pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    pub ring_id: RingId,
    pub relay_url: String,
    pub version: u64,
    /// `b64u(SHA-256(token))` of that version.
    pub hash: String,
    pub signed_by: SignKey,
}

/// The Desktop's answer.
pub type JoinResult = Result<Joined, PairRefusal>;

/// What message 2 says about the Desktop. Not authenticated until message 4 arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHello {
    pub ring_id: RingId,
    pub name: String,
    pub noise_key: NoiseKey,
}

fn params() -> NoiseParams {
    PAIR_PATTERN.parse().expect("XXpsk3 pattern")
}

fn prologue(slot: &str) -> Vec<u8> {
    format!("{PAIR_PROLOGUE}{slot}").into_bytes()
}

fn handshake(
    keys: &DeviceKeys,
    secret: &PairSecret,
    initiator: bool,
) -> Result<HandshakeState, PairError> {
    let s = keys.noise_secret();
    let psk = secret.psk();
    let pro = prologue(&secret.slot());
    let b = Builder::new(params())
        .local_private_key(&s[..])
        .and_then(|b| b.psk(3, &psk))
        .and_then(|b| b.prologue(&pro))
        .map_err(|_| PairError::Crypto)?;
    if initiator {
        b.build_initiator()
    } else {
        b.build_responder()
    }
    .map_err(|_| PairError::Crypto)
}

fn write(hs: &mut HandshakeState, payload: &[u8]) -> Result<Vec<u8>, PairError> {
    let mut buf = vec![0u8; MAX_PAIR_MESSAGE];
    let n = hs
        .write_message(payload, &mut buf)
        .map_err(|_| PairError::Crypto)?;
    buf.truncate(n);
    Ok(buf)
}

fn read(hs: &mut HandshakeState, msg: &[u8]) -> Result<Zeroizing<Vec<u8>>, PairError> {
    if msg.len() > MAX_PAIR_MESSAGE {
        return Err(PairError::Payload("message too long".into()));
    }
    let mut buf = Zeroizing::new(vec![0u8; MAX_PAIR_MESSAGE]);
    let n = hs
        .read_message(msg, &mut buf)
        .map_err(|_| PairError::Crypto)?;
    buf.truncate(n);
    Ok(buf)
}

fn object(p: &[u8]) -> Result<serde_json::Map<String, Value>, PairError> {
    let o = strict_object(p).map_err(PairError::Payload)?;
    if o.get("v").and_then(Value::as_u64) != Some(1) {
        return Err(PairError::Payload("unsupported version".into()));
    }
    Ok(o)
}

fn field<'a>(o: &'a serde_json::Map<String, Value>, k: &str) -> Result<&'a str, PairError> {
    o.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| PairError::Payload(format!("missing {k}")))
}

fn pop_message(h: &[u8]) -> Vec<u8> {
    let mut m = POP_CONTEXT.as_bytes().to_vec();
    m.extend_from_slice(h);
    m
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Desktop => "desktop",
        Role::Daemon => "daemon",
        Role::Mobile => "mobile",
    }
}

/// The joining device's side (a phone, or `xshelld pair`): the Noise initiator.
pub struct Guest {
    final_hash: Option<[u8; 32]>,
    send_nonce: u64,
    recv_nonce: u64,
    hs: Option<HandshakeState>,
    transport: Option<StatelessTransportState>,
    pin: Option<NoiseKey>,
    /// The handshake hash after message 2, which the proof of possession signs.
    h2: Option<Vec<u8>>,
}

impl Guest {
    /// The actual final XXpsk3 hash, available only after message 3 succeeded.
    pub fn final_handshake_hash(&self) -> Result<[u8; 32], PairError> {
        self.final_hash.ok_or(PairError::Cancelled)
    }
    /// Ordered transport messages; nonces never reset between proposal, consent and ack.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, PairError> {
        if plaintext.len() > MAX_PAIR_MESSAGE - 16 {
            return Err(PairError::Payload("message too long".into()));
        }
        let t = self.transport.as_ref().ok_or(PairError::Cancelled)?;
        let mut buf = vec![0; plaintext.len() + 16];
        let next = self.send_nonce.checked_add(1).ok_or(PairError::Crypto)?;
        let n = t
            .write_message(self.send_nonce, plaintext, &mut buf)
            .map_err(|_| PairError::Crypto)?;
        self.send_nonce = next;
        buf.truncate(n);
        Ok(buf)
    }
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, PairError> {
        if ciphertext.len() > MAX_PAIR_MESSAGE {
            return Err(PairError::Payload("message too long".into()));
        }
        let t = self.transport.as_ref().ok_or(PairError::Cancelled)?;
        let mut buf = Zeroizing::new(vec![0; MAX_PAIR_MESSAGE]);
        let next = self.recv_nonce.checked_add(1).ok_or(PairError::Crypto)?;
        let n = t
            .read_message(self.recv_nonce, ciphertext, &mut buf)
            .map_err(|_| PairError::Crypto)?;
        self.recv_nonce = next;
        buf.truncate(n);
        Ok(buf)
    }

    /// Message 1. `pin`: the Desktop's Noise key from the QR payload (`None` for a code).
    pub fn start(
        keys: &DeviceKeys,
        secret: &PairSecret,
        pin: Option<NoiseKey>,
    ) -> Result<(Guest, Vec<u8>), PairError> {
        let mut hs = handshake(keys, secret, true)?;
        let msg = write(&mut hs, &[])?;
        Ok((
            Guest {
                final_hash: None,
                send_nonce: 0,
                recv_nonce: 0,
                hs: Some(hs),
                transport: None,
                pin,
                h2: None,
            },
            msg,
        ))
    }

    /// Reads message 2. With a pin, a Desktop key other than the pinned one ends pairing
    /// here, before this device says anything about itself.
    pub fn read_hello(&mut self, msg2: &[u8]) -> Result<HostHello, PairError> {
        let hs = self.hs.as_mut().ok_or(PairError::Cancelled)?;
        if hs.is_handshake_finished() || self.h2.is_some() {
            return Err(PairError::Payload("message 2 twice".into()));
        }
        let p = read(hs, msg2)?;
        let rs = hs.get_remote_static().ok_or(PairError::Crypto)?;
        let mut k = [0u8; 32];
        k.copy_from_slice(&rs[..32]);
        let noise_key = NoiseKey::from_bytes(k).map_err(|_| PairError::Crypto)?;
        if self.pin.is_some_and(|pin| pin != noise_key) {
            self.hs = None;
            return Err(PairError::Pin);
        }
        self.h2 = Some(hs.get_handshake_hash().to_vec());
        let o = object(&p)?;
        Ok(HostHello {
            ring_id: RingId::parse(field(&o, "ringId")?)
                .map_err(|_| PairError::Payload("ringId".into()))?,
            name: member_name(field(&o, "name").unwrap_or(""), "desktop"),
            noise_key,
        })
    }

    /// Message 3: this device's role, sign key, name, and its signature over the handshake
    /// hash (so the sign key is bound to this handshake).
    pub fn join(
        &mut self,
        keys: &DeviceKeys,
        role: Role,
        name: &str,
    ) -> Result<Vec<u8>, PairError> {
        let h2 = self
            .h2
            .take()
            .ok_or(PairError::Payload("join before hello".into()))?;
        let mut hs = self.hs.take().ok_or(PairError::Cancelled)?;
        let pop = Signature::from_bytes(
            Signer::sign(keys, &pop_message(&h2)).map_err(|e| PairError::Relay(e.to_string()))?,
        );
        let body = json!({
            "v": 1,
            "role": role_name(role),
            "signKey": keys.sign_key(),
            "name": name,
            "pop": pop,
        })
        .to_string();
        let msg = write(&mut hs, body.as_bytes())?;
        self.final_hash = Some(
            hs.get_handshake_hash()
                .try_into()
                .map_err(|_| PairError::Crypto)?,
        );
        self.transport = Some(
            hs.into_stateless_transport_mode()
                .map_err(|_| PairError::Crypto)?,
        );
        Ok(msg)
    }

    /// Message 4, the Desktop's answer: the first message that proves it knows the secret.
    pub fn finish(&mut self, msg4: &[u8]) -> Result<Joined, PairError> {
        let buf = self.decrypt(msg4)?;
        parse_answer(&buf)
    }
}

/// Whether a decrypted transport message is a message-4 style answer
/// (`{"ok":true|false,..}`) rather than another payload (a v2 proposal handle).
pub(crate) fn is_answer(buf: &[u8]) -> bool {
    object(buf).is_ok_and(|o| o.contains_key("ok"))
}

/// A decrypted message-4 answer: the Joined head, or the Desktop's typed refusal.
pub(crate) fn parse_answer(buf: &[u8]) -> Result<Joined, PairError> {
    let o = object(buf)?;
    match o.get("ok") {
        Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => {
            let code = field(&o, "error")?;
            return Err(PairRefusal::from_code(code)
                .map(PairError::Refused)
                .unwrap_or_else(|| PairError::Payload(format!("refused: {code}"))));
        }
        _ => return Err(PairError::Payload("message 4 without ok".into())),
    }
    let relay_url = field(&o, "relayUrl")?.to_string();
    RelayUrl::parse(&relay_url).map_err(|e| PairError::Payload(e.to_string()))?;
    let hash = field(&o, "hash")?.to_string();
    b64::decode_array::<32>(&hash).map_err(|_| PairError::Payload("hash".into()))?;
    Ok(Joined {
        ring_id: RingId::parse(field(&o, "ringId")?)
            .map_err(|_| PairError::Payload("ringId".into()))?,
        relay_url,
        version: o
            .get("version")
            .and_then(Value::as_u64)
            .filter(|v| (1..=MAX_SAFE_INT).contains(v))
            .ok_or_else(|| PairError::Payload("version".into()))?,
        hash,
        signed_by: SignKey::parse(field(&o, "signedBy")?)
            .map_err(|_| PairError::Payload("signedBy".into()))?,
    })
}

/// A decrypted v2 "already enrolled" completion (`{"ok":true,"enrolled":true,..}`).
pub(crate) fn is_enrolled_answer(buf: &[u8]) -> bool {
    object(buf).is_ok_and(|o| o.get("enrolled") == Some(&Value::Bool(true)))
}

/// The Desktop's side: the Noise responder.
pub struct Host {
    final_hash: Option<[u8; 32]>,
    send_nonce: u64,
    recv_nonce: u64,
    hs: Option<HandshakeState>,
    transport: Option<StatelessTransportState>,
    h2: Option<Vec<u8>>,
    step: u8,
}

impl Host {
    /// The actual final XXpsk3 hash, available only after message 3 succeeded.
    pub fn final_handshake_hash(&self) -> Result<[u8; 32], PairError> {
        self.final_hash.ok_or(PairError::Cancelled)
    }
    /// Ordered transport messages; nonces never reset between proposal, consent and ack.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, PairError> {
        if plaintext.len() > MAX_PAIR_MESSAGE - 16 {
            return Err(PairError::Payload("message too long".into()));
        }
        let t = self.transport.as_ref().ok_or(PairError::Cancelled)?;
        let mut buf = vec![0; plaintext.len() + 16];
        let next = self.send_nonce.checked_add(1).ok_or(PairError::Crypto)?;
        let n = t
            .write_message(self.send_nonce, plaintext, &mut buf)
            .map_err(|_| PairError::Crypto)?;
        self.send_nonce = next;
        buf.truncate(n);
        Ok(buf)
    }
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, PairError> {
        if ciphertext.len() > MAX_PAIR_MESSAGE {
            return Err(PairError::Payload("message too long".into()));
        }
        let t = self.transport.as_ref().ok_or(PairError::Cancelled)?;
        let mut buf = Zeroizing::new(vec![0; MAX_PAIR_MESSAGE]);
        let next = self.recv_nonce.checked_add(1).ok_or(PairError::Crypto)?;
        let n = t
            .read_message(self.recv_nonce, ciphertext, &mut buf)
            .map_err(|_| PairError::Crypto)?;
        self.recv_nonce = next;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn start(keys: &DeviceKeys, secret: &PairSecret) -> Result<Host, PairError> {
        Ok(Host {
            final_hash: None,
            send_nonce: 0,
            recv_nonce: 0,
            hs: Some(handshake(keys, secret, false)?),
            transport: None,
            h2: None,
            step: 0,
        })
    }

    /// Message 1 (an ephemeral key, no payload).
    pub fn read_start(&mut self, msg1: &[u8]) -> Result<(), PairError> {
        let hs = self.hs.as_mut().ok_or(PairError::Cancelled)?;
        if self.step != 0 {
            return Err(PairError::Payload("message 1 twice".into()));
        }
        let p = read(hs, msg1)?;
        if !p.is_empty() {
            return Err(PairError::Payload("message 1 carries a payload".into()));
        }
        self.step = 1;
        Ok(())
    }

    /// Message 2: the Desktop's static key and `{"v":1,"ringId","name"}`.
    pub fn hello(&mut self, ring_id: &RingId, name: &str) -> Result<Vec<u8>, PairError> {
        let hs = self.hs.as_mut().ok_or(PairError::Cancelled)?;
        if self.step != 1 {
            return Err(PairError::Payload("hello out of order".into()));
        }
        let body = json!({ "v": 1, "ringId": ring_id, "name": name }).to_string();
        let msg = write(hs, body.as_bytes())?;
        self.h2 = Some(hs.get_handshake_hash().to_vec());
        self.step = 2;
        Ok(msg)
    }

    /// Message 3: decrypts only with the secret; checks the keys and the proof of
    /// possession. The caller then decides (and must consume the secret first).
    pub fn read_join(&mut self, msg3: &[u8]) -> Result<JoinRequest, PairError> {
        if self.step != 2 {
            return Err(PairError::Payload("join out of order".into()));
        }
        let mut hs = self.hs.take().ok_or(PairError::Cancelled)?;
        let h2 = self.h2.take().ok_or(PairError::Cancelled)?;
        let p = read(&mut hs, msg3)?;
        let rs = hs.get_remote_static().ok_or(PairError::Crypto)?;
        let mut k = [0u8; 32];
        k.copy_from_slice(&rs[..32]);
        let noise_key =
            NoiseKey::from_bytes(k).map_err(|_| PairError::Payload("noise key".into()))?;
        let o = object(&p)?;
        let role = match field(&o, "role")? {
            "mobile" => Role::Mobile,
            "daemon" => Role::Daemon,
            "desktop" => Role::Desktop,
            other => return Err(PairError::Payload(format!("role {other}"))),
        };
        let sign_key = SignKey::parse(field(&o, "signKey")?)
            .map_err(|_| PairError::Payload("signKey".into()))?;
        let pop =
            Signature::parse(field(&o, "pop")?).map_err(|_| PairError::Payload("pop".into()))?;
        if !verify(&sign_key, &pop_message(&h2), &pop) {
            return Err(PairError::Payload("proof of possession".into()));
        }
        let fallback = if role == Role::Mobile {
            "phone"
        } else {
            "computer"
        };
        let name = member_name(field(&o, "name").unwrap_or(""), fallback);
        self.final_hash = Some(
            hs.get_handshake_hash()
                .try_into()
                .map_err(|_| PairError::Crypto)?,
        );
        self.transport = Some(
            hs.into_stateless_transport_mode()
                .map_err(|_| PairError::Crypto)?,
        );
        self.step = 3;
        Ok(JoinRequest {
            role,
            sign_key,
            noise_key,
            name,
        })
    }

    /// Message 4: the answer.
    pub fn answer(&mut self, result: &JoinResult) -> Result<Vec<u8>, PairError> {
        let body = match result {
            Ok(j) => json!({
                "v": 1,
                "ok": true,
                "ringId": j.ring_id,
                "relayUrl": j.relay_url,
                "version": j.version,
                "hash": j.hash,
                "signedBy": j.signed_by,
            }),
            Err(r) => json!({ "v": 1, "ok": false, "error": r.as_code() }),
        }
        .to_string();
        self.encrypt(body.as_bytes())
    }

    /// v2 "already enrolled" completion, sent instead of a proposal handle: `j` is the
    /// accepted head that already lists the (possession-proven) device. No new version.
    pub fn answer_enrolled(&mut self, j: &Joined) -> Result<Vec<u8>, PairError> {
        // Message-4 format (`"v":1`), marked as a recovery completion.
        let body = json!({
            "v": 1,
            "ok": true,
            "enrolled": true,
            "ringId": j.ring_id,
            "relayUrl": j.relay_url,
            "version": j.version,
            "hash": j.hash,
            "signedBy": j.signed_by,
        })
        .to_string();
        self.encrypt(body.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: u8) -> DeviceKeys {
        DeviceKeys::from_seeds(&[n; 32], &[n + 1; 32])
    }

    fn offer() -> PairingOffer {
        let d = keys(1);
        PairingOffer {
            ring_id: RingId::derive(&d.sign_key()),
            relay_url: "wss://relay.example.com".into(),
            sign_key: d.sign_key(),
            noise_key: d.noise_key(),
            secret: PairSecret::from_bytes(&[7; 32]),
            expires_at: 1_767_225_600,
            head: None,
        }
    }

    fn joined(desk: &DeviceKeys) -> Joined {
        Joined {
            ring_id: RingId::derive(&desk.sign_key()),
            relay_url: "wss://relay.example.com".into(),
            version: 3,
            hash: b64::encode(&[5; 32]),
            signed_by: desk.sign_key(),
        }
    }

    /// Runs the handshake; `tweak` may change each message on the way.
    fn run(
        guest_secret: &PairSecret,
        host_secret: &PairSecret,
        pin: Option<NoiseKey>,
        role: Role,
        answer: JoinResult,
        mut tweak: impl FnMut(u8, &mut Vec<u8>),
    ) -> (Result<Joined, PairError>, Option<JoinRequest>) {
        let (desk, phone) = (keys(1), keys(3));
        let (mut g, mut m1) = Guest::start(&phone, guest_secret, pin).unwrap();
        tweak(1, &mut m1);
        let mut h = Host::start(&desk, host_secret).unwrap();
        if let Err(e) = h.read_start(&m1) {
            return (Err(e), None);
        }
        let mut m2 = h.hello(&RingId::derive(&desk.sign_key()), "desk").unwrap();
        tweak(2, &mut m2);
        if let Err(e) = g.read_hello(&m2) {
            return (Err(e), None);
        }
        let mut m3 = g.join(&phone, role, "my phone").unwrap();
        tweak(3, &mut m3);
        let req = match h.read_join(&m3) {
            Ok(r) => r,
            Err(e) => return (Err(e), None),
        };
        let mut m4 = h.answer(&answer).unwrap();
        tweak(4, &mut m4);
        (g.finish(&m4), Some(req))
    }

    #[test]
    fn offer_round_trip() {
        let o = offer();
        let text = o.encode();
        assert!(text.starts_with("xsp1."));
        let back = PairingOffer::parse(&format!("  {text}\n")).unwrap();
        assert_eq!(back.ring_id, o.ring_id);
        assert_eq!(back.relay_url, o.relay_url);
        assert_eq!(back.sign_key, o.sign_key);
        assert_eq!(back.noise_key, o.noise_key);
        assert_eq!(back.secret.as_bytes(), o.secret.as_bytes());
        assert_eq!(back.expires_at, o.expires_at);
        assert_eq!(back.secret.slot(), o.secret.slot());
        assert!(valid_slot(&o.secret.slot()));
        assert_ne!(&o.secret.psk()[..], o.secret.slot().as_bytes());
    }

    #[test]
    fn offer_refusals() {
        let o = offer();
        let good: Value = serde_json::from_slice(
            &b64::decode(o.encode().strip_prefix(OFFER_PREFIX).unwrap()).unwrap(),
        )
        .unwrap();
        let enc = |v: &Value| format!("{OFFER_PREFIX}{}", b64::encode(v.to_string().as_bytes()));
        assert!(PairingOffer::parse(&enc(&good)).is_ok());
        let mut cases: Vec<String> = vec![
            String::new(),
            "xsp2.".into(),
            o.encode().replace("xsp1.", "xsp1.=="),
            format!("{OFFER_PREFIX}{}", "A".repeat(MAX_OFFER)),
            format!("{OFFER_PREFIX}{}", b64::encode(br#"{"v":1,"v":1}"#)),
        ];
        for (k, v) in [
            ("v", json!(2)),
            ("ringId", json!("short")),
            ("relayUrl", json!("ws://relay.example.com")),
            ("signKey", json!("AAAA")),
            ("noiseKey", json!(b64::encode(&[0u8; 32]))),
            ("secret", json!(b64::encode(&[1u8; 31]))),
            ("expiresAt", json!(-1)),
            ("expiresAt", json!(1.5)),
            ("expiresAt", json!(MAX_SAFE_INT + 1)),
        ] {
            let mut bad = good.clone();
            bad[k] = v;
            cases.push(enc(&bad));
        }
        for k in [
            "ringId",
            "relayUrl",
            "signKey",
            "noiseKey",
            "secret",
            "expiresAt",
            "v",
        ] {
            let mut bad = good.clone();
            bad.as_object_mut().unwrap().remove(k);
            cases.push(enc(&bad));
        }
        for c in &cases {
            assert!(PairingOffer::parse(c).is_err(), "{c}");
        }
        // Unknown fields are fine.
        let mut extra = good.clone();
        extra["future"] = json!({"x": 1});
        assert!(PairingOffer::parse(&enc(&extra)).is_ok());
        assert_eq!(format!("{:?}", o.secret), "PairSecret(..)");
    }

    #[test]
    fn v2_offer_carries_the_head_anchor_and_v1_stays_byte_compatible() {
        let v1 = offer();
        let body = |o: &PairingOffer| -> Value {
            serde_json::from_slice(
                &b64::decode(o.encode().strip_prefix(OFFER_PREFIX).unwrap()).unwrap(),
            )
            .unwrap()
        };
        let b1 = body(&v1);
        assert_eq!(b1["v"], json!(1));
        assert!(b1.get("headVersion").is_none() && b1.get("headHash").is_none());
        assert!(PairingOffer::parse(&v1.encode()).unwrap().head.is_none());
        let mut v2 = offer();
        v2.head = Some(OfferHead {
            version: 7,
            hash: b64::encode(&[3; 32]),
        });
        assert_eq!(v2.version(), 2);
        let b2 = body(&v2);
        assert_eq!(b2["v"], json!(2));
        assert_eq!(b2["headVersion"], json!(7));
        let back = PairingOffer::parse(&v2.encode()).unwrap();
        assert_eq!(back.head, v2.head);
        assert_eq!(back.version(), 2);
        assert_eq!(back.sign_key, v2.sign_key);
        let enc = |v: &Value| format!("{OFFER_PREFIX}{}", b64::encode(v.to_string().as_bytes()));
        let mut cases = vec![];
        for k in ["headVersion", "headHash"] {
            let mut bad = b2.clone();
            bad.as_object_mut().unwrap().remove(k);
            cases.push(bad);
        }
        for (k, v) in [
            ("headVersion", json!(0)),
            ("headVersion", json!(MAX_SAFE_INT + 1)),
            ("headVersion", json!("7")),
            ("headHash", json!(b64::encode(&[3; 31]))),
            ("headHash", json!("not base64!")),
            ("v", json!(3)),
        ] {
            let mut bad = b2.clone();
            bad[k] = v;
            cases.push(bad);
        }
        for c in &cases {
            assert!(PairingOffer::parse(&enc(c)).is_err(), "{c}");
        }
    }

    #[test]
    fn v2_typed_refusals_round_trip_and_enrolled_answers_are_distinct() {
        for r in [
            PairRefusal::Duplicate,
            PairRefusal::Full,
            PairRefusal::Stale,
            PairRefusal::Cancelled,
            PairRefusal::Expired,
            PairRefusal::NotCommitted,
            PairRefusal::PublicationUncertain,
        ] {
            assert_eq!(PairRefusal::from_code(r.as_code()), Some(r));
        }
        let codes: std::collections::HashSet<_> = REFUSALS.iter().map(|(_, c)| *c).collect();
        assert_eq!(codes.len(), REFUSALS.len());
        let desk = keys(1);
        let phone = keys(2);
        let secret = PairSecret::from_bytes(&[5; 32]);
        let (mut g, m1) = Guest::start(&phone, &secret, Some(desk.noise_key())).unwrap();
        let mut h = Host::start(&desk, &secret).unwrap();
        h.read_start(&m1).unwrap();
        g.read_hello(&h.hello(&RingId::derive(&desk.sign_key()), "d").unwrap())
            .unwrap();
        h.read_join(&g.join(&phone, Role::Mobile, "p").unwrap())
            .unwrap();
        let refusal = g
            .decrypt(&h.answer(&Err(PairRefusal::Stale)).unwrap())
            .unwrap();
        assert!(is_answer(&refusal) && !is_enrolled_answer(&refusal));
        assert_eq!(
            parse_answer(&refusal),
            Err(PairError::Refused(PairRefusal::Stale))
        );
        let j = joined(&desk);
        let enrolled = g.decrypt(&h.answer_enrolled(&j).unwrap()).unwrap();
        assert!(is_answer(&enrolled) && is_enrolled_answer(&enrolled));
        assert_eq!(parse_answer(&enrolled), Ok(j.clone()));
        let normal = g.decrypt(&h.answer(&Ok(j)).unwrap()).unwrap();
        assert!(is_answer(&normal) && !is_enrolled_answer(&normal));
        assert!(!is_answer(br#"{"ringId":"x"}"#));
    }

    #[test]
    fn code_normalizes() {
        let c = PairCode::from_bytes([0x3a, 0x7f, 0x01, 0x80, 0xff, 0x00, 0x12, 0x34, 0x56, 0x78]);
        let text = c.format();
        assert_eq!(text.len(), 19);
        assert_eq!(text.matches('-').count(), 3);
        let back = PairCode::parse(&text).unwrap();
        assert_eq!(back.secret().as_bytes(), c.secret().as_bytes());
        // Case, spacing and the look-alike letters.
        let messy = text
            .to_lowercase()
            .replace('-', " ")
            .replace('0', "o")
            .replace('1', "l");
        assert_eq!(PairCode::parse(&messy).unwrap().format(), text, "{messy}");
        assert_eq!(
            PairCode::parse("0000-0000-0000-000I")
                .unwrap()
                .secret()
                .as_bytes(),
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        for bad in [
            "",
            "0000-0000-0000-000",
            "0000-0000-0000-00000",
            "UUUU-0000-0000-0000",
            "0000_0000_0000_0000",
        ] {
            assert!(PairCode::parse(bad).is_err(), "{bad}");
        }
        let a = PairCode::generate().unwrap();
        assert_eq!(PairCode::parse(&a.format()).unwrap().format(), a.format());
        assert_eq!(a.secret().as_bytes().len(), CODE_BYTES);
    }

    #[test]
    fn xxpsk3_round_trip() {
        let s = PairSecret::from_bytes(&[9; 32]);
        let desk = keys(1);
        let (r, req) = run(
            &s,
            &s,
            Some(desk.noise_key()),
            Role::Mobile,
            Ok(joined(&desk)),
            |_, _| {},
        );
        assert_eq!(r.unwrap(), joined(&desk));
        let req = req.unwrap();
        let phone = keys(3);
        assert_eq!(req.role, Role::Mobile);
        assert_eq!(req.sign_key, phone.sign_key());
        assert_eq!(req.noise_key, phone.noise_key());
        assert_eq!(req.name, "my phone");
        // A refusal reaches the guest too.
        let (r, _) = run(
            &s,
            &s,
            None,
            Role::Daemon,
            Err(PairRefusal::Used),
            |_, _| {},
        );
        assert_eq!(r, Err(PairError::Refused(PairRefusal::Used)));
    }

    #[test]
    fn wrong_psk_fails() {
        let a = PairCode::parse("0000-0000-0000-0001").unwrap().secret();
        let b = PairCode::parse("0000-0000-0000-0002").unwrap().secret();
        let desk = keys(1);
        let (r, req) = run(&a, &b, None, Role::Daemon, Ok(joined(&desk)), |_, _| {});
        // The prologue (the slot) differs too: no message decrypts past the first.
        assert!(r.is_err());
        assert!(req.is_none(), "the desktop must never see a join");
        // Tampering with any message fails the handshake.
        for step in 1..=4 {
            let s = PairSecret::from_bytes(&[4; 32]);
            let (r, req) = run(&s, &s, None, Role::Daemon, Ok(joined(&desk)), |i, m| {
                if i == step {
                    let last = m.len() - 1;
                    m[last] ^= 0x80;
                }
            });
            assert!(r.is_err(), "tampered message {step} accepted");
            if step >= 3 {
                assert!(step == 4 || req.is_none());
            }
        }
    }

    #[test]
    fn qr_pins_desktop_noise_key() {
        let s = PairSecret::from_bytes(&[2; 32]);
        let desk = keys(1);
        let (r, req) = run(
            &s,
            &s,
            Some(keys(9).noise_key()),
            Role::Mobile,
            Ok(joined(&desk)),
            |_, _| {},
        );
        assert_eq!(r, Err(PairError::Pin));
        assert!(req.is_none(), "the phone said nothing about itself");
    }

    #[test]
    fn pop_required() {
        // A guest whose message 3 carries a signature over another hash.
        let s = PairSecret::from_bytes(&[3; 32]);
        let (desk, phone) = (keys(1), keys(3));
        let (mut g, m1) = Guest::start(&phone, &s, None).unwrap();
        let mut h = Host::start(&desk, &s).unwrap();
        h.read_start(&m1).unwrap();
        let m2 = h.hello(&RingId::derive(&desk.sign_key()), "desk").unwrap();
        g.read_hello(&m2).unwrap();
        g.h2 = Some(vec![0; 32]);
        let m3 = g.join(&phone, Role::Mobile, "p").unwrap();
        assert_eq!(
            h.read_join(&m3),
            Err(PairError::Payload("proof of possession".into()))
        );
    }

    #[test]
    fn role_mismatch_refused() {
        // The handshake reports the role asked for; the Desktop refuses it with `role`, and
        // the guest reads the refusal.
        let s = PairSecret::from_bytes(&[6; 32]);
        let desk = keys(1);
        let (r, req) = run(
            &s,
            &s,
            Some(desk.noise_key()),
            Role::Daemon,
            Err(PairRefusal::Role),
            |_, _| {},
        );
        assert_eq!(req.unwrap().role, Role::Daemon);
        assert_eq!(r, Err(PairError::Refused(PairRefusal::Role)));
        assert_eq!(PairError::Refused(PairRefusal::Role).as_code(), "role");
        for (r, c) in REFUSALS {
            assert_eq!(PairRefusal::from_code(c), Some(*r));
        }
    }
}

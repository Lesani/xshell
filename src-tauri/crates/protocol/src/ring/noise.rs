//! Pairwise Noise sessions between Ring members (sans-IO): the envelope header, the
//! `Noise_IK_25519_ChaChaPoly_BLAKE2s` handshake bound to the Ring and both sign keys, and
//! the transport that carries an ordered byte stream. A Relay carries the envelopes and sees
//! only ciphertext. The driver that runs this over a Relay connection is
//! `ring::relay::sessions`; the wire format is in `SESSIONS.md`, included in
//! [`super::pairing`]'s docs.
//!
//! **Strict order.** DATA messages carry their AEAD nonce `n` in clear, and the receiver
//! takes only `n == next`: a replayed, reordered, dropped or altered message ends the
//! session ([`Opener`] is dead after its first failure). The stream above never sees a gap.

use super::json::strict_object;
use super::relay::wire::MAX_ENVELOPE_PAYLOAD;
use super::{DeviceKeys, Member, RingId, Role, SignKey, SignedRoster, MAX_SAFE_INT};
use serde_json::Value;
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, StatelessTransportState};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// The session handshake.
pub const IK_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// Prefix of a session's Noise prologue.
pub const SESSION_PROLOGUE: &str = "xshell-noise-v1\n";
/// The header's version byte.
pub const HEADER_VERSION: u8 = 1;
/// `ver (u8) | kind (u8) | sid (16 bytes) | n (u64, big-endian)`.
pub const HEADER_LEN: usize = 26;
pub const SID_LEN: usize = 16;
/// The AEAD tag.
pub const TAG_LEN: usize = 16;
/// The largest stream chunk one DATA message carries: a full envelope payload minus the
/// header, the tag and the inner type byte.
pub const MAX_STREAM_CHUNK: usize = MAX_ENVELOPE_PAYLOAD - HEADER_LEN - TAG_LEN - 1;
/// A session ends after this many DATA messages in one direction (no rekey in v1); the
/// initiator then opens a new one.
pub const MAX_MESSAGES: u64 = 1 << 48;
/// How a DATA message's plaintext starts.
pub mod inner {
    /// Stream bytes.
    pub const STREAM: u8 = 0;
    /// The sender closes the session; the body is a UTF-8 reason.
    pub const CLOSE: u8 = 1;
    /// Reserved (ping); ignored by v1 receivers.
    pub const PING: u8 = 2;
    /// Reserved (rekey); ignored by v1 receivers.
    pub const REKEY: u8 = 3;
}
/// The longest close reason sent.
pub const MAX_CLOSE_REASON: usize = 256;

/// A session id, random per handshake, chosen by the initiator.
pub type Sid = [u8; SID_LEN];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The initiator's handshake message.
    Hs1 = 1,
    /// The responder's answer.
    Hs2 = 2,
    /// A transport message.
    Data = 3,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub kind: Kind,
    pub sid: Sid,
    /// The AEAD nonce of a DATA message; 0 in handshake messages.
    pub n: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = HEADER_VERSION;
        h[1] = self.kind as u8;
        h[2..18].copy_from_slice(&self.sid);
        h[18..].copy_from_slice(&self.n.to_be_bytes());
        h
    }

    /// Splits an envelope into its header and the Noise message after it.
    pub fn parse(env: &[u8]) -> Result<(Header, &[u8]), NoiseError> {
        if env.len() < HEADER_LEN {
            return Err(NoiseError::Header("shorter than the header"));
        }
        if env.len() > MAX_ENVELOPE_PAYLOAD {
            return Err(NoiseError::Header("longer than an envelope"));
        }
        if env[0] != HEADER_VERSION {
            return Err(NoiseError::Header("unknown version"));
        }
        let kind = match env[1] {
            1 => Kind::Hs1,
            2 => Kind::Hs2,
            3 => Kind::Data,
            _ => return Err(NoiseError::Header("unknown kind")),
        };
        let mut sid = [0u8; SID_LEN];
        sid.copy_from_slice(&env[2..18]);
        let mut n = [0u8; 8];
        n.copy_from_slice(&env[18..26]);
        let n = u64::from_be_bytes(n);
        if kind != Kind::Data && n != 0 {
            return Err(NoiseError::Header("a handshake message with n != 0"));
        }
        Ok((Header { kind, sid, n }, &env[HEADER_LEN..]))
    }
}

/// Why a session message was refused. The driver kills the session on every error of an
/// established session except [`NoiseError::WrongSession`] and [`NoiseError::Header`],
/// which it drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoiseError {
    /// Not a session envelope.
    Header(&'static str),
    /// A handshake from a key that is not a member of the head under the Relay's `from`.
    UnknownPeer,
    /// For another session id.
    WrongSession,
    /// The handshake or a message failed to decrypt.
    Crypto,
    /// Not the next message (a replay, a reorder or a gap).
    OutOfOrder { expected: u64, got: u64 },
    /// A decrypted payload this version does not accept.
    Payload(String),
    /// The responder refused the session (`forbidden`, …).
    Refused(String),
    /// [`MAX_MESSAGES`] sent.
    Exhausted,
    /// The session failed before.
    Dead,
}

impl fmt::Display for NoiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoiseError::Header(m) => write!(f, "bad session header: {m}"),
            NoiseError::UnknownPeer => f.write_str("not a member of the ring"),
            NoiseError::WrongSession => f.write_str("for another session"),
            NoiseError::Crypto => f.write_str("decryption failed"),
            NoiseError::OutOfOrder { expected, got } => {
                write!(f, "message {got} where {expected} was next")
            }
            NoiseError::Payload(m) => write!(f, "bad session payload: {m}"),
            NoiseError::Refused(e) => write!(f, "session refused: {e}"),
            NoiseError::Exhausted => f.write_str("session used up"),
            NoiseError::Dead => f.write_str("session ended"),
        }
    }
}

impl std::error::Error for NoiseError {}

fn params() -> NoiseParams {
    // A constant that parses; checked by the tests.
    IK_PATTERN.parse().expect("IK pattern")
}

/// `"xshell-noise-v1\n" ‖ ringId ‖ "\n" ‖ initiatorSignKey ‖ "\n" ‖ responderSignKey ‖ "\n" ‖
/// sid` (keys in b64u, the sid as its 16 raw bytes): binds the Relay's `from` and `to`, the
/// Ring and the session id into the handshake hash.
pub fn prologue(ring: &RingId, initiator: &SignKey, responder: &SignKey, sid: &Sid) -> Vec<u8> {
    let mut p = format!("{SESSION_PROLOGUE}{ring}\n{initiator}\n{responder}\n").into_bytes();
    p.extend_from_slice(sid);
    p
}

fn new_sid() -> Result<Sid, NoiseError> {
    let mut sid = [0u8; SID_LEN];
    getrandom::getrandom(&mut sid).map_err(|_| NoiseError::Crypto)?;
    Ok(sid)
}

fn envelope(h: Header, msg: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + msg.len());
    v.extend_from_slice(&h.encode());
    v.extend_from_slice(msg);
    v
}

fn write(hs: &mut HandshakeState, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
    let mut buf = vec![0u8; MAX_ENVELOPE_PAYLOAD];
    let n = hs
        .write_message(payload, &mut buf)
        .map_err(|_| NoiseError::Crypto)?;
    buf.truncate(n);
    Ok(buf)
}

fn read(hs: &mut HandshakeState, msg: &[u8]) -> Result<Vec<u8>, NoiseError> {
    let mut buf = vec![0u8; MAX_ENVELOPE_PAYLOAD];
    let n = hs
        .read_message(msg, &mut buf)
        .map_err(|_| NoiseError::Crypto)?;
    buf.truncate(n);
    Ok(buf)
}

fn payload_object(p: &[u8]) -> Result<serde_json::Map<String, Value>, NoiseError> {
    strict_object(p).map_err(NoiseError::Payload)
}

/// The initiator's side of one handshake. A Desktop or a Mobile opens sessions; a Daemon only
/// answers them.
pub struct Initiator {
    /// `None` once the attempt ended.
    hs: Option<HandshakeState>,
    sid: Sid,
}

impl Initiator {
    /// The first message to `peer`, timestamped `ts` (ms; it must grow per peer, see
    /// [`Freshness`]).
    pub fn start(
        keys: &DeviceKeys,
        ring: &RingId,
        peer: &Member,
        ts: u64,
    ) -> Result<(Initiator, Vec<u8>), NoiseError> {
        if ts > MAX_SAFE_INT {
            return Err(NoiseError::Payload("ts above 2^53-1".into()));
        }
        let sid = new_sid()?;
        let secret = keys.noise_secret();
        let pro = prologue(ring, &keys.sign_key(), &peer.sign_key, &sid);
        let mut hs = Builder::new(params())
            .local_private_key(&secret[..])
            .and_then(|b| b.remote_public_key(peer.noise_key.as_bytes()))
            .and_then(|b| b.prologue(&pro))
            .and_then(|b| b.build_initiator())
            .map_err(|_| NoiseError::Crypto)?;
        let msg = write(&mut hs, format!("{{\"ts\":{ts}}}").as_bytes())?;
        let env = envelope(
            Header {
                kind: Kind::Hs1,
                sid,
                n: 0,
            },
            &msg,
        );
        Ok((Initiator { hs: Some(hs), sid }, env))
    }

    pub fn sid(&self) -> Sid {
        self.sid
    }

    /// Reads the responder's answer. [`NoiseError::WrongSession`] and [`NoiseError::Header`]
    /// (not this handshake's answer) leave the initiator waiting; any other error ends the
    /// attempt, and later calls fail with [`NoiseError::Dead`].
    pub fn finish(&mut self, env: &[u8]) -> Result<Session, NoiseError> {
        if self.hs.is_none() {
            return Err(NoiseError::Dead);
        }
        let (h, msg) = Header::parse(env)?;
        if h.kind != Kind::Hs2 || h.sid != self.sid {
            return Err(NoiseError::WrongSession);
        }
        let mut hs = self.hs.take().ok_or(NoiseError::Dead)?;
        let p = read(&mut hs, msg)?;
        let obj = payload_object(&p)?;
        match obj.get("ok") {
            Some(Value::Bool(true)) => {}
            Some(Value::Bool(false)) => {
                let e = obj
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("refused")
                    .chars()
                    .take(64)
                    .collect();
                return Err(NoiseError::Refused(e));
            }
            _ => return Err(NoiseError::Payload("HS2 without ok".into())),
        }
        let t = hs
            .into_stateless_transport_mode()
            .map_err(|_| NoiseError::Crypto)?;
        Ok(Session {
            sid: self.sid,
            transport: Arc::new(t),
        })
    }
}

/// A verified first message, not answered yet: who sent it and when.
pub struct Hello {
    hs: HandshakeState,
    sid: Sid,
    member: Member,
    ts: u64,
}

/// Reads an initiator's first message. The sender must be a member of `head` whose Noise
/// key is the handshake's static key and whose sign key is the Relay's `from`; otherwise
/// [`NoiseError::UnknownPeer`], which the driver drops without an answer.
pub fn read_hello(
    keys: &DeviceKeys,
    ring: &RingId,
    from: &SignKey,
    head: &SignedRoster,
    env: &[u8],
) -> Result<Hello, NoiseError> {
    let (h, msg) = Header::parse(env)?;
    if h.kind != Kind::Hs1 {
        return Err(NoiseError::WrongSession);
    }
    let secret = keys.noise_secret();
    let pro = prologue(ring, from, &keys.sign_key(), &h.sid);
    let mut hs = Builder::new(params())
        .local_private_key(&secret[..])
        .and_then(|b| b.prologue(&pro))
        .and_then(|b| b.build_responder())
        .map_err(|_| NoiseError::Crypto)?;
    let p = read(&mut hs, msg)?;
    let rs = hs.get_remote_static().ok_or(NoiseError::Crypto)?;
    let member = head
        .roster()
        .members
        .iter()
        .find(|m| m.noise_key.as_bytes()[..] == rs[..])
        .filter(|m| &m.sign_key == from)
        .cloned()
        .ok_or(NoiseError::UnknownPeer)?;
    let obj = payload_object(&p)?;
    let ts = obj
        .get("ts")
        .and_then(Value::as_u64)
        .filter(|t| *t <= MAX_SAFE_INT)
        .ok_or_else(|| NoiseError::Payload("HS1 without ts".into()))?;
    Ok(Hello {
        hs,
        sid: h.sid,
        member,
        ts,
    })
}

impl Hello {
    pub fn member(&self) -> &Member {
        &self.member
    }

    pub fn ts(&self) -> u64 {
        self.ts
    }

    pub fn sid(&self) -> Sid {
        self.sid
    }

    /// Accepts: the session and the answer to send.
    pub fn accept(mut self) -> Result<(Session, Vec<u8>), NoiseError> {
        let msg = write(&mut self.hs, br#"{"ok":true}"#)?;
        let t = self
            .hs
            .into_stateless_transport_mode()
            .map_err(|_| NoiseError::Crypto)?;
        let reply = envelope(
            Header {
                kind: Kind::Hs2,
                sid: self.sid,
                n: 0,
            },
            &msg,
        );
        Ok((
            Session {
                sid: self.sid,
                transport: Arc::new(t),
            },
            reply,
        ))
    }

    /// Refuses with `error` (`forbidden`): the answer to send.
    pub fn refuse(mut self, error: &str) -> Result<Vec<u8>, NoiseError> {
        let body = serde_json::json!({ "ok": false, "error": error }).to_string();
        let msg = write(&mut self.hs, body.as_bytes())?;
        Ok(envelope(
            Header {
                kind: Kind::Hs2,
                sid: self.sid,
                n: 0,
            },
            &msg,
        ))
    }
}

/// How a responder maps a member's Roster role to a session: Desktops and Mobiles may open
/// one, Daemons never (`None`).
pub fn session_role(role: Role) -> Option<Role> {
    match role {
        Role::Desktop | Role::Mobile => Some(role),
        Role::Daemon => None,
    }
}

/// The newest handshake timestamp accepted per peer, in memory: a first message with a `ts`
/// not above it is a replay and never replaces a live session (as in WireGuard). A peer whose
/// clock moved back is refused until the responder restarts.
#[derive(Default, Debug)]
pub struct Freshness(HashMap<SignKey, u64>);

impl Freshness {
    /// Whether `ts` is newer than every accepted one from `peer`.
    pub fn fresh(&self, peer: &SignKey, ts: u64) -> bool {
        self.0.get(peer).is_none_or(|last| ts > *last)
    }

    /// Records an accepted `ts`.
    pub fn accept(&mut self, peer: SignKey, ts: u64) {
        let e = self.0.entry(peer).or_insert(0);
        *e = (*e).max(ts);
    }

    /// The `ts` an initiator sends next: the clock, or one above the last it sent.
    pub fn next(&mut self, peer: SignKey, now_ms: u64) -> u64 {
        let e = self.0.entry(peer).or_insert(0);
        *e = now_ms.max(*e + 1);
        *e
    }
}

/// An established session, before it is split into its two directions.
pub struct Session {
    sid: Sid,
    transport: Arc<StatelessTransportState>,
}

impl Session {
    pub fn sid(&self) -> Sid {
        self.sid
    }

    /// The sending and the receiving half; each keeps its own counter.
    pub fn split(self) -> (Sealer, Opener) {
        (
            Sealer {
                sid: self.sid,
                t: self.transport.clone(),
                n: 0,
                dead: false,
            },
            Opener {
                sid: self.sid,
                t: self.transport,
                n: 0,
                dead: false,
            },
        )
    }
}

/// Seals DATA messages in order.
pub struct Sealer {
    sid: Sid,
    t: Arc<StatelessTransportState>,
    n: u64,
    dead: bool,
}

impl Sealer {
    /// One DATA message of type `kind` with `body` (at most [`MAX_STREAM_CHUNK`] bytes).
    pub fn seal(&mut self, kind: u8, body: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if self.dead {
            return Err(NoiseError::Dead);
        }
        if body.len() > MAX_STREAM_CHUNK {
            return Err(NoiseError::Payload("chunk too large".into()));
        }
        if self.n >= MAX_MESSAGES {
            self.dead = true;
            return Err(NoiseError::Exhausted);
        }
        let mut plain = Vec::with_capacity(body.len() + 1);
        plain.push(kind);
        plain.extend_from_slice(body);
        let mut out = vec![0u8; HEADER_LEN + plain.len() + TAG_LEN];
        out[..HEADER_LEN].copy_from_slice(
            &Header {
                kind: Kind::Data,
                sid: self.sid,
                n: self.n,
            }
            .encode(),
        );
        let len = match self.t.write_message(self.n, &plain, &mut out[HEADER_LEN..]) {
            Ok(l) => l,
            Err(_) => {
                self.dead = true;
                return Err(NoiseError::Crypto);
            }
        };
        out.truncate(HEADER_LEN + len);
        self.n += 1;
        Ok(out)
    }

    /// Stream bytes, in as many messages as they need.
    pub fn seal_stream(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, NoiseError> {
        data.chunks(MAX_STREAM_CHUNK)
            .map(|c| self.seal(inner::STREAM, c))
            .collect()
    }

    /// The close message, with `reason` cut to [`MAX_CLOSE_REASON`] bytes.
    pub fn seal_close(&mut self, reason: &str) -> Result<Vec<u8>, NoiseError> {
        let mut end = reason.len().min(MAX_CLOSE_REASON);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        self.seal(inner::CLOSE, &reason.as_bytes()[..end])
    }

    pub fn sent(&self) -> u64 {
        self.n
    }
}

/// Opens DATA messages in order; dead after its first failure.
pub struct Opener {
    sid: Sid,
    t: Arc<StatelessTransportState>,
    n: u64,
    dead: bool,
}

impl Opener {
    /// The next message's type and body. [`NoiseError::WrongSession`] and
    /// [`NoiseError::Header`] (not this session's message) leave the opener as it was; every
    /// other error ends it.
    pub fn open(&mut self, env: &[u8]) -> Result<(u8, Vec<u8>), NoiseError> {
        if self.dead {
            return Err(NoiseError::Dead);
        }
        let (h, msg) = Header::parse(env)?;
        if h.kind != Kind::Data || h.sid != self.sid {
            return Err(NoiseError::WrongSession);
        }
        self.dead = true;
        if h.n != self.n {
            return Err(NoiseError::OutOfOrder {
                expected: self.n,
                got: h.n,
            });
        }
        let mut plain = vec![0u8; msg.len()];
        let len = self
            .t
            .read_message(h.n, msg, &mut plain)
            .map_err(|_| NoiseError::Crypto)?;
        plain.truncate(len);
        let Some(&kind) = plain.first() else {
            return Err(NoiseError::Payload("empty message".into()));
        };
        if kind > inner::REKEY {
            return Err(NoiseError::Payload(format!("unknown message type {kind}")));
        }
        if kind == inner::CLOSE && std::str::from_utf8(&plain[1..]).is_err() {
            return Err(NoiseError::Payload("close reason is not UTF-8".into()));
        }
        self.dead = false;
        self.n += 1;
        plain.remove(0);
        Ok((kind, plain))
    }

    pub fn received(&self) -> u64 {
        self.n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{RosterChain, SignedRoster};

    struct Ring {
        desk: DeviceKeys,
        daemon: DeviceKeys,
        mobile: DeviceKeys,
        chain: RosterChain,
    }

    fn ring() -> Ring {
        let (desk, daemon, mobile) = (
            DeviceKeys::from_seeds(&[1; 32], &[2; 32]),
            DeviceKeys::from_seeds(&[3; 32], &[4; 32]),
            DeviceKeys::from_seeds(&[5; 32], &[6; 32]),
        );
        let g = SignedRoster::genesis(&desk, desk.noise_key(), "d", "wss://r.example", 1).unwrap();
        let v2 = g
            .next(&desk, 2, |d| {
                d.add(Member::new(
                    "h",
                    Role::Daemon,
                    daemon.sign_key(),
                    daemon.noise_key(),
                    2,
                ));
                d.add(Member::new(
                    "m",
                    Role::Mobile,
                    mobile.sign_key(),
                    mobile.noise_key(),
                    2,
                ));
            })
            .unwrap();
        Ring {
            desk,
            daemon,
            mobile,
            chain: RosterChain::from_chain(vec![g, v2]).unwrap(),
        }
    }

    impl Ring {
        fn member(&self, k: &DeviceKeys) -> Member {
            self.chain.head().member(&k.sign_key()).unwrap().clone()
        }

        /// A session from `from` to the Daemon: both halves on each side.
        fn pair(&self, from: &DeviceKeys) -> ((Sealer, Opener), (Sealer, Opener), Member) {
            let id = self.chain.ring_id();
            let (mut i, hs1) = Initiator::start(from, id, &self.member(&self.daemon), 7).unwrap();
            let hello =
                read_hello(&self.daemon, id, &from.sign_key(), self.chain.head(), &hs1).unwrap();
            let m = hello.member().clone();
            let (rs, hs2) = hello.accept().unwrap();
            let is = i.finish(&hs2).unwrap();
            assert_eq!(is.sid(), rs.sid());
            (is.split(), rs.split(), m)
        }
    }

    #[test]
    fn ik_round_trip() {
        let r = ring();
        let ((mut is, mut io), (mut rs, mut ro), m) = r.pair(&r.mobile);
        assert_eq!(m.role, Role::Mobile);
        assert_eq!(session_role(m.role), Some(Role::Mobile));
        for i in 0..3u8 {
            let env = is.seal(inner::STREAM, &[i; 10]).unwrap();
            assert_eq!(ro.open(&env).unwrap(), (inner::STREAM, vec![i; 10]));
            let back = rs.seal(inner::STREAM, &[i + 1; 3]).unwrap();
            assert_eq!(io.open(&back).unwrap(), (inner::STREAM, vec![i + 1; 3]));
        }
        let c = is.seal_close("bye").unwrap();
        assert_eq!(ro.open(&c).unwrap(), (inner::CLOSE, b"bye".to_vec()));
        assert_eq!(ro.received(), 4);
        assert_eq!(is.sent(), 4);
    }

    #[test]
    fn snow_public_equals_noise_key() {
        // The responder sees the static key snow derived from the seed: it must be the
        // Roster's noiseKey (x25519-dalek), or no member would ever be found.
        let r = ring();
        let id = r.chain.ring_id();
        let (_, hs1) = Initiator::start(&r.desk, id, &r.member(&r.daemon), 1).unwrap();
        let hello = read_hello(&r.daemon, id, &r.desk.sign_key(), r.chain.head(), &hs1).unwrap();
        assert_eq!(hello.member().noise_key, r.desk.noise_key());
        assert_eq!(hello.member().role, Role::Desktop);
        assert_eq!(hello.ts(), 1);
        assert!(IK_PATTERN.parse::<NoiseParams>().is_ok());
    }

    #[test]
    fn header_refusals() {
        let h = Header {
            kind: Kind::Data,
            sid: [9; 16],
            n: 1 << 40,
        };
        let enc = h.encode();
        assert_eq!(Header::parse(&enc).unwrap(), (h, &[][..]));
        assert!(Header::parse(&enc[..25]).is_err());
        let mut bad = enc;
        bad[0] = 2;
        assert!(Header::parse(&bad).is_err());
        let mut bad = enc;
        bad[1] = 4;
        assert!(Header::parse(&bad).is_err());
        let mut bad = enc;
        bad[1] = 0;
        assert!(Header::parse(&bad).is_err());
        let mut hs = enc;
        hs[1] = Kind::Hs1 as u8;
        assert!(Header::parse(&hs).is_err(), "handshake with n != 0");
        assert!(Header::parse(&vec![1u8; MAX_ENVELOPE_PAYLOAD + 1]).is_err());
    }

    #[test]
    fn chunks_fill_max_payload() {
        let r = ring();
        let ((mut is, _), (_, mut ro), _) = r.pair(&r.desk);
        let data = vec![7u8; MAX_STREAM_CHUNK * 2 + 5];
        let envs = is.seal_stream(&data).unwrap();
        assert_eq!(envs.len(), 3);
        assert_eq!(envs[0].len(), MAX_ENVELOPE_PAYLOAD);
        assert_eq!(envs[2].len(), HEADER_LEN + 1 + 5 + TAG_LEN);
        let mut got = Vec::new();
        for e in &envs {
            got.extend(ro.open(e).unwrap().1);
        }
        assert_eq!(got, data);
        assert!(is
            .seal(inner::STREAM, &vec![0; MAX_STREAM_CHUNK + 1])
            .is_err());
    }

    #[test]
    fn bit_flip_kills() {
        let r = ring();
        let ((mut is, _), (_, mut ro), _) = r.pair(&r.mobile);
        let mut env = is.seal(inner::STREAM, b"secret").unwrap();
        let last = env.len() - 1;
        env[last] ^= 1;
        assert_eq!(ro.open(&env), Err(NoiseError::Crypto));
        // Dead: even the genuine next message is refused.
        let next = is.seal(inner::STREAM, b"x").unwrap();
        assert_eq!(ro.open(&next), Err(NoiseError::Dead));
    }

    #[test]
    fn replay_kills() {
        let r = ring();
        let ((mut is, _), (_, mut ro), _) = r.pair(&r.mobile);
        let a = is.seal(inner::STREAM, b"a").unwrap();
        ro.open(&a).unwrap();
        assert_eq!(
            ro.open(&a),
            Err(NoiseError::OutOfOrder {
                expected: 1,
                got: 0
            })
        );
        assert_eq!(
            ro.open(&is.seal(inner::STREAM, b"b").unwrap()),
            Err(NoiseError::Dead)
        );
    }

    #[test]
    fn reorder_kills() {
        let r = ring();
        let ((mut is, _), (_, mut ro), _) = r.pair(&r.mobile);
        let a = is.seal(inner::STREAM, b"a").unwrap();
        let b = is.seal(inner::STREAM, b"b").unwrap();
        assert!(matches!(ro.open(&b), Err(NoiseError::OutOfOrder { .. })));
        assert_eq!(ro.open(&a), Err(NoiseError::Dead));
        // A forged n (the nonce is authenticated) fails too.
        let ((mut is, _), (_, mut ro), _) = r.pair(&r.mobile);
        let mut a = is.seal(inner::STREAM, b"a").unwrap();
        a[25] = 1;
        let _ = is.seal(inner::STREAM, b"b").unwrap();
        assert!(ro.open(&a).is_err());
    }

    #[test]
    fn wrong_sid_dropped() {
        let r = ring();
        let ((mut is, _), (_, mut ro), _) = r.pair(&r.mobile);
        let mut env = is.seal(inner::STREAM, b"a").unwrap();
        env[2] ^= 1;
        assert_eq!(ro.open(&env), Err(NoiseError::WrongSession));
        assert_eq!(
            ro.open(b"junk"),
            Err(NoiseError::Header("shorter than the header"))
        );
        // Not dead: the session goes on.
        env[2] ^= 1;
        assert_eq!(ro.open(&env).unwrap().1, b"a");
    }

    #[test]
    fn prologue_binds_sign_keys() {
        let r = ring();
        let id = r.chain.ring_id();
        let (_, hs1) = Initiator::start(&r.mobile, id, &r.member(&r.daemon), 1).unwrap();
        // The Relay claims another sender: the prologue differs and decryption fails.
        assert_eq!(
            read_hello(&r.daemon, id, &r.desk.sign_key(), r.chain.head(), &hs1).err(),
            Some(NoiseError::Crypto)
        );
        // Another Ring id fails the same way.
        let other = RingId::derive(&r.mobile.sign_key());
        assert!(read_hello(
            &r.daemon,
            &other,
            &r.mobile.sign_key(),
            r.chain.head(),
            &hs1
        )
        .is_err());
        // A member's noise key under a non-member's sign key is unknown. A stranger whose
        // Relay name matches its prologue still is not in the head.
        let stranger = DeviceKeys::from_seeds(&[11; 32], &[12; 32]);
        let (_, hs1) = Initiator::start(&stranger, id, &r.member(&r.daemon), 1).unwrap();
        assert_eq!(
            read_hello(&r.daemon, id, &stranger.sign_key(), r.chain.head(), &hs1).err(),
            Some(NoiseError::UnknownPeer)
        );
        // A message to another responder does not decrypt.
        let (_, hs1) = Initiator::start(&r.mobile, id, &r.member(&r.desk), 1).unwrap();
        assert!(read_hello(&r.daemon, id, &r.mobile.sign_key(), r.chain.head(), &hs1).is_err());
    }

    #[test]
    fn hs1_ts_must_grow() {
        let mut f = Freshness::default();
        let k = DeviceKeys::from_seeds(&[1; 32], &[1; 32]).sign_key();
        assert!(f.fresh(&k, 5));
        f.accept(k, 5);
        assert!(!f.fresh(&k, 5));
        assert!(!f.fresh(&k, 4));
        assert!(f.fresh(&k, 6));
        // An initiator never repeats a ts, even if its clock does.
        let mut mine = Freshness::default();
        assert_eq!(mine.next(k, 100), 100);
        assert_eq!(mine.next(k, 100), 101);
        assert_eq!(mine.next(k, 50), 102);
        assert_eq!(mine.next(k, 500), 500);
    }

    #[test]
    fn refusal_reaches_the_initiator() {
        let r = ring();
        let id = r.chain.ring_id();
        // A Daemon-role peer opening a session (it never should) is refused.
        let (mut i, hs1) = Initiator::start(&r.daemon, id, &r.member(&r.desk), 1).unwrap();
        let hello = read_hello(&r.desk, id, &r.daemon.sign_key(), r.chain.head(), &hs1).unwrap();
        assert_eq!(session_role(hello.member().role), None);
        let hs2 = hello.refuse("forbidden").unwrap();
        assert_eq!(
            i.finish(&hs2).err(),
            Some(NoiseError::Refused("forbidden".into()))
        );
        assert_eq!(i.finish(&hs2).err(), Some(NoiseError::Dead));
        // An answer for another sid leaves the initiator waiting.
        let (mut i, hs1) = Initiator::start(&r.mobile, id, &r.member(&r.daemon), 1).unwrap();
        let (_, other) = Initiator::start(&r.mobile, id, &r.member(&r.daemon), 2).unwrap();
        let hello2 =
            read_hello(&r.daemon, id, &r.mobile.sign_key(), r.chain.head(), &other).unwrap();
        let (_, hs2) = hello2.accept().unwrap();
        assert_eq!(i.finish(&hs2).err(), Some(NoiseError::WrongSession));
        let hello = read_hello(&r.daemon, id, &r.mobile.sign_key(), r.chain.head(), &hs1).unwrap();
        let (_, hs2) = hello.accept().unwrap();
        assert!(i.finish(&hs2).is_ok());
    }
}

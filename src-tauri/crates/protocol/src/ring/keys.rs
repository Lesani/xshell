//! Device keys. Every Ring member has an Ed25519 key that signs (Roster versions, Relay
//! challenges) and an X25519 key for its Noise sessions (#9). Public keys travel as canonical
//! base64url without padding and compare as bytes.

use super::b64;
use ed25519_dalek::Signer as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use zeroize::Zeroizing;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    Encoding(b64::B64Error),
    /// Not a usable Ed25519 public key: not a curve point, or of small order.
    InvalidSignKey,
    /// Not a usable X25519 public key: a non-canonical encoding, or a point of small order
    /// (on the curve or its twist), which would make the shared secret predictable.
    InvalidNoiseKey,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Encoding(e) => write!(f, "bad key encoding: {e}"),
            KeyError::InvalidSignKey => f.write_str("not a valid Ed25519 public key"),
            KeyError::InvalidNoiseKey => f.write_str("not a valid X25519 public key"),
        }
    }
}

impl std::error::Error for KeyError {}

macro_rules! b64_newtype {
    ($name:ident, $len:literal) => {
        impl $name {
            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            pub fn to_b64(&self) -> String {
                b64::encode(&self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_b64())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.to_b64())
            }
        }

        impl FromStr for $name {
            type Err = KeyError;
            fn from_str(s: &str) -> Result<Self, KeyError> {
                Self::parse(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_b64())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                Self::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

/// An Ed25519 public key: who a device is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SignKey([u8; 32]);
b64_newtype!(SignKey, 32);

impl SignKey {
    /// Accepts only keys `verify_strict` can use: a valid point that is not of small order.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, KeyError> {
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&bytes)
            .map_err(|_| KeyError::InvalidSignKey)?;
        if vk.is_weak() {
            return Err(KeyError::InvalidSignKey);
        }
        Ok(SignKey(bytes))
    }

    pub fn parse(s: &str) -> Result<Self, KeyError> {
        Self::from_bytes(b64::decode_array::<32>(s).map_err(KeyError::Encoding)?)
    }
}

/// An X25519 public key: how a device is reached through Noise.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NoiseKey([u8; 32]);
b64_newtype!(NoiseKey, 32);

/// The u-coordinates of X25519's small-order points (curve and twist), canonical forms:
/// 0, 1, the two order-8 points, and p−1. Their non-canonical forms (u + p, and p, p+1) are
/// refused by the canonical-encoding check.
const SMALL_ORDER: [[u8; 32]; 5] = [
    [0; 32],
    [
        1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4,
        0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49,
        0xb8, 0x00,
    ],
    [
        0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83, 0xef,
        0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd, 0xd0, 0x9f,
        0x11, 0x57,
    ],
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
];

/// p = 2^255 − 19, little-endian.
const P: [u8; 32] = [
    0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
];

/// `u < p` with the high bit clear: the one canonical encoding of a field element.
fn is_canonical_u(b: &[u8; 32]) -> bool {
    if b[31] & 0x80 != 0 {
        return false;
    }
    for i in (0..32).rev() {
        if b[i] != P[i] {
            return b[i] < P[i];
        }
    }
    false // equal to p
}

impl NoiseKey {
    /// Accepts only canonical encodings that are not of small order.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, KeyError> {
        if !is_canonical_u(&bytes) || SMALL_ORDER.contains(&bytes) {
            return Err(KeyError::InvalidNoiseKey);
        }
        Ok(NoiseKey(bytes))
    }

    /// Encodings [`NoiseKey::from_bytes`] must refuse: the small-order points in canonical
    /// and non-canonical form, p, p+1, and the high bit set on a valid key. Test vectors.
    pub fn refused_examples() -> Vec<[u8; 32]> {
        let mut v: Vec<[u8; 32]> = SMALL_ORDER.to_vec();
        let add_p = |u: &[u8; 32]| {
            let mut out = [0u8; 32];
            let mut carry = 0u16;
            for i in 0..32 {
                let s = u[i] as u16 + P[i] as u16 + carry;
                out[i] = s as u8;
                carry = s >> 8;
            }
            out
        };
        // u + p for u in {0, 1, both order-8 points} (fits in 256 bits; high bit set or ≥ p).
        for u in &SMALL_ORDER[..4] {
            v.push(add_p(u));
        }
        let mut p_plus_1 = P;
        p_plus_1[0] += 1;
        v.push(p_plus_1);
        let mut high = [9u8; 32];
        high[31] |= 0x80;
        v.push(high);
        v
    }

    pub fn parse(s: &str) -> Result<Self, KeyError> {
        Self::from_bytes(b64::decode_array::<32>(s).map_err(KeyError::Encoding)?)
    }
}

/// An Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; 64]);
b64_newtype!(Signature, 64);

impl Signature {
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Signature(bytes)
    }

    pub fn parse(s: &str) -> Result<Self, KeyError> {
        Ok(Signature(
            b64::decode_array::<64>(s).map_err(KeyError::Encoding)?,
        ))
    }
}

/// Strict Ed25519 verification: canonical `S`, no small-order `R` or key.
pub fn verify(key: &SignKey, msg: &[u8], sig: &Signature) -> bool {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&key.0) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(&sig.0);
    vk.verify_strict(msg, &sig).is_ok()
}

/// A signer could not sign (a locked or missing keystore key, a refused biometric prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignError(pub String);

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "signing failed: {}", self.0)
    }
}

impl std::error::Error for SignError {}

/// Whatever holds a device's Ed25519 private key. [`DeviceKeys`] holds it in memory; a
/// Mobile's keystore implements this itself (the Secure Enclave has no Ed25519, so it wraps
/// the key), and may fail.
pub trait Signer: Send + Sync {
    fn sign_key(&self) -> SignKey;
    /// Signs `msg` exactly as given; callers add the domain-separation context.
    fn sign(&self, msg: &[u8]) -> Result<[u8; 64], SignError>;
}

/// A device's two private keys, held in memory and zeroed on drop.
pub struct DeviceKeys {
    sign: ed25519_dalek::SigningKey,
    noise_seed: Zeroizing<[u8; 32]>,
}

impl DeviceKeys {
    /// Fresh keys from the operating system's random source.
    pub fn generate() -> Result<Self, SignError> {
        let mut sign = Zeroizing::new([0u8; 32]);
        let mut noise = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(&mut sign[..])
            .and_then(|_| getrandom::getrandom(&mut noise[..]))
            .map_err(|e| SignError(format!("no randomness: {e}")))?;
        Ok(Self::from_seeds(&sign, &noise))
    }

    /// Keys from their 32-byte seeds (the Ed25519 secret key and the X25519 scalar).
    pub fn from_seeds(sign_seed: &[u8; 32], noise_seed: &[u8; 32]) -> Self {
        DeviceKeys {
            sign: ed25519_dalek::SigningKey::from_bytes(sign_seed),
            noise_seed: Zeroizing::new(*noise_seed),
        }
    }

    /// The seeds, for storage in the device's keystore.
    pub fn seeds(&self) -> (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>) {
        (
            Zeroizing::new(self.sign.to_bytes()),
            Zeroizing::new(*self.noise_seed),
        )
    }

    pub fn sign_key(&self) -> SignKey {
        // A key derived from a secret is never small-order.
        SignKey(self.sign.verifying_key().to_bytes())
    }

    pub fn noise_key(&self) -> NoiseKey {
        let secret = x25519_dalek::StaticSecret::from(*self.noise_seed);
        NoiseKey(x25519_dalek::PublicKey::from(&secret).to_bytes())
    }
}

impl fmt::Debug for DeviceKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKeys")
            .field("sign_key", &self.sign_key())
            .finish_non_exhaustive()
    }
}

impl Signer for DeviceKeys {
    fn sign_key(&self) -> SignKey {
        DeviceKeys::sign_key(self)
    }

    fn sign(&self, msg: &[u8]) -> Result<[u8; 64], SignError> {
        Ok(self.sign.sign(msg).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_sign() {
        let k = DeviceKeys::from_seeds(&[1; 32], &[2; 32]);
        let pk = k.sign_key();
        assert_eq!(SignKey::parse(&pk.to_b64()).unwrap(), pk);
        assert_eq!(
            NoiseKey::parse(&k.noise_key().to_b64()).unwrap(),
            k.noise_key()
        );
        let sig = Signature::from_bytes(Signer::sign(&k, b"hello").unwrap());
        assert!(verify(&pk, b"hello", &sig));
        assert!(!verify(&pk, b"hellp", &sig));
        let (s, n) = k.seeds();
        assert_eq!((*s, *n), ([1; 32], [2; 32]));
    }

    #[test]
    fn refuses_weak_and_malformed_keys() {
        // The identity point (small order) and a non-point.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert_eq!(SignKey::from_bytes(identity), Err(KeyError::InvalidSignKey));
        assert!(SignKey::parse(&b64::encode(&[0u8; 31])).is_err());
        assert!(SignKey::parse("not a key").is_err());
        for bad in NoiseKey::refused_examples() {
            assert_eq!(
                NoiseKey::from_bytes(bad),
                Err(KeyError::InvalidNoiseKey),
                "{bad:02x?}"
            );
        }
        // The X25519 base point (u = 9) and real keys are fine.
        let mut nine = [0u8; 32];
        nine[0] = 9;
        assert!(NoiseKey::from_bytes(nine).is_ok());
        let k = DeviceKeys::from_seeds(&[5; 32], &[6; 32]);
        assert!(NoiseKey::from_bytes(*k.noise_key().as_bytes()).is_ok());
    }

    #[test]
    fn non_canonical_signature_is_refused() {
        let k = DeviceKeys::from_seeds(&[3; 32], &[4; 32]);
        let mut sig = Signer::sign(&k, b"m").unwrap();
        // S + L is the classic malleated signature; verify_strict refuses it.
        const L: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
        ];
        let mut carry = 0u16;
        for i in 0..32 {
            let v = sig[32 + i] as u16 + L[i] as u16 + carry;
            sig[32 + i] = v as u8;
            carry = v >> 8;
        }
        assert!(!verify(&k.sign_key(), b"m", &Signature::from_bytes(sig)));
    }
}

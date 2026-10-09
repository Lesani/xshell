//! Push Gateway entitlement tokens (amendment A2), the same format as the gateway's
//! `push/core/entitlement.ts`:
//!
//! ```text
//! token     = "xet1." + b64u(payloadJSON) + "." + b64u(signature)
//! signature = Ed25519(gatewayKey, ASCII("xshell-entitlement-v1\n") || ASCII(b64u(payloadJSON)))
//! payload   = {"v":1,"kid","ringId","tier":"push"|"hosted","purchaseRef","issuedAt","expiresAt"}
//! kid       = b64u(SHA-256(raw 32-byte public key)[0..8])
//! ```
//!
//! Devices never sign these; the Hosted Relay verifies them to decide whether it routes for a
//! Ring.

use super::json::strict_object;
use super::{
    b64, verify, RingId, SignError, SignKey, Signature, Signer, ENTITLEMENT_PREFIX, MAX_SAFE_INT,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;

pub const ENTITLEMENT_CONTEXT: &str = "xshell-entitlement-v1\n";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Push,
    /// Hosted includes Push.
    Hosted,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EntitlementClaims {
    pub v: u32,
    pub kid: String,
    pub ring_id: String,
    pub tier: Tier,
    pub purchase_ref: String,
    /// Unix seconds.
    pub issued_at: u64,
    /// Unix seconds; the token is invalid from this instant on (no grace).
    pub expires_at: u64,
}

/// The gateway's `VerifyFailure` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntitlementError {
    Malformed,
    BadSignature,
    UnknownKid,
    RingMismatch,
    Expired,
    WrongTier,
}

impl EntitlementError {
    pub fn as_code(&self) -> &'static str {
        match self {
            EntitlementError::Malformed => "malformed",
            EntitlementError::BadSignature => "bad_signature",
            EntitlementError::UnknownKid => "unknown_kid",
            EntitlementError::RingMismatch => "ring_mismatch",
            EntitlementError::Expired => "expired",
            EntitlementError::WrongTier => "wrong_tier",
        }
    }
}

impl fmt::Display for EntitlementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_code())
    }
}

impl std::error::Error for EntitlementError {}

/// `b64u(SHA-256(raw public key)[0..8])`.
pub fn entitlement_kid(key: &SignKey) -> String {
    b64::encode(&Sha256::digest(key.as_bytes())[..8])
}

/// The Push Gateway's public keys a Relay trusts, by kid.
#[derive(Clone, Debug, Default)]
pub struct GatewayKeys {
    keys: Vec<(String, SignKey)>,
}

impl GatewayKeys {
    pub fn new(keys: &[SignKey]) -> Self {
        GatewayKeys {
            keys: keys.iter().map(|k| (entitlement_kid(k), *k)).collect(),
        }
    }

    fn get(&self, kid: &str) -> Option<&SignKey> {
        self.keys.iter().find(|(k, _)| k == kid).map(|(_, v)| v)
    }
}

fn signed_bytes(payload_part: &str) -> Vec<u8> {
    let mut v = ENTITLEMENT_CONTEXT.as_bytes().to_vec();
    v.extend_from_slice(payload_part.as_bytes());
    v
}

/// Verifies `token` for `ring` at `now` (Unix seconds), in the gateway's order of checks.
/// `hosted` additionally requires the Hosted tier.
pub fn verify_entitlement(
    token: &str,
    ring: &RingId,
    now: u64,
    keys: &GatewayKeys,
    hosted: bool,
) -> Result<EntitlementClaims, EntitlementError> {
    use EntitlementError::*;
    if token.len() > super::relay::wire::MAX_ENTITLEMENT_TOKEN {
        return Err(Malformed);
    }
    let rest = token.strip_prefix(ENTITLEMENT_PREFIX).ok_or(Malformed)?;
    let (payload_part, sig_part) = rest.split_once('.').ok_or(Malformed)?;
    let payload = b64::decode(payload_part).map_err(|_| Malformed)?;
    let obj = strict_object(&payload).map_err(|_| Malformed)?;
    let claims: EntitlementClaims =
        serde_json::from_value(Value::Object(obj)).map_err(|_| Malformed)?;
    if claims.v != 1 || claims.issued_at > MAX_SAFE_INT || claims.expires_at > MAX_SAFE_INT {
        return Err(Malformed);
    }
    let sig = Signature::parse(sig_part).map_err(|_| Malformed)?;
    let key = keys.get(&claims.kid).ok_or(UnknownKid)?;
    if !verify(key, &signed_bytes(payload_part), &sig) {
        return Err(BadSignature);
    }
    if claims.ring_id != ring.as_str() {
        return Err(RingMismatch);
    }
    if now >= claims.expires_at {
        return Err(Expired);
    }
    if hosted && claims.tier != Tier::Hosted {
        return Err(WrongTier);
    }
    Ok(claims)
}

/// Signs claims as the gateway does (`kid` is taken from the signer). For tests and vectors.
pub fn sign_entitlement(
    gateway: &dyn Signer,
    ring: &RingId,
    tier: Tier,
    purchase_ref: &str,
    issued_at: u64,
    expires_at: u64,
) -> Result<String, SignError> {
    let claims = EntitlementClaims {
        v: 1,
        kid: entitlement_kid(&gateway.sign_key()),
        ring_id: ring.as_str().to_string(),
        tier,
        purchase_ref: purchase_ref.to_string(),
        issued_at,
        expires_at,
    };
    let json = serde_json::to_vec(&claims).map_err(|e| SignError(e.to_string()))?;
    let part = b64::encode(&json);
    let sig = gateway.sign(&signed_bytes(&part))?;
    Ok(format!("{ENTITLEMENT_PREFIX}{part}.{}", b64::encode(&sig)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::DeviceKeys;

    #[test]
    fn verifies_like_the_gateway() {
        let gw = DeviceKeys::from_seeds(&[0x61; 32], &[0x62; 32]);
        let keys = GatewayKeys::new(&[gw.sign_key()]);
        let ring = RingId::derive(&DeviceKeys::from_seeds(&[1; 32], &[2; 32]).sign_key());
        let other = RingId::derive(&DeviceKeys::from_seeds(&[3; 32], &[4; 32]).sign_key());
        let t = sign_entitlement(&gw, &ring, Tier::Hosted, "p1", 100, 200).unwrap();
        assert_eq!(
            verify_entitlement(&t, &ring, 150, &keys, true)
                .unwrap()
                .tier,
            Tier::Hosted
        );
        assert_eq!(
            verify_entitlement(&t, &ring, 200, &keys, true),
            Err(EntitlementError::Expired)
        );
        assert_eq!(
            verify_entitlement(&t, &other, 150, &keys, true),
            Err(EntitlementError::RingMismatch)
        );
        let push = sign_entitlement(&gw, &ring, Tier::Push, "p1", 100, 200).unwrap();
        assert!(verify_entitlement(&push, &ring, 150, &keys, false).is_ok());
        assert_eq!(
            verify_entitlement(&push, &ring, 150, &keys, true),
            Err(EntitlementError::WrongTier)
        );
        let stranger = DeviceKeys::from_seeds(&[0x71; 32], &[0x72; 32]);
        let foreign = sign_entitlement(&stranger, &ring, Tier::Hosted, "p", 100, 200).unwrap();
        assert_eq!(
            verify_entitlement(&foreign, &ring, 150, &keys, true),
            Err(EntitlementError::UnknownKid)
        );
        let (head, _) = t.rsplit_once('.').unwrap();
        let forged = format!("{head}.{}", b64::encode(&[0u8; 64]));
        assert_eq!(
            verify_entitlement(&forged, &ring, 150, &keys, true),
            Err(EntitlementError::BadSignature)
        );
        for bad in [
            "",
            "xet1.",
            "xet1.a",
            "xet2.e30.AA",
            &format!("{t}="),
            &t.replace("xet1.", "xet1.."),
        ] {
            assert_eq!(
                verify_entitlement(bad, &ring, 150, &keys, true),
                Err(EntitlementError::Malformed),
                "{bad}"
            );
        }
    }
}

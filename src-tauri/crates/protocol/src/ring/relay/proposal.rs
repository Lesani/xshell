//! Frozen unsigned Enroll metadata; never a membership token or a trust anchor.
use crate::ring::{b64, RingId, SignKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalHandle {
    pub ring_id: RingId,
    pub relay_url: String,
    pub operation: String,
    pub head: String,
    #[serde(deserialize_with = "super::wire::safe::int")]
    pub version: u64,
    pub guest: SignKey,
    /// Base64url of the actual final XXpsk3 hash, not an invented local flow hash.
    pub context: String,
    #[serde(deserialize_with = "super::wire::safe::int")]
    pub expires_at: u64,
    pub grant: String,
    pub resource: String,
}
impl ProposalHandle {
    pub fn read_message(&self, origin: &str, nonce: &str) -> Vec<u8> {
        format!(
            "xshell-proposal-read-v2\n{origin}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{nonce}",
            self.ring_id,
            self.relay_url,
            self.grant,
            self.resource,
            self.operation,
            self.head,
            self.version,
            self.guest,
            self.context,
            self.expires_at
        )
        .into_bytes()
    }
    pub fn validate(&self) -> bool {
        self.version > 0
            && self.version < 4096
            && self.expires_at <= crate::ring::MAX_SAFE_INT
            && [
                &self.operation,
                &self.head,
                &self.context,
                &self.grant,
                &self.resource,
            ]
            .iter()
            .all(|v| b64::decode_array::<32>(v).is_ok())
            && crate::ring::url::RelayUrl::parse(&self.relay_url).is_ok()
    }
}

/// Exact width reservation only: no consent proof or signature is created/verified here.
/// Real Ed25519 signatures have a fixed86-character unpadded base64url representation.
pub fn completed_token_bytes(
    r: &crate::ring::Roster,
    expires_at: u64,
    final_hash: &[u8; 32],
) -> Result<usize, crate::ring::RosterError> {
    use crate::ring::membership::{pairing_consent_context, RosterChange};
    let m = r.membership()?;
    let RosterChange::Enroll {
        enrollment_id,
        proofs,
    } = &m.change
    else {
        return Err(crate::ring::membership::invalid("not Enroll"));
    };
    if !proofs.is_empty() {
        return Err(crate::ring::membership::invalid("already signed consent"));
    }
    let u = m
        .unit(enrollment_id)
        .ok_or_else(|| crate::ring::membership::invalid("missing unit"))?;
    let context = pairing_consent_context(r, final_hash)?;
    let mut value = serde_json::to_value(r).expect("Roster serializes");
    value["membership"]["change"]["proofs"]=serde_json::Value::Array(u.members.iter().map(|key|serde_json::json!({"key":key,"expiresAt":expires_at,"context":context,"signature":"A".repeat(86)})).collect());
    let bytes = serde_json::to_vec(&value).expect("reservation serializes");
    Ok(5 + b64::encode(&bytes).len() + 1 + 86)
}

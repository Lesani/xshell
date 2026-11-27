//! Relay activity attestation, scoped to one Ring, policy and immutable enrollment.
//! An ordinary offline snapshot is not terminal evidence. Only the pinned activity source
//! can attest that expiry was finalized before a later reconnect or policy change.
use super::{
    b64,
    json::strict_object,
    membership::{invalid, Enrollment, Membership, RingPolicy, RosterChange},
    verify, RingError, RingId, RosterError, SignKey, Signature, SignedRoster, Signer, MAX_SAFE_INT,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
pub const ACTIVITY_CONTEXT: &str = "xshell-ring-activity-v2\n";
pub const ACTIVITY_PREFIX: &str = "xae2.";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnitActivity {
    pub enrollment_id: String,
    pub member_keys: Vec<SignKey>,
    pub online: bool,
    pub last_active: u64,
    pub terminal_expired: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityEvidence {
    pub ring_id: RingId,
    pub relay_url: String,
    pub policy_head_hash: String,
    pub policy_hash: String,
    pub observed_at: u64,
    pub units: Vec<UnitActivity>,
}
pub fn policy_hash(policy: &RingPolicy) -> String {
    b64::encode(&Sha256::digest(
        format!(
            "xshell-ring-policy-v2\n{}\n{}",
            policy.activity_key,
            policy
                .inactivity_seconds
                .map(|s| s.to_string())
                .unwrap_or("never".into())
        )
        .as_bytes(),
    ))
}
impl ActivityEvidence {
    pub fn sign(&self, signer: &dyn Signer) -> Result<String, RingError> {
        let part =
            b64::encode(&serde_json::to_vec(self).map_err(|e| RingError::Invalid(e.to_string()))?);
        let signature = signer.sign(format!("{ACTIVITY_CONTEXT}{part}").as_bytes())?;
        Ok(format!(
            "{ACTIVITY_PREFIX}{part}.{}",
            b64::encode(&signature)
        ))
    }
    pub fn parse(token: &str, key: &SignKey) -> Result<Self, RosterError> {
        if token.len() > super::roster::MAX_ROSTER_TOKEN {
            return Err(RosterError::TooLarge);
        }
        let (part, sig) = token
            .strip_prefix(ACTIVITY_PREFIX)
            .and_then(|s| s.split_once('.'))
            .ok_or_else(|| invalid("activity token"))?;
        let bytes = b64::decode(part).map_err(|_| invalid("activity encoding"))?;
        let sig = Signature::parse(sig).map_err(|_| invalid("activity signature encoding"))?;
        let obj = strict_object(&bytes).map_err(RosterError::Malformed)?;
        let e: Self = serde_json::from_value(serde_json::Value::Object(obj))
            .map_err(|_| invalid("activity fields"))?;
        if e.observed_at > MAX_SAFE_INT
            || e.units.len() > 64
            || b64::decode_array::<32>(&e.policy_head_hash).is_err()
            || b64::decode_array::<32>(&e.policy_hash).is_err()
        {
            return Err(invalid("activity bounds"));
        }
        let mut ids = HashSet::new();
        for u in &e.units {
            if !ids.insert(&u.enrollment_id)
                || u.last_active > e.observed_at
                || u.member_keys.is_empty()
                || u.member_keys.len() > 2
                || u.member_keys.iter().collect::<HashSet<_>>().len() != u.member_keys.len()
            {
                return Err(invalid("activity unit"));
            }
        }
        if !verify(key, format!("{ACTIVITY_CONTEXT}{part}").as_bytes(), &sig) {
            return Err(RosterError::BadSignature);
        }
        Ok(e)
    }
    pub fn binds(&self, head: &SignedRoster, policy: &RingPolicy) -> bool {
        self.ring_id == *head.ring_id()
            && self.relay_url == head.roster().relay_url
            && self.policy_head_hash == head.hash()
            && self.policy_hash == policy_hash(policy)
            && self.observed_at >= head.roster().issued_at
    }
    pub fn eligible(&self, unit: &Enrollment, policy: &RingPolicy, terminal: bool) -> bool {
        let Some(seconds) = policy.inactivity_seconds else {
            return false;
        };
        self.units.iter().any(|u| {
            u.enrollment_id == unit.id
                && u.member_keys.len() == unit.members.len()
                && unit.members.iter().all(|k| u.member_keys.contains(k))
                && !u.online
                && (!terminal || u.terminal_expired)
                && u.last_active >= unit.enrolled_at
                && self.observed_at.saturating_sub(u.last_active) >= seconds
        })
    }
}
/// Verifies activity-dependent transitions against actual chain history, including an
/// ancestor-policy terminal report. No supplied token is allowed to invent an ancestor.
pub fn verify_transition(history: &[SignedRoster], next: &SignedRoster) -> Result<(), RosterError> {
    let prev = history.last().ok_or(RosterError::NotGenesis)?;
    if prev.roster().v != 2 {
        return Ok(());
    }
    let old = prev.roster().membership()?;
    let new = next.roster().membership()?;
    match &new.change {
        RosterChange::Expire {
            enrollment_ids,
            evidence,
        } => {
            let e = ActivityEvidence::parse(evidence, &old.policy.activity_key)?;
            let head = history
                .iter()
                .find(|r| r.hash() == e.policy_head_hash)
                .ok_or_else(|| invalid("activity policy head not in chain"))?;
            let reference = head.roster().membership()?;
            if !e.binds(head, &reference.policy) || e.observed_at > next.roster().issued_at {
                return Err(invalid("activity binding/time"));
            }
            for id in enrollment_ids {
                let u = old
                    .unit(id)
                    .ok_or_else(|| invalid("expiry target absent"))?;
                if reference.unit(id) != Some(u) || !e.eligible(u, &reference.policy, true) {
                    return Err(invalid("not terminal expiry evidence"));
                }
            }
        }
        RosterChange::SetPolicy { affected, preview } => {
            let e = ActivityEvidence::parse(preview, &old.policy.activity_key)?;
            if !e.binds(prev, &new.policy)
                || e.units.len() != old.enrollments.len()
                || e.observed_at > next.roster().issued_at
            {
                return Err(invalid("preview binding"));
            }
            for u in &old.enrollments {
                let a = e
                    .units
                    .iter()
                    .find(|a| a.enrollment_id == u.id)
                    .ok_or_else(|| invalid("incomplete preview"))?;
                if a.member_keys.len() != u.members.len()
                    || u.members.iter().any(|k| !a.member_keys.contains(k))
                    || a.last_active < u.enrolled_at
                {
                    return Err(invalid("preview unit"));
                }
            }
            let eligible = old
                .enrollments
                .iter()
                .filter(|u| e.eligible(u, &new.policy, false))
                .map(|u| u.id.clone())
                .collect::<Vec<_>>();
            if eligible.len() != affected.len() || eligible.iter().any(|id| !affected.contains(id))
            {
                return Err(invalid("preview affected set"));
            }
        }
        _ => {}
    }
    Ok(())
}
/// Consumers preserve terminal evidence they independently verify. Unknown Relay activity
/// cannot be inferred from a roster signature alone.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct TerminalEnrollments {
    pub ids: HashSet<String>,
}
impl TerminalEnrollments {
    pub fn observe(&mut self, history: &[SignedRoster], token: &str) -> Result<(), RosterError> {
        let head = history.last().ok_or(RosterError::NotGenesis)?;
        let current = head.roster().membership()?;
        let e = ActivityEvidence::parse(token, &current.policy.activity_key)?;
        let reference = history
            .iter()
            .find(|r| r.hash() == e.policy_head_hash)
            .ok_or_else(|| invalid("unknown policy head"))?;
        let old: Membership = reference.roster().membership()?;
        if !e.binds(reference, &old.policy) {
            return Err(invalid("evidence binding"));
        }
        for u in &old.enrollments {
            if e.eligible(u, &old.policy, true) {
                self.ids.insert(u.id.clone());
            }
        }
        Ok(())
    }
}

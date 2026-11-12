//! Computer/headless XXpsk3 payloads. Only public counterpart identities cross the pipe.
//! Possession is not consent: enrollment proofs are produced only after verified metadata
//! reaches the explicit callback in `relay::pair::adopt_as_guest`.
use super::*;
use crate::ring::membership::{ConsentProof, EnrollmentKind, RosterChange};
use crate::ring::{Member, Roster};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicIdentity {
    pub sign_key: SignKey,
    pub noise_key: NoiseKey,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum JoinTarget {
    StandaloneDaemon {
        daemon: PublicIdentity,
    },
    Computer {
        desktop: PublicIdentity,
        daemon: PublicIdentity,
    },
}
impl JoinTarget {
    pub fn principal(&self) -> &PublicIdentity {
        match self {
            Self::StandaloneDaemon { daemon } => daemon,
            Self::Computer { desktop, .. } => desktop,
        }
    }
    pub fn identities(&self) -> Vec<(&PublicIdentity, Role)> {
        match self {
            Self::StandaloneDaemon { daemon } => vec![(daemon, Role::Daemon)],
            Self::Computer { desktop, daemon } => {
                vec![(desktop, Role::Desktop), (daemon, Role::Daemon)]
            }
        }
    }
    pub fn validate(&self) -> Result<(), PairError> {
        let identities = self.identities();
        if identities
            .iter()
            .any(|(p, _)| p.name != member_name(&p.name, "computer"))
            || (identities.len() == 2
                && (identities[0].0.sign_key == identities[1].0.sign_key
                    || identities[0].0.noise_key == identities[1].0.noise_key))
        {
            return Err(PairError::Payload("invalid target identities".into()));
        }
        Ok(())
    }
    pub fn matches_proposal(&self, r: &Roster) -> Result<(), PairError> {
        self.validate()?;
        let m = r
            .membership()
            .map_err(|e| PairError::Payload(e.to_string()))?;
        let RosterChange::Enroll { enrollment_id, .. } = &m.change else {
            return Err(PairError::Payload("not Enroll".into()));
        };
        let u = m
            .unit(enrollment_id)
            .ok_or_else(|| PairError::Payload("missing target unit".into()))?;
        let kind = match self {
            Self::Computer { .. } => EnrollmentKind::Computer,
            _ => EnrollmentKind::Daemon,
        };
        let ids = self.identities();
        if u.kind != kind
            || u.members.len() != ids.len()
            || ids.iter().any(|(p, role)| {
                !u.members.contains(&p.sign_key)
                    || r.member(&p.sign_key)
                        != Some(&Member::new(
                            &p.name,
                            *role,
                            p.sign_key,
                            p.noise_key,
                            r.issued_at,
                        ))
            })
        {
            return Err(PairError::Payload(
                "proposal changed intended identities".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdoptionIntent {
    pub operation: String,
    pub expires_at: u64,
    pub target: JoinTarget,
}
impl AdoptionIntent {
    pub fn validate(&self) -> Result<(), PairError> {
        if b64::decode_array::<32>(&self.operation).is_err() || self.expires_at > MAX_SAFE_INT {
            return Err(PairError::Payload("operation bounds".into()));
        }
        self.target.validate()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Possession {
    key: SignKey,
    signature: Signature,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinPayload {
    v: u8,
    intent: AdoptionIntent,
    proofs: Vec<Possession>,
}
pub fn possession_message(intent: &AdoptionIntent, h2: &[u8]) -> Vec<u8> {
    let bytes = serde_json::to_vec(intent).expect("intent serializes");
    format!(
        "xshell-adoption-possession-v2\n{}\n{}",
        b64::encode(h2),
        b64::encode(&Sha256::digest(bytes))
    )
    .into_bytes()
}
/// Local signers remain owned by their actual identities. A future protected IPC adapter
/// can implement Signer; no private seed or invented GUI-consent bit is part of this API.
pub struct AdoptionSigners<'a> {
    pub principal: &'a dyn Signer,
    pub companion: Option<&'a dyn Signer>,
}
impl AdoptionSigners<'_> {
    pub fn sign(
        &self,
        target: &JoinTarget,
        message: &[u8],
    ) -> Result<Vec<(SignKey, Signature)>, PairError> {
        let ids = target.identities();
        let signers: Vec<&dyn Signer> = std::iter::once(self.principal)
            .chain(self.companion)
            .collect();
        if signers.len() != ids.len()
            || signers
                .iter()
                .zip(&ids)
                .any(|(s, (p, _))| s.sign_key() != p.sign_key)
        {
            return Err(PairError::Payload("counterpart possession required".into()));
        }
        signers
            .iter()
            .map(|s| {
                s.sign(message)
                    .map(|sig| (s.sign_key(), Signature::from_bytes(sig)))
                    .map_err(|e| PairError::Relay(e.to_string()))
            })
            .collect()
    }
}
impl Guest {
    pub fn join_adoption(
        &mut self,
        keys: &DeviceKeys,
        intent: &AdoptionIntent,
        signers: &AdoptionSigners<'_>,
    ) -> Result<Vec<u8>, PairError> {
        intent.validate()?;
        if keys.sign_key() != intent.target.principal().sign_key
            || keys.noise_key() != intent.target.principal().noise_key
        {
            return Err(PairError::Pin);
        }
        let h2 = self.h2.take().ok_or(PairError::Cancelled)?;
        let proofs = signers
            .sign(&intent.target, &possession_message(intent, &h2))?
            .into_iter()
            .map(|(key, signature)| Possession { key, signature })
            .collect();
        let payload = serde_json::to_vec(&JoinPayload {
            v: 2,
            intent: intent.clone(),
            proofs,
        })
        .expect("join serializes");
        let mut hs = self.hs.take().ok_or(PairError::Cancelled)?;
        let message = write(&mut hs, &payload)?;
        self.final_hash = Some(
            hs.get_handshake_hash()
                .try_into()
                .map_err(|_| PairError::Crypto)?,
        );
        self.transport = Some(
            hs.into_stateless_transport_mode()
                .map_err(|_| PairError::Crypto)?,
        );
        Ok(message)
    }
}
impl Host {
    pub fn read_adoption(&mut self, msg3: &[u8]) -> Result<AdoptionIntent, PairError> {
        if self.step != 2 {
            return Err(PairError::Cancelled);
        }
        let mut hs = self.hs.take().ok_or(PairError::Cancelled)?;
        let h2 = self.h2.take().ok_or(PairError::Cancelled)?;
        let payload = read(&mut hs, msg3)?;
        strict_object(&payload).map_err(PairError::Payload)?;
        let p: JoinPayload = serde_json::from_slice(&payload)
            .map_err(|_| PairError::Payload("adoption payload".into()))?;
        p.intent.validate()?;
        let ids = p.intent.target.identities();
        if p.v != 2
            || p.proofs.len() != ids.len()
            || hs.get_remote_static()
                != Some(p.intent.target.principal().noise_key.as_bytes().as_slice())
            || p.proofs.iter().zip(ids).any(|(proof, (id, _))| {
                proof.key != id.sign_key
                    || !verify(
                        &proof.key,
                        &possession_message(&p.intent, &h2),
                        &proof.signature,
                    )
            })
        {
            return Err(PairError::Crypto);
        }
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
        Ok(p.intent)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdoptionConsent {
    pub operation: String,
    pub proposal_hash: String,
    pub proofs: Vec<ConsentProof>,
}
/// Check received real signatures, not placeholder proofs, before an approver signs a token.
pub fn apply_consent(
    r: &Roster,
    intent: &AdoptionIntent,
    hash: &[u8; 32],
    consent: AdoptionConsent,
) -> Result<Roster, PairError> {
    use crate::ring::membership::{
        consent_message, consent_proposal_hash, pairing_consent_context,
    };
    intent.target.matches_proposal(r)?;
    let context =
        pairing_consent_context(r, hash).map_err(|e| PairError::Payload(e.to_string()))?;
    let proposal_hash = consent_proposal_hash(r).map_err(|e| PairError::Payload(e.to_string()))?;
    if consent.operation != intent.operation
        || consent.proposal_hash != proposal_hash
        || consent.proofs.len() != intent.target.identities().len()
        || intent.target.identities().iter().any(|(id, _)| {
            consent
                .proofs
                .iter()
                .filter(|p| p.key == id.sign_key)
                .count()
                != 1
        })
        || consent.proofs.iter().any(|p| {
            p.context != context
                || p.expires_at != intent.expires_at
                || !verify(
                    &p.key,
                    &consent_message(r, p.expires_at, &context).unwrap_or_default(),
                    &p.signature,
                )
        })
    {
        return Err(PairError::Crypto);
    }
    let mut r = r.clone();
    let mut m = r
        .membership()
        .map_err(|e| PairError::Payload(e.to_string()))?;
    let RosterChange::Enroll { proofs, .. } = &mut m.change else {
        return Err(PairError::Crypto);
    };
    *proofs = consent.proofs;
    r.set_membership(&m);
    Ok(r)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AdoptionDecision {
    Consent {
        consent: AdoptionConsent,
    },
    Cancel {
        operation: String,
        proposal_hash: String,
    },
}

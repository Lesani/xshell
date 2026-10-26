//! Scoped Mobile enrollment through protected Desktop IPC and pinned task TLS.
//! Readiness consent cannot authorize them. Operation attribution is volatile; no field
//! is added to ring.json. After restart only exact verified identity reconciliation is safe.
use super::{printable, AccessError, MobileRole, LABEL_MAX, TASK_MAX_SECS};
use crate::ring::{url::RelayUrl, NoiseKey, RingId, SignKey, MAX_SAFE_INT};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const OFFER_MAX: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PairOperationId(Uuid);
impl PairOperationId {
    pub fn new(id: Uuid) -> Result<Self, AccessError> {
        if id.is_nil() {
            return Err(AccessError::InvalidRequest);
        }
        Ok(Self(id))
    }
    pub fn uuid(self) -> Uuid {
        self.0
    }
    pub fn validate(self) -> Result<(), AccessError> {
        Self::new(self.0).map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipientIdentity {
    pub signing: SignKey,
    pub noise: NoiseKey,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainHead {
    pub version: u64,
    pub hash: String,
}
impl ChainHead {
    pub fn validate(&self) -> Result<(), AccessError> {
        if self.version == 0
            || self.version > MAX_SAFE_INT
            || self.hash.len() != 43
            || crate::ring::b64::decode(&self.hash).map_or(true, |b| b.len() != 32)
        {
            return Err(AccessError::InvalidRequest);
        }
        Ok(())
    }
}
/// Public Desktop selection, before the phone supplies its existing device identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentTarget {
    pub approver_noise: NoiseKey,
    pub relay: String,
    pub device_name: String,
    pub initial_head: ChainHead,
}
impl EnrollmentTarget {
    pub fn parameters(
        &self,
        scope: &super::Scope,
        recipient: RecipientIdentity,
        expires_at: u64,
    ) -> EnrollmentParameters {
        EnrollmentParameters {
            ring: scope.ring.clone(),
            approver: RecipientIdentity {
                signing: scope.approver,
                noise: self.approver_noise,
            },
            recipient,
            relay: self.relay.clone(),
            role: scope.role,
            device_name: self.device_name.clone(),
            initial_head: self.initial_head.clone(),
            expires_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentParameters {
    pub ring: RingId,
    pub approver: RecipientIdentity,
    pub recipient: RecipientIdentity,
    pub relay: String,
    pub role: MobileRole,
    pub device_name: String,
    pub initial_head: ChainHead,
    pub expires_at: u64,
}
impl EnrollmentParameters {
    pub fn validate(&self, now: u64) -> Result<(), AccessError> {
        let relay = RelayUrl::parse(&self.relay).map_err(|_| AccessError::InvalidRequest)?;
        if format!("{}{}", relay.origin(), relay.path) != self.relay
            || !printable(&self.device_name, LABEL_MAX)
        {
            return Err(AccessError::InvalidRequest);
        }
        self.initial_head.validate()?;
        if self.expires_at <= now || self.expires_at.saturating_sub(now) > TASK_MAX_SECS {
            return Err(AccessError::Expired);
        }
        Ok(())
    }
}

/// No Clone, Display, public inner field or public helper representation. Owned decoded
/// and encoded buffers are wiped; serde/TLS/OS allocator copies are not an erasure guarantee.
/// ```compile_fail
/// use xshell_protocol::pairing_access::enrollment::SecretOffer;
/// fn duplicate(offer: SecretOffer) { let _copy = offer.clone(); }
/// ```
pub struct SecretOffer(Zeroizing<String>);
impl SecretOffer {
    pub fn new(value: String) -> Result<Self, AccessError> {
        let value = Zeroizing::new(value);
        if value.len() > OFFER_MAX {
            return Err(AccessError::TooLarge);
        }
        // xsp1 is canonical unpadded base64url; disallow JSON escaping and whitespace.
        let body = value
            .strip_prefix("xsp1.")
            .ok_or(AccessError::InvalidRequest)?;
        if body.is_empty()
            || !body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(AccessError::InvalidRequest);
        }
        Ok(Self(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl Serialize for SecretOffer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl std::fmt::Debug for SecretOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretOffer([REDACTED])")
    }
}
impl<'de> Deserialize<'de> for SecretOffer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // String's deserializer moves its owned buffer here, including escaped JSON input.
        Self::new(String::deserialize(d)?)
            .map_err(|_| serde::de::Error::custom("invalid private offer"))
    }
}
/// Protected IPC / pinned TLS only; deliberately not a variant of public HelperInput or
/// the readiness Request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentStart {
    pub operation: PairOperationId,
    pub parameters: EnrollmentParameters,
    pub offer: SecretOffer,
}
impl EnrollmentStart {
    /// Returns the effective absolute deadline; never renews the task after acquisition.
    pub fn validate(&self, now: u64) -> Result<u64, AccessError> {
        self.operation.validate()?;
        self.parameters.validate(now)?;
        let offer = crate::ring::pairing::PairingOffer::parse(self.offer.expose())
            .map_err(|_| AccessError::InvalidRequest)?;
        let params = &self.parameters;
        if offer.ring_id != params.ring
            || offer.sign_key != params.approver.signing
            || offer.noise_key != params.approver.noise
            || RelayUrl::parse(&offer.relay_url).map_err(|_| AccessError::InvalidRequest)?
                != RelayUrl::parse(&params.relay).map_err(|_| AccessError::InvalidRequest)?
        {
            return Err(AccessError::ScopeMismatch);
        }
        let deadline = params.expires_at.min(offer.expires_at);
        if deadline <= now {
            return Err(AccessError::Expired);
        }
        Ok(deadline)
    }
}

/// Exact, explicitly authorized reconciliation of existing verified membership. This is
/// not operation attribution and cannot reconstruct a successful task receipt after restart.
pub fn verify_enrollment_chain(
    parameters: &EnrollmentParameters,
    chain: &crate::ring::RosterChain,
) -> Result<(), AccessError> {
    let head = chain.head();
    let relay =
        RelayUrl::parse(&head.roster().relay_url).map_err(|_| AccessError::ScopeMismatch)?;
    if head.ring_id() != &parameters.ring
        || relay != RelayUrl::parse(&parameters.relay).map_err(|_| AccessError::ScopeMismatch)?
        || !chain
            .get(parameters.initial_head.version)
            .is_some_and(|r| r.hash() == parameters.initial_head.hash)
        || !head.member(&parameters.approver.signing).is_some_and(|m| {
            m.role == crate::ring::Role::Desktop && m.noise_key == parameters.approver.noise
        })
        || !head.member(&parameters.recipient.signing).is_some_and(|m| {
            m.role == crate::ring::Role::Mobile && m.noise_key == parameters.recipient.noise
        })
    {
        return Err(AccessError::ScopeMismatch);
    }
    Ok(())
}

/// Each stage is an observation by its named owner, not a connectivity inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnrollmentStage {
    HelperReserved,
    DesktopOfferReady,
    MobileAccepted,
    DesktopCommitted,
    PublicationAcknowledged,
    MobilePersisted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentProgress {
    pub operation: PairOperationId,
    pub stage: EnrollmentStage,
}
/// Evidence is cumulative, never independent booleans. Unknown is not proof of no commit.
/// Receipts are volatile. Do not serialize them into the shared Ring storage format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "evidence", deny_unknown_fields, rename_all = "kebab-case")]
pub enum EnrollmentEvidence {
    OutcomeUnknown,
    /// Only Desktop transaction/admission evidence can prove this, never Mobile cancel.
    NoDesktopCommit,
    DesktopCommitted {
        head: ChainHead,
    },
    PublicationAcknowledged {
        head: ChainHead,
    },
    MobilePersisted {
        desktop_head: ChainHead,
        mobile_head: ChainHead,
        recipient: RecipientIdentity,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentReceipt {
    pub operation: PairOperationId,
    pub evidence: EnrollmentEvidence,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing_access::{decode, read_frame, write_frame, FRAME_MAX};
    #[test]
    fn private_offer_real_frames_redact_and_bound() {
        // Synthetic only. Never print this value, including in failed assertion diffs.
        let synthetic = "xsp1.SyntheticPrivateCanary0123456789";
        let offer = SecretOffer::new(synthetic.into()).unwrap();
        assert!(!format!("{offer:?}").contains(synthetic));
        let mut frame = Zeroizing::new(Vec::new());
        write_frame(&mut *frame, &offer).unwrap();
        let back: SecretOffer = read_frame(&mut frame.as_slice()).unwrap();
        assert!(back.expose() == synthetic);
        let malformed = Zeroizing::new(format!("\"{synthetic}!\""));
        let error = decode::<SecretOffer>(malformed.as_bytes(), FRAME_MAX).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(synthetic));
        assert!(SecretOffer::new("a".repeat(OFFER_MAX + 1)).is_err());
        // Public parser never accepts an offer, including nested or unknown fields.
        assert!(decode::<super::super::Command>(
            b"{\"method\":\"start\",\"offer\":\"xsp1.test\"}",
            FRAME_MAX
        )
        .is_err());
    }
    #[test]
    fn maximum_enrollment_frame_fits_unchanged_budget() {
        use crate::ring::DeviceKeys;
        let keys = DeviceKeys::from_seeds(&[1; 32], &[2; 32]);
        let identity = RecipientIdentity {
            signing: keys.sign_key(),
            noise: keys.noise_key(),
        };
        let relay = format!("wss://example.invalid/{}", "a".repeat(490));
        let start = EnrollmentStart {
            operation: PairOperationId::new(Uuid::from_u128(1)).unwrap(),
            parameters: EnrollmentParameters {
                ring: RingId::derive(&keys.sign_key()),
                approver: identity.clone(),
                recipient: identity,
                relay,
                role: MobileRole::Mobile,
                device_name: "\"".repeat(LABEL_MAX),
                initial_head: ChainHead {
                    version: MAX_SAFE_INT,
                    hash: crate::ring::b64::encode(&[3; 32]),
                },
                expires_at: u64::MAX,
            },
            offer: SecretOffer::new(format!("xsp1.{}", "a".repeat(OFFER_MAX - 5))).unwrap(),
        };
        start.parameters.validate(u64::MAX - 600).unwrap();
        let mut frame = Zeroizing::new(Vec::new());
        write_frame(&mut *frame, &start).unwrap();
        assert!(frame.len() <= FRAME_MAX + 4);
        println!(
            "bounded enrollment frame body: {} bytes; cap: {}",
            frame.len() - 4,
            FRAME_MAX
        );
        let back: EnrollmentStart = read_frame(&mut frame.as_slice()).unwrap();
        assert!(back.offer.expose().len() == OFFER_MAX);
        assert!(back.parameters == start.parameters);
        let request = EnrollmentRequest {
            task: Uuid::from_u128(2),
            request: Uuid::from_u128(3),
            sequence: MAX_SAFE_INT,
            command: EnrollmentCommand::Enroll {
                start: Box::new(start),
            },
        };
        let mut envelope = Zeroizing::new(Vec::new());
        write_frame(&mut *envelope, &request).unwrap();
        assert!(envelope.len() <= FRAME_MAX + 4);
    }
    #[test]
    fn enrollment_public_descriptors_and_exact_verification_fit_existing_bounds() {
        use super::super::*;
        use crate::ring::DeviceKeys;
        let keys = DeviceKeys::from_seeds(&[11; 32], &[12; 32]);
        let scope = Scope {
            ring: RingId::derive(&keys.sign_key()),
            approver: keys.sign_key(),
            role: MobileRole::Mobile,
            purpose: ReachabilityPurpose::MobileEnrollment,
            enrollment: Some(EnrollmentTarget {
                approver_noise: keys.noise_key(),
                relay: format!("wss://relay.invalid/{}", "a".repeat(490)),
                device_name: "\"".repeat(LABEL_MAX),
                initial_head: ChainHead {
                    version: MAX_SAFE_INT,
                    hash: crate::ring::b64::encode(&[13; 32]),
                },
            }),
        };
        let proposal = ControllerDescriptor {
            version: VERSION,
            task: Uuid::from_u128(1),
            label: "\"".repeat(LABEL_MAX),
            certificate: Certificate(vec![14; CERT_MAX]),
            scope: scope.clone(),
            expires_at: MAX_SAFE_INT,
        };
        proposal.validate(MAX_SAFE_INT - 600).unwrap();
        let body = serde_json::to_vec(&proposal).unwrap();
        assert!(body.len() <= DESCRIPTOR_MAX);
        let link = format!(
            "xshell-remote://pairing-access?descriptor={}",
            crate::ring::b64::encode(&body)
        );
        assert!(link.len() <= LINK_MAX);
        let server = ServerDescriptor {
            version: VERSION,
            task: proposal.task,
            certificate: Certificate(vec![15; CERT_MAX]),
            controller_fingerprint: [255; 32],
            scope,
            endpoint: "[2001:db8::1]:65535".parse().unwrap(),
            expires_at: proposal.expires_at,
            recipient: Some(RecipientIdentity {
                signing: keys.sign_key(),
                noise: keys.noise_key(),
            }),
        };
        server
            .validate_for(&proposal, [255; 32], MAX_SAFE_INT - 600)
            .unwrap();
        let server_bytes = serde_json::to_vec(&server).unwrap();
        assert!(server_bytes.len() <= DESCRIPTOR_MAX);
        let decision = VerificationDecision {
            controller_fingerprint: [255; 32],
            server_fingerprint: [255; 32],
            endpoint: server.endpoint,
            descriptor: Some(server),
        };
        let mut frame = Vec::new();
        write_frame(&mut frame, &decision).unwrap();
        println!(
            "bounded enrollment proposal/server/link/verification: {}/{}/{}/{} bytes",
            body.len(),
            server_bytes.len(),
            link.len(),
            frame.len() - 4
        );
    }

    #[test]
    fn private_start_scope_deadline_and_exact_verified_reconciliation() {
        use crate::ring::pairing::{PairSecret, PairingOffer};
        use crate::ring::{DeviceKeys, Member, Role, RosterChain, SignedRoster};
        let desktop = DeviceKeys::from_seeds(&[1; 32], &[2; 32]);
        let mobile = DeviceKeys::from_seeds(&[3; 32], &[4; 32]);
        let genesis = SignedRoster::genesis(
            &desktop,
            desktop.noise_key(),
            "desktop",
            "wss://relay.invalid",
            1,
        )
        .unwrap();
        let successor = genesis
            .next(&desktop, 2, |roster| {
                roster.add(Member::new(
                    "phone",
                    Role::Mobile,
                    mobile.sign_key(),
                    mobile.noise_key(),
                    2,
                ))
            })
            .unwrap();
        let chain = RosterChain::from_chain(vec![genesis.clone(), successor]).unwrap();
        let parameters = EnrollmentParameters {
            ring: genesis.ring_id().clone(),
            approver: RecipientIdentity {
                signing: desktop.sign_key(),
                noise: desktop.noise_key(),
            },
            recipient: RecipientIdentity {
                signing: mobile.sign_key(),
                noise: mobile.noise_key(),
            },
            relay: "wss://relay.invalid".into(),
            role: MobileRole::Mobile,
            device_name: "phone".into(),
            initial_head: ChainHead {
                version: 1,
                hash: genesis.hash(),
            },
            expires_at: 600,
        };
        let offer = PairingOffer {
            ring_id: parameters.ring.clone(),
            relay_url: parameters.relay.clone(),
            sign_key: desktop.sign_key(),
            noise_key: desktop.noise_key(),
            secret: PairSecret::from_bytes(&[9; 32]),
            expires_at: 500,
        };
        let start = EnrollmentStart {
            operation: PairOperationId::new(Uuid::from_u128(1)).unwrap(),
            parameters: parameters.clone(),
            offer: SecretOffer::new(offer.encode()).unwrap(),
        };
        assert_eq!(start.validate(1), Ok(500));
        assert_eq!(start.validate(500), Err(AccessError::Expired));
        let mut frame = Zeroizing::new(Vec::new());
        write_frame(&mut *frame, &start).unwrap();
        let decoded: EnrollmentStart = read_frame(&mut frame.as_slice()).unwrap();
        assert_eq!(decoded.validate(1), Ok(500));
        assert!(!format!("{decoded:?}").contains(decoded.offer.expose()));
        assert_eq!(verify_enrollment_chain(&parameters, &chain), Ok(()));
        for changed in 0..5 {
            let mut wrong = parameters.clone();
            match changed {
                0 => wrong.relay = "wss://another.invalid".into(),
                1 => wrong.approver.signing = mobile.sign_key(),
                2 => wrong.approver.noise = mobile.noise_key(),
                3 => wrong.recipient.noise = desktop.noise_key(),
                4 => wrong.initial_head.hash = crate::ring::b64::encode(&[99; 32]),
                _ => unreachable!(),
            }
            assert_eq!(
                verify_enrollment_chain(&wrong, &chain),
                Err(AccessError::ScopeMismatch)
            );
        }
        let unknown = EnrollmentReceipt {
            operation: start.operation,
            evidence: EnrollmentEvidence::OutcomeUnknown,
        };
        let raw = serde_json::to_string(&unknown).unwrap();
        assert!(!raw.contains("head"));
        assert!(serde_json::from_str::<EnrollmentReceipt>(
            &raw.replace("outcome-unknown", "connected")
        )
        .is_err());
    }
}

/// Protected TLS start is nonrepeatable; polling and cancellation contain no credential.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", deny_unknown_fields, rename_all = "kebab-case")]
pub enum EnrollmentCommand {
    Enroll { start: Box<EnrollmentStart> },
    EnrollmentStatus { operation: PairOperationId },
    EnrollmentCancel { operation: PairOperationId },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentRequest {
    pub task: Uuid,
    pub request: Uuid,
    pub sequence: u64,
    pub command: EnrollmentCommand,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentResponse {
    pub progress: Option<EnrollmentProgress>,
    pub receipt: Option<EnrollmentReceipt>,
    pub error: Option<AccessError>,
}
impl EnrollmentResponse {
    pub fn error(error: AccessError) -> Self {
        Self {
            progress: None,
            receipt: None,
            error: Some(error),
        }
    }
}
/// Private same-user Desktop transport. These methods never enter the public helper parser.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", deny_unknown_fields, rename_all = "kebab-case")]
pub enum DesktopRequest {
    Scope {
        ring: RingId,
        device_name: String,
    },
    Offer {
        task: Uuid,
        operation: PairOperationId,
        parameters: Box<EnrollmentParameters>,
    },
    Status {
        task: Uuid,
        operation: PairOperationId,
    },
    Cancel {
        task: Uuid,
        operation: PairOperationId,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", deny_unknown_fields, rename_all = "kebab-case")]
pub enum DesktopResponse {
    Scope { scope: super::Scope },
    Offer { start: EnrollmentStart },
    Operation { response: EnrollmentResponse },
    Error { error: AccessError },
}

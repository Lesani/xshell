//! Bounded public Pairing-access contract. This is not an enrollment or cryptographic wire
//! protocol. UI invocations do not require external-task authorization.
use crate::ring::{RingId, SignKey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::SocketAddr;
use uuid::Uuid;

pub const VERSION: u8 = 1;
pub const FRAME_MAX: usize = 4096;
pub const DESCRIPTOR_MAX: usize = 3072;
pub const CERT_MAX: usize = 1024;
pub const LINK_MAX: usize = 4096;
pub const LABEL_MAX: usize = 64;
pub const SENTINEL_MAX: usize = 32;
pub const TASK_MAX_SECS: u64 = 600;
pub const CACHE_MAX: usize = 64;
pub const SERVER_NAME: &str = "pairing-task.invalid";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessError {
    Unauthorized,
    ScopeMismatch,
    RequestConflict,
    Replay,
    Expired,
    NotForeground,
    Locked,
    GenerationChanged,
    Busy,
    RateLimited,
    InvalidRequest,
    TooLarge,
    CoordinatorUnavailable,
    PermissionRequired,
    AddressUnavailable,
    BootstrapUnverified,
    TransportClosedOutcomeUnknown,
}
impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only fixed enum spellings; never reflect peer input or TLS diagnostics.
        f.write_str(serde_json::to_value(self).unwrap().as_str().unwrap())
    }
}
impl std::error::Error for AccessError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub ring: RingId,
    pub approver: SignKey,
    pub role: MobileRole,
    pub purpose: ReachabilityPurpose,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MobileRole {
    Mobile,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReachabilityPurpose {
    Reachability,
}

/// DER is public and encoded compactly; private material has no contract representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate(pub Vec<u8>);
impl Serialize for Certificate {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::ring::b64::encode(&self.0))
    }
}
impl<'de> Deserialize<'de> for Certificate {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() > CERT_MAX.div_ceil(3) * 4 {
            return Err(serde::de::Error::custom("certificate too large"));
        }
        let bytes = crate::ring::b64::decode(&s).map_err(serde::de::Error::custom)?;
        if bytes.is_empty() || bytes.len() > CERT_MAX {
            return Err(serde::de::Error::custom("invalid certificate size"));
        }
        Ok(Self(bytes))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerDescriptor {
    pub version: u8,
    pub task: Uuid,
    pub label: String,
    pub certificate: Certificate,
    pub scope: Scope,
    pub expires_at: u64,
}
impl ControllerDescriptor {
    pub fn validate(&self, now: u64) -> Result<(), AccessError> {
        if self.version != VERSION
            || self.task.is_nil()
            || !printable(&self.label, LABEL_MAX)
            || self.certificate.0.is_empty()
            || self.certificate.0.len() > CERT_MAX
        {
            return Err(AccessError::InvalidRequest);
        }
        if self.expires_at <= now || self.expires_at.saturating_sub(now) > TASK_MAX_SECS {
            return Err(AccessError::Expired);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerDescriptor {
    pub version: u8,
    pub task: Uuid,
    pub certificate: Certificate,
    pub controller_fingerprint: [u8; 32],
    pub scope: Scope,
    pub endpoint: SocketAddr,
    pub expires_at: u64,
}
impl ServerDescriptor {
    pub fn validate_for(
        &self,
        proposal: &ControllerDescriptor,
        controller_fingerprint: [u8; 32],
        now: u64,
    ) -> Result<(), AccessError> {
        proposal.validate(now)?;
        if self.version != VERSION
            || self.task != proposal.task
            || self.scope != proposal.scope
            || self.expires_at != proposal.expires_at
            || self.controller_fingerprint != controller_fingerprint
        {
            return Err(AccessError::ScopeMismatch);
        }
        validate_endpoint(self.endpoint)?;
        if self.certificate.0.is_empty() || self.certificate.0.len() > CERT_MAX {
            return Err(AccessError::InvalidRequest);
        }
        Ok(())
    }
}
/// An explicit bootstrap verification decision binding both public identities and endpoint.
/// Importing a descriptor does not construct or imply this decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationDecision {
    pub controller_fingerprint: [u8; 32],
    pub server_fingerprint: [u8; 32],
    pub endpoint: SocketAddr,
}

pub fn validate_endpoint(endpoint: SocketAddr) -> Result<(), AccessError> {
    let ip = endpoint.ip();
    if ip.is_unspecified()
        || ip.is_multicast()
        || endpoint.port() == 0
        || matches!(ip, std::net::IpAddr::V4(ip) if ip.is_broadcast())
    {
        return Err(AccessError::AddressUnavailable);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", deny_unknown_fields, rename_all = "kebab-case")]
pub enum InvocationContext {
    Ui {},
    External { task: Uuid },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", deny_unknown_fields, rename_all = "kebab-case")]
pub enum Command {
    Prepare { sentinel: String },
    Status {},
    Cancel {},
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub task: Uuid,
    pub request: Uuid,
    pub sequence: u64,
    pub scope: Scope,
    pub command: Command,
}
impl Request {
    pub fn validate(&self) -> Result<(), AccessError> {
        if self.task.is_nil()
            || self.request.is_nil()
            || self.sequence == 0
            || self.sequence > crate::ring::MAX_SAFE_INT
        {
            return Err(AccessError::InvalidRequest);
        }
        if let Command::Prepare { sentinel } = &self.command {
            if !printable(sentinel, SENTINEL_MAX) {
                return Err(AccessError::InvalidRequest);
            }
        }
        Ok(())
    }
}
pub fn printable(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskPhase {
    Authorized,
    Prepared,
    Cancelled,
    Closed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", deny_unknown_fields, rename_all = "kebab-case")]
pub enum Outcome {
    Prepared {},
    Status {},
    Cancelled {},
    Error { error: AccessError },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub phase: TaskPhase,
    pub accepted_sequence: u64,
    pub expires_at: u64,
    pub outcome: Outcome,
}

/// Bounds bytes before JSON/certificate processing. Serde rejects duplicate/unknown fields
/// and trailing bytes. Unavoidable OS/framework URL allocations are outside this boundary.
pub fn decode<T: DeserializeOwned>(bytes: &[u8], max: usize) -> Result<T, AccessError> {
    if bytes.len() > max {
        return Err(AccessError::TooLarge);
    }
    serde_json::from_slice(bytes).map_err(|_| AccessError::InvalidRequest)
}
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, AccessError> {
    let mut header = [0; 4];
    reader
        .read_exact(&mut header)
        .map_err(|_| AccessError::TransportClosedOutcomeUnknown)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > FRAME_MAX {
        return Err(AccessError::TooLarge);
    }
    let mut body = vec![0; len];
    reader
        .read_exact(&mut body)
        .map_err(|_| AccessError::TransportClosedOutcomeUnknown)?;
    decode(&body, FRAME_MAX)
}
pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), AccessError> {
    let body = serde_json::to_vec(value).map_err(|_| AccessError::InvalidRequest)?;
    if body.len() > FRAME_MAX {
        return Err(AccessError::TooLarge);
    }
    writer
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|_| writer.write_all(&body))
        .and_then(|_| writer.flush())
        .map_err(|_| AccessError::TransportClosedOutcomeUnknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairing_access_parser_limits() {
        assert_eq!(
            decode::<Command>(&vec![0; FRAME_MAX + 1], FRAME_MAX),
            Err(AccessError::TooLarge)
        );
        for raw in [
            r#"{"method":"status","extra":1}"#,
            r#"{"method":"prepare","sentinel":"a","sentinel":"b"}"#,
            r#"{"method":"start"}"#,
            r#"{"method":"status"} {}"#,
        ] {
            assert!(decode::<Command>(raw.as_bytes(), FRAME_MAX).is_err());
        }
        assert_eq!(
            read_frame::<Command>(&mut &(u32::MAX.to_be_bytes())[..]),
            Err(AccessError::TooLarge)
        );
        assert!(decode::<Certificate>(
            format!("\"{}\"", "a".repeat(1400)).as_bytes(),
            DESCRIPTOR_MAX
        )
        .is_err());
        assert!(decode::<Certificate>(b"\"%bad\"", DESCRIPTOR_MAX).is_err());
        assert!(!printable("\n", SENTINEL_MAX));
        assert!(!printable(&"x".repeat(33), SENTINEL_MAX));
    }
    #[test]
    fn pairing_access_invocation_context_is_explicit() {
        assert_eq!(
            decode::<InvocationContext>(br#"{"source":"ui"}"#, FRAME_MAX).unwrap(),
            InvocationContext::Ui {}
        );
    }
    #[test]
    fn pairing_access_descriptor_and_request_validation() {
        let keys = crate::ring::DeviceKeys::generate().unwrap();
        let scope = Scope {
            ring: RingId::derive(&keys.sign_key()),
            approver: keys.sign_key(),
            role: MobileRole::Mobile,
            purpose: ReachabilityPurpose::Reachability,
        };
        let mut descriptor = ControllerDescriptor {
            version: VERSION,
            task: Uuid::new_v4(),
            label: "controller".into(),
            certificate: Certificate(vec![1]),
            scope: scope.clone(),
            expires_at: 1600,
        };
        assert_eq!(descriptor.validate(1000), Ok(()));
        assert_eq!(descriptor.validate(1600), Err(AccessError::Expired));
        assert_eq!(descriptor.validate(999), Err(AccessError::Expired));
        descriptor.label = "x".repeat(LABEL_MAX + 1);
        assert_eq!(descriptor.validate(1000), Err(AccessError::InvalidRequest));
        descriptor.label = "controller".into();
        let body = serde_json::to_string(&descriptor).unwrap();
        let duplicate = body.replacen("{", "{\"version\":1,", 1);
        assert!(decode::<ControllerDescriptor>(duplicate.as_bytes(), DESCRIPTOR_MAX).is_err());
        assert_eq!(
            decode::<ControllerDescriptor>(&vec![b' '; DESCRIPTOR_MAX + 1], DESCRIPTOR_MAX),
            Err(AccessError::TooLarge)
        );
        let mut request = Request {
            task: descriptor.task,
            request: Uuid::new_v4(),
            sequence: 1,
            scope,
            command: Command::Status {},
        };
        assert_eq!(request.validate(), Ok(()));
        request.sequence = 0;
        assert_eq!(request.validate(), Err(AccessError::InvalidRequest));
        request.sequence = u64::MAX;
        assert_eq!(request.validate(), Err(AccessError::InvalidRequest));
        request.sequence = 1;
        request.request = Uuid::nil();
        assert_eq!(request.validate(), Err(AccessError::InvalidRequest));
        assert!(
            decode::<InvocationContext>(br#"{"source":"ui","task":"unexpected"}"#, FRAME_MAX)
                .is_err()
        );
    }
}

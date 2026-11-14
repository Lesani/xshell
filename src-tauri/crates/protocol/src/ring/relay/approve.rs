//! Approve a Device Code from a Connector owner without its own journal (the Mobile). The
//! joining machine shows the code; the user types the eight words here. The exchange is
//! the same as a Desktop approval: the joining keys prove possession, sign consent to the
//! exact proposal bound to the Noise transcript, and only then is the Enroll signed and
//! published by this approver.
//!
//! Resumable in two calls: [`prepare_approval`] runs the exchange until the joining
//! machine's possession-proven intent is read and signs the standalone removal it needs,
//! if any, publishing nothing; it returns a [`PreparedApproval`] whose [`ApprovalOp`] (no
//! secret) the caller stores. [`publish_approval`] then publishes: the standalone removal
//! (if any), the proposal, the joining keys' consent, the signed Enroll, and the answer to
//! the joining machine. After a crash or a lost answer, [`resume_approval`] reads the
//! Relay's chain and says which stage was reached. [`approve_device_code`] is prepare +
//! publish.
//!
//! A machine whose headless Daemon is already a standalone member joins as a Computer in
//! the same approval: its standalone unit is removed first (only after possession of the
//! Daemon key was proven in this pairing), then the Computer is enrolled.
//!
//! [`check_join`] is the joining phone's read-only counterpart: after a crash between
//! releasing its proofs and storing the chain, it asks the Relay whether it was enrolled.
use super::connector::{Connector, LinkState, ProposalConnection};
use super::pair::{
    approve_adoption_read, AdoptionHostRequest, AdoptionRead, Cancel, PairOptions, PairPipe,
};
use super::remove::{publish_removal, removal_candidate, RemovalOp, RemoveError};
pub use super::remove::Resume;
use super::{ChainPin, RingClient, RingTimeouts, VerifiedProposal};
use crate::ring::device_code::DeviceCode;
use crate::ring::membership::{enroll_approver, EnrollmentKind};
use crate::ring::pairing::adoption::{enroll_proposal, standalone_unit_to_replace, JoinTarget};
use crate::ring::pairing::{Joined, PairError, PairRefusal};
use crate::ring::{DeviceKeys, RingError, RingId, RosterChain};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the approver waits for its Connector to reach the head it needs (after the
/// standalone removal, or a head that moved).
const HEAD_WAIT: Duration = Duration::from_secs(15);

/// What the caller stores between [`prepare_approval`] and the end of
/// [`publish_approval`], so that [`resume_approval`] can finish the story. No secret: no
/// words, no pairing key, no private key; the removal candidate is a signed Roster that
/// only the Relay's chain can make effective.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalOp {
    /// The joining machine's operation: also the new enrollment unit's id.
    pub operation: String,
    pub ring_id: RingId,
    pub target: JoinTarget,
    pub expires_at: u64,
    /// The head this approval started from; [`resume_approval`] reads the chain from it.
    pub predecessor_version: u64,
    pub predecessor_hash: String,
    /// The removal of the standalone Daemon unit this approval replaces, signed but not
    /// yet published when the operation is first stored.
    #[serde(default)]
    pub replaced: Option<RemovalOp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovedDevice {
    pub chain: RosterChain,
    pub enrollment_id: String,
    pub kind: EnrollmentKind,
    /// The names of the new unit's members, in the accepted head.
    pub names: Vec<String>,
    /// The standalone Daemon unit removed in the same approval.
    pub replaced: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApproveError {
    /// The words do not parse.
    InvalidCode,
    /// Nobody waits with this code: unknown, wrong Relay, or the code screen was closed
    /// before anyone came.
    NotFound,
    /// The code expired.
    Expired,
    /// Another approver used this code first.
    AlreadyUsed,
    /// The joining machine left its code screen (or its pair command ended) during the
    /// exchange.
    Closed,
    /// This device may not approve (not an authorized Desktop of a Computer or Mobile).
    NotApprover,
    /// The Relay cannot be reached.
    Offline,
    /// Cancelled here.
    Cancelled,
    /// Another version took the Ring's head after the joining keys consented to the
    /// proposal (their consent is bound to it): nothing was enrolled; approve again.
    Conflict,
    /// The joining machine, the Relay or a local check refused.
    Refused(String),
    /// Sent, but the outcome is unknown: [`resume_approval`] decides later.
    Uncertain,
}

impl ApproveError {
    pub fn as_code(&self) -> &'static str {
        match self {
            Self::InvalidCode => "invalid_code",
            Self::NotFound => "not_found",
            Self::Expired => "expired",
            Self::AlreadyUsed => "already_used",
            Self::Closed => "closed",
            Self::NotApprover => "not_approver",
            Self::Offline => "offline",
            Self::Cancelled => "cancelled",
            Self::Conflict => "conflict",
            Self::Refused(_) => "refused",
            Self::Uncertain => "uncertain",
        }
    }
}

impl std::fmt::Display for ApproveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "refused: {e}"),
            e => f.write_str(e.as_code()),
        }
    }
}

impl std::error::Error for ApproveError {}

/// Parse typed words; never echoes them in the error.
pub fn parse_code(text: &str) -> Result<DeviceCode, ApproveError> {
    DeviceCode::parse(text).map_err(|_| ApproveError::InvalidCode)
}

/// `local_cancel`: this side's cancel flag was set (else a cancellation came from the
/// joining machine).
pub fn pair_error(e: PairError, local_cancel: bool) -> ApproveError {
    match e {
        PairError::NotFound => ApproveError::NotFound,
        PairError::Cancelled if local_cancel => ApproveError::Cancelled,
        PairError::Cancelled => ApproveError::Closed,
        PairError::Expired | PairError::Timeout => ApproveError::Expired,
        PairError::Invalid(_) => ApproveError::InvalidCode,
        PairError::Refused(PairRefusal::Used) => ApproveError::AlreadyUsed,
        PairError::Relay(m) if m.starts_with("not_approver") => ApproveError::NotApprover,
        PairError::Relay(m) if m.starts_with("offline") => ApproveError::Offline,
        PairError::Relay(m) if m.contains("pipe closed") => ApproveError::Closed,
        PairError::Refused(r) => ApproveError::Refused(r.as_code().into()),
        e => ApproveError::Refused(e.to_string()),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn approved(op: &ApprovalOp, chain: RosterChain) -> ApprovedDevice {
    let names = op
        .target
        .identities()
        .iter()
        .filter_map(|(i, _)| chain.head().member(&i.sign_key).map(|m| m.name.clone()))
        .collect();
    ApprovedDevice {
        enrollment_id: op.operation.clone(),
        kind: op.target.enrollment_kind(),
        names,
        replaced: op.replaced.as_ref().map(|r| r.enrollment_id.clone()),
        chain,
    }
}

/// What became of a stored `op`, from the Relay's chain on `connector`'s connection.
/// Never publishes or signs. `Done`: the new unit (id = the operation) is in the chain.
/// `NotDone { standalone_removed }`: it is not, and says whether the machine's standalone
/// Daemon unit was removed already.
pub fn resume_approval(connector: &Connector, op: &ApprovalOp) -> Resume<ApprovedDevice> {
    let pin = ChainPin {
        version: op.predecessor_version,
        hash: op.predecessor_hash.clone(),
    };
    let Ok(chain) = connector.read_accepted_chain(&pin) else {
        return Resume::Retry;
    };
    let enrolled = chain.versions().iter().any(|v| {
        v.roster()
            .membership()
            .is_ok_and(|m| m.unit(&op.operation).is_some())
    });
    if enrolled {
        return Resume::Done(approved(op, chain));
    }
    let standalone_removed = op.replaced.as_ref().is_some_and(|r| {
        chain
            .head()
            .roster()
            .membership()
            .is_ok_and(|m| m.retired.contains(&r.enrollment_id))
    });
    Resume::NotDone { standalone_removed }
}

/// An approval whose joining machine is on the pipe with a possession-proven intent.
/// Nothing is published yet. Store [`PreparedApproval::op`], then call
/// [`publish_approval`]. Dropping it publishes nothing and ends the exchange.
pub struct PreparedApproval<'c> {
    op: ApprovalOp,
    read: AdoptionRead,
    connector: &'c Connector,
    keys: Arc<DeviceKeys>,
    cancel: Option<Cancel>,
}

impl PreparedApproval<'_> {
    pub fn op(&self) -> &ApprovalOp {
        &self.op
    }
}

/// Meet the machine showing `code` and read its possession-proven intent. `keys` are this
/// device's identity in the Ring `connector` runs; it must be an approver (a Mobile, or a
/// Computer's Desktop). Publishes nothing: a standalone Daemon unit to replace is only
/// signed for removal, into the returned operation.
pub fn prepare_approval<'c>(
    connector: &'c Connector,
    keys: Arc<DeviceKeys>,
    code: &DeviceCode,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
) -> Result<PreparedApproval<'c>, ApproveError> {
    let me = keys.sign_key();
    let start = connector.chain();
    if start.head().roster().v != 2
        || !enroll_approver(start.head(), &me)
        || start.authorized_member(&me).is_none()
    {
        return Err(ApproveError::NotApprover);
    }
    if !matches!(connector.state(), LinkState::Connected { .. }) {
        return Err(ApproveError::Offline);
    }
    let ring_id = start.ring_id().clone();
    let relay = start.head().roster().relay_url.clone();
    let secret = code.secret();
    let stopped = || cancel.is_some_and(|c| c.load(Ordering::Acquire));
    // The exchange is bounded further by the joining machine's own expiry.
    let until = Instant::now() + Duration::from_secs(600);
    let pipe = PairPipe::open(&relay, &secret.slot(), opts).map_err(|e| match e {
        PairError::Relay(_) | PairError::Timeout => ApproveError::Offline,
        e => pair_error(e, stopped()),
    })?;
    // The joining machine holds its slot while its code is shown. Nobody there: an
    // unknown or expired code (never an offer to wait on).
    if !pipe.has_peer() {
        pipe.close();
        return Err(ApproveError::NotFound);
    }
    let read = approve_adoption_read(
        pipe,
        &AdoptionHostRequest {
            keys: &keys,
            secret: &secret,
            ring_id: &ring_id,
            until,
        },
        cancel,
    )
    .map_err(|e| pair_error(e, stopped()))?;
    let head = connector.chain();
    let replaced = match standalone_unit_to_replace(head.head(), &read.intent) {
        Err(e) => {
            read.refuse(&e);
            return Err(pair_error(e, false));
        }
        Ok(None) => None,
        Ok(Some(id)) => match removal_candidate(&head, &*keys, &id, now()) {
            Ok(Some(op)) => Some(op),
            Ok(None) => None,
            Err(e) => {
                let e = PairError::Relay(e.to_string());
                read.refuse(&e);
                return Err(pair_error(e, false));
            }
        },
    };
    Ok(PreparedApproval {
        op: ApprovalOp {
            operation: read.intent.operation.clone(),
            ring_id,
            target: read.intent.target.clone(),
            expires_at: read.intent.expires_at,
            predecessor_version: head.head().version(),
            predecessor_hash: head.head().hash(),
            replaced,
        },
        read,
        connector,
        keys,
        cancel: cancel.cloned(),
    })
}

/// Publish a prepared approval: the standalone removal first (if any; a conflicting head
/// is retried once with a fresh signature), then the proposal, the joining keys' consent,
/// the signed Enroll on the connection that created the proposal, the accepted read-back,
/// and the answer to the joining machine. [`ApproveError::Uncertain`]: the Enroll may have
/// been published; [`resume_approval`] decides. [`ApproveError::Conflict`]: the head moved
/// after consent; nothing was enrolled.
pub fn publish_approval(prepared: PreparedApproval<'_>) -> Result<ApprovedDevice, ApproveError> {
    let PreparedApproval {
        op,
        read,
        connector,
        keys,
        cancel,
    } = prepared;
    let me = keys.sign_key();
    let stopped = || cancel.as_ref().is_some_and(|c| c.load(Ordering::Acquire));
    let deadline = Instant::now() + HEAD_WAIT;
    let mut after = 0;
    if let Some(removal) = &op.replaced {
        if stopped() {
            read.refuse(&PairError::Cancelled);
            return Err(ApproveError::Cancelled);
        }
        match publish_removal(connector, &*keys, removal, deadline) {
            Ok(chain) => after = chain.head().version(),
            Err(e) => {
                let e = match e {
                    RemoveError::Offline => ApproveError::Offline,
                    RemoveError::Conflict => ApproveError::Conflict,
                    RemoveError::Uncertain => ApproveError::Uncertain,
                    RemoveError::NotAuthorized => ApproveError::NotApprover,
                    e => ApproveError::Refused(e.to_string()),
                };
                read.refuse(&PairError::Cancelled);
                return Err(e);
            }
        }
    }
    let lane = RefCell::new(None::<ProposalConnection>);
    let signed = read
        .sign_computer(
            &keys,
            cancel.as_ref(),
            |intent, hash| {
                if !enroll_approver(connector.chain().head(), &me) {
                    return Err(PairError::Relay("not_approver: no longer an approver".into()));
                }
                // The proposal lane needs the Connector at the accepted head (after the
                // standalone removal, when there was one).
                let (chain, captured) = loop {
                    if stopped() {
                        return Err(PairError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(PairError::Relay(
                            "offline: the Ring head did not arrive".into(),
                        ));
                    }
                    if let Ok(captured) = connector.proposal_connection() {
                        let chain = connector.chain();
                        if chain.head() == captured.predecessor()
                            && chain.head().version() >= after
                        {
                            break (chain, captured);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let proposal = enroll_proposal(chain.head(), me, intent, now())?;
                let handle = captured
                    .create(intent, hash, &proposal)
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                *lane.borrow_mut() = Some(captured);
                Ok(VerifiedProposal {
                    chain,
                    proposal,
                    handle,
                })
            },
            &|handle| {
                if let Some(lane) = lane.borrow().as_ref() {
                    let _ = lane.cancel(handle);
                }
            },
        )
        .map_err(|e| pair_error(e, stopped()))?;
    let lane = lane.into_inner().ok_or(ApproveError::Uncertain)?;
    let grant = signed.grant.clone();
    let accepted = RefCell::new(None::<RosterChain>);
    let maybe_sent = RefCell::new(false);
    let conflict = RefCell::new(false);
    let result = signed.publish(cancel.as_ref(), |candidate| {
        if stopped() || now() >= op.expires_at {
            return Err(PairError::Expired);
        }
        *maybe_sent.borrow_mut() = true;
        lane.publish_once(candidate).map_err(|e| {
            if let RingError::Relay { code, .. } = &e {
                *maybe_sent.borrow_mut() = false;
                *conflict.borrow_mut() = matches!(
                    code,
                    super::ErrorCode::RosterConflict | super::ErrorCode::RosterStale
                );
            }
            PairError::Relay(e.to_string())
        })?;
        let chain = lane
            .read_accepted(candidate)
            .map_err(|e| PairError::Relay(e.to_string()))?;
        *accepted.borrow_mut() = Some(chain);
        Ok(Joined {
            ring_id: candidate.ring_id().clone(),
            relay_url: candidate.roster().relay_url.clone(),
            version: candidate.version(),
            hash: candidate.hash(),
            signed_by: candidate.roster().signed_by,
        })
    });
    let _ = lane.cancel(&grant);
    if let Some(chain) = accepted.into_inner() {
        // A lost acknowledgment to the joining machine does not undo the publication.
        return Ok(approved(&op, chain));
    }
    match result {
        Ok(()) => Err(ApproveError::Uncertain),
        Err(_) if *conflict.borrow() => Err(ApproveError::Conflict),
        Err(_) if *maybe_sent.borrow() => Err(ApproveError::Uncertain),
        Err(e) => Err(pair_error(e, stopped())),
    }
}

/// [`prepare_approval`] + [`publish_approval`]; a lost answer is resolved by one
/// [`resume_approval`].
pub fn approve_device_code(
    connector: &Connector,
    keys: Arc<DeviceKeys>,
    code: &DeviceCode,
    opts: &PairOptions,
    cancel: Option<&Cancel>,
) -> Result<ApprovedDevice, ApproveError> {
    let prepared = prepare_approval(connector, keys, code, opts, cancel)?;
    let op = prepared.op().clone();
    match publish_approval(prepared) {
        Err(ApproveError::Uncertain) => match resume_approval(connector, &op) {
            Resume::Done(device) => Ok(device),
            _ => Err(ApproveError::Uncertain),
        },
        r => r,
    }
}

/// What a joining phone stores before it releases its proofs: no secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JoinRecord {
    pub ring_id: RingId,
    pub relay_url: String,
    /// The operation: also the id of the unit it would be enrolled in.
    pub operation: String,
    /// The head the proposal was built on (the proposal handle's `version` and `head`).
    pub predecessor_version: u64,
    pub predecessor_hash: String,
    pub expires_at: u64,
}

/// What [`check_join`] found.
#[derive(Clone, Debug, PartialEq)]
pub enum JoinCheck {
    /// Enrolled: the verified chain lists this device in the unit of `operation`.
    Enrolled(RosterChain),
    /// The Relay refuses this device as a member, or the chain lacks the unit. Final only
    /// once `expires_at` has passed (before, the approver may still publish).
    NotEnrolled,
    /// The Relay could not be reached: try later.
    Retry,
}

/// A joining device's read-only reconcile after a crash between releasing its proofs and
/// storing the chain: it authenticates as itself and reads the chain from the pinned
/// predecessor (the same read a Desktop's reconciler makes). `tls`: the same value the
/// device's `RingClientConfig::tls` holds (`None` by default).
pub fn check_join(
    keys: Arc<DeviceKeys>,
    record: &JoinRecord,
    tls: Option<Arc<rustls::ClientConfig>>,
    timeouts: RingTimeouts,
) -> JoinCheck {
    let pin = ChainPin {
        version: record.predecessor_version,
        hash: record.predecessor_hash.clone(),
    };
    let me = keys.sign_key();
    match RingClient::fetch_chain(&record.relay_url, &record.ring_id, keys, &pin, tls, timeouts) {
        Ok(chain) => {
            let listed = chain.head().roster().membership().is_ok_and(|m| {
                m.unit(&record.operation)
                    .is_some_and(|u| u.members.contains(&me))
            });
            if listed && chain.authorized_member(&me).is_some() {
                JoinCheck::Enrolled(chain)
            } else {
                JoinCheck::NotEnrolled
            }
        }
        Err(RingError::Relay { code, .. })
            if matches!(
                code,
                super::ErrorCode::NotMember
                    | super::ErrorCode::Removed
                    | super::ErrorCode::BadSignature
                    | super::ErrorCode::NoRoster
            ) =>
        {
            JoinCheck::NotEnrolled
        }
        Err(_) => JoinCheck::Retry,
    }
}

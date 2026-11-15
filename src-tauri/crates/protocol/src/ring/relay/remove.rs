//! Remove one enrollment unit from the Ring: any unit, the signer's own included. For a
//! Connector owner without its own journal (the Mobile). Resumable:
//! [`prepare_removal`] builds the signed candidate ([`RemovalOp`], no secret) that the
//! caller stores before anything is published, [`publish_removal`] gets it to the Relay,
//! and after a crash or a lost answer [`resume_removal`] reads the Relay's chain and says
//! whether it happened. [`remove_enrollment`] is prepare + publish.
//!
//! Publish first, then commit: nothing is accepted locally before the Relay holds the
//! candidate. A conflicting head is retried once with a fresh signature on the head that
//! won (never the old bytes moved).
use super::connector::{Connector, LinkState};
use super::{ChainPin, ErrorCode};
use crate::ring::membership::remove_unit;
use crate::ring::{RingError, RingId, RosterChain, RosterError, SignedRoster, Signer};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// A prepared removal, stored by the caller before [`publish_removal`]. No secret: the
/// candidate is a signed Roster that only the Relay's chain can make effective.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemovalOp {
    pub ring_id: RingId,
    pub enrollment_id: String,
    /// The signed Remove (Roster token) for the version after its predecessor.
    pub candidate: String,
}

impl RemovalOp {
    pub fn candidate(&self) -> Result<SignedRoster, RingError> {
        Ok(SignedRoster::parse(&self.candidate)?)
    }
    fn predecessor(&self) -> Result<ChainPin, RingError> {
        let c = self.candidate()?;
        Ok(ChainPin {
            version: c.version() - 1,
            hash: c
                .roster()
                .prev
                .clone()
                .ok_or_else(|| RingError::Invalid("removal without a predecessor".into()))?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoveError {
    /// Not connected to the Relay: nothing was sent.
    Offline,
    /// Another version won the slot twice.
    Conflict,
    /// Sent, but no answer: [`resume_removal`] decides.
    Uncertain,
    /// The Relay or the local verifier refused the candidate.
    Refused(String),
    /// This device may not remove that unit (not a member, or not an approver and not a
    /// member of the unit).
    NotAuthorized,
    /// The unit is not in the Ring (already removed): callers treat it as done.
    NotInRing,
    /// After a conflict, the replacement record could not be stored by the caller, so it
    /// was not sent. The stored original can never land (another version took its slot).
    Journal(String),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => f.write_str("offline: can't reach your Ring"),
            Self::Conflict => f.write_str("conflict: the Ring changed meanwhile, try again"),
            Self::Uncertain => f.write_str("uncertain: no answer from the Relay, try again"),
            Self::Refused(e) => write!(f, "refused: {e}"),
            Self::NotAuthorized => f.write_str("not_authorized: this device can't remove it"),
            Self::NotInRing => f.write_str("not_in_ring: it is not in the Ring"),
            Self::Journal(e) => write!(f, "journal: the retry was not recorded ({e})"),
        }
    }
}

impl std::error::Error for RemoveError {}

/// Where a stored operation stands ([`resume_removal`], and
/// [`super::approve::resume_approval`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Resume<T> {
    /// It happened: the Relay's verified chain holds it.
    Done(T),
    /// It did not happen and never can (another version took the slot, or nothing that
    /// could still be accepted was sent): safe to forget and start again. For an approval,
    /// `standalone_removed` says the machine's standalone Daemon unit was removed already
    /// (the machine returns to "not in a Ring" by itself and joins again with a new code).
    NotDone { standalone_removed: bool },
    /// The Relay's chain could not be read (offline, connection replaced): try later.
    Retry,
}

/// The signed Remove of `enrollment_id` on the Connector's chain (its terminal evidence
/// included). Signs only; nothing is sent. [`RemoveError::NotInRing`]: already gone.
pub fn prepare_removal(
    connector: &Connector,
    signer: &dyn Signer,
    enrollment_id: &str,
) -> Result<RemovalOp, RemoveError> {
    removal_candidate(&connector.chain(), signer, enrollment_id, now())?
        .ok_or(RemoveError::NotInRing)
}

/// [`prepare_removal`] on an explicit chain. `Ok(None)`: the unit is not in the head.
pub(crate) fn removal_candidate(
    chain: &RosterChain,
    signer: &dyn Signer,
    enrollment_id: &str,
    now: u64,
) -> Result<Option<RemovalOp>, RemoveError> {
    let head = chain.head();
    let m = head
        .roster()
        .membership()
        .map_err(|e| RemoveError::Refused(e.to_string()))?;
    if m.unit(enrollment_id).is_none() {
        return Ok(None);
    }
    if chain.authorized_member(&signer.sign_key()).is_none() {
        return Err(RemoveError::NotAuthorized);
    }
    let next = remove_unit(head, signer, now, enrollment_id).map_err(|e| match e {
        RingError::Roster(RosterError::SignerNotMember)
        | RingError::Roster(RosterError::SignerNotDesktop) => RemoveError::NotAuthorized,
        RingError::Roster(RosterError::Invalid(m)) if m.contains("signer scope") => {
            RemoveError::NotAuthorized
        }
        e => RemoveError::Refused(e.to_string()),
    })?;
    chain
        .extended(std::slice::from_ref(&next))
        .map_err(|e| RemoveError::Refused(e.to_string()))?;
    Ok(Some(RemovalOp {
        ring_id: chain.ring_id().clone(),
        enrollment_id: enrollment_id.into(),
        candidate: next.token().into(),
    }))
}

fn conflict(e: &RingError) -> bool {
    matches!(
        e,
        RingError::Relay {
            code: ErrorCode::RosterConflict | ErrorCode::RosterStale,
            ..
        } | RingError::Roster(RosterError::Stale | RosterError::PrevMismatch | RosterError::Gap)
    )
}

/// Upload `op`'s candidate on `connector`'s connection, fenced by its terminal evidence,
/// and wait for the Relay's answer until `deadline`. A conflicting head is retried once
/// with a fresh signature by `signer` on the head that won (never the old bytes moved);
/// a second conflict is [`RemoveError::Conflict`]. A unit that is already gone is done.
/// `Ok`: the Relay holds the removal; the chain returned is the Connector's chain with
/// it. The caller persists that chain unless it removed itself.
///
/// `on_replacement` is called with the retry's new record before the retry is sent: the
/// caller stores it in place of `op`, so that [`resume_removal`] after a lost answer
/// judges the candidate that may still land. An `Err` from it sends nothing more
/// ([`RemoveError::Journal`]; the stored original can never land).
pub fn publish_removal(
    connector: &Connector,
    signer: &dyn Signer,
    op: &RemovalOp,
    deadline: Instant,
    on_replacement: &mut dyn FnMut(&RemovalOp) -> Result<(), String>,
) -> Result<RosterChain, RemoveError> {
    let mut op = op.clone();
    let mut retried = false;
    loop {
        match publish_once(connector, &op, deadline) {
            Err(RemoveError::Conflict) if !retried => {
                retried = true;
                let head = op
                    .candidate()
                    .ok()
                    .and_then(|c| c.roster().prev.clone())
                    .unwrap_or_default();
                while connector.chain().head().hash() == head && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                match removal_candidate(&connector.chain(), signer, &op.enrollment_id, now())? {
                    Some(fresh) => {
                        on_replacement(&fresh).map_err(RemoveError::Journal)?;
                        op = fresh;
                    }
                    None => return Ok(connector.chain()),
                }
            }
            r => return r,
        }
    }
}

fn publish_once(
    connector: &Connector,
    op: &RemovalOp,
    deadline: Instant,
) -> Result<RosterChain, RemoveError> {
    let candidate = op
        .candidate()
        .map_err(|e| RemoveError::Refused(e.to_string()))?;
    if !matches!(connector.state(), LinkState::Connected { .. }) {
        return Err(RemoveError::Offline);
    }
    match connector.publish_fenced_until(&candidate, deadline, None) {
        Ok(()) => {
            let chain = connector.chain();
            match chain.get(candidate.version()) {
                Some(r) if r == &candidate => Ok(chain),
                Some(_) => Err(RemoveError::Conflict),
                None => chain
                    .extended(std::slice::from_ref(&candidate))
                    .map(|(c, _)| c)
                    .map_err(|e| RemoveError::Refused(e.to_string())),
            }
        }
        Err(e) if conflict(&e) => Err(RemoveError::Conflict),
        Err(e @ (RingError::Relay { .. } | RingError::Roster(_))) => {
            Err(RemoveError::Refused(e.to_string()))
        }
        Err(_) => Err(RemoveError::Uncertain),
    }
}

/// What became of a stored `op` (the original record, or the replacement
/// [`publish_removal`] handed to its `on_replacement`), from the Relay's accepted chain on
/// `connector`'s connection. The removal is in the chain (or the unit is retired anyway): `Done`. The
/// head moved past the record's predecessor without it: `NotDone` (the stored candidate
/// can never be accepted). The head is still the predecessor: the identical stored signed
/// candidate is sent again (idempotent; it completes the user's own action and is no new
/// operation): `Done` once the Relay holds it, `Retry` when offline. A self-removal that
/// was accepted closes this device's connection: the read then gives [`Resume::Retry`],
/// and the owner learns the outcome from [`super::check_removal`].
pub fn resume_removal(connector: &Connector, op: &RemovalOp) -> Resume<RosterChain> {
    let (Ok(candidate), Ok(pin)) = (op.candidate(), op.predecessor()) else {
        return Resume::NotDone {
            standalone_removed: false,
        };
    };
    let Ok(accepted) = connector.read_accepted_chain(&pin) else {
        return Resume::Retry;
    };
    let retired = |c: &RosterChain| {
        c.head()
            .roster()
            .membership()
            .is_ok_and(|m| m.retired.contains(&op.enrollment_id))
    };
    if accepted.get(candidate.version()) == Some(&candidate) || retired(&accepted) {
        return Resume::Done(accepted);
    }
    if accepted.head().version() != pin.version {
        return Resume::NotDone {
            standalone_removed: false,
        };
    }
    match publish_once(connector, op, Instant::now() + RESEND_WAIT) {
        Ok(chain) => Resume::Done(chain),
        Err(RemoveError::Conflict | RemoveError::Refused(_)) => {
            match connector.read_accepted_chain(&pin) {
                Ok(c) if c.get(candidate.version()) == Some(&candidate) || retired(&c) => {
                    Resume::Done(c)
                }
                Ok(_) => Resume::NotDone {
                    standalone_removed: false,
                },
                Err(_) => Resume::Retry,
            }
        }
        Err(_) => Resume::Retry,
    }
}

/// How long [`resume_removal`] waits for the Relay to take the re-sent candidate.
const RESEND_WAIT: Duration = Duration::from_secs(15);

/// [`prepare_removal`] + [`publish_removal`]; a lost answer is resolved by one
/// [`resume_removal`]. A unit already gone is done. Needs a connected Relay: offline is
/// [`RemoveError::Offline`] and nothing is queued.
pub fn remove_enrollment(
    connector: &Connector,
    signer: &dyn Signer,
    enrollment_id: &str,
    deadline: Instant,
) -> Result<RosterChain, RemoveError> {
    let op = match prepare_removal(connector, signer, enrollment_id) {
        Err(RemoveError::NotInRing) => return Ok(connector.chain()),
        r => r?,
    };
    let mut stored = op.clone();
    let result = publish_removal(connector, signer, &op, deadline, &mut |fresh| {
        stored = fresh.clone();
        Ok(())
    });
    match result {
        Err(RemoveError::Uncertain) => match resume_removal(connector, &stored) {
            Resume::Done(chain) => Ok(chain),
            _ => Err(RemoveError::Uncertain),
        },
        r => r,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

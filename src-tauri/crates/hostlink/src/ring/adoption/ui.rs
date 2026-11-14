//! What the Desktop's Ring UI needs on top of the controllers: the registered local
//! counterpart's client, a Join whose start the UI can cancel while it waits for the Relay
//! slot, and a read-only answer to "can this Desktop approve a Device Code?".
use super::*;
use crate::ring::store::{OutgoingPhase, ProfileState};
use std::collections::HashSet;
use xshell_protocol::ring::device_code::DeviceCode;

/// Whether this Desktop can approve a Device Code, and if not, why. Read-only: no admission,
/// no network, no counterpart request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalReadiness {
    /// An authorized v2 Computer Desktop.
    Ready,
    /// A retained v1 Ring: explicit Upgrade, then completion.
    NeedsUpgrade,
    /// A v2 Ring where this Desktop is still a singleton: explicit local completion.
    NeedsCompletion,
    /// No retained Desktop profile yet.
    NoProfile,
    /// A profile without membership (unpaired).
    NotEnrolled,
    /// Earlier adoption/proposal work is unresolved and needs reconciliation first.
    Unresolved,
    /// A retained Ring that no longer authorizes this Desktop (removed, or its unit ended)
    /// while no local adoption record is unresolved: nothing to reconcile, nothing to approve.
    Removed,
}

/// The operation ids of the durable adoption records (reservation, journal, outgoing).
pub(in crate::ring) fn record_operations(stored: &ProfileState) -> HashSet<String> {
    stored
        .pending_join
        .iter()
        .map(|p| p.intent.operation.clone())
        .chain(stored.journal.iter().map(|j| j.intent.operation.clone()))
        .chain(stored.outgoing.iter().map(|o| o.intent.operation.clone()))
        .collect()
}

/// The operation ids of the unresolved records.
fn unresolved_operations(stored: &ProfileState) -> Vec<&str> {
    let mut ops = vec![];
    if let Some(p) = &stored.pending_join {
        ops.push(p.intent.operation.as_str());
    }
    if let Some(j) = stored
        .journal
        .as_ref()
        .filter(|j| !j.phase.permits_new_operation())
    {
        ops.push(j.intent.operation.as_str());
    }
    if let Some(o) = stored
        .outgoing
        .as_ref()
        .filter(|o| !matches!(o.phase, OutgoingPhase::Published | OutgoingPhase::Cancelled))
    {
        ops.push(o.intent.operation.as_str());
    }
    ops
}

/// `live(op)`: the record belongs to the operation this process is driving right now (made
/// under the current admission). Only those are live work; any other unresolved record is
/// uncertainty, whatever admission is held.
pub(in crate::ring) fn readiness_of(
    stored: Option<&ProfileState>,
    live: impl Fn(&str) -> bool,
) -> ApprovalReadiness {
    let Some(stored) = stored else {
        return ApprovalReadiness::NoProfile;
    };
    if unresolved_operations(stored)
        .into_iter()
        .any(|op| !live(op))
    {
        return ApprovalReadiness::Unresolved;
    }
    let Some(member) = &stored.membership else {
        return ApprovalReadiness::NotEnrolled;
    };
    let head = member.chain.head().roster();
    if head.v == 1 {
        return ApprovalReadiness::NeedsUpgrade;
    }
    let me = stored.keys.sign_key();
    let computer = head
        .membership()
        .ok()
        .and_then(|m| {
            m.unit_for(&me)
                .map(|unit| unit.kind == EnrollmentKind::Computer)
        })
        .unwrap_or(false);
    let desktop = member
        .chain
        .authorized_member(&me)
        .is_some_and(|m| m.role == xshell_protocol::ring::Role::Desktop);
    if computer && desktop {
        ApprovalReadiness::Ready
    } else if desktop {
        ApprovalReadiness::NeedsCompletion
    } else {
        // Removed or terminal. Unresolved records were ruled out above, so this is a
        // definitive removal, not uncertainty.
        ApprovalReadiness::Removed
    }
}

impl DesktopRing {
    /// The client of this Desktop's registered local counterpart, from the retained
    /// profile only. `not_registered:` until the startup registration has succeeded.
    pub fn local_companion_client(&self) -> Result<CompanionClient, String> {
        let profile = self
            .store
            .load_profile()?
            .0
            .and_then(|p| p.companion)
            .ok_or("not_registered: this computer's Host is not registered yet")?;
        profile
            .validate()
            .map_err(|e| format!("not_registered: {e}"))?;
        Ok(CompanionClient::for_profile(&profile))
    }

    /// [`DesktopRing::begin_join`] with an external cancel flag that also ends the wait for
    /// the Relay slot. A cancelled start never returns an offer: its driver is cancelled and
    /// joined, so no words can be shown for it.
    /// `relay_url`: the user's own Relay chosen before Join, else the hosted default.
    pub fn begin_join_cancellable(
        self: &Arc<Self>,
        client: &CompanionClient,
        cancel: Arc<AtomicBool>,
        relay_url: Option<&str>,
    ) -> Result<JoinOffer, String> {
        let relay = relay_url.unwrap_or(&self.cfg.default_relay_url).to_string();
        xshell_protocol::ring::url::RelayUrl::parse(&relay).map_err(|e| e.to_string())?;
        let ttl = self.cfg.pair_ttl.min(Duration::from_secs(600));
        let until = Instant::now() + ttl;
        let offer = self.begin_join_with_offer_cancel(
            client,
            JoinOfferInput {
                operation: nonce()?,
                code: DeviceCode::generate().map_err(|e| e.to_string())?,
                relay_url: relay,
                expires_at: now() + ttl.as_secs(),
            },
            until,
            cancel.clone(),
        )?;
        loop {
            if cancel.load(Ordering::Acquire) {
                offer.cancel_join();
                drop(offer); // joins the driver
                return Err("cancelled: Join cancelled before its code was shown".into());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("waiting for Join readiness timed out".into());
            }
            match offer.await_ready(left.min(Duration::from_millis(50))) {
                Ok(()) => {
                    if cancel.load(Ordering::Acquire) {
                        continue;
                    }
                    return Ok(offer);
                }
                Err(error) => {
                    if matches!(
                        offer.join_status(),
                        JoinStatus::Failed { .. } | JoinStatus::ReconciliationRequired { .. }
                    ) {
                        if cancel.load(Ordering::Acquire) {
                            return Err(
                                "cancelled: Join cancelled before its code was shown".into()
                            );
                        }
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Leave this Ring locally after another device removed this Computer: the old Ring
    /// is not contacted. Refused unless readiness is [`ApprovalReadiness::Removed`] and no
    /// adoption runs. The Daemon leaves first (idempotent), then the Desktop store sets
    /// its membership aside atomically. An interruption between the two leaves this
    /// Desktop Removed, so Leave is offered again; nothing resurrects the old Ring.
    pub fn leave_removed(&self, client: &CompanionClient) -> Result<(), String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns the Desktop counterpart".into());
        }
        let removed = |stored: &ProfileState| match readiness_of(Some(stored), |_| false) {
            ApprovalReadiness::Removed => Ok(()),
            ApprovalReadiness::Unresolved => {
                Err("reconciliation_required: earlier Ring work is unresolved".to_string())
            }
            _ => Err("not_removed: this Desktop is still a member of its Ring".to_string()),
        };
        let stored = self.store.load_profile()?.0;
        match stored.as_ref() {
            Some(stored) if stored.membership.is_some() => removed(stored)?,
            _ => return Err("not_enrolled: this Desktop is not in a Ring".into()),
        }
        match client.request(CompanionRequest::LeaveRemoved)? {
            CompanionResponse::Left => {}
            _ => return Err("invalid counterpart Leave response".into()),
        }
        self.adoption_test_point(super::super::desktop::AdoptionTestPoint::LeaveDaemonLeft);
        self.store.leave_removed(removed)?;
        self.forget_membership();
        Ok(())
    }

    /// Read-only approval readiness from the retained profile.
    pub fn approval_readiness(&self) -> Result<ApprovalReadiness, String> {
        let stored = self.store.load_profile()?.0;
        let baseline = self
            .adoption_baseline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let held = self.adoption.load(Ordering::Acquire);
        Ok(readiness_of(stored.as_ref(), |op| {
            held && baseline.as_ref().is_some_and(|b| !b.contains(op))
        }))
    }

    /// An adoption, approval or registration holds the admission: nothing new can start.
    /// Separate from `Unresolved`, which is durable uncertainty.
    pub fn adoption_busy(&self) -> bool {
        self.adoption.load(Ordering::Acquire)
    }

    /// The retained Ring's Roster format (1 or 2), if this Desktop is in a Ring.
    pub fn ring_format(&self) -> Result<Option<u8>, String> {
        Ok(self
            .store
            .load_profile()?
            .0
            .and_then(|p| p.membership)
            .map(|m| m.chain.head().roster().v as u8))
    }
}

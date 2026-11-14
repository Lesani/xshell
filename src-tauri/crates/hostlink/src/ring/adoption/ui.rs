//! What the Desktop's Ring UI needs on top of the controllers: the registered local
//! counterpart's client, a Join whose start the UI can cancel while it waits for the Relay
//! slot, and a read-only answer to "can this Desktop approve a Device Code?".
use super::*;
use crate::ring::store::{OutgoingPhase, ProfileState};
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
}

/// `in_flight`: a local adoption/approval holds the admission right now, so its durable
/// journal or reservation is live work, not unresolved uncertainty.
pub(in crate::ring) fn readiness_of(
    stored: Option<&ProfileState>,
    in_flight: bool,
) -> ApprovalReadiness {
    let Some(stored) = stored else {
        return ApprovalReadiness::NoProfile;
    };
    if !in_flight
        && (stored.pending_join.is_some()
            || stored
                .journal
                .as_ref()
                .is_some_and(|j| !j.phase.permits_new_operation())
            || stored.outgoing.as_ref().is_some_and(|j| {
                !matches!(j.phase, OutgoingPhase::Published | OutgoingPhase::Cancelled)
            }))
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
        // Removed or terminal: nothing this Desktop can do but reconcile/leave.
        ApprovalReadiness::Unresolved
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
        Ok(CompanionClient::new(std::path::PathBuf::from(
            &profile.endpoint,
        )))
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

    /// Read-only approval readiness from the retained profile.
    pub fn approval_readiness(&self) -> Result<ApprovalReadiness, String> {
        let in_flight = self.adoption.load(Ordering::Acquire);
        Ok(readiness_of(
            self.store.load_profile()?.0.as_ref(),
            in_flight,
        ))
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

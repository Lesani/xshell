//! What the Desktop's Ring UI needs on top of the controllers: the registered local
//! counterpart's client, a Join whose start the UI can cancel while it waits for the Relay
//! slot, and a read-only answer to "where does this Desktop stand in a Ring?".
use super::*;
use crate::ring::store::{OutgoingPhase, ProfileState};
use std::collections::HashSet;
use xshell_protocol::ring::device_code::DeviceCode;

/// Where this Desktop stands in a Ring, for the UI. Read-only: no admission, no network,
/// no counterpart request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MembershipState {
    /// In no Ring (never joined, or set aside after a removal it did itself).
    None,
    /// An authorized format-2 Computer Desktop: it can approve, remove and pair the phone.
    Member,
    /// Earlier Join, Create or approval work is being finished automatically ("Finishing…").
    /// Create and Join wait until it is done.
    Finishing,
    /// Another device removed this machine ("This machine was removed from your Ring").
    /// Join again or Create clears it.
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

/// The operation ids of the unresolved records, owned.
pub(in crate::ring) fn unresolved(stored: &ProfileState) -> Vec<String> {
    unresolved_operations(stored)
        .into_iter()
        .map(String::from)
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
/// unfinished work that the reconciler finishes ([`MembershipState::Finishing`]).
pub(in crate::ring) fn membership_of(
    stored: Option<&ProfileState>,
    live: impl Fn(&str) -> bool,
) -> MembershipState {
    let Some(stored) = stored else {
        return MembershipState::None;
    };
    if unresolved_operations(stored)
        .into_iter()
        .any(|op| !live(op))
    {
        return MembershipState::Finishing;
    }
    if stored.removed.is_some() {
        return MembershipState::Removed;
    }
    let Some(member) = &stored.membership else {
        return MembershipState::None;
    };
    let head = member.chain.head().roster();
    let me = stored.keys.sign_key();
    let computer = head.v == 2
        && head.membership().ok().is_some_and(|m| {
            m.unit_for(&me)
                .is_some_and(|unit| unit.kind == EnrollmentKind::Computer)
        });
    let desktop = member
        .chain
        .authorized_member(&me)
        .is_some_and(|m| m.role == xshell_protocol::ring::Role::Desktop);
    if computer && desktop {
        MembershipState::Member
    } else if desktop {
        // A format-1 Ring or a singleton Desktop: set aside at the next start.
        MembershipState::None
    } else {
        // The retained chain no longer authorizes this Desktop: the removal check sets it
        // aside and shows the notice.
        MembershipState::Removed
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
    /// joined, so no words can be shown for it. Only `cancel` (the caller's) means
    /// "cancelled": the driver gets its own flag, which it also raises when it ends, so a
    /// driver that failed (an unreachable Relay) reports its error, never a cancellation.
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
            Arc::new(AtomicBool::new(false)),
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
                        JoinStatus::Failed { .. } | JoinStatus::Finishing { .. }
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

    /// Where this Desktop stands, from the retained profile.
    pub fn membership_state(&self) -> Result<MembershipState, String> {
        let stored = self.store.load_profile()?.0;
        let baseline = self
            .adoption_baseline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let held = self.adoption.load(Ordering::Acquire);
        Ok(membership_of(stored.as_ref(), |op| {
            held && baseline.as_ref().is_some_and(|b| !b.contains(op))
        }))
    }

    /// The notices for the UI: another device removed this Desktop, and when a format-1
    /// Ring was set aside at start (seconds).
    #[allow(clippy::type_complexity)]
    pub fn notices(
        &self,
    ) -> Result<(Option<crate::ring::store::RemovedNotice>, Option<u64>), String> {
        Ok(self
            .store
            .load_profile()?
            .0
            .map(|p| (p.removed, p.old_ring))
            .unwrap_or_default())
    }

    /// Whether the startup registration loop must register this Desktop with the local
    /// Daemon now: no registration is stored, or the Daemon no longer holds it (it was
    /// replaced, or the Daemon set its membership aside and rotated its identity; a stale
    /// registration is then dropped while this Desktop is in no Ring). Only the pairing
    /// owner registers. A Daemon that does not answer needs nothing now.
    pub fn companion_registration_needed(&self) -> bool {
        if !self.pairing_ipc_owner() {
            return false;
        }
        let Ok((Some(stored), _)) = self.store.load_profile() else {
            return true;
        };
        let Some(profile) = &stored.companion else {
            return true;
        };
        if profile.desktop.sign_key != stored.keys.sign_key() {
            return true;
        }
        match CompanionClient::for_profile(profile).request(CompanionRequest::Descriptor) {
            Ok(CompanionResponse::Identity {
                identity,
                profile: held,
            }) => {
                if held.as_ref() == Some(profile) {
                    false
                } else if identity.sign_key == profile.daemon.sign_key
                    && identity.noise_key == profile.daemon.noise_key
                {
                    true
                } else {
                    self.store.forget_companion(profile).unwrap_or(false)
                }
            }
            _ => false,
        }
    }

    /// An adoption, approval or registration holds the admission: nothing new can start.
    /// Separate from `Unresolved`, which is durable uncertainty.
    pub fn adoption_busy(&self) -> bool {
        self.adoption.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::store::{PendingJoin, RemovedNotice, RingState};
    use xshell_protocol::ring::pairing::adoption::{AdoptionIntent, JoinTarget};
    use xshell_protocol::ring::{RosterChain, SignedRoster};

    fn bare() -> ProfileState {
        ProfileState {
            keys: Arc::new(DeviceKeys::generate().unwrap()),
            membership: None,
            journal: None,
            companion: None,
            pending_join: None,
            outgoing: None,
            removed: None,
            old_ring: None,
        }
    }

    #[test]
    fn membership_of_none_member_finishing_removed() {
        assert_eq!(membership_of(None, |_| false), MembershipState::None);
        let p = bare();
        assert_eq!(membership_of(Some(&p), |_| false), MembershipState::None);
        // A notice: removed.
        let mut removed = bare();
        removed.removed = Some(RemovedNotice {
            ring_id: RingId::derive(&removed.keys.sign_key()),
            version: 3,
            at: 1,
        });
        assert_eq!(
            membership_of(Some(&removed), |_| false),
            MembershipState::Removed
        );
        // An unresolved record that is not this process's live work: finishing, even with
        // a notice.
        let mut finishing = removed.clone();
        let daemon = DeviceKeys::generate().unwrap();
        let identity = |k: &DeviceKeys| PublicIdentity {
            name: "x".into(),
            sign_key: k.sign_key(),
            noise_key: k.noise_key(),
        };
        let op = xshell_protocol::ring::b64::encode(&[9; 32]);
        finishing.pending_join = Some(PendingJoin {
            retained_predecessor: vec![],
            intent: AdoptionIntent {
                operation: op.clone(),
                expires_at: 10,
                target: JoinTarget::Computer {
                    desktop: identity(&finishing.keys),
                    daemon: identity(&daemon),
                },
            },
            relay_url: "wss://r.example".into(),
        });
        assert_eq!(
            membership_of(Some(&finishing), |_| false),
            MembershipState::Finishing
        );
        // The same record driven live by this process is not "finishing".
        assert_eq!(
            membership_of(Some(&finishing), |o| o == op),
            MembershipState::Removed
        );
        // A format-1 member is not a member (set aside at the next start).
        let mut v1 = bare();
        let g = SignedRoster::genesis(&*v1.keys, v1.keys.noise_key(), "d", "wss://r.example", 1)
            .unwrap();
        v1.membership = Some(RingState {
            keys: v1.keys.clone(),
            chain: RosterChain::from_chain(vec![g]).unwrap(),
            local_member: None,
            farewells: vec![],
            held_relays: Default::default(),
            host_members: Default::default(),
        });
        assert_eq!(membership_of(Some(&v1), |_| false), MembershipState::None);
    }
}

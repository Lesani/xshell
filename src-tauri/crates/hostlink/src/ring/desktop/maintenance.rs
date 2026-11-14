//! Automatic upkeep of this Desktop's membership, so that nothing needs the user to
//! resolve it ("Ring just works"):
//! - at start, a stored format-1 Ring (or format-1 work in the journal, or a singleton
//!   Desktop of an earlier build) is set aside, and a stored chain that no longer
//!   authorizes this Desktop is set aside as a removal;
//! - a background reconciler finishes Join, Create and approval work left unresolved by a
//!   crash or a lost answer ("Finishing…"), retrying while the Relay is unreachable;
//! - a removal is detected by the Desktop itself: when the Relay refuses or closes the
//!   connection as removed, or a verified chain drops this Desktop, it reads its signed
//!   history as a (former) member and sets its membership aside only on a verified removal.
//!
//! Every set-aside also rotates this Desktop's identity (see `Store::set_aside`); the
//! registration loop then registers the new identity with the local Daemon.
use super::*;
use crate::ring::store::{AsideReason, ProfileState, RemovedNotice};
use std::sync::Condvar;
use xshell_protocol::ring::companion::{CompanionRequest, ConsentTranscript};
use xshell_protocol::ring::membership::RosterChange;
use xshell_protocol::ring::relay::{check_removal, RemovalCheck};

#[derive(Default)]
struct State {
    kick: bool,
    reconciler: bool,
    removal: bool,
    /// The reconciler's last attempt could not reach the Relay.
    offline: bool,
}

#[derive(Default)]
pub(in crate::ring) struct Maintenance {
    st: Mutex<State>,
    cv: Condvar,
}

impl Maintenance {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The reconciler's retry delays.
const RECONCILE_MIN: Duration = Duration::from_secs(1);
const RECONCILE_MAX: Duration = Duration::from_secs(60);
/// The removal check's retry delays.
const REMOVAL_MIN: Duration = Duration::from_secs(5);
const REMOVAL_MAX: Duration = Duration::from_secs(300);

/// Format-1 state this build no longer supports (D2a): a format-1 head, a singleton Desktop
/// of a format-2 Ring (it needed the old local completion), or format-1 work (Upgrade, local
/// completion, retained completion) still recorded.
fn format1(stored: &ProfileState) -> bool {
    let me = stored.keys.sign_key();
    let head_v1 = stored.membership.as_ref().is_some_and(|m| {
        let head = m.chain.head();
        head.roster().v == 1
            || (m.chain.authorized_member(&me).is_some()
                && head
                    .roster()
                    .membership()
                    .ok()
                    .and_then(|ms| ms.unit_for(&me).map(|u| u.kind.clone()))
                    .is_some_and(|k| k != EnrollmentKind::Computer))
    });
    let local_v1 = |r: &xshell_protocol::ring::Roster| {
        matches!(
            r.membership().map(|m| m.change),
            Ok(RosterChange::Upgrade { .. } | RosterChange::CompleteComputer { .. })
        )
    };
    let journal_v1 = stored.journal.as_ref().is_some_and(|j| {
        matches!(j.transcript, ConsentTranscript::Local) && local_v1(&j.proposal)
    });
    let outgoing_v1 = stored
        .outgoing
        .as_ref()
        .is_some_and(|o| o.final_noise_hash.is_none());
    let pending_v1 = stored
        .pending_join
        .as_ref()
        .is_some_and(|p| !p.retained_predecessor.is_empty());
    head_v1 || journal_v1 || outgoing_v1 || pending_v1
}

impl DesktopRing {
    /// At open, before the stored Ring is adopted: format-1 state is set aside (`V1`), and
    /// a stored format-2 chain that no longer authorizes this Desktop is set aside as a
    /// removal (the local verified chain is the evidence). Best effort: an unreadable
    /// profile is left to the normal recovery report.
    pub(in crate::ring) fn set_aside_stale(&self) {
        let Ok((Some(stored), _)) = self.store.load_profile() else {
            return;
        };
        if format1(&stored) {
            self.tell_daemon_left(&stored, None);
            match self.store.set_aside(AsideReason::V1, |p| {
                format1(p)
                    .then_some(())
                    .ok_or_else(|| "changed".to_string())
            }) {
                Ok(true) => eprintln!("xshell: a format-1 Ring was set aside; this computer is in no Ring"),
                Ok(false) => {}
                Err(e) => eprintln!("xshell: cannot set the format-1 Ring aside: {e}"),
            }
            return;
        }
        let Some(member) = &stored.membership else {
            return;
        };
        if member
            .chain
            .authorized_member(&stored.keys.sign_key())
            .is_none()
        {
            let evidence = member.chain.clone();
            self.tell_daemon_left(&stored, Some(&evidence));
            if let Err(e) = self.set_aside_removed(&evidence) {
                eprintln!("xshell: cannot set the removed membership aside: {e}");
            }
        }
    }

    /// Best effort: the local Daemon sets its own membership aside too (an internal
    /// accelerator; the Daemon also detects its removal by itself).
    fn tell_daemon_left(&self, stored: &ProfileState, evidence: Option<&RosterChain>) {
        let Some(profile) = &stored.companion else {
            return;
        };
        let client = super::super::companion_ipc::CompanionClient::for_profile(profile);
        let rosters = evidence
            .or(stored.membership.as_ref().map(|m| &m.chain))
            .map(|c| c.versions().iter().map(|r| r.token().to_string()).collect())
            .unwrap_or_default();
        let _ = client.request(CompanionRequest::LeaveRemoved { rosters });
    }

    /// Sets the membership aside after a verified removal: `evidence` must be this Ring's
    /// chain, retiring this Desktop. Shows the notice; the live Ring is dropped.
    fn set_aside_removed(&self, evidence: &RosterChain) -> Result<(), String> {
        let me = self.store.load_profile()?.0.map(|p| p.keys.sign_key());
        if me.is_none_or(|me| evidence.authorized_member(&me).is_some()) {
            return Err("not removed".into());
        }
        let notice = RemovedNotice {
            ring_id: evidence.ring_id().clone(),
            version: evidence.head().version(),
            at: now(),
        };
        let ring = evidence.ring_id().clone();
        self.store.set_aside(AsideReason::Removed(notice), |p| {
            match &p.membership {
                Some(m) if m.chain.ring_id() == &ring => Ok(()),
                _ => Err("membership changed".into()),
            }
        })?;
        self.forget_membership();
        self.emit();
        Ok(())
    }

    /// After this Desktop removed its own unit and the Relay acknowledged it: the Daemon
    /// is told, the membership set aside without a notice, and the live Ring dropped.
    pub(in crate::ring) fn set_aside_self_removed(&self, evidence: &RosterChain) {
        if let Ok((Some(stored), _)) = self.store.load_profile() {
            self.tell_daemon_left(&stored, Some(evidence));
        }
        let ring = evidence.ring_id().clone();
        if let Err(e) = self.store.set_aside(AsideReason::SelfRemoved, |p| {
            match &p.membership {
                Some(m) if m.chain.ring_id() == &ring => Ok(()),
                _ => Err("membership changed".into()),
            }
        }) {
            eprintln!("xshell: cannot set the membership aside after removing this computer: {e}");
        }
        self.forget_membership();
        self.emit();
    }

    /// Starts the background reconciler (once). Only the pairing owner reconciles; it runs
    /// while the membership is [`MembershipState::Finishing`], with backoff from 1 s to
    /// 60 s, and is kicked by unresolved Joins, Creates and approvals.
    pub fn start_reconciler(self: &Arc<Self>) {
        {
            let mut st = self.maintenance.lock();
            if st.reconciler {
                return;
            }
            st.reconciler = true;
            st.kick = true;
        }
        let weak = Arc::downgrade(self);
        let maintenance = self.maintenance.clone();
        let _ = std::thread::Builder::new()
            .name("ring-finish".into())
            .spawn(move || {
                let mut delay = RECONCILE_MIN;
                loop {
                    {
                        let mut st = maintenance.lock();
                        if !st.kick {
                            st = maintenance
                                .cv
                                .wait_timeout(st, delay)
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                        st.kick = false;
                    }
                    let Some(ring) = weak.upgrade() else { return };
                    if ring.is_quitting() {
                        return;
                    }
                    if !ring.pairing_ipc_owner()
                        || ring.membership_state() != Ok(MembershipState::Finishing)
                    {
                        maintenance.lock().offline = false;
                        delay = RECONCILE_MAX;
                        continue;
                    }
                    let result = ring.reconcile_all();
                    let offline = matches!(&result, Err(e) if e.starts_with("offline:"));
                    let changed = {
                        let mut st = maintenance.lock();
                        let changed = st.offline != offline;
                        st.offline = offline;
                        changed
                    };
                    match result {
                        Ok(true) => delay = RECONCILE_MAX,
                        Ok(false) | Err(_) => delay = (delay * 2).clamp(RECONCILE_MIN, RECONCILE_MAX),
                    }
                    if changed || matches!(result, Ok(true)) {
                        ring.emit();
                    }
                }
            });
    }

    /// Run the reconciler now (a Join, Create or approval left something unresolved).
    pub(in crate::ring) fn kick_reconciler(&self) {
        let mut st = self.maintenance.lock();
        st.kick = true;
        self.maintenance.cv.notify_all();
    }

    /// Finishing, and the reconciler cannot reach the Relay right now.
    pub fn finishing_offline(&self) -> bool {
        self.maintenance.lock().offline
            && self.membership_state() == Ok(MembershipState::Finishing)
    }

    /// The Relay refused or closed this Desktop as removed, or a verified chain dropped it:
    /// check the signed history (single flight). Removed: set aside with the notice.
    /// Still a member: the newer chain is adopted and the Connector kicked, even when the
    /// head is unchanged (it stopped on the Relay's word). Unreachable: retried with
    /// backoff from 5 s to 5 min.
    pub(in crate::ring) fn spawn_removal_check(&self) {
        let Some(ring) = self.me.upgrade() else { return };
        {
            let mut st = self.maintenance.lock();
            if st.removal {
                return;
            }
            st.removal = true;
        }
        let maintenance = self.maintenance.clone();
        let spawned = std::thread::Builder::new()
            .name("ring-removal".into())
            .spawn(move || {
                let mut delay = REMOVAL_MIN;
                loop {
                    if ring.is_quitting() {
                        break;
                    }
                    let Some(state) = ring.lock().state.clone() else {
                        break;
                    };
                    match check_removal(
                        &state.chain,
                        state.keys.clone(),
                        None,
                        ring.cfg.timeouts.connect,
                    ) {
                        Ok(RemovalCheck::Removed(evidence)) => {
                            // Verified against the retained chain by the check itself.
                            if let Ok((Some(stored), _)) = ring.store.load_profile() {
                                ring.tell_daemon_left(&stored, Some(&evidence));
                            }
                            if let Err(e) = ring.set_aside_removed(&evidence) {
                                eprintln!("xshell: removal detected but not set aside: {e}");
                            }
                            break;
                        }
                        Ok(RemovalCheck::Member(newer)) => {
                            ring.persist_newer(&newer);
                            if let Some(c) = ring.lock().connector.clone() {
                                c.kick();
                            }
                            break;
                        }
                        Err(e) => {
                            eprintln!("xshell: removal check: {e}; retrying in {delay:?}");
                            std::thread::sleep(delay);
                            delay = (delay * 2).min(REMOVAL_MAX);
                        }
                    }
                }
                maintenance.lock().removal = false;
            });
        if spawned.is_err() {
            self.maintenance.lock().removal = false;
        }
    }
}

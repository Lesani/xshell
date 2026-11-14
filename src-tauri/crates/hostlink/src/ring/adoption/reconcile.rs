//! Restart recovery is observation plus installation only. No proof, proposal, Open,
//! signed upload, deadline renewal, or credential generation is reachable from here.
use super::*;
use crate::ring::store::{OutgoingPhase, ProfileState};
use xshell_protocol::ring::relay::ErrorCode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdoptionRecoveryState {
    /// A restart has no live offer/confirmation capability. These states cannot have
    /// released enrollment proofs; expiry here is not publication uncertainty.
    WaitingBeforeProposal,
    PreparedBeforeProof,
    DeclinedBeforeProof,
    ExpiredBeforeProof,
    CancelledBeforeProof,
    ReconciliationRequired,
    AcceptedInstalling,
    Installed,
    /// Verified accepted history no longer authorizes the recorded local participant(s).
    Terminal,
    /// Proofs may have left, but the Ring never enrolled it (or removed it again) and the
    /// offer expired: ended for good.
    Abandoned,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptionRecoveryStatus {
    pub operation: String,
    pub expires_at: u64,
    pub state: AdoptionRecoveryState,
    pub desktop_installed: bool,
    pub daemon_installed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublicationRecoveryState {
    BeforePublication { expired: bool },
    CancelledBeforePublication,
    ReconciliationRequired,
    Published,
}
/// Approver publication status deliberately has no guest private-store receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationRecoveryStatus {
    pub operation: String,
    pub expires_at: u64,
    pub state: PublicationRecoveryState,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdoptionRecovery {
    pub local: Option<AdoptionRecoveryStatus>,
    pub publication: Option<PublicationRecoveryStatus>,
}
fn tokens(chain: &RosterChain) -> Vec<String> {
    chain.versions().iter().map(|r| r.token().into()).collect()
}
fn terminal(journal: &AdoptionJournal, chain: &RosterChain) -> bool {
    // Upgrade intentionally does not enroll/group the registered, unpaired Daemon.
    let keys = if matches!(
        journal.proposal.membership().map(|m| m.change),
        Ok(RosterChange::Upgrade { .. })
    ) {
        vec![journal.proposal.signed_by]
    } else {
        journal
            .intent
            .target
            .identities()
            .iter()
            .map(|(i, _)| i.sign_key)
            .collect()
    };
    keys.iter()
        .any(|key| chain.authorized_member(key).is_none())
}
fn status(stored: &ProfileState) -> Result<AdoptionRecovery, String> {
    // A new pre-proposal reservation can coexist with the previous closed journal.
    // Never let that historical receipt hide the operation currently waiting for a peer.
    let local = match stored
        .journal
        .as_ref()
        .filter(|j| stored.pending_join.is_none() || !j.phase.permits_new_operation())
    {
        Some(j) => {
            let mut state = match j.phase {
                AdoptionPhase::Prepared => {
                    if now() >= j.intent.expires_at {
                        AdoptionRecoveryState::ExpiredBeforeProof
                    } else {
                        AdoptionRecoveryState::PreparedBeforeProof
                    }
                }
                AdoptionPhase::Cancelled => AdoptionRecoveryState::CancelledBeforeProof,
                AdoptionPhase::DeclinedBeforeProof => AdoptionRecoveryState::DeclinedBeforeProof,
                AdoptionPhase::ExpiredBeforeProof => AdoptionRecoveryState::ExpiredBeforeProof,
                AdoptionPhase::ConsentMayHaveLeft | AdoptionPhase::PublicationUncertain => {
                    AdoptionRecoveryState::ReconciliationRequired
                }
                AdoptionPhase::Accepted => AdoptionRecoveryState::AcceptedInstalling,
                AdoptionPhase::Installed => AdoptionRecoveryState::Installed,
                AdoptionPhase::Abandoned => AdoptionRecoveryState::Abandoned,
            };
            if !j.accepted.is_empty() {
                let mut accepted =
                    RosterChain::from_tokens(&j.accepted).map_err(|e| e.to_string())?;
                if let Some(m) = &stored.membership {
                    accepted.merge_terminal_from(&m.chain);
                    accepted = accepted
                        .extended(m.chain.versions())
                        .map_err(|e| e.to_string())?
                        .0;
                }
                if terminal(j, &accepted) {
                    state = AdoptionRecoveryState::Terminal;
                }
            }
            Some(AdoptionRecoveryStatus {
                operation: j.intent.operation.clone(),
                expires_at: j.intent.expires_at,
                state,
                desktop_installed: j.desktop_installed,
                daemon_installed: j.daemon_installed,
            })
        }
        None => stored
            .pending_join
            .as_ref()
            .map(|p| AdoptionRecoveryStatus {
                operation: p.intent.operation.clone(),
                expires_at: p.intent.expires_at,
                state: if now() >= p.intent.expires_at {
                    AdoptionRecoveryState::ExpiredBeforeProof
                } else {
                    AdoptionRecoveryState::WaitingBeforeProposal
                },
                desktop_installed: false,
                daemon_installed: false,
            }),
    };
    let publication = stored.outgoing.as_ref().map(|j| PublicationRecoveryStatus {
        operation: j.intent.operation.clone(),
        expires_at: j.intent.expires_at,
        state: match j.phase {
            OutgoingPhase::Prepared => PublicationRecoveryState::BeforePublication {
                expired: now() >= j.intent.expires_at,
            },
            OutgoingPhase::Cancelled => PublicationRecoveryState::CancelledBeforePublication,
            OutgoingPhase::PublicationUncertain => PublicationRecoveryState::ReconciliationRequired,
            OutgoingPhase::Published => PublicationRecoveryState::Published,
        },
    });
    Ok(AdoptionRecovery { local, publication })
}
fn pin(j: &AdoptionJournal) -> Result<ChainPin, String> {
    let signed = if j.predecessor.is_empty() {
        xshell_protocol::ring::SignedRoster::parse(
            j.signed_candidate
                .as_ref()
                .ok_or("no known genesis candidate; publication remains uncertain")?,
        )
        .map_err(|e| e.to_string())?
    } else {
        RosterChain::from_tokens(&j.predecessor)
            .map_err(|e| e.to_string())?
            .head()
            .clone()
    };
    Ok(ChainPin {
        version: signed.version(),
        hash: signed.hash(),
    })
}
/// Shared by recovery and accepted installation: replay under existing fences before
/// retaining candidate-only observations learned later in its history.
pub(super) fn merge(
    current: Option<&RosterChain>,
    mut observed: RosterChain,
) -> Result<RosterChain, String> {
    if let Some(current) = current {
        let mut current = current
            .extended(observed.versions())
            .map_err(|e| e.to_string())?
            .0;
        current.merge_terminal_from(&observed);
        observed = current;
    }
    Ok(observed)
}
impl DesktopRing {
    /// Durable state only; no words, leases, network access or guest-private approver receipts.
    pub fn adoption_status(&self) -> Result<AdoptionRecovery, String> {
        let Some(mut stored) = self.store.load_profile()?.0 else {
            return Ok(AdoptionRecovery::default());
        };
        if let (Some(member), Some(current)) =
            (&mut stored.membership, self.adoption_effective_chain()?)
        {
            member.chain = merge(Some(&member.chain), current)?;
        }
        status(&stored)
    }

    fn read_reconciliation_chain(
        &self,
        stored: &ProfileState,
        journal: &AdoptionJournal,
    ) -> Result<RosterChain, String> {
        let pin = pin(journal)?;
        let classify = |e: xshell_protocol::ring::RingError| match &e {
            xshell_protocol::ring::RingError::Relay { code, .. }
                if matches!(
                    code,
                    ErrorCode::NotMember
                        | ErrorCode::Removed
                        | ErrorCode::BadSignature
                        | ErrorCode::NoRoster
                ) =>
            {
                format!("not_member: {e}")
            }
            _ => format!("offline: {e}"),
        };
        let observed = if let Some(owner) = self.adoption_owner_connector() {
            // Even an unavailable/reconnecting owner is not permission to compete with it.
            owner.read_accepted_chain(&pin).map_err(classify)?
        } else {
            RingClient::fetch_chain(
                &journal.proposal.relay_url,
                &journal.proposal.ring_id,
                stored.keys.clone(),
                &pin,
                None,
                self.cfg.timeouts,
            )
            .map_err(classify)?
        };
        let observed = merge(stored.membership.as_ref().map(|m| &m.chain), observed)?;
        let observed = merge(self.adoption_effective_chain()?.as_ref(), observed)?;
        if journal.accepted.is_empty() {
            Ok(observed)
        } else {
            merge(
                Some(&RosterChain::from_tokens(&journal.accepted).map_err(|e| e.to_string())?),
                observed,
            )
        }
    }

    /// Reconcile one recorded operation automatically (never resume its live protocol):
    /// - a reservation or a journal still `Prepared` (no proof left): discarded, and the
    ///   counterpart's operation cancelled;
    /// - a guest journal after proofs may have left: the Ring's verified history decides.
    ///   Enrolled: installed. Enrolled and removed since, or another version at its slot:
    ///   abandoned. Not a member (or the Ring unknown) once the offer expired: abandoned,
    ///   and the counterpart's operation abandoned too. Before expiry, or the Relay
    ///   unreachable: an `Err` (try again later);
    /// - an outgoing (approver) record: `Prepared` ends (nothing left this machine);
    ///   `PublicationUncertain` is `Published` when the candidate is in the chain and
    ///   cancelled when another version holds its slot or it expired unpublished.
    pub fn reconcile_adoption(
        &self,
        client: &CompanionClient,
        operation: &str,
    ) -> Result<AdoptionRecovery, String> {
        self.reconcile_adoption_inner(client, operation)
    }

    /// Every unresolved record, once. `Ok(true)`: nothing is left unresolved. An `Err` whose
    /// text starts with `offline:` means the Relay could not be reached.
    pub fn reconcile_all(&self) -> Result<bool, String> {
        let Some(stored) = self.store.load_profile()?.0 else {
            return Ok(true);
        };
        let ops = super::ui::unresolved(&stored);
        if ops.is_empty() {
            return Ok(true);
        }
        let client = self.local_companion_client();
        let mut last = Ok(());
        for op in ops {
            let result = match &client {
                Ok(client) => self.reconcile_adoption_inner(client, &op).map(|_| ()),
                // No registered counterpart (it was set aside): records that need none.
                Err(e) => self
                    .reconcile_without_counterpart(&op)
                    .map_err(|_| e.clone()),
            };
            if let Err(e) = result {
                last = Err(e);
            }
        }
        last?;
        let left = self
            .store
            .load_profile()?
            .0
            .map(|p| super::ui::unresolved(&p).is_empty())
            .unwrap_or(true);
        Ok(left)
    }

    /// An outgoing record needs no counterpart; a guest record without one cannot be
    /// finished here.
    fn reconcile_without_counterpart(&self, operation: &str) -> Result<(), String> {
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained profile missing")?;
        if stored
            .outgoing
            .as_ref()
            .is_some_and(|o| o.intent.operation == operation)
        {
            let _admission = self.reserve_adoption()?;
            return self.reconcile_outgoing(&stored, operation);
        }
        Err("registered counterpart missing".into())
    }

    fn reconcile_adoption_inner(
        &self,
        client: &CompanionClient,
        operation: &str,
    ) -> Result<AdoptionRecovery, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("other_window: another window owns reconciliation".into());
        }
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained profile missing")?;
        // Counterpart requests whose refusal is an answer (the operation is unknown there,
        // already ended, or it was re-registered) proceed; transport failures retry.
        let tell = |request: CompanionRequest| match client.request(request) {
            Err(e) if e.starts_with("protected counterpart") => Err(e),
            _ => Ok(()),
        };
        if let Some(j) = stored
            .journal
            .as_ref()
            .filter(|j| j.intent.operation == operation)
        {
            if j.phase.permits_new_operation() {
                return status(&stored);
            }
            if j.phase == AdoptionPhase::Prepared {
                tell(CompanionRequest::Cancel {
                    operation: operation.into(),
                })?;
                self.store.transact_profile(|stored, _| {
                    let mut stored = stored.ok_or("retained profile missing")?;
                    if let Some(j) = stored
                        .journal
                        .as_mut()
                        .filter(|j| j.intent.operation == operation)
                    {
                        j.phase = if now() >= j.intent.expires_at {
                            AdoptionPhase::ExpiredBeforeProof
                        } else {
                            AdoptionPhase::Cancelled
                        };
                    }
                    stored.pending_join = None;
                    Ok((Some(stored), ()))
                })?;
                return self.adoption_status();
            }
            let abandon = |why: &str| -> Result<AdoptionRecovery, String> {
                tell(CompanionRequest::AbandonExpired {
                    operation: operation.into(),
                })?;
                self.journal_update(operation, |j| j.phase = AdoptionPhase::Abandoned)?;
                eprintln!("xshell: Join/Create {operation} abandoned: {why}");
                self.adoption_status()
            };
            // Local accepted history can prove terminal disposition even if Relay auth is
            // now refused. This protected snapshot is evidence, never an installation receipt.
            let snapshot = match client.request(CompanionRequest::MembershipSnapshot) {
                Ok(CompanionResponse::MembershipSnapshot { rosters }) if !rosters.is_empty() => {
                    RosterChain::from_tokens(&rosters).ok()
                }
                Err(e) if e.starts_with("protected counterpart") => return Err(e),
                _ => None,
            };
            let mut evidence = if j.accepted.is_empty() {
                None
            } else {
                Some(RosterChain::from_tokens(&j.accepted).map_err(|e| e.to_string())?)
            };
            if let Some(m) = &stored.membership {
                evidence = Some(merge(evidence.as_ref(), m.chain.clone())?);
            }
            if let Some(snapshot) = snapshot {
                // A snapshot of another Ring (or a fork) is no evidence for this one.
                if let Ok(merged) = merge(evidence.as_ref(), snapshot) {
                    evidence = Some(merged);
                }
            }
            if let Some(chain) = evidence
                .as_ref()
                .filter(|c| c.get(j.proposal.version).is_some())
            {
                if j.verify_accepted(chain).is_err() {
                    return abandon("another version holds its slot");
                }
                if terminal(j, chain) {
                    self.record_reconciled_acceptance(operation, chain)?;
                    return abandon("enrolled, then removed");
                }
            }
            let accepted = match self.read_reconciliation_chain(&stored, j) {
                Ok(chain) => merge(evidence.as_ref(), chain)?,
                // Enrolled (the accepted chain is known here) but refused now: read the
                // signed history as a former member; a verified removal ends it at once.
                Err(e)
                    if e.starts_with("not_member:")
                        && evidence
                            .as_ref()
                            .is_some_and(|c| c.get(j.proposal.version).is_some()) =>
                {
                    let known = evidence.as_ref().expect("checked");
                    match xshell_protocol::ring::relay::check_removal(
                        known,
                        stored.keys.clone(),
                        None,
                        self.cfg.timeouts.connect,
                    ) {
                        Ok(xshell_protocol::ring::relay::RemovalCheck::Removed(chain)) => {
                            self.record_reconciled_acceptance(operation, &chain)?;
                            return abandon("enrolled, then removed");
                        }
                        Ok(xshell_protocol::ring::relay::RemovalCheck::Member(chain)) => chain,
                        Err(e) => return Err(format!("offline: {e}")),
                    }
                }
                Err(e) if e.starts_with("not_member:") && now() >= j.intent.expires_at => {
                    return abandon(&e);
                }
                Err(e) if e.starts_with("not_member:") => {
                    return Err(format!("finishing: not enrolled yet ({e})"));
                }
                Err(e) => return Err(e),
            };
            if accepted.get(j.proposal.version).is_none() {
                if now() >= j.intent.expires_at {
                    return abandon("not enrolled when the offer expired");
                }
                return Err("finishing: not enrolled yet".into());
            }
            if j.verify_accepted(&accepted).is_err() {
                return abandon("another version holds its slot");
            }
            self.record_reconciled_acceptance(operation, &accepted)?;
            if terminal(j, &accepted) {
                return abandon("enrolled, then removed");
            }
            let profile = stored
                .companion
                .as_ref()
                .ok_or("registered counterpart missing")?;
            if profile.desktop.sign_key != stored.keys.sign_key()
                || profile.target() != j.intent.target
            {
                return abandon("the registered counterpart changed");
            }
            self.install_joined_computer(client, operation, profile, &accepted)?;
        } else if stored
            .outgoing
            .as_ref()
            .is_some_and(|j| j.intent.operation == operation)
        {
            self.reconcile_outgoing(&stored, operation)?;
        } else if stored
            .pending_join
            .as_ref()
            .is_some_and(|p| p.intent.operation == operation)
        {
            // No proposal arrived before the restart: nothing left this machine.
            tell(CompanionRequest::Cancel {
                operation: operation.into(),
            })?;
            self.store.transact_profile(|stored, _| {
                let mut stored = stored.ok_or("retained profile missing")?;
                if stored
                    .pending_join
                    .as_ref()
                    .is_some_and(|p| p.intent.operation == operation)
                {
                    stored.pending_join = None;
                }
                Ok((Some(stored), ()))
            })?;
        } else {
            return Err("unknown adoption operation".into());
        }
        self.adoption_status()
    }

    /// Under the admission: an approver's outgoing record.
    fn reconcile_outgoing(&self, stored: &ProfileState, operation: &str) -> Result<(), String> {
        let outgoing = stored
            .outgoing
            .as_ref()
            .filter(|j| j.intent.operation == operation)
            .ok_or("publication changed")?;
        let cancel = || {
            self.outgoing_update(operation, |o| o.phase = OutgoingPhase::Cancelled)
        };
        match outgoing.phase {
            OutgoingPhase::Published | OutgoingPhase::Cancelled => return Ok(()),
            // Nothing left this machine; the proposal grant lapsed with its connection.
            OutgoingPhase::Prepared => return cancel(),
            OutgoingPhase::PublicationUncertain => {}
        }
        let record = outgoing.validation_record()?;
        let accepted = match self.read_reconciliation_chain(stored, &record) {
            Ok(chain) => chain,
            Err(e) if e.starts_with("not_member:") => {
                // This approver was removed meanwhile: its candidate can never be accepted.
                return cancel();
            }
            Err(e) => return Err(e),
        };
        if accepted.get(record.proposal.version).is_none() {
            if now() >= record.intent.expires_at {
                return cancel();
            }
            return Err("finishing: publication not observed yet".into());
        }
        if record.verify_accepted(&accepted).is_err() {
            return cancel();
        }
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            let outgoing = stored
                .outgoing
                .as_mut()
                .filter(|j| j.intent.operation == operation)
                .ok_or("publication changed")?;
            outgoing.accepted = tokens(&accepted);
            outgoing.phase = OutgoingPhase::Published;
            let membership = stored
                .membership
                .as_mut()
                .ok_or("approver membership missing")?;
            membership.chain = merge(Some(&membership.chain), accepted)?;
            membership.retarget_farewells();
            Ok((Some(stored), ()))
        })?;
        self.reload();
        Ok(())
    }

    fn record_reconciled_acceptance(
        &self,
        operation: &str,
        accepted: &RosterChain,
    ) -> Result<(), String> {
        self.journal_update(operation, |j| {
            if j.phase != AdoptionPhase::Installed {
                j.phase = AdoptionPhase::Accepted;
            }
            j.accepted = tokens(accepted);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{DesktopRingConfig, RingObserver, RingView};
    use xshell_protocol::ring::activity::{policy_hash, ActivityEvidence, UnitActivity};
    use xshell_protocol::ring::{b64, Role};
    struct Quiet;
    impl RingObserver for Quiet {
        fn changed(&self, _: &RingView) {}
    }

    use xshell_protocol as protocol;
    mod chronology_fixture {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../protocol/tests/fixtures/recovery_chronology.rs"
        ));
    }

    #[test]
    fn recovery_merge_replays_cleanup_after_the_authorized_enroll() {
        let f = chronology_fixture::recovery_chronology("ws://127.0.0.1:9");
        assert!(f.legacy.head().version() < f.known.head().version());
        let accepted = merge(Some(&f.known), f.accepted.clone()).unwrap();
        assert_eq!(accepted.head(), f.accepted.head());
        assert!(accepted.terminal_enrollments().contains(&f.terminal_id));
        assert!(accepted.authorized_member(&f.b.sign_key()).is_none());
        assert!(accepted.authorized_member(&f.reader.sign_key()).is_some());
    }

    #[test]
    fn recovery_merge_keeps_terminal_constraints_known_before_enroll() {
        let f = chronology_fixture::recovery_chronology("ws://127.0.0.1:9");
        let mut known = f.known.clone();
        known.observe_activity(&f.evidence).unwrap();
        let before = known.clone();
        assert!(merge(Some(&known), f.accepted)
            .unwrap_err()
            .contains("terminal-expired signer"));
        assert_eq!(
            known, before,
            "rejected recovery/install cannot mutate its base"
        );
        assert!(known.terminal_enrollments().contains(&f.terminal_id));
    }

    #[test]
    fn desktop_status_uses_owner_same_head_terminal_evidence_not_live_state_cache() {
        // Cryptographic owner-path fixture, not public-workerd membership setup.
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("desktop");
        let store = crate::ring::store::Store::new(dir.clone());
        let keys = store.retained_keys().unwrap();
        let daemon = DeviceKeys::generate().unwrap();
        let source = DeviceKeys::generate().unwrap();
        let identity =
            |k: &DeviceKeys, name: &str| xshell_protocol::ring::pairing::adoption::PublicIdentity {
                name: name.into(),
                sign_key: k.sign_key(),
                noise_key: k.noise_key(),
            };
        let intent = AdoptionIntent {
            operation: b64::encode(&[1; 32]),
            expires_at: 1600,
            target: xshell_protocol::ring::pairing::adoption::JoinTarget::Computer {
                desktop: identity(&keys, "desktop"),
                daemon: identity(&daemon, "daemon"),
            },
        };
        let mut proposal = xshell_protocol::ring::Roster {
            v: 2,
            ring_id: RingId::derive(&keys.sign_key()),
            version: 1,
            prev: None,
            relay_url: "ws://127.0.0.1:9".into(),
            signed_by: keys.sign_key(),
            issued_at: 1000,
            members: vec![
                Member::new(
                    "desktop",
                    Role::Desktop,
                    keys.sign_key(),
                    keys.noise_key(),
                    1000,
                ),
                Member::new(
                    "daemon",
                    Role::Daemon,
                    daemon.sign_key(),
                    daemon.noise_key(),
                    1000,
                ),
            ],
            extra: Default::default(),
        };
        let unit = Enrollment {
            id: intent.operation.clone(),
            kind: EnrollmentKind::Computer,
            members: vec![keys.sign_key(), daemon.sign_key()],
            enrolled_at: 1000,
        };
        let policy = RingPolicy {
            inactivity_seconds: Some(30),
            activity_key: source.sign_key(),
        };
        proposal.set_membership(&Membership {
            enrollments: vec![unit.clone()],
            retired: vec![],
            policy: policy.clone(),
            change: RosterChange::Genesis { proofs: vec![] },
        });
        let context = local_consent_context(&proposal).unwrap();
        let message = consent_message(&proposal, 1600, &context).unwrap();
        let proofs = [keys.as_ref(), &daemon]
            .iter()
            .map(|k| ConsentProof {
                key: k.sign_key(),
                expires_at: 1600,
                context: context.clone(),
                signature: Signature::from_bytes(k.sign(&message).unwrap()),
            })
            .collect();
        let mut completed = proposal.clone();
        let mut m = completed.membership().unwrap();
        m.change = RosterChange::Genesis { proofs };
        completed.set_membership(&m);
        let signed = completed.sign(keys.as_ref()).unwrap();
        let chain = RosterChain::from_chain(vec![signed.clone()]).unwrap();
        store
            .transact_profile(|stored, _| {
                let mut stored = stored.unwrap();
                stored.journal = Some(AdoptionJournal {
                    intent,
                    predecessor: vec![],
                    proposal,
                    context,
                    transcript: ConsentTranscript::Local,
                    phase: AdoptionPhase::Installed,
                    signed_candidate: Some(signed.token().into()),
                    accepted: vec![signed.token().into()],
                    desktop_installed: true,
                    daemon_installed: true,
                });
                stored.membership = Some(RingState {
                    keys: keys.clone(),
                    chain: chain.clone(),
                    local_member: Some(daemon.sign_key()),
                    farewells: vec![],
                    held_relays: Default::default(),
                    host_members: Default::default(),
                });
                Ok((Some(stored), ()))
            })
            .unwrap();
        let ring = DesktopRing::open(
            DesktopRingConfig::new(dir, "desktop".into()),
            Arc::new(Quiet),
        );
        assert!(ring.pairing_ipc_owner());
        assert_eq!(
            ring.adoption_status().unwrap().local.unwrap().state,
            AdoptionRecoveryState::Installed
        );
        let token = ActivityEvidence {
            ring_id: chain.ring_id().clone(),
            relay_url: chain.head().roster().relay_url.clone(),
            policy_head_hash: signed.hash(),
            policy_hash: policy_hash(&policy),
            observed_at: 2000,
            units: vec![UnitActivity {
                enrollment_id: unit.id,
                member_keys: unit.members,
                online: false,
                last_active: 1000,
                terminal_expired: true,
            }],
        }
        .sign(&source)
        .unwrap();
        let owner = ring.adoption_owner_connector().unwrap();
        owner
            .test_with_stable_attempt(|| {
                owner.test_observe_activity(
                    &ChainPin {
                        version: 1,
                        hash: signed.hash(),
                    },
                    keys.sign_key(),
                    &token,
                    || {},
                )
            })
            .unwrap();
        assert_eq!(owner.chain().head(), &signed);
        assert!(owner.chain().authorized_member(&keys.sign_key()).is_none());
        assert!(
            ring.chain()
                .unwrap()
                .authorized_member(&keys.sign_key())
                .is_some(),
            "same-head Live.state cache remains deliberately unchanged"
        );
        let status = ring.adoption_status().unwrap();
        ring.quit();
        assert_eq!(status.local.unwrap().state, AdoptionRecoveryState::Terminal);
    }
}

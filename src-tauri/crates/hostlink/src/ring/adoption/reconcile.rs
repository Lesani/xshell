//! Restart recovery is observation plus installation only. No proof, proposal, Open,
//! signed upload, deadline renewal, or credential generation is reachable from here.
use super::*;
use crate::ring::store::{OutgoingPhase, ProfileState};

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
        let observed = if let Some(owner) = self.adoption_owner_connector() {
            // Even an unavailable/reconnecting owner is not permission to compete with it.
            owner.read_accepted_chain(&pin).map_err(|e| e.to_string())?
        } else {
            RingClient::fetch_chain(
                &journal.proposal.relay_url,
                &journal.proposal.ring_id,
                stored.keys.clone(),
                &pin,
                None,
                self.cfg.timeouts,
            )
            .map_err(|e| e.to_string())?
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

    /// Reconcile one recorded operation, never resume its live protocol. Fetch/auth/outage
    /// failure leaves the durable obligation blocked. There is no inference of absence.
    pub fn reconcile_adoption(
        &self,
        client: &CompanionClient,
        operation: &str,
    ) -> Result<AdoptionRecovery, String> {
        self.reconcile_adoption_inner(client, operation)
            .map_err(|e| {
                if e.starts_with("reconciliation_required:") {
                    e
                } else {
                    format!("reconciliation_required: {e}")
                }
            })
    }
    fn reconcile_adoption_inner(
        &self,
        client: &CompanionClient,
        operation: &str,
    ) -> Result<AdoptionRecovery, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns reconciliation".into());
        }
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained profile missing")?;
        if let Some(j) = stored
            .journal
            .as_ref()
            .filter(|j| j.intent.operation == operation)
        {
            if j.phase == AdoptionPhase::Prepared {
                // Pre-proof: retire it through the fenced counterpart cancellation.
                self.recover_pre_proof_join_admitted(client)?;
                return self.adoption_status();
            }
            if j.phase.before_proof_terminal() {
                return status(&stored);
            }
            let profile = stored
                .companion
                .as_ref()
                .ok_or("registered counterpart missing")?;
            if profile.desktop.sign_key != stored.keys.sign_key()
                || profile.target() != j.intent.target
            {
                return Err("reconciliation retained counterpart mismatch".into());
            }
            // Local accepted history can prove terminal disposition even if Relay auth is
            // now refused. This protected snapshot is evidence, never an installation receipt.
            let snapshot = match client.request(CompanionRequest::MembershipSnapshot)? {
                CompanionResponse::MembershipSnapshot { rosters } if rosters.is_empty() => None,
                CompanionResponse::MembershipSnapshot { rosters } => {
                    Some(RosterChain::from_tokens(&rosters).map_err(|e| e.to_string())?)
                }
                _ => return Err("counterpart membership snapshot missing".into()),
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
                evidence = Some(merge(evidence.as_ref(), snapshot)?);
            }
            if let Some(chain) = evidence
                .as_ref()
                .filter(|c| c.get(j.proposal.version).is_some())
            {
                j.verify_accepted(chain)?;
                if terminal(j, chain) {
                    self.record_reconciled_acceptance(operation, chain)?;
                    let mut result = self.adoption_status()?;
                    if let Some(local) = &mut result.local {
                        local.state = AdoptionRecoveryState::Terminal;
                    }
                    return Ok(result);
                }
            }
            let accepted = self
                .read_reconciliation_chain(&stored, j)
                .map_err(|e| format!("reconciliation_required: {e}"))?;
            let accepted = merge(evidence.as_ref(), accepted)?;
            j.verify_accepted(&accepted)?;
            self.record_reconciled_acceptance(operation, &accepted)?;
            if terminal(j, &accepted) {
                let mut result = self.adoption_status()?;
                if let Some(local) = &mut result.local {
                    local.state = AdoptionRecoveryState::Terminal;
                }
                return Ok(result);
            }
            self.install_joined_computer(client, operation, profile, &accepted)
                .map_err(|e| format!("reconciliation_required: {e}"))?;
        } else if let Some(outgoing) = stored
            .outgoing
            .as_ref()
            .filter(|j| j.intent.operation == operation)
        {
            if matches!(
                outgoing.phase,
                OutgoingPhase::Prepared | OutgoingPhase::Cancelled
            ) {
                return status(&stored);
            }
            let record = outgoing.validation_record()?;
            let accepted = self
                .read_reconciliation_chain(&stored, &record)
                .map_err(|e| format!("reconciliation_required: {e}"))?;
            record.verify_accepted(&accepted)?;
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
        } else if !stored
            .pending_join
            .as_ref()
            .is_some_and(|p| p.intent.operation == operation)
        {
            return Err("unknown adoption operation".into());
        } else {
            // A pending-only reservation never reached a proposal: fenced pre-proof retire.
            self.recover_pre_proof_join_admitted(client)?;
        }
        self.adoption_status()
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

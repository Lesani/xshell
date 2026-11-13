//! Explicit retained membership migration. Upgrade leaves every participant singleton;
//! only a separately confirmed, protected local completion may group counterparts.
use super::super::store::{OutgoingAdoption, OutgoingPhase, ProfileState};
use super::*;
use xshell_protocol::ring::relay::connector::ProposalConnection;
use xshell_protocol::ring::Role;

fn tokens(chain: &RosterChain) -> Vec<String> {
    chain.versions().iter().map(|r| r.token().into()).collect()
}
fn idle(stored: &ProfileState) -> Result<(), String> {
    if stored.pending_join.is_some()
        || stored
            .journal
            .as_ref()
            .is_some_and(|j| !j.phase.permits_new_operation())
        || stored.outgoing.as_ref().is_some_and(|j| {
            !matches!(j.phase, OutgoingPhase::Published | OutgoingPhase::Cancelled)
        })
    {
        return Err("reconciliation_required: retained adoption work is unresolved".into());
    }
    Ok(())
}
impl DesktopRing {
    fn retained_live(
        &self,
        lease: &CreateConfirmationLease,
        lane: &ProposalConnection,
    ) -> Result<(), String> {
        self.create_live(lease)?;
        lane.check_current().map_err(|e| e.to_string())?;
        let stored = self
            .store
            .load()?
            .0
            .ok_or("retained Desktop membership missing")?;
        if stored.chain.head() != lane.predecessor()
            || stored
                .chain
                .authorized_member(&stored.keys.sign_key())
                .is_none()
        {
            return Err("retained predecessor changed; fresh confirmation required".into());
        }
        // Store admission can block while the captured socket learns a successor.
        // Recheck that live head too, not just the still-retained durable predecessor.
        lane.check_current().map_err(|e| e.to_string())?;
        self.create_live(lease)
    }
    /// Explicit format Upgrade, never called by open, sync, registration or Host setup.
    /// Unproven participants remain independent migration/singleton units.
    pub fn upgrade_retained_ring(
        &self,
        client: &CompanionClient,
        confirm: impl FnOnce(
            &ConsentPreview,
            &CreateConfirmationLease,
        ) -> Result<CreateDecision, String>,
    ) -> Result<AdoptionOutcome, String> {
        let _admission = self.reserve_adoption()?;
        let ttl = self.cfg.pair_ttl.min(Duration::from_secs(600));
        let until = Instant::now() + ttl;
        let expires_at = now() + ttl.as_secs();
        if !self.pairing_ipc_owner() {
            return Err("another process owns Upgrade".into());
        }
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("Upgrade requires retained membership")?;
        idle(&stored)?;
        let (keys, profile) = self.register_companion_inner(client)?;
        let (chain, lane) = self.proposal_snapshot()?;
        if chain.head().roster().v != 1
            || stored
                .membership
                .as_ref()
                .is_none_or(|m| m.chain.head() != chain.head())
        {
            return Err("Upgrade requires exact retained v1 membership".into());
        }
        let member = chain
            .authorized_member(&keys.sign_key())
            .ok_or("retained Desktop is revoked")?;
        if member.role != Role::Desktop || member.noise_key != keys.noise_key() {
            return Err("Upgrade requires retained Desktop authority".into());
        }
        let operation = nonce()?;
        let intent = AdoptionIntent {
            operation: operation.clone(),
            expires_at,
            target: profile.target(),
        };
        let mut proposal = chain.head().roster().clone();
        proposal.v = 2;
        proposal.version += 1;
        proposal.prev = Some(chain.head().hash());
        proposal.signed_by = keys.sign_key();
        proposal.issued_at = now();
        let activity_key = RingClient::activity_key(
            &proposal.relay_url,
            &proposal.ring_id,
            None,
            self.cfg.timeouts.connect,
        )
        .map_err(|e| e.to_string())?;
        proposal.set_membership(&Membership {
            enrollments: proposal
                .members
                .iter()
                .map(|member| {
                    Ok(Enrollment {
                        id: nonce()?,
                        kind: match member.role {
                            Role::Desktop => EnrollmentKind::LegacyDesktop,
                            Role::Daemon => EnrollmentKind::Daemon,
                            Role::Mobile => EnrollmentKind::Mobile,
                        },
                        members: vec![member.sign_key],
                        enrolled_at: proposal.issued_at,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: Some(DEFAULT_INACTIVITY_SECONDS),
                activity_key,
            },
            change: RosterChange::Upgrade { proofs: vec![] },
        });
        verify_local_proposal(Some(chain.head()), &proposal).map_err(|e| e.to_string())?;
        lane.freeze_completion(&proposal)
            .map_err(|e| e.to_string())?;
        let context = local_consent_context(&proposal).map_err(|e| e.to_string())?;
        let hash = consent_proposal_hash(&proposal).map_err(|e| e.to_string())?;
        let lease = CreateConfirmationLease {
            operation: operation.clone(),
            proposal_hash: hash.clone(),
            until,
            expires_at,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.record_driver_cancel(&lease.cancelled)?;
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            idle(&stored)?;
            if stored
                .membership
                .as_ref()
                .is_none_or(|m| m.chain.head() != chain.head())
            {
                return Err("retained predecessor changed".into());
            }
            stored.outgoing = Some(OutgoingAdoption {
                intent,
                predecessor: tokens(&chain),
                proposal: proposal.clone(),
                final_noise_hash: None,
                phase: OutgoingPhase::Prepared,
                signed_candidate: None,
                accepted: vec![],
            });
            Ok((Some(stored), ()))
        })?;
        let result = (|| {
            self.retained_live(&lease, &lane)?;
            let preview = ConsentPreview {
                operation: operation.clone(),
                proposal_hash: hash,
                ring_id: proposal.ring_id.clone(),
                relay_url: proposal.relay_url.clone(),
                profile,
                context,
                proposal: proposal.clone(),
                expires_at,
            };
            let decision = confirm(&preview, &lease)?;
            let CreateDecision::Confirm(confirmation) = decision else {
                return Ok(AdoptionOutcome::Declined {
                    operation: operation.clone(),
                });
            };
            lease.verify(&confirmation)?;
            self.retained_live(&lease, &lane)?;
            let signed = proposal.sign(&*keys).map_err(|e| e.to_string())?;
            xshell_protocol::ring::verify_successor(chain.head(), &signed)
                .map_err(|e| e.to_string())?;
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::UpgradeBeforeUncertainWrite,
            );
            self.outgoing_update(&operation, |j| {
                j.signed_candidate = Some(signed.token().into());
                j.phase = OutgoingPhase::PublicationUncertain;
            })?;
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::UpgradeBeforePublish,
            );
            self.retained_live(&lease, &lane)?;
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::RetainedPublicationAttempt,
            );
            self.retained_live(&lease, &lane)?;
            lane.publish_once(&signed).map_err(|e| e.to_string())?;
            let accepted = lane.read_accepted(&signed).map_err(|e| e.to_string())?;
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::AcceptedBeforeJournal,
            );
            self.store.transact_profile(|stored, _| {
                let mut stored = stored.ok_or("retained profile missing")?;
                let outgoing = stored
                    .outgoing
                    .as_mut()
                    .filter(|j| j.intent.operation == operation)
                    .ok_or("Upgrade operation changed")?;
                outgoing.accepted = tokens(&accepted);
                outgoing.phase = OutgoingPhase::Published;
                let membership = stored
                    .membership
                    .as_mut()
                    .ok_or("retained membership missing")?;
                membership.chain = membership
                    .chain
                    .extended(accepted.versions())
                    .map_err(|e| e.to_string())?
                    .0;
                membership.retarget_farewells();
                Ok((Some(stored), ()))
            })?;
            self.reload();
            Ok(AdoptionOutcome::Installed {
                operation: operation.clone(),
                chain: accepted,
            })
        })();
        let phase = self
            .store
            .load_profile()
            .map_err(|e| format!("reconciliation_required: {e}"))?
            .0
            .and_then(|p| p.outgoing)
            .map(|j| j.phase);
        if phase == Some(OutgoingPhase::Prepared) {
            self.outgoing_update(&operation, |j| j.phase = OutgoingPhase::Cancelled)?;
        }
        result.map_err(|e| {
            if phase == Some(OutgoingPhase::PublicationUncertain) {
                format!("reconciliation_required: {e}")
            } else {
                e
            }
        })
    }

    /// Complete this existing Desktop's own registered local counterpart. This is not
    /// remote Host administration and cannot replace either store's retained trust.
    pub fn finish_retained_computer(
        &self,
        client: &CompanionClient,
        confirm: impl FnOnce(
            &ConsentPreview,
            &CreateConfirmationLease,
        ) -> Result<CreateDecision, String>,
    ) -> Result<AdoptionOutcome, String> {
        let _admission = self.reserve_adoption()?;
        let ttl = self.cfg.pair_ttl.min(Duration::from_secs(600));
        let until = Instant::now() + ttl;
        let expires_at = now() + ttl.as_secs();
        if !self.pairing_ipc_owner() {
            return Err("another process owns retained completion".into());
        }
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained Desktop profile missing")?;
        idle(&stored)?;
        let (keys, profile) = self.register_companion_inner(client)?;
        let (chain, lane) = self.proposal_snapshot()?;
        if stored
            .membership
            .as_ref()
            .is_none_or(|m| m.chain.head() != chain.head())
        {
            return Err("retained predecessor changed".into());
        }
        let operation = nonce()?;
        let intent = AdoptionIntent {
            operation: operation.clone(),
            expires_at,
            target: profile.target(),
        };
        let old = chain
            .head()
            .roster()
            .membership()
            .map_err(|e| e.to_string())?;
        let own = old
            .unit_for(&keys.sign_key())
            .ok_or("retained Desktop singleton missing")?;
        if own.kind != EnrollmentKind::LegacyDesktop
            || chain.authorized_member(&keys.sign_key()).is_none()
        {
            return Err("retained migration Desktop singleton required".into());
        }
        if chain.head().member(&profile.daemon.sign_key).is_none()
            && chain
                .versions()
                .iter()
                .any(|roster| roster.member(&profile.daemon.sign_key).is_some())
        {
            return Err("retained Daemon was removed; completion cannot re-enroll it".into());
        }
        let mut replaces = vec![own.id.clone()];
        if let Some(unit) = old.unit_for(&profile.daemon.sign_key) {
            if unit.kind != EnrollmentKind::Daemon
                || chain
                    .authorized_member(&profile.daemon.sign_key)
                    .is_none_or(|member| {
                        member.role != Role::Daemon || member.noise_key != profile.daemon.noise_key
                    })
            {
                return Err("counterpart is not an authorized Daemon singleton".into());
            }
            replaces.push(unit.id.clone());
        }
        let mut proposal = chain.head().roster().clone();
        proposal.version += 1;
        proposal.prev = Some(chain.head().hash());
        proposal.issued_at = now();
        proposal.signed_by = keys.sign_key();
        if proposal.member(&profile.daemon.sign_key).is_none() {
            proposal.members.push(Member::new(
                &profile.daemon.name,
                Role::Daemon,
                profile.daemon.sign_key,
                profile.daemon.noise_key,
                proposal.issued_at,
            ));
        }
        let mut membership = old;
        membership
            .enrollments
            .retain(|unit| !replaces.contains(&unit.id));
        membership.retired.extend(replaces.clone());
        membership.enrollments.push(Enrollment {
            id: operation.clone(),
            kind: EnrollmentKind::Computer,
            members: vec![profile.desktop.sign_key, profile.daemon.sign_key],
            enrolled_at: proposal.issued_at,
        });
        membership.change = RosterChange::CompleteComputer {
            replaces,
            enrollment_id: operation.clone(),
            proofs: vec![],
        };
        proposal.set_membership(&membership);
        verify_local_proposal(Some(chain.head()), &proposal).map_err(|e| e.to_string())?;
        lane.freeze_completion(&proposal)
            .map_err(|e| e.to_string())?;
        let context = local_consent_context(&proposal).map_err(|e| e.to_string())?;
        let hash = consent_proposal_hash(&proposal).map_err(|e| e.to_string())?;
        let lease = CreateConfirmationLease {
            operation: operation.clone(),
            proposal_hash: hash.clone(),
            until,
            expires_at,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.record_driver_cancel(&lease.cancelled)?;
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            idle(&stored)?;
            if stored
                .membership
                .as_ref()
                .is_none_or(|m| m.chain.head() != chain.head())
            {
                return Err("retained predecessor changed".into());
            }
            stored.journal = Some(AdoptionJournal {
                intent: intent.clone(),
                predecessor: tokens(&chain),
                proposal: proposal.clone(),
                context: context.clone(),
                transcript: ConsentTranscript::Local,
                phase: AdoptionPhase::Prepared,
                signed_candidate: None,
                accepted: vec![],
                desktop_installed: false,
                daemon_installed: false,
            });
            Ok((Some(stored), ()))
        })?;
        let result = (|| {
            match client.request(CompanionRequest::RetainedCompletionStart {
                intent: intent.clone(),
                predecessor: tokens(&chain),
            })? {
                CompanionResponse::Completion { pending: Some(p) } if p.intent == intent => {}
                _ => return Err("counterpart retained preparation receipt missing".into()),
            }
            client.request(CompanionRequest::Prepare {
                proposal: Box::new(CompanionProposal {
                    operation: operation.clone(),
                    predecessor: tokens(&chain),
                    proposal: proposal.clone(),
                    transcript: ConsentTranscript::Local,
                }),
            })?;
            self.retained_live(&lease, &lane)?;
            let preview = ConsentPreview {
                operation: operation.clone(),
                proposal_hash: hash,
                ring_id: proposal.ring_id.clone(),
                relay_url: proposal.relay_url.clone(),
                profile: profile.clone(),
                context: context.clone(),
                proposal: proposal.clone(),
                expires_at,
            };
            let decision = confirm(&preview, &lease)?;
            let CreateDecision::Confirm(confirmation) = decision else {
                return Ok(AdoptionOutcome::Declined {
                    operation: operation.clone(),
                });
            };
            lease.verify(&confirmation)?;
            self.retained_live(&lease, &lane)?;
            super::completion::check_prepared(client, &operation)?;
            self.retained_live(&lease, &lane)?;
            self.journal_update(&operation, |j| j.phase = AdoptionPhase::ConsentMayHaveLeft)?;
            super::completion::check_prepared(client, &operation)?;
            self.retained_live(&lease, &lane)?;
            let message =
                consent_message(&proposal, expires_at, &context).map_err(|e| e.to_string())?;
            let desktop_proof = ConsentProof {
                key: keys.sign_key(),
                expires_at,
                context,
                signature: Signature::from_bytes(keys.sign(&message).map_err(|e| e.to_string())?),
            };
            let daemon_proof = match client.request(CompanionRequest::Consent {
                operation: operation.clone(),
                desktop_proof: desktop_proof.clone(),
            })? {
                CompanionResponse::Consent { proof } => proof,
                _ => return Err("counterpart consent receipt missing".into()),
            };
            let mut membership = proposal.membership().map_err(|e| e.to_string())?;
            let RosterChange::CompleteComputer { proofs, .. } = &mut membership.change else {
                unreachable!()
            };
            *proofs = vec![desktop_proof, daemon_proof];
            proposal.set_membership(&membership);
            let signed = proposal.sign(&*keys).map_err(|e| e.to_string())?;
            xshell_protocol::ring::verify_successor(chain.head(), &signed)
                .map_err(|e| e.to_string())?;
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::CompletionBeforeUncertainWrite,
            );
            self.journal_update(&operation, |j| {
                j.signed_candidate = Some(signed.token().into());
                j.phase = AdoptionPhase::PublicationUncertain;
            })?;
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::CompletionBeforePublish,
            );
            self.retained_live(&lease, &lane)?;
            self.adoption_test_point(
                super::super::desktop::AdoptionTestPoint::RetainedPublicationAttempt,
            );
            self.retained_live(&lease, &lane)?;
            lane.publish_once(&signed).map_err(|e| e.to_string())?;
            let accepted = lane.read_accepted(&signed).map_err(|e| e.to_string())?;
            self.install_joined_computer(client, &operation, &profile, &accepted)?;
            Ok(AdoptionOutcome::Installed {
                operation: operation.clone(),
                chain: accepted,
            })
        })();
        let phase = self
            .store
            .load_profile()
            .map_err(|e| format!("reconciliation_required: {e}"))?
            .0
            .and_then(|p| p.journal)
            .map(|j| j.phase);
        if phase == Some(AdoptionPhase::Prepared)
            && client
                .request(CompanionRequest::Cancel {
                    operation: operation.clone(),
                })
                .is_ok()
        {
            self.journal_update(&operation, |j| {
                j.phase = if matches!(result, Ok(AdoptionOutcome::Declined { .. })) {
                    AdoptionPhase::DeclinedBeforeProof
                } else if now() >= j.intent.expires_at {
                    AdoptionPhase::ExpiredBeforeProof
                } else {
                    AdoptionPhase::Cancelled
                }
            })?;
        }
        result.map_err(|e| {
            if phase != Some(AdoptionPhase::Prepared) {
                format!("reconciliation_required: {e}")
            } else {
                e
            }
        })
    }
}

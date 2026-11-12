//! Daemon-only v2 completion over the existing authorized HostAccess. The enrolled
//! approver freezes/publishes; the unpaired Desktop alone owns local confirmation.
use super::*;
use crate::ring::store::{OutgoingAdoption, OutgoingPhase, PendingJoin};
use crate::HostAccess;
use xshell_protocol::ring::relay::connector::ProposalConnection;
use xshell_protocol::ring::Role;

pub enum CompletionOutcome {
    Pending {
        operation: String,
    },
    Consented {
        operation: String,
    },
    Declined {
        operation: String,
    },
    Installed {
        operation: String,
        chain: RosterChain,
    },
}
fn local_status(client: &CompanionClient) -> Result<CompletionPending, String> {
    match client.request(CompanionRequest::CompletionStatus)? {
        CompanionResponse::Completion {
            pending: Some(pending),
        } => Ok(*pending),
        _ => Err("no protected pending completion".into()),
    }
}
fn check_prepared(client: &CompanionClient, operation: &str) -> Result<(), String> {
    match client.request(CompanionRequest::CheckPrepared {
        operation: operation.into(),
    })? {
        CompanionResponse::Status {
            operation: op,
            phase: CompanionPhase::Prepared,
        } if op == operation => Ok(()),
        _ => Err("completion predecessor changed; fresh confirmation required".into()),
    }
}
fn rpc(
    host: &HostAccess,
    request: CompanionAdminRequest,
    timeout: Duration,
) -> Result<CompletionPending, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    host.companion_pending(
        request,
        Box::new(move |result| {
            let _ = tx.send(result);
        }),
    );
    let value = rx
        .recv_timeout(timeout)
        .map_err(|_| "completion Host RPC timed out; disposition unknown")?
        .map_err(|e| e.to_string())?;
    serde_json::from_value::<Option<CompletionPending>>(value)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no matching local pending completion".into())
}
fn tokens(chain: &RosterChain) -> Vec<String> {
    chain.versions().iter().map(|r| r.token().into()).collect()
}
impl DesktopRing {
    /// Durable pending intent, no credential and no network service on the Desktop.
    pub fn begin_companion_completion(&self, client: &CompanionClient) -> Result<String, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns completion".into());
        }
        let (keys, profile) = self.register_companion_inner(client)?;
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained profile missing")?;
        if stored.membership.is_some()
            || stored.pending_join.is_some()
            || stored.journal.as_ref().is_some_and(|j| {
                !matches!(j.phase, AdoptionPhase::Cancelled | AdoptionPhase::Installed)
            })
        {
            return Err(
                "leave_required or reconciliation_required: Desktop already has membership/work"
                    .into(),
            );
        }
        if keys.sign_key() != profile.desktop.sign_key {
            return Err("Desktop identity changed".into());
        }
        let intent = AdoptionIntent {
            operation: nonce()?,
            expires_at: now() + self.cfg.pair_ttl.min(Duration::from_secs(600)).as_secs(),
            target: profile.target(),
        };
        // The Descriptor/start is protected local ingress, not Host/Relay registration.
        let pending = match client.request(CompanionRequest::CompletionStart {
            intent: intent.clone(),
        })? {
            CompanionResponse::Completion { pending: Some(p) } if p.intent == intent => *p,
            _ => return Err("counterpart did not retain completion intent".into()),
        };
        let chain = RosterChain::from_tokens(&pending.predecessor).map_err(|e| e.to_string())?;
        if chain
            .authorized_member(&profile.daemon.sign_key)
            .is_none_or(|m| m.role != Role::Daemon || m.noise_key != profile.daemon.noise_key)
        {
            return Err("retained counterpart is not authorized".into());
        }
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            stored.pending_join = Some(PendingJoin {
                retained_predecessor: pending.predecessor.clone(),
                intent: intent.clone(),
                relay_url: chain.head().roster().relay_url.clone(),
            });
            Ok((Some(stored), ()))
        })?;
        Ok(intent.operation)
    }
    /// Explicitly abandon only a request for which local proofs have not been released.
    /// A new request uses a fresh operation/Computer ID and needs fresh confirmation.
    pub fn cancel_companion_completion(&self, client: &CompanionClient) -> Result<(), String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns completion".into());
        }
        let pending = local_status(client)?;
        if pending.proofs.is_some() || !pending.accepted.is_empty() {
            return Err("reconciliation_required: local completion proofs may have left".into());
        }
        client.request(CompanionRequest::Cancel {
            operation: pending.intent.operation.clone(),
        })?;
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            if stored
                .pending_join
                .as_ref()
                .is_some_and(|p| p.intent != pending.intent)
            {
                return Err("completion reservation changed".into());
            }
            stored.pending_join = None;
            if let Some(journal) = &mut stored.journal {
                if journal.intent == pending.intent && journal.phase == AdoptionPhase::Prepared {
                    journal.phase = AdoptionPhase::Cancelled;
                } else if !matches!(
                    journal.phase,
                    AdoptionPhase::Cancelled | AdoptionPhase::Installed
                ) {
                    return Err(
                        "reconciliation_required: completion journal is not cancellable".into(),
                    );
                }
            }
            Ok((Some(stored), ()))
        })?;
        Ok(())
    }
    pub fn confirm_companion_completion(
        &self,
        client: &CompanionClient,
        confirm: impl FnOnce(
            &ConsentPreview,
            &CreateConfirmationLease,
        ) -> Result<CreateDecision, String>,
    ) -> Result<CompletionOutcome, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns completion".into());
        }
        let pending = local_status(client)?;
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained profile missing")?;
        let reservation = stored
            .pending_join
            .as_ref()
            .ok_or("no pending Desktop completion")?;
        let profile = stored
            .companion
            .as_ref()
            .ok_or("no registered counterpart")?;
        if reservation.intent != pending.intent
            || profile.target() != pending.intent.target
            || stored.membership.is_some()
        {
            return Err("completion intent or Desktop identity changed".into());
        }
        let operation = pending.intent.operation.clone();
        let Some(proposal) = pending.proposal else {
            return Ok(CompletionOutcome::Pending { operation });
        };
        if proposal.operation != operation
            || !matches!(proposal.transcript, ConsentTranscript::Local)
        {
            return Err("completion proposal context mismatch".into());
        }
        let chain = RosterChain::from_tokens(&proposal.predecessor).map_err(|e| e.to_string())?;
        let retained = RosterChain::from_tokens(&reservation.retained_predecessor)
            .map_err(|e| e.to_string())?;
        let current = retained
            .extended(chain.versions())
            .map_err(|e| e.to_string())?
            .0;
        if current.head() != chain.head() {
            return Err("completion proposal is older than retained trust".into());
        }
        if chain.authorized_member(&profile.daemon.sign_key).is_none() {
            return Err("retained Daemon is revoked".into());
        }
        verify_local_proposal(Some(chain.head()), &proposal.proposal).map_err(|e| e.to_string())?;
        match client.request(CompanionRequest::Prepare {
            proposal: Box::new(proposal.clone()),
        })? {
            CompanionResponse::Status {
                operation: op,
                phase: CompanionPhase::Prepared,
            } if op == operation => {}
            _ => return Err("counterpart did not durably prepare this completion".into()),
        }
        let context = proposal.context()?;
        let hash = consent_proposal_hash(&proposal.proposal).map_err(|e| e.to_string())?;
        let lease = CreateConfirmationLease {
            operation: operation.clone(),
            proposal_hash: hash.clone(),
            expires_at: pending.intent.expires_at,
            until: Instant::now()
                + Duration::from_secs(pending.intent.expires_at.saturating_sub(now())),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            stored.pending_join = None;
            stored.journal = Some(AdoptionJournal {
                intent: pending.intent.clone(),
                predecessor: proposal.predecessor.clone(),
                proposal: proposal.proposal.clone(),
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
        let preview = ConsentPreview {
            operation: operation.clone(),
            proposal_hash: hash,
            ring_id: proposal.proposal.ring_id.clone(),
            relay_url: proposal.proposal.relay_url.clone(),
            profile: profile.clone(),
            context: context.clone(),
            proposal: proposal.proposal.clone(),
            expires_at: pending.intent.expires_at,
        };
        self.create_live(&lease)?;
        let decision = confirm(&preview, &lease)?;
        let CreateDecision::Confirm(confirmation) = decision else {
            client.request(CompanionRequest::Cancel {
                operation: operation.clone(),
            })?;
            self.journal_update(&operation, |j| j.phase = AdoptionPhase::Cancelled)?;
            return Ok(CompletionOutcome::Declined { operation });
        };
        lease.verify(&confirmation)?;
        self.create_live(&lease)?;
        check_prepared(client, &operation)?;
        self.create_live(&lease)?;
        self.journal_update(&operation, |j| j.phase = AdoptionPhase::ConsentMayHaveLeft)?;
        // The journal transaction can block too. Do not sign against a predecessor
        // known to have changed during that wait; D independently repeats this at consent.
        check_prepared(client, &operation).map_err(|e| format!("reconciliation_required: {e}"))?;
        self.create_live(&lease)?;
        let message = consent_message(&proposal.proposal, pending.intent.expires_at, &context)
            .map_err(|e| e.to_string())?;
        let desktop_proof = ConsentProof {
            key: stored.keys.sign_key(),
            expires_at: pending.intent.expires_at,
            context,
            signature: Signature::from_bytes(
                stored.keys.sign(&message).map_err(|e| e.to_string())?,
            ),
        };
        match client
            .request(CompanionRequest::Consent {
                operation: operation.clone(),
                desktop_proof,
            })
            .map_err(|e| format!("reconciliation_required: {e}"))?
        {
            CompanionResponse::Consent { proof }
                if proof.key == profile.daemon.sign_key
                    && proof.context == preview.context
                    && proof.expires_at == pending.intent.expires_at
                    && xshell_protocol::ring::verify(&proof.key, &message, &proof.signature) =>
            {
                Ok(CompletionOutcome::Consented { operation })
            }
            _ => Err("reconciliation_required: counterpart consent receipt missing".into()),
        }
    }
    /// Retry only installation of the exact accepted transition; never publication/enrollment.
    pub fn install_companion_completion(
        &self,
        client: &CompanionClient,
    ) -> Result<CompletionOutcome, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns completion".into());
        }
        let pending = local_status(client)?;
        let operation = pending.intent.operation.clone();
        if pending.accepted.is_empty() {
            return Ok(CompletionOutcome::Pending { operation });
        }
        let accepted = RosterChain::from_tokens(&pending.accepted).map_err(|e| e.to_string())?;
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained profile missing")?;
        let journal = stored
            .journal
            .as_ref()
            .ok_or("local completion journal missing")?;
        if journal.intent != pending.intent
            || !matches!(journal.transcript, ConsentTranscript::Local)
        {
            return Err("accepted result does not match local completion".into());
        }
        let profile = stored.companion.ok_or("registered counterpart missing")?;
        self.install_joined_computer(client, &operation, &profile, &accepted)
            .map_err(|e| format!("reconciliation_required: {e}"))?;
        Ok(CompletionOutcome::Installed {
            operation,
            chain: accepted,
        })
    }
    pub fn prepare_companion_completion(
        &self,
        host: HostAccess,
    ) -> Result<CompletionApproval<'_>, String> {
        let admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns completion approval".into());
        }
        let pending = rpc(
            &host,
            CompanionAdminRequest::Pending,
            self.cfg.timeouts.request,
        )?;
        if pending.proposal.is_some() || pending.proofs.is_some() || !pending.accepted.is_empty() {
            return Err("reconciliation_required: pending completion already proposed".into());
        }
        let stored = self
            .store
            .load_profile()?
            .0
            .ok_or("retained approver missing")?;
        if stored.outgoing.as_ref().is_some_and(|j| {
            !matches!(j.phase, OutgoingPhase::Published | OutgoingPhase::Cancelled)
        }) {
            return Err("reconciliation_required: earlier publication unresolved".into());
        }
        let (chain, lane) = self.proposal_snapshot()?;
        let member = stored
            .membership
            .as_ref()
            .ok_or("approval requires membership")?;
        if member.chain.head() != chain.head() {
            return Err("accepted predecessor changed".into());
        }
        let xshell_protocol::ring::pairing::adoption::JoinTarget::Computer { desktop, daemon } =
            &pending.intent.target
        else {
            return Err("completion requires Computer target".into());
        };
        let old = chain
            .head()
            .roster()
            .membership()
            .map_err(|e| e.to_string())?;
        let singleton = old
            .unit_for(&daemon.sign_key)
            .ok_or("retained singleton missing")?;
        if singleton.kind != EnrollmentKind::Daemon
            || chain.authorized_member(&daemon.sign_key).is_none()
            || chain.head().member(&desktop.sign_key).is_some()
        {
            return Err("only retained Daemon-only completion is supported".into());
        }
        let mut proposal = chain.head().roster().clone();
        proposal.version += 1;
        proposal.prev = Some(chain.head().hash());
        proposal.signed_by = stored.keys.sign_key();
        proposal.issued_at = now();
        proposal.members.push(Member::new(
            &desktop.name,
            Role::Desktop,
            desktop.sign_key,
            desktop.noise_key,
            proposal.issued_at,
        ));
        let mut membership = old.clone();
        membership
            .enrollments
            .retain(|unit| unit.id != singleton.id);
        membership.retired.push(singleton.id.clone());
        membership.enrollments.push(Enrollment {
            id: pending.intent.operation.clone(),
            kind: EnrollmentKind::Computer,
            members: vec![desktop.sign_key, daemon.sign_key],
            enrolled_at: proposal.issued_at,
        });
        membership.change = RosterChange::CompleteComputer {
            replaces: vec![singleton.id.clone()],
            enrollment_id: pending.intent.operation.clone(),
            proofs: vec![],
        };
        proposal.set_membership(&membership);
        verify_local_proposal(Some(chain.head()), &proposal).map_err(|e| e.to_string())?;
        lane.freeze_completion(&proposal)
            .map_err(|e| e.to_string())?;
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained approver missing")?;
            stored.outgoing = Some(OutgoingAdoption {
                intent: pending.intent.clone(),
                predecessor: tokens(&chain),
                proposal: proposal.clone(),
                final_noise_hash: None,
                phase: OutgoingPhase::Prepared,
                signed_candidate: None,
                accepted: vec![],
            });
            Ok((Some(stored), ()))
        })?;
        let operation = pending.intent.operation.clone();
        if let Err(error) = rpc(
            &host,
            CompanionAdminRequest::Propose {
                proposal: Box::new(CompanionProposal {
                    operation: operation.clone(),
                    predecessor: tokens(&chain),
                    proposal: proposal.clone(),
                    transcript: ConsentTranscript::Local,
                }),
            },
            self.cfg.timeouts.request,
        ) {
            self.outgoing_update(&operation, |j| j.phase = OutgoingPhase::Cancelled)?;
            return Err(error);
        }
        Ok(CompletionApproval {
            ring: self,
            host,
            lane,
            intent: pending.intent,
            proposal,
            _admission: admission,
            published: false,
            accepted: None,
        })
    }
}
pub struct CompletionApproval<'a> {
    ring: &'a DesktopRing,
    host: HostAccess,
    lane: ProposalConnection,
    intent: AdoptionIntent,
    proposal: Roster,
    _admission: AdoptionAdmission,
    published: bool,
    accepted: Option<RosterChain>,
}
impl CompletionApproval<'_> {
    /// Deliver only independently read-back accepted bytes on the current Host generation.
    /// Completion replaces the singleton ID and can invalidate the old HostAccess; this
    /// retry is a typed result, never a replay of the signed Relay mutation.
    pub fn deliver_result(&self, host: &HostAccess) -> Result<(), String> {
        if host.id() != self.host.id() {
            return Err("completion result Host identity changed".into());
        }
        let accepted = self
            .accepted
            .as_ref()
            .ok_or("completion has not been independently read back")?;
        rpc(
            host,
            CompanionAdminRequest::Accepted {
                operation: self.intent.operation.clone(),
                rosters: tokens(accepted),
            },
            self.ring.cfg.timeouts.request,
        )
        .map_err(|e| format!("published_result_pending: {e}"))?;
        Ok(())
    }
    /// None means local confirmation has not occurred. The typed Host request cannot
    /// produce it. Any possible publication is one-shot and requires reconciliation.
    pub fn publish(&mut self) -> Result<Option<join::ApprovalOutcome>, String> {
        if self.published {
            return Err("reconciliation_required: publication already attempted".into());
        }
        if now() >= self.intent.expires_at || !self.ring.pairing_ipc_owner() {
            return Err("completion proof/owner expired".into());
        }
        let pending = rpc(
            &self.host,
            CompanionAdminRequest::Proofs {
                operation: self.intent.operation.clone(),
            },
            self.ring.cfg.timeouts.request,
        )?;
        if pending.intent != self.intent {
            return Err("completion pending intent changed".into());
        }
        let Some(proofs) = pending.proofs else {
            return Ok(None);
        };
        if now() >= self.intent.expires_at || !self.ring.pairing_ipc_owner() {
            return Err("completion proof/owner expired".into());
        }
        let mut completed = self.proposal.clone();
        let mut membership = completed.membership().map_err(|e| e.to_string())?;
        let RosterChange::CompleteComputer { proofs: slot, .. } = &mut membership.change else {
            return Err("not completion".into());
        };
        *slot = proofs;
        completed.set_membership(&membership);
        let stored = self
            .ring
            .store
            .load_profile()?
            .0
            .ok_or("retained approver missing")?;
        let signed = completed
            .sign(stored.keys.as_ref())
            .map_err(|e| e.to_string())?;
        xshell_protocol::ring::verify_successor(self.lane.predecessor(), &signed)
            .map_err(|e| e.to_string())?;
        self.ring.outgoing_update(&self.intent.operation, |j| {
            j.signed_candidate = Some(signed.token().into())
        })?;
        if stored
            .membership
            .as_ref()
            .is_none_or(|m| m.chain.head().hash() != self.lane.predecessor().hash())
        {
            return Err("accepted predecessor changed; fresh confirmation required".into());
        }
        self.ring.adoption_test_point(
            super::super::desktop::AdoptionTestPoint::CompletionBeforeUncertainWrite,
        );
        self.ring.outgoing_update(&self.intent.operation, |j| {
            j.phase = OutgoingPhase::PublicationUncertain
        })?;
        self.published = true;
        if now() >= self.intent.expires_at || !self.ring.pairing_ipc_owner() {
            return Err(
                "reconciliation_required: completion proof/owner expired before publication".into(),
            );
        }
        self.ring
            .adoption_test_point(super::super::desktop::AdoptionTestPoint::CompletionBeforePublish);
        if now() >= self.intent.expires_at || !self.ring.pairing_ipc_owner() {
            return Err(
                "reconciliation_required: completion proof/owner expired before publication".into(),
            );
        }
        self.lane
            .publish_once(&signed)
            .map_err(|e| format!("reconciliation_required: {e}"))?;
        let accepted = self
            .lane
            .read_accepted(&signed)
            .map_err(|e| format!("reconciliation_required: {e}"))?;
        self.ring
            .store
            .transact_profile(|stored, _| {
                let mut stored = stored.ok_or("retained approver missing")?;
                let journal = stored
                    .outgoing
                    .as_mut()
                    .filter(|j| j.intent.operation == self.intent.operation)
                    .ok_or("outgoing completion changed")?;
                journal.phase = OutgoingPhase::Published;
                journal.accepted = tokens(&accepted);
                let member = stored
                    .membership
                    .as_mut()
                    .ok_or("approver membership missing")?;
                member.chain = member
                    .chain
                    .extended(accepted.versions())
                    .map_err(|e| e.to_string())?
                    .0;
                member.retarget_farewells();
                Ok((Some(stored), ()))
            })
            .map_err(|e| format!("reconciliation_required: {e}"))?;
        self.ring.reload();
        self.accepted = Some(accepted.clone());
        Ok(Some(join::ApprovalOutcome {
            operation: self.intent.operation.clone(),
            chain: accepted,
        }))
    }
}
impl Drop for CompletionApproval<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = self.ring.outgoing_update(&self.intent.operation, |j| {
                j.phase = OutgoingPhase::Cancelled
            });
        }
    }
}

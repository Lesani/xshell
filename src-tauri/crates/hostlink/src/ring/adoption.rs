//! Public consented Computer Create, Join and Approve controllers. Confirmation is a local
//! backend capability, never a Host/Relay request. Retained v2 Daemon-only completion
//! uses typed Host administration; native origins, Host-first and general recovery remain later.
use super::companion_ipc::CompanionClient;
use super::store::{AdoptionJournal, AdoptionPhase, RingState};
use super::DesktopRing;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_protocol::ring::companion::*;
use xshell_protocol::ring::membership::*;
use xshell_protocol::ring::pairing::adoption::{AdoptionIntent, PublicIdentity};
use xshell_protocol::ring::relay::{
    ByeReason, ChainPin, CloseReason, RingClient, RingClientConfig, RingEvents,
};
use xshell_protocol::ring::{
    DeviceKeys, Member, RingId, Roster, RosterChain, SignKey, Signature, Signer,
};

mod completion;
pub use completion::{CompletionApproval, CompletionOutcome};
mod join;
pub use join::{ApprovalOutcome, JoinOffer, JoinStatus};

struct AdoptionAdmission(Arc<AtomicBool>);
impl Drop for AdoptionAdmission {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn nonce() -> Result<String, String> {
    xshell_protocol::ring::relay::wire::new_nonce().map_err(|e| e.to_string())
}
#[derive(Clone, Debug)]
pub struct ConsentPreview {
    pub operation: String,
    pub proposal_hash: String,
    pub ring_id: RingId,
    pub relay_url: String,
    pub profile: CompanionProfile,
    pub context: ConsentContext,
    pub proposal: Roster,
    pub expires_at: u64,
}
/// An in-process, operation-specific lease, introduced for Create and also used by Join.
/// It cannot be deserialized or sent over Host IPC. Native window-origin integration is
/// deliberately not claimed by this backend capability.
#[derive(Clone)]
pub struct CreateConfirmationLease {
    operation: String,
    proposal_hash: String,
    until: Instant,
    expires_at: u64,
    cancelled: Arc<AtomicBool>,
}
pub struct CreateConfirmation {
    operation: String,
    proposal_hash: String,
    lease: Arc<AtomicBool>,
}
pub enum CreateDecision {
    Decline,
    Confirm(CreateConfirmation),
}
pub enum AdoptionOutcome {
    Declined {
        operation: String,
    },
    Installed {
        operation: String,
        chain: RosterChain,
    },
}
impl CreateConfirmationLease {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn live(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Acquire)
            || Instant::now() >= self.until
            || now() >= self.expires_at
        {
            return Err("Create confirmation lease cancelled or expired".into());
        }
        Ok(())
    }
    pub fn confirm(&self, proposal_hash: &str) -> Result<CreateConfirmation, String> {
        self.live()?;
        if self.proposal_hash != proposal_hash {
            return Err("Create confirmation proposal mismatch".into());
        }
        Ok(CreateConfirmation {
            operation: self.operation.clone(),
            proposal_hash: proposal_hash.into(),
            lease: self.cancelled.clone(),
        })
    }
    fn verify(&self, proof: &CreateConfirmation) -> Result<(), String> {
        self.live()?;
        if proof.operation != self.operation
            || proof.proposal_hash != self.proposal_hash
            || !Arc::ptr_eq(&self.cancelled, &proof.lease)
        {
            return Err("Create confirmation belongs to another operation or lease".into());
        }
        Ok(())
    }
}
struct BootstrapEvents;
impl RingEvents for BootstrapEvents {
    fn envelope(&self, _: SignKey, _: Vec<u8>) {}
    fn closed(&self, _: CloseReason) {}
    fn presence(&self, _: SignKey, _: xshell_protocol::ring::relay::MemberPresence) {}
    fn roster(&self, _: &xshell_protocol::ring::SignedRoster) {}
    fn roster_rejected(&self, _: xshell_protocol::ring::RosterError) {}
}
impl DesktopRing {
    fn adoption_test_point(&self, point: super::desktop::AdoptionTestPoint) {
        if let Some(hook) = &self.cfg.adoption_test_hook {
            (hook.0)(point);
        }
    }
    fn reserve_adoption(&self) -> Result<AdoptionAdmission, String> {
        self.adoption
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "another adoption operation is active")?;
        Ok(AdoptionAdmission(self.adoption.clone()))
    }

    pub fn register_companion(&self, client: &CompanionClient) -> Result<CompanionProfile, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns the Desktop counterpart".into());
        }
        self.register_companion_inner(client)
            .map(|(_, profile)| profile)
    }
    fn register_companion_inner(
        &self,
        client: &CompanionClient,
    ) -> Result<(Arc<DeviceKeys>, CompanionProfile), String> {
        let keys = self.store.retained_keys()?;
        let (daemon, existing) = match client.request(CompanionRequest::Descriptor)? {
            CompanionResponse::Identity { identity, profile } => (identity, profile),
            _ => return Err("invalid counterpart identity response".into()),
        };
        let desktop_dir = self.store.dir().canonicalize().map_err(|e| e.to_string())?;
        let daemon_dir = client
            .endpoint()
            .parent()
            .ok_or("counterpart endpoint has no directory")?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let profile = CompanionProfile::for_stores(
            desktop_dir
                .to_str()
                .ok_or("Desktop profile path is not UTF-8")?
                .into(),
            daemon_dir
                .to_str()
                .ok_or("Daemon profile path is not UTF-8")?
                .into(),
            PublicIdentity {
                name: xshell_protocol::ring::member_name(&self.cfg.name, "computer"),
                sign_key: keys.sign_key(),
                noise_key: keys.noise_key(),
            },
            daemon,
            client
                .endpoint()
                .to_str()
                .ok_or("counterpart endpoint is not UTF-8")?
                .into(),
        );
        profile.validate()?;
        if existing
            .as_ref()
            .is_some_and(|existing| existing != &profile)
        {
            return Err("conflicting retained counterpart registration".into());
        }
        // Retain the association before registration can escape to the other store. A
        // stopped/unavailable counterpart is never grounds to discard this descriptor.
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained Desktop profile missing")?;
            if stored
                .companion
                .as_ref()
                .is_some_and(|existing| existing != &profile)
            {
                return Err("Desktop profile is bound to another counterpart".into());
            }
            stored.companion = Some(profile.clone());
            Ok((Some(stored), ()))
        })?;
        let proof = Signature::from_bytes(
            keys.sign(&registration_message(&profile))
                .map_err(|e| e.to_string())?,
        );
        match client.request(CompanionRequest::Register {
            profile: profile.clone(),
            proof,
        })? {
            CompanionResponse::Descriptor {
                profile: Some(accepted),
            } if accepted == profile => Ok((keys, profile)),
            _ => Err("counterpart did not retain this registration".into()),
        }
    }
    fn journal_update(
        &self,
        operation: &str,
        update: impl FnOnce(&mut AdoptionJournal),
    ) -> Result<(), String> {
        self.store
            .transact_profile(|profile, _| {
                let mut profile = profile.ok_or("retained profile missing")?;
                let journal = profile
                    .journal
                    .as_mut()
                    .filter(|journal| journal.intent.operation == operation)
                    .ok_or("Create operation changed")?;
                update(journal);
                Ok((Some(profile), ()))
            })
            .map(|_| ())
    }
    fn create_live(&self, lease: &CreateConfirmationLease) -> Result<(), String> {
        lease.live()?;
        if !self.pairing_ipc_owner() {
            return Err("Create owner process ended or changed".into());
        }
        Ok(())
    }
    fn install_joined_computer(
        &self,
        client: &CompanionClient,
        operation: &str,
        profile: &CompanionProfile,
        accepted: &RosterChain,
    ) -> Result<(), String> {
        let tokens: Vec<String> = accepted
            .versions()
            .iter()
            .map(|roster| roster.token().into())
            .collect();
        self.journal_update(operation, |journal| {
            journal.phase = AdoptionPhase::Accepted;
            journal.accepted = tokens.clone();
        })?;
        match client.request(CompanionRequest::InstallAccepted {
            operation: operation.into(),
            rosters: tokens,
        })? {
            CompanionResponse::Installed { hash } if hash == accepted.head().hash() => {}
            _ => return Err("counterpart durable installation receipt missing".into()),
        }
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            let journal = stored
                .journal
                .as_mut()
                .filter(|journal| journal.intent.operation == operation)
                .ok_or("adoption journal changed")?;
            if stored.membership.is_some() {
                return Err("membership appeared before accepted local installation".into());
            }
            journal.daemon_installed = true;
            journal.desktop_installed = true;
            journal.phase = AdoptionPhase::Installed;
            stored.membership = Some(RingState {
                keys: stored.keys.clone(),
                chain: accepted.clone(),
                local_member: Some(profile.daemon.sign_key),
                farewells: vec![],
                held_relays: [accepted.head().roster().relay_url.clone()]
                    .into_iter()
                    .collect(),
                host_members: Default::default(),
            });
            Ok((Some(stored), ()))
        })?;
        self.reload();
        Ok(())
    }

    /// Explicit Create only. No connection/setup path calls this automatically. The callback
    /// must present the verified preview and return a matching in-process confirmation.
    pub fn create_computer(
        &self,
        client: &CompanionClient,
        confirm: impl FnOnce(
            &ConsentPreview,
            &CreateConfirmationLease,
        ) -> Result<CreateDecision, String>,
    ) -> Result<AdoptionOutcome, String> {
        let _admission = self.reserve_adoption()?;
        if !self.pairing_ipc_owner() {
            return Err("another process owns Create".into());
        }
        if let Some(profile) = self.store.load_profile()?.0 {
            if profile.pending_join.is_some() {
                return Err("reconciliation_required: an earlier Join is unresolved".into());
            }
            if profile.membership.is_some() {
                return Err("leave_required: Create needs an unpaired Desktop".into());
            }
            if profile.journal.as_ref().is_some_and(|journal| {
                !matches!(
                    journal.phase,
                    AdoptionPhase::Cancelled | AdoptionPhase::Installed
                )
            }) {
                return Err("reconciliation_required: an earlier adoption is unresolved".into());
            }
        }
        let (keys, profile) = self.register_companion_inner(client)?;
        let operation = nonce()?;
        let ttl = self.cfg.pair_ttl.min(Duration::from_secs(600));
        let expires_at = now() + ttl.as_secs();
        let until = Instant::now() + ttl;
        let intent = AdoptionIntent {
            operation: operation.clone(),
            expires_at,
            target: profile.target(),
        };
        let ring_id = RingId::derive(&keys.sign_key());
        let activity_key = RingClient::activity_key(
            &self.cfg.default_relay_url,
            &ring_id,
            None,
            self.cfg.timeouts.connect,
        )
        .map_err(|e| e.to_string())?;
        let at = now();
        let mut proposal = Roster {
            v: 2,
            ring_id: ring_id.clone(),
            version: 1,
            prev: None,
            relay_url: self.cfg.default_relay_url.clone(),
            signed_by: keys.sign_key(),
            issued_at: at,
            members: intent
                .target
                .identities()
                .iter()
                .map(|(identity, role)| {
                    Member::new(
                        &identity.name,
                        *role,
                        identity.sign_key,
                        identity.noise_key,
                        at,
                    )
                })
                .collect(),
            extra: Default::default(),
        };
        proposal.set_membership(&Membership {
            enrollments: vec![Enrollment {
                id: operation.clone(),
                kind: EnrollmentKind::Computer,
                members: vec![profile.desktop.sign_key, profile.daemon.sign_key],
                enrolled_at: at,
            }],
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: Some(DEFAULT_INACTIVITY_SECONDS),
                activity_key,
            },
            change: RosterChange::Genesis { proofs: vec![] },
        });
        verify_local_proposal(None, &proposal).map_err(|e| e.to_string())?;
        let context = local_consent_context(&proposal).map_err(|e| e.to_string())?;
        let proposal_hash = consent_proposal_hash(&proposal).map_err(|e| e.to_string())?;
        let lease = CreateConfirmationLease {
            operation: operation.clone(),
            proposal_hash: proposal_hash.clone(),
            until,
            expires_at,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        *self
            .adoption_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&lease.cancelled));
        self.store.transact_profile(|stored, _| {
            let mut stored = stored.ok_or("retained profile missing")?;
            if stored.membership.is_some() {
                return Err("leave_required: membership appeared before Create preparation".into());
            }
            stored.journal = Some(AdoptionJournal {
                intent: intent.clone(),
                predecessor: vec![],
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
            client.request(CompanionRequest::Start {
                intent: intent.clone(),
            })?;
            client.request(CompanionRequest::Prepare {
                proposal: Box::new(CompanionProposal {
                    operation: operation.clone(),
                    predecessor: vec![],
                    proposal: proposal.clone(),
                    transcript: ConsentTranscript::Local,
                }),
            })?;
            self.create_live(&lease)?;
            let preview = ConsentPreview {
                operation: operation.clone(),
                proposal_hash,
                ring_id,
                relay_url: proposal.relay_url.clone(),
                profile: profile.clone(),
                context: context.clone(),
                proposal: proposal.clone(),
                expires_at,
            };
            let decision = confirm(&preview, &lease)?;
            if let CreateDecision::Decline = decision {
                client.request(CompanionRequest::Cancel {
                    operation: operation.clone(),
                })?;
                self.journal_update(&operation, |journal| {
                    journal.phase = AdoptionPhase::Cancelled
                })?;
                return Ok(AdoptionOutcome::Declined {
                    operation: operation.clone(),
                });
            }
            let CreateDecision::Confirm(confirmation) = decision else {
                unreachable!()
            };
            lease.verify(&confirmation)?;
            self.create_live(&lease)?;
            // Durable before either local enrollment proof can escape. Expiry/cancellation
            // after this point must retain uncertainty, not regenerate a Ring or an offer.
            self.journal_update(&operation, |journal| {
                journal.phase = AdoptionPhase::ConsentMayHaveLeft
            })?;
            let message =
                consent_message(&proposal, expires_at, &context).map_err(|e| e.to_string())?;
            let desktop_proof = ConsentProof {
                key: keys.sign_key(),
                expires_at,
                context: context.clone(),
                signature: Signature::from_bytes(keys.sign(&message).map_err(|e| e.to_string())?),
            };
            let daemon_proof = match client.request(CompanionRequest::Consent {
                operation: operation.clone(),
                desktop_proof: desktop_proof.clone(),
            })? {
                CompanionResponse::Consent { proof } => proof,
                _ => return Err("counterpart consent response refused".into()),
            };
            let mut membership = proposal.membership().map_err(|e| e.to_string())?;
            membership.change = RosterChange::Genesis {
                proofs: vec![desktop_proof, daemon_proof],
            };
            let mut completed = proposal;
            completed.set_membership(&membership);
            let signed = completed.sign(&*keys).map_err(|e| e.to_string())?;
            let candidate =
                RosterChain::from_chain(vec![signed.clone()]).map_err(|e| e.to_string())?;
            self.create_live(&lease)?;
            self.journal_update(&operation, |journal| {
                journal.signed_candidate = Some(signed.token().into());
                journal.phase = AdoptionPhase::PublicationUncertain;
            })?;
            self.create_live(&lease)?;
            // No normal Connector knows this candidate. One explicit bootstrap client
            // uploads exactly these journalled bytes; no retry/reconnect loop exists here.
            let mut config = RingClientConfig::new(candidate, keys.clone());
            config.timeouts = self.cfg.timeouts;
            let bootstrap = RingClient::connect(config, Arc::new(BootstrapEvents))
                .map_err(|e| e.to_string())?;
            // Only after a positively closed bootstrap socket may the independent pinned
            // fetch authenticate with this same key. Failure leaves uncertainty, no retry.
            bootstrap
                .bye(ByeReason::quit())
                .map_err(|e| e.to_string())?;
            let accepted = RingClient::fetch_chain(
                &completed.relay_url,
                &completed.ring_id,
                keys.clone(),
                &ChainPin {
                    version: signed.version(),
                    hash: signed.hash(),
                },
                None,
                self.cfg.timeouts,
            )
            .map_err(|e| e.to_string())?;
            if accepted.get(signed.version()) != Some(&signed)
                || [profile.desktop.sign_key, profile.daemon.sign_key]
                    .iter()
                    .any(|key| accepted.authorized_member(key).is_none())
            {
                return Err(
                    "accepted Create is forked or no longer authorizes this Computer".into(),
                );
            }
            self.install_joined_computer(client, &operation, &profile, &accepted)?;
            Ok(AdoptionOutcome::Installed {
                operation: operation.clone(),
                chain: accepted,
            })
        })();
        if let Err(error) = result {
            let phase = self
                .store
                .load_profile()?
                .0
                .and_then(|profile| profile.journal)
                .map(|journal| journal.phase);
            if phase == Some(AdoptionPhase::Prepared) {
                if client
                    .request(CompanionRequest::Cancel {
                        operation: operation.clone(),
                    })
                    .is_ok()
                {
                    self.journal_update(&operation, |journal| {
                        journal.phase = AdoptionPhase::Cancelled
                    })?;
                }
                return Err(error);
            }
            return Err(format!("reconciliation_required: {error}"));
        }
        result
    }
}

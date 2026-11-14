//! Explicit command-scoped standalone adoption. No Host RPC can invoke or confirm this
//! controller; registration (including a stopped Desktop) always excludes it.
use super::ring::{write_private, Store};
use super::standalone_journal::{
    accepted_record, load_journal, recorded_predecessor, validate_standalone_install,
    StandaloneJournal,
};
use super::ServerHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::membership::consent_proposal_hash;
use xshell_protocol::ring::pairing::adoption::{
    AdoptionIntent, AdoptionSigners, JoinTarget, PublicIdentity,
};
use xshell_protocol::ring::relay::pair::{
    adopt_as_guest_with_ready, AdoptionGuestRequest, PairOptions,
};
use xshell_protocol::ring::relay::{ChainPin, RingClient, RingTimeouts};
use xshell_protocol::ring::RosterChain;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Denial-only durable preparation under the actual runtime -> ring.lock fence.
/// The owner never signs or confirms the command's proposal.
pub(super) fn prepare_standalone_command(
    ring: &super::ring::Ring,
    intent: AdoptionIntent,
    proposal: CompanionProposal,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<(), String> {
    let record = StandaloneJournal {
        intent,
        proposal,
        consent_may_have_left: true,
        accepted: vec![],
        installed: false,
    };
    ring.with_effective_chain_until(budget, |current| {
        let store = ring.standalone_store();
        let _lock = store.lock_until(budget)?;
        prepare_standalone_locked(store, &record, current.as_ref(), budget)
    })
}

fn prepare_standalone_locked(
    store: &Store,
    record: &StandaloneJournal,
    current: Option<&RosterChain>,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<(), String> {
    store.refuse_registered_binding()?;
    if current.is_some() || super::companion::trusted(store)?.is_some() {
        return Err("leave_required: standalone command owner retains membership".into());
    }
    recorded_predecessor(store, record)?;
    // The public envelope must name this exact Enroll, not merely the same own-key
    // target under a different command operation. Accepted installation checks this
    // too, but preparation must refuse before writing even a conservative obligation.
    let membership = record
        .proposal
        .proposal
        .membership()
        .map_err(|e| e.to_string())?;
    if !matches!(membership.change, xshell_protocol::ring::membership::RosterChange::Enroll { enrollment_id, .. }
            if enrollment_id == record.intent.operation)
    {
        return Err("standalone preparation operation differs from proposal".into());
    }
    let left = record.intent.expires_at.saturating_sub(now());
    if left == 0 || left > 600 {
        return Err("standalone preparation expired".into());
    }
    if let Some(existing) = load_journal(store)? {
        if &existing == record {
            return Ok(());
        }
        return Err(
            "reconciliation_required: standalone operation changed or already advanced".into(),
        );
    }
    budget.check().map_err(|e| e.to_string())?;
    write_private(
        &store.dir.join("standalone-adoption.json"),
        &serde_json::to_vec(record).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// An in-process command capability, not a serializable confirmation request.
pub struct StandaloneLease {
    hash: String,
    live: Arc<AtomicBool>,
    until: Instant,
}
pub struct StandaloneConfirmation(Arc<AtomicBool>);
impl StandaloneLease {
    pub fn confirm(&self, hash: &str) -> Result<StandaloneConfirmation, String> {
        if hash != self.hash || !self.live.load(Ordering::Acquire) || Instant::now() >= self.until {
            return Err("standalone command lease or proposal changed".into());
        }
        Ok(StandaloneConfirmation(self.live.clone()))
    }
}
pub struct StandalonePreview {
    pub intent: AdoptionIntent,
    pub proposal: CompanionProposal,
    pub proposal_hash: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StandaloneRecoveryState {
    ReconciliationRequired,
    AcceptedInstalling,
    Installed,
    Terminal,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandaloneRecoveryStatus {
    pub operation: String,
    pub expires_at: u64,
    pub state: StandaloneRecoveryState,
}
fn recovery_status(
    store: &Store,
    record: &StandaloneJournal,
    current: Option<&RosterChain>,
) -> Result<StandaloneRecoveryStatus, String> {
    let mut state = if record.installed {
        StandaloneRecoveryState::Installed
    } else if record.accepted.is_empty() {
        StandaloneRecoveryState::ReconciliationRequired
    } else {
        StandaloneRecoveryState::AcceptedInstalling
    };
    if !record.accepted.is_empty() {
        let mut accepted = accepted_record(store, record)?;
        if let Some(current) = current {
            accepted.merge_terminal_from(current);
            accepted = accepted
                .extended(current.versions())
                .map_err(|e| e.to_string())?
                .0;
        }
        if accepted
            .authorized_member(&record.intent.target.principal().sign_key)
            .is_none()
        {
            state = StandaloneRecoveryState::Terminal;
        }
    }
    Ok(StandaloneRecoveryStatus {
        operation: record.intent.operation.clone(),
        expires_at: record.intent.expires_at,
        state,
    })
}
/// Called only under ring.lock, and (when live) effective runtime guards.
fn finalize_standalone_receipt_locked(
    store: &Store,
    record: &mut StandaloneJournal,
    accepted: &RosterChain,
    effective: Option<RosterChain>,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<StandaloneRecoveryStatus, String> {
    // Preserve the complete-record concurrency fence for both the new receipt
    // and already-Installed reporting paths.
    if load_journal(store)?.as_ref() != Some(record) {
        return Err("standalone reconciliation journal changed before final receipt".into());
    }
    let durable =
        super::companion::trusted(store)?.ok_or("standalone durable installation missing")?;
    let mut current = effective
        .unwrap_or_else(|| accepted.clone())
        .extended(accepted.versions())
        .map_err(|e| e.to_string())?
        .0;
    current.merge_terminal_from(accepted);
    current = current
        .extended(durable.versions())
        .map_err(|e| e.to_string())?
        .0;
    if current
        .authorized_member(&record.intent.target.principal().sign_key)
        .is_none()
    {
        return recovery_status(store, record, Some(&current));
    }
    if current.head() != durable.head() {
        return Err("standalone current accepted ancestry is not durably installed".into());
    }
    budget.check().map_err(|e| e.to_string())?;
    if !record.installed {
        record.accepted = current
            .versions()
            .iter()
            .map(|r| r.token().into())
            .collect();
        record.installed = true;
        write_private(
            &store.dir.join("standalone-adoption.json"),
            &serde_json::to_vec(record).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    recovery_status(store, record, Some(&current))
}

/// The live command and in-process driver share the reconciliation final receipt.
pub(super) fn finalize_standalone_install(
    ring: &super::ring::Ring,
    operation: &str,
    accepted: &RosterChain,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<(), String> {
    ring.with_effective_chain_until(budget, |effective| {
        let store = ring.standalone_store();
        let _lock = store.lock_until(budget)?;
        validate_standalone_install(store, operation, accepted)?;
        let mut record = load_journal(store)?.ok_or("standalone obligation missing")?;
        let status =
            finalize_standalone_receipt_locked(store, &mut record, accepted, effective, budget)?;
        if status.state != StandaloneRecoveryState::Installed {
            return Err("reconciliation_required: standalone installation is terminal".into());
        }
        Ok(())
    })
}

impl ServerHandle {
    pub fn standalone_adoption_status(&self) -> Result<Option<StandaloneRecoveryStatus>, String> {
        let _operation = self.d.store_admission.admit()?;
        let store = Store::new(self.d.cfg.paths.ring_dir.clone());
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(
            Instant::now() + Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
        );
        self.d.ring.with_effective_chain_until(&budget, |current| {
            let _lock = store.lock_until(&budget)?;
            load_journal(&store)?
                .as_ref()
                .map(|r| recovery_status(&store, r, current.as_ref()))
                .transpose()
        })
    }

    /// Observe only, then retry the same accepted local obligation. No Pair Open, proof
    /// release, signed publication or fresh enrollment occurs on this path.
    pub fn reconcile_standalone_adoption(&self) -> Result<StandaloneRecoveryStatus, String> {
        self.reconcile_standalone_adoption_with(|original, keys, pin| {
            if let Some((owner, _, _)) = self.d.ring.push_link() {
                owner.read_accepted_chain(pin)
            } else {
                RingClient::fetch_chain(
                    &original.proposal.proposal.relay_url,
                    &original.proposal.proposal.ring_id,
                    keys,
                    pin,
                    None,
                    RingTimeouts::default(),
                )
            }
            .map_err(|e| format!("reconciliation_required: {e}"))
        })
    }

    fn reconcile_standalone_adoption_with(
        &self,
        observe: impl FnOnce(
            &StandaloneJournal,
            Arc<xshell_protocol::ring::DeviceKeys>,
            &ChainPin,
        ) -> Result<RosterChain, String>,
    ) -> Result<StandaloneRecoveryStatus, String> {
        let _operation = self.d.store_admission.admit()?;
        let store = Store::new(self.d.cfg.paths.ring_dir.clone());
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(
            Instant::now() + Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
        );
        let (original, keys, durable, current) =
            self.d.ring.with_effective_chain_until(&budget, |current| {
                let _lock = store.lock_until(&budget)?;
                Ok((
                    load_journal(&store)?.ok_or("standalone obligation missing")?,
                    Arc::new(
                        store
                            .load_keys()?
                            .ok_or("retained standalone keys missing")?,
                    ),
                    super::companion::trusted(&store)?,
                    current,
                ))
            })?;
        let known = match (durable, current) {
            (Some(mut durable), Some(current)) => {
                durable.merge_terminal_from(&current);
                Some(
                    durable
                        .extended(current.versions())
                        .map_err(|e| e.to_string())?
                        .0,
                )
            }
            (durable, current) => durable.or(current),
        };
        // Verifiable terminal history is usable even when Relay authentication is refused.
        if let Some(known) = known
            .as_ref()
            .filter(|c| c.get(original.proposal.proposal.version).is_some())
        {
            let mut evidence = original.clone();
            evidence.accepted = known.versions().iter().map(|r| r.token().into()).collect();
            accepted_record(&store, &evidence)?;
            if known.authorized_member(&keys.sign_key()).is_none() {
                return Ok(StandaloneRecoveryStatus {
                    operation: original.intent.operation.clone(),
                    expires_at: original.intent.expires_at,
                    state: StandaloneRecoveryState::Terminal,
                });
            }
        }
        let predecessor = recorded_predecessor(&store, &original)?;
        let pin = ChainPin {
            version: predecessor.head().version(),
            hash: predecessor.head().hash(),
        };
        let observed = observe(&original, keys, &pin)?;
        let accepted = match known {
            Some(known) => {
                let mut accepted = known
                    .extended(observed.versions())
                    .map_err(|e| e.to_string())?
                    .0;
                accepted.merge_terminal_from(&observed);
                accepted
            }
            None => observed,
        };
        let mut record = original.clone();
        record.accepted = accepted
            .versions()
            .iter()
            .map(|r| r.token().into())
            .collect();
        accepted_record(&store, &record)?;
        let terminal = accepted
            .authorized_member(&record.intent.target.principal().sign_key)
            .is_none();
        {
            let _lock = store.lock_until(&budget)?;
            if load_journal(&store)?.as_ref() != Some(&original)
                || !record.accepted.starts_with(&original.accepted)
            {
                return Err("standalone reconciliation obligation changed".into());
            }
            write_private(
                &store.dir.join("standalone-adoption.json"),
                &serde_json::to_vec(&record).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
        if terminal {
            return Ok(StandaloneRecoveryStatus {
                operation: record.intent.operation,
                expires_at: record.intent.expires_at,
                state: StandaloneRecoveryState::Terminal,
            });
        }
        // A historical Installed receipt does not prove that this running owner
        // enforces newly observed cleanup. Reuse the idempotent accepted install.
        self.d
            .ring
            .join_accepted_standalone(&record.accepted, &record.intent.operation, &budget)
            .map_err(|e| format!("reconciliation_required: {e}"))?;
        self.finish_standalone_reconciliation(&store, &mut record, &accepted, &budget)
    }

    fn finish_standalone_reconciliation(
        &self,
        store: &Store,
        record: &mut StandaloneJournal,
        accepted: &RosterChain,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<StandaloneRecoveryStatus, String> {
        if let Some(hook) = &self.d.cfg.test_hook {
            (hook.0)(
                uuid::Uuid::nil(),
                super::TestPoint::StandaloneInstalledBeforeJournal,
            );
        }
        self.d.ring.with_effective_chain_until(budget, |effective| {
            let _lock = store.lock_until(budget)?;
            finalize_standalone_receipt_locked(store, record, accepted, effective, budget)
        })
    }

    /// Runs only for this explicit command invocation. The initial words callback is the
    /// sole output of the credential. A missing ACK after confirmation is reconciliation,
    /// never permission to generate another offer or replace retained membership.
    pub fn adopt_standalone(
        &self,
        relay: &str,
        ttl: Duration,
        cancel: &Arc<AtomicBool>,
        words: impl FnOnce(&DeviceCode),
        confirm: impl FnOnce(
            &StandalonePreview,
            &StandaloneLease,
        ) -> Result<Option<StandaloneConfirmation>, String>,
    ) -> Result<RosterChain, String> {
        if self.d.cfg.gui_bound.is_some() {
            return Err("registered GUI ownership excludes standalone adoption".into());
        }
        let deadline = StandaloneDeadline::new(ttl)?;
        let _operation = self.d.store_admission.admit_standalone(cancel)?;
        StandaloneDriver {
            store: Store::new(self.d.cfg.paths.ring_dir.clone()),
            owner: StandaloneOwner::Runtime(&self.d.ring),
            exiting: Some(&self.d.exiting),
        }
        .run(
            relay,
            &super::ring::host_name(),
            deadline,
            cancel,
            |code| {
                words(code);
                Ok(())
            },
            confirm,
        )
    }
}

#[cfg(debug_assertions)]
fn offline_ownership_cut(
    name: &str,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<(), String> {
    if let Some(path) = std::env::var_os(name) {
        let path = std::path::PathBuf::from(path);
        write_private(&path, b"offline-owner-cut").map_err(|e| e.to_string())?;
        while path.exists() {
            budget.check().map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

/// Command-owned Store transaction; no serve, Desktop signer or Relay Connector is made.
struct StandaloneDeadline {
    until: Instant,
    expires_at: u64,
}
impl StandaloneDeadline {
    fn new(ttl: Duration) -> Result<Self, String> {
        let ttl = ttl.min(Duration::from_secs(600));
        if ttl.as_secs() == 0 {
            return Err("standalone deadline is empty".into());
        }
        Ok(Self {
            until: Instant::now() + ttl,
            expires_at: now() + ttl.as_secs(),
        })
    }
}
enum StandaloneOwner<'a> {
    Offline(super::ring::ownership::StoreOwnershipLease),
    RunningOwner(xshell_core::local_ipc::ProtectedEndpoint),
    Runtime(&'a super::ring::Ring),
}
struct StandaloneDriver<'a> {
    store: Store,
    owner: StandaloneOwner<'a>,
    exiting: Option<&'a AtomicBool>,
}
impl StandaloneDriver<'_> {
    fn transaction<T>(
        &self,
        budget: &xshell_core::local_ipc::RequestBudget,
        change: impl FnOnce(&Store) -> Result<T, String>,
    ) -> Result<T, String> {
        if let StandaloneOwner::Runtime(runtime) = &self.owner {
            return runtime.standalone_transaction(budget, change);
        }
        let refuse_paired_store = || -> Result<(), String> {
            self.store.refuse_registered_join()?;
            if super::companion::trusted(&self.store)?.is_some() {
                return Err(
                    "leave_required: standalone adoption requires an unpaired Daemon".into(),
                );
            }
            Ok(())
        };
        if matches!(self.owner, StandaloneOwner::RunningOwner(_)) {
            {
                let _lock = self.store.lock_until(budget)?;
                // Local denying evidence remains useful even without an owner. Release
                // the Store lock before IPC; this check never grants no-owner admission.
                refuse_paired_store()?;
            }
            let response = self.command_request(
                &xshell_protocol::ring::companion::CompanionRequest::StandaloneCommandReady,
                budget,
            )?;
            if !matches!(
                response,
                xshell_protocol::ring::companion::CompanionResponse::StandaloneCommandReady
            ) {
                return Err("running standalone owner admission unavailable".into());
            }
        }
        let _lock = self.store.lock_until(budget)?;
        refuse_paired_store()?;
        budget.check().map_err(|e| e.to_string())?;
        change(&self.store)
    }
    fn command_request(
        &self,
        request: &xshell_protocol::ring::companion::CompanionRequest,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<xshell_protocol::ring::companion::CompanionResponse, String> {
        let StandaloneOwner::RunningOwner(endpoint) = &self.owner else {
            return Err("running owner route required".into());
        };
        match endpoint
            .exchange_until(request, budget)
            .map_err(|e| format!("running owner unavailable: {e}"))?
        {
            xshell_protocol::ring::companion::CompanionResponse::Error { message } => Err(message),
            response => Ok(response),
        }
    }
    fn prepare(
        &self,
        record: &StandaloneJournal,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<(), String> {
        if matches!(self.owner, StandaloneOwner::RunningOwner(_)) {
            return match self.command_request(
                &xshell_protocol::ring::companion::CompanionRequest::PrepareStandaloneCommand {
                    intent: record.intent.clone(),
                    proposal: Box::new(record.proposal.clone()),
                },
                budget,
            )? {
                xshell_protocol::ring::companion::CompanionResponse::StandalonePrepared {
                    operation,
                } if operation == record.intent.operation => Ok(()),
                _ => Err("standalone preparation receipt missing".into()),
            };
        }
        self.transaction(budget, |store| {
            prepare_standalone_locked(store, record, None, budget)
        })
    }

    fn install(
        &self,
        tokens: &[String],
        operation: &str,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<(), String> {
        if let StandaloneOwner::Runtime(runtime) = &self.owner {
            runtime.join_accepted_standalone(tokens, operation, budget)?;
            runtime.standalone_installed_before_journal();
            let accepted = RosterChain::from_tokens(tokens).map_err(|e| e.to_string())?;
            return finalize_standalone_install(runtime, operation, &accepted, budget);
        }
        let chain = RosterChain::from_tokens(tokens).map_err(|e| e.to_string())?;
        if let StandaloneOwner::Offline(_lease) = &self.owner {
            let saved = self.store.save_join_chain(
                &chain,
                None,
                &super::ring::JoinOrigin::Standalone {
                    operation: operation.into(),
                },
                Some(budget),
                None,
            )?;
            if let Some(error) = saved.conflict.or(saved.write_error) {
                return Err(error);
            }
            return Ok(());
        }
        let reply = self.command_request(
            &xshell_protocol::ring::companion::CompanionRequest::InstallStandaloneAccepted {
                operation: operation.into(),
                rosters: tokens.to_vec(),
            },
            budget,
        )?;
        match reply {
            xshell_protocol::ring::companion::CompanionResponse::Installed { hash }
                if hash == chain.head().hash() =>
            {
                Ok(())
            }
            _ => Err("running owner accepted handoff receipt missing".into()),
        }
    }
    fn run(
        &self,
        relay: &str,
        name: &str,
        deadline: StandaloneDeadline,
        cancel: &Arc<AtomicBool>,
        words: impl FnOnce(&DeviceCode) -> Result<(), String>,
        confirm: impl FnOnce(
            &StandalonePreview,
            &StandaloneLease,
        ) -> Result<Option<StandaloneConfirmation>, String>,
    ) -> Result<RosterChain, String> {
        let StandaloneDeadline { until, expires_at } = deadline;
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(until, cancel.clone());
        let store = &self.store;
        #[cfg(debug_assertions)]
        if matches!(self.owner, StandaloneOwner::Offline(_)) {
            offline_ownership_cut("XSHELLD_TEST_OFFLINE_OWNER_BEFORE_KEYS", &budget)?;
        }
        let _admission = store.reserve_standalone_offer(&budget)?;
        let owner_identity = if matches!(self.owner, StandaloneOwner::RunningOwner(_)) {
            self.transaction(&budget, |_| Ok(()))?;
            match self.command_request(
                &xshell_protocol::ring::companion::CompanionRequest::Descriptor,
                &budget,
            )? {
                xshell_protocol::ring::companion::CompanionResponse::Identity {
                    identity,
                    profile: None,
                } => Some(identity),
                _ => return Err("standalone running owner identity unavailable".into()),
            }
        } else {
            None
        };
        let keys = self.transaction(&budget, |store| {
            let keys = if owner_identity.is_some() {
                store
                    .load_keys()?
                    .ok_or("retained running owner keys missing")?
            } else {
                store.keys_locked()?
            };
            if owner_identity.as_ref().is_some_and(|identity| {
                identity.sign_key != keys.sign_key() || identity.noise_key != keys.noise_key()
            }) {
                return Err("running owner identity changed".into());
            }
            Ok(Arc::new(keys))
        })?;
        let intent = AdoptionIntent {
            operation: xshell_protocol::ring::relay::wire::new_nonce()
                .map_err(|e| e.to_string())?,
            expires_at,
            target: JoinTarget::StandaloneDaemon {
                daemon: PublicIdentity {
                    name: name.into(),
                    sign_key: keys.sign_key(),
                    noise_key: keys.noise_key(),
                },
            },
        };
        let code = DeviceCode::generate().map_err(|e| e.to_string())?;
        let secret = code.secret();
        let mut journal = None;
        let result = adopt_as_guest_with_ready(
            &AdoptionGuestRequest {
                keys: keys.clone(),
                signers: AdoptionSigners {
                    principal: keys.as_ref(),
                    companion: None,
                },
                secret: &secret,
                intent: &intent,
                relay_url: relay,
                approver: None,
                until,
            },
            &PairOptions::default(),
            Some(cancel),
            || words(&code).map_err(xshell_protocol::ring::pairing::PairError::Relay),
            |request| {
                let proposal = CompanionProposal {
                    operation: intent.operation.clone(),
                    predecessor: request
                        .verified
                        .chain
                        .versions()
                        .iter()
                        .map(|r| r.token().into())
                        .collect(),
                    proposal: request.verified.proposal.clone(),
                    transcript: ConsentTranscript::Pairing {
                        final_noise_hash: request.verified.handle.context.clone(),
                    },
                };
                let hash = consent_proposal_hash(&proposal.proposal)
                    .map_err(|e| xshell_protocol::ring::pairing::PairError::Relay(e.to_string()))?;
                let live = Arc::new(AtomicBool::new(true));
                let lease = StandaloneLease {
                    hash: hash.clone(),
                    live: live.clone(),
                    until,
                };
                let decision = confirm(
                    &StandalonePreview {
                        intent: intent.clone(),
                        proposal: proposal.clone(),
                        proposal_hash: hash,
                    },
                    &lease,
                );
                let valid = decision
                    .map_err(xshell_protocol::ring::pairing::PairError::Relay)?
                    .is_some_and(|proof| Arc::ptr_eq(&proof.0, &live));
                live.store(false, Ordering::Release);
                if !valid {
                    return Ok(false);
                }
                if cancel.load(Ordering::Acquire)
                    || Instant::now() >= until
                    || self
                        .exiting
                        .is_some_and(|exiting| exiting.load(Ordering::Acquire))
                {
                    return Err(xshell_protocol::ring::pairing::PairError::Cancelled);
                }
                let record = StandaloneJournal {
                    intent: intent.clone(),
                    proposal,
                    consent_may_have_left: true,
                    accepted: vec![],
                    installed: false,
                };
                // A lost preparation reply cannot authorize a signing retry or fresh offer.
                journal = Some(record.clone());
                self.prepare(&record, &budget)
                    .map_err(xshell_protocol::ring::pairing::PairError::Relay)?;
                Ok(true)
            },
        );
        let (chain, _) = result.map_err(|e| {
            if journal.is_some() {
                format!("reconciliation_required: {e}")
            } else {
                e.to_string()
            }
        })?;
        let mut record = journal.ok_or("standalone consent receipt missing")?;
        record.accepted = chain.versions().iter().map(|r| r.token().into()).collect();
        let install_budget = xshell_core::local_ipc::RequestBudget::for_operation(
            Instant::now() + Duration::from_secs(5),
            cancel.clone(),
        );
        {
            let _lock = store.lock_until(&install_budget)?;
            let mut prepared = record.clone();
            prepared.accepted.clear();
            if load_journal(store)?.as_ref() != Some(&prepared) {
                return Err("reconciliation_required: standalone prepared record changed".into());
            }
            accepted_record(store, &record)?;
            write_private(
                &store.dir.join("standalone-adoption.json"),
                &serde_json::to_vec(&record).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
        #[cfg(debug_assertions)]
        if matches!(self.owner, StandaloneOwner::Offline(_)) {
            offline_ownership_cut("XSHELLD_TEST_OFFLINE_OWNER_BEFORE_INSTALL", &install_budget)?;
        }
        self.install(&record.accepted, &record.intent.operation, &install_budget)
            .map_err(|e| format!("reconciliation_required: {e}"))?;
        if let StandaloneOwner::Offline(_lease) = &self.owner {
            #[cfg(debug_assertions)]
            offline_ownership_cut("XSHELLD_TEST_OFFLINE_OWNER_BEFORE_RECEIPT", &install_budget)?;
            let _lock = store.lock_until(&install_budget)?;
            validate_standalone_install(store, &record.intent.operation, &chain)?;
            let status = finalize_standalone_receipt_locked(
                store,
                &mut record,
                &chain,
                None,
                &install_budget,
            )?;
            if status.state != StandaloneRecoveryState::Installed {
                return Err("reconciliation_required: standalone installation is terminal".into());
            }
        }
        Ok(chain)
    }
}

pub(crate) fn adopt_standalone_command(
    paths: &crate::paths::Paths,
    relay: &str,
    name: Option<&str>,
    ttl: Duration,
    cancel: &Arc<AtomicBool>,
    words: impl FnOnce(&DeviceCode) -> Result<(), String>,
) -> Result<RosterChain, String> {
    let deadline = StandaloneDeadline::new(ttl)?;
    let directory =
        super::ring::ownership::canonical_store(&paths.ring_dir).map_err(|e| e.to_string())?;
    let store = Store::new(directory.clone());
    // Registered/pending classification has already run, and is repeated at every
    // transaction/commit. Busy is the only route to an actual protected owner.
    let owner = match super::ring::ownership::StoreOwnershipLease::acquire(&directory)? {
        super::ring::ownership::StoreOwnership::Acquired(lease) => StandaloneOwner::Offline(lease),
        super::ring::ownership::StoreOwnership::Busy => StandaloneOwner::RunningOwner(
            xshell_core::local_ipc::ProtectedEndpoint::pin(directory.join("companion.sock"))
                .map_err(|e| format!("running owner unavailable: {e}"))?,
        ),
    };
    StandaloneDriver {
        store,
        owner,
        exiting: None,
    }
    .run(
        relay,
        &name
            .map(|name| xshell_protocol::ring::member_name(name, "Host"))
            .unwrap_or_else(super::ring::host_name),
        deadline,
        cancel,
        words,
        |preview, lease| {
            // Only this explicit standalone command grants consent, after the protocol
            // independently verifies this exact Daemon target and approving proposal.
            if !matches!(preview.intent.target, JoinTarget::StandaloneDaemon { .. }) {
                return Err("command cannot authorize Computer consent".into());
            }
            lease.confirm(&preview.proposal_hash).map(Some)
        },
    )
}

#[cfg(test)]
mod reconciliation_tests;

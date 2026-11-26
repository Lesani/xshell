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
use xshell_protocol::ring::pairing::PairError;
use xshell_protocol::ring::relay::pair::{
    adopt_as_guest_with_ready, AdoptionGuestRequest, PairOptions,
};
use xshell_protocol::ring::relay::{ChainPin, Connector, RingClient, RingTimeouts};
use xshell_protocol::ring::{DeviceKeys, RingError, RosterChain};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The accepted chain is observed from the journaled predecessor's head.
fn standalone_pin(store: &Store, record: &StandaloneJournal) -> Result<ChainPin, String> {
    let predecessor = recorded_predecessor(store, record)?;
    Ok(ChainPin {
        version: predecessor.head().version(),
        hash: predecessor.head().hash(),
    })
}

/// Observe only: the accepted chain from the Relay (through the owner's push link when one
/// exists), authenticated with this Daemon's retained keys. Never signs, publishes or opens
/// a pairing slot. The connection attempt never outlives `until`.
fn observe_standalone_accepted(
    link: Option<Arc<Connector>>,
    record: &StandaloneJournal,
    keys: Arc<DeviceKeys>,
    pin: &ChainPin,
    until: Instant,
) -> Result<RosterChain, RingError> {
    if let Some(owner) = link {
        return owner.read_accepted_chain(pin);
    }
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(RingError::Timeout);
    }
    let defaults = RingTimeouts::default();
    RingClient::fetch_chain(
        &record.proposal.proposal.relay_url,
        &record.proposal.proposal.ring_id,
        keys,
        pin,
        None,
        RingTimeouts {
            connect: defaults.connect.min(left),
            ..defaults
        },
    )
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
        Standalone { d: &self.d }.standalone_adoption_status()
    }

    /// Observe only, then retry the same accepted local obligation. No Pair Open, proof
    /// release, signed publication or fresh enrollment occurs on this path.
    pub fn reconcile_standalone_adoption(&self) -> Result<StandaloneRecoveryStatus, String> {
        Standalone { d: &self.d }.reconcile_standalone_adoption()
    }

    #[cfg(test)]
    fn reconcile_standalone_adoption_with(
        &self,
        observe: impl FnOnce(
            &StandaloneJournal,
            Arc<xshell_protocol::ring::DeviceKeys>,
            &ChainPin,
        ) -> Result<RosterChain, String>,
    ) -> Result<StandaloneRecoveryStatus, String> {
        Standalone { d: &self.d }.reconcile_standalone_adoption_with(observe)
    }

    #[cfg(test)]
    fn finish_standalone_reconciliation(
        &self,
        store: &Store,
        record: &mut StandaloneJournal,
        accepted: &RosterChain,
        budget: &xshell_core::local_ipc::RequestBudget,
    ) -> Result<StandaloneRecoveryStatus, String> {
        Standalone { d: &self.d }.finish_standalone_reconciliation(store, record, accepted, budget)
    }
}

/// The standalone obligation's recovery on a borrowed Daemon (no handle whose drop would
/// shut the Daemon down): used by the handle and by the maintenance loop.
pub(crate) struct Standalone<'a> {
    pub(crate) d: &'a Arc<super::registry::Daemon>,
}

impl Standalone<'_> {
    pub(crate) fn standalone_adoption_status(
        &self,
    ) -> Result<Option<StandaloneRecoveryStatus>, String> {
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

    pub(crate) fn reconcile_standalone_adoption(&self) -> Result<StandaloneRecoveryStatus, String> {
        self.reconcile_standalone_adoption_with(|original, keys, pin| {
            observe_standalone_accepted(
                self.d.ring.push_link().map(|(owner, _, _)| owner),
                original,
                keys,
                pin,
                Instant::now() + RingTimeouts::default().connect,
            )
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
        let pin = standalone_pin(&store, &original)?;
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
}

impl ServerHandle {
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
            {
                let mut words = Some(words);
                move |event| {
                    if let StandaloneEvent::Words(code) = event {
                        if let Some(words) = words.take() {
                            words(code);
                        }
                    }
                    Ok(())
                }
            },
            confirm,
        )
        .map_err(StandaloneCommandError::into_message)
    }
}

/// What the standalone command reports while it runs.
pub(crate) enum StandaloneEvent<'a> {
    /// The Device code words, once, when the Relay holds the slot.
    Words(&'a DeviceCode),
    /// The connection was lost after consent; the command keeps observing (once).
    Reconnecting,
}

/// How a standalone command ended without a membership.
#[derive(Debug)]
pub(crate) enum StandaloneCommandError {
    /// Consent may have left this machine and the obligation is retained: the Daemon
    /// (running now, or when it next runs) finishes it. Never a new offer.
    Unfinished { daemon_running: bool, cause: String },
    /// Nothing is pending on this command's behalf (or it ended as the approver declined).
    Failed(String),
}
impl StandaloneCommandError {
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Unfinished { cause, .. } | Self::Failed(cause) => cause,
        }
    }
}
impl From<String> for StandaloneCommandError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}
impl From<&str> for StandaloneCommandError {
    fn from(error: &str) -> Self {
        Self::Failed(error.into())
    }
}

/// The command's observation backoff after consent: 250 ms, doubling, at most 2 s.
const OBSERVE_FIRST: Duration = Duration::from_millis(250);
const OBSERVE_MAX: Duration = Duration::from_secs(2);
/// After the approver's typed refusal, how long the command still looks for a publication.
const OBSERVE_AFTER_REFUSAL: Duration = Duration::from_secs(30);
/// One observation's connection budget: Ctrl-C is honoured at the latest after this.
const OBSERVE_ATTEMPT: Duration = Duration::from_secs(5);

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
    fn daemon_running(&self) -> bool {
        !matches!(self.owner, StandaloneOwner::Offline(_))
    }
    fn stopping(&self, cancel: &AtomicBool) -> bool {
        cancel.load(Ordering::Acquire)
            || self
                .exiting
                .is_some_and(|exiting| exiting.load(Ordering::Acquire))
    }
    fn unfinished(&self, cause: String) -> StandaloneCommandError {
        StandaloneCommandError::Unfinished {
            daemon_running: self.daemon_running(),
            cause,
        }
    }

    /// After consent may have left, a lost connection is not final: observe the accepted
    /// chain (the same observation as reconciliation) with backoff until it lists this
    /// Daemon, within the original lifetime. Never signs again, never opens a new slot,
    /// never extends `until`. Every Relay refusal before the deadline is retried; only a
    /// verified chain whose accepted Enroll no longer authorizes this Daemon, cancellation
    /// and the deadline end it. After the approver's typed refusal it looks for at most
    /// [`OBSERVE_AFTER_REFUSAL`], then ends as declined.
    fn observe_after_consent(
        &self,
        record: &StandaloneJournal,
        keys: &Arc<DeviceKeys>,
        until: Instant,
        cancel: &AtomicBool,
        error: &PairError,
        mut reconnecting: impl FnMut(),
    ) -> Result<RosterChain, StandaloneCommandError> {
        let unfinished =
            |e: &dyn std::fmt::Display| self.unfinished(format!("reconciliation_required: {e}"));
        if matches!(error, PairError::Cancelled) || self.stopping(cancel) || Instant::now() >= until
        {
            return Err(unfinished(error));
        }
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(
            until,
            Arc::new(AtomicBool::new(false)),
        );
        let pin = {
            let _lock = self
                .store
                .lock_until(&budget)
                .map_err(|_| unfinished(error))?;
            if load_journal(&self.store).ok().flatten().as_ref() != Some(record) {
                return Err(unfinished(error));
            }
            standalone_pin(&self.store, record).map_err(|_| unfinished(error))?
        };
        let refused = matches!(error, PairError::Refused(_));
        let stop = if refused {
            until.min(Instant::now() + OBSERVE_AFTER_REFUSAL)
        } else {
            until
        };
        let link = match &self.owner {
            StandaloneOwner::Runtime(runtime) => runtime.push_link().map(|(owner, _, _)| owner),
            _ => None,
        };
        reconnecting();
        let mut delay = OBSERVE_FIRST;
        let mut last = error.to_string();
        loop {
            if self.stopping(cancel) {
                return Err(unfinished(&last));
            }
            if Instant::now() >= stop {
                return Err(if refused && Instant::now() < until {
                    StandaloneCommandError::Failed(format!("reconciliation_required: {error}"))
                } else {
                    unfinished(&last)
                });
            }
            let observed = observe_standalone_accepted(
                link.clone(),
                record,
                keys.clone(),
                &pin,
                stop.min(Instant::now() + OBSERVE_ATTEMPT),
            );
            // An answer that arrives after the deadline or a cancellation is not used.
            if self.stopping(cancel) || Instant::now() >= until {
                return Err(unfinished(&last));
            }
            match observed {
                Ok(chain) => {
                    let mut candidate = record.clone();
                    candidate.accepted =
                        chain.versions().iter().map(|r| r.token().into()).collect();
                    if accepted_record(&self.store, &candidate).is_ok() {
                        if chain.authorized_member(&keys.sign_key()).is_some() {
                            return Ok(chain);
                        }
                        // Signed evidence: the accepted Enroll is followed by this Daemon's
                        // removal. Terminal; the Daemon abandons the obligation.
                        return Err(StandaloneCommandError::Failed(
                            "reconciliation_required: the accepted Ring no longer lists this machine"
                                .into(),
                        ));
                    }
                    last = "the accepted Ring does not list this machine yet".into();
                }
                Err(e) => last = e.to_string(),
            }
            let wake = Instant::now() + delay;
            while Instant::now() < wake.min(stop) && !self.stopping(cancel) {
                std::thread::sleep(
                    Duration::from_millis(50).min(wake.saturating_duration_since(Instant::now())),
                );
            }
            delay = (delay * 2).min(OBSERVE_MAX);
        }
    }

    fn run(
        &self,
        relay: &str,
        name: &str,
        deadline: StandaloneDeadline,
        cancel: &Arc<AtomicBool>,
        mut events: impl FnMut(StandaloneEvent<'_>) -> Result<(), String>,
        confirm: impl FnOnce(
            &StandalonePreview,
            &StandaloneLease,
        ) -> Result<Option<StandaloneConfirmation>, String>,
    ) -> Result<RosterChain, StandaloneCommandError> {
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
        // Set once this command's preparation is durable: only then can a proof leave.
        let mut prepared = false;
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
            || events(StandaloneEvent::Words(&code)).map_err(PairError::Relay),
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
                prepared = true;
                Ok(true)
            },
        );
        let chain = match result {
            Ok((chain, _)) => chain,
            Err(error) => match journal.as_ref() {
                Some(record) if prepared => {
                    self.observe_after_consent(record, &keys, until, cancel, &error, || {
                        let _ = events(StandaloneEvent::Reconnecting);
                    })?
                }
                Some(_) => {
                    return Err(StandaloneCommandError::Failed(format!(
                        "reconciliation_required: {error}"
                    )))
                }
                None => return Err(StandaloneCommandError::Failed(error.to_string())),
            },
        };
        let record = journal.ok_or("standalone consent receipt missing")?;
        self.install_accepted(record, chain, cancel)
            .map_err(|cause| self.unfinished(cause))
    }

    /// The local tail after an accepted chain: journal it, install it through this route
    /// and write the final receipt. Its own 5 s budget is local completion only (no network
    /// use) and is the only work allowed past the command's deadline.
    fn install_accepted(
        &self,
        mut record: StandaloneJournal,
        chain: RosterChain,
        cancel: &Arc<AtomicBool>,
    ) -> Result<RosterChain, String> {
        let store = &self.store;
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
    events: impl FnMut(StandaloneEvent<'_>) -> Result<(), String>,
) -> Result<RosterChain, StandaloneCommandError> {
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
        events,
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

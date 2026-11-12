//! Explicit command-scoped standalone adoption. No Host RPC can invoke or confirm this
//! controller; registration (including a stopped Desktop) always excludes it.
use super::ring::{read_private, write_private, Store};
use super::ServerHandle;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::membership::consent_proposal_hash;
use xshell_protocol::ring::pairing::adoption::{
    AdoptionIntent, AdoptionSigners, JoinTarget, PublicIdentity,
};
use xshell_protocol::ring::relay::pair::{adopt_as_guest, AdoptionGuestRequest, PairOptions};
use xshell_protocol::ring::RosterChain;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StandaloneJournal {
    intent: AdoptionIntent,
    proposal: CompanionProposal,
    consent_may_have_left: bool,
    accepted: Vec<String>,
    installed: bool,
}
fn load_journal(store: &Store) -> Result<Option<StandaloneJournal>, String> {
    read_private(&store.dir.join("standalone-adoption.json"))
        .map_err(|e| e.to_string())?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|e| {
                format!("reconciliation_required: unreadable standalone obligation: {e}")
            })
        })
        .transpose()
}
fn accepted_record(store: &Store, record: &StandaloneJournal) -> Result<RosterChain, String> {
    record.intent.validate().map_err(|e| e.to_string())?;
    if !matches!(
        record.proposal.transcript,
        ConsentTranscript::Pairing { .. }
    ) || record.proposal.predecessor.len() as u64 + 1 != record.proposal.proposal.version
    {
        return Err("standalone recorded transcript/predecessor mismatch".into());
    }
    if !record.consent_may_have_left {
        return Err("standalone consent receipt missing".into());
    }
    let chain = RosterChain::from_tokens(&record.accepted).map_err(|e| e.to_string())?;
    let keys = store
        .load_keys()?
        .ok_or("retained standalone keys missing")?;
    let JoinTarget::StandaloneDaemon { daemon } = &record.intent.target else {
        return Err("not a standalone obligation".into());
    };
    if daemon.sign_key != keys.sign_key()
        || daemon.noise_key != keys.noise_key()
        || record.proposal.operation != record.intent.operation
        || record
            .proposal
            .predecessor
            .iter()
            .enumerate()
            .any(|(i, token)| chain.versions().get(i).map(|r| r.token()) != Some(token.as_str()))
    {
        return Err("standalone retained identity/predecessor changed".into());
    }
    let signed = chain
        .get(record.proposal.proposal.version)
        .ok_or("standalone accepted transition missing")?;
    let mut unsigned = signed.roster().clone();
    let mut membership = unsigned.membership().map_err(|e| e.to_string())?;
    let xshell_protocol::ring::membership::RosterChange::Enroll {
        proofs,
        enrollment_id,
    } = &mut membership.change
    else {
        return Err("standalone accepted transition is not Enroll".into());
    };
    if enrollment_id != &record.intent.operation {
        return Err("standalone accepted operation changed".into());
    }
    let context = record.proposal.context()?;
    if proofs.len() != 1
        || proofs[0].key != keys.sign_key()
        || proofs[0].context != context
        || proofs[0].expires_at != record.intent.expires_at
    {
        return Err("standalone accepted proof differs from recorded consent".into());
    }
    proofs.clear();
    unsigned.set_membership(&membership);
    if unsigned != record.proposal.proposal {
        return Err("standalone accepted proposal changed".into());
    }
    record
        .intent
        .target
        .matches_proposal(&unsigned)
        .map_err(|e| e.to_string())?;
    Ok(chain)
}
/// Shared admission/commit fence. Expiry and process death cannot turn possible consent
/// into permission to establish another membership. Only a verified durable installation
/// permits the later registered-counterpart completion path.
pub(super) fn refuse_unresolved_standalone(store: &Store) -> Result<(), String> {
    let Some(record) = load_journal(store)? else {
        return Ok(());
    };
    let check = || -> Result<(), String> {
        if !record.installed {
            return Err("standalone consent may have left".into());
        }
        let accepted = accepted_record(store, &record)?;
        let durable =
            super::companion::trusted(store)?.ok_or("standalone installation is not durable")?;
        let merged = accepted
            .extended(durable.versions())
            .map_err(|e| e.to_string())?
            .0;
        if merged.head() != durable.head()
            || durable
                .authorized_member(
                    &store
                        .load_keys()?
                        .ok_or("standalone keys missing")?
                        .sign_key(),
                )
                .is_none()
        {
            return Err("standalone installed membership is terminal or changed".into());
        }
        Ok(())
    };
    check().map_err(|e| format!("reconciliation_required: {e}"))
}
/// Internal accepted-operation exception, never selected by a Host request or boolean.
pub(super) fn validate_standalone_install(
    store: &Store,
    operation: &str,
    chain: &RosterChain,
) -> Result<(), String> {
    if super::ring::companion_registered(&store.dir) {
        return Err("registered counterpart excludes standalone installation".into());
    }
    let record = load_journal(store)?.ok_or("standalone accepted obligation missing")?;
    let accepted = accepted_record(store, &record)?;
    if record.intent.operation != operation || accepted.versions() != chain.versions() {
        return Err("standalone installation is not its exact accepted operation".into());
    }
    Ok(())
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
        let ttl = ttl.min(Duration::from_secs(600));
        if ttl.as_secs() == 0 {
            return Err("standalone deadline is empty".into());
        }
        let until = Instant::now() + ttl;
        let expires_at = now() + ttl.as_secs();
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(until, cancel.clone());
        let store = Store::new(self.d.cfg.paths.ring_dir.clone());
        let _admission = store.reserve_standalone_offer(&budget)?;
        let keys = self
            .d
            .ring
            .standalone_transaction(&budget, |store| store.keys_locked().map(Arc::new))?;
        let intent = AdoptionIntent {
            operation: xshell_protocol::ring::relay::wire::new_nonce()
                .map_err(|e| e.to_string())?,
            expires_at,
            target: JoinTarget::StandaloneDaemon {
                daemon: PublicIdentity {
                    name: super::ring::host_name(),
                    sign_key: keys.sign_key(),
                    noise_key: keys.noise_key(),
                },
            },
        };
        let code = DeviceCode::generate().map_err(|e| e.to_string())?;
        let secret = code.secret();
        words(&code);
        let mut journal = None;
        let result = adopt_as_guest(
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
                    || self.d.exiting.load(Ordering::Acquire)
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
                self.d
                    .ring
                    .standalone_transaction(&budget, |store| {
                        budget.check().map_err(|e| e.to_string())?;
                        write_private(
                            &store.dir.join("standalone-adoption.json"),
                            &serde_json::to_vec(&record).expect("journal serializes"),
                        )
                        .map_err(|e| e.to_string())
                    })
                    .map_err(xshell_protocol::ring::pairing::PairError::Relay)?;
                journal = Some(record);
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
            write_private(
                &store.dir.join("standalone-adoption.json"),
                &serde_json::to_vec(&record).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
        self.d
            .ring
            .join_accepted_standalone(&record.accepted, &record.intent.operation, &install_budget)
            .map_err(|e| format!("reconciliation_required: {e}"))?;
        let _lock = store.lock_until(&install_budget)?;
        let durable =
            super::companion::trusted(&store)?.ok_or("standalone durable installation missing")?;
        if durable.head() != chain.head() {
            return Err("standalone durable head changed".into());
        }
        record.installed = true;
        write_private(
            &store.dir.join("standalone-adoption.json"),
            &serde_json::to_vec(&record).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(chain)
    }
}

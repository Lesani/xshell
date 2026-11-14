//! Explicit command-scoped standalone adoption. No Host RPC can invoke or confirm this
//! controller; registration (including a stopped Desktop) always excludes it.
use super::ring::{write_private, Store};
use super::standalone_journal::StandaloneJournal;
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
use xshell_protocol::ring::relay::pair::{adopt_as_guest, AdoptionGuestRequest, PairOptions};
use xshell_protocol::ring::RosterChain;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

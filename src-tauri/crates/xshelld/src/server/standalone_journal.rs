//! Durable standalone uncertainty fences are shared even where standalone commands are unsupported.
use super::ring::{read_private, PrivateRingFile, Store};
use serde::{Deserialize, Serialize};
use xshell_protocol::ring::companion::{CompanionProposal, ConsentTranscript};
use xshell_protocol::ring::pairing::adoption::{AdoptionIntent, JoinTarget};
use xshell_protocol::ring::RosterChain;
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct StandaloneJournal {
    pub(super) intent: AdoptionIntent,
    pub(super) proposal: CompanionProposal,
    pub(super) consent_may_have_left: bool,
    pub(super) accepted: Vec<String>,
    pub(super) installed: bool,
}
pub(super) fn load_journal(store: &Store) -> Result<Option<StandaloneJournal>, String> {
    read_private(
        &store.dir.join("standalone-adoption.json"),
        PrivateRingFile::StandaloneJournal,
    )
    .map_err(|e| e.to_string())?
    .map(|bytes| {
        serde_json::from_slice(&bytes)
            .map_err(|e| format!("reconciliation_required: unreadable standalone obligation: {e}"))
    })
    .transpose()
}
pub(super) fn recorded_predecessor(
    store: &Store,
    record: &StandaloneJournal,
) -> Result<RosterChain, String> {
    record.intent.validate().map_err(|e| e.to_string())?;
    let predecessor =
        RosterChain::from_tokens(&record.proposal.predecessor).map_err(|e| e.to_string())?;
    let keys = store
        .load_keys()?
        .ok_or("retained standalone keys missing")?;
    let JoinTarget::StandaloneDaemon { daemon } = &record.intent.target else {
        return Err("not a standalone obligation".into());
    };
    if !record.consent_may_have_left
        || record.proposal.operation != record.intent.operation
        || daemon.sign_key != keys.sign_key()
        || daemon.noise_key != keys.noise_key()
        || !matches!(
            record.proposal.transcript,
            ConsentTranscript::Pairing { .. }
        )
    {
        return Err("standalone retained intent/identity/transcript mismatch".into());
    }
    record.proposal.context()?;
    xshell_protocol::ring::membership::verify_enroll_proposal(
        predecessor.head(),
        &record.proposal.proposal,
    )
    .map_err(|e| e.to_string())?;
    record
        .intent
        .target
        .matches_proposal(&record.proposal.proposal)
        .map_err(|e| e.to_string())?;
    Ok(predecessor)
}
pub(super) fn accepted_record(
    store: &Store,
    record: &StandaloneJournal,
) -> Result<RosterChain, String> {
    recorded_predecessor(store, record)?;
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
/// Standalone commands (`JoinOrigin::Standalone`) exist only on Unix.
#[cfg(unix)]
pub(super) fn validate_standalone_install(
    store: &Store,
    operation: &str,
    chain: &RosterChain,
) -> Result<(), String> {
    if !matches!(std::fs::symlink_metadata(store.dir.join("companion.json")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err("registered counterpart excludes standalone installation".into());
    }
    // Startup reservation is denial for new offers, not registration or authority.
    // Only the exact recorded accepted operation below may finish through it.
    if xshell_core::companion_reservation::read_reservation(&store.dir)
        .map_err(|e| e.to_string())?
        .is_some_and(|reservation| reservation.legacy_outcome_uncertain)
    {
        return Err("uncertain legacy outcome excludes standalone installation".into());
    }
    let record = load_journal(store)?.ok_or("standalone accepted obligation missing")?;
    let accepted = accepted_record(store, &record)?;
    if record.intent.operation != operation || accepted.versions() != chain.versions() {
        return Err("standalone installation is not its exact accepted operation".into());
    }
    Ok(())
}

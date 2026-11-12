//! Retained v2 Daemon-only completion mailbox. Host administration can supply a
//! proposal/result, but only protected counterpart ingress can release local proofs.
use super::companion::{load, save, trusted};
use super::registry::Daemon;
use super::ring::{read_private, write_private, Store};
use serde_json::Value;
use xshell_protocol::ring::companion::*;
use xshell_protocol::ring::membership::{EnrollmentKind, RosterChange};
use xshell_protocol::ring::pairing::adoption::AdoptionIntent;
use xshell_protocol::ring::{Role, RosterChain, SignKey};
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn read(store: &Store) -> Result<Option<CompletionPending>, String> {
    read_private(&store.dir.join("completion.json"))
        .map_err(|e| e.to_string())?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|e| format!("unreadable completion: {e}"))
        })
        .transpose()
}
fn write(store: &Store, pending: &CompletionPending) -> Result<(), String> {
    write_private(
        &store.dir.join("completion.json"),
        &serde_json::to_vec(pending).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
pub(super) fn start(
    store: &Store,
    policy: &mut CompanionPolicy,
    intent: AdoptionIntent,
) -> Result<CompanionResponse, String> {
    let chain = trusted(store)?.ok_or("completion needs retained membership")?;
    let profile = policy.profile();
    let member = chain
        .authorized_member(&profile.daemon.sign_key)
        .ok_or("retained Daemon is revoked")?;
    let membership = chain
        .head()
        .roster()
        .membership()
        .map_err(|e| e.to_string())?;
    if chain.head().roster().v != 2
        || member.noise_key != profile.daemon.noise_key
        || membership
            .unit_for(&member.sign_key)
            .is_none_or(|unit| unit.kind != EnrollmentKind::Daemon)
        || chain.head().member(&profile.desktop.sign_key).is_some()
    {
        return Err("only retained v2 Daemon-only completion is supported".into());
    }
    if let Some(old) = read(store)? {
        if policy.phase(&old.intent.operation)? != CompanionPhase::Cancelled {
            return Err("reconciliation_required: completion already pending".into());
        }
    }
    policy.start(intent.clone(), now())?;
    // Persist the matching local obligation first. A mailbox write failure is not success.
    save(store, policy)?;
    let pending = CompletionPending {
        intent,
        predecessor: chain.versions().iter().map(|r| r.token().into()).collect(),
        proposal: None,
        proofs: None,
        accepted: vec![],
    };
    write(store, &pending)?;
    Ok(CompanionResponse::Completion {
        pending: Some(Box::new(pending)),
    })
}
/// Local registered Desktop bootstrap: preparation trust only, not a Host RPC and
/// not an installation. The policy repeats the identity/head checks before consent.
pub(super) fn start_retained(
    store: &Store,
    policy: &mut CompanionPolicy,
    intent: AdoptionIntent,
    predecessor: Vec<String>,
) -> Result<CompanionResponse, String> {
    super::adoption::refuse_unresolved_standalone(store)?;
    if predecessor.len() > 4096
        || predecessor.iter().map(String::len).sum::<usize>() > 16 * 1024 * 1024
    {
        return Err("retained completion predecessor bounds".into());
    }
    let recorded = RosterChain::from_tokens(&predecessor).map_err(|e| e.to_string())?;
    let retained = trusted(store)?;
    policy.completion_trust(Some(&recorded), retained.as_ref())?;
    let membership = recorded
        .head()
        .roster()
        .membership()
        .map_err(|e| e.to_string())?;
    for identity in [&policy.profile().desktop, &policy.profile().daemon] {
        if let Some(member) = recorded.head().member(&identity.sign_key) {
            if recorded.authorized_member(&identity.sign_key).is_none()
                || member.noise_key != identity.noise_key
                || membership.unit_for(&identity.sign_key).is_none_or(|unit| {
                    !matches!(
                        unit.kind,
                        EnrollmentKind::LegacyDesktop | EnrollmentKind::Daemon
                    )
                })
            {
                return Err("retained completion requires authorized matching singletons".into());
            }
        }
    }
    if let Some(old) = read(store)? {
        if policy.phase(&old.intent.operation)? != CompanionPhase::Cancelled {
            return Err("reconciliation_required: completion already pending".into());
        }
    }
    policy.start(intent.clone(), now())?;
    save(store, policy)?;
    let pending = CompletionPending {
        intent,
        predecessor,
        proposal: None,
        proofs: None,
        accepted: vec![],
    };
    write(store, &pending)?;
    Ok(CompanionResponse::Completion {
        pending: Some(Box::new(pending)),
    })
}
pub(super) fn prepared(store: &Store, proposal: &CompanionProposal) -> Result<(), String> {
    let Some(mut pending) = read(store)? else {
        return Ok(());
    };
    if pending.intent.operation != proposal.operation {
        return Ok(());
    }
    if pending.proposal.as_ref().is_some_and(|old| old != proposal) {
        return Err("completion proposal already frozen".into());
    }
    pending.proposal = Some(proposal.clone());
    write(store, &pending)
}
pub(super) fn status(
    store: &Store,
    policy: &CompanionPolicy,
) -> Result<Option<CompletionPending>, String> {
    let Some(mut pending) = read(store)? else {
        return Ok(None);
    };
    if policy.phase(&pending.intent.operation)? == CompanionPhase::Cancelled {
        return Ok(None);
    }
    pending.proofs = pending
        .proposal
        .as_ref()
        .and_then(|p| policy.completion_proofs(p));
    Ok(Some(pending))
}
pub(super) fn admin(
    daemon: &Daemon,
    peer: Option<SignKey>,
    request: CompanionAdminRequest,
) -> Result<Value, String> {
    let store = Store::new(daemon.cfg.paths.ring_dir.clone());
    let live = daemon.ring.trusted_chain();
    let _lock = store.lock()?;
    let chain = trusted(&store)?.ok_or("retained membership missing")?;
    if live.as_ref().is_none_or(|live| live.head() != chain.head()) {
        return Err("retained membership is not durably current".into());
    }
    let peer = peer.ok_or("authorized Ring Desktop required")?;
    let member = chain
        .authorized_member(&peer)
        .ok_or("approver is revoked")?;
    if member.role != Role::Desktop
        || chain
            .head()
            .roster()
            .membership()
            .map_err(|e| e.to_string())?
            .unit_for(&peer)
            .is_none_or(|unit| unit.kind != EnrollmentKind::Computer)
    {
        return Err("only an authorized Computer can approve completion".into());
    }
    let policy = load(&store)?.ok_or("no registered local completion")?;
    let mut pending = status(&store, &policy)?.ok_or("no matching local pending completion")?;
    match request {
        CompanionAdminRequest::Pending => {}
        CompanionAdminRequest::Propose { proposal } => {
            if proposal.operation != pending.intent.operation
                || proposal.proposal.signed_by != peer
                || !matches!(proposal.transcript, ConsentTranscript::Local)
                || pending.proofs.is_some()
                || !pending.accepted.is_empty()
            {
                return Err("completion proposal does not match the local pending intent".into());
            }
            let membership = proposal.proposal.membership().map_err(|e| e.to_string())?;
            let old = chain
                .head()
                .roster()
                .membership()
                .map_err(|e| e.to_string())?;
            let unit = old
                .unit_for(&policy.profile().daemon.sign_key)
                .ok_or("retained singleton missing")?;
            if !matches!(&membership.change, RosterChange::CompleteComputer { replaces, .. } if replaces == &vec![unit.id.clone()])
            {
                return Err("completion may retire only its retained Daemon singleton".into());
            }
            if pending
                .proposal
                .as_ref()
                .is_some_and(|old| old != proposal.as_ref())
            {
                return Err(
                    "proposal already frozen; fresh local intent/confirmation required".into(),
                );
            }
            let mut checked = policy.clone();
            checked.prepare(*proposal.clone(), Some(&chain), now())?;
            pending.proposal = Some(*proposal);
            write(&store, &pending)?;
        }
        CompanionAdminRequest::Proofs { operation } => {
            if operation != pending.intent.operation {
                return Err("completion operation mismatch".into());
            }
        }
        CompanionAdminRequest::Accepted { operation, rosters } => {
            if operation != pending.intent.operation {
                return Err("completion operation mismatch".into());
            }
            let accepted = RosterChain::from_tokens(&rosters).map_err(|e| e.to_string())?;
            let mut checked = policy.clone();
            let current = checked.accept_chain(&operation, &accepted, Some(&chain))?;
            pending.accepted = current
                .versions()
                .iter()
                .map(|r| r.token().into())
                .collect();
            write(&store, &pending)?;
        }
    }
    serde_json::to_value(Some(pending)).map_err(|e| e.to_string())
}

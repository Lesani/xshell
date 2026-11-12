//! Protected counterpart ingress, separate from Host/Relay/Mobile protocols. Registration
//! survives endpoint/process loss. Only this Daemon's store and keys are ever written here.
#![cfg(unix)]
use super::registry::Daemon;
use super::ring::{read_private, write_private, Store};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use xshell_protocol::ring::companion::*;
use xshell_protocol::ring::pairing::adoption::PublicIdentity;
use xshell_protocol::ring::{verify, DeviceKeys, RosterChain};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn load(store: &Store) -> Result<Option<CompanionPolicy>, String> {
    read_private(&store.dir.join("companion.json"))
        .map_err(|e| e.to_string())?
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|e| format!("unreadable companion registration: {e}"))
        })
        .transpose()
}
fn save(store: &Store, policy: &CompanionPolicy) -> Result<(), String> {
    write_private(
        &store.dir.join("companion.json"),
        &serde_json::to_vec(policy).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
fn trusted(store: &Store) -> Result<Option<RosterChain>, String> {
    let chain = store.read_chain()?;
    if chain.is_none()
        && std::fs::read_dir(&store.dir)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("roster.json.bad-")
            })
    {
        return Err("retained roster requires explicit recovery, not genesis".into());
    }
    Ok(chain)
}
fn validate_profile(
    store: &Store,
    profile: &CompanionProfile,
    keys: &DeviceKeys,
) -> Result<(), String> {
    profile.validate()?;
    let daemon_dir = store.dir.canonicalize().map_err(|e| e.to_string())?;
    if Path::new(&profile.daemon_store) != daemon_dir
        || Path::new(&profile.endpoint) != daemon_dir.join("companion.sock")
        || profile.daemon.sign_key != keys.sign_key()
        || profile.daemon.noise_key != keys.noise_key()
    {
        return Err("companion Daemon/profile/endpoint association mismatch".into());
    }
    xshell_core::local_ipc::private_dir(Path::new(&profile.desktop_store))
        .map_err(|e| e.to_string())?;
    if profile.desktop_store == profile.daemon_store {
        return Err("counterpart stores must be distinct".into());
    }
    Ok(())
}
pub(super) fn validate_install_locked(
    store: &Store,
    operation: &str,
    chain: &RosterChain,
) -> Result<(), String> {
    let mut policy = load(store)?.ok_or("companion is not registered")?;
    let keys = store.load_keys()?.ok_or("retained Daemon keys missing")?;
    validate_profile(store, policy.profile(), &keys)?;
    policy.accept_chain(operation, chain, trusted(store)?.as_ref())?;
    Ok(())
}
fn dispatch(
    daemon: &Arc<Daemon>,
    request: CompanionRequest,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<CompanionResponse, String> {
    budget.check().map_err(|e| e.to_string())?;
    if daemon.exiting.load(Ordering::Acquire) || daemon.stopping.load(Ordering::Acquire) {
        return Err("Daemon is stopping; registration retained".into());
    }
    if let Some(hook) = &daemon.cfg.test_hook {
        (hook.0)(
            uuid::Uuid::nil(),
            super::TestPoint::CompanionRequestAdmitted,
        );
    }
    budget.check().map_err(|e| e.to_string())?;
    let store = Store::new(daemon.cfg.paths.ring_dir.clone());
    if matches!(request, CompanionRequest::Descriptor) {
        let _lock = store.lock_until(budget)?;
        trusted(&store)?;
        let existing = load(&store)?;
        budget.check().map_err(|e| e.to_string())?;
        let keys = match existing.as_ref() {
            Some(_) => store
                .load_keys()?
                .ok_or("registered counterpart has lost its retained keys")?,
            None => store.keys_locked()?,
        };
        if let Some(policy) = &existing {
            validate_profile(&store, policy.profile(), &keys)?;
        }
        return Ok(CompanionResponse::Identity {
            identity: PublicIdentity {
                sign_key: keys.sign_key(),
                noise_key: keys.noise_key(),
                name: super::ring::host_name(),
            },
            profile: existing.map(|policy| policy.profile().clone()),
        });
    }
    if let CompanionRequest::InstallAccepted { operation, rosters } = request {
        let accepted = RosterChain::from_tokens(&rosters).map_err(|e| e.to_string())?;
        {
            let _lock = store.lock_until(budget)?;
            let mut policy = load(&store)?.ok_or("companion is not registered")?;
            let keys = store.load_keys()?.ok_or("retained Daemon keys missing")?;
            validate_profile(&store, policy.profile(), &keys)?;
            policy.accept_chain(&operation, &accepted, trusted(&store)?.as_ref())?;
            // Publication/acceptance is not a durable installation receipt.
            budget.check().map_err(|e| e.to_string())?;
            save(&store, &policy)?;
        }
        daemon
            .ring
            .join_accepted_companion(&rosters, &operation, budget)?;
        let _lock = store.lock_until(budget)?;
        let durable = trusted(&store)?.ok_or("accepted membership was not durably installed")?;
        let mut policy = load(&store)?.ok_or("registration disappeared")?;
        policy.installed(&operation, &durable)?;
        budget.check().map_err(|e| e.to_string())?;
        save(&store, &policy)?;
        return Ok(CompanionResponse::Installed {
            hash: durable.head().hash(),
        });
    }
    let _lock = store.lock_until(budget)?;
    let keys = store
        .load_keys()?
        .ok_or("request counterpart Descriptor before registration")?;
    let mut policy = load(&store)?;
    if let CompanionRequest::Register { profile, proof } = request {
        validate_profile(&store, &profile, &keys)?;
        if !verify(
            &profile.desktop.sign_key,
            &registration_message(&profile),
            &proof,
        ) {
            return Err("companion registration possession proof refused".into());
        }
        if let Some(existing) = &policy {
            if existing.profile() != &profile {
                return Err(
                    "conflicting companion registration; no downgrade or replacement".into(),
                );
            }
        } else {
            let _offer = store.registration_admission()?;
            let registered = CompanionPolicy::new(profile.clone())?;
            budget.check().map_err(|e| e.to_string())?;
            save(&store, &registered)?;
        }
        return Ok(CompanionResponse::Descriptor {
            profile: Some(profile),
        });
    }
    let policy = policy.as_mut().ok_or("companion is not registered")?;
    validate_profile(&store, policy.profile(), &keys)?;
    budget.check().map_err(|e| e.to_string())?;
    let response = match request {
        CompanionRequest::Start { intent } => {
            if trusted(&store)?.is_some() {
                return Err("leave_required: Create requires an unpaired Daemon".into());
            }
            let operation = intent.operation.clone();
            policy.start(intent, now())?;
            CompanionResponse::Status {
                phase: policy.phase(&operation)?,
                operation,
            }
        }
        CompanionRequest::Status { operation } => {
            return Ok(CompanionResponse::Status {
                phase: policy.phase(&operation)?,
                operation,
            })
        }
        CompanionRequest::Cancel { operation } => {
            policy.cancel(&operation)?;
            CompanionResponse::Status {
                phase: policy.phase(&operation)?,
                operation,
            }
        }
        CompanionRequest::Prepare { proposal } => {
            let operation = proposal.operation.clone();
            policy.prepare(*proposal, trusted(&store)?.as_ref(), now())?;
            CompanionResponse::Status {
                phase: policy.phase(&operation)?,
                operation,
            }
        }
        CompanionRequest::Possession {
            operation,
            handshake_hash,
        } => CompanionResponse::Possession {
            signature: policy.possession(
                &keys,
                &operation,
                xshell_protocol::ring::b64::decode_array::<32>(&handshake_hash)
                    .map_err(|e| e.to_string())?,
                now(),
            )?,
        },
        CompanionRequest::Consent {
            operation,
            desktop_proof,
        } => CompanionResponse::Consent {
            proof: policy.consent(&keys, &operation, &desktop_proof, now())?,
        },
        _ => return Err("invalid counterpart operation".into()),
    };
    // No proof is returned to the socket unless the matching obligation is durable.
    budget.check().map_err(|e| e.to_string())?;
    save(&store, policy)?;
    Ok(response)
}
pub(super) fn serve(
    daemon: Weak<Daemon>,
) -> std::io::Result<Option<xshell_core::local_ipc::Server>> {
    let live = daemon
        .upgrade()
        .ok_or_else(|| std::io::Error::other("Daemon disappeared"))?;
    let store = Store::new(live.cfg.paths.ring_dir.clone());
    store.ensure_dir()?;
    let directory = store.dir.canonicalize()?;
    xshell_core::local_ipc::private_dir(&directory)?;
    let path = directory.join("companion.sock");
    if let Err(error) = xshell_core::local_ipc::check_path(&path) {
        // Only this exact absent, overlong optional endpoint is unsupported. Unsafe,
        // conflicting or ambiguous existing paths must still fail the Daemon start.
        if error.kind() == std::io::ErrorKind::Unsupported {
            match std::fs::symlink_metadata(&path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(other) => return Err(other),
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "unsupported companion path is occupied",
                    ))
                }
            }
        }
        return Err(error);
    }
    drop(live);
    xshell_core::local_ipc::serve(path, move |request, budget| {
        daemon
            .upgrade()
            .ok_or_else(|| "Daemon stopped; registration retained".to_string())
            .and_then(|daemon| dispatch(&daemon, request, &budget))
            .unwrap_or_else(|message| CompanionResponse::Error { message })
    })
    .map(Some)
}

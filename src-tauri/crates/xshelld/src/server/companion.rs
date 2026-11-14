//! Protected counterpart ingress, separate from Host/Relay/Mobile protocols. Registration
//! survives endpoint/process loss. Only this Daemon's store and keys are ever written here.
#![cfg(any(unix, windows))]
use super::registry::Daemon;
use super::ring::{read_private, write_private, PrivateRingFile, Store};
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
pub(super) fn load(store: &Store) -> Result<Option<CompanionPolicy>, String> {
    read_private(
        &store.dir.join("companion.json"),
        PrivateRingFile::CompanionPolicy,
    )
    .map_err(|e| e.to_string())?
    .map(|bytes| {
        serde_json::from_slice(&bytes)
            .map_err(|e| format!("unreadable companion registration: {e}"))
    })
    .transpose()
}
pub(super) fn save(store: &Store, policy: &CompanionPolicy) -> Result<(), String> {
    write_private(
        &store.dir.join("companion.json"),
        &serde_json::to_vec(policy).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
pub(super) fn trusted(store: &Store) -> Result<Option<RosterChain>, String> {
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
    let daemon_dir =
        xshell_core::local_ipc::canonical_store(&store.dir).map_err(|e| e.to_string())?;
    if Path::new(&profile.daemon_store) != daemon_dir
        || Path::new(&profile.endpoint)
            != xshell_core::local_ipc::companion_endpoint(&daemon_dir).map_err(|e| e.to_string())?
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
/// Shared by live serve and the bounded registration-only child. Caller holds ring.lock.
fn registration_locked(
    store: &Store,
    request: CompanionRequest,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<CompanionResponse, String> {
    budget.check().map_err(|e| e.to_string())?;
    if let CompanionRequest::ReserveRegistration { desktop_store } = &request {
        // Same deny-only boundary on Unix and Windows (BINDING Decision 2).
        xshell_core::companion_reservation::reserve_locked(
            &store.dir.canonicalize().map_err(|e| e.to_string())?,
            Path::new(desktop_store),
        )
        .map_err(|e| e.to_string())?;
        return Ok(CompanionResponse::Descriptor { profile: None });
    }
    refuse_legacy_uncertainty(store)?;
    if matches!(request, CompanionRequest::Descriptor) {
        trusted(store)?;
        let existing = load(store)?;
        budget.check().map_err(|e| e.to_string())?;
        let keys = match existing.as_ref() {
            Some(_) => store
                .load_keys()?
                .ok_or("registered counterpart has lost its retained keys")?,
            None => store.keys_locked()?,
        };
        if let Some(policy) = &existing {
            validate_profile(store, policy.profile(), &keys)?;
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
    let keys = store
        .load_keys()?
        .ok_or("request counterpart Descriptor before registration")?;
    let policy = load(store)?;
    if let CompanionRequest::Register { profile, proof } = request {
        validate_profile(store, &profile, &keys)?;
        validate_pending(store, &profile.desktop_store)?;
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
            super::standalone_journal::refuse_unresolved_standalone(store)?;
            let registered = CompanionPolicy::new(profile.clone())?;
            budget.check().map_err(|e| e.to_string())?;
            save(store, &registered)?;
        }
        // Commit valid registration durably before removing only its exact denial.
        sync_store_dir(&store.dir)?;
        // Debug-only owned-child crash cut: valid policy is durable, denial still exists.
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("XSHELLD_TEST_REGISTRATION_AFTER_SAVE") {
            let path = std::path::PathBuf::from(path);
            write_private(&path, b"registration-durable-before-denial-clear")
                .map_err(|e| e.to_string())?;
            while path.exists() {
                budget.check().map_err(|e| e.to_string())?;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        budget.check().map_err(|e| e.to_string())?;
        validate_pending(store, &profile.desktop_store)?;
        match std::fs::remove_file(store.dir.join("companion-pending.json")) {
            Ok(()) => sync_store_dir(&store.dir)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        return Ok(CompanionResponse::Descriptor {
            profile: Some(profile),
        });
    }
    Err("registration helper refuses non-registration requests".into())
}
/// Persist a directory entry change. Windows has no std directory handle to flush: the
/// policy rename is written through (`write_private`) and NTFS journals the unlink.
fn sync_store_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}
/// Pending startup reservation (desktop store, legacy uncertainty), on every platform.
struct Reservation {
    desktop_store: String,
    legacy_outcome_uncertain: bool,
}
fn read_reservation(store: &Store) -> Result<Option<Reservation>, String> {
    Ok(
        xshell_core::companion_reservation::read_reservation(&store.dir)
            .map_err(|e| e.to_string())?
            .map(|r| Reservation {
                desktop_store: r.desktop_store,
                legacy_outcome_uncertain: r.legacy_outcome_uncertain,
            }),
    )
}
fn refuse_legacy_uncertainty(store: &Store) -> Result<(), String> {
    if read_reservation(store)?.is_some_and(|reservation| reservation.legacy_outcome_uncertain) {
        return Err("reconciliation_required: incumbent legacy pairing remote outcome is uncertain; registration cannot resolve or clear this denial".into());
    }
    Ok(())
}
fn validate_pending(store: &Store, desktop_store: &str) -> Result<(), String> {
    if let Some(reservation) = read_reservation(store)? {
        if reservation.desktop_store != desktop_store {
            return Err("conflicting pending counterpart registration".into());
        }
        if reservation.legacy_outcome_uncertain {
            return Err(
                "reconciliation_required: incumbent legacy pairing remote outcome is uncertain"
                    .into(),
            );
        }
    }
    Ok(())
}
fn dispatch(
    daemon: &Arc<Daemon>,
    request: CompanionRequest,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<CompanionResponse, String> {
    let _operation = daemon.store_admission.admit()?;
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
    if matches!(
        request,
        CompanionRequest::Descriptor
            | CompanionRequest::Register { .. }
            | CompanionRequest::ReserveRegistration { .. }
    ) {
        let _lock = store.lock_until(budget)?;
        return registration_locked(&store, request, budget);
    }
    // Standalone commands (`xshelld pair` Host-first adoption) exist only on Unix.
    #[cfg(not(unix))]
    if matches!(
        request,
        CompanionRequest::StandaloneCommandReady
            | CompanionRequest::PrepareStandaloneCommand { .. }
            | CompanionRequest::InstallStandaloneAccepted { .. }
    ) {
        return Err("standalone command is not supported on this platform".into());
    }
    #[cfg(unix)]
    if matches!(request, CompanionRequest::StandaloneCommandReady) {
        if daemon.cfg.gui_bound.is_some() {
            return Err("GUI-bound owner excludes standalone command".into());
        }
        return daemon
            .ring
            .standalone_transaction(budget, |_| Ok(CompanionResponse::StandaloneCommandReady));
    }
    #[cfg(unix)]
    if let CompanionRequest::PrepareStandaloneCommand { intent, proposal } = request {
        if daemon.cfg.gui_bound.is_some() {
            return Err("GUI-bound owner excludes standalone command".into());
        }
        let operation = intent.operation.clone();
        if let Some(hook) = &daemon.cfg.test_hook {
            (hook.0)(
                uuid::Uuid::nil(),
                super::TestPoint::StandaloneCommandBeforePrepare,
            );
        }
        super::adoption::prepare_standalone_command(&daemon.ring, intent, *proposal, budget)?;
        if let Some(hook) = &daemon.cfg.test_hook {
            (hook.0)(
                uuid::Uuid::nil(),
                super::TestPoint::StandaloneCommandPreparedBeforeReply,
            );
        }
        return Ok(CompanionResponse::StandalonePrepared { operation });
    }
    if matches!(request, CompanionRequest::PairDriverReady) {
        // No Descriptor fallback: readiness must not create keys or mutate registration.
        return daemon.ring.with_effective_chain_until(budget, |_| {
            let _lock = store.lock_until(budget)?;
            refuse_legacy_uncertainty(&store)?;
            let policy = load(&store)?.ok_or("registered counterpart not ready")?;
            let keys = store.load_keys()?.ok_or("registered Daemon keys missing")?;
            validate_profile(&store, policy.profile(), &keys)?;
            Ok(CompanionResponse::Descriptor {
                profile: Some(policy.profile().clone()),
            })
        });
    }
    if !matches!(
        request,
        CompanionRequest::Status { .. }
            | CompanionRequest::Cancel { .. }
            | CompanionRequest::CompletionStatus
    ) {
        refuse_legacy_uncertainty(&store)?;
    }
    if matches!(request, CompanionRequest::MembershipSnapshot) {
        return daemon.ring.with_effective_chain_until(budget, |live| {
            let _lock = store.lock_until(budget)?;
            let policy = load(&store)?.ok_or("companion is not registered")?;
            let keys = store.load_keys()?.ok_or("retained Daemon keys missing")?;
            validate_profile(&store, policy.profile(), &keys)?;
            let durable = trusted(&store)?;
            let chain = match (live.as_ref(), durable) {
                (Some(live), Some(mut durable)) => {
                    durable.merge_terminal_from(live);
                    durable
                        .extended(live.versions())
                        .map_err(|e| e.to_string())?
                        .0
                }
                (None, None) => {
                    return Ok(CompanionResponse::MembershipSnapshot { rosters: vec![] })
                }
                // A failed roster write can leave verified accepted membership in force.
                // Export evidence, not a durable receipt, so recovery may retry that write.
                (Some(live), None) => live.clone(),
                (None, Some(durable)) => durable,
            };
            if chain.head().roster().v == 2
                && chain
                    .head()
                    .roster()
                    .membership()
                    .map_err(|e| e.to_string())?
                    .enrollments
                    .iter()
                    .any(|unit| chain.terminal_enrollments().contains(&unit.id))
            {
                // This token-only snapshot cannot export independently observed activity
                // evidence. Refuse rather than strip a current terminal fence on recovery.
                return Err(
                    "counterpart has terminal evidence not represented by signed cleanup".into(),
                );
            }
            Ok(CompanionResponse::MembershipSnapshot {
                rosters: chain.versions().iter().map(|r| r.token().into()).collect(),
            })
        });
    }
    #[cfg(unix)]
    if let CompanionRequest::InstallStandaloneAccepted { operation, rosters } = request {
        if daemon.cfg.gui_bound.is_some() {
            return Err("GUI-bound owner excludes standalone installation".into());
        }
        daemon
            .ring
            .join_accepted_standalone(&rosters, &operation, budget)?;
        let accepted = RosterChain::from_tokens(&rosters).map_err(|e| e.to_string())?;
        super::adoption::finalize_standalone_install(&daemon.ring, &operation, &accepted, budget)?;
        return Ok(CompanionResponse::Installed {
            hash: accepted.head().hash(),
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
    // Only retained completion needs this additional current/durable comparison. Taking
    // the runtime membership lock for registration would invert the legacy Host-commit
    // ordering (and a fresh Create has no retained membership to compare).
    let completion_check = matches!(
        &request,
        CompanionRequest::CompletionStart { .. }
            | CompanionRequest::RetainedCompletionStart { .. }
            | CompanionRequest::CheckPrepared { .. }
    ) || matches!(&request, CompanionRequest::Prepare { proposal } if matches!(proposal.transcript, ConsentTranscript::Local) && !proposal.predecessor.is_empty())
        || matches!(&request, CompanionRequest::Consent { .. });
    let live = if completion_check {
        Some(daemon.ring.lock_trusted_chain_until(budget)?)
    } else {
        None
    };
    let _lock = store.lock_until(budget)?;
    let keys = store
        .load_keys()?
        .ok_or("request counterpart Descriptor before registration")?;
    let mut policy = load(&store)?;
    // Decide under ring.lock after reading the obligation, not by mailbox-file
    // existence: a mailbox write failure or concurrent preparation cannot bypass it.
    let retained_check = match &request {
        CompanionRequest::Consent { operation, .. } => {
            policy.as_ref().is_some_and(|p| p.retained_local(operation))
        }
        _ => completion_check,
    };
    if retained_check
        && live
            .as_ref()
            .and_then(|guard| guard.as_ref())
            .map(|c| c.head().hash())
            != trusted(&store)?.as_ref().map(|c| c.head().hash())
    {
        return Err("retained membership is not durably current".into());
    }
    let policy = policy.as_mut().ok_or("companion is not registered")?;
    validate_profile(&store, policy.profile(), &keys)?;
    budget.check().map_err(|e| e.to_string())?;
    let response = match request {
        CompanionRequest::CompletionStart { intent } => {
            super::completion::start(&store, policy, intent)?
        }
        CompanionRequest::RetainedCompletionStart {
            intent,
            predecessor,
        } => super::completion::start_retained(&store, policy, intent, predecessor)?,
        CompanionRequest::CompletionStatus => {
            return Ok(CompanionResponse::Completion {
                pending: super::completion::status(&store, policy)?.map(Box::new),
            });
        }
        CompanionRequest::CheckPrepared { operation } => {
            policy.check_prepared(
                &operation,
                live.as_ref().and_then(|guard| guard.as_ref()),
                now(),
            )?;
            return Ok(CompanionResponse::Status {
                operation,
                phase: CompanionPhase::Prepared,
            });
        }
        CompanionRequest::Start { intent } => {
            super::standalone_journal::refuse_unresolved_standalone(&store)?;
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
            policy.prepare(*proposal.clone(), trusted(&store)?.as_ref(), now())?;
            super::completion::prepared(&store, &proposal)?;
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
        } => {
            if policy.retained_local(&operation) {
                policy.check_prepared(
                    &operation,
                    live.as_ref().and_then(|guard| guard.as_ref()),
                    now(),
                )?;
            }
            CompanionResponse::Consent {
                proof: policy.consent(&keys, &operation, &desktop_proof, now())?,
            }
        }
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
    let directory = xshell_core::local_ipc::canonical_store(&store.dir)?;
    xshell_core::local_ipc::private_dir(&directory)?;
    let path = xshell_core::local_ipc::companion_endpoint(&directory)?;
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

/// Read-only classification for the actual CLI. Unreadable association is an error,
/// never standalone permission; a pending reservation may contain no keys at all.
pub(crate) fn registered_desktop(
    store: &Store,
    budget: &xshell_core::local_ipc::RequestBudget,
) -> Result<String, String> {
    let _lock = store.lock_until(budget)?;
    budget.check().map_err(|e| e.to_string())?;
    if let Some(policy) = load(store)? {
        let keys = store.load_keys()?.ok_or("registered Daemon keys missing")?;
        validate_profile(store, policy.profile(), &keys)?;
        validate_pending(store, &policy.profile().desktop_store)?;
        budget.check().map_err(|e| e.to_string())?;
        return Ok(policy.profile().desktop_store.clone());
    }
    let path = xshell_core::companion_reservation::read_reservation(&store.dir)
        .map_err(|e| e.to_string())?
        .ok_or("registered Computer association is unreadable")?
        .desktop_store;
    xshell_core::local_ipc::private_dir(Path::new(&path)).map_err(|e| e.to_string())?;
    budget.check().map_err(|e| e.to_string())?;
    Ok(path)
}

/// Registration-only command: no Daemon, Host, Relay, signer RPC or membership runtime.
/// stdin owns its lifetime; EOF or the original 15-second bound drains the endpoint.
/// Unix only: Windows has no headless Daemon (ADR-0005), so its Desktop registers in
/// process with the live GUI-bound Daemon it started instead of a registration child.
#[cfg(unix)]
pub fn registration_child(opts: &crate::cli::Opts) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};
    let home = opts
        .home
        .clone()
        .or_else(dirs::home_dir)
        .ok_or("home unavailable")?;
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let mut paths = crate::paths::resolve(&home, xdg.as_deref(), opts.socket.as_deref());
    paths.ring_dir =
        super::ring::ownership::canonical_store(&paths.ring_dir).map_err(|e| e.to_string())?;
    let until = Instant::now() + Duration::from_secs(15);
    let stdin = std::io::stdin();
    let mut bytes = Vec::new();
    let input_until = until.min(Instant::now() + Duration::from_secs(5));
    // Bounded private pipe input; paths/public metadata only, never a seed or capability.
    loop {
        poll_stdin(stdin.as_raw_fd(), input_until)?;
        let mut byte = [0];
        let count = unsafe { libc::read(stdin.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
        if count <= 0 {
            return Err("registration child cancelled or interrupted".into());
        }
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() >= 4096 {
            return Err("registration input too large".into());
        }
        bytes.push(byte[0]);
    }
    let desktop_store: String = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let _ownership = match super::ring::ownership::StoreOwnershipLease::acquire(&paths.ring_dir)? {
        super::ring::ownership::StoreOwnership::Acquired(lease) => lease,
        super::ring::ownership::StoreOwnership::Busy => {
            return Err("registration child cannot own Daemon Store".into())
        }
    };
    crate::paths::ensure_private_dir(paths.lock.parent().ok_or("lock parent missing")?)
        .map_err(|e| e.to_string())?;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&paths.lock)
        .map_err(|e| e.to_string())?;
    let meta = lock.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err("unsafe Daemon owner lock".into());
    }
    lock.try_lock()
        .map_err(|e| format!("registration child cannot own Daemon endpoint: {e}"))?;
    let home = home.canonicalize().map_err(|e| e.to_string())?;
    xshell_core::companion_reservation::reserve(&home, Path::new(&desktop_store))
        .map_err(|e| e.to_string())?;
    let store = Store::new(paths.ring_dir);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let budget = xshell_core::local_ipc::RequestBudget::for_operation(until, stop);
    {
        let _lock = store.lock_until(&budget)?;
        registration_locked(
            &store,
            CompanionRequest::ReserveRegistration { desktop_store },
            &budget,
        )?;
    }
    let endpoint = store
        .dir
        .canonicalize()
        .map_err(|e| e.to_string())?
        .join("companion.sock");
    let server = xshell_core::local_ipc::serve(endpoint, move |request, budget| {
        let result = (|| {
            let _lock = store.lock_until(&budget)?;
            registration_locked(&store, request, &budget)
        })();
        result.unwrap_or_else(|message| CompanionResponse::Error { message })
    })
    .map_err(|e| e.to_string())?;
    // Only EOF is accepted after the initial line. No generic command channel.
    let result = poll_stdin(stdin.as_raw_fd(), until);
    drop(server); // cancel budgets and drain before releasing the endpoint-owner lock
    drop(lock);
    match result {
        Ok(()) => Ok(()),
        Err(e) if Instant::now() >= until => {
            let _ = e;
            Ok(())
        }
        Err(e) => Err(e),
    }
}
#[cfg(unix)]
fn poll_stdin(fd: std::os::fd::RawFd, until: std::time::Instant) -> Result<(), String> {
    let left = until.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Err("registration child deadline".into());
    }
    let mut event = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut event, 1, left.as_millis().min(i32::MAX as u128) as i32) };
    if ready <= 0 {
        return Err("registration child deadline or interrupted".into());
    }
    Ok(())
}

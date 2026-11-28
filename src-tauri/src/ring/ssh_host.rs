//! Add to Ring for a saved SSH Host (#115): the command `ring_add_ssh_host`. The Desktop runs
//! `xshelld pair` on the Host over its SSH route, reads the Device Code from the command's
//! output and approves it itself, bound to the key the Host's `ring.identity` reported. The
//! work is `xshell_hostlink::ring::ssh_host`; this is the Tauri glue: the Host lookup, one
//! attempt at a time, and the sync poke afterwards.
//!
//! The Device Code words never leave Rust: they are not in the reply, an error, an event or
//! a log line. Errors are `code: detail` (`not-ready`, `offline`, `unsupported`, `windows`,
//! `own-desktop`, `other-ring`, `rate-limited`, `unfinished`, `expired`, `service`, `busy`,
//! `failed`); the frontend shows one sentence per code.
//!
//! `ring_ssh_host_keep_running` (#119) makes the Host's Daemon start by itself before the
//! Desktop takes the Host out of its SSH list.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Manager as _};
use xshell_hostlink::ring::ssh_host::{
    add_ssh_host, check_host, keep_running, resolve_binary, AddCode, AddError, AddRequest, Timeouts,
};
use xshell_hostlink::{CancelToken, LOCAL_HOST_ID};

use super::join::{refresh_approval, ApprovedComputer};
use super::{run, worker, HOST_SYNC};

/// How long the Host has to answer `ring.identity`.
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(20);

/// One Add to Ring at a time per Desktop.
static ADDING: AtomicBool = AtomicBool::new(false);

/// Holds [`ADDING`] while an attempt runs.
pub struct AddingGuard(());

impl AddingGuard {
    pub fn take() -> Result<AddingGuard, AddError> {
        ADDING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| AddingGuard(()))
            .map_err(|_| AddError::new(AddCode::Busy, "another Add to Ring is running"))
    }
}

impl Drop for AddingGuard {
    fn drop(&mut self) {
        ADDING.store(false, Ordering::Release);
    }
}

/// A saved SSH Host's id: not the Local Host, not a Ring Host (`r_…`).
pub fn saved_ssh_id(host: &str) -> Result<(), AddError> {
    if host == LOCAL_HOST_ID || host.starts_with("r_") {
        return Err(AddError::new(AddCode::Failed, "not a saved SSH Host"));
    }
    Ok(())
}

fn add(app: &AppHandle, host: &str) -> Result<ApprovedComputer, AddError> {
    saved_ssh_id(host)?;
    let w = worker(app).map_err(|e| AddError::new(AddCode::NotReady, e))?;
    let manager = app
        .try_state::<crate::hosts::Hosts>()
        .map(|h| h.manager.clone())
        .ok_or_else(|| AddError::new(AddCode::NotReady, "Hosts are not set up"))?;
    let route = manager
        .ssh_route(host)
        .ok_or_else(|| AddError::new(AddCode::Failed, "not a saved SSH Host"))?;
    let status = route.handle.status();
    check_host(&status)?;
    let link_gen = HOST_SYNC
        .seen(host)
        .map(|(seen, _)| seen)
        .filter(|seen| seen.usable)
        .map(|seen| seen.link_gen)
        .ok_or_else(|| AddError::new(AddCode::Offline, "the Host is not connected"))?;
    let cancel = CancelToken::new();
    let binary = resolve_binary(
        &*route.transport,
        route.config.daemon_override(),
        &route.install_dir,
        status.os.as_deref(),
        &cancel,
    );
    let handle = route.handle.clone();
    // Captured once; an edit of the Host or a new link during the action fails it.
    let captured = (
        route.config.ssh_target.clone(),
        route.config.daemon_override().map(String::from),
    );
    let unchanged = || {
        manager.ssh_route(host).is_some_and(|now| {
            (
                now.config.ssh_target.clone(),
                now.config.daemon_override().map(String::from),
            ) == captured
        }) && HOST_SYNC
            .seen(host)
            .is_some_and(|(seen, _)| seen.usable && seen.link_gen == link_gen)
    };
    let added = add_ssh_host(
        &AddRequest {
            ring: &w.ring,
            name: &route.config.name,
            transport: &*route.transport,
            binary: &binary,
            cancel: &cancel,
            timeouts: Timeouts::default(),
        },
        move || {
            let (tx, rx) = mpsc::channel();
            handle.ring_identity(
                link_gen,
                Box::new(move |r| {
                    let _ = tx.send(r);
                }),
            );
            match rx.recv_timeout(IDENTITY_TIMEOUT) {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(e.message),
                Err(_) => Err("no answer".into()),
            }
        },
        unchanged,
    )?;
    Ok(ApprovedComputer {
        names: added.names,
        version: added.version,
    })
}

/// Add to Ring on a Host row in Settings → Remote hosts.
#[tauri::command]
pub async fn ring_add_ssh_host(app: AppHandle, host: String) -> Result<ApprovedComputer, String> {
    let guard = AddingGuard::take().map_err(|e| e.to_string())?;
    run(move || {
        let _guard = guard;
        let result = add(&app, &host);
        // The Host maps to its member (or a partial attempt is looked at again) either way.
        HOST_SYNC.poke_all();
        refresh_approval(&app);
        result.map_err(|e| {
            // `e` never holds the words (the module docs of `ssh_host`).
            eprintln!("xshell: Add to Ring on {host}: {e}");
            e.to_string()
        })
    })
    .await
}

fn keep(app: &AppHandle, host: &str) -> Result<(), AddError> {
    saved_ssh_id(host)?;
    let manager = app
        .try_state::<crate::hosts::Hosts>()
        .map(|h| h.manager.clone())
        .ok_or_else(|| AddError::new(AddCode::NotReady, "Hosts are not set up"))?;
    let route = manager
        .ssh_route(host)
        .ok_or_else(|| AddError::new(AddCode::Failed, "not a saved SSH Host"))?;
    let status = route.handle.status();
    check_host(&status)?;
    let cancel = CancelToken::new();
    let binary = resolve_binary(
        &*route.transport,
        route.config.daemon_override(),
        &route.install_dir,
        status.os.as_deref(),
        &cancel,
    );
    keep_running(&*route.transport, &binary, &cancel)
}

/// Before a saved SSH Host that joined the Ring leaves the SSH list (#119): its Daemon must
/// start again by itself, since no SSH connection will start it. Runs the existing
/// `xshelld service install` on the Host over its SSH route. Errors are `code: detail`.
#[tauri::command]
pub async fn ring_ssh_host_keep_running(app: AppHandle, host: String) -> Result<(), String> {
    run(move || {
        keep(&app, &host).map_err(|e| {
            eprintln!("xshell: keep the Daemon running on {host}: {e}");
            e.to_string()
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_attempt_at_a_time() {
        let first = AddingGuard::take().unwrap();
        let second = AddingGuard::take().err().unwrap();
        assert_eq!(second.code, AddCode::Busy);
        assert!(second.to_string().starts_with("busy: "));
        drop(first);
        drop(AddingGuard::take().unwrap());
    }

    #[test]
    fn only_saved_ssh_hosts() {
        assert_eq!(
            saved_ssh_id(LOCAL_HOST_ID).unwrap_err().code,
            AddCode::Failed
        );
        assert_eq!(
            saved_ssh_id(&format!("r_{}", "a".repeat(64)))
                .unwrap_err()
                .code,
            AddCode::Failed
        );
        assert_eq!(saved_ssh_id("h_abcdefgh"), Ok(()));
    }
}

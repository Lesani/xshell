//! Explicit Device Code command. Registered Computers forward one ephemeral offer
//! to their elected Desktop; only owner-local backend consent can complete it. Truly
//! unregistered Unix Hosts use the standalone v2 transaction, never a Desktop signer.
//! Windows has no standalone headless Host (ADR-0005): an unregistered Store is refused.
//! Zero requires accepted membership and the required durable installation receipts.

use crate::cli::Opts;
use crate::paths::Paths;
#[cfg(all(test, unix))]
use crate::server::ring::is_daemon_member;
use crate::server::ring::Store;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::pairing::PAIR_TTL_SECS;
use xshell_protocol::ring::url::{RelayUrl, HOSTED_RELAY_URL};
#[cfg(all(test, unix))]
use xshell_protocol::ring::RosterChain;

/// The exit code when nobody entered the code in time.
pub const EXPIRED_EXIT: i32 = 5;

enum Failure {
    /// The standalone (Unix-only) offer expired.
    #[cfg_attr(windows, allow(dead_code))]
    Expired,
    ComputerExpired,
    Other(String),
}

impl From<String> for Failure {
    fn from(s: String) -> Self {
        Failure::Other(s)
    }
}

pub fn run_pair(opts: &Opts, paths: &Paths) -> i32 {
    crate::server::block_exit_signals();
    let cancel = Arc::new(AtomicBool::new(false));
    if let Err(error) = crate::server::watch_pair_signals(cancel.clone()) {
        eprintln!("xshelld: cannot watch Pair cancellation: {error}");
        return 1;
    }
    let code = run_pair_cancel(opts, paths, &mut io::stdout(), &cancel);
    #[cfg(windows)]
    crate::server::pair_cleanup_done();
    code
}

/// In-process output facade; actual CLI lifetime cancellation is installed by run_pair.
pub fn run_pair_to(opts: &Opts, paths: &Paths, out: &mut dyn Write) -> i32 {
    run_pair_cancel(opts, paths, out, &Arc::new(AtomicBool::new(false)))
}
fn run_pair_cancel(
    opts: &Opts,
    paths: &Paths,
    out: &mut dyn Write,
    cancel: &Arc<AtomicBool>,
) -> i32 {
    match pair_cancel(opts, paths, out, cancel) {
        Ok(()) => 0,
        Err(Failure::Expired | Failure::ComputerExpired) => {
            let _ = writeln!(
                out,
                "Pairing request expired. Reconcile any recorded outcome in xshell before explicitly requesting a new code."
            );
            EXPIRED_EXIT
        }
        Err(Failure::Other(message)) => {
            eprintln!("xshelld: {message}");
            1
        }
    }
}

/// How long the code is good for: 10 minutes (`XSHELLD_PAIR_TTL_MS`, test only, shortens it).
fn ttl() -> Duration {
    std::env::var("XSHELLD_PAIR_TTL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(PAIR_TTL_SECS))
        .min(Duration::from_secs(PAIR_TTL_SECS))
}

#[cfg(all(test, unix))]
fn pair(opts: &Opts, paths: &Paths, out: &mut dyn Write) -> Result<(), Failure> {
    pair_cancel(opts, paths, out, &Arc::new(AtomicBool::new(false)))
}
fn pair_cancel(
    opts: &Opts,
    paths: &Paths,
    out: &mut dyn Write,
    cancel: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    // Windows: a Store no Desktop ever registered gets no directories or files from Pair.
    #[cfg(windows)]
    if matches!(std::fs::symlink_metadata(&paths.ring_dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound)
    {
        return Err(Failure::Other(NO_REGISTERED_DESKTOP.into()));
    }
    let mut canonical_paths = paths.clone();
    canonical_paths.ring_dir = crate::server::ring::ownership::canonical_store(&paths.ring_dir)
        .map_err(|e| format!("protected_counterpart_required: unsafe local Store: {e}"))?;
    let paths = &canonical_paths;
    let store = Store::new(paths.ring_dir.clone());
    xshell_core::companion_reservation::validate_daemon_store(&paths.ring_dir)
        .map_err(|e| format!("protected_counterpart_required: unsafe local Store: {e}"))?;
    let relay = opts
        .relay
        .clone()
        .unwrap_or_else(|| HOSTED_RELAY_URL.to_string());
    RelayUrl::parse(&relay).map_err(|_| "invalid Pair Relay".to_string())?;
    if crate::server::ring::companion_registered(&paths.ring_dir) {
        return registered_computer_pair(&store, &relay, out, cancel);
    }
    #[cfg(windows)]
    {
        let _ = (opts, out);
        Err(Failure::Other(NO_REGISTERED_DESKTOP.into()))
    }
    #[cfg(unix)]
    standalone_pair(opts, paths, &relay, out, cancel)
}

/// Windows refusal: never a standalone fallback (BINDING Decision 1).
#[cfg(windows)]
const NO_REGISTERED_DESKTOP: &str =
    "protected_counterpart_required: no registered xshell Desktop; open xshell";

#[cfg(unix)]
fn standalone_pair(
    opts: &Opts,
    paths: &Paths,
    relay: &str,
    out: &mut dyn Write,
    cancel: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    let relay = relay.to_string();
    let lifetime = ttl();
    let until = Instant::now() + lifetime;
    crate::server::adoption::adopt_standalone_command(
        paths,
        &relay,
        opts.name.as_deref(),
        lifetime,
        cancel,
        |code| print_words(out, code, &relay, until),
    )
    .map_err(|error| {
        if error == "the code expired" {
            Failure::Expired
        } else {
            Failure::Other(error)
        }
    })?;
    writeln!(out, "Paired this standalone Host.")
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn print_words(
    out: &mut dyn Write,
    code: &DeviceCode,
    relay: &str,
    until: Instant,
) -> Result<(), String> {
    writeln!(out, "Device code: {}", code.format().as_str())
        .and_then(|()| {
            writeln!(
                out,
                "In xshell, open Settings -> Hosts -> Ring and enter these eight words."
            )
        })
        .and_then(|()| {
            writeln!(
                out,
                "One use; {} seconds remaining in this command. Relay: {relay}",
                until.saturating_duration_since(Instant::now()).as_secs()
            )
        })
        .and_then(|()| out.flush())
        .map_err(|_| "Pair origin output closed".into())
}
fn registered_computer_pair(
    store: &Store,
    relay: &str,
    out: &mut dyn Write,
    cancel: &Arc<AtomicBool>,
) -> Result<(), Failure> {
    use xshell_hostlink::ring::administration::{
        AdministrationClient, AdministrationRequest as R, AdministrationStatus as S,
    };
    use xshell_hostlink::ring::adoption::JoinOfferInput;
    let duration = ttl();
    let until = Instant::now() + duration;
    let monotonic_expires_at_ns = xshell_hostlink::ring::administration::monotonic::capture(until)
        .map_err(|_| Failure::ComputerExpired)?;
    let budget = xshell_core::local_ipc::RequestBudget::for_operation(until, cancel.clone());
    let desktop =
        crate::server::companion::registered_desktop(store, &budget).map_err(|error| {
            if Instant::now() >= until {
                Failure::ComputerExpired
            } else {
                Failure::Other(format!("protected_counterpart_required: {error}"))
            }
        })?;
    budget.check().map_err(|_| Failure::ComputerExpired)?;
    let operation = uuid::Uuid::new_v4();
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + duration.as_secs();
    let offer = JoinOfferInput {
        operation: xshell_protocol::ring::relay::wire::new_nonce().map_err(|e| e.to_string())?,
        code: DeviceCode::generate().map_err(|e| e.to_string())?,
        relay_url: relay.into(),
        expires_at,
    };
    writeln!(
        out,
        "Computer pairing pending. Open xshell and continue in the pairing window."
    )
    .and_then(|()| out.flush())
    .map_err(|_| "Pair origin output closed".to_string())?;
    let mut client = None;
    let mut printed = false;
    let mut started = false;
    let mut result = Err(Failure::ComputerExpired);
    while Instant::now() < until && !cancel.load(Ordering::Acquire) {
        if client.is_none() {
            if let Ok(connected) = AdministrationClient::connect(Path::new(&desktop), until) {
                client = Some(connected);
            } else {
                std::thread::sleep(
                    Duration::from_millis(100).min(until.saturating_duration_since(Instant::now())),
                );
                continue;
            }
        }
        let connected = client.as_mut().expect("connected");
        let request = if !started {
            started = true;
            R::Start {
                operation,
                expires_at,
                offer: offer.clone(),
                monotonic_expires_at_ns,
            }
        } else {
            R::Status { operation }
        };
        let status = match connected.request(request, until) {
            Ok(status) => status,
            Err(_) => {
                if Instant::now() >= until {
                    break;
                }
                result = Err(Failure::Other("Computer pairing owner/epoch disconnected; the offer will not be reopened. If consent may have left, reconcile in xshell.".into()));
                break;
            }
        };
        if !printed
            && matches!(
                status,
                S::SlotReady | S::AwaitingConfirmation | S::Completing | S::Installed
            )
        {
            if cancel.load(Ordering::Acquire) || Instant::now() >= until {
                break;
            }
            if let Err(error) = print_words(out, &offer.code, relay, until) {
                result = Err(Failure::Other(error));
                break;
            }
            printed = true;
            let _ = connected.request(R::RequestOwnerFocus {}, until);
        }
        match status {
            S::Installed => {
                result = writeln!(
                    out,
                    "Paired this Computer; both local stores are installed."
                )
                .and_then(|()| out.flush())
                .map_err(|_| Failure::Other("Pair origin output closed".into()));
                break;
            }
            S::ComputerPending | S::SlotReady | S::AwaitingConfirmation | S::Completing => {}
            S::Expired => break,
            S::ReconciliationRequired => {
                result = Err(Failure::Other(
                    "reconciliation_required: continue the recorded operation in xshell".into(),
                ));
                break;
            }
            _ => {
                result = Err(Failure::Other(
                    "Computer pairing ended without installation".into(),
                ));
                break;
            }
        }
        std::thread::sleep(
            Duration::from_millis(100).min(until.saturating_duration_since(Instant::now())),
        );
    }
    if cancel.load(Ordering::Acquire) {
        result = Err(Failure::Other(
            "Pairing cancelled; possible transmitted consent requires reconciliation".into(),
        ));
    }
    if let Some(mut connected) = client {
        // Cleanup only: never a fresh Start, owner, offer or lifetime.
        let _ = connected.request(
            R::Cancel { operation },
            until.min(Instant::now()) + Duration::from_millis(100),
        );
    }
    result
}

#[cfg(all(test, unix))]
fn legacy_outcome_uncertain(store: &Store) -> bool {
    xshell_core::companion_reservation::read_reservation(&store.dir)
        .ok()
        .flatten()
        .is_some_and(|reservation| reservation.legacy_outcome_uncertain)
}
#[cfg(all(test, unix))]
fn uncertain_legacy_failure(message: String) -> Failure {
    Failure::Other(format!("reconciliation_required: legacy pairing remote outcome is uncertain; startup denial does not revoke a transmitted proof or establish nonmembership ({message})"))
}

/// Stores only an initial membership or an authorized extension of the retained Ring.
/// Foreign/terminal membership is never a force-replacement or a new enrollment attempt.
#[cfg(all(test, unix))]
fn adopt(
    store: &Store,
    fetched: RosterChain,
    keys: &xshell_protocol::ring::DeviceKeys,
) -> Result<RosterChain, Failure> {
    let _lock = store.lock()?;
    store.refuse_registered_join().map_err(|error| {
        if legacy_outcome_uncertain(store) {
            uncertain_legacy_failure(error)
        } else {
            Failure::Other(error)
        }
    })?;
    let disk = store
        .read_chain()
        .map_err(|e| format!("the stored ring is unreadable ({e})"))?;
    let next = match disk {
        None => fetched,
        Some(disk) => {
            if disk.ring_id() != fetched.ring_id() || !is_daemon_member(&disk, keys) {
                return Err(Failure::Other(
                    "leave_required: retained foreign or terminal membership cannot be replaced"
                        .into(),
                ));
            }
            disk.extended(fetched.versions())
                .map_err(|e| {
                    format!(
                        "the relay's roster conflicts with the one stored here ({})",
                        e.as_code()
                    )
                })?
                .0
        }
    };
    if !is_daemon_member(&next, keys) {
        return Err(Failure::Other(
            "the retained Ring does not authorize this computer".into(),
        ));
    }
    store.write_chain(&next)?;
    Ok(next)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use xshell_protocol::ring::{DeviceKeys, Member, Role, SignedRoster};

    fn paths(t: &tempfile::TempDir) -> Paths {
        crate::paths::resolve(t.path(), Some(&t.path().join("run")), None)
    }

    /// v1 (a Desktop) → v2 (adds `me` as a daemon) → v3 (adds a phone).
    fn chain(me: &DeviceKeys) -> (DeviceKeys, RosterChain) {
        let desk = DeviceKeys::generate().unwrap();
        let g = SignedRoster::genesis(&desk, desk.noise_key(), "d", "ws://127.0.0.1:9", 1).unwrap();
        let v2 = g
            .next(&desk, 2, |d| {
                d.add(Member::new(
                    "me",
                    Role::Daemon,
                    me.sign_key(),
                    me.noise_key(),
                    2,
                ))
            })
            .unwrap();
        let p = DeviceKeys::generate().unwrap();
        let v3 = v2
            .next(&desk, 3, |d| {
                d.add(Member::new(
                    "p",
                    Role::Mobile,
                    p.sign_key(),
                    p.noise_key(),
                    3,
                ))
            })
            .unwrap();
        (desk, RosterChain::from_chain(vec![g, v2, v3]).unwrap())
    }

    fn prefix(c: &RosterChain, n: usize) -> RosterChain {
        RosterChain::from_chain(c.versions()[..n].to_vec()).unwrap()
    }

    #[test]
    fn incumbent_local_legacy_commit_is_refused_without_discarding_uncertainty_or_keys() {
        use std::os::unix::fs::DirBuilderExt;
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let store = Store::new(paths.ring_dir.clone());
        let offer = store.reserve_legacy_offer().unwrap();
        let me = store.load_or_create_keys().unwrap();
        let (_, fetched) = chain(&me); // Historical v1 negative fixture, not new adoption authority.
        let desktop = temp.path().join("desktop");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&desktop)
            .unwrap();
        xshell_core::companion_reservation::reserve(
            &temp.path().canonicalize().unwrap(),
            &desktop.canonicalize().unwrap(),
        )
        .unwrap();
        assert!(
            matches!(adopt(&store, fetched, &me), Err(Failure::Other(message)) if message.contains("remote outcome is uncertain"))
        );
        drop(offer);
        assert!(store.read_chain().unwrap().is_none());
        assert_eq!(
            store.load_keys().unwrap().unwrap().sign_key(),
            me.sign_key()
        );
        assert!(
            xshell_core::companion_reservation::read_reservation(&paths.ring_dir)
                .unwrap()
                .unwrap()
                .legacy_outcome_uncertain
        );
    }
    #[test]
    fn an_older_pinned_head_never_overwrites_newer_local_trust() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(&t);
        let store = Store::new(p.ring_dir.clone());
        let me = store.load_or_create_keys().unwrap();
        let (_, full) = chain(&me);
        store.write_chain(&full).unwrap();
        // The Relay showed only up to v2 (it hid v3): the stored v3 stays.
        let kept = adopt(&store, prefix(&full, 2), &me).ok().unwrap();
        assert_eq!(kept, full);
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
        // A newer one extends what is stored.
        store.write_chain(&prefix(&full, 2)).unwrap();
        assert_eq!(adopt(&store, full.clone(), &me).ok().unwrap(), full);
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
    }

    #[test]
    fn terminal_membership_is_not_automatically_reenrolled() {
        // Historical v1 file fixture: the local key has already been removed.
        let t = tempfile::tempdir().unwrap();
        let p = paths(&t);
        let store = Store::new(p.ring_dir.clone());
        let me = store.load_or_create_keys().unwrap();
        let (desktop, full) = chain(&me);
        let removed = full
            .head()
            .next(&desktop, 4, |draft| {
                draft.remove(&me.sign_key());
            })
            .unwrap();
        let mut terminal = full.clone();
        terminal.accept(&[removed]).unwrap();
        store.write_chain(&terminal).unwrap();
        assert!(
            matches!(adopt(&store, full, &me), Err(Failure::Other(message)) if message.starts_with("leave_required:"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), terminal);
        assert_eq!(
            store.load_keys().unwrap().unwrap().sign_key(),
            me.sign_key()
        );
    }

    #[test]
    fn forks_and_other_rings_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(&t);
        let store = Store::new(p.ring_dir.clone());
        let me = store.load_or_create_keys().unwrap();
        let (desk, full) = chain(&me);
        store.write_chain(&full).unwrap();
        // Another v3 of the same Ring: a fork.
        let other = DeviceKeys::generate().unwrap();
        let fork_v3 = full
            .get(2)
            .unwrap()
            .next(&desk, 9, |d| {
                d.add(Member::new(
                    "o",
                    Role::Mobile,
                    other.sign_key(),
                    other.noise_key(),
                    9,
                ))
            })
            .unwrap();
        let fork = RosterChain::from_chain(vec![
            full.get(1).unwrap().clone(),
            full.get(2).unwrap().clone(),
            fork_v3,
        ])
        .unwrap();
        assert!(
            matches!(adopt(&store, fork, &me), Err(Failure::Other(m)) if m.contains("conflicts"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
        // `--force` is not a Leave and cannot bypass retained foreign membership.
        let (_, theirs) = chain(&me);
        assert!(
            matches!(adopt(&store, theirs.clone(), &me), Err(Failure::Other(m)) if m.starts_with("leave_required:"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
        assert!(
            matches!(adopt(&store, theirs, &me), Err(Failure::Other(m)) if m.starts_with("leave_required:"))
        );
        assert_eq!(store.read_chain().unwrap().unwrap(), full);
    }
    #[test]
    fn force_pair_refuses_before_offer_and_preserves_retained_keys() {
        let t = tempfile::tempdir().unwrap();
        let paths = paths(&t);
        let store = Store::new(paths.ring_dir.clone());
        let keys = store.load_or_create_keys().unwrap();
        let (_, retained) = chain(&keys);
        store.write_chain(&retained).unwrap();
        for force in [false, true] {
            let mut output = Vec::new();
            let options = Opts {
                force,
                ..Default::default()
            };
            assert!(
                matches!(pair(&options, &paths, &mut output), Err(Failure::Other(message)) if message.starts_with("leave_required:"))
            );
            assert!(
                output.is_empty(),
                "no code or network offer before the membership fence"
            );
            assert_eq!(store.read_chain().unwrap().unwrap(), retained);
            assert_eq!(
                store.load_keys().unwrap().unwrap().sign_key(),
                keys.sign_key()
            );
        }
    }
    #[test]
    fn stopped_or_unreadable_registration_never_falls_back_to_headless_offer() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let store = Store::new(paths.ring_dir.clone());
        store.ensure_dir().unwrap();
        std::fs::write(paths.ring_dir.join("companion.json"), b"{unreadable").unwrap();
        for force in [false, true] {
            let mut output = Vec::new();
            let options = Opts {
                force,
                ..Default::default()
            };
            assert!(
                matches!(pair(&options, &paths, &mut output), Err(Failure::Other(message)) if message.starts_with("protected_counterpart_required:"))
            );
            assert!(output.is_empty());
            assert!(
                !paths.ring_dir.join("keys.json").exists(),
                "refusal must not create replacement identities"
            );
        }
    }
    #[test]
    fn legacy_adopt_rechecks_effective_binding_under_ring_lock() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let store = Store::new(paths.ring_dir.clone());
        let keys = store.load_or_create_keys().unwrap();
        let (_, fetched) = chain(&keys); // Historical v1 legacy negative, not v2 bootstrap.
        {
            let _lock = store.lock().unwrap();
            crate::server::ring::write_private(
                &paths.ring_dir.join("companion.json"),
                b"retained binding",
            )
            .unwrap();
        }
        assert!(
            matches!(adopt(&store, fetched, &keys), Err(Failure::Other(message)) if message.starts_with("protected_counterpart_required:"))
        );
        assert!(store.read_chain().unwrap().is_none());
        assert_eq!(
            store.load_keys().unwrap().unwrap().sign_key(),
            keys.sign_key()
        );
    }
}

#[cfg(feature = "legacy-pair-test-control")]
mod legacy_test_control;
#[cfg(feature = "legacy-pair-test-control")]
pub use legacy_test_control::run_legacy_pair_test_control;

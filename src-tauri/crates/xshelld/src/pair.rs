//! Explicit Unix Device Code command. Registered Computers forward one ephemeral offer
//! to their elected Desktop; only owner-local backend consent can complete it. Truly
//! unregistered Hosts use the standalone v2 transaction, never a Desktop signer.
//! Zero requires accepted membership and the required durable installation receipts.

use crate::cli::Opts;
use crate::paths::Paths;
#[cfg(test)]
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
#[cfg(test)]
use xshell_protocol::ring::RosterChain;

/// The exit code when nobody entered the code in time.
pub const EXPIRED_EXIT: i32 = 5;
/// The exit code when the Relay refused new pairing attempts from this network for now
/// (HTTP 429, #112): not a connection problem.
pub const RATE_LIMITED_EXIT: i32 = 7;
/// The exit code when another device approved this machine but the command ended before the
/// Join was confirmed: xshell on this machine finishes it by itself (never a new offer).
pub const UNFINISHED_EXIT: i32 = 6;

enum Failure {
    Expired,
    ComputerExpired,
    /// The service could not be reached before anything was uncertain.
    Offline,
    /// The user stopped the command (Ctrl-C).
    Cancelled,
    /// Approved; the machine finishes the Join by itself.
    Finishing,
    /// Standalone: consent may have left and the obligation is retained; the Daemon
    /// finishes it (running now, or when it next runs). `cancelled`: ended by Ctrl-C.
    Unfinished {
        daemon_running: bool,
        cancelled: bool,
        cause: String,
    },
    /// An earlier attempt of this machine is still unresolved while a Daemon runs: it
    /// finishes it. `minutes`: whole minutes (1 to 10) until its code expires; `None` once
    /// it has expired (the service stayed unreachable).
    EarlierRunning {
        minutes: Option<u64>,
    },
    /// An earlier attempt is unresolved and no Daemon runs here to finish it.
    EarlierStopped,
    /// Another `xshelld pair` is showing words on this machine.
    AlreadyRunning,
    /// The Store is owned but nothing answers: a Daemon is starting or stopping.
    Busy,
    /// Too many pairing attempts from this network in the Relay's last minute.
    RateLimited,
    Other(String),
}

// What `xshelld pair` prints: the approved copy (plans/copy-rjw-desktop.md section 13,
// keys `cli.pair.*`), verbatim.
const INSTRUCTION: &str = "Type these eight words on a machine or phone already in your Ring. On a machine, open Settings > Ring > Add a machine.";
const LIFETIME: &str = "These words work once, for 10 minutes, while this command is running.";
const WAITING: &str = "Waiting for another device to add this machine...";
const EXPIRED: &str = "These words have expired. Run xshelld pair again for new words.";
const OFFLINE: &str =
    "Can't reach the service. Check your connection, then run xshelld pair again.";
// COPY-GAP: no approved line for a pair command stopped with Ctrl-C; modelled on
// `cli.pair.expired`.
const CANCELLED: &str = "These words were cancelled. Run xshelld pair again for new words.";
// COPY-GAP: no approved headless line for an approved Join that finishes automatically;
// `finish.body` (section 8).
const FINISHING: &str =
    "xshell is checking whether this machine joined your Ring. You do not need to do anything.";

// `cli.pair.reconnecting` and `cli.pair.unfinished.*` (plans/plan-pairing-a.md, COPY).
const RECONNECTING: &str = "Connection to the service was lost; this command is still trying. Do not enter the words again or start over.";
const UNFINISHED_RUNNING: &str = "Pairing isn't confirmed; this machine keeps checking and joins when it reaches the service. No action needed; watch for it in Settings > Ring on another device.";
const UNFINISHED_STOPPED: &str = "Pairing isn't confirmed; xshell finishes it when it next runs in the background here (xshelld serve or a terminal connects). Do not run xshelld pair again first.";
const UNFINISHED_CANCELLED: &str = "Stopping does not undo approval; this machine may still join your Ring. If you don't want it to, remove it in Settings > Ring on another device.";

// `cli.pair.earlier.*`, `cli.pair.alreadyRunning`, `cli.pair.busy` (plans/plan-pairing-bc.md,
// COPY).
const EARLIER_CHECKING: &str =
    "An earlier pairing of this machine is still being finished. Checking now; no new words yet.";
const EARLIER_STOPPED: &str = "Pairing needs xshell running in the background here: start xshelld serve. Then check Settings > Ring on another device; no new words needed yet.";
const ALREADY_RUNNING: &str = "Another xshelld pair command is showing words on this machine. Use those words, or stop that command first.";
const BUSY: &str =
    "xshell on this machine is starting or stopping and hasn't answered. Try again in a moment.";
// `cli.pair.rateLimited` (plans/plan-pairing-bc.md, COPY).
const RATE_LIMITED: &str = "Too many pairing attempts from this network in the last minute. Wait about a minute, then run xshelld pair again.";

/// `cli.pair.earlier.running`; `minutes` is 1 to 10.
fn earlier_running(minutes: u64) -> String {
    format!("Pairing isn't confirmed; the machine keeps checking and joins when it reaches the service. If not joined in about {minutes} min, run xshelld pair for new words.")
}

/// `cli.pair.done.one` / `cli.pair.done.other`.
fn done_line(machines: usize) -> String {
    if machines == 1 {
        "Added this machine to your Ring. Your Ring has 1 machine.".into()
    } else {
        format!("Added this machine to your Ring. Your Ring has {machines} machines.")
    }
}

/// COPY-GAP: no approved line for `pair` on a machine already in a Ring; modelled on
/// `cli.pair.done.*`.
fn already_line(machines: usize) -> String {
    if machines == 1 {
        "This machine is already in your Ring. Your Ring has 1 machine.".into()
    } else {
        format!("This machine is already in your Ring. Your Ring has {machines} machines.")
    }
}

/// The machines in the Ring: its Computer, standalone Host and legacy Desktop units, not
/// its phones.
fn machines(chain: &xshell_protocol::ring::RosterChain) -> usize {
    use xshell_protocol::ring::membership::EnrollmentKind;
    chain
        .head()
        .roster()
        .membership()
        .map(|m| {
            m.enrollments
                .iter()
                .filter(|u| u.kind != EnrollmentKind::Mobile)
                .count()
        })
        .unwrap_or_else(|_| {
            chain
                .head()
                .roster()
                .members
                .iter()
                .filter(|m| m.role != xshell_protocol::ring::Role::Mobile)
                .count()
        })
}

/// The format-2 Ring this Host's Daemon is an authorized member of, from its own store
/// (format-1 state is set aside by the Daemon, never reported as a membership).
fn current_ring(store: &Store) -> Option<xshell_protocol::ring::RosterChain> {
    let chain = store.read_chain().ok()??;
    if chain.head().roster().v != 2
        || xshell_protocol::ring::membership::unsupported_stored_membership(chain.head())
    {
        return None;
    }
    let keys = store.load_keys().ok()??;
    chain
        .authorized_member(&keys.sign_key())
        .is_some()
        .then_some(chain)
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
    run_pair_cancel(opts, paths, &mut io::stdout(), &cancel)
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
            let _ = writeln!(out, "{EXPIRED}");
            EXPIRED_EXIT
        }
        Err(failure) => {
            let (code, lines) = failure_report(failure);
            for line in lines {
                eprintln!("{line}");
            }
            code
        }
    }
}

/// The exit code and the stderr lines of a failed `pair` (other than expiry).
fn failure_report(failure: Failure) -> (i32, Vec<String>) {
    match failure {
        Failure::Expired | Failure::ComputerExpired => (EXPIRED_EXIT, vec![EXPIRED.into()]),
        Failure::Offline => (1, vec![OFFLINE.into()]),
        Failure::Cancelled => (1, vec![CANCELLED.into()]),
        Failure::Finishing => (1, vec![FINISHING.into()]),
        // The copy says what happens next; the detail line stays for logs and tests.
        Failure::Unfinished {
            daemon_running,
            cancelled,
            cause,
        } => {
            let copy = if cancelled {
                UNFINISHED_CANCELLED
            } else if daemon_running {
                UNFINISHED_RUNNING
            } else {
                UNFINISHED_STOPPED
            };
            (
                UNFINISHED_EXIT,
                vec![copy.into(), format!("xshelld: {cause}")],
            )
        }
        Failure::EarlierRunning {
            minutes: Some(minutes),
        } => (UNFINISHED_EXIT, vec![earlier_running(minutes)]),
        // Its code has expired and it is still unresolved: no promise about when it ends.
        Failure::EarlierRunning { minutes: None } => {
            (UNFINISHED_EXIT, vec![UNFINISHED_RUNNING.into()])
        }
        Failure::EarlierStopped => (UNFINISHED_EXIT, vec![EARLIER_STOPPED.into()]),
        Failure::AlreadyRunning => (1, vec![ALREADY_RUNNING.into()]),
        Failure::Busy => (1, vec![BUSY.into()]),
        Failure::RateLimited => (RATE_LIMITED_EXIT, vec![RATE_LIMITED.into()]),
        Failure::Other(message) => (1, vec![format!("xshelld: {message}")]),
    }
}

/// A standalone command's end as a [`Failure`]. Retained post-consent work is checked
/// before cancellation, so Ctrl-C after approval never reads as "the words were cancelled".
fn standalone_failure(
    error: crate::server::adoption::StandaloneCommandError,
    cancelled: bool,
) -> Failure {
    use crate::server::adoption::StandaloneCommandError as E;
    match error {
        E::Unfinished {
            daemon_running,
            cause,
        } => Failure::Unfinished {
            daemon_running,
            cancelled,
            cause,
        },
        E::RateLimited => Failure::RateLimited,
        E::Failed(error) if error == "the code expired" => Failure::Expired,
        E::Failed(_) if cancelled => Failure::Cancelled,
        E::Failed(error) if error.contains("cannot reach the relay") => Failure::Offline,
        // Races with the checks before the offer: another command took the offer lock, or
        // the Store's owner has no endpoint yet (starting) or any more (stopping).
        E::Failed(error) if error.starts_with("legacy_offer_active") => Failure::AlreadyRunning,
        E::Failed(error) if error.starts_with("running owner unavailable") => Failure::Busy,
        E::Failed(error) => Failure::Other(error),
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

#[cfg(test)]
fn pair(opts: &Opts, paths: &Paths, out: &mut dyn Write) -> Result<(), Failure> {
    pair_cancel(opts, paths, out, &Arc::new(AtomicBool::new(false)))
}
fn pair_cancel(
    opts: &Opts,
    paths: &Paths,
    out: &mut dyn Write,
    cancel: &Arc<AtomicBool>,
) -> Result<(), Failure> {
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
    // "Already in your Ring" needs the durable receipt too: a Roster installed without it
    // (an earlier attempt whose consent may have left) is not yet a membership. The Roster
    // is read first and the journal after it: a standalone attempt writes its journal
    // before it installs any Roster and sets aside only an uninstalled journal, so a
    // Roster seen first is never paired with a journal state older than it. An unresolved
    // attempt is waited for (briefly, once) instead of refused with a raw code.
    let mut waited = false;
    loop {
        let chain = current_ring(&store);
        #[cfg(debug_assertions)]
        test_cut("XSHELLD_TEST_PAIR_BEFORE_JOURNAL_READ");
        match (earlier_pairing(&store), chain) {
            (EarlierPairing::Unresolved { expires_at }, _) => {
                if waited {
                    return Err(Failure::EarlierRunning {
                        minutes: minutes_left(expires_at),
                    });
                }
                waited = true;
                if let Earlier::Added(chain) = wait_for_earlier(&store, out, cancel, expires_at)? {
                    return writeln!(out, "{}", done_line(machines(&chain)))
                        .and_then(|()| out.flush())
                        .map_err(|e| Failure::Other(e.to_string()));
                }
                // Set aside: look again from the start.
            }
            // An installed journal whose durable validation fails (or an unreadable one)
            // never reads as a membership: the existing refusal.
            (EarlierPairing::Other, Some(_)) => {
                return Err(Failure::Other(
                    crate::server::standalone_journal::standalone_refusal(&store).unwrap_or_else(
                        || "reconciliation_required: standalone obligation changed".into(),
                    ),
                ));
            }
            (EarlierPairing::Absent | EarlierPairing::Installed, Some(chain)) => {
                return writeln!(out, "{}", already_line(machines(&chain)))
                    .and_then(|()| out.flush())
                    .map_err(|e| Failure::Other(e.to_string()));
            }
            (_, None) => break,
        }
    }
    if crate::server::ring::companion_registered(&paths.ring_dir) {
        return registered_computer_pair(&store, &relay, out, cancel);
    }
    let lifetime = ttl();
    let pending = format!(
        "reconciliation_required: {}",
        crate::server::standalone_journal::CONSENT_PENDING
    );
    let chain = loop {
        // An attempt that became unresolved since the check above (a race): same path.
        if let EarlierPairing::Unresolved { expires_at } = earlier_pairing(&store) {
            if waited {
                return Err(Failure::EarlierRunning {
                    minutes: minutes_left(expires_at),
                });
            }
            waited = true;
            if let Earlier::Added(chain) = wait_for_earlier(&store, out, cancel, expires_at)? {
                break chain;
            }
        }
        if offer_held_by_another(&store) {
            return Err(Failure::AlreadyRunning);
        }
        match crate::server::adoption::adopt_standalone_command(
            paths,
            &relay,
            opts.name.as_deref(),
            lifetime,
            cancel,
            |event| match event {
                crate::server::adoption::StandaloneEvent::Words(code) => {
                    print_words(out, code, true)
                }
                crate::server::adoption::StandaloneEvent::Reconnecting => {
                    writeln!(out, "{RECONNECTING}")
                        .and_then(|()| out.flush())
                        .map_err(|_| "Pair origin output closed".into())
                }
            },
        ) {
            Ok(chain) => break chain,
            // An attempt became unresolved after the check above (a race): same path.
            Err(crate::server::adoption::StandaloneCommandError::Failed(error))
                if error == pending && !cancel.load(Ordering::Acquire) =>
            {
                continue
            }
            Err(error) => return Err(standalone_failure(error, cancel.load(Ordering::Acquire))),
        }
    };
    writeln!(out, "{}", done_line(machines(&chain)))
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())?;
    Ok(())
}

use crate::server::standalone_journal::{earlier_pairing, EarlierPairing};

/// How a wait for an earlier attempt ended without a failure.
enum Earlier {
    /// The Daemon installed it (with its durable receipt): this machine is in the Ring.
    Added(xshell_protocol::ring::RosterChain),
    /// It was abandoned (its journal set aside): a fresh attempt may start.
    Fresh,
}

/// Whether a Daemon runs for this Store, found without electing an owner: a read-only
/// request to its protected companion endpoint with a short budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalOwner {
    /// It answered (with anything, a refusal included).
    Running,
    /// No endpoint, or nothing listens on it.
    Stopped,
    /// Something listens but does not answer in time.
    Busy,
}

fn local_owner(store: &Store) -> LocalOwner {
    use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
    let budget = xshell_core::local_ipc::RequestBudget::for_operation(
        Instant::now() + Duration::from_secs(2),
        Arc::new(AtomicBool::new(false)),
    );
    match xshell_core::local_ipc::exchange_until::<_, CompanionResponse>(
        &store.dir.join("companion.sock"),
        &CompanionRequest::StandaloneCommandReady,
        &budget,
    ) {
        Ok(_) => LocalOwner::Running,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            LocalOwner::Stopped
        }
        Err(_) => LocalOwner::Busy,
    }
}

/// Test cut: the command waits here while the named file exists (at most 60 s), so a test
/// can order another process's work between two of its reads.
#[cfg(debug_assertions)]
fn test_cut(name: &str) {
    if let Some(path) = std::env::var_os(name) {
        let path = std::path::PathBuf::from(path);
        if crate::server::ring::write_private(&path, b"reached").is_ok() {
            let until = Instant::now() + Duration::from_secs(60);
            while path.exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// How long the second command waits for an earlier attempt: 15 s
/// (`XSHELLD_PAIR_EARLIER_WAIT_MS`, test only, shortens it).
fn earlier_wait() -> Duration {
    const EARLIER_WAIT: Duration = Duration::from_secs(15);
    std::env::var("XSHELLD_PAIR_EARLIER_WAIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(EARLIER_WAIT)
        .min(EARLIER_WAIT)
}

/// Whole minutes (rounded up, 1 to 10) until `expires_at`; `None` once it has passed.
fn minutes_left(expires_at: u64) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let left = expires_at.checked_sub(now).filter(|left| *left > 0)?;
    Some(left.div_ceil(60).clamp(1, 10))
}

/// Waits while the Daemon finishes an earlier unresolved attempt. Only reads local files
/// and asks the Daemon whether it runs: never takes the offer lock or the Store ownership
/// lease, never opens a pairing slot. Success needs the installed journal with its durable
/// membership validation, not merely a readable Roster.
fn wait_for_earlier(
    store: &Store,
    out: &mut dyn Write,
    cancel: &Arc<AtomicBool>,
    mut expires_at: u64,
) -> Result<Earlier, Failure> {
    match local_owner(store) {
        LocalOwner::Running => {}
        LocalOwner::Stopped => return Err(Failure::EarlierStopped),
        LocalOwner::Busy => return Err(Failure::Busy),
    }
    writeln!(out, "{EARLIER_CHECKING}")
        .and_then(|()| out.flush())
        .map_err(|_| Failure::Other("Pair origin output closed".into()))?;
    let until = Instant::now() + earlier_wait();
    let mut probe_at = Instant::now() + Duration::from_secs(1);
    loop {
        match earlier_pairing(store) {
            EarlierPairing::Absent => return Ok(Earlier::Fresh),
            EarlierPairing::Installed => {
                if let Some(chain) = current_ring(store) {
                    return Ok(Earlier::Added(chain));
                }
            }
            EarlierPairing::Unresolved { expires_at: at } => expires_at = at,
            EarlierPairing::Other => {}
        }
        let now = Instant::now();
        if cancel.load(Ordering::Acquire) || now >= until {
            break;
        }
        // The Daemon that would finish it has exited: say so instead of waiting it out.
        if now >= probe_at {
            if local_owner(store) == LocalOwner::Stopped {
                return Err(Failure::EarlierStopped);
            }
            probe_at = Instant::now() + Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_millis(250).min(until.saturating_duration_since(now)));
    }
    Err(Failure::EarlierRunning {
        minutes: minutes_left(expires_at),
    })
}

/// Another `xshelld pair` holds the offer lock. The Daemon also takes it briefly while it
/// finishes or sets aside an earlier attempt, so a held lock is rechecked for up to 1 s.
fn offer_held_by_another(store: &Store) -> bool {
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        match store.try_offer_lock() {
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => return true,
            // Free (the guard is dropped at once) or unreadable: the ordinary path decides.
            Ok(Some(_)) | Err(_) => return false,
        }
    }
}
/// The words on a line of their own, where to type them, how long they work, and (unless
/// it was printed already) that the command waits.
/// `cli.pair.version`: this build's version, beside the words on the standalone route only
/// (on a registered Computer the Desktop joins, so this CLI's version would mislead). It
/// lets the user compare it with the approving Desktop's before entering the words (#111).
fn version_line() -> String {
    format!("xshell {} on this machine", env!("CARGO_PKG_VERSION"))
}

fn print_words(out: &mut dyn Write, code: &DeviceCode, waiting: bool) -> Result<(), String> {
    writeln!(out, "{}", code.format().as_str())
        .and_then(|()| {
            if waiting {
                writeln!(out, "{}", version_line())
            } else {
                Ok(())
            }
        })
        .and_then(|()| writeln!(out, "{INSTRUCTION}"))
        .and_then(|()| writeln!(out, "{LIFETIME}"))
        .and_then(|()| {
            if waiting {
                writeln!(out, "{WAITING}")
            } else {
                Ok(())
            }
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
    // COPY-GAP: no approved line while xshell on this machine prepares the words;
    // `cli.pair.waiting`.
    writeln!(out, "{WAITING}")
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
                result = Err(Failure::Other("xshell on this computer stopped answering; the code is no longer valid. If pairing had begun, xshell finishes it automatically.".into()));
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
            if let Err(error) = print_words(out, &offer.code, false) {
                result = Err(Failure::Other(error));
                break;
            }
            printed = true;
        }
        match status {
            S::Installed => {
                let count = current_ring(store).map_or(1, |chain| machines(&chain));
                result = writeln!(out, "{}", done_line(count))
                    .and_then(|()| out.flush())
                    .map_err(|_| Failure::Other("Pair origin output closed".into()));
                break;
            }
            S::ComputerPending | S::SlotReady | S::AwaitingConfirmation | S::Completing => {}
            S::Expired => break,
            // Kept through the Administration status (#112): not "can't reach the service".
            S::RateLimited => {
                result = Err(Failure::RateLimited);
                break;
            }
            S::ReconciliationRequired => {
                result = Err(Failure::Finishing);
                break;
            }
            // Ended before any words were shown: the service could not be reached (an
            // existing membership was answered before the command started).
            S::Failed if !printed => {
                result = Err(Failure::Offline);
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
        result = Err(Failure::Cancelled);
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

#[cfg(test)]
fn legacy_outcome_uncertain(store: &Store) -> bool {
    xshell_core::companion_reservation::read_reservation(&store.dir)
        .ok()
        .flatten()
        .is_some_and(|reservation| reservation.legacy_outcome_uncertain)
}
#[cfg(test)]
fn uncertain_legacy_failure(message: String) -> Failure {
    Failure::Other(format!("reconciliation_required: legacy pairing remote outcome is uncertain; startup denial does not revoke a transmitted proof or establish nonmembership ({message})"))
}

/// Stores only an initial membership or an authorized extension of the retained Ring.
/// Foreign/terminal membership is never a force-replacement or a new enrollment attempt.
#[cfg(test)]
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

#[cfg(test)]
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
    fn unfinished_after_cancel_maps_to_unfinished_copy() {
        use crate::server::adoption::StandaloneCommandError as E;
        let unfinished = |daemon_running| E::Unfinished {
            daemon_running,
            cause: "reconciliation_required: connection closed".into(),
        };
        // Ctrl-C after approval: the cancelled copy, never "the words were cancelled".
        for daemon_running in [true, false] {
            let (code, lines) =
                failure_report(standalone_failure(unfinished(daemon_running), true));
            assert_eq!(code, UNFINISHED_EXIT);
            assert_eq!(lines[0], UNFINISHED_CANCELLED);
            assert!(!lines.iter().any(|l| l == CANCELLED));
            assert_eq!(
                lines[1],
                "xshelld: reconciliation_required: connection closed"
            );
        }
        let (code, lines) = failure_report(standalone_failure(unfinished(true), false));
        assert_eq!(
            (code, lines[0].as_str()),
            (UNFINISHED_EXIT, UNFINISHED_RUNNING)
        );
        let (code, lines) = failure_report(standalone_failure(unfinished(false), false));
        assert_eq!(
            (code, lines[0].as_str()),
            (UNFINISHED_EXIT, UNFINISHED_STOPPED)
        );
        // Before consent nothing is retained: Ctrl-C keeps its own copy and exit code 1.
        let (code, lines) = failure_report(standalone_failure(
            E::Failed("connection closed".into()),
            true,
        ));
        assert_eq!((code, lines), (1, vec![CANCELLED.to_string()]));
        assert!(matches!(
            standalone_failure(E::Failed("the code expired".into()), true),
            Failure::Expired
        ));
        assert_ne!(UNFINISHED_EXIT, EXPIRED_EXIT);
        assert!(crate::cli::USAGE.contains("pair exit codes:"));
        assert!(crate::cli::USAGE.contains(&format!("  {UNFINISHED_EXIT}  ")));
        assert!(crate::cli::USAGE.contains(&format!("  {EXPIRED_EXIT}  ")));
    }
    #[test]
    fn words_block_names_this_build_version() {
        let code = DeviceCode::generate().unwrap();
        let mut standalone = Vec::new();
        print_words(&mut standalone, &code, true).unwrap();
        let lines: Vec<String> = String::from_utf8(standalone)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines[0], code.format().as_str());
        assert_eq!(
            lines[1],
            format!("xshell {} on this machine", env!("CARGO_PKG_VERSION"))
        );
        // Never mistaken for the words.
        assert!(DeviceCode::parse(&lines[1]).is_err());
        assert_eq!(lines[2..], [INSTRUCTION, LIFETIME, WAITING]);
        // The registered-Computer route: the Desktop joins, no CLI version.
        let mut registered = Vec::new();
        print_words(&mut registered, &code, false).unwrap();
        assert!(!String::from_utf8(registered)
            .unwrap()
            .contains("on this machine"));
    }
    #[test]
    fn rate_limited_maps_to_copy_and_exit_7() {
        use crate::server::adoption::StandaloneCommandError as E;
        let (code, lines) = failure_report(standalone_failure(E::RateLimited, false));
        assert_eq!(
            (code, lines),
            (RATE_LIMITED_EXIT, vec![RATE_LIMITED.to_string()])
        );
        assert!(!RATE_LIMITED.contains("reach"));
        assert!(crate::cli::USAGE.contains(&format!("  {RATE_LIMITED_EXIT}  ")));
        assert_ne!(RATE_LIMITED_EXIT, UNFINISHED_EXIT);
    }
    #[test]
    fn earlier_pairing_maps_to_copy_and_exit_6() {
        use crate::server::adoption::StandaloneCommandError as E;
        let cases = [
            (
                Failure::EarlierRunning { minutes: Some(7) },
                UNFINISHED_EXIT,
                earlier_running(7),
            ),
            (
                Failure::EarlierRunning { minutes: None },
                UNFINISHED_EXIT,
                UNFINISHED_RUNNING.to_string(),
            ),
            (
                Failure::EarlierStopped,
                UNFINISHED_EXIT,
                EARLIER_STOPPED.to_string(),
            ),
            (Failure::AlreadyRunning, 1, ALREADY_RUNNING.to_string()),
            (Failure::Busy, 1, BUSY.to_string()),
        ];
        for (failure, code, line) in cases {
            let (got, lines) = failure_report(failure);
            assert_eq!((got, lines), (code, vec![line]));
        }
        // Races after the checks map to the same copy, never a raw code.
        for (raw, code) in [
            (
                "legacy_offer_active: a legacy pairing offer is still open",
                1,
            ),
            ("running owner unavailable: No such file or directory", 1),
        ] {
            let (got, lines) = failure_report(standalone_failure(E::Failed(raw.into()), false));
            assert_eq!(got, code);
            assert!(
                lines.iter().all(|l| !l.starts_with("xshelld:")),
                "{lines:?}"
            );
        }
        assert_eq!(minutes_left(0), None);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(minutes_left(now + 30), Some(1));
        assert_eq!(minutes_left(now + 61), Some(2));
        assert_eq!(minutes_left(now + 3_600), Some(10));
        assert!(crate::cli::USAGE.contains("earlier"));
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

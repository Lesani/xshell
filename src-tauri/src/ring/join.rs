//! Computer adoption in Settings → Hosts → Ring (#87): Create ring, Join ring with its local
//! consent, the pairing window for `xshelld pair` started on this computer, Add computer by
//! eight words, and the explicit Ring upgrade. Commands `ring_create_computer`,
//! `ring_join_start`, `ring_join_confirm`, `ring_join_decline`, `ring_join_cancel`,
//! `ring_join_overview`, `ring_approve_device_code`, `ring_upgrade_computer` and
//! `ring_leave_removed`; the events
//! `ring:join` and `ring:focus`.
//!
//! The Device Code words leave Rust exactly once, in the `ring_join_start` reply. They are
//! never logged, persisted, put in an event or in an error. The confirmation lease never
//! crosses IPC: it stays in [`JoinEntry`] and the frontend only names the operation.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager as _};
use uuid::Uuid;
use xshell_hostlink::ring::adoption::{
    ApprovalReadiness, ConsentPreview, CreateConfirmationLease, JoinOffer, JoinStatus,
};
use xshell_protocol::ring::device_code::DeviceCode;
use xshell_protocol::ring::Roster;

use super::{run, status_of, worker, RingState, RingStatus, HOST_SYNC};

/// Who started a Join: this window's Join ring screen, or `xshelld pair` on this computer.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum JoinOrigin {
    Gui,
    Cli,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum JoinState {
    Waiting,
    AwaitingConfirmation,
    Completing,
    Installed,
    Declined,
    Expired,
    Failed,
    ReconciliationRequired,
    Cancelled,
}

impl JoinState {
    pub fn terminal(self) -> bool {
        !matches!(
            self,
            JoinState::Waiting | JoinState::AwaitingConfirmation | JoinState::Completing
        )
    }
}

/// What the consent card shows. No keys, no words, no lease.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JoinPreview {
    pub ring_id: String,
    pub ring_short: String,
    pub approver: Option<String>,
    pub computer: String,
    pub expires_at: u64,
}

/// The `ring:join` event and the rows of `ring_join_overview`.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JoinEvent {
    pub operation: Uuid,
    pub origin: JoinOrigin,
    pub state: JoinState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<JoinPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub expires_at: u64,
}

/// The single reply that carries the words.
pub struct JoinStarted {
    pub operation: Uuid,
    pub words: zeroize::Zeroizing<String>,
    pub expires_at: u64,
}

impl std::fmt::Debug for JoinStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinStarted")
            .field("operation", &self.operation)
            .field("words", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Serialize for JoinStarted {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("JoinStarted", 3)?;
        st.serialize_field("operation", &self.operation)?;
        st.serialize_field("words", self.words.as_str())?;
        st.serialize_field("expiresAt", &self.expires_at)?;
        st.end()
    }
}

/// `ring_join_overview`: whether Create/Join can run here, whether this window owns pairing,
/// and the joins in progress (the CLI-started ones included).
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JoinOverview {
    /// `platform`, `in-process` or `not-registered`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<&'static str>,
    pub owner: bool,
    pub joins: Vec<JoinEvent>,
}

/// `ring_approve_device_code`: the devices the approval added, and the new Roster version.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovedComputer {
    pub names: Vec<String>,
    pub version: u64,
}

// ── Pure mappings ─────────────────────────────────────────────────────────

/// The UI state of a backend Join status. A failure after the code's expiry is `expired`; a
/// failure after this window cancelled is `cancelled`.
pub fn join_state(
    status: &JoinStatus,
    cancelled: bool,
    expires_at: u64,
    now: u64,
) -> (JoinState, Option<String>) {
    match status {
        JoinStatus::Waiting => (JoinState::Waiting, None),
        JoinStatus::AwaitingConfirmation(_) => (JoinState::AwaitingConfirmation, None),
        JoinStatus::Completing => (JoinState::Completing, None),
        JoinStatus::Installed { .. } => (JoinState::Installed, None),
        JoinStatus::Declined => (JoinState::Declined, None),
        JoinStatus::Failed { .. } if cancelled => (JoinState::Cancelled, None),
        JoinStatus::Failed { .. } if now >= expires_at => (JoinState::Expired, None),
        JoinStatus::Failed { error } => (JoinState::Failed, Some(error.clone())),
        JoinStatus::ReconciliationRequired { error } => {
            (JoinState::ReconciliationRequired, Some(error.clone()))
        }
    }
}

/// `text` with every whole word of `code` masked: defence in depth, the backend's errors
/// never format a credential.
pub fn scrub(text: &str, code: &DeviceCode) -> String {
    let formatted = code.format();
    let words: HashSet<&str> = formatted.split('-').collect();
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if words.contains(word.to_ascii_lowercase().as_str()) {
            out.push_str("•••");
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphabetic() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The approving device: the proposal's signer.
pub fn approver_name(proposal: &Roster) -> Option<String> {
    proposal
        .members
        .iter()
        .find(|m| m.sign_key == proposal.signed_by)
        .map(|m| m.name.clone())
}

pub fn preview_of(p: &ConsentPreview) -> JoinPreview {
    let ring_id = p.ring_id.to_string();
    JoinPreview {
        ring_short: ring_id.chars().take(8).collect(),
        ring_id,
        approver: approver_name(&p.proposal),
        computer: p.profile.desktop.name.clone(),
        expires_at: p.expires_at,
    }
}

/// The names of the members in `after` that `before` did not have.
pub fn new_member_names(before: Option<&Roster>, after: &Roster) -> Vec<String> {
    after
        .members
        .iter()
        .filter(|m| before.is_none_or(|b| b.member(&m.sign_key).is_none()))
        .map(|m| m.name.clone())
        .collect()
}

/// `ring_approve_device_code`'s error: `invalid_code|not_approver|reconciliation_required|busy|
/// other_window|other` first, then the backend's text (which never holds the words).
pub fn approve_error(error: &str) -> String {
    const CODES: [&str; 5] = [
        "invalid_code",
        "not_approver",
        "reconciliation_required",
        "busy",
        "other_window",
    ];
    if CODES.iter().any(|c| error.starts_with(c)) {
        return error.to_string();
    }
    let code = if error == "another adoption operation is active" {
        "busy"
    } else if error.starts_with("another process owns") {
        "other_window"
    } else if error.contains("approval requires") || error.contains("no retained approver profile")
    {
        "not_approver"
    } else {
        "other"
    };
    format!("{code}: {error}")
}

/// A Create/Join/Upgrade start error: a local Host that is registered but not serving yet
/// (first run, Daemon still starting) is `not_registered:` like a missing registration, so
/// the page shows "not ready yet" instead of a socket error. Other errors pass unchanged.
pub fn start_error(error: String) -> String {
    const NOT_READY: [&str; 2] = [
        "protected counterpart unavailable",
        "protected counterpart readiness unavailable",
    ];
    if NOT_READY.iter().any(|p| error.starts_with(p)) {
        format!("not_registered: {error}")
    } else {
        error
    }
}

/// The local Host's client once it is registered and its endpoint exists (Unix: the socket
/// file). Before that `not_registered:`, which `ring_join_overview` reports as
/// `not-registered` so Create/Join stay disabled.
fn ready_local_client(
    ring: &xshell_hostlink::ring::DesktopRing,
) -> Result<xshell_hostlink::ring::companion_ipc::CompanionClient, String> {
    let client = ring.local_companion_client()?;
    #[cfg(unix)]
    if std::fs::symlink_metadata(client.endpoint()).is_err() {
        return Err("not_registered: this computer's Host is not running yet".into());
    }
    Ok(client)
}

/// `RingStatus.approval`.
pub fn approval_text(readiness: ApprovalReadiness, local_available: bool) -> &'static str {
    match readiness {
        ApprovalReadiness::Ready => "ready",
        ApprovalReadiness::NeedsUpgrade | ApprovalReadiness::NeedsCompletion
            if !local_available =>
        {
            "unavailable"
        }
        ApprovalReadiness::NeedsUpgrade => "needs-upgrade",
        ApprovalReadiness::NeedsCompletion => "needs-completion",
        ApprovalReadiness::Unresolved => "unresolved",
        ApprovalReadiness::Removed => "removed",
        ApprovalReadiness::NoProfile | ApprovalReadiness::NotEnrolled => "none",
    }
}

/// The joins to cancel when this process's window loses focus (BINDING AMENDMENT 3): every
/// GUI join and start. Each xshell window is its own process, so losing focus covers
/// switching to another xshell window. CLI-started offers belong to the terminal.
pub fn gui_joins_to_cancel_on_blur(entries: &[(Uuid, JoinOrigin)]) -> Vec<Uuid> {
    entries
        .iter()
        .filter(|(_, origin)| *origin == JoinOrigin::Gui)
        .map(|(op, _)| *op)
        .collect()
}

// ── The registry ──────────────────────────────────────────────────────────

pub struct JoinEntry {
    pub origin: JoinOrigin,
    offer: Arc<JoinOffer>,
    lease: Mutex<Option<(String, CreateConfirmationLease)>>,
    preview: Mutex<Option<JoinPreview>>,
    cancelled: AtomicBool,
    last: Mutex<Option<JoinEvent>>,
}

impl JoinEntry {
    fn new(origin: JoinOrigin, offer: Arc<JoinOffer>) -> Self {
        Self {
            origin,
            offer,
            lease: Mutex::new(None),
            preview: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            last: Mutex::new(None),
        }
    }

    /// The current event; takes the lease once the verified preview is up.
    fn observe(&self, operation: Uuid, now: u64) -> JoinEvent {
        let status = self.offer.join_status();
        if matches!(status, JoinStatus::AwaitingConfirmation(_)) {
            let mut lease = self.lease.lock().unwrap_or_else(|e| e.into_inner());
            if lease.is_none() {
                if let Ok((p, l)) = self.offer.await_consent(std::time::Duration::ZERO) {
                    *self.preview.lock().unwrap_or_else(|e| e.into_inner()) = Some(preview_of(&p));
                    *lease = Some((p.proposal_hash.clone(), l));
                }
            }
        }
        let (state, error) = join_state(
            &status,
            self.cancelled.load(Ordering::Acquire),
            self.offer.expires_at,
            now,
        );
        let preview = match state {
            JoinState::AwaitingConfirmation | JoinState::Completing => self
                .preview
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            _ => None,
        };
        JoinEvent {
            operation,
            origin: self.origin,
            state,
            preview,
            error: error.map(|e| scrub(&e, &self.offer.code)),
            expires_at: self.offer.expires_at,
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some((_, lease)) = &*self.lease.lock().unwrap_or_else(|e| e.into_inner()) {
            lease.cancel();
        }
        self.offer.cancel_join();
    }
}

#[derive(Default)]
struct JoinInner {
    entries: HashMap<Uuid, Arc<JoinEntry>>,
    /// GUI starts not returned yet: their cancel flag.
    starting: HashMap<Uuid, Arc<AtomicBool>>,
    /// CLI operations already tracked (so a finished one listed a while longer is not
    /// tracked again).
    seen_cli: HashSet<Uuid>,
}

#[derive(Default)]
pub struct JoinRegistry {
    inner: Mutex<JoinInner>,
}

impl JoinRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, JoinInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserves a GUI start before the backend runs, so a cancel during the start is seen.
    pub fn reserve(&self, operation: Uuid) -> Result<Arc<AtomicBool>, String> {
        let mut inner = self.lock();
        if inner.entries.contains_key(&operation) || inner.starting.contains_key(&operation) {
            return Err("busy: this Join is already running".into());
        }
        let flag = Arc::new(AtomicBool::new(false));
        inner.starting.insert(operation, flag.clone());
        Ok(flag)
    }

    /// Ends a start: `Err("cancelled")` if it was cancelled meanwhile (`make` is not called),
    /// else the entry `make` builds, registered under the same lock.
    pub fn finish_start(
        &self,
        operation: Uuid,
        make: impl FnOnce() -> Arc<JoinEntry>,
    ) -> Result<Arc<JoinEntry>, String> {
        let mut inner = self.lock();
        let flag = inner
            .starting
            .remove(&operation)
            .ok_or("cancelled: Join start was not reserved")?;
        if flag.load(Ordering::Acquire) {
            return Err("cancelled: Join cancelled before its code was shown".into());
        }
        let entry = make();
        inner.entries.insert(operation, entry.clone());
        Ok(entry)
    }

    /// A start that failed: forget its reservation.
    pub fn abandon_start(&self, operation: Uuid) {
        self.lock().starting.remove(&operation);
    }

    /// Cancels a GUI start or a GUI join. CLI joins belong to the terminal: no-op.
    pub fn cancel(&self, operation: Uuid) {
        let entry = {
            let inner = self.lock();
            if let Some(flag) = inner.starting.get(&operation) {
                flag.store(true, Ordering::Release);
                return;
            }
            inner.entries.get(&operation).cloned()
        };
        if let Some(entry) = entry.filter(|e| e.origin == JoinOrigin::Gui) {
            entry.cancel();
        }
    }

    pub fn get(&self, operation: Uuid) -> Option<Arc<JoinEntry>> {
        self.lock().entries.get(&operation).cloned()
    }

    fn remove(&self, operation: Uuid) -> Option<Arc<JoinEntry>> {
        self.lock().entries.remove(&operation)
    }

    /// The CLI offers seen for the first time: tracked, and returned to watch. Forgets the
    /// tombstones of offers no longer listed.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn track_cli(&self, listed: Vec<(Uuid, Arc<JoinOffer>)>) -> Vec<(Uuid, Arc<JoinEntry>)> {
        let mut inner = self.lock();
        let ids: HashSet<Uuid> = listed.iter().map(|(op, _)| *op).collect();
        inner.seen_cli.retain(|op| ids.contains(op));
        let mut new = vec![];
        for (op, offer) in listed {
            if inner.seen_cli.insert(op) && !inner.entries.contains_key(&op) {
                let entry = Arc::new(JoinEntry::new(JoinOrigin::Cli, offer));
                inner.entries.insert(op, entry.clone());
                new.push((op, entry));
            }
        }
        new
    }

    /// The window lost focus: cancels every GUI start and join.
    pub fn cancel_on_blur(&self) -> Vec<Uuid> {
        let rows: Vec<(Uuid, JoinOrigin)> = {
            let inner = self.lock();
            inner
                .entries
                .iter()
                .map(|(op, e)| (*op, e.origin))
                .chain(inner.starting.keys().map(|op| (*op, JoinOrigin::Gui)))
                .collect()
        };
        let ops = gui_joins_to_cancel_on_blur(&rows);
        for op in &ops {
            self.cancel(*op);
        }
        ops
    }

    fn events(&self) -> Vec<JoinEvent> {
        let entries: Vec<_> = self.lock().entries.values().cloned().collect();
        entries
            .iter()
            .filter_map(|e| e.last.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .collect()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn emit_join<R: tauri::Runtime>(app: &AppHandle<R>, event: &JoinEvent) {
    let _ = app.emit("ring:join", event);
}

/// Polls one join every 150 ms, emits `ring:join` on every change, and forgets it at its end.
fn watch<R: tauri::Runtime>(
    app: AppHandle<R>,
    registry: Arc<JoinRegistry>,
    operation: Uuid,
    entry: Arc<JoinEntry>,
) {
    let spawned = std::thread::Builder::new()
        .name("ring-join-watch".into())
        .spawn(move || loop {
            let event = entry.observe(operation, now());
            let changed = {
                let mut last = entry.last.lock().unwrap_or_else(|e| e.into_inner());
                let changed = last.as_ref() != Some(&event);
                *last = Some(event.clone());
                changed
            };
            if changed {
                emit_join(&app, &event);
            }
            if event.state.terminal() {
                // Dropping the last reference joins the (finished) driver: off any UI thread.
                drop(registry.remove(operation));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        });
    if let Err(error) = spawned {
        eprintln!("xshell: cannot watch a Join: {error}");
    }
}

/// From the 100 ms owner thread: tracks Joins that `xshelld pair` started on this computer.
#[cfg(unix)]
pub fn track_cli_joins(app: &AppHandle, ring: &xshell_hostlink::ring::DesktopRing) {
    let Some(state) = app.try_state::<RingState>() else {
        return;
    };
    let listed = ring.administration_joins();
    if listed.is_empty() && state.joins.lock().seen_cli.is_empty() {
        return;
    }
    for (op, entry) in state.joins.track_cli(listed) {
        watch(app.clone(), state.joins.clone(), op, entry);
    }
}

/// This window lost focus: its GUI codes die (switching to another window, BINDING
/// AMENDMENT 3).
pub fn window_blurred<R: tauri::Runtime>(app: &AppHandle<R>) {
    if let Some(state) = app.try_state::<RingState>() {
        state.joins.cancel_on_blur();
    }
}

fn local_in_process(app: &AppHandle) -> bool {
    app.try_state::<crate::hosts::Hosts>().is_some_and(|h| {
        matches!(
            h.local_mode,
            crate::hosts::local::LocalMode::InProcess { .. }
        )
    })
}

/// Recomputes `RingStatus.approval` and stores it for `status_of`. Never from inside the
/// RingObserver callback (it reads the store).
pub fn refresh_approval(app: &AppHandle) -> &'static str {
    let Some(state) = app.try_state::<RingState>() else {
        return "none";
    };
    let local = cfg!(unix) && !local_in_process(app);
    let text = state
        .worker
        .ring
        .approval_readiness()
        .map(|r| approval_text(r, local))
        .unwrap_or("none");
    *state.approval.lock().unwrap_or_else(|e| e.into_inner()) = text;
    *state.format.lock().unwrap_or_else(|e| e.into_inner()) =
        state.worker.ring.ring_format().ok().flatten();
    *state.busy.lock().unwrap_or_else(|e| e.into_inner()) = state.worker.ring.adoption_busy();
    text
}

fn ensure_local(app: &AppHandle) -> Result<(), String> {
    if !cfg!(unix) {
        return Err("unavailable: platform".into());
    }
    if local_in_process(app) {
        return Err("unavailable: in-process".into());
    }
    Ok(())
}

fn confirm_local(
    p: &ConsentPreview,
    lease: &CreateConfirmationLease,
) -> Result<xshell_hostlink::ring::adoption::CreateDecision, String> {
    // The click on Create ring / Upgrade Ring is the explicit local consent for exactly the
    // preview the backend presents.
    Ok(xshell_hostlink::ring::adoption::CreateDecision::Confirm(
        lease.confirm(&p.proposal_hash)?,
    ))
}

// ── Commands ──────────────────────────────────────────────────────────────

/// The Relay chosen on the unpaired screen: `None` (hosted default) or a valid URL.
pub fn chosen_relay(relay_url: Option<String>) -> Result<Option<String>, String> {
    match relay_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
    {
        None => Ok(None),
        Some(url) => {
            xshell_protocol::ring::url::RelayUrl::parse(&url)
                .map_err(|e| format!("invalid_relay: {e}"))?;
            Ok(Some(url))
        }
    }
}

/// Create ring: this Computer Device (Desktop and its Local Host) becomes the Ring's first
/// member, on `relay_url` if the user chose their own Relay. The click is the local consent.
#[tauri::command]
pub async fn ring_create_computer(
    app: AppHandle,
    relay_url: Option<String>,
) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    ensure_local(&app)?;
    let relay = chosen_relay(relay_url)?;
    run(move || {
        let client = ready_local_client(&w.ring)?;
        let outcome = match &relay {
            Some(url) => w.ring.create_computer_on(&client, url, confirm_local),
            None => w.ring.create_computer(&client, confirm_local),
        }
        .map_err(start_error)?;
        match outcome {
            xshell_hostlink::ring::adoption::AdoptionOutcome::Installed { .. } => {}
            xshell_hostlink::ring::adoption::AdoptionOutcome::Declined { .. } => {
                return Err("declined".into())
            }
        }
        HOST_SYNC.poke_all();
        refresh_approval(&app);
        Ok(status_of(&app, w.ring.view()))
    })
    .await
}

/// Join ring: shows a Device Code. Returns once the Relay holds its slot; progress arrives
/// as `ring:join`. Cancelled by `ring_join_cancel`, also while it starts.
#[tauri::command]
pub async fn ring_join_start(
    app: AppHandle,
    operation: Uuid,
    relay_url: Option<String>,
) -> Result<JoinStarted, String> {
    let w = worker(&app)?;
    ensure_local(&app)?;
    let relay = chosen_relay(relay_url)?;
    let joins = app
        .try_state::<RingState>()
        .map(|s| s.joins.clone())
        .ok_or("mobile access is unavailable")?;
    let flag = joins.reserve(operation)?;
    let watch_app = app.clone();
    run(move || {
        let offer = ready_local_client(&w.ring).and_then(|client| {
            w.ring
                .begin_join_cancellable(&client, flag, relay.as_deref())
                .map_err(start_error)
        });
        let offer = match offer {
            Ok(offer) => Arc::new(offer),
            Err(error) => {
                joins.abandon_start(operation);
                return Err(error);
            }
        };
        let started = JoinStarted {
            operation,
            words: offer.code.format(),
            expires_at: offer.expires_at,
        };
        match joins.finish_start(operation, || {
            Arc::new(JoinEntry::new(JoinOrigin::Gui, offer.clone()))
        }) {
            Ok(entry) => {
                watch(watch_app, joins.clone(), operation, entry);
                Ok(started)
            }
            Err(error) => {
                offer.cancel_join();
                drop(offer); // joins the driver
                Err(error)
            }
        }
    })
    .await
}

fn entry_of(app: &AppHandle, operation: Uuid) -> Result<Arc<JoinEntry>, String> {
    app.try_state::<RingState>()
        .and_then(|s| s.joins.get(operation))
        .ok_or_else(|| "not_found: this Join has ended".to_string())
}

/// Confirm on the consent card: the lease taken for the presented preview, never one from IPC.
#[tauri::command]
pub async fn ring_join_confirm(app: AppHandle, operation: Uuid) -> Result<(), String> {
    let entry = entry_of(&app, operation)?;
    run(move || {
        let confirmation = {
            let lease = entry.lease.lock().unwrap_or_else(|e| e.into_inner());
            let (hash, lease) = lease
                .as_ref()
                .ok_or("not_ready: no verified request to confirm")?;
            lease.confirm(hash)?
        };
        entry.offer.confirm_join(confirmation)
    })
    .await
}

#[tauri::command]
pub async fn ring_join_decline(app: AppHandle, operation: Uuid) -> Result<(), String> {
    let entry = entry_of(&app, operation)?;
    run(move || entry.offer.decline_join()).await
}

/// Leaving the Join screen: the code dies with it. No-op for CLI-started offers.
#[tauri::command]
pub async fn ring_join_cancel(app: AppHandle, operation: Uuid) -> Result<(), String> {
    if let Some(state) = app.try_state::<RingState>() {
        state.joins.cancel(operation);
    }
    Ok(())
}

#[tauri::command]
pub async fn ring_join_overview(app: AppHandle) -> Result<JoinOverview, String> {
    let w = worker(&app)?;
    run(move || {
        let unavailable = if !cfg!(unix) {
            Some("platform")
        } else if local_in_process(&app) {
            Some("in-process")
        } else if ready_local_client(&w.ring).is_err() {
            Some("not-registered")
        } else {
            None
        };
        let joins = app
            .try_state::<RingState>()
            .map(|s| s.joins.events())
            .unwrap_or_default();
        Ok(JoinOverview {
            unavailable,
            owner: w.ring.pairing_ipc_owner(),
            joins,
        })
    })
    .await
}

/// Add computer: approves the eight words the other computer shows.
#[tauri::command]
pub async fn ring_approve_device_code(
    app: AppHandle,
    code: String,
) -> Result<ApprovedComputer, String> {
    let code = zeroize::Zeroizing::new(code);
    let w = worker(&app)?;
    let parsed = DeviceCode::parse(&code).map_err(|e| format!("invalid_code: {e}"))?;
    drop(code);
    run(move || {
        let before = w.ring.chain().map(|c| c.head().roster().clone());
        let outcome = w
            .ring
            .approve_device_code(&parsed)
            .map_err(|e| approve_error(&scrub(&e, &parsed)))?;
        HOST_SYNC.poke_all();
        refresh_approval(&app);
        let head = outcome.chain.head();
        Ok(ApprovedComputer {
            names: new_member_names(before.as_ref(), head.roster()),
            version: head.version(),
        })
    })
    .await
}

/// Upgrade Ring: a retained v1 Ring is upgraded and this Computer completed; one click is the
/// consent for both. A v2 Ring that only needs completion is completed.
#[tauri::command]
pub async fn ring_upgrade_computer(app: AppHandle) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    ensure_local(&app)?;
    run(move || {
        let readiness = w.ring.approval_readiness()?;
        let result = (|| match readiness {
            ApprovalReadiness::Ready => Ok(()),
            ApprovalReadiness::Unresolved => {
                Err("reconciliation_required: earlier Ring work is unresolved".to_string())
            }
            ApprovalReadiness::NoProfile | ApprovalReadiness::NotEnrolled => {
                Err("not_enrolled: this Desktop is not in a Ring".to_string())
            }
            ApprovalReadiness::Removed => {
                Err("not_enrolled: this Desktop is no longer a member of its Ring".to_string())
            }
            ApprovalReadiness::NeedsUpgrade => {
                let client = ready_local_client(&w.ring)?;
                w.ring
                    .upgrade_retained_ring(&client, confirm_local)
                    .map_err(start_error)?;
                w.ring.finish_retained_computer(&client, confirm_local)?;
                Ok(())
            }
            ApprovalReadiness::NeedsCompletion => {
                let client = ready_local_client(&w.ring)?;
                w.ring
                    .finish_retained_computer(&client, confirm_local)
                    .map_err(start_error)?;
                Ok(())
            }
        })();
        HOST_SYNC.poke_all();
        refresh_approval(&app);
        result.map(|()| status_of(&app, w.ring.view()))
    })
    .await
}

/// Leave this Ring: only after another device removed this computer (approval `removed`).
/// Local-only cleanup of both identities' membership; the old Ring is not contacted.
#[tauri::command]
pub async fn ring_leave_removed(app: AppHandle) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    ensure_local(&app)?;
    run(move || {
        let result = w
            .ring
            .local_companion_client()
            .and_then(|client| w.ring.leave_removed(&client).map_err(start_error));
        HOST_SYNC.poke_all();
        refresh_approval(&app);
        result.map(|()| status_of(&app, w.ring.view()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_protocol::ring::{DeviceKeys, Member, RingId, Role};

    // Deterministic: a random code could share a word with a JSON key.
    fn fixed_code() -> DeviceCode {
        DeviceCode::from_bytes([
            0x5a, 0x13, 0xc7, 0x02, 0x9e, 0x41, 0xb8, 0x6d, 0x20, 0xf3, 0x77, 0x0c,
        ])
    }

    fn words_in(text: &str, code: &DeviceCode) -> bool {
        let formatted = code.format();
        let words: HashSet<&str> = formatted.split('-').collect();
        text.to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphabetic())
            .any(|token| words.contains(token))
    }

    fn key(n: u8) -> DeviceKeys {
        DeviceKeys::from_seeds(&[n; 32], &[n.wrapping_add(100); 32])
    }

    fn roster(signer: &DeviceKeys, others: &[(&str, &DeviceKeys)]) -> Roster {
        let mut members = vec![Member::new(
            "approver desk",
            Role::Desktop,
            signer.sign_key(),
            signer.noise_key(),
            1,
        )];
        for (name, k) in others {
            members.push(Member::new(
                name,
                Role::Daemon,
                k.sign_key(),
                k.noise_key(),
                1,
            ));
        }
        Roster {
            v: 2,
            ring_id: RingId::derive(&signer.sign_key()),
            version: 1,
            prev: None,
            relay_url: "wss://relay.example".into(),
            signed_by: signer.sign_key(),
            issued_at: 1,
            members,
            extra: Default::default(),
        }
    }

    #[test]
    fn join_event_is_camel_case_and_has_no_words() {
        let code = fixed_code();
        assert!(!words_in(
            "operation origin cli gui state awaitingConfirmation preview ringId ringShort approver computer expiresAt error relay laptop desk",
            &code
        ));
        let event = JoinEvent {
            operation: Uuid::from_u128(7),
            origin: JoinOrigin::Cli,
            state: JoinState::AwaitingConfirmation,
            preview: Some(JoinPreview {
                ring_id: "abcdefghijkl".into(),
                ring_short: "abcdefgh".into(),
                approver: Some("desk".into()),
                computer: "laptop".into(),
                expires_at: 9,
            }),
            error: Some(scrub(&format!("relay: {}", code.format().as_str()), &code)),
            expires_at: 9,
        };
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["state"], "awaitingConfirmation");
        assert_eq!(v["origin"], "cli");
        assert_eq!(v["expiresAt"], 9);
        assert_eq!(v["preview"]["ringShort"], "abcdefgh");
        assert_eq!(v["preview"]["expiresAt"], 9);
        assert!(v.get("words").is_none());
        assert!(!words_in(&v.to_string(), &code), "an event carries a word");
        let reconcile = JoinEvent {
            state: JoinState::ReconciliationRequired,
            preview: None,
            error: None,
            ..event
        };
        let v = serde_json::to_value(&reconcile).unwrap();
        assert_eq!(v["state"], "reconciliationRequired");
        assert!(v.get("preview").is_none() && v.get("error").is_none());
    }

    #[test]
    fn join_started_debug_redacts_words_and_serializes_once() {
        let code = fixed_code();
        let started = JoinStarted {
            operation: Uuid::nil(),
            words: code.format(),
            expires_at: 5,
        };
        let debug = format!("{started:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!words_in(&debug, &code), "Debug leaks a word");
        let v = serde_json::to_value(&started).unwrap();
        assert_eq!(v["words"], code.format().as_str());
        assert_eq!(v["expiresAt"], 5);
    }

    #[test]
    fn failed_after_expiry_is_expired_and_cancel_wins() {
        let failed = JoinStatus::Failed {
            error: "cancelled".into(),
        };
        assert_eq!(
            join_state(&failed, false, 100, 100),
            (JoinState::Expired, None)
        );
        assert_eq!(
            join_state(&failed, false, 100, 50),
            (JoinState::Failed, Some("cancelled".into()))
        );
        assert_eq!(
            join_state(&failed, true, 100, 50),
            (JoinState::Cancelled, None)
        );
        assert_eq!(
            join_state(&failed, true, 100, 200),
            (JoinState::Cancelled, None)
        );
    }

    #[test]
    fn reconciliation_and_declined_mapping() {
        assert_eq!(
            join_state(
                &JoinStatus::ReconciliationRequired {
                    error: "reconciliation_required: x".into()
                },
                true,
                100,
                200
            ),
            (
                JoinState::ReconciliationRequired,
                Some("reconciliation_required: x".into())
            )
        );
        assert_eq!(
            join_state(&JoinStatus::Declined, false, 100, 200),
            (JoinState::Declined, None)
        );
        assert_eq!(
            join_state(&JoinStatus::Installed { version: 2 }, false, 100, 200),
            (JoinState::Installed, None)
        );
        assert_eq!(
            join_state(&JoinStatus::Completing, false, 100, 200),
            (JoinState::Completing, None)
        );
        assert!(!JoinState::Completing.terminal());
        assert!(!JoinState::AwaitingConfirmation.terminal());
        for s in [
            JoinState::Installed,
            JoinState::Declined,
            JoinState::Expired,
            JoinState::Failed,
            JoinState::ReconciliationRequired,
            JoinState::Cancelled,
        ] {
            assert!(s.terminal());
        }
    }

    #[test]
    fn tauri_join_delivery_preserves_operation_identity() {
        use tauri::Listener;
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.listen_any("ring:join", move |event| {
            tx.send(event.payload().to_string()).unwrap();
        });
        let operation = Uuid::from_u128(87);
        for state in [JoinState::Waiting, JoinState::Expired] {
            emit_join(
                app.handle(),
                &JoinEvent {
                    operation,
                    origin: JoinOrigin::Gui,
                    state,
                    preview: None,
                    error: None,
                    expires_at: 1,
                },
            );
        }
        for state in ["waiting", "expired"] {
            let payload: serde_json::Value =
                serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap())
                    .unwrap();
            assert_eq!(payload["operation"], operation.to_string());
            assert_eq!(payload["state"], state);
        }
    }

    #[test]
    fn cancel_while_starting_sets_the_flag_and_refuses_the_late_offer() {
        let registry = JoinRegistry::default();
        let op = Uuid::from_u128(1);
        let flag = registry.reserve(op).unwrap();
        assert!(registry.reserve(op).is_err(), "one start per operation");
        registry.cancel(op);
        assert!(
            flag.load(Ordering::Acquire),
            "cancel reaches the starting backend"
        );
        let late =
            registry.finish_start(op, || unreachable!("a cancelled start registers nothing"));
        assert!(late.err().expect("cancelled").starts_with("cancelled"));
        assert!(registry.get(op).is_none());
        // Unknown operations: no-op.
        registry.cancel(Uuid::from_u128(2));
        // Losing focus cancels a GUI start.
        let other = registry.reserve(Uuid::from_u128(3)).unwrap();
        assert_eq!(registry.cancel_on_blur(), vec![Uuid::from_u128(3)]);
        assert!(other.load(Ordering::Acquire));
    }

    #[test]
    fn blur_cancels_every_gui_join_and_no_cli_offer() {
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let rows = [
            (a, JoinOrigin::Gui),
            (b, JoinOrigin::Cli),
            (c, JoinOrigin::Gui),
        ];
        assert_eq!(gui_joins_to_cancel_on_blur(&rows), vec![a, c]);
        assert_eq!(
            gui_joins_to_cancel_on_blur(&[(b, JoinOrigin::Cli)]),
            Vec::<Uuid>::new()
        );
    }

    #[test]
    fn chosen_relay_is_validated_and_empty_means_hosted() {
        assert_eq!(chosen_relay(None), Ok(None));
        assert_eq!(chosen_relay(Some("  ".into())), Ok(None));
        assert_eq!(
            chosen_relay(Some(" wss://relay.example.com ".into())),
            Ok(Some("wss://relay.example.com".into()))
        );
        assert!(chosen_relay(Some("http://x".into()))
            .unwrap_err()
            .starts_with("invalid_relay: "));
    }

    #[test]
    fn approver_is_the_member_that_signed_the_proposal() {
        let (signer, daemon) = (key(1), key(2));
        let r = roster(&signer, &[("approver host", &daemon)]);
        assert_eq!(approver_name(&r).as_deref(), Some("approver desk"));
        let mut foreign = r.clone();
        foreign.signed_by = key(3).sign_key();
        assert_eq!(approver_name(&foreign), None);
    }

    #[test]
    fn host_not_serving_yet_is_not_registered() {
        for raw in [
            "protected counterpart unavailable: No such file or directory (os error 2)",
            "protected counterpart unavailable: Connection refused (os error 111)",
            "protected counterpart readiness unavailable",
        ] {
            let mapped = start_error(raw.to_string());
            assert!(mapped.starts_with("not_registered: "), "{mapped}");
            assert!(mapped.ends_with(raw));
        }
        for kept in [
            "reconciliation_required: unresolved",
            "another adoption operation is active",
            "declined",
        ] {
            assert_eq!(start_error(kept.to_string()), kept);
        }
    }

    #[test]
    fn approve_error_codes() {
        assert_eq!(
            approve_error("another adoption operation is active"),
            "busy: another adoption operation is active"
        );
        assert!(approve_error("another process owns approval").starts_with("other_window: "));
        assert!(
            approve_error("approval requires enrolled membership").starts_with("not_approver: ")
        );
        assert!(
            approve_error("approval requires an authorized v2 Computer Desktop")
                .starts_with("not_approver: ")
        );
        assert!(approve_error("no retained approver profile").starts_with("not_approver: "));
        assert_eq!(
            approve_error("reconciliation_required: an earlier proposal is unresolved"),
            "reconciliation_required: an earlier proposal is unresolved"
        );
        assert!(approve_error("nobody is waiting with this code").starts_with("other: "));
    }

    #[test]
    fn invalid_code_error_has_no_words() {
        let words = "Apple-ZEBRA ocean notaword river stone cloud lamp";
        let error = DeviceCode::parse(words)
            .map(|_| ())
            .map_err(|e| format!("invalid_code: {e}"))
            .unwrap_err();
        assert!(error.starts_with("invalid_code: "));
        let lower = error.to_lowercase();
        for w in words.split([' ', '-']) {
            assert!(!lower.contains(&w.to_lowercase()), "{error}");
        }
        assert!(DeviceCode::parse("one two three").is_err());
        // Scrubbing masks whole words of a real code, whatever their case.
        let code = fixed_code();
        let first = code.format().split('-').next().unwrap().to_uppercase();
        assert_eq!(scrub(&format!("x {first}."), &code), "x •••.");
    }

    #[test]
    fn approved_names_are_the_new_members_of_the_head() {
        let (signer, old, new_desk, new_host) = (key(1), key(2), key(3), key(4));
        let before = roster(&signer, &[("old host", &old)]);
        let after = roster(
            &signer,
            &[
                ("old host", &old),
                ("laptop", &new_desk),
                ("laptop host", &new_host),
            ],
        );
        assert_eq!(
            new_member_names(Some(&before), &after),
            vec!["laptop".to_string(), "laptop host".to_string()]
        );
        assert_eq!(new_member_names(Some(&after), &after), Vec::<String>::new());
    }

    #[test]
    fn approval_status_text() {
        use ApprovalReadiness::*;
        assert_eq!(approval_text(Ready, true), "ready");
        assert_eq!(approval_text(NeedsUpgrade, true), "needs-upgrade");
        assert_eq!(approval_text(NeedsCompletion, true), "needs-completion");
        assert_eq!(approval_text(NeedsUpgrade, false), "unavailable");
        assert_eq!(approval_text(Unresolved, true), "unresolved");
        assert_eq!(approval_text(Removed, true), "removed");
        assert_eq!(approval_text(Removed, false), "removed");
        assert_eq!(approval_text(NotEnrolled, true), "none");
        assert_eq!(approval_text(NoProfile, false), "none");
    }
}

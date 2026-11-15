//! The quit prompt.
//!
//! Always-on Daemon (Linux, macOS; ADR-0017), kind `keep`: closing the last Desktop window
//! on this machine while the Local Host has running Terminals asks "Keep N sessions
//! running?" (Keep running / Stop them / cancel, with "Do not ask again" for Keep). "Stop
//! them" ends exactly the Terminals the prompt counted (A9); the Daemon itself keeps running
//! either way. Interception is off until the frontend calls `local_quit_prompt_ready` (after
//! it listens for `local:quit-prompt`). An OS shutdown sends SIGTERM without a close
//! request: no prompt, the sessions are kept.
//!
//! Window-bound Daemon (Windows; W4), kind `close`: closing the process that owns the Local
//! Host's Daemon (its last window) while that Daemon has Terminals, or may have (the count is
//! asked for at close, briefly; still unknown asks without a number, A2), confirms "This will
//! close N sessions" (Close / Cancel). A process that does not own the Daemon, the updater's
//! relaunch and an OS shutdown or logoff close without asking. A prompt raised before the
//! frontend listens is held and shown when it does.
//!
//! Never after `local_quit_bypass` (the updater's relaunch).

use super::local::{read_keep_sessions, KeepSessions, KEEP_SESSIONS_KEY};
use super::presence::Presence;
use super::Hosts;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager as _};
use uuid::Uuid;
use xshell_hostlink::LOCAL_HOST_ID;
use xshell_protocol::msg::TerminalInfo;

/// How long "Stop them" waits for the Terminals to end.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// How long a closing Desktop waits for another one's election.
const ELECTION_WAIT: Duration = Duration::from_secs(2);

/// How long a close waits for the Local Host's Terminal list when it is not known yet
/// (window-bound, A2).
const LIST_WAIT: Duration = Duration::from_secs(2);

/// How the Local Host's Daemon lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// Linux and macOS: the always-on Daemon outlives every window (ADR-0017).
    AlwaysOn,
    /// Windows: the Daemon ends with the Desktop process that started it (W1).
    WindowBound,
}

impl Lifetime {
    pub fn of_platform() -> Lifetime {
        if cfg!(windows) {
            Lifetime::WindowBound
        } else {
            Lifetime::AlwaysOn
        }
    }
}

/// What decides whether closing asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseInputs {
    pub lifetime: Lifetime,
    /// The frontend listens for the prompt (always-on only: a window-bound prompt waits).
    pub attached: bool,
    /// The updater relaunches.
    pub bypass: bool,
    pub keep: KeepSessions,
    /// No other Desktop process runs on this machine.
    pub last_desktop: bool,
    /// This is the process's last window.
    pub last_window: bool,
    /// Running Terminals of the Local Host; `None` while their list is unknown.
    pub running: Option<usize>,
    /// This process started the running window-bound Daemon: closing it ends the Daemon.
    pub owner: bool,
    /// The OS is shutting down or logging off.
    pub session_ending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    Close,
    /// Always-on: "Keep N sessions running?".
    Ask(usize),
    /// Window-bound: "This will close N sessions"; `None`: the count is unknown.
    Confirm(Option<usize>),
}

/// Asks only when every gate holds.
pub fn decide(i: &CloseInputs) -> CloseDecision {
    if i.lifetime == Lifetime::WindowBound {
        // Unknown is not zero (A2): only known-zero, the updater and the OS close silently.
        return match i.running {
            _ if i.bypass || i.session_ending || !i.owner || !i.last_window => CloseDecision::Close,
            Some(0) => CloseDecision::Close,
            n => CloseDecision::Confirm(n),
        };
    }
    match i.running {
        Some(n)
            if n > 0
                && i.attached
                && !i.bypass
                && i.keep == KeepSessions::Ask
                && i.last_desktop
                && i.last_window =>
        {
            CloseDecision::Ask(n)
        }
        _ => CloseDecision::Close,
    }
}

/// The running Terminals (no exit code yet) of a list.
pub fn running_ids(list: Option<&[TerminalInfo]>) -> Option<Vec<Uuid>> {
    list.map(|l| {
        l.iter()
            .filter(|t| t.exit_code.is_none())
            .map(|t| t.terminal)
            .collect()
    })
}

/// "Stop them": the shown Terminals still running are stopped; those started since are
/// `added` (asked about again). Terminals that ended meanwhile are simply gone.
pub fn plan_stop(shown: &[Uuid], now: &[Uuid]) -> (Vec<Uuid>, Vec<Uuid>) {
    let stop = shown.iter().filter(|t| now.contains(t)).copied().collect();
    let added = now.iter().filter(|t| !shown.contains(t)).copied().collect();
    (stop, added)
}

/// How a "Stop them" ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// `n` of the shown Terminals did not end: the window stays.
    Failed(usize),
    /// The shown ones ended, but these were started meanwhile (read after the stop): ask again.
    Added(Vec<Uuid>),
    /// Nothing runs any more that was not shown: close.
    Done,
}

/// Stop the shown Terminals that still run (`stop` returns how many did not end), then read
/// the authoritative running set again: anything not shown is `Added`.
pub fn run_stop(
    shown: &[Uuid],
    running: &dyn Fn() -> Option<Vec<Uuid>>,
    stop: &dyn Fn(&[Uuid]) -> usize,
) -> StopOutcome {
    let now = running().unwrap_or_default();
    let (to_stop, _) = plan_stop(shown, &now);
    let left = stop(&to_stop);
    if left > 0 {
        return StopOutcome::Failed(left);
    }
    let after = running().unwrap_or_default();
    let (_, added) = plan_stop(shown, &after);
    if added.is_empty() {
        StopOutcome::Done
    } else {
        StopOutcome::Added(added)
    }
}

/// Which question a prompt asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Always-on: Keep running / Stop them.
    Keep,
    /// Window-bound: Close (ends them) / Cancel.
    Close,
}

impl PromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptKind::Keep => "keep",
            PromptKind::Close => "close",
        }
    }
}

/// Whether `choice` answers a prompt of `kind`.
pub fn valid_choice(kind: PromptKind, choice: &str) -> bool {
    match kind {
        PromptKind::Keep => matches!(choice, "keep" | "stop" | "cancel"),
        PromptKind::Close => matches!(choice, "close" | "cancel"),
    }
}

/// A prompt on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: String,
    /// The window it was shown in.
    pub label: String,
    pub kind: PromptKind,
    /// The Terminals it counted.
    pub terminals: Vec<Uuid>,
    /// The number shown; `None`: unknown (a `close` prompt only).
    pub count: Option<usize>,
    /// An application quit (not a window close) asked: keep/stop then exit the app.
    pub exit: bool,
    /// A resolve is working on it: further closes and resolves are refused until it ends.
    pub in_flight: bool,
}

impl Pending {
    fn event(&self) -> PromptEvent {
        PromptEvent {
            id: self.id.clone(),
            terminals: self.count,
            kind: self.kind.as_str(),
        }
    }
}

/// The `local:quit-prompt` payload. `terminals` is `null` only for a `close` prompt whose
/// count is unknown.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PromptEvent {
    pub id: String,
    pub terminals: Option<usize>,
    pub kind: &'static str,
}

pub struct QuitState {
    attached: AtomicBool,
    bypass: AtomicBool,
    /// A window-bound close waits for the Terminal list on a worker: further closes wait.
    deciding: AtomicBool,
    pending: Mutex<Option<Pending>>,
    next: AtomicU64,
    presence: Option<Presence>,
}

impl QuitState {
    pub fn new(presence: Option<Presence>) -> Self {
        Self {
            attached: AtomicBool::new(false),
            bypass: AtomicBool::new(false),
            deciding: AtomicBool::new(false),
            pending: Mutex::new(None),
            next: AtomicU64::new(1),
            presence,
        }
    }

    pub fn set_attached(&self) {
        self.attached.store(true, Ordering::SeqCst);
    }

    pub fn attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    /// A close is being decided off the UI thread (A2).
    pub fn deciding(&self) -> bool {
        self.deciding.load(Ordering::SeqCst)
    }

    pub fn set_bypass(&self) {
        self.bypass.store(true, Ordering::SeqCst);
    }

    pub fn bypassed(&self) -> bool {
        self.bypass.load(Ordering::SeqCst)
    }

    pub fn pending(&self) -> Option<Pending> {
        self.pending.lock().unwrap().clone()
    }

    /// Start a `keep` prompt; `None` when one is already pending (a second close is
    /// ignored, A9).
    pub fn begin(&self, label: &str, terminals: Vec<Uuid>, exit: bool) -> Option<PromptEvent> {
        let count = Some(terminals.len());
        self.begin_kind(label, PromptKind::Keep, terminals, count, exit)
    }

    /// Start a `close` prompt (window-bound) for `count` Terminals (`None`: unknown).
    pub fn begin_close(
        &self,
        label: &str,
        terminals: Vec<Uuid>,
        count: Option<usize>,
        exit: bool,
    ) -> Option<PromptEvent> {
        self.begin_kind(label, PromptKind::Close, terminals, count, exit)
    }

    fn begin_kind(
        &self,
        label: &str,
        kind: PromptKind,
        terminals: Vec<Uuid>,
        count: Option<usize>,
        exit: bool,
    ) -> Option<PromptEvent> {
        let mut g = self.pending.lock().unwrap();
        if g.is_some() {
            return None;
        }
        let p = Pending {
            id: format!("q{}", self.next.fetch_add(1, Ordering::SeqCst)),
            label: label.into(),
            kind,
            terminals,
            count,
            exit,
            in_flight: false,
        };
        let ev = p.event();
        *g = Some(p);
        Some(ev)
    }

    /// The pending prompt not being answered, to show again once the frontend listens.
    pub fn replay(&self) -> Option<(String, PromptEvent)> {
        let g = self.pending.lock().unwrap();
        g.as_ref()
            .filter(|p| !p.in_flight)
            .map(|p| (p.label.clone(), p.event()))
    }

    /// Start resolving prompt `id`: it stays pending, marked in flight, until [`Self::end`].
    pub fn start(&self, id: &str) -> Result<Pending, String> {
        let mut g = self.pending.lock().unwrap();
        match g.as_mut() {
            Some(p) if p.id == id && !p.in_flight => {
                p.in_flight = true;
                Ok(p.clone())
            }
            Some(p) if p.id == id => Err("busy: this quit prompt is being answered".into()),
            _ => Err("stale: no such quit prompt".into()),
        }
    }

    /// A resolution failed (Terminals did not stop): the same prompt stays pending, no longer
    /// in flight, so its id can be resolved again (Stop again, Keep, Cancel).
    pub fn release(&self) {
        if let Some(p) = self.pending.lock().unwrap().as_mut() {
            p.in_flight = false;
        }
    }

    /// End a resolution: no prompt, or a new one (Terminals added meanwhile).
    pub fn end(&self, next: Option<(String, Vec<Uuid>, bool)>) -> Option<PromptEvent> {
        let mut g = self.pending.lock().unwrap();
        *g = None;
        let (label, terminals, exit) = next?;
        let p = Pending {
            id: format!("q{}", self.next.fetch_add(1, Ordering::SeqCst)),
            label,
            kind: PromptKind::Keep,
            count: Some(terminals.len()),
            terminals,
            exit,
            in_flight: false,
        };
        let ev = p.event();
        *g = Some(p);
        Some(ev)
    }

    /// The decision inputs this state knows.
    fn inputs(
        &self,
        lifetime: Lifetime,
        keep: KeepSessions,
        last_window: bool,
        running: Option<usize>,
        owner: bool,
    ) -> CloseInputs {
        CloseInputs {
            lifetime,
            attached: self.attached(),
            bypass: self.bypassed(),
            keep,
            last_desktop: true,
            last_window,
            running,
            owner,
            session_ending: session_ending(),
        }
    }
}

/// The OS is shutting down or logging off (Windows: `SM_SHUTTINGDOWN`). Elsewhere an OS
/// shutdown sends no close request at all.
fn session_ending() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
        unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// This process started the Local Host's running window-bound Daemon (it is in our Job):
/// closing this process ends it. A Desktop that only connected to it does not own it.
fn owns_daemon(hosts: &Hosts) -> bool {
    hosts.local_daemon_now().and_then(|d| d.gui_pid()).is_some()
}

/// The Local Host's running Terminals, when known.
fn local_running(hosts: &Hosts) -> Option<Vec<Uuid>> {
    let snap = hosts.manager.access(LOCAL_HOST_ID)?.snapshot();
    running_ids(snap.terminals.as_deref())
}

/// The Local Host's running Terminals, waiting up to `wait` for a list that is not known
/// yet (the Local Host may still be connecting).
fn local_running_within(hosts: &Hosts, wait: Duration) -> Option<Vec<Uuid>> {
    let deadline = Instant::now() + wait;
    if let Some(h) = hosts.manager.access(LOCAL_HOST_ID) {
        h.kick();
    }
    loop {
        if let Some(r) = local_running(hosts) {
            return Some(r);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What a close (or quit) request comes to.
enum Evaluated {
    Close,
    /// A prompt was begun: keep the window. Shown now when the frontend listens, else when
    /// it does.
    Prompt(PromptEvent),
    /// Window-bound with an unknown count: decided on a worker (A2); keep the window.
    Deferred,
}

/// The decision for closing (or quitting) from window `label`. On Ask the prompt is begun
/// and its event returned; on Close, when this process goes, its presence is left while the
/// election is still held (A6).
fn evaluate(app: &AppHandle, label: &str, last_window: bool, exit: bool) -> Evaluated {
    let (Some(q), Some(hosts)) = (app.try_state::<QuitState>(), app.try_state::<Hosts>()) else {
        return Evaluated::Close;
    };
    let lifetime = Lifetime::of_platform();
    let running = local_running(&hosts);
    let keep = read_keep_sessions(hosts.store.as_deref());
    let owner = owns_daemon(&hosts);
    let mut inputs = q.inputs(
        lifetime,
        keep,
        last_window,
        running.as_ref().map(Vec::len),
        owner,
    );
    // Only elect when everything else would ask: a cheap close stays cheap.
    let pre = decide(&inputs);
    if pre == CloseDecision::Confirm(None) {
        // Unknown is not zero (A2): ask for the list off the UI thread, the window open.
        q.deciding.store(true, Ordering::SeqCst);
        let worker_app = app.clone();
        let worker_label = label.to_string();
        let spawned = std::thread::Builder::new()
            .name("quit-decide".into())
            .spawn(move || decide_later(&worker_app, &worker_label, inputs, exit));
        if spawned.is_err() {
            // No worker: ask without a number rather than close unasked.
            q.deciding.store(false, Ordering::SeqCst);
            return q
                .begin_close(label, Vec::new(), None, exit)
                .map_or(Evaluated::Close, Evaluated::Prompt);
        }
        return Evaluated::Deferred;
    }
    let election = match (pre, &q.presence) {
        (CloseDecision::Ask(_), Some(p)) => Some(p.elect(ELECTION_WAIT)),
        _ => None,
    };
    inputs.last_desktop = election.as_ref().is_none_or(|e| e.last);
    match decide(&inputs) {
        CloseDecision::Ask(_) => q
            .begin(label, running.unwrap_or_default(), exit)
            .map_or(Evaluated::Close, Evaluated::Prompt),
        CloseDecision::Confirm(n) => q
            .begin_close(label, running.unwrap_or_default(), n, exit)
            .map_or(Evaluated::Close, Evaluated::Prompt),
        CloseDecision::Close => {
            if last_window || exit {
                if let Some(p) = &q.presence {
                    p.leave();
                }
            }
            drop(election);
            Evaluated::Close
        }
    }
}

/// The worker half of a window-bound close whose Terminal count was unknown: wait briefly
/// for the list, decide again, then close or show the `close` prompt (with or without a
/// number).
fn decide_later(app: &AppHandle, label: &str, mut inputs: CloseInputs, exit: bool) {
    let (Some(q), Some(hosts)) = (app.try_state::<QuitState>(), app.try_state::<Hosts>()) else {
        return;
    };
    let running = local_running_within(&hosts, LIST_WAIT);
    inputs.running = running.as_ref().map(Vec::len);
    inputs.bypass = q.bypassed();
    inputs.session_ending = session_ending();
    let decision = decide(&inputs);
    q.deciding.store(false, Ordering::SeqCst);
    match decision {
        CloseDecision::Confirm(n) => {
            if let Some(ev) = q.begin_close(label, running.unwrap_or_default(), n, exit) {
                show(app, &q, label, ev);
            }
        }
        _ => close_now(app, label, exit),
    }
}

/// Emit `local:quit-prompt` to window `label` once the frontend listens; until then the
/// prompt stays pending and `local_quit_prompt_ready` shows it.
fn show(app: &AppHandle, q: &QuitState, label: &str, ev: PromptEvent) {
    if q.attached() {
        let _ = app.emit_to(label, "local:quit-prompt", ev);
    }
}

/// `local_quit_prompt_ready`: the frontend listens now; a prompt held for it is shown.
pub fn prompt_ready(app: &AppHandle) {
    let Some(q) = app.try_state::<QuitState>() else {
        return;
    };
    q.set_attached();
    if let Some((label, ev)) = q.replay() {
        let _ = app.emit_to(label.as_str(), "local:quit-prompt", ev);
    }
}

/// Close window `label` (or quit the app) without asking: the deferred decision came to
/// Close.
fn close_now(app: &AppHandle, label: &str, exit: bool) {
    let q = app.state::<QuitState>();
    if exit {
        q.set_bypass();
        app.exit(0);
        return;
    }
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.destroy();
    }
}

/// `WindowEvent::CloseRequested`.
pub fn close_requested(window: &tauri::Window, api: &tauri::CloseRequestApi) {
    let app = window.app_handle();
    let Some(q) = app.try_state::<QuitState>() else {
        return;
    };
    if q.pending().is_some() || q.deciding() {
        // A prompt is on screen (or being decided): a second close is ignored (A9), never
        // taken as an answer.
        api.prevent_close();
        return;
    }
    let last_window = app.webview_windows().len() <= 1;
    match evaluate(app, window.label(), last_window, false) {
        Evaluated::Close => {}
        Evaluated::Deferred => api.prevent_close(),
        Evaluated::Prompt(ev) => {
            api.prevent_close();
            show(app, &q, window.label(), ev);
        }
    }
}

/// `RunEvent::ExitRequested` from an application quit (not the last window closing, which
/// `close_requested` handled): the same decision (A7). Returns whether to prevent the exit.
pub fn exit_requested(app: &AppHandle) -> bool {
    let Some(q) = app.try_state::<QuitState>() else {
        return false;
    };
    if q.pending().is_some() || q.deciding() {
        return true;
    }
    let Some(label) = app.webview_windows().keys().next().cloned() else {
        return false;
    };
    match evaluate(app, &label, true, true) {
        Evaluated::Close => false,
        Evaluated::Deferred => true,
        Evaluated::Prompt(ev) => {
            show(app, &q, &label, ev);
            true
        }
    }
}

/// What `local_quit_check` answers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuitCheck {
    pub ask: bool,
    /// Which prompt closing would show: `keep` or `close`.
    pub kind: &'static str,
    pub terminals: usize,
    pub last_desktop: bool,
    pub keep_sessions: &'static str,
}

pub fn check(app: &AppHandle) -> Option<QuitCheck> {
    let q = app.try_state::<QuitState>()?;
    let hosts = app.try_state::<Hosts>()?;
    let lifetime = Lifetime::of_platform();
    let running = local_running(&hosts);
    let keep = read_keep_sessions(hosts.store.as_deref());
    let last_desktop = q.presence.as_ref().is_none_or(|p| p.others_alive() == 0);
    let mut i = q.inputs(
        lifetime,
        keep,
        app.webview_windows().len() <= 1,
        running.as_ref().map(Vec::len),
        owns_daemon(&hosts),
    );
    // A check is informational: ask as if the frontend listened.
    i.attached = true;
    i.last_desktop = last_desktop;
    Some(QuitCheck {
        ask: !matches!(decide(&i), CloseDecision::Close),
        kind: match lifetime {
            Lifetime::AlwaysOn => PromptKind::Keep.as_str(),
            Lifetime::WindowBound => PromptKind::Close.as_str(),
        },
        terminals: running.map_or(0, |r| r.len()),
        last_desktop,
        keep_sessions: keep.as_str(),
    })
}

/// End the window (or the app) the prompt was for.
fn finish(app: &AppHandle, p: &Pending) {
    let q = app.state::<QuitState>();
    if p.exit {
        q.set_bypass();
        app.exit(0);
        return;
    }
    if app.webview_windows().len() <= 1 {
        if let Some(pr) = &q.presence {
            pr.leave();
        }
    }
    if let Some(w) = app.get_webview_window(&p.label) {
        let _ = w.destroy();
    }
}

/// Stop `ids` on the Local Host; returns how many did not end within [`STOP_WAIT`].
fn stop_terminals(hosts: &Hosts, ids: &[Uuid]) -> usize {
    let Some(access) = hosts.manager.access(LOCAL_HOST_ID) else {
        return ids.len();
    };
    for t in ids {
        access.term_close(*t, Box::new(|_| {}));
    }
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        let still: usize = match local_running(hosts) {
            Some(now) => ids.iter().filter(|t| now.contains(t)).count(),
            None => ids.len(),
        };
        if still == 0 || Instant::now() >= deadline {
            return still;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `local_quit_resolve`: `Ok(None)` (done), `Ok(Some({changed:{id, added}}))` (Terminals were
/// started while the prompt was up or the stop ran; the shown ones are stopped already and a
/// new prompt counts only the new ones), or `Err("failed:<n> …")` / `Err("stale…")` /
/// `Err("busy…")`. The prompt stays pending (in flight) for the whole resolution, so closes
/// and other resolves are refused meanwhile.
pub fn resolve(
    app: &AppHandle,
    id: &str,
    choice: &str,
    remember: bool,
) -> Result<Option<serde_json::Value>, String> {
    let q = app.state::<QuitState>();
    let hosts = app.state::<Hosts>();
    if !matches!(choice, "keep" | "stop" | "close" | "cancel") {
        return Err(format!("invalid choice {choice:?}"));
    }
    if let Some(kind) = q.pending().filter(|p| p.id == id).map(|p| p.kind) {
        if !valid_choice(kind, choice) {
            return Err(format!(
                "invalid choice {choice:?} for a {} prompt",
                kind.as_str()
            ));
        }
    }
    let p = q.start(id)?;
    match choice {
        "cancel" => {
            q.end(None);
            Ok(None)
        }
        // Window-bound: this process ends, and with it the Daemon and every Terminal
        // (`Hosts::quit` at exit). `remember` does not apply.
        "close" => {
            q.end(None);
            finish(app, &p);
            Ok(None)
        }
        "keep" => {
            if remember {
                let saved = hosts
                    .store
                    .as_deref()
                    .ok_or_else(|| "settings are unavailable".to_string())
                    .and_then(|store| {
                        store.set_saved(
                            KEEP_SESSIONS_KEY,
                            serde_json::Value::from(KeepSessions::Always.as_str()),
                        )
                    });
                if let Err(e) = saved {
                    return Err(keep_save_failed(&q, e));
                }
            }
            q.end(None);
            finish(app, &p);
            Ok(None)
        }
        _ => match run_stop(&p.terminals, &|| local_running(&hosts), &|ids| {
            stop_terminals(&hosts, ids)
        }) {
            StopOutcome::Failed(left) => {
                // The dialog stays open: the same id may be answered again. Terminals that
                // did stop are simply gone from the next attempt's running set.
                q.release();
                Err(format!("failed:{left} sessions did not stop"))
            }
            StopOutcome::Added(added) => {
                let ev = q
                    .end(Some((p.label.clone(), added, p.exit)))
                    .expect("a new prompt");
                Ok(Some(serde_json::json!({
                    "changed": { "id": ev.id, "added": ev.terminals }
                })))
            }
            StopOutcome::Done => {
                q.end(None);
                finish(app, &p);
                Ok(None)
            }
        },
    }
}

/// Saving "always keep" failed: the prompt stays pending under the same id (not in flight),
/// so the open dialog can answer it again, as after a failed Stop.
fn keep_save_failed(q: &QuitState, e: String) -> String {
    q.release();
    format!("failed:0 {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed save of "always keep" keeps the prompt resolvable under the same id.
    #[test]
    fn keep_save_failure_keeps_the_prompt_resolvable() {
        let q = QuitState::new(None);
        let ev = q.begin("main", vec![Uuid::new_v4()], false).unwrap();
        q.start(&ev.id).unwrap();
        assert_eq!(
            keep_save_failed(&q, "disk full".into()),
            "failed:0 disk full"
        );
        let again = q.pending().expect("still pending");
        assert_eq!(again.id, ev.id);
        assert!(!again.in_flight);
        assert_eq!(
            q.start(&ev.id).unwrap().id,
            ev.id,
            "Keep, Stop or Cancel again"
        );
    }

    fn base() -> CloseInputs {
        CloseInputs {
            lifetime: Lifetime::AlwaysOn,
            attached: true,
            bypass: false,
            keep: KeepSessions::Ask,
            last_desktop: true,
            last_window: true,
            running: Some(2),
            owner: false,
            session_ending: false,
        }
    }

    /// Window-bound (W4, A2): the owner's last window confirms; the count unknown still
    /// confirms (without a number); keep/presence/attachment do not apply.
    fn window_bound() -> CloseInputs {
        CloseInputs {
            lifetime: Lifetime::WindowBound,
            owner: true,
            ..base()
        }
    }

    #[test]
    fn decide_table() {
        assert_eq!(decide(&base()), CloseDecision::Ask(2));
        type Gate = (&'static str, fn(&mut CloseInputs));
        let gates: [Gate; 7] = [
            ("unattached", |i| i.attached = false),
            ("bypass", |i| i.bypass = true),
            ("always keep", |i| i.keep = KeepSessions::Always),
            ("other desktop", |i| i.last_desktop = false),
            ("other window", |i| i.last_window = false),
            ("unknown list", |i| i.running = None),
            ("nothing running", |i| i.running = Some(0)),
        ];
        for (what, f) in gates {
            let mut i = base();
            f(&mut i);
            assert_eq!(decide(&i), CloseDecision::Close, "{what}");
        }
        // Owner and session state do not change the always-on decision.
        let mut i = base();
        i.owner = true;
        i.session_ending = true;
        assert_eq!(decide(&i), CloseDecision::Ask(2));

        // Window-bound.
        assert_eq!(decide(&window_bound()), CloseDecision::Confirm(Some(2)));
        let asks: [Gate; 4] = [
            ("always keep is ignored", |i| i.keep = KeepSessions::Always),
            ("presence is ignored", |i| i.last_desktop = false),
            ("unattached still asks (held)", |i| i.attached = false),
            ("a terminal of another window counts", |i| {
                i.running = Some(1)
            }),
        ];
        for (what, f) in asks {
            let mut i = window_bound();
            f(&mut i);
            assert!(
                matches!(decide(&i), CloseDecision::Confirm(Some(_))),
                "{what}"
            );
        }
        let mut i = window_bound();
        i.running = None;
        assert_eq!(
            decide(&i),
            CloseDecision::Confirm(None),
            "unknown is not zero"
        );
        let closes: [Gate; 5] = [
            ("not the owner", |i| i.owner = false),
            ("session ending", |i| i.session_ending = true),
            ("updater bypass", |i| i.bypass = true),
            ("nothing running", |i| i.running = Some(0)),
            ("other window", |i| i.last_window = false),
        ];
        for (what, f) in closes {
            let mut i = window_bound();
            f(&mut i);
            assert_eq!(decide(&i), CloseDecision::Close, "{what}");
            // These close with the count unknown too.
            if what != "nothing running" {
                i.running = None;
                assert_eq!(decide(&i), CloseDecision::Close, "{what}, count unknown");
            }
        }
    }

    /// A `close` prompt takes only Close and Cancel; a `keep` prompt never takes Close.
    #[test]
    fn close_kind_accepts_only_close_and_cancel() {
        assert!(valid_choice(PromptKind::Close, "close"));
        assert!(valid_choice(PromptKind::Close, "cancel"));
        assert!(!valid_choice(PromptKind::Close, "keep"));
        assert!(!valid_choice(PromptKind::Close, "stop"));
        assert!(valid_choice(PromptKind::Keep, "keep"));
        assert!(valid_choice(PromptKind::Keep, "stop"));
        assert!(valid_choice(PromptKind::Keep, "cancel"));
        assert!(!valid_choice(PromptKind::Keep, "close"));
        let q = QuitState::new(None);
        let ev = q.begin_close("main", vec![], None, false).unwrap();
        assert_eq!(ev.kind, "close");
        assert_eq!(ev.terminals, None);
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            serde_json::json!({"id": ev.id, "terminals": null, "kind": "close"})
        );
        // One prompt at a time, whatever its kind.
        assert!(q.begin("main", vec![Uuid::new_v4()], false).is_none());
        assert_eq!(q.pending().unwrap().kind, PromptKind::Close);
        q.end(None);
        let ev = q.begin("main", vec![Uuid::new_v4()], false).unwrap();
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            serde_json::json!({"id": ev.id, "terminals": 1, "kind": "keep"})
        );
    }

    /// A prompt begun before the frontend listened is replayed once it does (A2), but not
    /// while it is being answered.
    #[test]
    fn held_prompt_is_replayed_when_the_frontend_listens() {
        let q = QuitState::new(None);
        assert!(q.replay().is_none());
        let ev = q
            .begin_close("main", vec![Uuid::new_v4(), Uuid::new_v4()], Some(2), false)
            .unwrap();
        assert_eq!(q.replay(), Some(("main".to_string(), ev.clone())));
        q.start(&ev.id).unwrap();
        assert!(q.replay().is_none());
        q.release();
        assert_eq!(q.replay().unwrap().1.terminals, Some(2));
    }

    #[test]
    fn unattached_never_intercepts() {
        let q = QuitState::new(None);
        let inputs =
            |q: &QuitState| q.inputs(Lifetime::AlwaysOn, KeepSessions::Ask, true, Some(3), false);
        assert_eq!(decide(&inputs(&q)), CloseDecision::Close);
        q.set_attached();
        assert_eq!(decide(&inputs(&q)), CloseDecision::Ask(3));
        q.set_bypass();
        assert_eq!(decide(&inputs(&q)), CloseDecision::Close);
    }

    /// A9: a second close while a prompt is pending begins no second prompt (and is not
    /// taken as Keep); a resolve must name the prompt shown.
    #[test]
    fn second_close_while_pending_is_ignored_and_resolve_needs_the_shown_id() {
        let q = QuitState::new(None);
        let a = Uuid::new_v4();
        let ev = q.begin("main", vec![a], false).unwrap();
        assert_eq!(ev.terminals, Some(1));
        assert!(
            q.begin("main", vec![a], false).is_none(),
            "second prompt begun"
        );
        assert_eq!(q.pending().unwrap().terminals, vec![a]);
        assert!(q.start("q999").unwrap_err().starts_with("stale"));
        assert_eq!(q.start(&ev.id).unwrap().id, ev.id);
        q.end(None);
        assert!(q.start(&ev.id).is_err(), "resolved twice");
        // A new prompt gets a new id.
        let ev2 = q.begin("main", vec![a], false).unwrap();
        assert_ne!(ev2.id, ev.id);
    }

    /// P1-a: while a resolution is in flight the prompt stays pending: closes begin nothing
    /// and a second resolve is refused.
    #[test]
    fn close_during_in_flight_resolution_is_ignored() {
        let q = QuitState::new(None);
        let a = Uuid::new_v4();
        let ev = q.begin("main", vec![a], false).unwrap();
        let p = q.start(&ev.id).unwrap();
        assert!(p.in_flight);
        assert!(
            q.pending().is_some_and(|p| p.in_flight),
            "pending while in flight"
        );
        assert!(
            q.begin("main", vec![a], false).is_none(),
            "a close began a prompt"
        );
        assert!(q.start(&ev.id).unwrap_err().starts_with("busy"));
        q.end(None);
        assert!(q.pending().is_none());
    }

    /// After `failed:<n>` the prompt stays pending under the same id, not in flight, so the
    /// open dialog can retry Stop (only what still runs), Keep or Cancel.
    #[test]
    fn stop_failure_keeps_the_prompt_resolvable() {
        use std::cell::RefCell;
        let q = QuitState::new(None);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let ev = q.begin("main", vec![a, b], false).unwrap();
        let p = q.start(&ev.id).unwrap();
        // a stops, b does not.
        let running = RefCell::new(vec![a, b]);
        let out = run_stop(&p.terminals, &|| Some(running.borrow().clone()), &|_| {
            *running.borrow_mut() = vec![b];
            1
        });
        assert_eq!(out, StopOutcome::Failed(1));
        q.release();
        let again = q.pending().expect("still pending");
        assert_eq!(again.id, ev.id);
        assert!(!again.in_flight);
        assert!(
            q.begin("main", vec![b], false).is_none(),
            "a close began a prompt"
        );
        // Retry Stop under the same id: only b is stopped this time.
        let p = q.start(&ev.id).unwrap();
        let stopped = RefCell::new(Vec::new());
        let out = run_stop(&p.terminals, &|| Some(running.borrow().clone()), &|ids| {
            stopped.borrow_mut().extend_from_slice(ids);
            running.borrow_mut().clear();
            0
        });
        assert_eq!(out, StopOutcome::Done);
        assert_eq!(*stopped.borrow(), vec![b]);
        q.end(None);
        // Keep or Cancel after a failure find the same id too.
        let ev = q.begin("main", vec![a], false).unwrap();
        q.start(&ev.id).unwrap();
        q.release();
        assert_eq!(q.start(&ev.id).unwrap().id, ev.id);
    }

    /// P1-a: a Terminal opened during the stop wait is reported as added (the window stays);
    /// the shown ones were stopped.
    #[test]
    fn terminal_opened_during_stop_is_added_and_window_stays() {
        use std::cell::RefCell;
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let running = RefCell::new(vec![a, b]);
        let stopped = RefCell::new(Vec::new());
        let out = run_stop(&[a, b], &|| Some(running.borrow().clone()), &|ids| {
            stopped.borrow_mut().extend_from_slice(ids);
            // The shown ones end; meanwhile c is opened.
            *running.borrow_mut() = vec![c];
            0
        });
        assert_eq!(out, StopOutcome::Added(vec![c]));
        assert_eq!(*stopped.borrow(), vec![a, b]);
        // Only shrank: done, close.
        let running = RefCell::new(vec![a]);
        let out = run_stop(&[a, b], &|| Some(running.borrow().clone()), &|_| {
            running.borrow_mut().clear();
            0
        });
        assert_eq!(out, StopOutcome::Done);
        // A shown Terminal that would not end: failed, never Added or Done.
        let out = run_stop(&[a], &|| Some(vec![a]), &|_| 1);
        assert_eq!(out, StopOutcome::Failed(1));
    }

    #[test]
    fn running_count_ignores_exited() {
        let t = |exit: Option<i32>| -> TerminalInfo {
            let mut v = serde_json::json!({
                "terminal": Uuid::new_v4(),
                "spec": {"cwd": "/"},
                "meta": {},
                "createdAtMs": 0,
                "pid": null,
                "exitCode": exit,
            });
            if exit.is_none() {
                v.as_object_mut().unwrap().remove("exitCode");
            }
            serde_json::from_value(v).unwrap()
        };
        let list = vec![t(None), t(Some(0)), t(None), t(Some(1))];
        let ids = running_ids(Some(&list)).unwrap();
        assert_eq!(ids, vec![list[0].terminal, list[2].terminal]);
        assert_eq!(running_ids(None), None);
        assert_eq!(running_ids(Some(&[])), Some(vec![]));
    }

    #[test]
    fn stop_plan_stops_shown_and_counts_added() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        // b ended meanwhile, c was started after the prompt.
        let (stop, added) = plan_stop(&[a, b], &[a, c]);
        assert_eq!(stop, vec![a]);
        assert_eq!(added, vec![c]);
        // Only shrank: nothing added, no new prompt.
        let (stop, added) = plan_stop(&[a, b], &[b]);
        assert_eq!(stop, vec![b]);
        assert!(added.is_empty());
    }
}

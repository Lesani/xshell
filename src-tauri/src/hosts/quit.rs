//! The quit prompt (ADR-0017): closing the last Desktop window on this machine while the
//! Local Host has running Terminals asks "Keep N sessions running?" (Keep running / Stop
//! them / cancel, with "Do not ask again" for Keep). "Stop them" ends exactly the Terminals
//! the prompt counted (A9); the Daemon itself keeps running either way.
//!
//! Interception is off until the frontend calls `local_quit_prompt_ready` (after it listens
//! for `local:quit-prompt`), and never on Windows (Stage 2) or after `local_quit_bypass` (the
//! updater's relaunch). An OS shutdown sends SIGTERM without a close request: no prompt, the
//! sessions are kept.

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

/// What decides whether closing asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseInputs {
    /// Linux or macOS (Windows confirms in Stage 2).
    pub unix: bool,
    /// The frontend listens for the prompt.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    Close,
    Ask(usize),
}

/// Asks only when every gate holds.
pub fn decide(i: &CloseInputs) -> CloseDecision {
    match i.running {
        Some(n)
            if n > 0
                && i.unix
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

/// A prompt on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: String,
    /// The window it was shown in.
    pub label: String,
    /// The Terminals it counted.
    pub terminals: Vec<Uuid>,
    /// An application quit (not a window close) asked: keep/stop then exit the app.
    pub exit: bool,
}

/// The `local:quit-prompt` payload.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PromptEvent {
    pub id: String,
    pub terminals: usize,
}

pub struct QuitState {
    attached: AtomicBool,
    bypass: AtomicBool,
    pending: Mutex<Option<Pending>>,
    next: AtomicU64,
    presence: Option<Presence>,
}

impl QuitState {
    pub fn new(presence: Option<Presence>) -> Self {
        Self {
            attached: AtomicBool::new(false),
            bypass: AtomicBool::new(false),
            pending: Mutex::new(None),
            next: AtomicU64::new(1),
            presence,
        }
    }

    pub fn set_attached(&self) {
        self.attached.store(true, Ordering::SeqCst);
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

    /// Start a prompt; `None` when one is already pending (a second close is ignored, A9).
    pub fn begin(&self, label: &str, terminals: Vec<Uuid>, exit: bool) -> Option<PromptEvent> {
        let mut g = self.pending.lock().unwrap();
        if g.is_some() {
            return None;
        }
        let id = format!("q{}", self.next.fetch_add(1, Ordering::SeqCst));
        let ev = PromptEvent {
            id: id.clone(),
            terminals: terminals.len(),
        };
        *g = Some(Pending {
            id,
            label: label.into(),
            terminals,
            exit,
        });
        Some(ev)
    }

    /// Take the pending prompt if it is `id`.
    pub fn take(&self, id: &str) -> Result<Pending, String> {
        let mut g = self.pending.lock().unwrap();
        match g.as_ref() {
            Some(p) if p.id == id => Ok(g.take().unwrap()),
            _ => Err("stale: no such quit prompt".into()),
        }
    }

    /// The decision inputs this state knows.
    fn inputs(&self, keep: KeepSessions, last_window: bool, running: Option<usize>) -> CloseInputs {
        CloseInputs {
            unix: cfg!(unix),
            attached: self.attached.load(Ordering::SeqCst),
            bypass: self.bypassed(),
            keep,
            last_desktop: true,
            last_window,
            running,
        }
    }
}

/// The Local Host's running Terminals, when known.
fn local_running(hosts: &Hosts) -> Option<Vec<Uuid>> {
    let snap = hosts.manager.access(LOCAL_HOST_ID)?.snapshot();
    running_ids(snap.terminals.as_deref())
}

/// The decision for closing (or quitting) from window `label`. On Ask the prompt is begun
/// and its event returned; on Close, when this process goes, its presence is left while the
/// election is still held (A6).
fn evaluate(app: &AppHandle, label: &str, last_window: bool, exit: bool) -> Option<PromptEvent> {
    let q = app.try_state::<QuitState>()?;
    let hosts = app.try_state::<Hosts>()?;
    let running = local_running(&hosts);
    let keep = read_keep_sessions(hosts.store.as_deref());
    let mut inputs = q.inputs(keep, last_window, running.as_ref().map(Vec::len));
    // Only elect when everything else would ask: a cheap close stays cheap.
    let pre = decide(&inputs);
    let election = match (pre, &q.presence) {
        (CloseDecision::Ask(_), Some(p)) => Some(p.elect(ELECTION_WAIT)),
        _ => None,
    };
    inputs.last_desktop = election.as_ref().is_none_or(|e| e.last);
    match decide(&inputs) {
        CloseDecision::Ask(_) => q.begin(label, running.unwrap_or_default(), exit),
        CloseDecision::Close => {
            if last_window || exit {
                if let Some(p) = &q.presence {
                    p.leave();
                }
            }
            drop(election);
            None
        }
    }
}

/// `WindowEvent::CloseRequested`.
pub fn close_requested(window: &tauri::Window, api: &tauri::CloseRequestApi) {
    let app = window.app_handle();
    let Some(q) = app.try_state::<QuitState>() else {
        return;
    };
    if q.pending().is_some() {
        // A prompt is on screen: a second close is ignored (A9), never taken as Keep.
        api.prevent_close();
        return;
    }
    let last_window = app.webview_windows().len() <= 1;
    if let Some(ev) = evaluate(app, window.label(), last_window, false) {
        api.prevent_close();
        let _ = app.emit_to(window.label(), "local:quit-prompt", ev);
    }
}

/// `RunEvent::ExitRequested` from an application quit (not the last window closing, which
/// `close_requested` handled): the same decision (A7). Returns whether to prevent the exit.
pub fn exit_requested(app: &AppHandle) -> bool {
    let Some(q) = app.try_state::<QuitState>() else {
        return false;
    };
    if q.pending().is_some() {
        return true;
    }
    let Some(label) = app.webview_windows().keys().next().cloned() else {
        return false;
    };
    match evaluate(app, &label, true, true) {
        Some(ev) => {
            let _ = app.emit_to(label.as_str(), "local:quit-prompt", ev);
            true
        }
        None => false,
    }
}

/// What `local_quit_check` answers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuitCheck {
    pub ask: bool,
    pub terminals: usize,
    pub last_desktop: bool,
    pub keep_sessions: &'static str,
}

pub fn check(app: &AppHandle) -> Option<QuitCheck> {
    let q = app.try_state::<QuitState>()?;
    let hosts = app.try_state::<Hosts>()?;
    let running = local_running(&hosts);
    let keep = read_keep_sessions(hosts.store.as_deref());
    let last_desktop = q.presence.as_ref().is_none_or(|p| p.others_alive() == 0);
    let mut i = q.inputs(
        keep,
        app.webview_windows().len() <= 1,
        running.as_ref().map(Vec::len),
    );
    // A check is informational: ask as if the frontend listened.
    i.attached = true;
    i.last_desktop = last_desktop;
    Some(QuitCheck {
        ask: matches!(decide(&i), CloseDecision::Ask(_)),
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
/// started after the prompt; the shown ones are stopped already and a new prompt counts only
/// the new ones), or `Err("failed:<n> …")` / `Err("stale…")`.
pub fn resolve(
    app: &AppHandle,
    id: &str,
    choice: &str,
    remember: bool,
) -> Result<Option<serde_json::Value>, String> {
    let q = app.state::<QuitState>();
    let hosts = app.state::<Hosts>();
    if !matches!(choice, "keep" | "stop" | "cancel") {
        return Err(format!("invalid choice {choice:?}"));
    }
    let p = q.take(id)?;
    match choice {
        "cancel" => Ok(None),
        "keep" => {
            if remember {
                let store = hosts.store.as_deref().ok_or("settings are unavailable")?;
                store
                    .set_saved(
                        KEEP_SESSIONS_KEY,
                        serde_json::Value::from(KeepSessions::Always.as_str()),
                    )
                    .map_err(|e| format!("failed:0 {e}"))?;
            }
            finish(app, &p);
            Ok(None)
        }
        _ => {
            let now = local_running(&hosts).unwrap_or_default();
            let (stop, added) = plan_stop(&p.terminals, &now);
            let left = stop_terminals(&hosts, &stop);
            if left > 0 {
                return Err(format!("failed:{left} sessions did not stop"));
            }
            if !added.is_empty() {
                let ev = q
                    .begin(&p.label, added, p.exit)
                    .ok_or("another quit prompt is pending")?;
                return Ok(Some(serde_json::json!({
                    "changed": { "id": ev.id, "added": ev.terminals }
                })));
            }
            finish(app, &p);
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> CloseInputs {
        CloseInputs {
            unix: true,
            attached: true,
            bypass: false,
            keep: KeepSessions::Ask,
            last_desktop: true,
            last_window: true,
            running: Some(2),
        }
    }

    #[test]
    fn decide_table() {
        assert_eq!(decide(&base()), CloseDecision::Ask(2));
        type Gate = (&'static str, fn(&mut CloseInputs));
        let gates: [Gate; 8] = [
            ("windows", |i| i.unix = false),
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
    }

    #[test]
    fn unattached_never_intercepts() {
        let q = QuitState::new(None);
        let i = q.inputs(KeepSessions::Ask, true, Some(3));
        assert_eq!(decide(&i), CloseDecision::Close);
        q.set_attached();
        let i = q.inputs(KeepSessions::Ask, true, Some(3));
        assert_eq!(decide(&i), CloseDecision::Ask(3));
        q.set_bypass();
        let i = q.inputs(KeepSessions::Ask, true, Some(3));
        assert_eq!(decide(&i), CloseDecision::Close);
    }

    /// A9: a second close while a prompt is pending is ignored, not taken as Keep, and a
    /// resolve must name the prompt shown.
    #[test]
    fn second_close_while_pending_closes() {
        let q = QuitState::new(None);
        let a = Uuid::new_v4();
        let ev = q.begin("main", vec![a], false).unwrap();
        assert_eq!(ev.terminals, 1);
        assert!(
            q.begin("main", vec![a], false).is_none(),
            "second prompt begun"
        );
        assert_eq!(q.pending().unwrap().terminals, vec![a]);
        assert!(q.take("q999").is_err());
        assert_eq!(q.take(&ev.id).unwrap().id, ev.id);
        assert!(q.take(&ev.id).is_err(), "taken twice");
        // A new prompt gets a new id.
        let ev2 = q.begin("main", vec![a], false).unwrap();
        assert_ne!(ev2.id, ev.id);
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

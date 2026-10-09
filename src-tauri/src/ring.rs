//! Settings → Mobile: Tauri glue over `xshell_hostlink::ring`. Commands `ring_status`,
//! `ring_enable`, `ring_set_relay_url` and `ring_claim_host`; the event `ring:status`.
//! Pairing (#9): `ring_pair_phone_start`, `ring_pair_phone_cancel`, `ring_pair_computer` and
//! `ring_pair_cancel`; the event `ring:pairing`.
//!
//! Every Host's Daemon (the Local Host's and each Remote Host's) joins the Ring whenever
//! its Host connects: the Host Observer records the connection in [`HOST_SYNC`], and
//! `xshell_hostlink::ring::SyncWorker` does the work on its own thread.

use serde::Serialize;
use std::sync::Arc;
use xshell_hostlink::ring::{
    default_relay_url, DesktopRing, DesktopRingConfig, HostRingState, HostSync, PairingEvent,
    PairingFlow, PairingObserver, RingObserver, RingView, SyncWorker,
};
use xshell_hostlink::{HostStatus, LOCAL_HOST_ID};

/// The one the app's Host Observer feeds.
pub static HOST_SYNC: HostSync = HostSync::new();

/// The Observer's hook: records the Host's connection. No work here (see the module docs).
pub fn observe(sync: &HostSync, s: &HostStatus) {
    sync.observe(s);
}

/// What the frontend gets: the Ring's view plus how this computer's terminals and the
/// Remote Hosts stand.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RingStatus {
    #[serde(flatten)]
    pub view: RingView,
    /// `daemon` (can join), `in-process` (terminals run inside the app) or `too-old`.
    pub local: &'static str,
    /// The Remote Hosts not in the Ring, and why.
    pub hosts: Vec<HostRingState>,
}

pub struct RingState {
    pub worker: Arc<SyncWorker>,
}

fn host_name() -> String {
    #[cfg(unix)]
    let raw = {
        let mut buf = [0u8; 256];
        let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if r == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            String::new()
        }
    };
    #[cfg(not(unix))]
    let raw = std::env::var("COMPUTERNAME").unwrap_or_default();
    xshell_protocol::ring::member_name(&raw, "this computer")
}

use tauri::{AppHandle, Emitter, Manager as _};

fn status_of(app: &AppHandle, view: RingView) -> RingStatus {
    let in_process = app.try_state::<crate::hosts::Hosts>().is_some_and(|h| {
        matches!(
            h.local_mode,
            crate::hosts::local::LocalMode::InProcess { .. }
        )
    });
    let worker = app.try_state::<RingState>().map(|r| r.worker.clone());
    let too_old = worker
        .as_ref()
        .is_some_and(|w| w.state_of(LOCAL_HOST_ID) == Some("too-old"));
    let hosts = worker
        .map(|w| w.host_states())
        .unwrap_or_default()
        .into_iter()
        // The Local Host has its own line (`local`).
        .filter(|h| h.host != LOCAL_HOST_ID)
        .collect();
    RingStatus {
        view,
        local: if in_process {
            "in-process"
        } else if too_old {
            "too-old"
        } else {
            "daemon"
        },
        hosts,
    }
}

struct TauriRingObserver(AppHandle);

impl RingObserver for TauriRingObserver {
    fn changed(&self, view: &RingView) {
        let _ = self.0.emit("ring:status", status_of(&self.0, view.clone()));
    }
}

/// Opens the Ring state and starts the worker. After `Hosts` is managed.
pub fn setup(app: &AppHandle) {
    let dir = match app.path().app_data_dir() {
        Ok(d) => d.join("ring"),
        Err(e) => {
            eprintln!("xshell: mobile access is unavailable: no app data dir: {e}");
            return;
        }
    };
    let mut cfg = DesktopRingConfig::new(dir, host_name());
    cfg.default_relay_url = default_relay_url(&|k| std::env::var(k).ok());
    let ring = DesktopRing::open(cfg, Arc::new(TauriRingObserver(app.clone())));
    let (a, b) = (app.clone(), app.clone());
    let worker = SyncWorker::new(
        ring,
        &HOST_SYNC,
        Box::new(move |id| {
            a.try_state::<crate::hosts::Hosts>()
                .and_then(|h| h.manager.host(id))
        }),
        Box::new(move || {
            if let Some(r) = b.try_state::<RingState>() {
                let _ = b.emit("ring:status", status_of(&b, r.worker.ring.view()));
            }
        }),
    );
    app.manage(RingState {
        worker: worker.clone(),
    });
    // Hosts may be connected already: record them before attaching, so the start
    // reconciles them.
    if let Some(h) = app.try_state::<crate::hosts::Hosts>() {
        for s in h.manager.snapshot() {
            observe(&HOST_SYNC, &s.status);
        }
    }
    if let Err(e) = worker.start() {
        eprintln!("xshell: cannot start the ring worker: {e}");
    }
}

/// Quitting: no more jobs, and the Relay hears goodbye. Blocks up to the goodbye.
pub fn quit(app: &AppHandle) {
    HOST_SYNC.shutdown();
    if let Some(r) = app.try_state::<RingState>() {
        r.worker.ring.quit();
    }
}

fn worker(app: &AppHandle) -> Result<Arc<SyncWorker>, String> {
    app.try_state::<RingState>()
        .map(|r| r.worker.clone())
        .ok_or_else(|| "mobile access is unavailable".to_string())
}

async fn run<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn ring_status(app: AppHandle) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || Ok(status_of(&app, w.ring.view()))).await
}

/// Enables Mobile access: creates the Ring with this computer's Daemon when it can join;
/// the connected Remote Hosts follow in the next version. After unreadable settings were set
/// aside, a new Ring needs `startOver`.
#[tauri::command]
pub async fn ring_enable(app: AppHandle, start_over: Option<bool>) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || {
        let local = w.identity_of(LOCAL_HOST_ID);
        let view = w.ring.enable(local, start_over.unwrap_or(false))?;
        // Every connected Daemon joins (and is added, if it is not in the Ring yet).
        HOST_SYNC.poke_all();
        Ok(status_of(&app, view))
    })
    .await
}

/// Moves the Ring to `url`: a new Roster version; every connected Daemon follows.
#[tauri::command]
pub async fn ring_set_relay_url(app: AppHandle, url: String) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || {
        w.ring.set_relay_url(&url)?;
        HOST_SYNC.poke_all();
        Ok(status_of(&app, w.ring.view()))
    })
    .await
}

/// Pairs a Remote Host that is paired with another set of devices with this Desktop's.
#[tauri::command]
pub async fn ring_claim_host(app: AppHandle, host: String) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || {
        w.claim(&host)?;
        Ok(status_of(&app, w.ring.view()))
    })
    .await
}

// ── Pairing (#9) ──────────────────────────────────────────────────────────

/// A QR code as the UI draws it: `size` rows of `size` modules, `'1'` dark and `'0'` light.
/// No quiet zone; the UI adds the margin.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Qr {
    pub size: usize,
    pub rows: Vec<String>,
}

/// The QR code of `payload`, at error-correction level M.
pub fn qr_rows(payload: &str) -> Result<Qr, String> {
    use qrcode::{Color, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)
        .map_err(|e| format!("cannot build the QR code: {e}"))?;
    let size = code.width();
    let rows = code
        .to_colors()
        .chunks(size)
        .map(|row| {
            row.iter()
                .map(|c| if *c == Color::Dark { '1' } else { '0' })
                .collect()
        })
        .collect();
    Ok(Qr { size, rows })
}

/// What `ring_pair_phone_start` answers: the pairing text, its QR code, and when it expires
/// (unix seconds; shown only, the Desktop enforces the expiry itself).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PhoneStart {
    pub payload: String,
    pub qr: Qr,
    pub expires_at: u64,
}

/// The `ring:pairing` event: `{flow, state, name?, role?, code?}`.
#[derive(Serialize, Clone)]
struct PairingPayload<'a> {
    flow: PairingFlow,
    #[serde(flatten)]
    event: &'a PairingEvent,
}

struct TauriPairingObserver(AppHandle);

impl PairingObserver for TauriPairingObserver {
    fn pairing(&self, flow: PairingFlow, event: &PairingEvent) {
        let _ = self.0.emit("ring:pairing", PairingPayload { flow, event });
    }
}

fn pairing_observer(app: &AppHandle) -> Arc<dyn PairingObserver> {
    Arc::new(TauriPairingObserver(app.clone()))
}

/// Shows a new phone offer (cancelling the previous one). Progress arrives as `ring:pairing`.
#[tauri::command]
pub async fn ring_pair_phone_start(app: AppHandle) -> Result<PhoneStart, String> {
    let w = worker(&app)?;
    let obs = pairing_observer(&app);
    run(move || {
        let offer = w.ring.pair_phone(obs)?;
        let qr = qr_rows(&offer.payload)?;
        Ok(PhoneStart {
            payload: offer.payload,
            qr,
            expires_at: offer.expires_at,
        })
    })
    .await
}

#[tauri::command]
pub async fn ring_pair_phone_cancel(app: AppHandle) -> Result<(), String> {
    let w = worker(&app)?;
    run(move || {
        w.ring.cancel_pairing(PairingFlow::Phone);
        Ok(())
    })
    .await
}

/// Adds the computer that `xshelld pair` shows `code` on. Returns once the code is valid;
/// progress arrives as `ring:pairing`.
#[tauri::command]
pub async fn ring_pair_computer(app: AppHandle, code: String) -> Result<(), String> {
    let w = worker(&app)?;
    let obs = pairing_observer(&app);
    run(move || w.ring.pair_computer(&code, obs)).await
}

#[tauri::command]
pub async fn ring_pair_cancel(app: AppHandle) -> Result<(), String> {
    let w = worker(&app)?;
    run(move || {
        w.ring.cancel_pairing(PairingFlow::Computer);
        Ok(())
    })
    .await
}

#[cfg(test)]
mod pairing_tests {
    use super::*;

    #[test]
    fn qr_rows_are_square_bits() {
        let payload = format!("xsp1.{}", "A".repeat(300));
        let qr = qr_rows(&payload).unwrap();
        assert!(qr.size >= 21);
        assert_eq!(qr.rows.len(), qr.size);
        for r in &qr.rows {
            assert_eq!(r.len(), qr.size);
            assert!(r.chars().all(|c| c == '0' || c == '1'));
        }
        // Finder pattern: the top-left module is dark.
        assert!(qr.rows[0].starts_with("1111111"));
    }

    #[test]
    fn pairing_event_flattens_next_to_flow() {
        let v = |flow, event: PairingEvent| {
            serde_json::to_value(PairingPayload {
                flow,
                event: &event,
            })
            .unwrap()
        };
        assert_eq!(
            v(PairingFlow::Phone, PairingEvent::Waiting),
            serde_json::json!({"flow": "phone", "state": "waiting"})
        );
        assert_eq!(
            v(
                PairingFlow::Computer,
                PairingEvent::Failed {
                    code: "not_found".into()
                }
            ),
            serde_json::json!({"flow": "computer", "state": "failed", "code": "not_found"})
        );
    }
}

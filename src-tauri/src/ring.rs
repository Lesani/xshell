//! Settings → Mobile: Tauri glue over `xshell_hostlink::ring`. Commands `ring_status`,
//! `ring_enable`, `ring_set_relay_url`, `ring_claim_host` and `ring_remove_member` (#22); the
//! event `ring:status`.
//! Pairing (#9): `ring_pair_phone_start`, `ring_pair_phone_cancel`, `ring_pair_computer` and
//! `ring_pair_cancel`; the event `ring:pairing`.
//!
//! Every Host's Daemon (the Local Host's and each Remote Host's) joins the Ring whenever
//! its Host connects: the Host Observer records the connection in [`HOST_SYNC`], and
//! `xshell_hostlink::ring::SyncWorker` does the work on its own thread.

use serde::Serialize;
use std::sync::Arc;
use xshell_hostlink::ring::{
    configured_hosts, default_relay_url, DesktopRing, DesktopRingConfig, HostRingState, HostSync,
    PairingEvent, PairingFlow, PairingObserver, RingObserver, RingView, SyncWorker,
};
use xshell_hostlink::{HostStatus, LOCAL_HOST_ID};
use xshell_protocol::ring::SignKey;

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
    _pairing_ipc: Option<xshell_pairing_access::local::Server>,
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
    // A Host counts as configured when this window has it or `settings.json` on disk lists
    // it (another window may have saved it): its Daemon is not offered for removal.
    match app
        .path()
        .resolve("settings.json", tauri::path::BaseDirectory::AppData)
    {
        Ok(settings) => {
            let c = app.clone();
            worker
                .ring
                .set_configured_hosts(configured_hosts(settings, move |id| {
                    c.try_state::<crate::hosts::Hosts>()
                        .is_some_and(|h| h.manager.host(id).is_some())
                }));
        }
        Err(e) => eprintln!("xshell: cannot find settings.json: {e}"),
    }
    let pairing_ipc = xshell_pairing_access::local::endpoint()
        .and_then(|path| xshell_pairing_access::local::serve(worker.ring.clone(), path));
    if let Err(error) = &pairing_ipc {
        eprintln!("xshell: pairing access: {error}");
    }
    app.manage(RingState {
        worker: worker.clone(),
        _pairing_ipc: pairing_ipc.ok(),
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

/// Moves the Ring to `url` at once (ADR-0012): a new Roster version, and the connection and
/// every connected Daemon follow without waiting for the old Relay. Devices still on an
/// unreachable old Relay are owed a farewell, which runs in the background (`farewells` in
/// the status).
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

/// Removes a device from the Ring (#22): a new Roster version without it. Every Daemon
/// connected over its Host link is told at once; the others learn it from the Relay.
#[tauri::command]
pub async fn ring_remove_member(app: AppHandle, sign_key: String) -> Result<RingStatus, String> {
    let w = worker(&app)?;
    run(move || {
        let key = SignKey::parse(&sign_key).map_err(|e| e.to_string())?;
        w.remove_member(&key)?;
        HOST_SYNC.poke_all();
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
    pub operation: uuid::Uuid,
    pub payload: String,
    pub qr: Qr,
    pub expires_at: u64,
}

#[derive(Serialize, Clone)]
pub struct PairingStarted {
    pub operation: uuid::Uuid,
}

/// The `ring:pairing` event: `{flow, state, name?, role?, code?}`.
#[derive(Serialize, Clone)]
struct PairingPayload<'a> {
    operation: uuid::Uuid,
    flow: PairingFlow,
    #[serde(flatten)]
    event: &'a PairingEvent,
}

struct TauriPairingObserver<R: tauri::Runtime = tauri::Wry>(AppHandle<R>, uuid::Uuid);

impl<R: tauri::Runtime> PairingObserver for TauriPairingObserver<R> {
    fn pairing(&self, flow: PairingFlow, event: &PairingEvent) {
        let _ = self.0.emit(
            "ring:pairing",
            PairingPayload {
                operation: self.1,
                flow,
                event,
            },
        );
    }
}

fn pairing_observer(app: &AppHandle, operation: uuid::Uuid) -> Arc<dyn PairingObserver> {
    Arc::new(TauriPairingObserver(app.clone(), operation))
}

/// Shows a new phone offer (cancelling the previous one). Progress arrives as `ring:pairing`.
#[tauri::command]
pub async fn ring_pair_phone_start(
    app: AppHandle,
    operation: uuid::Uuid,
) -> Result<PhoneStart, String> {
    let w = worker(&app)?;
    let obs = pairing_observer(&app, operation);
    run(move || {
        let offer = w.ring.pair_phone_owned(operation, obs)?;
        let qr = qr_rows(&offer.payload)?;
        Ok(PhoneStart {
            operation,
            payload: offer.payload,
            qr,
            expires_at: offer.expires_at,
        })
    })
    .await
}

#[tauri::command]
pub async fn ring_pair_phone_cancel(app: AppHandle, operation: uuid::Uuid) -> Result<(), String> {
    let w = worker(&app)?;
    run(move || {
        w.ring.cancel_pairing_owned(PairingFlow::Phone, operation);
        Ok(())
    })
    .await
}

/// Adds the computer that `xshelld pair` shows `code` on. Returns once the code is valid;
/// progress arrives as `ring:pairing`.
#[tauri::command]
pub async fn ring_pair_computer(
    app: AppHandle,
    code: String,
    operation: uuid::Uuid,
) -> Result<PairingStarted, String> {
    let w = worker(&app)?;
    let obs = pairing_observer(&app, operation);
    run(move || {
        w.ring
            .pair_computer_owned(operation, &code, obs)
            .map(|()| PairingStarted { operation })
    })
    .await
}

#[tauri::command]
pub async fn ring_pair_cancel(app: AppHandle, operation: uuid::Uuid) -> Result<(), String> {
    let w = worker(&app)?;
    run(move || {
        w.ring
            .cancel_pairing_owned(PairingFlow::Computer, operation);
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
    fn tauri_pairing_delivery_preserves_operation_identity() {
        use tauri::Listener;
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.listen_any("ring:pairing", move |event| {
            tx.send(event.payload().to_string()).unwrap();
        });
        let operation = uuid::Uuid::from_u128(19);
        let observer = TauriPairingObserver(app.handle().clone(), operation);
        observer.pairing(PairingFlow::Phone, &PairingEvent::Waiting);
        observer.pairing(PairingFlow::Phone, &PairingEvent::Expired);
        for state in ["waiting", "expired"] {
            let payload: serde_json::Value =
                serde_json::from_str(&rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap())
                    .unwrap();
            assert_eq!(payload["operation"], operation.to_string());
            assert_eq!(payload["state"], state);
        }
    }

    #[test]
    fn pairing_event_flattens_next_to_flow() {
        let v = |flow, event: PairingEvent| {
            serde_json::to_value(PairingPayload {
                operation: uuid::Uuid::nil(),
                flow,
                event: &event,
            })
            .unwrap()
        };
        assert_eq!(
            v(PairingFlow::Phone, PairingEvent::Waiting),
            serde_json::json!({"operation": uuid::Uuid::nil(), "flow": "phone", "state": "waiting"})
        );
        assert_eq!(
            v(
                PairingFlow::Computer,
                PairingEvent::Failed {
                    code: "not_found".into()
                }
            ),
            serde_json::json!({"operation": uuid::Uuid::nil(), "flow": "computer", "state": "failed", "code": "not_found"})
        );
    }
}

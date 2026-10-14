//! Native drag-out of explorer files to the OS (Lesani/xshell#46, D3): the explorer's grip
//! calls [`drag_out_start`] while the mouse button is still held, with local paths (a Local
//! Host file itself, or a Remote Host file prepared in the drag area of `open_copies.rs`).
//!
//! The drag runs on the main thread through the vendored, patched `drag` crate (see
//! `vendor/drag/XSHELL-PATCH.md`). Release builds abort on panic, so nothing here unwraps.

use serde::Serialize;
use std::path::PathBuf;
use tauri::ipc::Channel;

/// What the webview hears about a drag it started: one message, at its end.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum DragEvent {
    /// The drag could not start (or not be set up).
    Failed { error: String },
    /// The drag ended: dropped somewhere, or cancelled.
    Ended { dropped: bool },
}

/// The paths a drag may carry: absolute, existing.
fn checked_paths(paths: &[String]) -> Result<Vec<PathBuf>, String> {
    if paths.is_empty() {
        return Err("nothing to drag".into());
    }
    paths
        .iter()
        .map(|p| crate::open_copies::check_openable(p))
        .collect()
}

/// Start a native drag of `paths` out of `window`. Returns once the drag is scheduled on the
/// main thread; how it went arrives on `on_event` (exactly one [`DragEvent`]).
#[tauri::command]
pub fn drag_out_start(
    window: tauri::WebviewWindow,
    paths: Vec<String>,
    on_event: Channel<DragEvent>,
) -> Result<(), String> {
    let paths = checked_paths(&paths)?;
    let w = window.clone();
    let events = on_event.clone();
    window
        .run_on_main_thread(move || {
            if let Err(error) = start_on_main_thread(&w, paths, events.clone()) {
                let _ = events.send(DragEvent::Failed { error });
            }
        })
        .map_err(|e| e.to_string())
}

fn start_on_main_thread(
    window: &tauri::WebviewWindow,
    paths: Vec<PathBuf>,
    events: Channel<DragEvent>,
) -> Result<(), String> {
    let item = drag::DragItem::Files(paths);
    let icon = drag::Image::Raw(include_bytes!("../icons/32x32.png").to_vec());
    let on_end = move |result: drag::DragResult, _: drag::CursorPosition| {
        let dropped = matches!(result, drag::DragResult::Dropped);
        let _ = events.send(DragEvent::Ended { dropped });
    };
    let options = drag::Options {
        skip_animatation_on_cancel_or_failure: false,
        mode: drag::DragMode::Copy,
    };
    #[cfg(target_os = "linux")]
    {
        let gtk_window = window.gtk_window().map_err(|e| e.to_string())?;
        drag::start_drag(&gtk_window, item, icon, on_end, options).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        drag::start_drag(window, item, icon, on_end, options).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_must_be_absolute_and_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let ok = checked_paths(&[f.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(ok, vec![f.clone()]);
        assert!(checked_paths(&[]).is_err());
        assert!(checked_paths(&["a.txt".into()]).is_err());
        let missing = tmp.path().join("missing").to_string_lossy().into_owned();
        assert!(checked_paths(&[f.to_string_lossy().into_owned(), missing]).is_err());
    }

    #[test]
    fn events_serialize_for_the_webview() {
        let v = serde_json::to_value(DragEvent::Ended { dropped: true }).unwrap();
        assert_eq!(v, serde_json::json!({"event": "ended", "dropped": true}));
        let v = serde_json::to_value(DragEvent::Failed { error: "x".into() }).unwrap();
        assert_eq!(v, serde_json::json!({"event": "failed", "error": "x"}));
    }
}

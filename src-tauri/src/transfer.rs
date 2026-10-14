//! Whole downloads to, and path uploads from, this machine: the pump runs here, so the bytes
//! never cross into the webview (one IPC call and a progress `Channel` per transfer). Uploads
//! of bytes that only exist in the webview (dropped `File`s) loop over the Host methods in
//! TypeScript instead.

use crate::hosts::Hosts;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::State;
use xshell_core::transfer::{OnClash, Transferred};
use xshell_hostlink::{HostError, HostHandle};

/// `file_read` calls in flight per Host, across every pump.
pub const READS_PER_HOST: usize = 4;
/// How long a cancel for a transfer not registered (yet) is remembered.
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(60);

#[derive(Default, Clone)]
pub struct Transfers {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    flags: Mutex<HashMap<String, Flag>>,
    reads: ReadGates,
}

struct Flag {
    cancel: Arc<AtomicBool>,
    /// Set for a cancel that came before its transfer: dropped after [`EARLY_CANCEL_TTL`].
    early: Option<Instant>,
}

impl Transfers {
    /// The cancel flag of `id`, registered before its pump is scheduled. A cancel that came
    /// first leaves it set already.
    fn register(&self, id: &str) -> Arc<AtomicBool> {
        let mut flags = self.inner.flags.lock().unwrap();
        let now = Instant::now();
        flags.retain(|_, f| {
            f.early
                .is_none_or(|t| now.duration_since(t) < EARLY_CANCEL_TTL)
        });
        let f = flags.entry(id.to_string()).or_insert_with(|| Flag {
            cancel: Arc::new(AtomicBool::new(false)),
            early: None,
        });
        f.early = None;
        f.cancel.clone()
    }

    fn cancel(&self, id: &str) {
        let mut flags = self.inner.flags.lock().unwrap();
        let f = flags.entry(id.to_string()).or_insert_with(|| Flag {
            cancel: Arc::new(AtomicBool::new(false)),
            early: Some(Instant::now()),
        });
        f.cancel.store(true, Ordering::SeqCst);
    }

    fn done(&self, id: &str) {
        self.inner.flags.lock().unwrap().remove(id);
    }
}

/// Removes a transfer's flag when its command ends, however it ends.
struct Registered<'a> {
    t: &'a Transfers,
    id: String,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.t.done(&self.id);
    }
}

/// A counting gate per Host: at most [`READS_PER_HOST`] `file_read` calls at once.
#[derive(Default)]
struct ReadGates {
    in_flight: Mutex<HashMap<String, usize>>,
    cv: Condvar,
}

impl ReadGates {
    fn enter(&self, host: &str) -> ReadPermit<'_> {
        let mut m = self.in_flight.lock().unwrap();
        while m.get(host).copied().unwrap_or(0) >= READS_PER_HOST {
            m = self.cv.wait(m).unwrap();
        }
        *m.entry(host.to_string()).or_default() += 1;
        ReadPermit {
            gates: self,
            host: host.to_string(),
        }
    }
}

struct ReadPermit<'a> {
    gates: &'a ReadGates,
    host: String,
}

impl Drop for ReadPermit<'_> {
    fn drop(&mut self) {
        let mut m = self.gates.in_flight.lock().unwrap();
        if let Some(n) = m.get_mut(&self.host) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.host);
            }
        }
        self.gates.cv.notify_all();
    }
}

/// `call` with its `file_read`s through `host`'s gate.
fn gated<'a, E>(
    gates: &'a ReadGates,
    host: &'a str,
    mut call: impl FnMut(&str, Value) -> Result<Value, E> + 'a,
) -> impl FnMut(&str, Value) -> Result<Value, E> + 'a {
    move |m, p| {
        let _permit = (m == "file_read").then(|| gates.enter(host));
        call(m, p)
    }
}

/// Where a pump's Host calls go: this process's own core (the Local Host), or a Remote
/// Host's link.
enum Target {
    Local,
    Remote(Arc<HostHandle>),
}

impl Target {
    fn new(hosts: &Hosts, host: Option<&str>) -> Result<Self, HostError> {
        match host {
            None => Ok(Target::Local),
            Some(h) => crate::hosts::commands::handle(hosts, h).map(Target::Remote),
        }
    }

    /// A blocking call. Local errors are `remote`, like a remote Daemon's own error text.
    fn call(&self, method: &str, params: Value) -> Result<Value, HostError> {
        match self {
            Target::Local => {
                xshell_core::dispatch(&crate::ctx(), method, params).map_err(HostError::remote)
            }
            Target::Remote(h) => {
                let (tx, rx) = std::sync::mpsc::channel();
                h.call(
                    method.to_string(),
                    params,
                    Box::new(move |r| {
                        let _ = tx.send(r);
                    }),
                );
                rx.recv()
                    .unwrap_or_else(|_| Err(HostError::offline("the request was dropped")))
            }
        }
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

/// Run `pump` off the runtime with `id`'s cancel flag, which is registered first.
async fn run<F>(transfers: &Transfers, id: String, pump: F) -> Result<Transferred, HostError>
where
    F: FnOnce(&Transfers, &AtomicBool) -> Result<Transferred, HostError> + Send + 'static,
{
    let flag = transfers.register(&id);
    let _registered = Registered { t: transfers, id };
    let t = transfers.clone();
    tauri::async_runtime::spawn_blocking(move || pump(&t, &flag))
        .await
        .map_err(|e| HostError::offline(e.to_string()))?
}

/// Download `remote_path` on `host` (`None`: this machine) to `local_path`. The caller
/// takes `local_path` from a Save dialog or its own temp area only.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn transfer_download(
    transfers: State<'_, Transfers>,
    hosts: State<'_, Hosts>,
    transfer_id: String,
    host: Option<String>,
    remote_path: String,
    local_path: String,
    private: bool,
    on_progress: Channel<Progress>,
) -> Result<Transferred, HostError> {
    let target = Target::new(&hosts, host.as_deref())?;
    run(&transfers, transfer_id, move |t, cancel| {
        download(
            t,
            &target,
            host,
            &remote_path,
            &local_path,
            private,
            &on_progress,
            cancel,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
fn download(
    t: &Transfers,
    target: &Target,
    host: Option<String>,
    remote_path: &str,
    local_path: &str,
    private: bool,
    on_progress: &Channel<Progress>,
    cancel: &AtomicBool,
) -> Result<Transferred, HostError> {
    let key = host.unwrap_or_default();
    let mut call = gated(&t.inner.reads, &key, |m: &str, p| target.call(m, p));
    xshell_core::transfer::download_with(
        &mut call,
        &|m| HostError::remote(m),
        remote_path,
        Path::new(local_path),
        private,
        &mut |done, total| {
            let _ = on_progress.send(Progress { done, total });
        },
        cancel,
    )
}

/// Upload the local file `local_path` to `dir/name` on `host`.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn transfer_upload_path(
    transfers: State<'_, Transfers>,
    hosts: State<'_, Hosts>,
    transfer_id: String,
    host: Option<String>,
    local_path: String,
    dir: String,
    name: String,
    on_clash: OnClash,
    if_version: Option<String>,
    on_progress: Channel<Progress>,
) -> Result<Transferred, HostError> {
    let target = Target::new(&hosts, host.as_deref())?;
    run(&transfers, transfer_id, move |_, cancel| {
        xshell_core::transfer::upload_path_with(
            &mut |m: &str, p| target.call(m, p),
            &|m| HostError::remote(m),
            Path::new(&local_path),
            &dir,
            &name,
            on_clash,
            if_version,
            &mut |done, total| {
                let _ = on_progress.send(Progress { done, total });
            },
            cancel,
        )
    })
    .await
}

/// Stop a transfer (also one whose command has not started yet). An upload whose commit
/// is under way completes.
#[tauri::command]
pub fn transfer_cancel(transfers: State<'_, Transfers>, transfer_id: String) {
    transfers.cancel(&transfer_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use xshell_core::transfer::{download_with, TRANSFER_CANCELLED};

    #[test]
    fn cancel_before_register_is_kept() {
        let t = Transfers::default();
        t.cancel("a");
        let flag = t.register("a");
        assert!(flag.load(Ordering::SeqCst));
        t.done("a");
        let flag = t.register("a");
        assert!(!flag.load(Ordering::SeqCst));
        t.cancel("a");
        assert!(flag.load(Ordering::SeqCst));
        // A pump that starts cancelled makes no call.
        let calls = AtomicUsize::new(0);
        let e = download_with(
            &mut |_: &str, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<Value, HostError>(Value::Null)
            },
            &|m| HostError::remote(m),
            "/r",
            Path::new("/nonexistent/l"),
            false,
            &mut |_, _| {},
            &flag,
        )
        .unwrap_err();
        assert_eq!(e.message, TRANSFER_CANCELLED);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn early_cancels_expire() {
        let t = Transfers::default();
        t.cancel("old");
        t.inner.flags.lock().unwrap().get_mut("old").unwrap().early =
            Some(Instant::now() - EARLY_CANCEL_TTL * 2);
        t.register("new");
        assert!(!t.inner.flags.lock().unwrap().contains_key("old"));
    }

    /// Ten pumps against one fake Host never have more than four reads in flight; another
    /// Host has its own four.
    #[test]
    fn reads_per_host_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transfers::default();
        let now = Arc::new(Mutex::new(HashMap::<String, (usize, usize)>::new()));
        let threads: Vec<_> = (0..10)
            .map(|i| {
                let (t, now) = (t.clone(), now.clone());
                let local = dir.path().join(format!("f{i}"));
                std::thread::spawn(move || {
                    let host = if i % 2 == 0 { "a" } else { "b" }.to_string();
                    let fake = |m: &str, p: Value| -> Result<Value, HostError> {
                        let size = 3 * xshell_core::transfer::MAX_CHUNK;
                        if m == "file_stat" {
                            return Ok(serde_json::json!({ "size": size, "mtime_ms": 1,
                                "is_dir": false, "is_symlink": false, "version": null }));
                        }
                        {
                            let mut g = now.lock().unwrap();
                            let e = g.entry(host.clone()).or_default();
                            e.0 += 1;
                            e.1 = e.1.max(e.0);
                        }
                        std::thread::sleep(Duration::from_millis(15));
                        now.lock().unwrap().get_mut(&host).unwrap().0 -= 1;
                        let off = p["offset"].as_u64().unwrap();
                        let n = (size - off).min(xshell_core::transfer::MAX_CHUNK);
                        use base64::Engine as _;
                        let b =
                            base64::engine::general_purpose::STANDARD.encode(vec![0u8; n as usize]);
                        Ok(serde_json::json!({ "bytes_base64": b, "eof": off + n >= size }))
                    };
                    let mut call = gated(&t.inner.reads, &host, fake);
                    download_with(
                        &mut call,
                        &|m| HostError::remote(m),
                        "/r",
                        &local,
                        false,
                        &mut |_, _| {},
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                })
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        let g = now.lock().unwrap();
        for h in ["a", "b"] {
            let (open, max) = g[h];
            assert_eq!(open, 0);
            assert!(max <= READS_PER_HOST, "{h}: {max}");
            assert!(max > 1, "{h}: the pumps ran side by side");
        }
        assert!(t.inner.reads.in_flight.lock().unwrap().is_empty());
    }
}

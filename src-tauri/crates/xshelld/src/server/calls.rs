//! `call`: any Host-side command through core's dispatch table, each on its own thread so a
//! slow one (a big `git` status) never stalls Terminal input on the same connection.

use super::conn::reply;
use super::outbox::Outbox;
use super::registry::Daemon;
use super::role::{self, Role};
use serde_json::Value;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub(crate) fn spawn_call(
    d: &Arc<Daemon>,
    ob: &Arc<Outbox>,
    inflight: &Arc<AtomicUsize>,
    role: Role,
    id: Option<u64>,
    method: String,
    params: Value,
) {
    if inflight.fetch_add(1, Ordering::SeqCst) >= d.cfg.max_calls_per_conn {
        inflight.fetch_sub(1, Ordering::SeqCst);
        reply(ob, id, Err("too many concurrent calls".into()));
        return;
    }
    let name: String = method
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(24)
        .collect();
    let (ctx, ob2, inflight2) = (d.ctx.clone(), ob.clone(), inflight.clone());
    let r = std::thread::Builder::new()
        .name(format!("call-{name}"))
        .spawn(move || {
            // Needs `panic = "unwind"`: xshelld release builds use the release-daemon profile.
            // The role's history checks run here, off the connection's reader.
            let res = catch_unwind(AssertUnwindSafe(|| {
                role::authorize_call(role, &ctx, &method, &params)?;
                xshell_core::dispatch(&ctx, &method, params)
            }))
            .unwrap_or_else(|_| Err(format!("internal error in {method}")));
            reply(&ob2, id, res);
            inflight2.fetch_sub(1, Ordering::SeqCst);
        });
    if let Err(e) = r {
        inflight.fetch_sub(1, Ordering::SeqCst);
        reply(ob, id, Err(format!("cannot start call: {e}")));
    }
}

//! Agent Status: the hooks agents launch with, `term.event` reports from them, and
//! publishing a changed status in the `terminals` list.

use super::conn::reply;
use super::outbox::Outbox;
use super::registry::Daemon;
use super::terminal::Terminal;
use super::Config;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;
use xshell_core::agent_status::{AgentHooks, AgentStatus};

/// The hooks for this Daemon's agents, with their Claude Code settings file written. `None`
/// (agents launch without hooks) without an event executable or when the file cannot be
/// written.
pub(crate) fn hooks(cfg: &Config) -> Option<AgentHooks> {
    let hooks = AgentHooks {
        exe: cfg.event_exe.clone()?,
        endpoint: cfg.paths.socket.to_string_lossy().into_owned(),
        claude_settings: cfg.paths.claude_hooks.clone(),
    };
    match hooks.write_claude_settings() {
        Ok(()) => Some(hooks),
        Err(e) => {
            crate::log!(
                "WARN",
                "agents launch without status hooks: cannot write {}: {e}",
                hooks.claude_settings.display()
            );
            None
        }
    }
}

/// `term.event`: a hook reports `status` for `terminal`'s process `run`. Refused for an
/// unknown Terminal, an older run and a Terminal whose agent reports none.
pub(crate) fn on_event(
    d: &Arc<Daemon>,
    ob: &Outbox,
    id: Option<u64>,
    terminal: Uuid,
    run: u64,
    status: AgentStatus,
) {
    let reg = d.reg.lock().unwrap();
    let r = match reg.terminals.get(&terminal) {
        None => Err(format!("unknown terminal {terminal}")),
        Some(t) => t.on_agent_event(run, status).map(|changed| {
            if changed && !reg.frozen {
                d.broadcast_terminals(&reg);
                // The agent reported it itself: a push may go out (input never pushes).
                d.push.notify(terminal, run, status);
            }
            Value::Null
        }),
    };
    drop(reg);
    reply(ob, id, r);
}

/// `t`'s Agent Status changed by itself (input, output, exit): tell every connection, if
/// `t` is the Terminal listed under its UUID.
pub(crate) fn changed(d: &Daemon, t: &Terminal) {
    let reg = d.reg.lock().unwrap();
    let listed = reg
        .terminals
        .get(&t.id)
        .is_some_and(|c| std::ptr::eq(Arc::as_ptr(c), t));
    if listed && !reg.frozen {
        d.broadcast_terminals(&reg);
    }
}

//! `term.relaunch`: end a Terminal's process and start it again under the same UUID with
//! `skipPermissions` changed, resuming the agent's session. Attached connections stay
//! attached: they see the old output, a reset, then the new process's output, and never a
//! `term.exit`. A Relaunch that fails after the old process ended leaves the Terminal listed
//! as ended with its spec unchanged, as if the process had exited by itself.

use super::conn::reply;
use super::outbox::Outbox;
use super::registry::{Daemon, Registry};
use super::terminal::{self, SpawnError, Terminal};
use super::TestPoint;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::launch::relaunch_spec;
use xshell_core::terminal::state::Leader;

/// How long past the kill grace the old process may take to end before the Relaunch gives
/// up. SIGKILL is sent at the grace, so this only covers reaping and draining its output.
const EXIT_SLACK: Duration = Duration::from_secs(3);

/// Validate and reserve the Terminal, then end and restart it on a worker thread. Answers
/// `{pid, relaunched}`; `relaunched` is false when `skip` is already the Terminal's value.
pub(crate) fn start(
    d: &Arc<Daemon>,
    ob: &Arc<Outbox>,
    id: Option<u64>,
    terminal: Uuid,
    skip: bool,
) {
    let mut reg = d.reg.lock().unwrap();
    let t = match reserve(d, &reg, terminal, skip) {
        Ok(Reserved::Started(t)) => t,
        Ok(Reserved::Unchanged(pid)) => {
            drop(reg);
            reply(ob, id, Ok(json!({ "pid": pid, "relaunched": false })));
            return;
        }
        Err(e) => {
            drop(reg);
            reply(ob, id, Err(e));
            return;
        }
    };
    d.persist(&reg);
    let (d2, t2, ob2) = (d.clone(), t.clone(), ob.clone());
    let r = if d.test_point(terminal, TestPoint::StartWorker) {
        Err(std::io::Error::other("refused by a test hook"))
    } else {
        std::thread::Builder::new()
            .name(format!("relaunch-{}", &terminal.simple().to_string()[..8]))
            .spawn(move || run(&d2, &t2, skip, &ob2, id))
            .map(drop)
    };
    if let Err(e) = r {
        t.abort_relaunch(d, &mut reg);
        drop(reg);
        reply(ob, id, Err(format!("cannot start the relaunch: {e}")));
    }
}

enum Reserved {
    Started(Arc<Terminal>),
    /// Already at the requested value: the current pid.
    Unchanged(Option<u32>),
}

/// The checks that need no process change, in order: the Daemon, the Terminal's lifecycle,
/// the spec, the no-op case, the list budget. Then the reservation.
fn reserve(
    d: &Arc<Daemon>,
    reg: &Registry,
    terminal: Uuid,
    skip: bool,
) -> Result<Reserved, String> {
    if reg.frozen {
        return Err("xshelld is upgrading or shutting down".into());
    }
    let t = reg
        .terminals
        .get(&terminal)
        .cloned()
        .ok_or_else(|| format!("unknown terminal {terminal}"))?;
    t.check_relaunch()?;
    let current = t.spec();
    let spec = relaunch_spec(&current, skip)?;
    if current.skip_permissions.unwrap_or(false) == skip {
        return Ok(Reserved::Unchanged(t.info().pid));
    }
    let (_, meta, _, _) = t.relaunch_parts();
    d.check_budget(reg, terminal, &spec, &meta)?;
    t.begin_relaunch(skip)?;
    Ok(Reserved::Started(t))
}

/// The worker: end the old process, then, under the registry lock, start the replacement
/// from the record as it is now (a `term.update` in the meantime counts) and list it in the
/// old one's place.
fn run(d: &Arc<Daemon>, t: &Arc<Terminal>, skip: bool, ob: &Arc<Outbox>, id: Option<u64>) {
    let deadline = Instant::now() + d.cfg.kill_grace + EXIT_SLACK;
    t.signal_groups(d.cfg.kill_grace);
    d.test_point(t.id, TestPoint::Signalled);
    let exited = t.wait_exited(deadline);
    let timed_out = d.test_point(t.id, TestPoint::Waited { exited }) || !exited;

    let mut reg = d.reg.lock().unwrap();
    if timed_out {
        // Its exit may have landed since the wait gave up; it is published now, once.
        t.abort_relaunch(d, &mut reg);
        drop(reg);
        reply(ob, id, Err("previous process did not exit".into()));
        return;
    }
    if !d.is_current(&reg, t) {
        drop(reg);
        reply(
            ob,
            id,
            Err("terminal was closed during the relaunch".into()),
        );
        return;
    }
    match replacement(d, &reg, t, skip) {
        Ok(next) => {
            let dropped = t.hand_over(&next);
            let pid = next.info().pid;
            reg.terminals.insert(next.id, next);
            d.persist(&reg);
            d.broadcast_terminals(&reg);
            drop(reg);
            d.nudge_overflowed(dropped);
            reply(ob, id, Ok(json!({ "pid": pid, "relaunched": true })));
        }
        Err(e) => {
            if let Some(failed) = e.started {
                // Its process exists but not all of its threads: it is being ended. The state
                // file keeps naming it until it is confirmed gone; its exit needs the lock.
                drop(reg);
                let gone = failed.wait_exited(Instant::now() + d.cfg.kill_grace + EXIT_SLACK);
                reg = d.reg.lock().unwrap();
                if gone {
                    t.set_replacement(None);
                }
            }
            t.abort_relaunch(d, &mut reg);
            drop(reg);
            reply(ob, id, Err(e.message));
        }
    }
}

/// Start the replacement. Its identity is persisted (as the old Terminal's leader) before
/// any of its threads start, so a crash from then on leaves a record that ends it.
fn replacement(
    d: &Arc<Daemon>,
    reg: &Registry,
    t: &Arc<Terminal>,
    skip: bool,
) -> Result<Arc<Terminal>, SpawnError> {
    if reg.frozen {
        return Err(String::from("xshelld is upgrading or shutting down").into());
    }
    let (spec, meta, created_at_ms, size) = t.relaunch_parts();
    let spec = relaunch_spec(&spec, skip)?;
    d.check_budget(reg, t.id, &spec, &meta)?;
    let spawned = |leader: &Leader| {
        t.set_replacement(Some(leader.clone()));
        d.persist(reg);
        d.test_point(t.id, TestPoint::ReplacementSpawned { pid: leader.pid });
    };
    terminal::spawn_with(d, t.id, spec, meta, size, created_at_ms, spawned).map_err(|e| {
        SpawnError {
            message: format!("restart failed: {}", e.message),
            started: e.started,
        }
    })
}

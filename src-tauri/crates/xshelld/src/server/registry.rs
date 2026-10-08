//! The shared Daemon state: Terminals, connections, persistence, idle tracking and exit.

use super::orphans::{self, Cleanup};
use super::outbox::Outbox;
use super::terminal::{self, Terminal};
use super::{conn, Config, ConnId, ExitReason};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_core::protocol::msg::{encode_msg, ServerMsg, TerminalInfo};
use xshell_core::terminal::state;
use xshell_core::HostCtx;

pub(crate) struct Daemon {
    pub cfg: Config,
    pub ctx: Arc<HostCtx>,
    pub reg: Mutex<Registry>,
    /// Set once by the first `exit`.
    pub exiting: AtomicBool,
    /// The accept loop stops when it sees this.
    pub stopping: AtomicBool,
    pub exit: Mutex<Option<ExitReason>>,
    pub exit_cv: Condvar,
    pub lock_file: Mutex<Option<fs::File>>,
    pub next_conn: AtomicU64,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub terminals: BTreeMap<Uuid, Arc<Terminal>>,
    pub conns: HashMap<ConnId, Arc<Outbox>>,
    /// `Some` iff there are no Terminals and no connections.
    pub idle_since: Option<Instant>,
    /// Upgrading or shutting down: no persistence, no `term.open`, no list changes.
    pub frozen: bool,
    /// Shutting down: no new connections are registered.
    pub closed: bool,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Encode a server message. Never panics: a message that does not fit in a frame is logged
/// and dropped (the list budget keeps `terminals` far below the limit).
pub(crate) fn frame(msg: &ServerMsg) -> Option<Arc<[u8]>> {
    match encode_msg(msg, None) {
        Ok(f) => Some(Arc::from(f)),
        Err(e) => {
            crate::log!("ERROR", "cannot encode a server message: {e}");
            None
        }
    }
}

impl Daemon {
    pub fn list(&self, reg: &Registry) -> Vec<TerminalInfo> {
        reg.terminals.values().map(|t| t.info()).collect()
    }

    pub fn terminals_frame(&self, reg: &Registry) -> Option<Arc<[u8]>> {
        frame(&ServerMsg::Terminals {
            list: self.list(reg),
        })
    }

    pub fn broadcast_terminals(&self, reg: &Registry) {
        if let Some(f) = self.terminals_frame(reg) {
            for ob in reg.conns.values() {
                ob.push_terminals(f.clone());
            }
        }
    }

    /// Refuse a Terminal entry (new, or `id` updated) that would make one entry or the whole
    /// `terminals` list exceed its budget. Checked before anything is changed.
    pub fn check_budget(
        &self,
        reg: &Registry,
        id: Uuid,
        spec: &LaunchSpec,
        meta: &Map<String, Value>,
    ) -> Result<(), String> {
        let n = terminal::entry_bytes(spec, meta);
        if n > self.cfg.max_terminal_bytes {
            return Err(format!(
                "terminal metadata too large ({n} bytes, max {})",
                self.cfg.max_terminal_bytes
            ));
        }
        let others: usize = reg
            .terminals
            .values()
            .filter(|t| t.id != id)
            .map(|t| t.entry_bytes())
            .sum();
        if others + n > self.cfg.max_list_bytes {
            return Err(format!(
                "terminal list too large ({} bytes, max {})",
                others + n,
                self.cfg.max_list_bytes
            ));
        }
        Ok(())
    }

    pub fn broadcast(&self, reg: &Registry, f: Arc<[u8]>) {
        for ob in reg.conns.values() {
            ob.push_control(f.clone());
        }
    }

    pub fn persist(&self, reg: &Registry) {
        if reg.frozen {
            return;
        }
        let list: Vec<_> = reg.terminals.values().map(|t| t.persisted()).collect();
        if let Err(e) = state::save_atomic(&self.cfg.paths.state, &list) {
            crate::log!(
                "ERROR",
                "cannot save {}: {e}",
                self.cfg.paths.state.display()
            );
        }
    }

    pub fn touch_idle(&self, reg: &mut Registry) {
        if reg.terminals.is_empty() && reg.conns.is_empty() {
            reg.idle_since.get_or_insert_with(Instant::now);
        } else {
            reg.idle_since = None;
        }
    }

    /// Nudge Terminals whose queued output a connection just dropped (rate-limited).
    pub fn nudge_overflowed(&self, ids: Vec<Uuid>) {
        if ids.is_empty() {
            return;
        }
        let ts: Vec<Arc<Terminal>> = {
            let reg = self.reg.lock().unwrap();
            ids.iter()
                .filter_map(|id| reg.terminals.get(id).cloned())
                .collect()
        };
        for t in ts {
            t.overflow_nudge(self.cfg.nudge_delay);
        }
    }

    /// Relaunch every persisted Terminal under its UUID, ending leftovers of the previous
    /// run first. A Terminal that fails to start is dropped.
    pub fn restore(self: &Arc<Self>) {
        let loaded = match state::load(&self.cfg.paths.state) {
            Ok(l) => l,
            Err(e) => {
                crate::log!(
                    "ERROR",
                    "cannot read {}: {e}",
                    self.cfg.paths.state.display()
                );
                state::Loaded::default()
            }
        };
        if let Some(p) = &loaded.moved_aside {
            crate::log!("WARN", "corrupt state file moved to {}", p.display());
        }
        let mut reg = self.reg.lock().unwrap();
        let had = loaded.terminals.len();
        for p in loaded.terminals {
            // No budget check here: budgets bound new mutations, and a record saved under an
            // earlier limit is never dropped for its size.
            if let Some(leader) = &p.leader {
                let cleanup = self.cfg.cleanup_override.unwrap_or(orphans::end_leftovers)(
                    leader,
                    self.cfg.kill_grace,
                );
                if cleanup == Cleanup::Unresolved {
                    crate::log!(
                        "WARN",
                        "not relaunching {}: leftovers of its previous run may still be running; \
                         listed as exited ({})",
                        p.terminal,
                        terminal::UNRESOLVED_EXIT
                    );
                    let t = terminal::unresolved(self, p);
                    reg.terminals.insert(t.id, t);
                    continue;
                }
            }
            match terminal::spawn(
                self,
                p.terminal,
                p.spec,
                p.meta,
                p.cols,
                p.rows,
                p.created_at_ms,
            ) {
                Ok(t) => {
                    reg.terminals.insert(t.id, t);
                }
                Err(e) => crate::log!("WARN", "dropping {}: relaunch failed: {e}", p.terminal),
            }
        }
        if had > 0 || self.cfg.paths.state.exists() {
            self.persist(&reg);
        }
        self.touch_idle(&mut reg);
    }

    pub fn accept_loop(self: Arc<Self>, listener: UnixListener) {
        for s in listener.incoming() {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            match s {
                Ok(sock) => {
                    let id = self.next_conn.fetch_add(1, Ordering::SeqCst);
                    let d = self.clone();
                    let r = std::thread::Builder::new()
                        .name(format!("conn-{id}-r"))
                        .spawn(move || conn::handle(d, sock, id));
                    if let Err(e) = r {
                        crate::log!("ERROR", "cannot start connection thread: {e}");
                    }
                }
                Err(e) => {
                    crate::log!("WARN", "accept failed: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    pub fn supervise(self: Arc<Self>) {
        let tick = (self.cfg.idle_timeout / 4).min(Duration::from_secs(1));
        loop {
            std::thread::sleep(tick);
            if self.exiting.load(Ordering::SeqCst) {
                return;
            }
            let idle = {
                let reg = self.reg.lock().unwrap();
                reg.idle_since
                    .is_some_and(|t| t.elapsed() >= self.cfg.idle_timeout)
            };
            if idle {
                crate::log!("INFO", "idle for {:?}; exiting", self.cfg.idle_timeout);
                self.exit(ExitReason::Idle);
                return;
            }
        }
    }

    /// End every Terminal, close every connection, release the socket and the lock.
    /// Idempotent; only the first caller does the work.
    pub fn exit(self: &Arc<Self>, reason: ExitReason) {
        if self.exiting.swap(true, Ordering::SeqCst) {
            return;
        }
        let terms: Vec<Arc<Terminal>> = {
            let mut reg = self.reg.lock().unwrap();
            reg.frozen = true;
            reg.terminals.values().cloned().collect()
        };
        for t in &terms {
            t.kill(self.cfg.kill_grace);
        }
        let deadline = Instant::now() + self.cfg.kill_grace * 2 + Duration::from_secs(2);
        for t in &terms {
            if !t.wait_exited(deadline) {
                crate::log!("WARN", "terminal {} did not end in time", t.id);
            }
        }

        self.stopping.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = UnixStream::connect(&self.cfg.paths.socket);
        let conns: Vec<Arc<Outbox>> = {
            let mut reg = self.reg.lock().unwrap();
            reg.closed = true;
            reg.conns.values().cloned().collect()
        };
        for ob in &conns {
            ob.close();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        for ob in &conns {
            ob.wait_done(deadline);
        }
        let _ = fs::remove_file(&self.cfg.paths.socket);
        let _ = fs::remove_file(&self.cfg.paths.pid);
        // Released last: a successor may start as soon as this is gone.
        self.lock_file.lock().unwrap().take();
        *self.exit.lock().unwrap() = Some(reason);
        self.exit_cv.notify_all();
    }
}

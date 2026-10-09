//! The shared Daemon state: Terminals, connections, persistence, idle tracking and exit.

use super::orphans::{self, Cleanup};
use super::outbox::Outbox;
use super::terminal::{self, Terminal};
use super::transport::{self, Listener};
use super::{conn, Config, ConnId, ExitReason, Role, TestPoint};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_core::terminal::state;
use xshell_core::HostCtx;
use xshell_protocol::msg::{encode_msg, ServerMsg, TerminalInfo};

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
    /// How agents report their Agent Status; `None` when they launch without hooks.
    pub hooks: Option<xshell_core::agent_status::AgentHooks>,
    /// The next Terminal process's run number (see `Terminal::run`).
    pub next_run: AtomicU64,
    /// Hung-up process groups whose SIGKILL is still due, Terminal listed or not.
    pub escalations: Arc<orphans::Escalations>,
    /// This Host's Ring membership and Relay connection.
    pub ring: super::ring::Ring,
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
    /// Run the test hook at `point`; `false` when there is none.
    pub fn test_point(&self, terminal: Uuid, point: TestPoint) -> bool {
        self.cfg
            .test_hook
            .as_ref()
            .is_some_and(|h| (h.0)(terminal, point))
    }

    /// Whether `t` is the Terminal listed under its UUID. A Relaunch lists a new one under the
    /// same UUID, and a replacement that failed to start is never listed.
    pub fn is_current(&self, reg: &Registry, t: &Arc<Terminal>) -> bool {
        reg.terminals.get(&t.id).is_some_and(|c| Arc::ptr_eq(c, t))
    }

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
        let _ = self.try_persist(reg);
    }

    /// [`Daemon::persist`], reporting a failed write (already logged). Frozen: nothing is
    /// written, and that is not a failure.
    pub fn try_persist(&self, reg: &Registry) -> Result<(), String> {
        if reg.frozen {
            return Ok(());
        }
        let list: Vec<_> = reg.terminals.values().map(|t| t.persisted()).collect();
        state::save_atomic(&self.cfg.paths.state, &list).map_err(|e| {
            crate::log!(
                "ERROR",
                "cannot save {}: {e}",
                self.cfg.paths.state.display()
            );
            format!("cannot save the terminal list: {e}")
        })
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

    /// Whether [`Config::abort`] fired.
    fn aborted(&self) -> bool {
        self.cfg.abort.as_ref().is_some_and(|a| a.is_set())
    }

    /// Relaunch every persisted Terminal under its UUID, ending leftovers of the previous
    /// run first. A Terminal that fails to start is dropped. `false`: [`Config::abort`]
    /// fired, so restore stopped early and saved nothing (the state file keeps every
    /// Terminal for the next start).
    pub fn restore(self: &Arc<Self>) -> bool {
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
            self.test_point(p.terminal, TestPoint::Restore);
            if self.aborted() {
                // Under the lock: no exit of a Terminal restored so far saves a shorter list.
                reg.frozen = true;
                return false;
            }
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
        if self.aborted() {
            reg.frozen = true;
            return false;
        }
        if had > 0 || self.cfg.paths.state.exists() {
            self.persist(&reg);
        }
        self.touch_idle(&mut reg);
        true
    }

    pub fn accept_loop(self: Arc<Self>, listener: Listener) {
        loop {
            let s = transport::accept(&listener);
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            match s {
                Ok(sock) => {
                    let id = self.next_conn.fetch_add(1, Ordering::SeqCst);
                    let d = self.clone();
                    // The socket carries Desktops only: local ones and `xshelld connect`.
                    let r = std::thread::Builder::new()
                        .name(format!("conn-{id}-r"))
                        .spawn(move || conn::handle(d, sock, id, Role::Desktop));
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
        // A GUI-bound Daemon lives exactly as long as the app.
        if self.cfg.gui_bound.is_some() {
            return;
        }
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
        // The goodbye runs alongside ending the Terminals and is over before the lock is
        // released, so an upgraded successor connects only after it.
        let me = self.clone();
        let bye = std::thread::Builder::new()
            .name("ring-bye".into())
            .spawn(move || me.ring.stop(reason.bye_reason()));
        let bye = match bye {
            Ok(h) => Some(h),
            Err(_) => {
                self.ring.stop(reason.bye_reason());
                None
            }
        };
        let terms: Vec<Arc<Terminal>> = {
            let mut reg = self.reg.lock().unwrap();
            reg.frozen = true;
            reg.terminals.values().cloned().collect()
        };
        let hung_up = Instant::now();
        for t in &terms {
            t.kill(self.cfg.kill_grace);
        }
        let deadline = hung_up + self.cfg.kill_grace * 2 + Duration::from_secs(2);
        for t in &terms {
            if !t.wait_exited(deadline) {
                crate::log!("WARN", "terminal {} did not end in time", t.id);
            }
        }
        // A Terminal's exit is its leader's; a process of its group that ignores the hangup
        // must still get its SIGKILL before this process, and the kill timers, are gone.
        // That includes Terminals closed earlier, which left the list already.
        self.escalations.drain(deadline);

        self.stopping.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        transport::wake(&self.cfg.paths.socket);
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
        let _ = transport::remove_endpoint(&self.cfg.paths.socket);
        let _ = fs::remove_file(&self.cfg.paths.pid);
        if let Some(h) = bye {
            let _ = h.join();
        }
        // Released last: a successor may start as soon as this is gone.
        // Unlocked explicitly: a child forked meanwhile may still share the file.
        if let Some(l) = self.lock_file.lock().unwrap().take() {
            let _ = l.unlock();
        }
        *self.exit.lock().unwrap() = Some(reason);
        self.exit_cv.notify_all();
    }
}

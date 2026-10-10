//! The shared Daemon state: Terminals, connections, persistence, idle tracking and exit.

use super::orphans::{self, Cleanup};
use super::outbox::Outbox;
use super::terminal::{self, SessionHold, Terminal};
use super::transport::{self, Listener};
use super::{conn, role, Config, ConnId, ExitReason, Role, TestPoint};
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
    /// Push notifications to the Ring's Mobiles.
    pub push: Arc<super::push::Push>,
    /// Reads agent Terminals' last lines.
    pub last_lines: super::last_line::LastLines,
    /// Reads agent Terminals' Permission Prompts off their screens.
    pub prompts: super::prompts::Prompts,
    /// The state file's writes: the version of the snapshot last put in place. Taken after
    /// `Registry` (every save under it), or alone (a prompt-id floor), and nothing under it.
    pub saves: Mutex<u64>,
    /// Snapshot versions, drawn under the registry lock: a higher one holds newer state.
    pub snap_seq: AtomicU64,
    /// Streams agent Terminals' conversations to subscribed connections.
    pub session_streams: super::session_stream::SessionStreams,
    /// The dropped files replies hold, kept out of the sweep.
    pub drops: Arc<super::drops::Drops>,
}

/// A registered connection: its outbox and the role that decides what it is told.
pub(crate) struct Peer {
    pub ob: Arc<Outbox>,
    pub role: Role,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub terminals: BTreeMap<Uuid, Arc<Terminal>>,
    pub conns: HashMap<ConnId, Peer>,
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

/// Output a connection dropped from its queue for one Terminal: the Terminal, and the
/// connection's outbox when it is a Mobile's (whose recovery differs; see
/// [`Terminal::recover_mobile_overflow`]).
pub(crate) type Overflow = (Uuid, Option<Arc<Outbox>>);

/// The Terminals `ob` dropped output of, as [`Overflow`]s.
pub(crate) fn overflowed(ob: &Arc<Outbox>, ids: Vec<Uuid>) -> impl Iterator<Item = Overflow> {
    let mobile = ob.is_mobile().then(|| ob.clone());
    ids.into_iter().map(move |id| (id, mobile.clone()))
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

    /// The listed Terminal that holds `agent`'s session `sid` (capability
    /// `term.open-existing`), with how it holds it: a live one (also one in the middle of a
    /// Relaunch) first, then one being closed, then an unresolved restored one. Among several
    /// (older duplicates, a relinked Terminal) the oldest wins, then the lowest UUID. A
    /// Terminal whose process ended holds nothing. Call with `reg` locked, so an open that
    /// finds no holder starts its Terminal before any other open looks.
    pub fn session_owner(
        &self,
        reg: &Registry,
        agent: &str,
        sid: &str,
    ) -> Option<(Arc<Terminal>, SessionHold)> {
        let rank = |h: SessionHold| match h {
            SessionHold::Live => 0,
            SessionHold::Closing => 1,
            SessionHold::Unresolved => 2,
            SessionHold::Ended => 3,
        };
        reg.terminals
            .values()
            .filter(|t| t.runs_session(agent, sid))
            .map(|t| (t.clone(), t.session_hold()))
            .filter(|(_, h)| *h != SessionHold::Ended)
            .min_by_key(|(t, h)| (rank(*h), t.created_at_ms(), t.id))
    }

    pub fn list(&self, reg: &Registry) -> Vec<TerminalInfo> {
        reg.terminals.values().map(|t| t.info()).collect()
    }

    /// The `terminals` frame for a connection of `role`: the Terminals it [`sees`](role::sees).
    pub fn terminals_frame(&self, reg: &Registry, role: Role) -> Option<Arc<[u8]>> {
        let mut list = self.list(reg);
        list.retain(|i| role::sees(role, &i.spec));
        frame(&ServerMsg::Terminals { list })
    }

    /// Send every connection the current `terminals` list for its role. Both lists come from
    /// one snapshot, each is encoded once, and only for a role that is connected.
    pub fn broadcast_terminals(&self, reg: &Registry) {
        let (desk, mob) = reg.conns.values().fold((false, false), |(d, m), p| {
            (d || p.role == Role::Desktop, m || p.role == Role::Mobile)
        });
        if !desk && !mob {
            return;
        }
        let list = self.list(reg);
        // Only the entries a Mobile sees are copied, and only when a Mobile is connected.
        let mobile = mob
            .then(|| {
                let list = list
                    .iter()
                    .filter(|i| role::sees(Role::Mobile, &i.spec))
                    .cloned()
                    .collect();
                frame(&ServerMsg::Terminals { list })
            })
            .flatten();
        let full = desk
            .then(|| frame(&ServerMsg::Terminals { list }))
            .flatten();
        for p in reg.conns.values() {
            let f = match p.role {
                Role::Desktop => &full,
                Role::Mobile => &mobile,
            };
            if let Some(f) = f {
                p.ob.push_terminals(f.clone());
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

    /// Make `spec` and `meta` `t`'s record, as a `term.update` and the Daemon's own Codex
    /// link do: checked against the list budget first (refused: nothing changes), then
    /// stored, persisted and published. A changed session drops the previous session's last
    /// line, reads the new one's and resets its session streams.
    pub fn apply_record(
        &self,
        reg: &Registry,
        t: &Terminal,
        spec: LaunchSpec,
        meta: Map<String, Value>,
    ) -> Result<(), String> {
        self.check_budget(reg, t.id, &spec, &meta)?;
        let relinked = spec.session_id != t.spec().session_id;
        t.set_record(spec, meta);
        if relinked {
            // The line was the previous session's; the new one's is read.
            t.set_last_line(None);
            self.last_lines.request(t.id);
            self.session_streams.wake(t.id);
        }
        self.persist(reg);
        self.broadcast_terminals(reg);
        Ok(())
    }

    /// Send `f`, a message about `terminal` (running `spec`), to every connection that
    /// [`sees`](role::sees) that Terminal: a barrier its output never crosses
    /// ([`Outbox::push_about`]).
    pub fn broadcast_about(&self, reg: &Registry, terminal: Uuid, spec: &LaunchSpec, f: Arc<[u8]>) {
        for p in reg.conns.values() {
            if role::sees(p.role, spec) {
                p.ob.push_about(terminal, f.clone());
            }
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
        let (version, list) = self.snapshot(reg);
        let mut last = self.saves.lock().unwrap();
        state::save_atomic(&self.cfg.paths.state, &list).map_err(|e| self.save_failed(e))?;
        *last = (*last).max(version);
        Ok(())
    }

    /// What the state file would hold now, and the snapshot's version.
    pub fn snapshot(&self, reg: &Registry) -> (u64, Vec<state::PersistedTerminal>) {
        let version = self.snap_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let list = reg.terminals.values().map(|t| t.persisted()).collect();
        (version, list)
    }

    /// Save a snapshot taken for `terminal`'s new prompt-id floor, with no registry lock held:
    /// written and synced aside, then put in place only if no newer snapshot is there already
    /// (a newer one holds the floor too: a floor waiting to be saved is in every snapshot).
    pub fn save_snapshot(
        &self,
        terminal: Uuid,
        (version, list): (u64, Vec<state::PersistedTerminal>),
    ) -> Result<(), String> {
        self.test_point(terminal, TestPoint::PromptPersist);
        let tag = format!("{version}.tmp");
        let prepared =
            state::prepare(&self.cfg.paths.state, &list, &tag).map_err(|e| self.save_failed(e))?;
        let mut last = self.saves.lock().unwrap();
        if version <= *last {
            prepared.discard();
            return Ok(());
        }
        prepared.commit().map_err(|e| self.save_failed(e))?;
        *last = version;
        Ok(())
    }

    fn save_failed(&self, e: std::io::Error) -> String {
        crate::log!(
            "ERROR",
            "cannot save {}: {e}",
            self.cfg.paths.state.display()
        );
        format!("cannot save the terminal list: {e}")
    }

    pub fn touch_idle(&self, reg: &mut Registry) {
        if reg.terminals.is_empty() && reg.conns.is_empty() {
            reg.idle_since.get_or_insert_with(Instant::now);
        } else {
            reg.idle_since = None;
        }
    }

    /// Recover Terminals whose queued output a connection just dropped: nudge them to
    /// redraw (rate-limited), or for a Mobile see [`Terminal::recover_mobile_overflow`].
    pub fn nudge_overflowed(&self, ids: Vec<Overflow>) {
        if ids.is_empty() {
            return;
        }
        let ts: Vec<(Arc<Terminal>, Option<Arc<Outbox>>)> = {
            let reg = self.reg.lock().unwrap();
            ids.into_iter()
                .filter_map(|(id, mobile)| Some((reg.terminals.get(&id)?.clone(), mobile)))
                .collect()
        };
        for (t, mobile) in ts {
            match mobile {
                None => t.overflow_nudge(self.cfg.nudge_delay),
                Some(ob) => t.recover_mobile_overflow(&ob, self.cfg.nudge_delay),
            }
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
            let floor = p.prompt_id_floor.unwrap_or(0);
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
                    // Prompt ids continue above the previous run's, whatever the clock says.
                    t.restore_prompt_floor(floor);
                    reg.terminals.insert(t.id, t);
                }
                Err(e) => crate::log!("WARN", "dropping {}: relaunch failed: {e}", p.terminal),
            }
        }
        if self.aborted() {
            reg.frozen = true;
            return false;
        }
        for t in reg.terminals.values() {
            self.last_lines.request(t.id);
        }
        warn_restored_duplicates(&reg);
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
                        .spawn(move || conn::handle(d, sock, id, Role::Desktop, None));
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

    /// Sweep the drop directory of files older than `Config::drop_max_age` every
    /// `Config::drop_sweep` until the Daemon has exited, whether or not anything is dropped.
    pub fn sweep_drops(self: Arc<Self>) {
        let mut g = self.exit.lock().unwrap();
        loop {
            // Checked before every wait: the exit's notice may have come while sweeping.
            let due = Instant::now() + self.cfg.drop_sweep;
            while g.is_none() && !self.exiting.load(Ordering::SeqCst) && Instant::now() < due {
                g = self
                    .exit_cv
                    .wait_timeout(g, due.saturating_duration_since(Instant::now()))
                    .unwrap()
                    .0;
            }
            if g.is_some() || self.exiting.load(Ordering::SeqCst) {
                return;
            }
            drop(g);
            self.drops.sweep(
                &self.ctx,
                self.cfg.drop_max_age,
                std::time::SystemTime::now(),
            );
            g = self.exit.lock().unwrap();
        }
    }

    /// End every Terminal, close every connection, release the socket and the lock.
    /// Idempotent; only the first caller does the work.
    pub fn exit(self: &Arc<Self>, reason: ExitReason) {
        if self.exiting.swap(true, Ordering::SeqCst) {
            return;
        }
        self.push.stop();
        self.last_lines.stop();
        self.prompts.stop();
        self.session_streams.stop();
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
            reg.conns.values().map(|p| p.ob.clone()).collect()
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
        // The Relay connection is gone, so pushes in flight end at once; none writes once
        // the lock is released.
        self.push.shutdown(Instant::now() + Duration::from_secs(5));
        // Released last: a successor may start as soon as this is gone.
        // Unlocked explicitly: a child forked meanwhile may still share the file.
        if let Some(l) = self.lock_file.lock().unwrap().take() {
            let _ = l.unlock();
        }
        *self.exit.lock().unwrap() = Some(reason);
        self.exit_cv.notify_all();
    }
}

/// Log every agent session more than one restored Terminal resumes (a state file written
/// before capability `term.open-existing`, or a relinked Terminal). All of them are kept: a
/// Terminal is never ended silently; `term.open` resumes the oldest.
fn warn_restored_duplicates(reg: &Registry) {
    let mut seen: BTreeMap<(String, String), Vec<Uuid>> = BTreeMap::new();
    for t in reg.terminals.values() {
        if let Some((a, s)) = t.spec().agent_session() {
            seen.entry((a.to_string(), s.to_string()))
                .or_default()
                .push(t.id);
        }
    }
    for ((agent, sid), ids) in seen.into_iter().filter(|(_, ids)| ids.len() > 1) {
        crate::log!(
            "WARN",
            "{} restored terminals resume {agent} session {sid}: {ids:?}; all are kept",
            ids.len()
        );
    }
}

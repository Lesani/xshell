//! One Terminal: a PTY process plus its reader, waiter and input threads, its replay buffer
//! and the connections attached to it.

use super::outbox::Outbox;
use super::registry::Registry;
use super::registry::{frame, Daemon};
use super::size::SizeArbiter;
use super::{ConnId, TestPoint};
use portable_pty::{native_pty_system, MasterPty, PtySize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::{AgentStatus, TerminalHooks, Tracker};
use xshell_core::launch::{relaunch_spec, LaunchSpec};
use xshell_core::terminal::replay::ReplayBuffer;
use xshell_core::terminal::state::{Leader, PersistedTerminal, ProcIdentity};
use xshell_protocol::msg::{encode_res, ServerMsg, TerminalInfo};

const READ_BUF: usize = 16 * 1024;
const INPUT_BACKLOG: usize = 1024;
/// Replay is queued in pieces so the per-connection output cap applies to it as well.
const REPLAY_CHUNK: usize = 256 * 1024;
const OVERFLOW_NUDGE_EVERY: Duration = Duration::from_secs(1);

struct Record {
    spec: LaunchSpec,
    meta: Map<String, Value>,
    created_at_ms: u64,
    /// The `skipPermissions` a Relaunch in progress will start with. Persisted in its place,
    /// so a restart during the Relaunch restores what the user asked for.
    pending_skip: Option<bool>,
    /// The process a Relaunch started to replace this Terminal's, persisted as this
    /// Terminal's leader from the moment it exists until it is listed in its own right or
    /// confirmed gone: a restart in between ends it instead of running a second agent.
    replacement: Option<Leader>,
}

struct TermIo {
    /// `None` for a Terminal restored without a process (see [`unresolved`]).
    master: Option<Box<dyn MasterPty + Send>>,
    arb: SizeArbiter,
}

struct TermOutput {
    replay: ReplayBuffer,
    subs: HashMap<ConnId, Arc<Outbox>>,
    /// Set when `term.exit` is published; no output follows it.
    exit_code: Option<i32>,
}

#[derive(Default)]
struct Life {
    closing: bool,
    /// A Relaunch is ending the process to start a replacement: its exit is held back.
    relaunching: bool,
    reader_done: bool,
    exited: Option<i32>,
    /// An exit held back for a Relaunch, published only if the Relaunch fails.
    held_exit: Option<i32>,
}

impl Life {
    /// Whether a Relaunch may start.
    fn check_relaunch(&self) -> Result<(), String> {
        if self.exited.is_some() {
            Err("terminal has exited".into())
        } else if self.closing {
            Err("terminal is closing".into())
        } else if self.relaunching {
            Err("terminal is already relaunching".into())
        } else {
            Ok(())
        }
    }
}

pub(crate) struct Terminal {
    pub id: Uuid,
    record: Mutex<Record>,
    io: Mutex<TermIo>,
    out: Mutex<TermOutput>,
    input: Mutex<Option<SyncSender<Vec<u8>>>>,
    /// The session leader; portable-pty runs it under `setsid`, so it is also the pgid.
    pid: Option<u32>,
    start_time: Option<u64>,
    life: Mutex<Life>,
    life_cv: Condvar,
    nudge_pending: AtomicBool,
    last_overflow_nudge: Mutex<Option<Instant>>,
    persist_pending: AtomicBool,
    /// For a Terminal restored without a process: the previous run's leader, kept in the
    /// state file so a later start retries ending it.
    kept_leader: Option<Leader>,
    /// This process's run: unique per Daemon start and process, so a hook of a process a
    /// Relaunch or restart replaced never reports for its successor.
    pub run: u64,
    /// The Agent Status of this run. Locked last, never across another lock.
    status: Mutex<Tracker>,
    /// The Daemon's pending SIGKILLs, which [`Terminal::kill`] adds to.
    escalations: Arc<super::orphans::Escalations>,
}

/// Fixed per-entry cost in a `terminals` list on top of the spec and metadata (UUID, pid,
/// exit code, timestamps, keys).
const ENTRY_OVERHEAD: usize = 256;

/// The size a Terminal with this spec and metadata adds to a serialized `terminals` list.
pub(crate) fn entry_bytes(spec: &LaunchSpec, meta: &Map<String, Value>) -> usize {
    let len = |v: serde_json::Result<Vec<u8>>| v.map_or(usize::MAX / 4, |b| b.len());
    len(serde_json::to_vec(spec)) + len(serde_json::to_vec(meta)) + ENTRY_OVERHEAD
}

fn short(id: &Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// Why [`spawn_with`] failed.
pub(crate) struct SpawnError {
    pub message: String,
    /// Set when the process started but some of its threads did not: it is being ended, and
    /// its exit is published on this Terminal (never on a listed one).
    pub started: Option<Arc<Terminal>>,
}

impl From<String> for SpawnError {
    fn from(message: String) -> Self {
        Self {
            message,
            started: None,
        }
    }
}

/// Start a Terminal. Unlike the Desktop, a cwd that is not a directory is an error: the PTY
/// library would silently fall back to `$HOME`, and a restore must fail visibly instead.
pub(crate) fn spawn(
    d: &Arc<Daemon>,
    id: Uuid,
    spec: LaunchSpec,
    meta: Map<String, Value>,
    cols: u16,
    rows: u16,
    created_at_ms: u64,
) -> Result<Arc<Terminal>, String> {
    spawn_with(d, id, spec, meta, (cols, rows), created_at_ms, |_| {}).map_err(|e| e.message)
}

/// [`spawn`], calling `spawned` with the new process's identity as soon as it exists, before
/// any of its threads start.
pub(crate) fn spawn_with(
    d: &Arc<Daemon>,
    id: Uuid,
    spec: LaunchSpec,
    meta: Map<String, Value>,
    (cols, rows): (u16, u16),
    created_at_ms: u64,
    spawned: impl FnOnce(&Leader),
) -> Result<Arc<Terminal>, SpawnError> {
    if !spec.cwd.is_empty() && !Path::new(&spec.cwd).is_dir() {
        return Err(format!("working directory does not exist: {}", spec.cwd).into());
    }
    let run = d.next_run.fetch_add(1, Ordering::SeqCst);
    let hooks = d.hooks.as_ref().map(|hooks| TerminalHooks {
        hooks,
        terminal: id,
        run,
    });
    let plan = xshell_core::plan_command_with(&d.ctx, &spec, hooks)?;
    let tracker = Tracker::new(&spec);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("failed to open PTY: {e}"))?;
    let mut child = pair
        .slave
        .spawn_command(plan.to_command_builder())
        .map_err(|e| format!("failed to start {}: {e}", plan.program))?;
    let pid = child.process_id();
    let start_time = pid.and_then(|p| super::orphans::start_time(p as i32));
    if let Some(pid) = pid {
        spawned(&Leader {
            pid,
            start_time,
            groups: vec![ProcIdentity {
                pid: pid as i32,
                start_time,
            }],
        });
    }
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("failed to clone PTY reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("failed to take PTY writer: {e}"))?;
    let (tx, rx) = sync_channel::<Vec<u8>>(INPUT_BACKLOG);
    let t = Arc::new(Terminal {
        id,
        record: Mutex::new(Record {
            spec,
            meta,
            created_at_ms,
            pending_skip: None,
            replacement: None,
        }),
        io: Mutex::new(TermIo {
            master: Some(pair.master),
            arb: SizeArbiter::new(cols, rows),
        }),
        out: Mutex::new(TermOutput {
            replay: ReplayBuffer::new(d.cfg.replay_capacity),
            subs: HashMap::new(),
            exit_code: None,
        }),
        input: Mutex::new(Some(tx)),
        pid,
        start_time,
        life: Mutex::new(Life::default()),
        life_cv: Condvar::new(),
        nudge_pending: AtomicBool::new(false),
        last_overflow_nudge: Mutex::new(None),
        persist_pending: AtomicBool::new(false),
        kept_leader: None,
        run,
        status: Mutex::new(tracker),
        escalations: d.escalations.clone(),
    });
    let tag = short(&id);

    let (tr, dr) = (t.clone(), d.clone());
    let reader_thread = std::thread::Builder::new()
        .name(format!("pty-read-{tag}"))
        .spawn(move || tr.read_loop(&dr, reader));

    let (tw, dw) = (t.clone(), d.clone());
    let waiter = std::thread::Builder::new()
        .name(format!("pty-wait-{tag}"))
        .spawn(move || {
            let code = match child.wait() {
                Ok(st) => st.exit_code() as i32,
                Err(e) => {
                    crate::log!("WARN", "wait for terminal {} failed: {e}", tw.id);
                    -1
                }
            };
            // `term.exit` follows the last output: wait for the reader to see EOF.
            let mut l = tw.life.lock().unwrap();
            while !l.reader_done {
                l = tw.life_cv.wait(l).unwrap();
            }
            drop(l);
            tw.publish_exit(&dw, code);
        });

    let input_thread = if d.test_point(id, TestPoint::StartInput) {
        Err(std::io::Error::other("refused by a test hook"))
    } else {
        std::thread::Builder::new()
            .name(format!("pty-in-{tag}"))
            .spawn(move || {
                let mut writer = writer;
                for data in rx {
                    if writer
                        .write_all(&data)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })
    };
    if let Some(e) = [reader_thread.err(), waiter.err(), input_thread.err()]
        .into_iter()
        .flatten()
        .next()
    {
        t.kill(d.cfg.kill_grace);
        return Err(SpawnError {
            message: format!("failed to start terminal threads: {e}"),
            started: Some(t),
        });
    }
    Ok(t)
}

impl Terminal {
    fn read_loop(&self, d: &Arc<Daemon>, mut reader: Box<dyn Read + Send>) {
        let mut buf = vec![0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.on_output(d, &buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // EIO once every slave fd is closed.
                Err(_) => break,
            }
        }
        self.life.lock().unwrap().reader_done = true;
        self.life_cv.notify_all();
    }

    fn on_output(&self, d: &Arc<Daemon>, bytes: &[u8]) {
        let chunk: Arc<[u8]> = Arc::from(bytes);
        let mut dropped = Vec::new();
        {
            let mut o = self.out.lock().unwrap();
            o.replay.push(bytes);
            for ob in o.subs.values() {
                dropped.extend(ob.push_output(self.id, chunk.clone()));
            }
        }
        d.nudge_overflowed(dropped);
        if self.status.lock().unwrap().on_output(bytes) {
            super::agent::changed(d, self);
        }
    }

    /// A hook's report for process `run`.
    pub fn on_agent_event(&self, run: u64, status: AgentStatus) -> Result<bool, String> {
        let mut tracker = self.status.lock().unwrap();
        if tracker.agent().is_none() {
            return Err("not an agent terminal".into());
        }
        if run != self.run {
            return Err("stale run".into());
        }
        tracker.on_event(status)
    }

    /// Input about to be written: it may answer a prompt or interrupt a turn.
    pub fn note_input(&self, d: &Daemon, data: &[u8]) {
        if self.status.lock().unwrap().on_input(data) {
            super::agent::changed(d, self);
        }
    }

    /// Runs once the process has exited and its output is drained. During a Relaunch the
    /// exit is held back: no `term.exit`, and the attached connections stay for the
    /// replacement.
    fn publish_exit(self: &Arc<Self>, d: &Arc<Daemon>, code: i32) {
        let mut reg = d.reg.lock().unwrap();
        let held = {
            let mut l = self.life.lock().unwrap();
            let hold = l.relaunching && !l.closing && !reg.frozen;
            if hold {
                l.exited = Some(code);
                l.held_exit = Some(code);
            }
            hold
        };
        if held {
            self.input.lock().unwrap().take();
            self.life_cv.notify_all();
        } else {
            self.finish_exit(d, &mut reg, code);
        }
        drop(reg);
        d.test_point(self.id, TestPoint::ExitHandled { pid: self.pid });
    }

    /// Mark the Terminal ended and tell every connection, unless the Daemon is upgrading or
    /// shutting down or this Terminal is not the one listed under its UUID.
    fn finish_exit(self: &Arc<Self>, d: &Arc<Daemon>, reg: &mut Registry, code: i32) {
        // Published by the `terminals` list below.
        self.status.lock().unwrap().on_exit();
        {
            let mut o = self.out.lock().unwrap();
            o.exit_code = Some(code);
            o.subs.clear();
        }
        // Ends the input thread, which drops the PTY writer.
        self.input.lock().unwrap().take();
        let closing = {
            let mut l = self.life.lock().unwrap();
            l.exited = Some(code);
            l.closing
        };
        self.life_cv.notify_all();
        if reg.frozen {
            // Upgrade/shutdown: the Terminal stays in the state file and on the Desktops.
            return;
        }
        if !d.is_current(reg, self) {
            // A Relaunch replacement whose start failed: the listed Terminal is not this one.
            return;
        }
        if let Some(f) = frame(&ServerMsg::TermExit {
            terminal: self.id,
            code,
        }) {
            d.broadcast(reg, f);
        }
        if closing {
            reg.terminals.remove(&self.id);
            d.touch_idle(reg);
        }
        // Also when it stays listed: the state file must stop naming its process, whose pid
        // may be reused from now on.
        d.persist(reg);
        d.broadcast_terminals(reg);
    }

    /// The current launch spec.
    pub fn spec(&self) -> LaunchSpec {
        self.record.lock().unwrap().spec.clone()
    }

    /// Whether a Relaunch may start now.
    pub fn check_relaunch(&self) -> Result<(), String> {
        self.life.lock().unwrap().check_relaunch()
    }

    /// Reserve the Terminal for a Relaunch to `skip`: from now on its exit is held back and
    /// the state file names the pending spec. Refused while it has exited, is closing or is
    /// already relaunching.
    pub fn begin_relaunch(&self, skip: bool) -> Result<(), String> {
        let mut r = self.record.lock().unwrap();
        let mut l = self.life.lock().unwrap();
        l.check_relaunch()?;
        l.relaunching = true;
        r.pending_skip = Some(skip);
        Ok(())
    }

    /// Undo [`Terminal::begin_relaunch`] after a failed Relaunch: publish an exit held back
    /// in the meantime, exactly once, and drop the pending spec from the state file.
    pub fn abort_relaunch(self: &Arc<Self>, d: &Arc<Daemon>, reg: &mut Registry) {
        self.record.lock().unwrap().pending_skip = None;
        let held = {
            let mut l = self.life.lock().unwrap();
            l.relaunching = false;
            l.held_exit.take()
        };
        match held {
            Some(code) => self.finish_exit(d, reg, code),
            None => d.persist(reg),
        }
    }

    /// Name (or stop naming) the process a Relaunch started to replace this one's.
    pub fn set_replacement(&self, leader: Option<Leader>) {
        self.record.lock().unwrap().replacement = leader;
    }

    /// What a Relaunch restarts with: spec, metadata, creation time and the current size.
    pub fn relaunch_parts(&self) -> (LaunchSpec, Map<String, Value>, u64, (u16, u16)) {
        let r = self.record.lock().unwrap();
        let size = self.io.lock().unwrap().arb.current();
        (r.spec.clone(), r.meta.clone(), r.created_at_ms, size)
    }

    /// Move the attached connections and the size arbiter to `next`, which replaces this
    /// Terminal under its UUID, and send them `next`'s replay: a reset, then whatever `next`
    /// printed so far. Returns Terminals whose queued output was dropped.
    pub fn hand_over(&self, next: &Terminal) -> Vec<Uuid> {
        let (subs, arb) = {
            let mut io = self.io.lock().unwrap();
            let mut o = self.out.lock().unwrap();
            let (cols, rows) = io.arb.current();
            (
                std::mem::take(&mut o.subs),
                std::mem::replace(&mut io.arb, SizeArbiter::new(cols, rows)),
            )
        };
        let mut dropped = Vec::new();
        let mut io = next.io.lock().unwrap();
        io.arb = arb;
        let mut o = next.out.lock().unwrap();
        let snap = o.replay.snapshot();
        for (conn, ob) in subs {
            for piece in snap.chunks(REPLAY_CHUNK) {
                dropped.extend(ob.push_output(self.id, Arc::from(piece)));
            }
            o.subs.insert(conn, ob);
        }
        dropped
    }

    pub fn is_exited(&self) -> bool {
        self.life.lock().unwrap().exited.is_some()
    }

    pub fn wait_exited(&self, deadline: Instant) -> bool {
        let mut l = self.life.lock().unwrap();
        while l.exited.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            l = self.life_cv.wait_timeout(l, left).unwrap().0;
        }
        true
    }

    /// Subscribe `conn`: queue the reply, then the replay, atomically with respect to new
    /// output (both happen under the output lock the reader takes). For an exited Terminal,
    /// `term.exit` follows the replay. Returns Terminals whose queued output was dropped.
    pub fn attach(&self, conn: ConnId, ob: &Arc<Outbox>, id: Option<u64>) -> Vec<Uuid> {
        let mut dropped = Vec::new();
        let mut o = self.out.lock().unwrap();
        if let Some(id) = id {
            ob.push_control(Arc::from(encode_res(
                id,
                Ok(json!({ "exitCode": o.exit_code })),
            )));
        }
        let snap = o.replay.snapshot();
        for piece in snap.chunks(REPLAY_CHUNK) {
            dropped.extend(ob.push_output(self.id, Arc::from(piece)));
        }
        match o.exit_code {
            Some(code) => {
                if let Some(f) = frame(&ServerMsg::TermExit {
                    terminal: self.id,
                    code,
                }) {
                    ob.push_control(f);
                }
            }
            None => {
                o.subs.insert(conn, ob.clone());
            }
        }
        dropped
    }

    pub fn detach(&self, conn: ConnId) {
        self.io.lock().unwrap().arb.forget(conn);
        self.out.lock().unwrap().subs.remove(&conn);
    }

    fn apply_size(io: &TermIo, (cols, rows): (u16, u16)) -> Result<(), String> {
        let Some(master) = &io.master else {
            return Ok(());
        };
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("resize failed: {e}"))
    }

    /// Returns whether the PTY size changed.
    pub fn resize(&self, conn: ConnId, cols: u16, rows: u16) -> Result<bool, String> {
        let mut io = self.io.lock().unwrap();
        match io.arb.on_resize(conn, cols, rows) {
            Some(sz) => Self::apply_size(&io, sz).map(|_| true),
            None => Ok(false),
        }
    }

    /// `conn` typed: it takes over the size. Returns whether the PTY size changed.
    pub fn write_input(&self, conn: ConnId, data: String) -> Result<bool, String> {
        let changed = {
            let mut io = self.io.lock().unwrap();
            match io.arb.on_input(conn) {
                Some(sz) => Self::apply_size(&io, sz).is_ok(),
                None => false,
            }
        };
        let g = self.input.lock().unwrap();
        let tx = g.as_ref().ok_or("terminal has exited")?;
        match tx.try_send(data.into_bytes()) {
            Ok(()) => Ok(changed),
            Err(TrySendError::Full(_)) => Err("input backlog full".into()),
            Err(TrySendError::Disconnected(_)) => Err("terminal has exited".into()),
        }
    }

    /// Force full-screen TUIs to repaint: shrink by a row, then restore the current size.
    /// The gap keeps the two SIGWINCHs from merging into one.
    pub fn nudge(self: &Arc<Self>, delay: Duration) {
        if self.is_exited() || self.nudge_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let t = self.clone();
        let r = std::thread::Builder::new()
            .name(format!("pty-nudge-{}", short(&self.id)))
            .spawn(move || {
                {
                    let io = t.io.lock().unwrap();
                    let (cols, rows) = io.arb.current();
                    if rows > 1 {
                        let _ = Self::apply_size(&io, (cols, rows - 1));
                    }
                }
                std::thread::sleep(delay);
                {
                    // The current size, so a real resize during the gap wins.
                    let io = t.io.lock().unwrap();
                    let _ = Self::apply_size(&io, io.arb.current());
                }
                t.nudge_pending.store(false, Ordering::SeqCst);
            });
        if r.is_err() {
            self.nudge_pending.store(false, Ordering::SeqCst);
        }
    }

    pub fn overflow_nudge(self: &Arc<Self>, delay: Duration) {
        {
            let mut last = self.last_overflow_nudge.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < OVERFLOW_NUDGE_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        self.nudge(delay);
    }

    /// Persist the size after it settles, at most once per `delay`.
    pub fn schedule_persist(self: &Arc<Self>, d: &Arc<Daemon>) {
        if self.persist_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let (t, d) = (self.clone(), d.clone());
        let r = std::thread::Builder::new()
            .name(format!("persist-{}", short(&self.id)))
            .spawn(move || {
                std::thread::sleep(d.cfg.resize_persist_delay);
                t.persist_pending.store(false, Ordering::SeqCst);
                let reg = d.reg.lock().unwrap();
                if reg.terminals.contains_key(&t.id) {
                    d.persist(&reg);
                }
            });
        if r.is_err() {
            self.persist_pending.store(false, Ordering::SeqCst);
        }
    }

    /// End the Terminal: [`Terminal::signal_groups`], marked as closing so that its exit
    /// removes it from the list.
    pub fn kill(self: &Arc<Self>, grace: Duration) {
        self.hang_up(grace, true);
    }

    /// End the process: SIGHUP the session leader's group and the foreground job's group
    /// (job control puts an agent under `bash -i` in its own group), then SIGKILL each group
    /// that still exists after `grace`. A Terminal that already exited is never signalled.
    pub fn signal_groups(self: &Arc<Self>, grace: Duration) {
        self.hang_up(grace, false);
    }

    /// The process groups ending the Terminal signals: the session leader's and the
    /// foreground job's.
    fn groups(&self) -> Vec<i32> {
        let mut groups: Vec<i32> = self.pid.map(|p| p as i32).into_iter().collect();
        let fg = self
            .io
            .lock()
            .unwrap()
            .master
            .as_ref()
            .and_then(|m| m.process_group_leader());
        if let Some(g) = fg {
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
        groups
    }

    /// [`Terminal::kill`], then wait until the process has exited and every group it
    /// signalled is gone, or until `deadline`. Answers whether they are gone.
    pub fn end_and_wait(self: &Arc<Self>, grace: Duration, deadline: Instant) -> bool {
        let groups = self.groups();
        self.kill(grace);
        if !self.wait_exited(deadline) {
            return false;
        }
        loop {
            if groups.iter().all(|&g| unsafe { libc::killpg(g, 0) } != 0) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn hang_up(self: &Arc<Self>, grace: Duration, closing: bool) {
        let groups = self.groups();
        {
            let mut l = self.life.lock().unwrap();
            if l.exited.is_some() {
                return;
            }
            l.closing |= closing;
            for &g in &groups {
                unsafe { libc::killpg(g, libc::SIGHUP) };
            }
        }
        // Recorded until the timer ran, so an exit in between still sends the SIGKILL.
        let esc = self.escalations.clone();
        let entry = esc.add(groups.clone(), Instant::now() + grace);
        let r = std::thread::Builder::new()
            .name(format!("pty-kill-{}", short(&self.id)))
            .spawn(move || {
                std::thread::sleep(grace);
                for g in groups {
                    if unsafe { libc::killpg(g, 0) } == 0 {
                        unsafe { libc::killpg(g, libc::SIGKILL) };
                    }
                }
                esc.done(entry);
            });
        if let Err(e) = r {
            crate::log!("ERROR", "cannot start kill timer for {}: {e}", self.id);
        }
    }

    /// The spec and metadata a `term.update` would produce, without applying it.
    pub fn updated(
        &self,
        session_id: Option<String>,
        meta: Option<Map<String, Value>>,
    ) -> (LaunchSpec, Map<String, Value>) {
        let r = self.record.lock().unwrap();
        let (mut spec, mut m) = (r.spec.clone(), r.meta.clone());
        if let Some(s) = session_id {
            spec.session_id = Some(s);
        }
        for (k, v) in meta.into_iter().flatten() {
            if v.is_null() {
                m.remove(&k);
            } else {
                m.insert(k, v);
            }
        }
        (spec, m)
    }

    pub fn set_record(&self, spec: LaunchSpec, meta: Map<String, Value>) {
        let mut r = self.record.lock().unwrap();
        r.spec = spec;
        r.meta = meta;
    }

    /// Connections attached to the output.
    pub fn attached(&self) -> usize {
        self.out.lock().unwrap().subs.len()
    }

    /// Connections the size arbiter still remembers.
    pub fn size_tracked(&self) -> usize {
        self.io.lock().unwrap().arb.tracked()
    }

    /// This Terminal's share of the `terminals` list budget.
    pub fn entry_bytes(&self) -> usize {
        let r = self.record.lock().unwrap();
        entry_bytes(&r.spec, &r.meta)
    }

    pub fn info(&self) -> TerminalInfo {
        let r = self.record.lock().unwrap();
        let exit_code = self.out.lock().unwrap().exit_code;
        TerminalInfo {
            terminal: self.id,
            spec: r.spec.clone(),
            meta: r.meta.clone(),
            created_at_ms: r.created_at_ms,
            pid: self.pid,
            exit_code,
            agent_status: self.status.lock().unwrap().status(),
        }
    }

    pub fn persisted(&self) -> PersistedTerminal {
        let r = self.record.lock().unwrap();
        let io = self.io.lock().unwrap();
        let (cols, rows) = io.arb.current();
        // An exited Terminal has no process left to end; its pid may already be reused.
        let pid = self.pid.filter(|_| !self.is_exited());
        let leader = pid.map(|pid| {
            let mut groups = vec![ProcIdentity {
                pid: pid as i32,
                start_time: self.start_time,
            }];
            if let Some(g) = io.master.as_ref().and_then(|m| m.process_group_leader()) {
                if g != pid as i32 {
                    groups.push(super::orphans::identity(g));
                }
            }
            Leader {
                pid,
                start_time: self.start_time,
                groups,
            }
        });
        let spec = r
            .pending_skip
            .and_then(|skip| relaunch_spec(&r.spec, skip).ok())
            .unwrap_or_else(|| r.spec.clone());
        PersistedTerminal {
            terminal: self.id,
            spec,
            meta: r.meta.clone(),
            cols,
            rows,
            created_at_ms: r.created_at_ms,
            leader: leader
                .or_else(|| r.replacement.clone())
                .or_else(|| self.kept_leader.clone()),
        }
    }
}

/// Exit code listed for a Terminal that was not relaunched because leftovers of its previous
/// run could not be ended or confirmed gone.
pub(crate) const UNRESOLVED_EXIT: i32 = -1;

/// A restored Terminal without a process: listed as exited (`UNRESOLVED_EXIT`), its record
/// and previous leader kept in the state file until `term.close` removes it.
pub(crate) fn unresolved(d: &Arc<Daemon>, p: PersistedTerminal) -> Arc<Terminal> {
    let mut status = Tracker::new(&p.spec);
    status.on_exit();
    let t = Terminal {
        id: p.terminal,
        record: Mutex::new(Record {
            spec: p.spec,
            meta: p.meta,
            created_at_ms: p.created_at_ms,
            pending_skip: None,
            replacement: None,
        }),
        io: Mutex::new(TermIo {
            master: None,
            arb: SizeArbiter::new(p.cols, p.rows),
        }),
        out: Mutex::new(TermOutput {
            replay: ReplayBuffer::new(d.cfg.replay_capacity),
            subs: HashMap::new(),
            exit_code: Some(UNRESOLVED_EXIT),
        }),
        input: Mutex::new(None),
        pid: None,
        start_time: None,
        life: Mutex::new(Life {
            exited: Some(UNRESOLVED_EXIT),
            reader_done: true,
            ..Life::default()
        }),
        life_cv: Condvar::new(),
        nudge_pending: AtomicBool::new(false),
        last_overflow_nudge: Mutex::new(None),
        persist_pending: AtomicBool::new(false),
        kept_leader: p.leader,
        run: d.next_run.fetch_add(1, Ordering::SeqCst),
        status: Mutex::new(status),
        escalations: d.escalations.clone(),
    };
    Arc::new(t)
}

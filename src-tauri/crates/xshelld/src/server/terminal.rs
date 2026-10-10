//! One Terminal: a PTY process plus its reader, waiter and input threads, its replay buffer
//! and the connections attached to it.

use super::outbox::Outbox;
use super::registry::{frame, now_ms, overflowed, Daemon, Overflow, Registry};
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
use xshell_protocol::msg::{encode_res, LastLine, ServerMsg, TerminalInfo};

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
    /// The most a Mobile is replayed (`Config::mobile_replay_cap`).
    mobile_replay_cap: usize,
    /// For a Terminal restored without a process: the previous run's leader, kept in the
    /// state file so a later start retries ending it.
    kept_leader: Option<Leader>,
    /// This process's run: unique per Daemon start and process, so a hook of a process a
    /// Relaunch or restart replaced never reports for its successor.
    pub run: u64,
    /// The Agent Status of this run and when it changed. Locked last, never across another
    /// lock.
    status: Mutex<StatusCell>,
    /// The newest text message of the agent's session, as the last-line worker last read
    /// it. Locked last, never across another lock.
    last_line: Mutex<Option<LastLine>>,
    /// The Daemon's pending SIGKILLs, which [`Terminal::kill`] adds to.
    escalations: Arc<super::orphans::Escalations>,
    /// Windows: the kill-on-close Job Object the process runs in, with everything it
    /// starts. Closing it (the Terminal and its escalation dropped) ends them all.
    #[cfg(windows)]
    job: Option<Arc<xshell_core::job::Job>>,
}

/// A run's Agent Status and the time of its last change.
pub(crate) struct StatusCell {
    tracker: Tracker,
    /// When the status last changed (Unix ms); `None` before the first change.
    at_ms: Option<u64>,
    /// Every stamp is above this: the last stamp of the run this one replaced.
    floor: u64,
}

impl StatusCell {
    pub(crate) fn new(tracker: Tracker, floor: u64) -> Self {
        Self {
            tracker,
            at_ms: None,
            floor,
        }
    }

    /// Pass on a [`Tracker`] method's result; a change is stamped with `now`, or just after
    /// the previous stamp when the clock has not moved past it (equal or set back), so the
    /// stamps of one Terminal strictly increase.
    pub(crate) fn stamp(&mut self, changed: bool, now: u64) -> bool {
        if changed {
            self.at_ms = Some(now.max(self.last_stamp() + 1));
        }
        changed
    }

    /// Continue after `floor` (the replaced run's last stamp): a stamp made before, with a
    /// clock below it, moves to just after it.
    pub(crate) fn raise_floor(&mut self, floor: u64) {
        self.floor = self.floor.max(floor);
        if self.at_ms.is_some_and(|at| at <= floor) {
            self.at_ms = Some(floor + 1);
        }
    }

    /// The latest stamp this Terminal has had, the replaced run's included.
    pub(crate) fn last_stamp(&self) -> u64 {
        self.at_ms.unwrap_or(0).max(self.floor)
    }

    /// The status and its stamp, as listed.
    pub(crate) fn listed(&self) -> (Option<AgentStatus>, Option<u64>) {
        let status = self.tracker.status();
        (status, status.and(self.at_ms))
    }
}

/// Fixed per-entry cost in a `terminals` list on top of the spec and metadata (UUID, pid,
/// exit code, timestamps, keys).
const ENTRY_OVERHEAD: usize = 256;

/// Reserved per entry for the fields the Daemon fills in after admission, at their largest
/// serialized: `agentStatus`, `statusAtMs` and a `lastLine` of [`LAST_LINE_MAX_CHARS`]
/// four-byte characters (control characters never reach it; `"` and `\` escape to two bytes).
/// Counted in every entry's budget, so filling them never grows a list past its limit.
///
/// [`LAST_LINE_MAX_CHARS`]: xshell_protocol::msg::LAST_LINE_MAX_CHARS
pub const OPTIONAL_FIELDS_BYTES: usize = 1024;

/// The size a Terminal with this spec and metadata adds to a serialized `terminals` list.
pub(crate) fn entry_bytes(spec: &LaunchSpec, meta: &Map<String, Value>) -> usize {
    let len = |v: serde_json::Result<Vec<u8>>| v.map_or(usize::MAX / 4, |b| b.len());
    len(serde_json::to_vec(spec))
        + len(serde_json::to_vec(meta))
        + ENTRY_OVERHEAD
        + OPTIONAL_FIELDS_BYTES
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
    spawn_with(d, id, spec, meta, (cols, rows), created_at_ms, None, |_| {}).map_err(|e| e.message)
}

/// [`spawn`], calling `spawned` with the new process's identity as soon as it exists, before
/// any of its threads start. `first` is a new chat's first message (`term.open` only): it
/// goes to this launch's argv and nowhere else, so a restore or Relaunch never sends it again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_with(
    d: &Arc<Daemon>,
    id: Uuid,
    spec: LaunchSpec,
    meta: Map<String, Value>,
    (cols, rows): (u16, u16),
    created_at_ms: u64,
    first: Option<&str>,
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
    let plan = xshell_core::plan_command_first(&d.ctx, &spec, hooks, first)?;
    let tracker = Tracker::new(&spec);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("failed to open PTY: {e}"))?;
    // Windows: until the Terminal exists, a failure hands the console to a cleanup thread
    // that drains it (closing it may wait for that) and ends whatever started, so nothing
    // here blocks under the registry lock.
    #[cfg(windows)]
    let mut pair = win::SpawnGuard::new(pair, id);
    #[cfg(unix)]
    let cmd = plan.to_command_builder();
    #[cfg(windows)]
    let (job, cmd) = {
        let name = format!(
            r"Local\xshelld-term-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        );
        let job = xshell_core::job::Job::new_named(&name)
            .map_err(|e| format!("failed to create the terminal's job: {e}"))?;
        let job = Arc::new(job);
        pair.job = Some(job.clone());
        let launcher = d.cfg.job_launcher.as_deref();
        (job, win::command(&plan, launcher, &name))
    };
    #[cfg(unix)]
    let spawned_child = pair.slave.spawn_command(cmd);
    #[cfg(windows)]
    let spawned_child = pair.slave().spawn_command(cmd);
    let mut child = spawned_child.map_err(|e| format!("failed to start {}: {e}", plan.program))?;
    // Without the launcher (an in-process server), the job is assigned from here: a process
    // the child starts before that escapes it.
    #[cfg(windows)]
    if d.cfg.job_launcher.is_none() {
        let assigned = child
            .as_raw_handle()
            .ok_or_else(|| std::io::Error::other("no process handle"))
            .and_then(|h| job.assign(h));
        if let Err(e) = assigned {
            let _ = child.kill();
            return Err(format!("failed to put {} in its job: {e}", plan.program).into());
        }
    }
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
    #[cfg(unix)]
    drop(pair.slave);
    #[cfg(windows)]
    pair.release_slave();
    #[cfg(unix)]
    let pty_master = &pair.master;
    #[cfg(windows)]
    let pty_master = pair.master();
    let pty = pty_master
        .try_clone_reader()
        .map_err(|e| format!("failed to clone PTY reader: {e}"))
        .and_then(|r| {
            pty_master
                .take_writer()
                .map(|w| (r, w))
                .map_err(|e| format!("failed to take PTY writer: {e}"))
        });
    let (reader, writer) = pty?;
    #[cfg(windows)]
    let master = pair.into_master();
    #[cfg(unix)]
    let master = pair.master;
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
            master: Some(master),
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
        mobile_replay_cap: d.cfg.mobile_replay_cap,
        kept_leader: None,
        run,
        status: Mutex::new(StatusCell::new(tracker, 0)),
        last_line: Mutex::new(None),
        escalations: d.escalations.clone(),
        #[cfg(windows)]
        job: Some(job),
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
            // A ConPTY's output never ends while the console lives: close it now, while the
            // reader drains what is left. Whatever the process left in its job (a detached
            // descendant) ends after the grace, listed Terminal or not.
            #[cfg(windows)]
            {
                if let Some(job) = &tw.job {
                    tw.kill_after(vec![job.clone()], dw.cfg.kill_grace);
                }
                let master = tw.io.lock().unwrap().master.take();
                drop(master);
            }
            // `term.exit` follows the last output: wait for the reader to see EOF. Windows:
            // bounded, in case the closed console never ends its output.
            #[cfg(windows)]
            let drained_by = Some(Instant::now() + win::DRAIN_WAIT);
            #[cfg(not(windows))]
            let drained_by: Option<Instant> = None;
            let mut l = tw.life.lock().unwrap();
            while !l.reader_done {
                match drained_by {
                    None => l = tw.life_cv.wait(l).unwrap(),
                    Some(by) => {
                        let left = by.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            crate::log!("WARN", "output of terminal {} never ended", tw.id);
                            break;
                        }
                        l = tw.life_cv.wait_timeout(l, left).unwrap().0;
                    }
                }
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
        #[cfg(windows)]
        let mut startup = win::StartupQuery::default();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                #[cfg(windows)]
                Ok(n) => {
                    let (out, answer) = startup.feed(&buf[..n]);
                    if answer {
                        if let Some(tx) = self.input.lock().unwrap().as_ref() {
                            let _ = tx.try_send(win::CURSOR_AT_ORIGIN.to_vec());
                        }
                    }
                    if !out.is_empty() {
                        self.on_output(d, &out);
                    }
                }
                #[cfg(not(windows))]
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
                dropped.extend(overflowed(ob, ob.push_output(self.id, chunk.clone())));
            }
        }
        d.nudge_overflowed(dropped);
        let changed = {
            let mut c = self.status.lock().unwrap();
            let changed = c.tracker.on_output(bytes);
            c.stamp(changed, now_ms())
        };
        if changed {
            super::agent::changed(d, self);
            // Codex's OSC 9 notification: the agent itself says it needs you.
            d.push.notify(self.id, self.run, AgentStatus::NeedsYou);
        }
    }

    /// Start this run's Agent Status afresh, as a running process would (tests only: a
    /// Terminal restored without a process has ended).
    #[cfg(test)]
    pub(crate) fn reset_status_for_test(&self) {
        let tracker = Tracker::new(&self.spec());
        *self.status.lock().unwrap() = StatusCell::new(tracker, 0);
    }

    /// The Agent Status of this run.
    pub fn agent_status(&self) -> Option<AgentStatus> {
        self.status.lock().unwrap().tracker.status()
    }

    /// A hook's report for process `run`.
    pub fn on_agent_event(&self, run: u64, status: AgentStatus) -> Result<bool, String> {
        let mut c = self.status.lock().unwrap();
        if c.tracker.agent().is_none() {
            return Err("not an agent terminal".into());
        }
        if run != self.run {
            return Err("stale run".into());
        }
        let changed = c.tracker.on_event(status)?;
        Ok(c.stamp(changed, now_ms()))
    }

    /// Input about to be written: it may answer a prompt or interrupt a turn.
    pub fn note_input(&self, d: &Daemon, data: &[u8]) {
        let changed = {
            let mut c = self.status.lock().unwrap();
            let changed = c.tracker.on_input(data);
            c.stamp(changed, now_ms())
        };
        if changed {
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
        {
            let mut c = self.status.lock().unwrap();
            let changed = c.tracker.on_exit();
            c.stamp(changed, now_ms());
        }
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
            d.broadcast_about(reg, self.id, &self.spec(), f);
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
    /// printed so far (a Mobile: its tail). A size this Terminal took after `next` started
    /// is applied to `next`. Returns the overflows to recover.
    pub fn hand_over(&self, next: &Terminal) -> Vec<Overflow> {
        let (subs, arb) = {
            let mut io = self.io.lock().unwrap();
            let mut o = self.out.lock().unwrap();
            let (cols, rows) = io.arb.current();
            (
                std::mem::take(&mut o.subs),
                std::mem::replace(&mut io.arb, SizeArbiter::new(cols, rows)),
            )
        };
        // The replacement resumes the same session, and its stamps continue after this run's,
        // also one it made already (its reader runs before the hand-over).
        let floor = self.status.lock().unwrap().last_stamp();
        next.status.lock().unwrap().raise_floor(floor);
        let line = self.last_line.lock().unwrap().clone();
        *next.last_line.lock().unwrap() = line;
        let mut dropped = Vec::new();
        let mut io = next.io.lock().unwrap();
        let spawned = io.arb.current();
        io.arb = arb;
        let size = io.arb.current();
        let mut o = next.out.lock().unwrap();
        let (full, tail) = (
            o.replay.snapshot(),
            o.replay.snapshot_tail(next.mobile_replay_cap),
        );
        for (conn, ob) in subs {
            let snap = if ob.is_mobile() { &tail } else { &full };
            for piece in snap.chunks(REPLAY_CHUNK) {
                dropped.extend(overflowed(&ob, ob.push_replay(self.id, Arc::from(piece))));
            }
            o.subs.insert(conn, ob);
        }
        // A size taken while `next` was starting (it started at the size read then).
        if size != spawned && Self::apply_size(&io, size).is_ok() {
            next.notify_size(&o, size);
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

    /// Subscribe `conn`: queue the reply (`{exitCode, cols, rows}`), then the replay (a
    /// Mobile's is a shorter tail), atomically with respect to new output and size changes
    /// (both happen under the size and output locks). For an exited Terminal, `term.exit`
    /// follows the replay. Returns the overflows to recover, and whether no other connection
    /// is attached.
    pub fn attach(&self, conn: ConnId, ob: &Arc<Outbox>, id: Option<u64>) -> (Vec<Overflow>, bool) {
        let mut dropped = Vec::new();
        let io = self.io.lock().unwrap();
        let mut o = self.out.lock().unwrap();
        if let Some(id) = id {
            let (cols, rows) = io.arb.current();
            let res = json!({ "exitCode": o.exit_code, "cols": cols, "rows": rows });
            ob.push_about(self.id, Arc::from(encode_res(id, Ok(res))));
        }
        let snap = if ob.is_mobile() {
            o.replay.snapshot_tail(self.mobile_replay_cap)
        } else {
            o.replay.snapshot()
        };
        for piece in snap.chunks(REPLAY_CHUNK) {
            dropped.extend(overflowed(ob, ob.push_replay(self.id, Arc::from(piece))));
        }
        let alone = o.subs.keys().all(|c| *c == conn);
        match o.exit_code {
            Some(code) => {
                if let Some(f) = frame(&ServerMsg::TermExit {
                    terminal: self.id,
                    code,
                }) {
                    ob.push_about(self.id, f);
                }
            }
            None => {
                o.subs.insert(conn, ob.clone());
            }
        }
        (dropped, alone)
    }

    /// Unsubscribe `conn` and forget its size. A Mobile's output still queued is dropped.
    /// Returns whether the size changed (an owning Mobile left; see [`SizeArbiter::forget`]).
    pub fn detach(&self, conn: ConnId) -> bool {
        let mut io = self.io.lock().unwrap();
        let mut o = self.out.lock().unwrap();
        if let Some(ob) = o.subs.remove(&conn) {
            if ob.is_mobile() {
                ob.cancel_output(self.id);
            }
        }
        match io.arb.forget(conn) {
            Some(sz) if Self::apply_size(&io, sz).is_ok() => {
                self.notify_size(&o, sz);
                true
            }
            _ => false,
        }
    }

    /// Tell the attached Mobiles the size now applied.
    fn notify_size(&self, o: &TermOutput, (cols, rows): (u16, u16)) {
        let mut mobiles = o.subs.values().filter(|ob| ob.is_mobile()).peekable();
        if mobiles.peek().is_none() {
            return;
        }
        if let Some(f) = frame(&ServerMsg::TermSize {
            terminal: self.id,
            cols,
            rows,
        }) {
            for ob in mobiles {
                ob.push_size(self.id, f.clone());
            }
        }
    }

    /// Recover a Mobile connection's dropped output: the redraw nudge when nobody else is
    /// attached, else (a nudge would reflow everyone's screen) a fresh tail of the replay in
    /// place of what is still queued.
    pub fn recover_mobile_overflow(self: &Arc<Self>, ob: &Arc<Outbox>, delay: Duration) {
        let more = {
            let o = self.out.lock().unwrap();
            if !o.subs.values().any(|s| Arc::ptr_eq(s, ob)) {
                return;
            }
            if o.subs.len() > 1 {
                ob.drop_output(self.id);
                let tail = o.replay.snapshot_tail(self.mobile_replay_cap);
                let mut more = Vec::new();
                for piece in tail.chunks(REPLAY_CHUNK) {
                    more.extend(ob.push_output(self.id, Arc::from(piece)));
                }
                Some(more)
            } else {
                None
            }
        };
        match more {
            None => self.overflow_nudge(delay),
            Some(more) if !more.is_empty() => {
                crate::log!("WARN", "a Mobile's send queue overflowed again on recovery")
            }
            Some(_) => {}
        }
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

    /// `conn` (a Mobile when `mobile`) resized its view; see [`SizeArbiter::on_resize`].
    /// Returns whether the PTY size changed.
    pub fn resize(&self, conn: ConnId, cols: u16, rows: u16, mobile: bool) -> Result<bool, String> {
        let mut io = self.io.lock().unwrap();
        match io.arb.on_resize(conn, cols, rows, mobile) {
            Some(sz) => {
                Self::apply_size(&io, sz)?;
                self.notify_size(&self.out.lock().unwrap(), sz);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// `conn` (a Mobile when `mobile`) typed: it takes over the size. Returns whether the PTY
    /// size changed.
    pub fn write_input(&self, conn: ConnId, data: String, mobile: bool) -> Result<bool, String> {
        let changed = {
            let mut io = self.io.lock().unwrap();
            match io.arb.on_input(conn, mobile) {
                Some(sz) if Self::apply_size(&io, sz).is_ok() => {
                    self.notify_size(&self.out.lock().unwrap(), sz);
                    true
                }
                _ => false,
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
    #[cfg(unix)]
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

    /// What ending the Terminal kills after the grace: its process groups (Windows: its job).
    #[cfg(unix)]
    fn kill_groups(&self) -> Vec<super::orphans::Group> {
        self.groups()
    }

    #[cfg(windows)]
    fn kill_groups(&self) -> Vec<super::orphans::Group> {
        self.job.iter().cloned().collect()
    }

    /// [`Terminal::kill`], then wait until the process has exited and every group it
    /// signalled is gone, or until `deadline`. Answers whether they are gone.
    pub fn end_and_wait(self: &Arc<Self>, grace: Duration, deadline: Instant) -> bool {
        let groups = self.kill_groups();
        self.kill(grace);
        if !self.wait_exited(deadline) {
            return false;
        }
        loop {
            if !groups.iter().any(super::orphans::group_alive) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
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
        self.kill_after(groups, grace);
    }

    /// Windows: closing the ConPTY is the hangup (every process attached to it gets
    /// `CTRL_CLOSE_EVENT`); after `grace` the Terminal's job ends whatever is left of its
    /// tree, detached processes included.
    #[cfg(windows)]
    fn hang_up(self: &Arc<Self>, grace: Duration, closing: bool) {
        {
            let mut l = self.life.lock().unwrap();
            if l.exited.is_some() {
                return;
            }
            l.closing |= closing;
        }
        // Closing may wait for the console's output to drain: never under a lock.
        let master = self.io.lock().unwrap().master.take();
        if let Some(m) = master {
            win::close_console(m, &self.id);
        }
        if let Some(job) = &self.job {
            self.kill_after(vec![job.clone()], grace);
        }
    }

    /// Kill what is left of `groups` after `grace`. Recorded until the timer ran, so an exit
    /// in between, or the Daemon's own exit, still kills them.
    fn kill_after(&self, groups: Vec<super::orphans::Group>, grace: Duration) {
        let esc = self.escalations.clone();
        let entry = esc.add(groups.clone(), Instant::now() + grace);
        let r = std::thread::Builder::new()
            .name(format!("pty-kill-{}", short(&self.id)))
            .spawn(move || {
                std::thread::sleep(grace);
                super::orphans::kill_remaining(&groups);
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

    /// Store the last line the worker read; `true` when it changed.
    pub fn set_last_line(&self, line: Option<LastLine>) -> bool {
        let mut l = self.last_line.lock().unwrap();
        if *l == line {
            return false;
        }
        *l = line;
        true
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
        let (agent_status, status_at_ms) = self.status.lock().unwrap().listed();
        TerminalInfo {
            terminal: self.id,
            spec: r.spec.clone(),
            meta: r.meta.clone(),
            created_at_ms: r.created_at_ms,
            pid: self.pid,
            exit_code,
            agent_status,
            status_at_ms,
            last_line: self.last_line.lock().unwrap().clone(),
        }
    }

    pub fn persisted(&self) -> PersistedTerminal {
        let r = self.record.lock().unwrap();
        let io = self.io.lock().unwrap();
        let (cols, rows) = io.arb.current();
        // An exited Terminal has no process left to end; its pid may already be reused.
        let pid = self.pid.filter(|_| !self.is_exited());
        let leader = pid.map(|pid| {
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut groups = vec![ProcIdentity {
                pid: pid as i32,
                start_time: self.start_time,
            }];
            #[cfg(unix)]
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
    let mut status = StatusCell::new(Tracker::new(&p.spec), 0);
    let ended = status.tracker.on_exit();
    status.stamp(ended, now_ms());
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
        mobile_replay_cap: d.cfg.mobile_replay_cap,
        kept_leader: p.leader,
        run: d.next_run.fetch_add(1, Ordering::SeqCst),
        status: Mutex::new(status),
        last_line: Mutex::new(None),
        escalations: d.escalations.clone(),
        #[cfg(windows)]
        job: None,
    };
    Arc::new(t)
}

#[cfg(windows)]
mod win {
    use portable_pty::{CommandBuilder, MasterPty, PtyPair, SlavePty};
    use std::io::{Read, Write};
    use std::sync::Arc;
    use xshell_core::job::Job;

    /// How long a Terminal's output may go on after its process exited and its console was
    /// closed.
    pub(super) const DRAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    /// What a ConPTY created to inherit the cursor (as portable-pty creates it) prints first.
    const CURSOR_QUERY: &[u8] = b"\x1b[6n";
    /// The answer: the cursor is at the top left.
    pub(super) const CURSOR_AT_ORIGIN: &[u8] = b"\x1b[1;1R";

    /// The ConPTY's startup cursor query. It renders nothing until the query is answered, and
    /// a Terminal may have no Desktop attached (a restore), so the Daemon answers it and keeps
    /// it out of the output: a Desktop replaying the output must not answer it again.
    #[derive(Default)]
    pub(super) struct StartupQuery {
        head: Vec<u8>,
        done: bool,
    }

    impl StartupQuery {
        /// The output to pass on, and whether the query was seen (answer it now).
        pub(super) fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, bool) {
            if self.done {
                return (bytes.to_vec(), false);
            }
            self.head.extend_from_slice(bytes);
            if self.head.starts_with(CURSOR_QUERY) {
                self.done = true;
                return (self.head.split_off(CURSOR_QUERY.len()), true);
            }
            if CURSOR_QUERY.starts_with(&self.head) {
                return (Vec::new(), false);
            }
            self.done = true;
            (std::mem::take(&mut self.head), false)
        }
    }

    use std::path::Path;
    use uuid::Uuid;
    use xshell_core::CommandPlan;

    /// The Terminal's command. With a `launcher` (this `xshelld`), the process is
    /// `xshelld job-exec <job> -- <program> <args…>`: it joins the job itself before it starts
    /// the program, so nothing the program starts escapes the job.
    ///
    /// `CommandBuilder::new` reloads the registry's environment over this process's: ours is
    /// put back, PATH included (followed by registry entries it lacks, such as a tool
    /// installed since the Desktop started), then the plan's variables.
    pub(super) fn command(
        plan: &CommandPlan,
        launcher: Option<&Path>,
        job: &str,
    ) -> CommandBuilder {
        let mut cmd = match launcher {
            Some(exe) => {
                let mut c = CommandBuilder::new(exe);
                c.args(["job-exec", job, "--"]);
                c.arg(&plan.program);
                c
            }
            None => CommandBuilder::new(&plan.program),
        };
        for a in &plan.args {
            cmd.arg(a);
        }
        let registry_path = cmd
            .get_env("PATH")
            .map(|p| p.to_string_lossy().into_owned());
        for (k, v) in std::env::vars_os() {
            cmd.env(k, v);
        }
        if let (Ok(ours), Some(reg)) = (std::env::var("PATH"), registry_path) {
            let mut all: Vec<&str> = ours.split(';').filter(|e| !e.is_empty()).collect();
            for e in reg.split(';').filter(|e| !e.is_empty()) {
                if !all.iter().any(|a| a.eq_ignore_ascii_case(e)) {
                    all.push(e);
                }
            }
            cmd.env("PATH", all.join(";"));
        }
        for (k, v) in &plan.env {
            cmd.env(k, v);
        }
        cmd.cwd(&plan.cwd);
        cmd
    }

    /// A ConPTY and the Terminal's job while the Terminal is being started. Dropped before
    /// [`SpawnGuard::into_master`] (a failed start), it ends the job's processes and hands
    /// the console to [`abandon_console`].
    pub(super) struct SpawnGuard {
        master: Option<Box<dyn MasterPty + Send>>,
        slave: Option<Box<dyn SlavePty + Send>>,
        pub(super) job: Option<Arc<Job>>,
        id: Uuid,
    }

    impl SpawnGuard {
        pub(super) fn new(pair: PtyPair, id: Uuid) -> SpawnGuard {
            SpawnGuard {
                master: Some(pair.master),
                slave: Some(pair.slave),
                job: None,
                id,
            }
        }

        pub(super) fn master(&self) -> &(dyn MasterPty + Send) {
            self.master.as_deref().expect("held until handed on")
        }

        pub(super) fn slave(&self) -> &(dyn SlavePty + Send) {
            self.slave.as_deref().expect("held until released")
        }

        /// Drop the slave (the started process holds the console now). It shares the
        /// console with the master, so this closes nothing yet.
        pub(super) fn release_slave(&mut self) {
            self.slave.take();
        }

        /// The start succeeded: the Terminal owns the console from now on.
        pub(super) fn into_master(mut self) -> Box<dyn MasterPty + Send> {
            self.master.take().expect("held until handed on")
        }
    }

    impl Drop for SpawnGuard {
        fn drop(&mut self) {
            let Some(master) = self.master.take() else {
                return;
            };
            if let Some(job) = &self.job {
                let _ = job.terminate(1);
            }
            abandon_console(master, self.slave.take(), self.id);
        }
    }

    /// Close a console nobody reads, off this thread: its output is drained (and its startup
    /// cursor query answered) so the close can complete.
    pub(super) fn abandon_console(
        master: Box<dyn MasterPty + Send>,
        slave: Option<Box<dyn SlavePty + Send>>,
        id: Uuid,
    ) {
        let r = std::thread::Builder::new()
            .name(format!("pty-abandon-{}", &id.simple().to_string()[..8]))
            .spawn(move || {
                let reader = master.try_clone_reader().ok();
                let mut writer = master.take_writer().ok();
                if let Some(mut reader) = reader {
                    let _ = std::thread::Builder::new()
                        .name("pty-drain".into())
                        .spawn(move || {
                            let mut q = StartupQuery::default();
                            let mut buf = [0u8; 4096];
                            while let Ok(n) = reader.read(&mut buf) {
                                if n == 0 {
                                    break;
                                }
                                if q.feed(&buf[..n]).1 {
                                    if let Some(w) = writer.as_mut() {
                                        let _ = w.write_all(CURSOR_AT_ORIGIN);
                                    }
                                }
                            }
                        });
                }
                drop(slave);
                drop(master);
            });
        if let Err(e) = r {
            crate::log!("ERROR", "cannot close the console of {id} off-thread: {e}");
        }
    }

    /// Close a ConPTY on a thread of its own: `ClosePseudoConsole` may wait until the
    /// console's output is drained.
    pub(super) fn close_console(m: Box<dyn MasterPty + Send>, id: &Uuid) {
        let r = std::thread::Builder::new()
            .name(format!("pty-close-{}", &id.simple().to_string()[..8]))
            .spawn(move || drop(m));
        if let Err(e) = r {
            crate::log!("ERROR", "cannot close the console of {id} off-thread: {e}");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn startup_query_is_answered_and_dropped() {
            let mut q = StartupQuery::default();
            assert_eq!(q.feed(b"\x1b["), (vec![], false));
            assert_eq!(q.feed(b"6nhello"), (b"hello".to_vec(), true));
            assert_eq!(q.feed(b"\x1b[6n"), (b"\x1b[6n".to_vec(), false));
            let mut q = StartupQuery::default();
            assert_eq!(q.feed(b"\x1b[2J"), (b"\x1b[2J".to_vec(), false));
            assert_eq!(q.feed(b"\x1b[6n"), (b"\x1b[6n".to_vec(), false));
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    fn cell(floor: u64) -> StatusCell {
        StatusCell::new(
            Tracker::new(&LaunchSpec {
                agent: Some("claude".into()),
                ..Default::default()
            }),
            floor,
        )
    }

    fn event(c: &mut StatusCell, s: AgentStatus, now: u64) -> bool {
        let changed = c.tracker.on_event(s).unwrap();
        c.stamp(changed, now)
    }

    #[test]
    fn optional_fields_fit_their_reserve() {
        use xshell_protocol::msg::{Speaker, LAST_LINE_MAX_CHARS};
        let bare = TerminalInfo {
            terminal: Uuid::new_v4(),
            spec: LaunchSpec::default(),
            meta: Map::new(),
            created_at_ms: u64::MAX,
            pid: Some(u32::MAX),
            exit_code: Some(i32::MIN),
            agent_status: None,
            status_at_ms: None,
            last_line: None,
        };
        let size = |i: &TerminalInfo| serde_json::to_vec(i).unwrap().len();
        // The widest character in JSON: four UTF-8 bytes, or two for an escaped `"`.
        for widest in ["\u{1D11E}", "\"", "\\"] {
            let text =
                xshell_core::last_line::normalize(&widest.repeat(LAST_LINE_MAX_CHARS * 2)).unwrap();
            let full = TerminalInfo {
                agent_status: Some(AgentStatus::NeedsYou),
                status_at_ms: Some(u64::MAX),
                last_line: Some(LastLine {
                    from: Speaker::Agent,
                    text,
                }),
                ..bare.clone()
            };
            let extra = size(&full) - size(&bare);
            assert!(extra <= OPTIONAL_FIELDS_BYTES, "{widest:?}: {extra}");
        }
        // The fixed fields of the bare entry fit the per-entry overhead.
        let fixed = size(&bare)
            - serde_json::to_vec(&bare.spec).unwrap().len()
            - serde_json::to_vec(&bare.meta).unwrap().len();
        assert!(fixed <= ENTRY_OVERHEAD, "{fixed}");
    }

    #[test]
    fn status_stamps_strictly_increase() {
        let mut c = cell(0);
        assert_eq!(c.listed(), (None, None));
        assert!(event(&mut c, AgentStatus::Working, 1000));
        assert_eq!(c.listed(), (Some(AgentStatus::Working), Some(1000)));
        // The same status again: no change, no new stamp.
        assert!(!event(&mut c, AgentStatus::Working, 2000));
        assert_eq!(c.listed().1, Some(1000));
        // The clock did not move: just after the previous stamp.
        assert!(event(&mut c, AgentStatus::Finished, 1000));
        assert_eq!(c.listed(), (Some(AgentStatus::Finished), Some(1001)));
        // The clock went back.
        assert!(event(&mut c, AgentStatus::Working, 10));
        assert_eq!(c.listed().1, Some(1002));
        // And forward again.
        assert!(event(&mut c, AgentStatus::NeedsYou, 5000));
        assert_eq!(c.listed().1, Some(5000));
        assert_eq!(c.last_stamp(), 5000);
    }

    #[test]
    fn hand_over_moves_an_early_replacement_stamp_after_the_previous_run() {
        let dir = tempfile::tempdir().unwrap();
        let d = super::super::role::tests::daemon(dir.path());
        let persisted = || PersistedTerminal {
            terminal: Uuid::new_v4(),
            spec: LaunchSpec {
                agent: Some("codex".into()),
                ..Default::default()
            },
            meta: Map::new(),
            cols: 80,
            rows: 24,
            created_at_ms: 0,
            leader: None,
        };
        let prev = unresolved(&d, persisted());
        let next = unresolved(&d, persisted());
        // The previous run stamped at 5000; the replacement's reader stamped an OSC 9 at
        // 100 (a clock set back), before the hand-over.
        prev.status.lock().unwrap().at_ms = Some(5000);
        {
            let mut c = next.status.lock().unwrap();
            c.at_ms = None;
            c.tracker = Tracker::new(&next.spec());
            let changed = c.tracker.on_output(b"\x1b]9;Approval requested\x07");
            assert!(c.stamp(changed, 100));
            assert_eq!(c.listed().1, Some(100));
        }
        prev.hand_over(&next);
        assert_eq!(
            next.info().status_at_ms,
            Some(5001),
            "listed after the previous run's stamp"
        );
        // Later stamps continue from there, the clock still behind.
        let mut c = next.status.lock().unwrap();
        let changed = c.tracker.on_event(AgentStatus::Working).unwrap();
        assert!(c.stamp(changed, 200));
        assert_eq!(c.listed().1, Some(5002));
        drop(c);
        // A replacement stamped after the floor keeps its stamp.
        let mut c = cell(0);
        assert!(event(&mut c, AgentStatus::Working, 9000));
        c.raise_floor(5000);
        assert_eq!(c.listed().1, Some(9000));
    }

    /// A Relaunch's replacement starts at the size read before it started; a size taken on
    /// the old Terminal after that (late input) is applied to it at the hand-over, and the
    /// attached Mobiles are told. The replacement is a real process: its PTY's size is read
    /// back.
    #[cfg(unix)]
    #[test]
    fn hand_over_applies_a_size_taken_while_the_replacement_started() {
        use super::super::outbox::{Out, PaceCfg};
        let dir = tempfile::tempdir().unwrap();
        let d = super::super::role::tests::daemon(dir.path());
        let id = Uuid::new_v4();
        let persisted = || PersistedTerminal {
            terminal: id,
            spec: LaunchSpec {
                agent: Some("claude".into()),
                ..Default::default()
            },
            meta: Map::new(),
            cols: 80,
            rows: 24,
            created_at_ms: 0,
            leader: None,
        };
        let prev = unresolved(&d, persisted());
        let prog = dir.path().join("quiet");
        std::fs::write(&prog, "#!/bin/sh\nexec sleep 100\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = LaunchSpec {
            cwd: dir.path().to_string_lossy().into_owned(),
            shell_mode: Some("raw".into()),
            shell_command: Some(prog.to_string_lossy().into_owned()),
            ..Default::default()
        };
        // Started at the size read before the late input.
        let next = spawn(&d, id, spec, Map::new(), 80, 24, 0).unwrap();
        let pty_size = |t: &Terminal| {
            let io = t.io.lock().unwrap();
            let s = io.master.as_ref().unwrap().get_size().unwrap();
            (s.cols, s.rows)
        };
        assert_eq!(pty_size(&next), (80, 24));
        let desk = Outbox::with_pace(1 << 20, 1 << 21, None, None);
        let pace = PaceCfg {
            idle: Duration::from_secs(1),
            burst: Duration::from_millis(100),
            window: Duration::from_secs(3),
        };
        let mob = Outbox::with_pace(1 << 20, 1 << 21, None, Some(pace));
        {
            let mut o = prev.out.lock().unwrap();
            o.subs.insert(1, desk.clone());
            o.subs.insert(2, mob.clone());
        }
        // The Mobile typed after the replacement started at 80×24.
        prev.io.lock().unwrap().arb.on_resize(2, 40, 20, true);
        prev.io.lock().unwrap().arb.on_input(2, true);
        prev.hand_over(&next);
        assert_eq!(next.io.lock().unwrap().arb.current(), (40, 20));
        assert_eq!(
            pty_size(&next),
            (40, 20),
            "the replacement's PTY was not resized"
        );
        assert_eq!(next.attached(), 2);
        let kinds = |ob: &Outbox| -> Vec<String> {
            ob.take_all()
                .into_iter()
                .map(|o| match o {
                    Out::Output { urgent, .. } => format!("output urgent={urgent}"),
                    Out::Size { frame, .. } => String::from_utf8_lossy(&frame[5..]).into(),
                    _ => "other".into(),
                })
                .collect()
        };
        assert_eq!(kinds(&desk), vec!["output urgent=true"]);
        assert_eq!(
            kinds(&mob),
            vec![
                "output urgent=true".to_string(),
                format!(r#"{{"t":"term.size","terminal":"{id}","cols":40,"rows":20}}"#)
            ]
        );
        // Unchanged size: no notice.
        let third = unresolved(&d, persisted());
        third.io.lock().unwrap().arb = SizeArbiter::new(40, 20);
        next.hand_over(&third);
        assert_eq!(kinds(&mob), vec!["output urgent=true"]);
        next.kill(Duration::from_millis(100));
        assert!(next.wait_exited(Instant::now() + Duration::from_secs(5)));
    }

    /// An overflow recovered with a fresh replay keeps the size notice still queued.
    #[test]
    fn mobile_overflow_recovery_keeps_the_size_notice() {
        use super::super::outbox::{Out, PaceCfg};
        let dir = tempfile::tempdir().unwrap();
        let d = super::super::role::tests::daemon(dir.path());
        let t = unresolved(
            &d,
            PersistedTerminal {
                terminal: Uuid::new_v4(),
                spec: LaunchSpec::default(),
                meta: Map::new(),
                cols: 80,
                rows: 24,
                created_at_ms: 0,
                leader: None,
            },
        );
        let desk = Outbox::with_pace(1 << 20, 1 << 21, None, None);
        let pace = PaceCfg {
            idle: Duration::from_secs(1),
            burst: Duration::from_millis(100),
            window: Duration::from_secs(3),
        };
        let mob = Outbox::with_pace(1 << 20, 1 << 21, None, Some(pace));
        {
            let mut o = t.out.lock().unwrap();
            o.subs.insert(1, desk.clone());
            o.subs.insert(2, mob.clone());
            mob.push_output(t.id, Arc::from(&b"stale"[..]));
            t.notify_size(&o, (40, 20));
            mob.push_output(t.id, Arc::from(&b"more"[..]));
        }
        t.recover_mobile_overflow(&mob, Duration::from_millis(10));
        let got: Vec<String> = mob
            .take_all()
            .into_iter()
            .map(|o| match o {
                Out::Output { data, .. } => format!("out {}", String::from_utf8_lossy(&data)),
                Out::Size { frame, .. } => String::from_utf8_lossy(&frame[5..]).into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                format!(
                    r#"{{"t":"term.size","terminal":"{}","cols":40,"rows":20}}"#,
                    t.id
                ),
                "out \u{1b}c".to_string(),
            ]
        );
    }

    #[test]
    fn replacement_run_stamps_after_the_previous_run() {
        // A Relaunch's run starts unstamped, above the replaced run's last stamp.
        let mut c = cell(5000);
        assert_eq!(c.listed(), (None, None));
        assert_eq!(c.last_stamp(), 5000);
        assert!(event(&mut c, AgentStatus::Working, 4000));
        assert_eq!(c.listed().1, Some(5001));
        let mut c = cell(5000);
        assert!(event(&mut c, AgentStatus::Working, 9000));
        assert_eq!(c.listed().1, Some(9000));
        // Non-agent Terminals have no status and so no stamp.
        let mut shell = StatusCell::new(
            Tracker::new(&LaunchSpec {
                shell_mode: Some("raw".into()),
                ..Default::default()
            }),
            0,
        );
        let ended = shell.tracker.on_exit();
        assert!(!shell.stamp(ended, 1));
        assert_eq!(shell.listed(), (None, None));
    }
}

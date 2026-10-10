//! Local Host Terminals: one PTY per Tab, its output pumped to the frontend, and Relaunch
//! (restart the agent in place with `skipPermissions` changed, resuming its session).
//!
//! A Terminal runs one process at a time, a *run*. Each run has three threads: the reader
//! (PTY → pending buffer), the flusher (pending buffer → sink, then the exit once drained)
//! and the waiter (reaps the child). Runs are numbered, and a thread of an older run never
//! changes the Terminal once a newer run started.
//!
//! Locking: `LocalPtys.terms` → `LocalTerminal.st` → `LocalTerminal.master` →
//! `LocalTerminal.writer`. The flusher publishes the exit under `st`, which close and
//! Relaunch also hold to change the phase, so an exit is emitted at most once per run and
//! never for a run a Relaunch is replacing. A write blocks while the PTY's input queue is
//! full (the process does not read), so it holds only its run's [`WriterCell`], which no
//! other path ever waits for: close and Relaunch always get through.
//!
//! Agent Status: each Terminal has a token, the id its agent's hooks report under (Tab ids
//! are guessable, the token is not), and a [`Tracker`] for the current run in
//! `LocalTerminal.agent`, which is locked last and alone. Changes go to the observer outside
//! every lock, numbered, so a later change always wins over an earlier one.

use portable_pty::{native_pty_system, Child, ChildKiller, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::{AgentHooks, AgentStatus, Tracker};
use xshell_core::launch::{relaunch_spec, CommandPlan, LaunchSpec, TerminalHooks};
use xshell_core::terminal::replay::RESET;
use xshell_core::terminal::OVERFLOW_NOTICE;

// PTY transport tuning. The flusher coalesces a short window after the first
// byte so a burst ships as one binary chunk; MAX_IDLE is just a wakeup safety net. The pending
// buffer is capped so a frontend that stalls can't grow it unbounded — on overflow we discard
// the backlog and inject a hard reset rather than slice a CSI sequence in half.
const FLUSH_COALESCE: Duration = Duration::from_millis(4);

const FLUSH_MAX_IDLE: Duration = Duration::from_millis(50);

const READ_BUF: usize = 16 * 1024;

const MAX_PENDING: usize = 4 * 1024 * 1024;

/// Where a Terminal's output and exit go (the Tab's channels).
pub trait Sink: Send + Sync {
    /// `false`: the receiver is gone.
    fn data(&self, bytes: Vec<u8>) -> bool;
    fn exit(&self, code: i32);
}

/// Resolves a launch spec to the process to start, with the agent's status hooks if any.
pub type Planner =
    Arc<dyn Fn(&LaunchSpec, Option<TerminalHooks>) -> Result<CommandPlan, String> + Send + Sync>;

/// Told each Agent Status change of a Tab's Terminal: the Tab id, the status (`None`: none,
/// after a Relaunch or once closed) and a number that grows with every change.
pub type Observer = Arc<dyn Fn(&str, Option<AgentStatus>, u64) + Send + Sync>;

/// Where Agent Status changes go, shared by every Terminal.
#[derive(Default)]
struct AgentOut {
    seq: AtomicU64,
    observer: OnceLock<Observer>,
}

/// The current run's Agent Status.
struct AgentRun {
    run: u64,
    tracker: Tracker,
    /// The Terminal was closed: nothing changes any more.
    closed: bool,
    /// The Terminal is listed under its Tab id. Until then its tracker follows the run, but
    /// nothing is published: a Terminal it replaces must clear the Tab first.
    published: bool,
}

/// A change to publish once the locks are released.
struct Change {
    status: Option<AgentStatus>,
    seq: u64,
}

/// Where a [`Hook`] runs. Tests use it to order races; it runs with no lock held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// A Relaunch reserved the Terminal and is about to end its process.
    Reserved,
    /// A Relaunch stopped waiting for the old process; `done` says whether it ended and its
    /// output drained in time. Returning `true` treats it as a timeout.
    Waited { done: bool },
    /// A run's output drained; `published` says whether its exit went to the sink.
    Drained { published: bool },
    /// A write holds its run's writer and is about to write (it may block).
    Writing,
    /// A run's reader and flusher threads run and its waiter is next. Returning `true`
    /// makes the waiter's start fail.
    StartWaiter { pid: Option<u32> },
}

pub type Hook = Arc<dyn Fn(&str, Point) -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Live,
    /// A Relaunch is ending the process: its exit is held back.
    Relaunching,
    Closing,
    Exited,
}

/// One run's PTY writer. A write clones the cell out of `LocalTerminal.writer` and writes
/// holding only the cell, so replacing or dropping the run never waits for a blocked write.
type WriterCell = Arc<Mutex<Box<dyn Write + Send>>>;

/// A run's PTY, taken out to be dropped outside the locks (dropping the writer writes to the
/// PTY).
type Pty = (Option<Box<dyn MasterPty + Send>>, Option<WriterCell>);

struct St {
    phase: Phase,
    spec: LaunchSpec,
    run: u64,
    pid: Option<u32>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The current run's child was reaped.
    reaped: bool,
    /// The current run's output reached the sink in full.
    drained: bool,
}

struct LocalTerminal {
    id: String,
    /// What the agent's hooks report under; kept across Relaunches.
    token: Uuid,
    sink: Arc<dyn Sink>,
    st: Mutex<St>,
    cv: Condvar,
    /// The current run's PTY and writer. `None` once closed (and on Windows while a Relaunch
    /// tears the pseudoconsole down).
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<WriterCell>>,
    agent: Mutex<AgentRun>,
    agent_out: Arc<AgentOut>,
}

/// A started process before its threads run.
struct Started {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    reader: Box<dyn Read + Send>,
    child: Box<dyn Child + Send + Sync>,
}

pub struct LocalPtys {
    terms: Mutex<HashMap<String, Arc<LocalTerminal>>>,
    planner: Planner,
    /// SIGHUP → SIGKILL delay when a Relaunch ends a process.
    pub(crate) kill_grace: Duration,
    /// How long after SIGKILL a Relaunch waits for the process to end and its output to drain.
    pub(crate) exit_timeout: Duration,
    pub(crate) hook: Option<Hook>,
    /// End a run by terminating it and closing its pseudoconsole (Windows, which has no
    /// hangup) rather than only by signals. Settable so tests run that path everywhere.
    pub(crate) close_console: bool,
    /// How agents report their Agent Status, once the event socket is up. Unset (no hooks)
    /// until then, on Windows, and when the socket or settings file failed.
    hooks: OnceLock<AgentHooks>,
    agent_out: Arc<AgentOut>,
}

fn start(plan: &CommandPlan, size: PtySize) -> Result<Started, String> {
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|e| format!("Failed to open PTY: {}", e))?;
    let child = pair
        .slave
        .spawn_command(plan.to_command_builder())
        .map_err(|e| format!("Failed to spawn command: {}", e))?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone reader: {}", e))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to take writer: {}", e))?;
    Ok(Started {
        master: pair.master,
        writer,
        reader,
        child,
    })
}

impl LocalPtys {
    pub fn new(planner: Planner) -> Self {
        Self {
            terms: Mutex::new(HashMap::new()),
            planner,
            kill_grace: Duration::from_secs(2),
            exit_timeout: Duration::from_secs(5),
            hook: None,
            close_console: cfg!(windows),
            hooks: OnceLock::new(),
            agent_out: Arc::default(),
        }
    }

    /// Launch agents with status hooks from now on. Only the first call counts.
    // Unused on Windows: in-process Tabs there have no event socket (their Daemon's do).
    #[cfg_attr(windows, allow(dead_code))]
    pub fn set_hooks(&self, hooks: AgentHooks) {
        let _ = self.hooks.set(hooks);
    }

    /// Where Agent Status changes go. Only the first call counts.
    // Unused on Windows: in-process Tabs there have no event socket (their Daemon's do).
    #[cfg_attr(windows, allow(dead_code))]
    pub fn set_observer(&self, observer: Observer) {
        let _ = self.agent_out.observer.set(observer);
    }

    fn plan(&self, spec: &LaunchSpec, token: Uuid, run: u64) -> Result<CommandPlan, String> {
        let hooks = self.hooks.get().map(|hooks| TerminalHooks {
            hooks,
            terminal: token,
            run,
        });
        (self.planner)(spec, hooks)
    }

    /// A hook's report: `token` names the Terminal, `run` its process.
    // Unused on Windows: in-process Tabs there have no event socket (their Daemon's do).
    #[cfg_attr(windows, allow(dead_code))]
    pub fn on_agent_event(&self, token: Uuid, run: u64, status: AgentStatus) -> Result<(), String> {
        let t = self
            .terms
            .lock()
            .unwrap()
            .values()
            .find(|t| t.token == token)
            .cloned()
            .ok_or_else(|| format!("unknown terminal {token}"))?;
        let change = {
            let mut a = t.agent.lock().unwrap();
            if a.closed {
                return Err(format!("unknown terminal {token}"));
            }
            if a.tracker.agent().is_none() {
                return Err("not an agent terminal".into());
            }
            if a.run != run {
                return Err("stale run".into());
            }
            let changed = a.tracker.on_event(status)?;
            changed.then(|| t.change(&a))
        };
        if let Some(c) = change {
            t.emit(c);
        }
        Ok(())
    }

    fn hook(&self, id: &str, p: Point) -> bool {
        self.hook.as_ref().is_some_and(|h| h(id, p))
    }

    fn get(&self, id: &str) -> Option<Arc<LocalTerminal>> {
        self.terms.lock().unwrap().get(id).cloned()
    }

    /// Start a Terminal under `id`, replacing (and closing) any earlier one.
    pub fn spawn(
        self: &Arc<Self>,
        id: String,
        spec: LaunchSpec,
        cols: u16,
        rows: u16,
        sink: Arc<dyn Sink>,
    ) -> Result<(), String> {
        let token = Uuid::new_v4();
        let plan = self.plan(&spec, token, 0)?;
        let tracker = Tracker::new(&spec);
        let s = start(
            &plan,
            PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
        )?;
        let t = Arc::new(LocalTerminal {
            id: id.clone(),
            token,
            sink,
            st: Mutex::new(St {
                phase: Phase::Live,
                spec,
                run: 0,
                pid: s.child.process_id(),
                killer: s.child.clone_killer(),
                reaped: false,
                drained: false,
            }),
            cv: Condvar::new(),
            master: Mutex::new(Some(s.master)),
            writer: Mutex::new(Some(Arc::new(Mutex::new(s.writer)))),
            agent: Mutex::new(AgentRun {
                run: 0,
                tracker,
                closed: false,
                published: false,
            }),
            agent_out: self.agent_out.clone(),
        });
        if let Err(e) = self.run_threads(&t, 0, s.reader, s.child) {
            // Nothing was listed or shown yet: no exit to publish.
            t.st.lock().unwrap().phase = Phase::Exited;
            drop(t.take_pty());
            return Err(e);
        }
        // Under the map lock: the Terminal replaced clears the Tab, then this one's status
        // (if its run already reported) is published, numbered in that order.
        let (old, opened) = {
            let mut terms = self.terms.lock().unwrap();
            let old = terms.insert(id, t.clone()).map(|old| {
                let cleared = old.agent_close();
                (old.mark_closing(), cleared, old)
            });
            (old, t.agent_publish())
        };
        if let Some((pty, cleared, old)) = old {
            drop(pty);
            if let Some(c) = cleared {
                old.emit(c);
            }
        }
        if let Some(c) = opened {
            t.emit(c);
        }
        Ok(())
    }

    pub fn write(&self, id: &str, data: &[u8]) -> Result<(), String> {
        let Some(t) = self.get(id) else {
            return Ok(());
        };
        t.agent_change(None, |tr| tr.on_input(data));
        let cell = t.writer.lock().unwrap().clone();
        let Some(cell) = cell else {
            return Ok(());
        };
        let mut w = cell.lock().unwrap();
        self.hook(id, Point::Writing);
        w.write_all(data)
            .map_err(|e| format!("Write failed: {}", e))?;
        w.flush().map_err(|e| format!("Flush failed: {}", e))?;
        Ok(())
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let Some(t) = self.get(id) else {
            return Ok(());
        };
        let master = t.master.lock().unwrap();
        if let Some(master) = master.as_ref() {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| format!("Resize failed: {}", e))?;
        }
        Ok(())
    }

    /// Forget the Terminal and drop its PTY, as closing a Tab always did. A Relaunch in
    /// progress ends with an error and starts nothing.
    pub fn close(&self, id: &str) {
        // Marked under the map lock, which a Relaunch holds while it starts the replacement:
        // a Terminal is either replaced before it is closed or closed before it is replaced.
        let t = {
            let mut terms = self.terms.lock().unwrap();
            // Numbered under the map lock: a Terminal spawned under this Tab id later is
            // numbered after it.
            terms
                .remove(id)
                .map(|t| (t.mark_closing(), t.agent_close(), t))
        };
        if let Some((pty, cleared, t)) = t {
            drop(pty);
            if let Some(c) = cleared {
                t.emit(c);
            }
        }
    }

    /// End the Terminal's process and start it again with `skipPermissions` set to `skip`,
    /// resuming `session_id`: the Tab's current session, which the frontend may have linked
    /// or switched after the start. `Ok(false)` when `skip` already applies. The sink sees
    /// the old output, a reset, then the new process's output, and no exit.
    pub fn relaunch(
        &self,
        id: &str,
        skip: bool,
        session_id: Option<String>,
        agent: Option<String>,
    ) -> Result<bool, String> {
        let t = self
            .get(id)
            .ok_or_else(|| format!("unknown terminal {id}"))?;
        let (next, size, groups) = {
            let mut st = t.st.lock().unwrap();
            match st.phase {
                Phase::Live => {}
                Phase::Relaunching => return Err("terminal is already relaunching".into()),
                Phase::Closing => return Err("terminal is closing".into()),
                Phase::Exited => return Err("terminal has exited".into()),
            }
            st.spec.session_id = session_id;
            st.spec.agent = agent;
            let next = relaunch_spec(&st.spec, skip)?;
            if st.spec.skip_permissions.unwrap_or(false) == skip {
                return Ok(false);
            }
            // Never the writer: a write blocked on a process that does not read must not keep
            // the Relaunch from ending that process.
            let master = t.master.lock().unwrap();
            let master = master.as_ref().ok_or("terminal is closing")?;
            // Taken before the teardown: on Windows the pseudoconsole goes with the process.
            let size = master
                .get_size()
                .map_err(|e| format!("cannot read the terminal size: {e}"))?;
            let groups = process_groups(st.pid, master.as_ref());
            st.phase = Phase::Relaunching;
            (next, size, groups)
        };
        self.hook(id, Point::Reserved);

        t.hang_up(&groups, self.close_console);
        let mut done = t.wait_done(Instant::now() + self.kill_grace);
        if !done {
            kill_groups(&groups);
            done = t.wait_done(Instant::now() + self.exit_timeout);
        }
        let timed_out = self.hook(id, Point::Waited { done }) || !done;

        let (r, changes) = self.finish_relaunch(&t, id, next, size, timed_out);
        for c in changes {
            t.emit(c);
        }
        r
    }

    /// The end of a Relaunch, under the locks: start the replacement, or publish the exit
    /// held back in the meantime. Returns the Agent Status change to publish after.
    fn finish_relaunch(
        &self,
        t: &Arc<LocalTerminal>,
        id: &str,
        next: LaunchSpec,
        size: PtySize,
        timed_out: bool,
    ) -> (Result<bool, String>, Vec<Change>) {
        let terms = self.terms.lock().unwrap();
        let mut st = t.st.lock().unwrap();
        let listed = terms.get(id).is_some_and(|c| Arc::ptr_eq(c, t));
        if st.phase == Phase::Closing || !listed {
            return (
                Err("terminal was closed during the relaunch".into()),
                vec![],
            );
        }
        if timed_out {
            // Nothing replaces the process. Publish an exit held back in the meantime, once.
            if st.drained {
                st.phase = Phase::Exited;
                t.sink.exit(0);
                let ended = t.agent_update(Some(st.run), |tr| tr.on_exit());
                return (
                    Err("previous process did not exit".into()),
                    ended.into_iter().collect(),
                );
            }
            st.phase = Phase::Live;
            return (Err("previous process did not exit".into()), vec![]);
        }
        // The new run starts with no Agent Status, before its process exists: its first
        // output and hook reports already count for it.
        let mut changes: Vec<Change> = t.agent_reset(st.run + 1, &next).into_iter().collect();
        let started = self
            .plan(&next, t.token, st.run + 1)
            .and_then(|plan| start(&plan, size));
        let s = match started {
            Ok(s) => s,
            Err(e) => {
                st.phase = Phase::Exited;
                t.sink.exit(0);
                changes.extend(t.agent_update(Some(st.run + 1), |tr| tr.on_exit()));
                return (Err(format!("restart failed: {e}")), changes);
            }
        };
        // Before the new run's flusher can send anything.
        t.sink.data(RESET.to_vec());
        let (pid, killer) = (s.child.process_id(), s.child.clone_killer());
        // The new run's threads wait for `st`, held here, before they touch the Terminal.
        if let Err(e) = self.run_threads(t, st.run + 1, s.reader, s.child) {
            // The replacement was ended and reaped; the Tab sees the run end, once.
            st.phase = Phase::Exited;
            t.sink.exit(0);
            changes.extend(t.agent_update(Some(st.run + 1), |tr| tr.on_exit()));
            return (Err(format!("restart failed: {e}")), changes);
        }
        st.spec = next;
        st.run += 1;
        st.pid = pid;
        st.killer = killer;
        st.reaped = false;
        st.drained = false;
        st.phase = Phase::Live;
        // Drops the old writer (once no write holds it); the EOF it writes goes to a PTY
        // nothing reads any more.
        *t.master.lock().unwrap() = Some(s.master);
        *t.writer.lock().unwrap() = Some(Arc::new(Mutex::new(s.writer)));
        (Ok(true), changes)
    }

    /// Start run `run`'s reader, flusher and waiter. If one cannot start, the child is
    /// killed and reaped here and the threads already running drain into a run that never
    /// becomes current.
    fn run_threads(
        &self,
        t: &Arc<LocalTerminal>,
        run: u64,
        reader: Box<dyn Read + Send>,
        child: Box<dyn Child + Send + Sync>,
    ) -> Result<(), String> {
        let pid = child.process_id();
        let child = Arc::new(Mutex::new(Some(child)));
        let fail = |e: std::io::Error| {
            if let Some(mut c) = child.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            format!("Failed to start terminal threads: {e}")
        };
        let thread = |name: &str| std::thread::Builder::new().name(format!("pty-{name}-{}", t.id));
        // ── PTY → frontend transport ─────────────────────────────────────────
        // Reader thread does blocking reads of large chunks and appends RAW BYTES to a shared
        // buffer. A separate flusher coalesces a short window so a burst (e.g. a full TUI
        // repaint) ships as ONE binary Channel message instead of many JSON events. The
        // frontend feeds the bytes straight to xterm, which reassembles multibyte/escape
        // sequences across chunk boundaries — so the renderer only ever sees whole frames (no
        // partial-frame jitter), and we never split a CSI sequence or a UTF-8 codepoint.
        let pending: Arc<(Mutex<Vec<u8>>, Condvar)> =
            Arc::new((Mutex::new(Vec::with_capacity(READ_BUF)), Condvar::new()));
        let done = Arc::new(AtomicBool::new(false));

        let pending_r = pending.clone();
        let done_r = done.clone();
        let tr = t.clone();
        thread("read")
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                let mut buf = [0u8; READ_BUF];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let (lock, cv) = &*pending_r;
                            let mut g = lock.lock().unwrap();
                            // Backpressure: discard the whole backlog (slicing it would corrupt
                            // xterm mid-escape) and drop a hard reset + notice in its place.
                            if g.len() + n > MAX_PENDING {
                                g.clear();
                                g.extend_from_slice(OVERFLOW_NOTICE);
                            }
                            g.extend_from_slice(&buf[..n]);
                            cv.notify_one();
                            drop(g);
                            tr.agent_change(Some(run), |a| a.on_output(&buf[..n]));
                        }
                    }
                }
                done_r.store(true, Ordering::Release);
                pending_r.1.notify_one();
            })
            .map_err(fail)?;

        // Flusher: wait for data, coalesce a burst into one chunk, send as binary. When the
        // reader has hit EOF and the buffer is fully drained, publish the exit — same thread,
        // so the exit never races ahead of the final output chunk.
        let (tf, hook) = (t.clone(), self.hook.clone());
        thread("flush")
            .spawn(move || {
                let (lock, cv) = &*pending;
                loop {
                    {
                        let mut g = lock.lock().unwrap();
                        while g.is_empty() {
                            if done.load(Ordering::Acquire) {
                                drop(g);
                                let published = tf.drained(run);
                                if let (Some(h), Some(published)) = (hook, published) {
                                    h(&tf.id, Point::Drained { published });
                                }
                                return;
                            }
                            let (next, _) = cv.wait_timeout(g, FLUSH_MAX_IDLE).unwrap();
                            g = next;
                        }
                    }
                    std::thread::sleep(FLUSH_COALESCE);
                    let chunk = std::mem::take(&mut *lock.lock().unwrap());
                    if chunk.is_empty() {
                        continue;
                    }
                    if !tf.sink.data(chunk) {
                        break;
                    }
                }
            })
            .map_err(fail)?;

        let (tw, cw) = (t.clone(), child.clone());
        let waiter = if self.hook(&t.id, Point::StartWaiter { pid }) {
            Err(std::io::Error::other("refused by a test hook"))
        } else {
            thread("wait").spawn(move || {
                let Some(mut child) = cw.lock().unwrap().take() else {
                    return;
                };
                let _ = child.wait();
                let mut st = tw.st.lock().unwrap();
                if st.run == run {
                    st.reaped = true;
                    tw.cv.notify_all();
                }
            })
        };
        waiter.map(drop).map_err(fail)
    }
}

impl LocalTerminal {
    /// Run `run`'s output reached the sink in full: publish its exit unless a Relaunch holds
    /// it back. Returns whether it was published, or `None` for a run already replaced.
    fn drained(&self, run: u64) -> Option<bool> {
        let mut st = self.st.lock().unwrap();
        if st.run != run {
            return None;
        }
        st.drained = true;
        self.cv.notify_all();
        let publish = match st.phase {
            Phase::Live => {
                st.phase = Phase::Exited;
                true
            }
            // The Tab is going away; tell it anyway, as closing always did.
            Phase::Closing => true,
            Phase::Relaunching | Phase::Exited => false,
        };
        if publish {
            self.sink.exit(0);
        }
        drop(st);
        if publish {
            self.agent_change(Some(run), |a| a.on_exit());
        }
        Some(publish)
    }

    /// The current Agent Status change, numbered. Called with `agent` locked.
    fn change(&self, a: &AgentRun) -> Change {
        Change {
            status: a.tracker.status(),
            seq: self.agent_out.seq.fetch_add(1, Ordering::SeqCst),
        }
    }

    /// Apply `f` to run `run`'s tracker (the current run's with `None`); the change to
    /// publish, if it changed anything.
    fn agent_update(
        &self,
        run: Option<u64>,
        f: impl FnOnce(&mut Tracker) -> bool,
    ) -> Option<Change> {
        let mut a = self.agent.lock().unwrap();
        if a.closed || run.is_some_and(|r| r != a.run) {
            return None;
        }
        (f(&mut a.tracker) && a.published).then(|| self.change(&a))
    }

    /// The Terminal is now listed: publish its status from here on, and now if its run has
    /// one already. Called under the map lock.
    fn agent_publish(&self) -> Option<Change> {
        let mut a = self.agent.lock().unwrap();
        a.published = true;
        (!a.closed && a.tracker.status().is_some()).then(|| self.change(&a))
    }

    /// [`LocalTerminal::agent_update`], published. Call with no lock held.
    fn agent_change(&self, run: Option<u64>, f: impl FnOnce(&mut Tracker) -> bool) {
        if let Some(c) = self.agent_update(run, f) {
            self.emit(c);
        }
    }

    /// Start run `run`'s tracker afresh; the change to publish (no status) unless the
    /// Terminal is closed.
    fn agent_reset(&self, run: u64, spec: &LaunchSpec) -> Option<Change> {
        let mut a = self.agent.lock().unwrap();
        if a.closed {
            return None;
        }
        a.run = run;
        a.tracker = Tracker::new(spec);
        a.published.then(|| self.change(&a))
    }

    /// The Terminal is gone: nothing changes its status any more. Returns the change that
    /// clears its Tab, to publish once the locks are released; call it under the map lock,
    /// so whatever is listed under the Tab id next is numbered after it.
    fn agent_close(&self) -> Option<Change> {
        let mut a = self.agent.lock().unwrap();
        if a.closed {
            return None;
        }
        a.closed = true;
        a.published.then(|| Change {
            status: None,
            seq: self.agent_out.seq.fetch_add(1, Ordering::SeqCst),
        })
    }

    fn emit(&self, c: Change) {
        if let Some(o) = self.agent_out.observer.get() {
            o(&self.id, c.status, c.seq);
        }
    }

    /// Whether the current run's child is reaped and its output drained, waiting until
    /// `deadline`.
    fn wait_done(&self, deadline: Instant) -> bool {
        let mut st = self.st.lock().unwrap();
        while !(st.reaped && st.drained) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.cv.wait_timeout(st, left).unwrap().0;
        }
        true
    }

    /// Mark the Terminal closing and take its PTY, for the caller to drop outside the locks
    /// (dropping the writer writes to the PTY).
    fn mark_closing(&self) -> Pty {
        let mut st = self.st.lock().unwrap();
        if st.phase != Phase::Exited {
            st.phase = Phase::Closing;
        }
        self.take_pty()
    }

    fn take_pty(&self) -> Pty {
        (
            self.master.lock().unwrap().take(),
            self.writer.lock().unwrap().take(),
        )
    }

    /// Ask the process to end: SIGHUP its session's and foreground job's process groups on
    /// Unix. Windows has no hangup: the process is terminated and the pseudoconsole closed.
    fn hang_up(&self, groups: &[i32], close_console: bool) {
        #[cfg(unix)]
        for &g in groups {
            unsafe { libc::killpg(g, libc::SIGHUP) };
        }
        #[cfg(windows)]
        let _ = groups;
        if close_console {
            if cfg!(windows) {
                let _ = self.st.lock().unwrap().killer.kill();
            }
            // Closing the pseudoconsole ends what still runs in it and lets the reader drain.
            // The size went with it: the Relaunch read it before.
            drop(self.take_pty());
        }
    }
}

/// The process groups a Relaunch ends: the session leader's (portable-pty starts it with
/// `setsid`) and the foreground job's (job control puts an agent under `bash -i` in its own).
#[cfg(unix)]
fn process_groups(pid: Option<u32>, master: &dyn MasterPty) -> Vec<i32> {
    let mut groups: Vec<i32> = pid.map(|p| p as i32).into_iter().collect();
    if let Some(g) = master.process_group_leader() {
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    groups
}

#[cfg(windows)]
fn process_groups(_pid: Option<u32>, _master: &dyn MasterPty) -> Vec<i32> {
    Vec::new()
}

fn kill_groups(groups: &[i32]) {
    #[cfg(unix)]
    for &g in groups {
        if unsafe { libc::killpg(g, 0) } == 0 {
            unsafe { libc::killpg(g, libc::SIGKILL) };
        }
    }
    #[cfg(windows)]
    let _ = groups;
}

/// The pseudoconsole teardown, run on every platform (`close_console`): the replacement gets
/// the size the Tab had, read before the teardown took the PTY away.
#[cfg(test)]
mod teardown_tests {
    use super::*;

    struct NullSink;

    impl Sink for NullSink {
        fn data(&self, _: Vec<u8>) -> bool {
            true
        }
        fn exit(&self, _: i32) {}
    }

    /// A long-running child that never reads and outlives a hangup.
    fn idle_child(p: CommandPlan) -> CommandPlan {
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd.exe", &["/C", "ping -n 1000 127.0.0.1 >NUL"])
        } else {
            ("/bin/sh", &["-c", "trap '' HUP; exec sleep 1000"])
        };
        CommandPlan {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..p
        }
    }

    #[test]
    fn local_relaunch_console_teardown_keeps_size() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = xshell_core::HostCtx::with_home(dir.path().join("home"), dir.path().join("tmp"));
        let planner: Planner = Arc::new(move |spec, hooks| {
            Ok(idle_child(xshell_core::plan_command_with(
                &ctx, spec, hooks,
            )?))
        });
        let mut ptys = LocalPtys::new(planner);
        ptys.kill_grace = Duration::from_millis(200);
        ptys.close_console = true;
        let ptys = Arc::new(ptys);
        let spec = LaunchSpec {
            session_id: Some("s1".into()),
            cwd: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        ptys.spawn("t".into(), spec, 80, 24, Arc::new(NullSink))
            .unwrap();
        ptys.resize("t", 100, 30).unwrap();
        assert_eq!(ptys.relaunch("t", true, Some("s1".into()), None), Ok(true));
        let t = ptys.get("t").unwrap();
        let size = t
            .master
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .get_size()
            .unwrap();
        assert_eq!((size.cols, size.rows), (100, 30));
        let _ = t.st.lock().unwrap().killer.kill();
        ptys.close("t");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use xshell_core::claude::encode_project_name;
    use xshell_core::HostCtx;

    const T: Duration = Duration::from_secs(10);
    const SID: &str = "11111111-2222-3333-4444-555555555555";
    /// Ignores SIGHUP like a stubborn agent, prints its pid and argv, exits on a line of input.
    const AGENT: &str = "trap '' HUP\necho \"pid $$ args $*.\"\nread line\nexit 0";

    #[derive(Default)]
    struct VecSink {
        data: Mutex<Vec<u8>>,
        exits: Mutex<Vec<i32>>,
        cv: Condvar,
    }

    impl Sink for VecSink {
        fn data(&self, bytes: Vec<u8>) -> bool {
            self.data.lock().unwrap().extend(bytes);
            self.cv.notify_all();
            true
        }
        fn exit(&self, code: i32) {
            self.exits.lock().unwrap().push(code);
            self.cv.notify_all();
        }
    }

    impl VecSink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.data.lock().unwrap()).into_owned()
        }

        /// Wait until the output contains `needle`; returns the output.
        fn wait_for(&self, needle: &str) -> String {
            let deadline = Instant::now() + T;
            let mut g = self.data.lock().unwrap();
            loop {
                let s = String::from_utf8_lossy(&g).into_owned();
                if s.contains(needle) {
                    return s;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "no {needle:?} in {s:?}");
                g = self.cv.wait_timeout(g, left).unwrap().0;
            }
        }

        fn wait_exit(&self) -> Vec<i32> {
            let deadline = Instant::now() + T;
            let mut g = self.exits.lock().unwrap();
            while g.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "no exit");
                g = self.cv.wait_timeout(g, left).unwrap().0;
            }
            g.clone()
        }

        fn exits(&self) -> Vec<i32> {
            self.exits.lock().unwrap().clone()
        }
    }

    /// The pid printed by the run whose argv is `args`.
    fn pid_of(text: &str, args: &str) -> i32 {
        let end = text
            .find(&format!(" args {args}."))
            .unwrap_or_else(|| panic!("no run with {args:?} in {text:?}"));
        let start = text[..end].rfind("pid ").unwrap() + 4;
        text[start..end].parse().unwrap()
    }

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    struct Fx {
        dir: tempfile::TempDir,
        ptys: Arc<LocalPtys>,
        sink: Arc<VecSink>,
        /// Plans started so far.
        launches: Arc<Mutex<Vec<Vec<String>>>>,
    }

    /// Plans every spec with core's `plan_command` (home in a temp dir), then runs `script`
    /// under `sh` with the planned arguments instead of the agent. With `fail_after`, plans
    /// after that many fail.
    fn fx_with(
        script: &'static str,
        fail_after: Option<usize>,
        tweak: impl FnOnce(&mut LocalPtys),
    ) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let ctx = HostCtx::with_home(dir.path().join("home"), dir.path().join("tmp"));
        let launches: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let l = launches.clone();
        let planner: Planner = Arc::new(move |spec, hooks| {
            let mut l = l.lock().unwrap();
            if fail_after.is_some_and(|n| l.len() >= n) {
                return Err("refused by the test planner".into());
            }
            let p = xshell_core::plan_command_with(&ctx, spec, hooks)?;
            l.push(p.args.clone());
            let mut args = vec!["-c".to_string(), script.to_string(), "agent".to_string()];
            args.extend(p.args);
            Ok(CommandPlan {
                program: "/bin/sh".into(),
                args,
                ..p
            })
        });
        let mut ptys = LocalPtys::new(planner);
        ptys.kill_grace = Duration::from_millis(200);
        tweak(&mut ptys);
        Fx {
            dir,
            ptys: Arc::new(ptys),
            sink: Arc::default(),
            launches,
        }
    }

    fn fx() -> Fx {
        fx_with(AGENT, None, |_| {})
    }

    impl Fx {
        fn cwd(&self) -> String {
            self.dir.path().to_string_lossy().into_owned()
        }

        fn spawn(&self, agent: &str, session: Option<&str>) {
            let spec = LaunchSpec {
                agent: Some(agent.into()),
                session_id: session.map(str::to_string),
                cwd: self.cwd(),
                ..Default::default()
            };
            self.ptys
                .spawn("t".into(), spec, 80, 24, self.sink.clone())
                .unwrap();
        }

        /// A Claude transcript for `sid`, so a launch resumes it.
        fn transcript(&self, sid: &str) {
            let d = self
                .dir
                .path()
                .join("home/.claude/projects")
                .join(encode_project_name(&self.cwd()));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(format!("{sid}.jsonl")), "{}\n").unwrap();
        }

        fn relaunch(&self, skip: bool, agent: &str, session: Option<&str>) -> Result<bool, String> {
            self.ptys
                .relaunch("t", skip, session.map(str::to_string), Some(agent.into()))
        }

        /// Relaunch on another thread; joins with the result.
        fn relaunch_bg(&self, skip: bool) -> std::thread::JoinHandle<Result<bool, String>> {
            let ptys = self.ptys.clone();
            std::thread::spawn(move || {
                ptys.relaunch("t", skip, Some(SID.into()), Some("claude".into()))
            })
        }

        fn launches(&self) -> Vec<Vec<String>> {
            self.launches.lock().unwrap().clone()
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            for pid in self.sink.text().split("pid ").skip(1) {
                if let Some(Ok(p)) = pid.split_whitespace().next().map(str::parse::<i32>) {
                    unsafe { libc::kill(p, libc::SIGKILL) };
                }
            }
        }
    }

    /// A hook that holds the thread reaching a matching point until released, and reports
    /// every `Drained` point.
    struct Gate {
        reached: Receiver<Point>,
        release: Sender<()>,
    }

    fn gate(matches: fn(Point) -> bool) -> (Gate, Hook) {
        let (reached_tx, reached) = channel();
        let (release, release_rx) = channel::<()>();
        let (reached_tx, release_rx) = (Mutex::new(reached_tx), Mutex::new(release_rx));
        let hook: Hook = Arc::new(move |_, p| {
            if matches(p) || matches!(p, Point::Drained { .. }) {
                let _ = reached_tx.lock().unwrap().send(p);
            }
            if matches(p) {
                let _ = release_rx.lock().unwrap().recv_timeout(T);
            }
            false
        });
        (Gate { reached, release }, hook)
    }

    impl Gate {
        fn wait(&self, p: Point) {
            loop {
                let got = self.reached.recv_timeout(T).expect("hook point reached");
                if got == p {
                    return;
                }
            }
        }
    }

    #[test]
    fn local_relaunch_suppresses_exit() {
        let f = fx();
        f.transcript(SID);
        f.spawn("claude", Some(SID));
        f.sink.wait_for(&format!("args --resume {SID}."));
        assert_eq!(f.relaunch(true, "claude", Some(SID)), Ok(true));
        let new = format!("args --dangerously-skip-permissions --resume {SID}.");
        let text = f.sink.wait_for(&new);
        let reset = text.find("\x1bc").expect("a reset before the new run");
        assert!(text[reset..].contains(&new), "{text:?}");
        assert!(!text[reset..].contains(&format!("args --resume {SID}.")));
        assert!(f.sink.exits().is_empty());
        // And off again: the flag is gone.
        assert_eq!(f.relaunch(false, "claude", Some(SID)), Ok(true));
        assert_eq!(f.launches().last().unwrap(), &["--resume", SID]);
        assert!(f.sink.exits().is_empty());
    }

    #[test]
    fn local_relaunch_kills_hup_ignorer() {
        let f = fx();
        f.spawn("claude", Some(SID));
        let text = f.sink.wait_for(&format!("args --session-id {SID}."));
        let old = pid_of(&text, &format!("--session-id {SID}"));
        assert_eq!(f.relaunch(true, "claude", Some(SID)), Ok(true));
        let args = format!("--dangerously-skip-permissions --session-id {SID}");
        let text = f.sink.wait_for(&format!("args {args}."));
        // Reaped before the replacement started, so the pid is gone.
        assert!(!alive(old), "the old agent survived");
        assert!(alive(pid_of(&text, &args)));
    }

    #[test]
    fn local_relaunch_uses_current_session() {
        // Codex links its session after the start.
        let f = fx();
        f.spawn("codex", None);
        f.sink.wait_for("args .");
        assert_eq!(f.relaunch(true, "codex", Some("late")), Ok(true));
        f.sink
            .wait_for("args resume --dangerously-bypass-approvals-and-sandbox late.");

        // A Claude Tab switched to another branch of the conversation.
        let f = fx();
        f.spawn("claude", Some(SID));
        f.sink.wait_for(&format!("args --session-id {SID}."));
        let other = "99999999-8888-7777-6666-555555555555";
        f.transcript(other);
        assert_eq!(f.relaunch(true, "claude", Some(other)), Ok(true));
        f.sink.wait_for(&format!(
            "args --dangerously-skip-permissions --resume {other}."
        ));
    }

    #[test]
    fn local_relaunch_refusals() {
        let f = fx();
        f.spawn("cursor", Some(SID));
        f.sink.wait_for("args ");
        assert_eq!(
            f.relaunch(true, "cursor", Some(SID)),
            Err("cursor-agent has no flag to skip permission prompts".into())
        );
        assert_eq!(
            f.relaunch(true, "claude", None),
            Err("no session to resume".into())
        );
        assert_eq!(f.relaunch(false, "claude", Some(SID)), Ok(false));
        assert_eq!(
            f.ptys.relaunch("nope", true, None, None),
            Err("unknown terminal nope".into())
        );
        assert_eq!(f.launches().len(), 1);
        assert!(f.sink.exits().is_empty());
    }

    #[test]
    fn local_relaunch_spawn_failure_emits_exit_err() {
        let f = fx_with(AGENT, Some(1), |_| {});
        f.spawn("claude", Some(SID));
        f.sink.wait_for("args ");
        let err = f.relaunch(true, "claude", Some(SID)).unwrap_err();
        assert_eq!(err, "restart failed: refused by the test planner");
        assert_eq!(f.sink.exits(), vec![0]);
        assert!(
            !f.sink.text().contains('\x1b'),
            "no reset: {:?}",
            f.sink.text()
        );
        assert_eq!(
            f.relaunch(false, "claude", Some(SID)),
            Err("terminal has exited".into())
        );
    }

    /// A process outside the killed groups keeps the PTY open, so the output never drains:
    /// nothing replaces the run, and its exit comes once, when it really ends.
    #[test]
    fn local_relaunch_timeout_rolls_back() {
        const HOLDS_PTY: &str =
            "set -m\nsleep 1000 &\necho \"bg $!\"\ntrap '' HUP\necho \"pid $$ args $*.\"\nread line";
        let f = fx_with(HOLDS_PTY, None, |p| {
            p.exit_timeout = Duration::from_millis(300);
        });
        f.spawn("claude", Some(SID));
        let text = f.sink.wait_for("args ");
        let bg: i32 = text
            .split("bg ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            f.relaunch(true, "claude", Some(SID)),
            Err("previous process did not exit".into())
        );
        assert_eq!(f.launches().len(), 1);
        assert!(f.sink.exits().is_empty());
        assert!(!f.sink.text().contains("\x1bc"));
        unsafe { libc::kill(bg, libc::SIGKILL) };
        assert_eq!(f.sink.wait_exit(), vec![0]);
    }

    #[test]
    fn local_close_during_relaunch() {
        let (g, hook) = gate(|p| p == Point::Reserved);
        let f = fx_with(AGENT, None, |p| p.hook = Some(hook));
        f.spawn("claude", Some(SID));
        let old = pid_of(&f.sink.wait_for("args "), &format!("--session-id {SID}"));
        let r = f.relaunch_bg(true);
        g.wait(Point::Reserved);
        f.ptys.close("t");
        let _ = g.release.send(());
        assert_eq!(
            r.join().unwrap(),
            Err("terminal was closed during the relaunch".into())
        );
        assert_eq!(f.launches().len(), 1);
        assert!(!alive(old));
        assert!(!f.sink.text().contains("\x1bc"));
    }

    /// The process ends by itself just as the Relaunch reserves the Terminal: its exit is
    /// held back and the Relaunch starts the replacement.
    #[test]
    fn local_natural_exit_at_reservation() {
        let (g, hook) = gate(|p| p == Point::Reserved);
        let f = fx_with(AGENT, None, |p| p.hook = Some(hook));
        f.spawn("claude", Some(SID));
        f.sink.wait_for("args ");
        let r = f.relaunch_bg(true);
        g.wait(Point::Reserved);
        f.ptys.write("t", b"\n").unwrap();
        g.wait(Point::Drained { published: false });
        let _ = g.release.send(());
        assert_eq!(r.join().unwrap(), Ok(true));
        f.sink.wait_for(&format!(
            "args --dangerously-skip-permissions --session-id {SID}."
        ));
        assert!(f.sink.exits().is_empty());
    }

    /// The other order: the exit is published first, and the Relaunch is refused.
    #[test]
    fn local_exit_before_reservation_refused() {
        let f = fx();
        f.spawn("claude", Some(SID));
        f.sink.wait_for("args ");
        f.ptys.write("t", b"\n").unwrap();
        assert_eq!(f.sink.wait_exit(), vec![0]);
        assert_eq!(
            f.relaunch(true, "claude", Some(SID)),
            Err("terminal has exited".into())
        );
        assert_eq!(f.launches().len(), 1);
    }

    #[test]
    fn local_concurrent_relaunch_refused() {
        let (g, hook) = gate(|p| p == Point::Reserved);
        let f = fx_with(AGENT, None, |p| p.hook = Some(hook));
        f.spawn("claude", Some(SID));
        f.sink.wait_for("args ");
        let r = f.relaunch_bg(true);
        g.wait(Point::Reserved);
        for skip in [true, false] {
            assert_eq!(
                f.relaunch(skip, "claude", Some(SID)),
                Err("terminal is already relaunching".into())
            );
        }
        let _ = g.release.send(());
        assert_eq!(r.join().unwrap(), Ok(true));
    }

    /// A process that never reads fills the PTY's input queue, and a write blocks holding the
    /// writer. A Relaunch must still end that process, and a close must still return.
    #[test]
    fn local_relaunch_and_close_with_a_blocked_write() {
        const NO_READ: &str =
            "stty raw -echo\ntrap '' HUP\necho \"pid $$ args $*.\"\nexec sleep 1000";
        let (g, hook) = gate(|p| p == Point::Writing);
        let f = fx_with(NO_READ, None, |p| p.hook = Some(hook));
        f.spawn("claude", Some(SID));
        f.sink.wait_for("args ");
        let blocked_write = |ptys: Arc<LocalPtys>| {
            std::thread::spawn(move || ptys.write("t", &vec![b'x'; 4 << 20]))
        };
        let _w1 = blocked_write(f.ptys.clone());
        g.wait(Point::Writing);
        let _ = g.release.send(());

        let (tx, rx) = channel();
        let ptys = f.ptys.clone();
        std::thread::spawn(move || {
            let _ = tx.send(ptys.relaunch("t", true, Some(SID.into()), Some("claude".into())));
        });
        assert_eq!(
            rx.recv_timeout(T).expect("relaunch blocked by a write"),
            Ok(true)
        );
        f.sink.wait_for(&format!(
            "args --dangerously-skip-permissions --session-id {SID}."
        ));

        let _w2 = blocked_write(f.ptys.clone());
        g.wait(Point::Writing);
        let _ = g.release.send(());
        let (tx, rx) = channel();
        let ptys = f.ptys.clone();
        std::thread::spawn(move || {
            ptys.close("t");
            let _ = tx.send(());
        });
        rx.recv_timeout(T).expect("close blocked by a write");
    }

    /// The replacement's waiter cannot start: the replacement is killed and reaped, the Tab
    /// sees one exit, and no lock is left poisoned.
    #[test]
    fn local_relaunch_thread_start_failure_rolls_back() {
        let pids = Arc::new(Mutex::new(Vec::new()));
        let p = pids.clone();
        let f = fx_with(AGENT, None, move |ptys| {
            ptys.hook = Some(Arc::new(move |_, point| match point {
                Point::StartWaiter { pid } => {
                    let mut p = p.lock().unwrap();
                    p.push(pid);
                    p.len() == 2
                }
                _ => false,
            }))
        });
        f.spawn("claude", Some(SID));
        f.sink.wait_for("args ");
        let err = f.relaunch(true, "claude", Some(SID)).unwrap_err();
        assert_eq!(
            err,
            "restart failed: Failed to start terminal threads: refused by a test hook"
        );
        assert_eq!(f.sink.exits(), vec![0]);
        let replacement = pids.lock().unwrap()[1].expect("replacement pid") as i32;
        assert!(!alive(replacement), "the replacement was not reaped");
        assert_eq!(
            f.relaunch(false, "claude", Some(SID)),
            Err("terminal has exited".into())
        );
        // Every lock still works.
        f.ptys.write("t", b"x").unwrap();
        f.ptys.resize("t", 90, 20).unwrap();
        f.ptys.close("t");
        f.spawn("claude", Some(SID));
        f.sink.wait_for(&format!("args --session-id {SID}."));
    }

    // ── Agent Status ──

    /// Prints its hook variables, then exits on a line of input.
    const HOOKED: &str = "trap '' HUP\necho \"pid $$ args $*.\"\n\
        echo \"hook $XSHELL_TERMINAL_ID $XSHELL_EVENT_SOCKET end\"\nread line\nexit 0";

    type Seen = Arc<Mutex<Vec<(String, Option<AgentStatus>, u64)>>>;

    /// A fixture whose agents launch with hooks over a real event socket in a fresh app-data
    /// dir, and an observer recording every change.
    fn hooked_fx() -> (Fx, crate::agent_events::EventSocket, Seen) {
        let f = fx_with(HOOKED, None, |_| {});
        let seen: Seen = Arc::default();
        let s = seen.clone();
        f.ptys.set_observer(Arc::new(move |id, st, seq| {
            s.lock().unwrap().push((id.to_string(), st, seq));
        }));
        let sock = crate::agent_events::start(
            f.ptys.clone(),
            "/opt/xshell/xshell".into(),
            &f.dir.path().join("data/agent-hooks"),
            f.dir.path().join("run/xshell/desktop-1.sock"),
        )
        .unwrap();
        (f, sock, seen)
    }

    /// The newest run's `(token, run, socket)` as its hooks see them.
    fn hook_env(f: &Fx) -> (Uuid, u64, std::path::PathBuf) {
        let text = f.sink.wait_for(" end");
        let line = text.rsplit("hook ").next().unwrap();
        let mut w = line.split_whitespace();
        let (id, sock) = (w.next().unwrap(), w.next().unwrap());
        let (token, run) = id.rsplit_once('.').unwrap();
        (token.parse().unwrap(), run.parse().unwrap(), sock.into())
    }

    fn statuses(seen: &Seen) -> Vec<Option<AgentStatus>> {
        let v = seen.lock().unwrap();
        // Numbered in the order they happened.
        assert!(v.windows(2).all(|w| w[0].2 < w[1].2), "{v:?}");
        v.iter().map(|e| e.1).collect()
    }

    fn send(sock: &std::path::Path, token: Uuid, run: u64, s: AgentStatus) -> Result<(), String> {
        xshell_core::agent_status::send_event(sock, token, run, s, None)
    }

    #[test]
    fn local_hook_event_sets_status() {
        use std::os::unix::fs::PermissionsExt;
        let (f, sock, seen) = hooked_fx();
        f.spawn("claude", Some(SID));
        let (token, run, endpoint) = hook_env(&f);
        assert_eq!(endpoint, sock.path);
        assert_eq!(run, 0);
        // The settings file, written into the fresh app-data dir before any launch.
        let settings = f.dir.path().join("data/agent-hooks/claude-settings.json");
        assert_eq!(
            f.launches()[0],
            [
                "--session-id",
                SID,
                "--settings",
                &settings.to_string_lossy()
            ]
        );
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&settings), 0o600);
        assert_eq!(mode(settings.parent().unwrap()), 0o700);
        assert_eq!(mode(&sock.path), 0o600);
        assert_eq!(mode(sock.path.parent().unwrap()), 0o700);
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/opt/xshell/xshell' event - finished"
        );

        assert_eq!(send(&sock.path, token, run, AgentStatus::NeedsYou), Ok(()));
        assert_eq!(send(&sock.path, token, run, AgentStatus::NeedsYou), Ok(()));
        // Typing does not answer for Claude; an interrupt ends the turn.
        f.ptys.write("t", b"1").unwrap();
        f.ptys.write("t", b"\x1b").unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("t".to_string(), Some(AgentStatus::NeedsYou), 0),
                ("t".to_string(), Some(AgentStatus::Finished), 1),
            ]
        );
        sock.remove();
        assert!(!sock.path.exists());
    }

    #[test]
    fn local_exit_sets_ended() {
        let (f, sock, seen) = hooked_fx();
        f.spawn("codex", None);
        let (token, run, _) = hook_env(&f);
        // Codex: Enter starts a turn.
        f.ptys.write("t", b"\r").unwrap();
        assert_eq!(f.sink.wait_exit(), vec![0]);
        let deadline = Instant::now() + T;
        while statuses(&seen).last() != Some(&Some(AgentStatus::Ended)) {
            assert!(Instant::now() < deadline, "{:?}", statuses(&seen));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            statuses(&seen),
            vec![Some(AgentStatus::Working), Some(AgentStatus::Ended)]
        );
        // Ended is final.
        assert_eq!(send(&sock.path, token, run, AgentStatus::Working), Ok(()));
        assert_eq!(statuses(&seen).len(), 2);
    }

    #[test]
    fn local_relaunch_keeps_token_resets_status() {
        let (f, sock, seen) = hooked_fx();
        f.spawn("claude", Some(SID));
        let (token, run, _) = hook_env(&f);
        send(&sock.path, token, run, AgentStatus::Working).unwrap();
        assert_eq!(f.relaunch(true, "claude", Some(SID)), Ok(true));
        f.sink.wait_for("args --dangerously-skip-permissions");
        let (token2, run2, _) = hook_env(&f);
        assert_eq!(token2, token);
        assert_eq!(run2, run + 1);
        // The new run starts with no status; the old run's late SessionEnd is refused.
        assert_eq!(
            send(&sock.path, token, run, AgentStatus::Ended),
            Err("stale run".into())
        );
        send(&sock.path, token, run2, AgentStatus::NeedsYou).unwrap();
        assert_eq!(
            statuses(&seen),
            vec![
                Some(AgentStatus::Working),
                None,
                Some(AgentStatus::NeedsYou)
            ]
        );
        // Closing clears the Tab's status, and nothing reports for it afterwards.
        f.ptys.close("t");
        assert_eq!(statuses(&seen).last(), Some(&None));
        let n = statuses(&seen).len();
        assert!(send(&sock.path, token, run2, AgentStatus::Finished)
            .unwrap_err()
            .starts_with("unknown terminal"));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(statuses(&seen).len(), n);
    }

    #[test]
    fn local_unknown_token_rejected() {
        let (f, sock, seen) = hooked_fx();
        f.spawn("claude", Some(SID));
        hook_env(&f);
        let other = Uuid::new_v4();
        assert_eq!(
            send(&sock.path, other, 0, AgentStatus::Working),
            Err(format!("unknown terminal {other}"))
        );
        assert!(statuses(&seen).is_empty());
    }

    #[test]
    fn local_shells_and_hookless_agents_get_no_hooks() {
        let (f, sock, _seen) = hooked_fx();
        f.spawn("cursor", Some(SID));
        let text = f.sink.wait_for(" end");
        assert!(text.contains("hook   end"), "{text:?}");
        assert_eq!(
            f.launches()[0],
            ["--resume=11111111-2222-3333-4444-555555555555"]
        );
        drop(sock);
    }

    /// Asks for approval the moment it starts, like Codex resuming into a pending prompt.
    const PROMPTS_AT_ONCE: &str = "trap '' HUP\necho \"pid $$ args $*.\"\n\
        printf '\\033]9;Approval requested: ls\\007'\n\
        echo \"hook $XSHELL_TERMINAL_ID $XSHELL_EVENT_SOCKET end\"\nread line\nexit 0";

    /// The Tab's status as the frontend keeps it: the change numbered last wins.
    fn latest(seen: &Seen, id: &str) -> Option<Option<AgentStatus>> {
        let v = seen.lock().unwrap();
        v.iter()
            .filter(|e| e.0 == id)
            .max_by_key(|e| e.2)
            .map(|e| e.1)
    }

    fn wait_latest(seen: &Seen, id: &str, want: Option<AgentStatus>) {
        let deadline = Instant::now() + T;
        while latest(seen, id) != Some(want) {
            assert!(Instant::now() < deadline, "{:?}", seen.lock().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Every run after the first is held before its waiter starts (its reader already runs,
    /// and inside a Relaunch the Terminal's locks are held) long enough for its prompt to
    /// be read: a change published too early, or dropped, shows.
    fn prompting_fx() -> (Fx, crate::agent_events::EventSocket, Seen) {
        let runs = Arc::new(AtomicU64::new(0));
        let f = fx_with(PROMPTS_AT_ONCE, None, move |p| {
            p.hook = Some(Arc::new(move |_, point| {
                if matches!(point, Point::StartWaiter { .. })
                    && runs.fetch_add(1, Ordering::SeqCst) > 0
                {
                    std::thread::sleep(Duration::from_millis(300));
                }
                false
            }))
        });
        let seen: Seen = Arc::default();
        let s = seen.clone();
        f.ptys.set_observer(Arc::new(move |id, st, seq| {
            s.lock().unwrap().push((id.to_string(), st, seq));
        }));
        let sock = crate::agent_events::start(
            f.ptys.clone(),
            "/opt/xshell/xshell".into(),
            &f.dir.path().join("data/agent-hooks"),
            f.dir.path().join("run/xshell/desktop-1.sock"),
        )
        .unwrap();
        (f, sock, seen)
    }

    /// The replacement's first output is scanned for its own run: the reset for the new run
    /// is in place before the replacement exists.
    #[test]
    fn local_relaunch_osc9_at_start_is_kept() {
        for _ in 0..5 {
            let (f, _sock, seen) = prompting_fx();
            f.spawn("codex", Some("c1"));
            wait_latest(&seen, "t", Some(AgentStatus::NeedsYou));
            assert_eq!(f.relaunch(true, "codex", Some("c1")), Ok(true));
            f.sink
                .wait_for("args resume --dangerously-bypass-approvals-and-sandbox");
            f.sink.wait_for("hook ");
            let n = seen.lock().unwrap().len();
            // Reset, then the replacement's prompt, in that order.
            let deadline = Instant::now() + T;
            while seen.lock().unwrap().len() < n.max(3) {
                assert!(Instant::now() < deadline, "{:?}", seen.lock().unwrap());
                std::thread::sleep(Duration::from_millis(10));
            }
            wait_latest(&seen, "t", Some(AgentStatus::NeedsYou));
            // In number order: emission may overtake, the frontend keeps the latest number.
            let mut v = seen.lock().unwrap().clone();
            v.sort_by_key(|e| e.2);
            assert_eq!(
                v.iter().map(|e| e.1).collect::<Vec<_>>(),
                vec![
                    Some(AgentStatus::NeedsYou),
                    None,
                    Some(AgentStatus::NeedsYou)
                ],
                "{v:?}"
            );
        }
    }

    /// A Terminal spawned under a Tab id already in use: the old one's clear is numbered
    /// before anything the new one reports, so the new badge stays.
    #[test]
    fn local_replacement_under_same_tab_keeps_its_badge() {
        for _ in 0..5 {
            let (f, _sock, seen) = prompting_fx();
            f.spawn("codex", None);
            wait_latest(&seen, "t", Some(AgentStatus::NeedsYou));
            let n = f.sink.text().matches("hook ").count();
            f.spawn("codex", None);
            let deadline = Instant::now() + T;
            while f.sink.text().matches("hook ").count() <= n {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            // The old Terminal's clear and the new one's prompt both arrive; the prompt wins.
            let deadline = Instant::now() + T;
            while !seen.lock().unwrap().iter().any(|e| e.1.is_none()) {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            wait_latest(&seen, "t", Some(AgentStatus::NeedsYou));
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(latest(&seen, "t"), Some(Some(AgentStatus::NeedsYou)));
            f.ptys.close("t");
            assert_eq!(latest(&seen, "t"), Some(None));
        }
    }
}

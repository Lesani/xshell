//! Permission Prompts (capability `agent.prompt`): one worker thread reads agent Terminals'
//! screen models, off every connection and PTY thread, and publishes the prompt in the
//! `terminals` list when it changed.
//!
//! A Terminal is read [`Config::prompt_settle`](super::Config) after its last trigger (it
//! became needs-you, or output arrived while it is needs-you or lists a prompt), at most four
//! settle periods after the first; at the end of the grace period of a needs-you episode
//! (for a text-only prompt); and [`Config::prompt_recheck`](super::Config) after an answer
//! or typing, for a prompt the TUI ignored.

use super::registry::Daemon;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Default)]
struct Job {
    /// The first trigger the settle waits for.
    first: Option<Instant>,
    settle_at: Option<Instant>,
    grace_at: Option<Instant>,
    recheck_at: Option<Instant>,
}

impl Job {
    fn due(&self) -> Option<Instant> {
        [self.settle_at, self.grace_at, self.recheck_at]
            .into_iter()
            .flatten()
            .min()
    }
}

#[derive(Default)]
struct Queue {
    jobs: HashMap<Uuid, Job>,
    stop: bool,
}

struct Inner {
    settle: Duration,
    recheck: Duration,
    queue: Mutex<Queue>,
    cv: Condvar,
    daemon: OnceLock<Weak<Daemon>>,
}

pub(crate) struct Prompts {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

fn lock(m: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Prompts {
    pub fn new(settle: Duration, recheck: Duration) -> Prompts {
        Prompts {
            inner: Arc::new(Inner {
                settle,
                recheck,
                queue: Mutex::new(Queue::default()),
                cv: Condvar::new(),
                daemon: OnceLock::new(),
            }),
            worker: Mutex::new(None),
        }
    }

    /// Serves `d` from now on: starts the worker. Requests made before are kept.
    pub fn bind(&self, d: Weak<Daemon>) {
        if self.inner.daemon.set(d).is_err() {
            return;
        }
        let inner = self.inner.clone();
        match std::thread::Builder::new()
            .name("prompts".into())
            .spawn(move || inner.run())
        {
            Ok(h) => *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(h),
            Err(e) => crate::log!("ERROR", "cannot start the prompt worker: {e}"),
        }
    }

    fn with_job(&self, terminal: Uuid, f: impl FnOnce(&mut Job, Instant)) {
        let mut q = lock(&self.inner.queue);
        if q.stop {
            return;
        }
        f(q.jobs.entry(terminal).or_default(), Instant::now());
        drop(q);
        self.inner.cv.notify_all();
    }

    /// Read `terminal`'s screen once it settles. Never blocks on I/O or on another lock: safe
    /// on any thread, also under the registry lock.
    pub fn request(&self, terminal: Uuid) {
        let settle = self.inner.settle;
        self.with_job(terminal, |j, now| {
            let first = *j.first.get_or_insert(now);
            j.settle_at = Some((now + settle).min(first + settle * 4));
        });
    }

    /// Read `terminal`'s screen at `at` (the end of a grace period).
    pub fn at(&self, terminal: Uuid, at: Instant) {
        self.with_job(terminal, |j, _| {
            j.grace_at = Some(j.grace_at.map_or(at, |g| g.min(at)));
        });
    }

    /// Recheck `terminal`'s screen after an answer or typing.
    pub fn recheck(&self, terminal: Uuid) {
        let recheck = self.inner.recheck;
        self.with_job(terminal, |j, now| j.recheck_at = Some(now + recheck));
    }

    /// Stops the worker and waits for it: a read in progress finishes first.
    pub fn stop(&self) {
        {
            let mut q = lock(&self.inner.queue);
            q.stop = true;
            q.jobs.clear();
        }
        self.inner.cv.notify_all();
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = worker {
            let _ = h.join();
        }
    }
}

impl Inner {
    fn run(&self) {
        let mut q = lock(&self.queue);
        loop {
            if q.stop {
                return;
            }
            let now = Instant::now();
            let due = q
                .jobs
                .iter()
                .filter(|(_, j)| j.due().is_some_and(|t| t <= now))
                .min_by_key(|(_, j)| j.due())
                .map(|(id, _)| *id);
            if let Some(id) = due {
                let mut recheck = false;
                if let Some(j) = q.jobs.get_mut(&id) {
                    let is_due = |t: &mut Option<Instant>| {
                        let d = t.is_some_and(|t| t <= now);
                        if d {
                            *t = None;
                        }
                        d
                    };
                    if is_due(&mut j.settle_at) {
                        j.first = None;
                    }
                    is_due(&mut j.grace_at);
                    recheck = is_due(&mut j.recheck_at);
                    if j.due().is_none() {
                        q.jobs.remove(&id);
                    }
                }
                drop(q);
                self.scan(id, recheck);
                q = lock(&self.queue);
                continue;
            }
            let next = q.jobs.values().filter_map(Job::due).min();
            q = match next {
                Some(t) => {
                    self.cv
                        .wait_timeout(q, t.saturating_duration_since(now))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.cv.wait(q).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    /// Read the listed Terminal's screen and commit the result to its prompt; publish the
    /// list when the prompt changed (persisting the id floor first when a new id was handed
    /// out), and only while the same Terminal is listed.
    fn scan(&self, id: Uuid, recheck: bool) {
        let Some(d) = self.daemon.get().and_then(Weak::upgrade) else {
            return;
        };
        let Some(t) = d.reg.lock().unwrap().terminals.get(&id).cloned() else {
            return;
        };
        let out = t.read_prompt(&d, recheck);
        if out.recheck_again {
            let mut q = lock(&self.queue);
            if !q.stop {
                q.jobs.entry(id).or_default().recheck_at = Some(Instant::now() + self.recheck);
            }
        }
        if let Some(at) = out.grace_at {
            let mut q = lock(&self.queue);
            if !q.stop {
                let j = q.jobs.entry(id).or_default();
                j.grace_at = Some(j.grace_at.map_or(at, |g| g.min(at)));
            }
        }
        let mut changed = out.changed;
        if let Some(floor) = out.reserve {
            // The new id's floor reaches the state file before the prompt can be listed or
            // answered; if it cannot, the prompt is dropped (the next read tries again). The
            // write holds no lock: a snapshot is taken under the registry lock, saved aside,
            // and confirmed under it again only while the Terminal is still the one listed.
            let snap = {
                let reg = d.reg.lock().unwrap();
                (d.is_current(&reg, &t) && !reg.frozen).then(|| d.snapshot(&reg))
            };
            let saved = snap.is_some_and(|snap| d.save_snapshot(id, snap).is_ok());
            let reg = d.reg.lock().unwrap();
            let ok = saved && d.is_current(&reg, &t) && !reg.frozen;
            changed |= t.confirm_prompt(floor, ok);
            if changed && ok {
                d.broadcast_terminals(&reg);
            }
            return;
        }
        if changed {
            let reg = d.reg.lock().unwrap();
            if d.is_current(&reg, &t) && !reg.frozen {
                d.broadcast_terminals(&reg);
            }
        }
    }
}

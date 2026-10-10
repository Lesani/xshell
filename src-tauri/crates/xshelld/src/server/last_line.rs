//! The last line of agent Terminals (capability `agent.last-line`): one worker thread reads
//! it from the agent's session file, off every connection and PTY thread, and publishes it in
//! the `terminals` list when it changed.
//!
//! A request reads now and once more after [`Config::last_line_retry`](super::Config), which
//! covers a transcript written after the hook that triggered the read. Requests for a
//! Terminal coalesce while one is pending. Each read first checks and links a session a Codex
//! hook reported (`codex_link`), so that check runs here too, off the connection threads.

use super::registry::Daemon;
use super::TestPoint;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Default)]
struct Job {
    /// Read as soon as the worker is free.
    now: bool,
    /// Read again at this time.
    retry_at: Option<Instant>,
}

#[derive(Default)]
struct Queue {
    jobs: HashMap<Uuid, Job>,
    stop: bool,
}

struct Inner {
    retry: Duration,
    queue: Mutex<Queue>,
    cv: Condvar,
    daemon: OnceLock<Weak<Daemon>>,
}

pub(crate) struct LastLines {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

fn lock(m: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LastLines {
    pub fn new(retry: Duration) -> LastLines {
        LastLines {
            inner: Arc::new(Inner {
                retry,
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
            .name("last-line".into())
            .spawn(move || inner.run())
        {
            Ok(h) => *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(h),
            Err(e) => crate::log!("ERROR", "cannot start the last-line worker: {e}"),
        }
    }

    /// Read `terminal`'s last line now and once more after the retry delay. Never blocks
    /// on I/O or on another lock: safe on any thread, also under the registry lock.
    pub fn request(&self, terminal: Uuid) {
        let mut q = lock(&self.inner.queue);
        if q.stop {
            return;
        }
        let job = q.jobs.entry(terminal).or_default();
        job.now = true;
        job.retry_at = Some(Instant::now() + self.inner.retry);
        drop(q);
        self.inner.cv.notify_all();
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
                .find(|(_, j)| j.now)
                .or_else(|| {
                    q.jobs
                        .iter()
                        .find(|(_, j)| j.retry_at.is_some_and(|t| t <= now))
                })
                .map(|(id, _)| *id);
            if let Some(id) = due {
                if let Some(j) = q.jobs.get_mut(&id) {
                    if j.now {
                        j.now = false;
                    } else {
                        j.retry_at = None;
                    }
                    if !j.now && j.retry_at.is_none() {
                        q.jobs.remove(&id);
                    }
                }
                drop(q);
                self.read(id);
                q = lock(&self.queue);
                continue;
            }
            let next = q.jobs.values().filter_map(|j| j.retry_at).min();
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

    /// Read the listed Terminal's last line without any lock held, then store and publish it
    /// only if the same Terminal is still listed on the same session and agent: a line read
    /// for a session a `term.update` has since replaced is dropped.
    fn read(&self, id: Uuid) {
        let Some(d) = self.daemon.get().and_then(Weak::upgrade) else {
            return;
        };
        let Some(t) = d.reg.lock().unwrap().terminals.get(&id).cloned() else {
            return;
        };
        // A session the agent reported is linked first, so its line is the one read.
        super::codex_link::resolve(&d, &t);
        let spec = t.spec();
        let line = xshell_core::last_line::last_line(&d.ctx, &spec);
        d.test_point(id, TestPoint::LastLineRead);
        let reg = d.reg.lock().unwrap();
        if !d.is_current(&reg, &t) {
            return;
        }
        let current = t.spec();
        if current.session_id != spec.session_id
            || current.direct_agent() != spec.direct_agent()
            || current.cwd != spec.cwd
        {
            return;
        }
        if t.set_last_line(line) && !reg.frozen {
            d.broadcast_terminals(&reg);
        }
    }
}

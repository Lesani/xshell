//! A cancellation token per supervisor run. Every phase checks it, every child process and
//! download registers a hook on it, so stopping a Host interrupts whatever it is doing.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct St {
    cancelled: bool,
    next: u64,
    hooks: HashMap<u64, Hook>,
}

#[derive(Default)]
struct Inner {
    st: Mutex<St>,
    cv: Condvar,
}

#[derive(Clone, Default)]
pub struct CancelToken(Arc<Inner>);

/// Removes its hook when dropped (the work finished without being cancelled).
pub struct CancelHook {
    token: CancelToken,
    id: Option<u64>,
}

impl Drop for CancelHook {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.token.0.st.lock().unwrap().hooks.remove(&id);
        }
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs every hook (outside the lock) and wakes every sleeper. Idempotent.
    pub fn cancel(&self) {
        let hooks: Vec<Hook> = {
            let mut st = self.0.st.lock().unwrap();
            st.cancelled = true;
            st.hooks.drain().map(|(_, h)| h).collect()
        };
        self.0.cv.notify_all();
        for h in hooks {
            h();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.st.lock().unwrap().cancelled
    }

    /// Run `f` on cancel, or right now when already cancelled.
    pub fn on_cancel(&self, f: impl FnOnce() + Send + 'static) -> CancelHook {
        let mut st = self.0.st.lock().unwrap();
        if st.cancelled {
            drop(st);
            f();
            return CancelHook {
                token: self.clone(),
                id: None,
            };
        }
        st.next += 1;
        let id = st.next;
        st.hooks.insert(id, Box::new(f));
        CancelHook {
            token: self.clone(),
            id: Some(id),
        }
    }

    /// Sleep for `d` unless cancelled first. Returns whether it was cancelled.
    pub fn sleep(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut st = self.0.st.lock().unwrap();
        loop {
            if st.cancelled {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.0.cv.wait_timeout(st, left).unwrap().0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn hooks_run_once_and_unregister() {
        let t = CancelToken::new();
        let n = Arc::new(AtomicUsize::new(0));
        let n1 = n.clone();
        let kept = t.on_cancel(move || {
            n1.fetch_add(1, Ordering::SeqCst);
        });
        let n2 = n.clone();
        drop(t.on_cancel(move || {
            n2.fetch_add(10, Ordering::SeqCst);
        }));
        t.cancel();
        t.cancel();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        drop(kept);
        let n3 = n.clone();
        let _late = t.on_cancel(move || {
            n3.fetch_add(100, Ordering::SeqCst);
        });
        assert_eq!(n.load(Ordering::SeqCst), 101);
    }

    #[test]
    fn sleep_is_interrupted() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let start = Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            t2.cancel();
        });
        assert!(t.sleep(Duration::from_secs(10)));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!CancelToken::new().sleep(Duration::from_millis(1)));
    }
}

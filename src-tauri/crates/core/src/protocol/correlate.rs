use std::collections::HashMap;

/// Matches responses to requests by id. Runtime-free: the waiter can be a oneshot sender, an
/// mpsc sender or anything else.
#[derive(Debug)]
pub struct Correlator<T> {
    next: u64,
    pending: HashMap<u64, T>,
}

impl<T> Default for Correlator<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Correlator<T> {
    pub fn new() -> Self {
        Self {
            next: 1,
            pending: HashMap::new(),
        }
    }

    /// A fresh id for `waiter`. Ids start at 1, skip 0 on wrap-around and never collide with
    /// one still pending.
    pub fn register(&mut self, waiter: T) -> u64 {
        loop {
            let id = self.next;
            self.next = self.next.wrapping_add(1);
            if id != 0 && !self.pending.contains_key(&id) {
                self.pending.insert(id, waiter);
                return id;
            }
        }
    }

    pub fn complete(&mut self, id: u64) -> Option<T> {
        self.pending.remove(&id)
    }

    /// Every pending waiter, e.g. to fail them all on disconnect.
    pub fn drain(&mut self) -> Vec<T> {
        self.pending.drain().map(|(_, w)| w).collect()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_complete() {
        let mut c = Correlator::new();
        let a = c.register("a");
        let b = c.register("b");
        assert!(a >= 1 && b > a);
        assert_eq!(c.complete(a), Some("a"));
        assert_eq!(c.complete(a), None);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn wraps_without_zero_or_collision() {
        let mut c = Correlator::new();
        c.next = u64::MAX;
        let a = c.register(1);
        let b = c.register(2);
        assert_eq!(a, u64::MAX);
        assert_eq!(b, 1);
        c.next = u64::MAX;
        assert_eq!(c.register(3), 2);
    }

    #[test]
    fn drain() {
        let mut c = Correlator::new();
        c.register(1);
        c.register(2);
        let mut all = c.drain();
        all.sort();
        assert_eq!(all, vec![1, 2]);
        assert!(c.is_empty());
    }
}

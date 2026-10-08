//! A Terminal's size follows the connection that last sent it input or a resize.

use super::ConnId;
use std::collections::HashMap;

#[derive(Debug)]
pub(crate) struct SizeArbiter {
    owner: Option<ConnId>,
    current: (u16, u16),
    wanted: HashMap<ConnId, (u16, u16)>,
}

impl SizeArbiter {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            owner: None,
            current: (cols, rows),
            wanted: HashMap::new(),
        }
    }

    /// `(cols, rows)` currently applied to the PTY.
    pub fn current(&self) -> (u16, u16) {
        self.current
    }

    fn apply(&mut self, size: (u16, u16)) -> Option<(u16, u16)> {
        if size == self.current {
            None
        } else {
            self.current = size;
            Some(size)
        }
    }

    /// `c` resized its view: it becomes the owner. Returns the size to apply, if it changed.
    pub fn on_resize(&mut self, c: ConnId, cols: u16, rows: u16) -> Option<(u16, u16)> {
        self.owner = Some(c);
        self.wanted.insert(c, (cols, rows));
        self.apply((cols, rows))
    }

    /// `c` typed: it becomes the owner, and its last known size applies if it differs.
    pub fn on_input(&mut self, c: ConnId) -> Option<(u16, u16)> {
        self.owner = Some(c);
        let w = *self.wanted.get(&c)?;
        self.apply(w)
    }

    /// `c` detached or disconnected. The current size stays.
    pub fn forget(&mut self, c: ConnId) {
        self.wanted.remove(&c);
        if self.owner == Some(c) {
            self.owner = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_interactor_wins() {
        let mut s = SizeArbiter::new(80, 24);
        assert_eq!(s.on_resize(1, 100, 30), Some((100, 30)));
        assert_eq!(s.on_resize(2, 80, 20), Some((80, 20)));
        assert_eq!(s.on_input(2), None);
        assert_eq!(s.on_input(1), Some((100, 30)));
        assert_eq!(s.on_input(1), None);
        s.forget(1);
        assert_eq!(s.current(), (100, 30));
        assert_eq!(s.on_input(1), None);
        assert_eq!(s.on_input(3), None);
        assert_eq!(s.on_input(2), Some((80, 20)));
    }
}

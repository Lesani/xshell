//! A Terminal's size follows the connection that last interacted with it. A Desktop interacts
//! by typing or resizing its view; a Mobile only by typing into a view it sized (its Terminal
//! View): its `term.resize` records the size it wants, applied once it types (or at once while
//! it already holds the size), and typing without one (its Chat View) takes nothing. When a
//! Mobile that holds the size leaves, the size goes back to the Desktop that held it most
//! recently and is still attached.

use super::ConnId;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub(crate) struct SizeArbiter {
    owner: Option<ConnId>,
    current: (u16, u16),
    wanted: HashMap<ConnId, (u16, u16)>,
    /// The connections known here that are Mobiles.
    mobiles: HashSet<ConnId>,
    /// When each connection last took (or kept) the size, as a sequence number.
    owned_at: HashMap<ConnId, u64>,
    seq: u64,
}

impl SizeArbiter {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            owner: None,
            current: (cols, rows),
            wanted: HashMap::new(),
            mobiles: HashSet::new(),
            owned_at: HashMap::new(),
            seq: 0,
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

    fn note_role(&mut self, c: ConnId, mobile: bool) {
        if mobile {
            self.mobiles.insert(c);
        } else {
            self.mobiles.remove(&c);
        }
    }

    fn take(&mut self, c: ConnId) {
        self.owner = Some(c);
        self.seq += 1;
        self.owned_at.insert(c, self.seq);
    }

    /// `c` resized its view. A Desktop becomes the owner; a Mobile only records the size,
    /// applied now if it already owns the size. Returns the size to apply, if it changed.
    pub fn on_resize(
        &mut self,
        c: ConnId,
        cols: u16,
        rows: u16,
        mobile: bool,
    ) -> Option<(u16, u16)> {
        self.note_role(c, mobile);
        self.wanted.insert(c, (cols, rows));
        if mobile && self.owner != Some(c) {
            return None;
        }
        self.take(c);
        self.apply((cols, rows))
    }

    /// `c` typed: it becomes the owner, and its last known size applies if it differs. A
    /// Mobile that never sized a view of this Terminal (a reply from its Chat View) types
    /// without taking the size.
    pub fn on_input(&mut self, c: ConnId, mobile: bool) -> Option<(u16, u16)> {
        let w = self.wanted.get(&c).copied();
        if mobile && w.is_none() {
            return None;
        }
        self.note_role(c, mobile);
        self.take(c);
        self.apply(w?)
    }

    /// How many connections have a size recorded here.
    pub fn tracked(&self) -> usize {
        self.wanted.len() + usize::from(self.owner.is_some_and(|o| !self.wanted.contains_key(&o)))
    }

    /// `c` detached or disconnected. The current size stays, unless `c` is a Mobile that owns
    /// it: then the Desktop still known here that owned it most recently, with a recorded
    /// size, owns it again. Returns the size to apply, if it changed.
    pub fn forget(&mut self, c: ConnId) -> Option<(u16, u16)> {
        let mobile_owner = self.owner == Some(c) && self.mobiles.contains(&c);
        self.wanted.remove(&c);
        self.mobiles.remove(&c);
        self.owned_at.remove(&c);
        if self.owner != Some(c) {
            return None;
        }
        self.owner = None;
        if !mobile_owner {
            return None;
        }
        let back = self
            .owned_at
            .iter()
            .filter(|(d, _)| !self.mobiles.contains(d) && self.wanted.contains_key(d))
            .max_by_key(|(_, at)| **at)
            .map(|(d, _)| *d)?;
        self.take(back);
        let w = self.wanted[&back];
        self.apply(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESK: bool = false;
    const MOB: bool = true;

    #[test]
    fn last_interactor_wins() {
        let mut s = SizeArbiter::new(80, 24);
        assert_eq!(s.on_resize(1, 100, 30, DESK), Some((100, 30)));
        assert_eq!(s.on_resize(2, 80, 20, DESK), Some((80, 20)));
        assert_eq!(s.on_input(2, DESK), None);
        assert_eq!(s.on_input(1, DESK), Some((100, 30)));
        assert_eq!(s.on_input(1, DESK), None);
        assert_eq!(s.forget(1), None);
        assert_eq!(s.current(), (100, 30));
        assert_eq!(s.on_input(1, DESK), None);
        assert_eq!(s.on_input(3, DESK), None);
        assert_eq!(s.on_input(2, DESK), Some((80, 20)));
    }

    /// A Desktop (1) at 100×30 and a Mobile (9).
    fn desk_and_mobile() -> SizeArbiter {
        let mut s = SizeArbiter::new(80, 24);
        assert_eq!(s.on_resize(1, 100, 30, DESK), Some((100, 30)));
        s
    }

    #[test]
    fn mobile_resize_records_without_claiming() {
        let mut s = desk_and_mobile();
        assert_eq!(s.on_resize(9, 40, 20, MOB), None);
        assert_eq!(s.current(), (100, 30));
        assert_eq!(s.tracked(), 2);
    }

    #[test]
    fn mobile_input_claims_recorded_size() {
        let mut s = desk_and_mobile();
        s.on_resize(9, 40, 20, MOB);
        assert_eq!(s.on_input(9, MOB), Some((40, 20)));
        assert_eq!(s.current(), (40, 20));
    }

    #[test]
    fn mobile_resize_while_owner_applies() {
        let mut s = desk_and_mobile();
        s.on_resize(9, 40, 20, MOB);
        s.on_input(9, MOB);
        // The keyboard opened.
        assert_eq!(s.on_resize(9, 40, 12, MOB), Some((40, 12)));
    }

    #[test]
    fn desktop_input_takes_size_back() {
        let mut s = desk_and_mobile();
        s.on_resize(9, 40, 20, MOB);
        s.on_input(9, MOB);
        assert_eq!(s.on_input(1, DESK), Some((100, 30)));
        // The Mobile resizing again is recorded only.
        assert_eq!(s.on_resize(9, 41, 20, MOB), None);
        assert_eq!(s.current(), (100, 30));
    }

    #[test]
    fn owning_mobile_forget_restores_last_desktop() {
        let mut s = desk_and_mobile();
        s.on_resize(9, 40, 20, MOB);
        s.on_input(9, MOB);
        assert_eq!(s.forget(9), Some((100, 30)));
        assert_eq!(s.current(), (100, 30));
        // The Desktop owns it again: its own resize applies, a later Mobile's does not.
        assert_eq!(s.on_resize(8, 30, 10, MOB), None);
        assert_eq!(s.on_resize(1, 120, 40, DESK), Some((120, 40)));
    }

    #[test]
    fn non_owner_forget_changes_nothing() {
        let mut s = desk_and_mobile();
        s.on_resize(9, 40, 20, MOB);
        assert_eq!(s.forget(9), None);
        assert_eq!(s.current(), (100, 30));
        // A Mobile that typed and then lost the size to the Desktop.
        s.on_resize(9, 40, 20, MOB);
        s.on_input(9, MOB);
        s.on_input(1, DESK);
        assert_eq!(s.forget(9), None);
        assert_eq!(s.current(), (100, 30));
        assert_eq!(s.tracked(), 1);
    }

    #[test]
    fn forget_restores_nothing_when_that_desktop_is_gone() {
        let mut s = desk_and_mobile();
        s.on_resize(9, 40, 20, MOB);
        s.on_input(9, MOB);
        assert_eq!(s.forget(1), None);
        assert_eq!(s.forget(9), None);
        assert_eq!(s.current(), (40, 20));
        assert_eq!(s.tracked(), 0);
    }

    /// Two Desktops (1, 2) and two Mobiles (8, 9): the size goes back to the Desktop that
    /// held it most recently, by input as well as by resize.
    #[test]
    fn restores_most_recent_desktop_owner() {
        let mut s = SizeArbiter::new(80, 24);
        s.on_resize(1, 100, 30, DESK);
        s.on_resize(2, 120, 40, DESK);
        s.on_resize(8, 30, 15, MOB);
        s.on_resize(9, 40, 20, MOB);
        assert_eq!(s.on_input(1, DESK), Some((100, 30)));
        assert_eq!(s.on_input(9, MOB), Some((40, 20)));
        assert_eq!(s.forget(9), Some((100, 30)));
        // Then the newest Desktop owner disconnects: the other one is next.
        s.on_input(2, DESK);
        s.on_input(8, MOB);
        assert_eq!(s.current(), (30, 15));
        assert_eq!(s.forget(2), None);
        assert_eq!(s.forget(8), Some((100, 30)));
    }

    /// A Mobile typing without a view of the Terminal (its Chat View) takes nothing: a view
    /// it opens later still only records its size.
    #[test]
    fn mobile_input_without_a_view_never_claims() {
        let mut s = desk_and_mobile();
        assert_eq!(s.on_input(9, MOB), None);
        assert_eq!(s.on_resize(9, 40, 20, MOB), None);
        assert_eq!(s.current(), (100, 30));
        assert_eq!(s.forget(9), None);
        assert_eq!(s.current(), (100, 30));
        assert_eq!(s.tracked(), 1);
    }

    /// A Desktop that only typed has no size recorded: it is never restored to.
    #[test]
    fn desktop_without_recorded_size_is_skipped() {
        let mut s = SizeArbiter::new(80, 24);
        s.on_resize(1, 100, 30, DESK);
        s.on_input(2, DESK);
        s.on_resize(9, 40, 20, MOB);
        s.on_input(9, MOB);
        assert_eq!(s.forget(9), Some((100, 30)));
    }
}

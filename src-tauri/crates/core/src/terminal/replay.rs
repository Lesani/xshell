use std::collections::VecDeque;

pub const DEFAULT_REPLAY_CAPACITY: usize = 2 * 1024 * 1024;
/// RIS (full reset), the same reset the Desktop's local overflow notice starts with.
pub const RESET: &[u8] = b"\x1bc";

/// The most recent output of a Terminal, replayed to a Tab when it attaches.
#[derive(Debug)]
pub struct ReplayBuffer {
    buf: VecDeque<u8>,
    cap: usize,
    truncated: bool,
}

impl ReplayBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
            truncated: false,
        }
    }

    /// Append output, dropping the oldest bytes beyond the capacity.
    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.cap {
            self.truncated |= !self.buf.is_empty() || bytes.len() > self.cap;
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let over = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        if over > 0 {
            self.buf.drain(..over);
            self.truncated = true;
        }
        self.buf.extend(bytes);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// `RESET` followed by a tail that never starts mid-escape or mid-character: the whole
    /// buffer while nothing was dropped, else from just after the first newline, else from
    /// the first ESC, else nothing (the redraw nudge repaints full-screen TUIs).
    pub fn snapshot(&self) -> Vec<u8> {
        self.snapshot_tail(usize::MAX)
    }

    /// [`ReplayBuffer::snapshot`] of at most the last `max` bytes: `RESET` followed by at most
    /// `max` bytes. When the window cuts the buffer, it is trimmed by the same rules (after
    /// the first newline in the window, else from its first ESC, else nothing).
    pub fn snapshot_tail(&self, max: usize) -> Vec<u8> {
        let from = self.buf.len().saturating_sub(max);
        let window = self.buf.range(from..);
        let mut out = Vec::with_capacity(RESET.len() + window.len());
        out.extend_from_slice(RESET);
        let start = if !self.truncated && from == 0 {
            Some(0)
        } else {
            window
                .clone()
                .position(|&b| b == b'\n')
                .map(|i| i + 1)
                .or_else(|| window.clone().position(|&b| b == 0x1b))
        };
        if let Some(start) = start {
            out.extend(self.buf.range(from + start..));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(r: &[u8]) -> Vec<u8> {
        let mut v = RESET.to_vec();
        v.extend_from_slice(r);
        v
    }

    #[test]
    fn untruncated_is_whole_with_reset() {
        let mut b = ReplayBuffer::new(64);
        b.push(b"a\nb");
        assert_eq!(b.snapshot(), with(b"a\nb"));
    }

    #[test]
    fn truncated_starts_after_newline() {
        let mut b = ReplayBuffer::new(8);
        b.push(b"xx\x1b[31mAB\nCD");
        assert_eq!(b.snapshot(), with(b"CD"));
    }

    #[test]
    fn truncated_no_newline_starts_at_esc() {
        let mut b = ReplayBuffer::new(8);
        b.push(b"zz");
        b.push(b"\x1b[2Jqqqq");
        assert_eq!(b.snapshot(), with(b"\x1b[2Jqqqq"));
    }

    #[test]
    fn truncated_nothing_safe() {
        let mut b = ReplayBuffer::new(4);
        b.push(b"abcdefgh");
        assert_eq!(b.snapshot(), RESET);
    }

    #[test]
    fn capacity_never_exceeded() {
        let cap = 2 * 1024 * 1024;
        let mut b = ReplayBuffer::new(cap);
        let mut all = Vec::new();
        // Deterministic pseudo-random chunk sizes (xorshift).
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut n: u8 = 0;
        while all.len() < 10 * 1024 * 1024 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 100_000) as usize + 1;
            let chunk: Vec<u8> = (0..len)
                .map(|_| {
                    n = n.wrapping_add(1);
                    n
                })
                .collect();
            b.push(&chunk);
            all.extend_from_slice(&chunk);
            assert!(b.len() <= cap);
        }
        let tail: Vec<u8> = b.buf.iter().copied().collect();
        assert_eq!(tail, all[all.len() - cap..]);
    }

    #[test]
    fn chunk_larger_than_cap() {
        let mut b = ReplayBuffer::new(4);
        b.push(b"0123456789ab");
        assert_eq!(b.buf.iter().copied().collect::<Vec<_>>(), b"89ab");
        assert!(b.truncated);
    }

    #[test]
    fn snapshot_tail_caps_and_cuts_at_newline() {
        let mut b = ReplayBuffer::new(64);
        b.push(b"line one\nline two\nline three");
        // Fits: the same as the full snapshot.
        assert_eq!(b.snapshot_tail(1000), b.snapshot());
        assert_eq!(b.snapshot_tail(b.len()), b.snapshot());
        // Cut: after the first newline inside the last 16 bytes ("two\nline three").
        let t = b.snapshot_tail(16);
        assert_eq!(t, with(b"line three"));
        assert!(t.len() <= RESET.len() + 16);
        // No newline in the window: from its first ESC.
        let mut b = ReplayBuffer::new(64);
        b.push(b"aaaa\nbbbb\x1b[1mcccc");
        assert_eq!(b.snapshot_tail(10), with(b"\x1b[1mcccc"));
        // Neither: RESET alone.
        assert_eq!(b.snapshot_tail(4), RESET);
        // A truncated buffer is cut by the window when it is the tighter bound.
        let mut b = ReplayBuffer::new(8);
        b.push(b"xx\nyy\nzzzzzzz");
        assert_eq!(b.snapshot_tail(100), b.snapshot());
        assert_eq!(b.snapshot_tail(4), RESET);
    }

    #[test]
    fn exact_fill_is_not_truncated() {
        let mut b = ReplayBuffer::new(4);
        b.push(b"abcd");
        assert_eq!(b.snapshot(), with(b"abcd"));
    }
}

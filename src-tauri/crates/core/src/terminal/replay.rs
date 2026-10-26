use std::collections::VecDeque;

pub const DEFAULT_REPLAY_CAPACITY: usize = 2 * 1024 * 1024;
/// RIS (full reset), the same reset the Desktop's local overflow notice starts with.
pub const RESET: &[u8] = b"\x1bc";
pub const BRACKETED_PASTE_ENABLE: &[u8] = b"\x1b[?2004h";

#[derive(Debug, Default)]
struct PasteMode(bool);

impl vte::Perform for PasteMode {
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if !ignore && intermediates == b"!" && action == 'p' {
            // xterm's soft reset (DECSTR) clears DEC2004 too.
            self.0 = false;
        } else if !ignore
            && intermediates == b"?"
            && matches!(action, 'h' | 'l')
            && params.iter().any(|p| p == [2004])
        {
            self.0 = action == 'h';
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.0 = false;
        }
    }
}

/// The most recent output of a Terminal, replayed to a Tab when it attaches.
pub struct ReplayBuffer {
    buf: VecDeque<u8>,
    cap: usize,
    truncated: bool,
    parser: vte::Parser,
    paste_mode: PasteMode,
}

impl std::fmt::Debug for ReplayBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayBuffer")
            .field("buf", &self.buf)
            .field("cap", &self.cap)
            .field("truncated", &self.truncated)
            .field("paste_mode", &self.paste_mode)
            .finish_non_exhaustive()
    }
}

impl ReplayBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
            truncated: false,
            parser: vte::Parser::new(),
            paste_mode: PasteMode::default(),
        }
    }

    /// Append output, dropping the oldest bytes beyond the capacity.
    pub fn push(&mut self, bytes: &[u8]) {
        // Only original program output enters this parser, before any content is evicted.
        self.parser.advance(&mut self.paste_mode, bytes);
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

    pub fn bracketed_paste(&self) -> bool {
        self.paste_mode.0
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// `RESET`, optional DEC2004 restoration, then a tail: the whole
    /// buffer while nothing was dropped, else from just after the first newline, else from
    /// the first ESC, else nothing (the redraw nudge repaints full-screen TUIs).
    pub fn snapshot(&self) -> Vec<u8> {
        self.snapshot_tail(usize::MAX)
    }

    /// [`ReplayBuffer::snapshot`] of at most the last `max` content bytes. The synthetic
    /// prefix adds at most `RESET.len() + BRACKETED_PASTE_ENABLE.len()` bytes, independently
    /// of retained-content capacity. Restore immediately after RIS, never after the tail:
    /// its final escape sequence or UTF-8 character may need live output to complete it.
    /// When the window cuts the buffer, it is trimmed by the same rules (after
    /// the first newline in the window, else from its first ESC, else nothing).
    pub fn snapshot_tail(&self, max: usize) -> Vec<u8> {
        let from = self.buf.len().saturating_sub(max);
        let window = self.buf.range(from..);
        let mut out = Vec::with_capacity(RESET.len() + BRACKETED_PASTE_ENABLE.len() + window.len());
        out.extend_from_slice(RESET);
        if self.bracketed_paste() {
            out.extend_from_slice(BRACKETED_PASTE_ENABLE);
        }
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

    fn screen(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn evicted_enable_and_short_tail_preserve_paste_mode() {
        for cap in [32, 256] {
            let mut b = ReplayBuffer::new(cap);
            b.push(b"\x1b[?2004h");
            b.push(b"discarded line 00000000000000000000000000000000000000000000000000000000000000000000\nkept");
            for snap in [b.snapshot(), b.snapshot_tail(16)] {
                assert!(screen(&snap).screen().bracketed_paste());
                assert!(snap.ends_with(b"kept"));
                assert!(snap.len() <= RESET.len() + 8 + cap);
            }
        }
    }

    #[test]
    fn snapshot_live_continuation_does_not_interrupt_original_sequences() {
        for short in [false, true] {
            for (pending, continuation, enabled, text) in [
                (&b"\x1b[?2004"[..], &b"l"[..], false, "kept"),
                (&b"\x1b[31"[..], &b"mred"[..], true, "keptred"),
                (&b"\xc3"[..], &b"\xa4"[..], true, "keptä"),
            ] {
                let mut b = ReplayBuffer::new(64);
                b.push(b"\x1b[?2004h");
                b.push(&[b'x'; 128]);
                b.push(b"\nkept");
                b.push(pending);
                let snap = if short {
                    b.snapshot_tail(24)
                } else {
                    b.snapshot()
                };
                let mut p = screen(&snap);
                p.process(continuation);
                assert_eq!(p.screen().bracketed_paste(), enabled);
                assert_eq!(p.screen().contents(), text);
                if continuation == b"mred" {
                    assert_eq!(
                        p.screen().cell(0, 4).unwrap().fgcolor(),
                        vt100::Color::Idx(1)
                    );
                }
            }
            // Conversely, an incomplete enable must complete after a disabled snapshot.
            let mut b = ReplayBuffer::new(32);
            b.push(b"old line\nkept\x1b[?2004");
            let snap = if short {
                b.snapshot_tail(16)
            } else {
                b.snapshot()
            };
            let mut p = screen(&snap);
            p.process(b"h");
            assert!(p.screen().bracketed_paste());
        }
    }

    #[test]
    fn original_mode_changes_reset_and_string_payloads_are_respected() {
        for (bytes, enabled) in [
            (&b"\x1b[?1;2004;25h"[..], true),
            (&b"\x1b[?2004h\x1b[?25;2004l"[..], false),
            (&b"\x1b[?2004h\x1bc"[..], false),
            (&b"\x1bc\x1b[?2004h"[..], true),
            (&b"\x1b]0;fake [ ?2004h\x07"[..], false),
            (&b"\x1bPfake ?2004h\x1b\\"[..], false),
        ] {
            let mut b = ReplayBuffer::new(24);
            // Bytewise parsing exercises split ESC, CSI and parameters.
            for byte in bytes {
                b.push(&[*byte]);
            }
            assert_eq!(screen(&b.snapshot()).screen().bracketed_paste(), enabled);
            b.push(&[b'x'; 128]);
            assert_eq!(screen(&b.snapshot()).screen().bracketed_paste(), enabled);
            assert_eq!(
                screen(&b.snapshot_tail(4)).screen().bracketed_paste(),
                enabled
            );
        }
    }

    #[test]
    fn decstr_clears_paste_mode_before_eviction_and_short_tail() {
        // xterm 5.5 softReset resets DEC2004; vt100 does not implement DECSTR, so
        // check tracked state and exact replay bytes against xterm's disabled default.
        let mut b = ReplayBuffer::new(32);
        b.push(BRACKETED_PASTE_ENABLE);
        for chunk in [b"\x1b".as_slice(), b"[", b"!"] {
            b.push(chunk);
            assert!(b.bracketed_paste());
        }
        b.push(b"p");
        assert!(!b.bracketed_paste());
        // The short tail loses both original controls even without buffer eviction.
        b.push(b"discarded\nkept");
        assert_eq!(b.snapshot_tail(8), with(b"kept"));
        // Eviction must not resurrect the preceding enable either.
        b.push(&[b'x'; 64]);
        b.push(b"\nkept");
        assert_eq!(b.snapshot(), with(b"kept"));
        assert_eq!(b.snapshot_tail(8), with(b"kept"));
        b.push(BRACKETED_PASTE_ENABLE);
        assert!(b.bracketed_paste());
    }

    #[test]
    fn large_single_write_observes_enable_before_eviction() {
        let mut b = ReplayBuffer::new(16);
        let mut bytes = b"\x1b[?2004h".to_vec();
        bytes.extend_from_slice(&[b'x'; 128]);
        bytes.extend_from_slice(b"\nkept");
        b.push(&bytes);
        assert!(screen(&b.snapshot()).screen().bracketed_paste());
        assert_eq!(b.len(), 16);
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

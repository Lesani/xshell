//! Per-connection send queue and its writer thread. Producers (PTY readers, call threads,
//! the connection reader) never block on a socket: they enqueue, and the writer drains.
//!
//! Slow-consumer policy: queued Terminal output is capped; on overflow it is dropped and
//! replaced by `OVERFLOW_NOTICE` (a reset), and the affected Terminals are nudged to redraw.
//! Control frames are never dropped; if they alone exceed the total cap, the peer is
//! disconnected. A peer that makes no write progress for `write_stall_timeout` is dropped.

use super::transport::Stream;
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::net::Shutdown;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::terminal::OVERFLOW_NOTICE;
use xshell_protocol::frame::KIND_OUTPUT;

const WRITE_BUF: usize = 256 * 1024;
/// Adjacent output chunks for one Terminal are merged into frames of at most this size.
const MERGE_MAX: usize = 1024 * 1024;

pub(crate) enum Out {
    /// Responses, `hello`, `term.exit`, `error`: never dropped.
    Control(Arc<[u8]>),
    /// A `terminals` list: only the newest queued one is kept.
    Terminals(Arc<[u8]>),
    /// Raw Terminal output, framed by the writer.
    Output { terminal: Uuid, data: Arc<[u8]> },
}

impl Out {
    fn len(&self) -> usize {
        match self {
            Out::Control(f) | Out::Terminals(f) => f.len(),
            Out::Output { data, .. } => data.len(),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum State {
    Open,
    /// Flush what is queued, then shut the socket down.
    Closing,
    /// Drop everything; the socket is already shut down.
    Aborted,
}

struct Inner {
    q: VecDeque<Out>,
    output_bytes: usize,
    total_bytes: usize,
    state: State,
    writer_done: bool,
}

pub(crate) struct Outbox {
    inner: Mutex<Inner>,
    cv: Condvar,
    output_cap: usize,
    total_cap: usize,
    /// A handle on the connection's socket, to cut a writer stuck on a dead peer.
    sock: Option<Stream>,
}

impl Outbox {
    pub fn new(output_cap: usize, total_cap: usize, sock: Option<Stream>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                q: VecDeque::new(),
                output_bytes: 0,
                total_bytes: 0,
                state: State::Open,
                writer_done: false,
            }),
            cv: Condvar::new(),
            output_cap,
            total_cap,
            sock,
        })
    }

    fn abort_locked(&self, g: &mut Inner) {
        if g.state != State::Aborted {
            g.state = State::Aborted;
            g.q.clear();
            g.output_bytes = 0;
            g.total_bytes = 0;
            if let Some(s) = &self.sock {
                let _ = s.shutdown(Shutdown::Both);
            }
            self.cv.notify_all();
        }
    }

    fn push_locked(&self, g: &mut Inner, item: Out) -> bool {
        if g.state != State::Open {
            return false;
        }
        if g.total_bytes + item.len() > self.total_cap {
            crate::log!("WARN", "connection send queue over its cap; disconnecting");
            self.abort_locked(g);
            return false;
        }
        g.total_bytes += item.len();
        if let Out::Output { .. } = item {
            g.output_bytes += item.len();
        }
        g.q.push_back(item);
        self.cv.notify_all();
        true
    }

    /// Queue a control frame. `false` if the connection is closed (or was just closed for
    /// exceeding the total cap).
    pub fn push_control(&self, f: Arc<[u8]>) -> bool {
        let mut g = self.inner.lock().unwrap();
        self.push_locked(&mut g, Out::Control(f))
    }

    /// Queue a `terminals` list, replacing a list still waiting in the queue. The new one
    /// goes to the end so it never overtakes an event it already reflects.
    pub fn push_terminals(&self, f: Arc<[u8]>) {
        let mut g = self.inner.lock().unwrap();
        if g.state != State::Open {
            return;
        }
        if let Some(i) = g.q.iter().position(|o| matches!(o, Out::Terminals(_))) {
            let old = g.q.remove(i).unwrap();
            g.total_bytes -= old.len();
        }
        self.push_locked(&mut g, Out::Terminals(f));
    }

    /// Queue Terminal output; never blocks. Returns the Terminals whose queued output was
    /// dropped to make room (each gets `OVERFLOW_NOTICE` first), so the caller can nudge them.
    pub fn push_output(&self, terminal: Uuid, data: Arc<[u8]>) -> Vec<Uuid> {
        let mut g = self.inner.lock().unwrap();
        if g.state != State::Open {
            return Vec::new();
        }
        let mut dropped = Vec::new();
        if g.output_bytes + data.len() > self.output_cap {
            // Each Terminal's first dropped chunk becomes its notice, in place: the notice
            // stays ahead of any control frame queued after that output (a `term.exit` is a
            // barrier no output of its Terminal may cross).
            let notice: Arc<[u8]> = Arc::from(OVERFLOW_NOTICE);
            let mut kept = VecDeque::with_capacity(g.q.len());
            let q = std::mem::take(&mut g.q);
            for o in q {
                match o {
                    Out::Output {
                        terminal: t,
                        data: d,
                    } => {
                        g.output_bytes -= d.len();
                        g.total_bytes -= d.len();
                        if !dropped.contains(&t) {
                            dropped.push(t);
                            g.output_bytes += notice.len();
                            g.total_bytes += notice.len();
                            kept.push_back(Out::Output {
                                terminal: t,
                                data: notice.clone(),
                            });
                        }
                    }
                    other => kept.push_back(other),
                }
            }
            g.q = kept;
        }
        self.push_locked(&mut g, Out::Output { terminal, data });
        dropped
    }

    /// Flush what is queued, then close the connection.
    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        if g.state == State::Open {
            g.state = State::Closing;
            self.cv.notify_all();
        }
    }

    /// Close the connection now, dropping anything queued.
    pub fn abort(&self) {
        let mut g = self.inner.lock().unwrap();
        self.abort_locked(&mut g);
    }

    pub fn is_open(&self) -> bool {
        self.inner.lock().unwrap().state == State::Open
    }

    /// Bytes queued and not yet taken by the writer. Producers of deferrable control frames
    /// (session appends) wait while it is high instead of growing a queue that is never
    /// dropped.
    pub fn queued_bytes(&self) -> usize {
        self.inner.lock().unwrap().total_bytes
    }

    /// Wait until the writer has finished (flushed and shut down), at most until `deadline`.
    pub fn wait_done(&self, deadline: Instant) -> bool {
        let mut g = self.inner.lock().unwrap();
        while !g.writer_done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            g = self.cv.wait_timeout(g, left).unwrap().0;
        }
        true
    }

    /// Everything queued right now, or `None` once the writer should stop.
    fn next_batch(&self) -> Option<Vec<Out>> {
        let mut g = self.inner.lock().unwrap();
        loop {
            match g.state {
                State::Aborted => return None,
                _ if !g.q.is_empty() => {
                    g.output_bytes = 0;
                    g.total_bytes = 0;
                    return Some(g.q.drain(..).collect());
                }
                State::Closing => return None,
                State::Open => g = self.cv.wait(g).unwrap(),
            }
        }
    }
}

fn write_output<W: Write>(w: &mut W, terminal: &Uuid, chunks: &[Arc<[u8]>]) -> std::io::Result<()> {
    let len: usize = chunks.iter().map(|c| c.len()).sum();
    w.write_all(&((1 + 16 + len) as u32).to_be_bytes())?;
    w.write_all(&[KIND_OUTPUT])?;
    w.write_all(terminal.as_bytes())?;
    for c in chunks {
        w.write_all(c)?;
    }
    Ok(())
}

/// Output chunks of one Terminal waiting to be merged into one frame: id, chunks, total.
type Run = (Uuid, Vec<Arc<[u8]>>, usize);

fn write_batch<W: Write>(w: &mut W, batch: Vec<Out>) -> std::io::Result<()> {
    let mut run: Option<Run> = None;
    for item in batch {
        if let Out::Output { terminal, data } = &item {
            if let Some((t, chunks, size)) = &mut run {
                if t == terminal && *size + data.len() <= MERGE_MAX {
                    *size += data.len();
                    chunks.push(data.clone());
                    continue;
                }
            }
        }
        if let Some((t, chunks, _)) = run.take() {
            write_output(w, &t, &chunks)?;
        }
        match item {
            Out::Output { terminal, data } => {
                let n = data.len();
                run = Some((terminal, vec![data], n));
            }
            Out::Control(f) | Out::Terminals(f) => w.write_all(&f)?,
        }
    }
    if let Some((t, chunks, _)) = run {
        write_output(w, &t, &chunks)?;
    }
    w.flush()
}

/// Drain `ob` into `sock` until it is closed or a write fails or stalls.
pub(crate) fn writer_loop(ob: Arc<Outbox>, sock: Stream, stall: Duration) {
    let _ = sock.set_write_timeout(Some(stall));
    let mut w = BufWriter::with_capacity(WRITE_BUF, &sock);
    while let Some(batch) = ob.next_batch() {
        if let Err(e) = write_batch(&mut w, batch) {
            // After a timeout the stream may hold half a frame: the connection is unusable.
            crate::log!("INFO", "dropping connection: write failed: {e}");
            ob.abort();
            break;
        }
    }
    drop(w);
    let _ = sock.shutdown(Shutdown::Both);
    let mut g = ob.inner.lock().unwrap();
    if g.state == State::Open {
        g.state = State::Closing;
    }
    g.writer_done = true;
    ob.cv.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};

    fn bytes(b: &[u8]) -> Arc<[u8]> {
        Arc::from(b)
    }

    fn queued(ob: &Outbox) -> Vec<(char, Vec<u8>)> {
        ob.inner
            .lock()
            .unwrap()
            .q
            .iter()
            .map(|o| match o {
                Out::Control(f) => ('c', f.to_vec()),
                Out::Terminals(f) => ('l', f.to_vec()),
                Out::Output { data, .. } => ('o', data.to_vec()),
            })
            .collect()
    }

    #[test]
    fn overflow_drops_output_keeps_control() {
        let ob = Outbox::new(100, 10_000, None);
        let t1 = Uuid::new_v4();
        assert!(ob.push_control(bytes(b"ctl")));
        let a = [b'a'; 60];
        let b = [b'b'; 60];
        let c = [b'c'; 60];
        assert!(ob.push_output(t1, bytes(&a)).is_empty());
        assert_eq!(ob.push_output(t1, bytes(&b)), vec![t1]);
        assert_eq!(ob.push_output(t1, bytes(&c)), vec![t1]);
        assert_eq!(
            queued(&ob),
            vec![
                ('c', b"ctl".to_vec()),
                ('o', OVERFLOW_NOTICE.to_vec()),
                ('o', c.to_vec())
            ]
        );
    }

    /// Overflow for B must not move A's notice behind A's `term.exit`.
    #[test]
    fn overflow_notice_stays_before_exit_barrier() {
        let ob = Outbox::new(100, 10_000, None);
        let (ta, tb) = (Uuid::new_v4(), Uuid::new_v4());
        ob.push_output(ta, bytes(b"last words"));
        ob.push_control(bytes(b"exit-A"));
        ob.push_output(tb, bytes(&[b'b'; 60]));
        let mut dropped = ob.push_output(tb, bytes(&[b'c'; 60]));
        dropped.sort();
        let mut want = vec![ta, tb];
        want.sort();
        assert_eq!(dropped, want);
        assert_eq!(
            queued(&ob),
            vec![
                ('o', OVERFLOW_NOTICE.to_vec()),
                ('c', b"exit-A".to_vec()),
                ('o', OVERFLOW_NOTICE.to_vec()),
                ('o', [b'c'; 60].to_vec()),
            ]
        );
        // And which Terminal each output belongs to.
        let owners: Vec<Option<Uuid>> = ob
            .inner
            .lock()
            .unwrap()
            .q
            .iter()
            .map(|o| match o {
                Out::Output { terminal, .. } => Some(*terminal),
                _ => None,
            })
            .collect();
        assert_eq!(owners, vec![Some(ta), None, Some(tb), Some(tb)]);
    }

    #[test]
    fn control_over_total_cap_disconnects() {
        let ob = Outbox::new(100, 10, None);
        assert!(ob.push_control(bytes(b"12345")));
        assert!(!ob.push_control(bytes(b"123456")));
        assert!(!ob.is_open());
        assert!(queued(&ob).is_empty());
    }

    #[test]
    fn terminals_coalesced() {
        let ob = Outbox::new(100, 10_000, None);
        ob.push_terminals(bytes(b"L1"));
        ob.push_control(bytes(b"ctl"));
        ob.push_terminals(bytes(b"L2"));
        assert_eq!(
            queued(&ob),
            vec![('c', b"ctl".to_vec()), ('l', b"L2".to_vec())]
        );
    }

    #[test]
    fn writer_merges_adjacent_output() {
        let (a, mut b) = super::super::transport::pair().unwrap();
        let ob = Outbox::new(1 << 20, 1 << 21, None);
        let t1 = Uuid::new_v4();
        for chunk in [&b"one "[..], b"two ", b"three"] {
            ob.push_output(t1, bytes(chunk));
        }
        ob.close();
        let ob2 = ob.clone();
        let h = std::thread::spawn(move || writer_loop(ob2, a, Duration::from_secs(1)));
        let f = read_frame(&mut b, MAX_FRAME_LEN).unwrap().unwrap();
        assert_eq!(
            f,
            Frame::Output {
                terminal: t1,
                data: b"one two three".to_vec()
            }
        );
        assert!(read_frame(&mut b, MAX_FRAME_LEN).unwrap().is_none());
        h.join().unwrap();
        assert!(ob.wait_done(Instant::now()));
    }
}

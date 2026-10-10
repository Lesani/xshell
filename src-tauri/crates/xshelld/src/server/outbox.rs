//! Per-connection send queue and its writer thread. Producers (PTY readers, call threads,
//! the connection reader) never block on a socket: they enqueue, and the writer drains.
//!
//! Slow-consumer policy: queued Terminal output is capped; on overflow it is dropped and
//! replaced by `OVERFLOW_NOTICE` (a reset), and the affected Terminals are nudged to redraw.
//! Control frames are never dropped; if they alone exceed the total cap, the peer is
//! disconnected. A peer that makes no write progress for `write_stall_timeout` is dropped.
//!
//! Pacing (a Mobile's connection, [`PaceCfg`]): Terminal output is held and written at most
//! once per interval, one frame per Terminal; everything else goes at once. Output held still
//! counts against the caps. A message about a Terminal (`term.exit`, an attach's `res`) and
//! an attach's replay are barriers: the output of that Terminal queued before them goes with
//! them, due or not, and no byte ever crosses one. A `term.size` waits for the output of its
//! Terminal queued before it, and output queued after it waits for the next interval: one
//! output frame per Terminal per interval, barriers apart.

use super::transport::Stream;
use std::collections::{HashSet, VecDeque};
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
    /// Responses, `hello`, `error`: never dropped.
    Control(Arc<[u8]>),
    /// A `terminals` list: only the newest queued one is kept.
    Terminals(Arc<[u8]>),
    /// Raw Terminal output, framed by the writer. `urgent` (an attach's replay) is written
    /// at once on a paced connection.
    Output {
        terminal: Uuid,
        data: Arc<[u8]>,
        urgent: bool,
    },
    /// A control frame about one Terminal (`term.exit`, an attach's `res`): never dropped,
    /// and a barrier for that Terminal's output.
    About { terminal: Uuid, frame: Arc<[u8]> },
    /// A `term.size`: only the newest queued one per Terminal is kept.
    Size { terminal: Uuid, frame: Arc<[u8]> },
}

impl Out {
    fn len(&self) -> usize {
        match self {
            Out::Control(f)
            | Out::Terminals(f)
            | Out::About { frame: f, .. }
            | Out::Size { frame: f, .. } => f.len(),
            Out::Output { data, .. } => data.len(),
        }
    }

    fn is_output(&self) -> bool {
        matches!(self, Out::Output { .. })
    }
}

/// How often a paced connection gets Terminal output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaceCfg {
    /// The interval between flushes normally.
    pub idle: Duration,
    /// The interval between flushes for `window` after the peer's input.
    pub burst: Duration,
    pub window: Duration,
}

/// When a paced connection's held output is due. Pure: every method takes the time.
#[derive(Debug)]
struct Pace {
    cfg: PaceCfg,
    last_input: Option<Instant>,
    last_flush: Option<Instant>,
}

impl Pace {
    fn new(cfg: PaceCfg) -> Self {
        Self {
            cfg,
            last_input: None,
            last_flush: None,
        }
    }

    fn interval(&self, now: Instant) -> Duration {
        match self.last_input {
            Some(at) if now.saturating_duration_since(at) < self.cfg.window => self.cfg.burst,
            _ => self.cfg.idle,
        }
    }

    /// When held output may be written next.
    fn due_at(&self, now: Instant) -> Instant {
        match self.last_flush {
            None => now,
            Some(f) => f + self.interval(now),
        }
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.due_at(now)
    }

    fn flushed(&mut self, now: Instant) {
        self.last_flush = Some(now);
    }

    fn input(&mut self, now: Instant) {
        self.last_input = Some(now);
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
    /// `Some` on a paced connection.
    pace: Option<Pace>,
}

impl Inner {
    /// Remove the items marked in `take` (in order) and account for them.
    fn extract(&mut self, take: &[bool]) -> Vec<Out> {
        let mut batch = Vec::new();
        let mut kept = VecDeque::with_capacity(self.q.len());
        for (o, take) in std::mem::take(&mut self.q).into_iter().zip(take) {
            if *take {
                self.total_bytes -= o.len();
                if o.is_output() {
                    self.output_bytes -= o.len();
                }
                batch.push(o);
            } else {
                kept.push_back(o);
            }
        }
        self.q = kept;
        batch
    }

    /// What a paced connection writes at `now`, and when held output is due next (`None`:
    /// nothing is held).
    fn take_paced(&mut self, now: Instant) -> (Vec<Out>, Option<Instant>) {
        let Some(due) = self.pace.as_ref().map(|p| p.due(now)) else {
            return (self.q.drain(..).collect(), None);
        };
        let n = self.q.len();
        // From the back: a barrier about T takes T's output and sizes queued before it, due
        // or not.
        let mut forced_at = vec![false; n];
        let mut forced: HashSet<Uuid> = HashSet::new();
        for (i, o) in self.q.iter().enumerate().rev() {
            match o {
                Out::About { terminal, .. }
                | Out::Output {
                    terminal,
                    urgent: true,
                    ..
                } => {
                    forced.insert(*terminal);
                    forced_at[i] = true;
                }
                Out::Output { terminal, .. } | Out::Size { terminal, .. } => {
                    forced_at[i] = forced.contains(terminal)
                }
                Out::Control(_) | Out::Terminals(_) => {}
            }
        }
        // From the front. Output goes when due, but one segment per Terminal: output after a
        // size of a Terminal whose output this flush already writes waits for the next one.
        // A size goes unless output of its Terminal before it is held.
        let mut take = vec![false; n];
        let mut held: HashSet<Uuid> = HashSet::new();
        let mut wrote: HashSet<Uuid> = HashSet::new();
        let mut cut: HashSet<Uuid> = HashSet::new();
        for (i, o) in self.q.iter().enumerate() {
            take[i] = match o {
                Out::Control(_) | Out::Terminals(_) | Out::About { .. } => true,
                Out::Output { urgent: true, .. } => true,
                Out::Output { terminal, .. } => {
                    let go = forced_at[i]
                        || (due && !held.contains(terminal) && !cut.contains(terminal));
                    if go {
                        wrote.insert(*terminal);
                    } else {
                        held.insert(*terminal);
                    }
                    go
                }
                Out::Size { terminal, .. } => {
                    let go = forced_at[i] || !held.contains(terminal);
                    if go && wrote.contains(terminal) {
                        cut.insert(*terminal);
                    }
                    go
                }
            };
        }
        let batch = self.extract(&take);
        let pace = self.pace.as_mut().expect("paced");
        if due && batch.iter().any(Out::is_output) {
            pace.flushed(now);
        }
        let held = self.q.iter().any(Out::is_output);
        (batch, held.then(|| pace.due_at(now)))
    }
}

pub(crate) struct Outbox {
    inner: Mutex<Inner>,
    cv: Condvar,
    output_cap: usize,
    total_cap: usize,
    /// A handle on the connection's socket, to cut a writer stuck on a dead peer.
    sock: Option<Stream>,
    paced: bool,
}

impl Outbox {
    /// An unpaced outbox (tests; connections are made by [`Outbox::with_pace`]).
    #[cfg(test)]
    pub fn new(output_cap: usize, total_cap: usize, sock: Option<Stream>) -> Arc<Self> {
        Self::with_pace(output_cap, total_cap, sock, None)
    }

    /// An outbox whose Terminal output is paced by `pace` (a Mobile's connection).
    pub fn with_pace(
        output_cap: usize,
        total_cap: usize,
        sock: Option<Stream>,
        pace: Option<PaceCfg>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                q: VecDeque::new(),
                output_bytes: 0,
                total_bytes: 0,
                state: State::Open,
                writer_done: false,
                pace: pace.map(Pace::new),
            }),
            cv: Condvar::new(),
            output_cap,
            total_cap,
            sock,
            paced: pace.is_some(),
        })
    }

    /// Whether this is a Mobile's connection (exactly the paced ones): it gets `term.size`,
    /// a shorter replay, and its attach nudges only when nobody else is attached.
    pub fn is_mobile(&self) -> bool {
        self.paced
    }

    /// The peer typed: a paced connection gets output faster for a while.
    pub fn note_input(&self) {
        self.note_input_at(Instant::now());
    }

    fn note_input_at(&self, now: Instant) {
        let mut g = self.inner.lock().unwrap();
        if let Some(p) = g.pace.as_mut() {
            p.input(now);
            self.cv.notify_all();
        }
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
        if item.is_output() {
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

    /// Queue a control frame about `terminal` (`term.exit`, an attach's `res`): a barrier no
    /// output of that Terminal crosses, which takes the output queued before it along on a
    /// paced connection.
    pub fn push_about(&self, terminal: Uuid, frame: Arc<[u8]>) -> bool {
        let mut g = self.inner.lock().unwrap();
        self.push_locked(&mut g, Out::About { terminal, frame })
    }

    /// Queue a `term.size`, replacing one for the same Terminal still waiting in the queue.
    pub fn push_size(&self, terminal: Uuid, frame: Arc<[u8]>) {
        let mut g = self.inner.lock().unwrap();
        if g.state != State::Open {
            return;
        }
        let old =
            g.q.iter()
                .position(|o| matches!(o, Out::Size { terminal: t, .. } if *t == terminal));
        if let Some(i) = old {
            let old = g.q.remove(i).unwrap();
            g.total_bytes -= old.len();
        }
        self.push_locked(&mut g, Out::Size { terminal, frame });
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
        self.push_output_inner(terminal, data, false)
    }

    /// [`Outbox::push_output`] for a piece of an attach's replay: a paced connection writes
    /// it at once.
    pub fn push_replay(&self, terminal: Uuid, data: Arc<[u8]>) -> Vec<Uuid> {
        self.push_output_inner(terminal, data, true)
    }

    fn push_output_inner(&self, terminal: Uuid, data: Arc<[u8]>, urgent: bool) -> Vec<Uuid> {
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
                        urgent,
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
                                urgent,
                            });
                        }
                    }
                    other => kept.push_back(other),
                }
            }
            g.q = kept;
        }
        self.push_locked(
            &mut g,
            Out::Output {
                terminal,
                data,
                urgent,
            },
        );
        dropped
    }

    /// Drop the output and `term.size` of `terminal` still queued: the peer detached.
    pub fn cancel_output(&self, terminal: Uuid) {
        self.remove_about(terminal, true);
    }

    /// Drop the output of `terminal` still queued, keeping its `term.size` (an overflow
    /// recovery replaces the output by a fresh replay; the size it shows still holds).
    pub fn drop_output(&self, terminal: Uuid) {
        self.remove_about(terminal, false);
    }

    fn remove_about(&self, terminal: Uuid, sizes: bool) {
        let mut g = self.inner.lock().unwrap();
        let take: Vec<bool> =
            g.q.iter()
                .map(|o| match o {
                    Out::Output { terminal: t, .. } => *t == terminal,
                    Out::Size { terminal: t, .. } => sizes && *t == terminal,
                    _ => false,
                })
                .collect();
        g.extract(&take);
    }

    /// Everything queued, taken out of the queue (tests).
    #[cfg(test)]
    pub(crate) fn take_all(&self) -> Vec<Out> {
        let mut g = self.inner.lock().unwrap();
        g.output_bytes = 0;
        g.total_bytes = 0;
        g.q.drain(..).collect()
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

    /// What to write now, or `None` once the writer should stop. A paced connection waits
    /// while only held output is queued.
    fn next_batch(&self) -> Option<Vec<Out>> {
        let mut g = self.inner.lock().unwrap();
        loop {
            match g.state {
                State::Aborted => return None,
                State::Closing if g.q.is_empty() => return None,
                State::Open if g.q.is_empty() => g = self.cv.wait(g).unwrap(),
                State::Closing => {
                    g.output_bytes = 0;
                    g.total_bytes = 0;
                    return Some(g.q.drain(..).collect());
                }
                State::Open => {
                    let now = Instant::now();
                    let (batch, next) = g.take_paced(now);
                    if !batch.is_empty() {
                        if g.pace.is_none() {
                            g.output_bytes = 0;
                            g.total_bytes = 0;
                        }
                        return Some(batch);
                    }
                    let wait = next.map_or(Duration::from_secs(1), |at| {
                        at.saturating_duration_since(now)
                    });
                    g = self.cv.wait_timeout(g, wait).unwrap().0;
                }
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
        if let Out::Output { terminal, data, .. } = &item {
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
            Out::Output { terminal, data, .. } => {
                let n = data.len();
                run = Some((terminal, vec![data], n));
            }
            Out::Control(f)
            | Out::Terminals(f)
            | Out::About { frame: f, .. }
            | Out::Size { frame: f, .. } => w.write_all(&f)?,
        }
    }
    if let Some((t, chunks, _)) = run {
        write_output(w, &t, &chunks)?;
    }
    w.flush()
}

/// A paced flush: one output frame per Terminal, in the order the Terminals first appear,
/// cut only by a barrier or size of that Terminal (its output so far is written first).
/// Other control frames go first.
fn write_paced<W: Write>(w: &mut W, batch: Vec<Out>) -> std::io::Result<()> {
    let mut runs: Vec<(Uuid, Vec<Arc<[u8]>>)> = Vec::new();
    for item in batch {
        match item {
            Out::Output { terminal, data, .. } => {
                match runs.iter_mut().find(|(t, _)| *t == terminal) {
                    Some((_, chunks)) => chunks.push(data),
                    None => runs.push((terminal, vec![data])),
                }
            }
            Out::About { terminal, frame } | Out::Size { terminal, frame } => {
                if let Some(i) = runs.iter().position(|(t, _)| *t == terminal) {
                    let (t, chunks) = runs.remove(i);
                    write_output(w, &t, &chunks)?;
                }
                w.write_all(&frame)?;
            }
            Out::Control(f) | Out::Terminals(f) => w.write_all(&f)?,
        }
    }
    for (t, chunks) in runs {
        write_output(w, &t, &chunks)?;
    }
    w.flush()
}

/// Drain `ob` into `sock` until it is closed or a write fails or stalls.
pub(crate) fn writer_loop(ob: Arc<Outbox>, sock: Stream, stall: Duration) {
    let _ = sock.set_write_timeout(Some(stall));
    let mut w = BufWriter::with_capacity(WRITE_BUF, &sock);
    while let Some(batch) = ob.next_batch() {
        let r = if ob.paced {
            write_paced(&mut w, batch)
        } else {
            write_batch(&mut w, batch)
        };
        if let Err(e) = r {
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
                Out::Control(f) | Out::About { frame: f, .. } => ('c', f.to_vec()),
                Out::Terminals(f) => ('l', f.to_vec()),
                Out::Size { frame: f, .. } => ('s', f.to_vec()),
                Out::Output { data, .. } => ('o', data.to_vec()),
            })
            .collect()
    }

    const PACE: PaceCfg = PaceCfg {
        idle: Duration::from_secs(1),
        burst: Duration::from_millis(100),
        window: Duration::from_secs(3),
    };

    fn paced() -> Arc<Outbox> {
        Outbox::with_pace(1 << 20, 1 << 21, None, Some(PACE))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// What the writer would write at `now`, decoded: `("c", control bytes)` for JSON frames
    /// (written raw in these tests) and `("o:<t>", bytes)` per output frame.
    fn flush(ob: &Outbox, now: Instant) -> Vec<(String, Vec<u8>)> {
        let batch = ob.inner.lock().unwrap().take_paced(now).0;
        let mut buf = Vec::new();
        write_paced(&mut buf, batch).unwrap();
        decode(&buf)
    }

    fn decode(mut buf: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        while !buf.is_empty() {
            if buf[0] == b'#' {
                // A raw test "control frame": `#name;`.
                let end = buf.iter().position(|&b| b == b';').unwrap() + 1;
                out.push(("c".to_string(), buf[1..end - 1].to_vec()));
                buf = &buf[end..];
                continue;
            }
            match read_frame(&mut buf, MAX_FRAME_LEN).unwrap().unwrap() {
                Frame::Output { terminal, data } => {
                    out.push((format!("o:{}", &terminal.to_string()[..4]), data))
                }
                f => panic!("{f:?}"),
            }
        }
        out
    }

    fn ctl(name: &str) -> Arc<[u8]> {
        Arc::from(format!("#{name};").into_bytes())
    }

    fn tid(n: u8) -> Uuid {
        Uuid::from_bytes([n, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    }

    fn o(n: u8) -> String {
        format!("o:{}", &tid(n).to_string()[..4])
    }

    fn c(name: &str) -> (String, Vec<u8>) {
        ("c".into(), name.as_bytes().to_vec())
    }

    fn out(n: u8, data: &str) -> (String, Vec<u8>) {
        (o(n), data.as_bytes().to_vec())
    }

    #[test]
    fn pace_interval_idle_then_burst_then_idle() {
        let t0 = Instant::now();
        let mut p = Pace::new(PACE);
        assert!(p.due(t0), "the first flush is due at once");
        p.flushed(t0);
        assert_eq!(p.interval(t0), ms(1000));
        assert!(!p.due(t0 + ms(999)));
        assert!(p.due(t0 + ms(1000)));
        p.flushed(t0 + ms(1000));
        p.input(t0 + ms(1050));
        assert_eq!(p.interval(t0 + ms(1050)), ms(100));
        assert!(p.due(t0 + ms(1100)));
        p.flushed(t0 + ms(1100));
        assert!(!p.due(t0 + ms(1199)));
        assert_eq!(p.due_at(t0 + ms(1150)), t0 + ms(1200));
        // Three seconds after the input: idle again.
        assert_eq!(p.interval(t0 + ms(4049)), ms(100));
        assert_eq!(p.interval(t0 + ms(4050)), ms(1000));
        p.flushed(t0 + ms(4000));
        assert!(!p.due(t0 + ms(4100)));
        assert!(p.due(t0 + ms(5000)));
    }

    /// With an injected clock: a Terminal printing every 20 ms gets one frame per second
    /// idle, ten per second for three seconds after input, then one per second again.
    #[test]
    fn paced_flush_times_follow_the_interval() {
        let ob = paced();
        let t0 = Instant::now();
        let mut flushes = Vec::new();
        for step in 0..400u64 {
            let now = t0 + ms(step * 20);
            if step == 150 {
                ob.note_input_at(now);
            }
            ob.push_output(tid(1), bytes(b"x"));
            if !flush(&ob, now).is_empty() {
                flushes.push(step * 20);
            }
        }
        let idle: Vec<u64> = flushes.iter().copied().filter(|&t| t < 3000).collect();
        assert_eq!(idle, vec![0, 1000, 2000]);
        let burst: Vec<u64> = flushes
            .iter()
            .copied()
            .filter(|&t| (3000..6000).contains(&t))
            .collect();
        // 3000 is due as the burst starts, then every 100 ms until 6000.
        assert_eq!(burst, (30..60).map(|i| i * 100).collect::<Vec<_>>());
        let after: Vec<u64> = flushes.iter().copied().filter(|&t| t >= 6000).collect();
        assert_eq!(after, vec![6900, 7900]);
    }

    #[test]
    fn paced_controls_pass_while_output_held() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"a"));
        assert_eq!(flush(&ob, t0), vec![out(1, "a")]);
        ob.push_output(tid(1), bytes(b"b"));
        ob.push_control(ctl("res"));
        ob.push_terminals(ctl("list"));
        assert_eq!(flush(&ob, t0 + ms(10)), vec![c("res"), c("list")]);
        assert_eq!(flush(&ob, t0 + ms(20)), vec![]);
        let (_, next) = ob.inner.lock().unwrap().take_paced(t0 + ms(20));
        assert_eq!(next, Some(t0 + ms(1000)));
        assert_eq!(flush(&ob, t0 + ms(1000)), vec![out(1, "b")]);
    }

    #[test]
    fn paced_exit_flushes_output_first() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"start"));
        flush(&ob, t0);
        ob.push_output(tid(1), bytes(b"last "));
        ob.push_output(tid(1), bytes(b"words"));
        ob.push_about(tid(1), ctl("exit"));
        assert_eq!(
            flush(&ob, t0 + ms(10)),
            vec![out(1, "last words"), c("exit")]
        );
        assert!(queued(&ob).is_empty());
    }

    /// An exit forces out only its own Terminal's output: a watched Terminal that keeps
    /// printing stays at one frame per second while other Terminals exit.
    #[test]
    fn unrelated_exits_keep_the_pace() {
        let ob = paced();
        let t0 = Instant::now();
        let mut frames_of_1 = Vec::new();
        for step in 0..150u64 {
            let now = t0 + ms(step * 20);
            ob.push_output(tid(1), bytes(b"x"));
            if step % 5 == 0 {
                let other = tid(2 + (step % 3) as u8);
                ob.push_output(other, bytes(b"bye"));
                ob.push_about(other, ctl("exit"));
            }
            for (k, _) in flush(&ob, now) {
                if k == o(1) {
                    frames_of_1.push(step * 20);
                }
            }
        }
        assert_eq!(frames_of_1, vec![0, 1000, 2000]);
    }

    #[test]
    fn paced_flush_writes_one_frame_per_terminal() {
        let ob = paced();
        let t0 = Instant::now();
        for (t, d) in [(1, "a"), (2, "1"), (1, "b"), (2, "2"), (1, "c")] {
            ob.push_output(tid(t), bytes(d.as_bytes()));
        }
        assert_eq!(flush(&ob, t0), vec![out(1, "abc"), out(2, "12")]);
    }

    /// Barriers cut the grouping: no byte moves across an exit or a `res`, and a reused
    /// UUID's old and new output stay on their sides of the exit.
    #[test]
    fn barriers_keep_exact_order() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"old"));
        ob.push_output(tid(2), bytes(b"x"));
        ob.push_about(tid(1), ctl("exit1"));
        ob.push_output(tid(1), bytes(b"new"));
        ob.push_control(ctl("res"));
        ob.push_output(tid(2), bytes(b"y"));
        ob.push_about(tid(2), ctl("res2"));
        ob.push_replay(tid(2), bytes(b"replay"));
        ob.push_output(tid(1), bytes(b"newer"));
        // Due: everything, cut at each barrier of the same Terminal.
        assert_eq!(
            flush(&ob, t0),
            vec![
                out(1, "old"),
                c("exit1"),
                c("res"),
                out(2, "xy"),
                c("res2"),
                out(1, "newnewer"),
                out(2, "replay"),
            ]
        );
        // Not due: the exit takes the output before it and leaves what follows held.
        ob.push_output(tid(1), bytes(b"old"));
        ob.push_about(tid(1), ctl("exit1"));
        ob.push_output(tid(1), bytes(b"new"));
        assert_eq!(flush(&ob, t0 + ms(10)), vec![out(1, "old"), c("exit1")]);
        assert_eq!(flush(&ob, t0 + ms(1000)), vec![out(1, "new")]);
    }

    /// An attach's replay goes at once, `res` first, whatever the pacing state.
    #[test]
    fn replay_is_immediate_after_its_res() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"a"));
        flush(&ob, t0);
        ob.push_output(tid(2), bytes(b"held"));
        ob.push_about(tid(1), ctl("res"));
        ob.push_replay(tid(1), bytes(b"replay"));
        ob.push_output(tid(1), bytes(b"live"));
        assert_eq!(flush(&ob, t0 + ms(5)), vec![c("res"), out(1, "replay")]);
        // Rapid reattach: again at once.
        ob.cancel_output(tid(1));
        ob.push_about(tid(1), ctl("res"));
        ob.push_replay(tid(1), bytes(b"replay2"));
        assert_eq!(flush(&ob, t0 + ms(10)), vec![c("res"), out(1, "replay2")]);
        assert_eq!(flush(&ob, t0 + ms(1000)), vec![out(2, "held")]);
    }

    #[test]
    fn size_latest_wins_and_waits_for_earlier_output() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"a"));
        flush(&ob, t0);
        // No output held before it: at once.
        ob.push_size(tid(1), ctl("s1"));
        assert_eq!(flush(&ob, t0 + ms(1)), vec![c("s1")]);
        // Behind held output: it waits with it, and only the newest is kept.
        ob.push_output(tid(1), bytes(b"b"));
        ob.push_size(tid(1), ctl("s2"));
        ob.push_size(tid(2), ctl("other"));
        ob.push_size(tid(1), ctl("s3"));
        assert_eq!(queued(&ob).iter().filter(|(k, _)| *k == 's').count(), 2);
        assert_eq!(flush(&ob, t0 + ms(2)), vec![c("other")]);
        assert_eq!(flush(&ob, t0 + ms(1000)), vec![out(1, "b"), c("s3")]);
    }

    /// A size notice never splits a paced flush: output queued after it waits for the next
    /// interval, so a Terminal gets at most one output frame per interval.
    #[test]
    fn size_keeps_one_output_frame_per_interval() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"a"));
        flush(&ob, t0);
        ob.push_output(tid(1), bytes(b"old"));
        ob.push_size(tid(1), ctl("size"));
        ob.push_output(tid(1), bytes(b"redraw"));
        ob.push_output(tid(2), bytes(b"other"));
        assert_eq!(flush(&ob, t0 + ms(500)), vec![]);
        assert_eq!(
            flush(&ob, t0 + ms(1000)),
            vec![out(1, "old"), c("size"), out(2, "other")]
        );
        assert_eq!(flush(&ob, t0 + ms(1500)), vec![]);
        assert_eq!(flush(&ob, t0 + ms(2000)), vec![out(1, "redraw")]);
        // A size with no output of its Terminal before it in the flush cuts nothing.
        ob.push_size(tid(1), ctl("size2"));
        ob.push_output(tid(1), bytes(b"after"));
        assert_eq!(flush(&ob, t0 + ms(2001)), vec![c("size2")]);
        assert_eq!(flush(&ob, t0 + ms(3000)), vec![out(1, "after")]);
        // Barriers still take everything before them along.
        ob.push_output(tid(1), bytes(b"x"));
        ob.push_size(tid(1), ctl("size3"));
        ob.push_output(tid(1), bytes(b"y"));
        ob.push_about(tid(1), ctl("exit"));
        assert_eq!(
            flush(&ob, t0 + ms(3001)),
            vec![out(1, "x"), c("size3"), out(1, "y"), c("exit")]
        );
    }

    /// Resizes while a Terminal prints (a Desktop dragging its window): the viewing Mobile
    /// still gets at most one output frame per interval.
    #[test]
    fn repeated_resizes_keep_the_pace() {
        let ob = paced();
        let t0 = Instant::now();
        let mut frames = Vec::new();
        for step in 0..200u64 {
            let now = t0 + ms(step * 20);
            ob.push_output(tid(1), bytes(b"x"));
            ob.push_size(tid(1), ctl(&format!("s{}", step % 7)));
            ob.push_output(tid(1), bytes(b"y"));
            let got = flush(&ob, now);
            let n = got.iter().filter(|(k, _)| *k == o(1)).count();
            assert!(n <= 1, "{n} frames in one flush at {}", step * 20);
            if n == 1 {
                frames.push(step * 20);
            }
        }
        assert_eq!(frames, vec![0, 1000, 2000, 3000]);
    }

    #[test]
    fn drop_output_keeps_sizes() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"a"));
        flush(&ob, t0);
        ob.push_output(tid(1), bytes(b"held"));
        ob.push_size(tid(1), ctl("s"));
        ob.drop_output(tid(1));
        assert_eq!(queued(&ob), vec![('s', b"#s;".to_vec())]);
        ob.cancel_output(tid(1));
        assert!(queued(&ob).is_empty());
    }

    #[test]
    fn paced_overflow_still_drops_with_notice() {
        let ob = Outbox::with_pace(100, 10_000, None, Some(PACE));
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"first"));
        flush(&ob, t0);
        assert!(ob.push_output(tid(1), bytes(&[b'a'; 60])).is_empty());
        assert_eq!(ob.push_output(tid(1), bytes(&[b'b'; 60])), vec![tid(1)]);
        assert_eq!(flush(&ob, t0 + ms(10)), vec![]);
        let mut want = OVERFLOW_NOTICE.to_vec();
        want.extend_from_slice(&[b'b'; 60]);
        assert_eq!(flush(&ob, t0 + ms(1000)), vec![(o(1), want)]);
        let g = ob.inner.lock().unwrap();
        assert_eq!((g.output_bytes, g.total_bytes), (0, 0));
    }

    #[test]
    fn cancel_output_drops_only_that_terminal() {
        let ob = paced();
        let t0 = Instant::now();
        ob.push_output(tid(1), bytes(b"a"));
        flush(&ob, t0);
        ob.push_output(tid(1), bytes(b"held"));
        ob.push_size(tid(1), ctl("s"));
        ob.push_output(tid(2), bytes(b"other"));
        ob.push_control(ctl("res"));
        ob.cancel_output(tid(1));
        assert_eq!(
            queued(&ob),
            vec![('o', b"other".to_vec()), ('c', b"#res;".to_vec())]
        );
        let g = ob.inner.lock().unwrap();
        assert_eq!(g.output_bytes, 5);
        assert_eq!(g.total_bytes, 5 + 5);
    }

    /// The writer thread: held output waits for its interval, `close` flushes it.
    #[test]
    fn paced_writer_holds_then_flushes_on_close() {
        let (a, mut b) = super::super::transport::pair().unwrap();
        let ob = paced();
        let ob2 = ob.clone();
        let h = std::thread::spawn(move || writer_loop(ob2, a, Duration::from_secs(1)));
        let t1 = Uuid::new_v4();
        ob.push_output(t1, bytes(b"one"));
        let f = read_frame(&mut b, MAX_FRAME_LEN).unwrap().unwrap();
        assert_eq!(
            f,
            Frame::Output {
                terminal: t1,
                data: b"one".to_vec()
            }
        );
        ob.push_output(t1, bytes(b"two "));
        ob.push_output(t1, bytes(b"three"));
        std::thread::sleep(ms(100));
        ob.close();
        let f = read_frame(&mut b, MAX_FRAME_LEN).unwrap().unwrap();
        assert_eq!(
            f,
            Frame::Output {
                terminal: t1,
                data: b"two three".to_vec()
            }
        );
        assert!(read_frame(&mut b, MAX_FRAME_LEN).unwrap().is_none());
        h.join().unwrap();
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

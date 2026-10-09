//! The Ring client's IO thread: the one owner of the socket. Non-blocking, driven by `poll`
//! (WSAPoll on Windows) on the socket and a loopback wake-up socket, so a stalled peer
//! never blocks it. rustls and tungstenite keep their state across `WouldBlock`; tungstenite
//! buffers what the socket refuses, up to a cap, and the outbound queue in front of it is
//! byte-bounded, so a peer that stops reading surfaces as `Backpressure`, not as memory
//! growth. Keepalive and goodbye deadlines run on the monotonic clock, independent of
//! reads.
//!
//! A private seam: the client sees only [`Outbox`] and [`Handler`], so this loop can move to
//! another readiness API or an async runtime without changing the client's API.

use super::super::RingError;
use super::transport::{is_would_block, Conn};
use super::wire::{ByeReason, ClientFrame, CloseReason, PING};
use std::io;
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// What the IO thread delivers. Called on the IO thread, in wire order.
pub(crate) trait Handler: Send {
    /// First, before any frame.
    fn start(&mut self) {}
    fn text(&mut self, text: &str);
    /// Exactly once, last.
    fn closed(&mut self, why: CloseReason);
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IoConfig {
    pub ping_interval: Duration,
    pub dead_after: Duration,
    pub bye_timeout: Duration,
    pub queue_bytes: usize,
}

enum Out {
    Text(String),
    /// Where the goodbye stands in the queue; the request itself is in `Outbox::bye`.
    Bye,
}

/// A goodbye, timestamped when it was requested: its deadline does not wait for the queue.
struct ByeRequest {
    deadline: Instant,
    done: mpsc::SyncSender<Result<(), RingError>>,
    /// Taken when written.
    frame: Option<String>,
}

/// The sending half, shared by the client and its handler.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: Sender<Out>,
    queued: Arc<AtomicUsize>,
    cap: usize,
    waker: Arc<UdpSocket>,
    closed: Arc<Mutex<Option<CloseReason>>>,
    local_close: Arc<AtomicBool>,
    tcp: Arc<TcpStream>,
    bye: Arc<Mutex<Option<ByeRequest>>>,
    bye_timeout: Duration,
}

impl Outbox {
    fn closed_error(&self) -> Option<RingError> {
        let c = self.closed.lock().unwrap_or_else(|e| e.into_inner());
        c.clone().map(RingError::Closed)
    }

    /// Queues one text frame. Fails with `Backpressure` when the queue is over its byte cap.
    pub(crate) fn send_text(&self, text: String) -> Result<(), RingError> {
        if let Some(e) = self.closed_error() {
            return Err(e);
        }
        let n = text.len();
        let prev = self.queued.fetch_add(n, Ordering::AcqRel);
        if prev + n > self.cap {
            self.queued.fetch_sub(n, Ordering::AcqRel);
            return Err(RingError::Backpressure);
        }
        if self.tx.send(Out::Text(text)).is_err() {
            self.queued.fetch_sub(n, Ordering::AcqRel);
            return Err(self
                .closed_error()
                .unwrap_or(RingError::Closed(CloseReason::Local)));
        }
        self.wake();
        Ok(())
    }

    /// Queues the goodbye behind everything already queued. The receiver gets `Ok` once the
    /// Relay closed after it, `Err(Timeout)` when the goodbye deadline passed first.
    pub(crate) fn bye(
        &self,
        reason: ByeReason,
    ) -> Result<Receiver<Result<(), RingError>>, RingError> {
        if let Some(e) = self.closed_error() {
            return Err(e);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut slot = self.bye.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_some() {
                return Err(RingError::Invalid("goodbye already requested".into()));
            }
            *slot = Some(ByeRequest {
                deadline: Instant::now() + self.bye_timeout,
                done: tx,
                frame: Some(ClientFrame::Bye { reason }.encode()),
            });
        }
        self.tx
            .send(Out::Bye)
            .map_err(|_| RingError::Closed(CloseReason::Local))?;
        self.wake();
        Ok(rx)
    }

    fn wake(&self) {
        // Best effort: the loop also wakes on its own timers.
        let _ = self.waker.send(&[1]);
    }

    /// Drops the connection without a goodbye.
    pub(crate) fn shutdown(&self) {
        self.local_close.store(true, Ordering::Release);
        let _ = self.tcp.shutdown(std::net::Shutdown::Both);
        self.wake();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed_error().is_some()
    }
}

fn waker_pair() -> io::Result<(UdpSocket, UdpSocket)> {
    let rx = UdpSocket::bind("127.0.0.1:0")?;
    let tx = UdpSocket::bind("127.0.0.1:0")?;
    tx.connect(rx.local_addr()?)?;
    rx.connect(tx.local_addr()?)?;
    rx.set_nonblocking(true)?;
    tx.set_nonblocking(true)?;
    Ok((tx, rx))
}

/// Starts the IO thread over an authenticated, non-blocking connection.
pub(crate) fn spawn(
    ws: WebSocket<Conn>,
    cfg: IoConfig,
    handler: impl FnOnce(Outbox) -> Box<dyn Handler>,
) -> io::Result<Outbox> {
    let (wake_tx, wake_rx) = waker_pair()?;
    let tcp = Arc::new(ws.get_ref().tcp().try_clone()?);
    let (tx, rx) = mpsc::channel();
    let outbox = Outbox {
        tx,
        queued: Arc::new(AtomicUsize::new(0)),
        cap: cfg.queue_bytes,
        waker: Arc::new(wake_tx),
        closed: Arc::new(Mutex::new(None)),
        local_close: Arc::new(AtomicBool::new(false)),
        tcp,
        bye: Arc::new(Mutex::new(None)),
        bye_timeout: cfg.bye_timeout,
    };
    let mut io = Io {
        ws,
        rx,
        wake: wake_rx,
        outbox: outbox.clone(),
        cfg,
        handler: handler(outbox.clone()),
        pending: None,
        want_write: false,
        bye_reached: false,
        bye_timed_out: false,
    };
    std::thread::Builder::new()
        .name("xshell-ring-io".into())
        .spawn(move || io.run())?;
    Ok(outbox)
}

struct Io {
    ws: WebSocket<Conn>,
    rx: Receiver<Out>,
    wake: UdpSocket,
    outbox: Outbox,
    cfg: IoConfig,
    handler: Box<dyn Handler>,
    /// A frame tungstenite refused because its buffer is full.
    pending: Option<Message>,
    /// The socket refused bytes; poll for writability.
    want_write: bool,
    /// The queue reached the goodbye's place.
    bye_reached: bool,
    bye_timed_out: bool,
}

/// Messages read per turn before the loop serves writes and timers.
const READ_BURST: usize = 64;
/// Bytes read from the stream per turn. Bounds the work a peer can cause between two turns
/// even with frames that never complete a message (empty continuation fragments).
const READ_BUDGET: usize = 64 * 1024;

impl Io {
    fn run(&mut self) {
        self.handler.start();
        let why = self.run_loop();
        let local = self.outbox.local_close.load(Ordering::Acquire);
        let why = if local { CloseReason::Local } else { why };
        let bye = self
            .outbox
            .bye
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(b) = bye {
            // The goodbye succeeded if it was fully written and the Relay then closed.
            let flushed = b.frame.is_none() && self.pending.is_none() && !self.want_write;
            let relay_closed = matches!(why, CloseReason::Relay { .. } | CloseReason::Io(_));
            let ok = flushed && relay_closed && !self.bye_timed_out;
            let _ = b
                .done
                .send(if ok { Ok(()) } else { Err(RingError::Timeout) });
            *self.outbox.closed.lock().unwrap_or_else(|e| e.into_inner()) = Some(CloseReason::Bye);
            let _ = self.ws.get_ref().tcp().shutdown(std::net::Shutdown::Both);
            self.handler.closed(CloseReason::Bye);
            return;
        }
        *self.outbox.closed.lock().unwrap_or_else(|e| e.into_inner()) = Some(why.clone());
        let _ = self.ws.get_ref().tcp().shutdown(std::net::Shutdown::Both);
        self.handler.closed(why);
    }

    fn run_loop(&mut self) -> CloseReason {
        let start = Instant::now();
        let mut last_rx = start;
        let mut next_ping = start + self.cfg.ping_interval;
        loop {
            if self.outbox.local_close.load(Ordering::Acquire) {
                return CloseReason::Local;
            }
            // 1. Inbound, bounded per turn in messages and in bytes.
            let mut more_to_read = false;
            self.ws.get_mut().set_read_budget(Some(READ_BUDGET));
            for i in 0..READ_BURST {
                match self.ws.read() {
                    Ok(Message::Text(t)) => {
                        last_rx = Instant::now();
                        self.handler.text(t.as_str());
                    }
                    Ok(Message::Close(frame)) => {
                        let code = frame.map(|f| u16::from(f.code));
                        // Let tungstenite queue its close reply; then we are done.
                        let _ = self.ws.flush();
                        return CloseReason::Relay {
                            close_code: code,
                            error: None,
                        };
                    }
                    // A v1 Relay never sends binary frames; control frames count as life.
                    Ok(_) => last_rx = Instant::now(),
                    Err(tungstenite::Error::Io(e)) if is_would_block(&e) => break,
                    Err(e) => return self.read_error(e),
                }
                if i == READ_BURST - 1 {
                    more_to_read = true;
                }
            }
            // A spent budget may leave decrypted bytes that poll cannot see: come back soon.
            if self.ws.get_ref().read_budget_spent() {
                more_to_read = true;
            }
            self.ws.get_mut().set_read_budget(None);

            // 2. Outbound: the refused frame, the queue, the goodbye, then flush.
            if let Err(why) = self.pump_out() {
                return why;
            }

            // 3. Timers.
            let now = Instant::now();
            if now.duration_since(last_rx) >= self.cfg.dead_after {
                return CloseReason::Dead;
            }
            let bye_deadline = self.bye_deadline();
            if bye_deadline.is_some_and(|d| now >= d) {
                self.bye_timed_out = true;
                return CloseReason::Bye;
            }
            if now >= next_ping {
                next_ping = now + self.cfg.ping_interval;
                if self.pending.is_none() {
                    if let Err(why) = self.write(Message::text(PING)) {
                        return why;
                    }
                    if let Err(why) = self.flush() {
                        return why;
                    }
                }
            }

            // 4. Wait.
            if more_to_read {
                continue;
            }
            let mut deadline = (last_rx + self.cfg.dead_after).min(next_ping);
            if let Some(d) = bye_deadline {
                deadline = deadline.min(d);
            }
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1));
            let want_write = self.want_write || self.pending.is_some();
            match poll::wait(self.ws.get_ref().tcp(), want_write, &self.wake, timeout) {
                Ok(r) => {
                    if r.woken {
                        let mut buf = [0u8; 64];
                        while self.wake.recv(&mut buf).is_ok() {}
                    }
                }
                Err(e) => return CloseReason::Io(format!("poll: {e}")),
            }
        }
    }

    fn bye_deadline(&self) -> Option<Instant> {
        let slot = self.outbox.bye.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|b| b.deadline)
    }

    fn read_error(&self, e: tungstenite::Error) -> CloseReason {
        if self.outbox.local_close.load(Ordering::Acquire) {
            return CloseReason::Local;
        }
        match e {
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                CloseReason::Relay {
                    close_code: None,
                    error: None,
                }
            }
            tungstenite::Error::Io(e) => CloseReason::Io(e.to_string()),
            other => CloseReason::Protocol(other.to_string()),
        }
    }

    /// Hands one frame to tungstenite. `WouldBlock` means it is buffered; a full buffer keeps
    /// the frame in `pending`.
    fn write(&mut self, msg: Message) -> Result<(), CloseReason> {
        match self.ws.write(msg) {
            Ok(()) => Ok(()),
            Err(tungstenite::Error::Io(e)) if is_would_block(&e) => {
                self.want_write = true;
                Ok(())
            }
            Err(tungstenite::Error::WriteBufferFull(m)) => {
                self.pending = Some(*m);
                Ok(())
            }
            Err(e) => Err(self.read_error(e)),
        }
    }

    fn flush(&mut self) -> Result<(), CloseReason> {
        match self.ws.flush() {
            Ok(()) => {
                self.want_write = false;
                Ok(())
            }
            Err(tungstenite::Error::Io(e)) if is_would_block(&e) => {
                self.want_write = true;
                Ok(())
            }
            Err(e) => Err(self.read_error(e)),
        }
    }

    fn pump_out(&mut self) -> Result<(), CloseReason> {
        if let Some(m) = self.pending.take() {
            self.write(m)?;
        }
        while self.pending.is_none() {
            // The goodbye goes out after everything queued before it.
            if self.bye_reached {
                let frame = {
                    let mut slot = self.outbox.bye.lock().unwrap_or_else(|e| e.into_inner());
                    slot.as_mut().and_then(|b| b.frame.take())
                };
                if let Some(f) = frame {
                    self.write(Message::text(f))?;
                }
                break;
            }
            match self.rx.try_recv() {
                Ok(Out::Text(t)) => {
                    self.outbox.queued.fetch_sub(t.len(), Ordering::AcqRel);
                    self.write(Message::text(t))?;
                }
                Ok(Out::Bye) => self.bye_reached = true,
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        self.flush()
    }
}

/// Readiness of the socket and the wake-up socket.
mod poll {
    use std::io;
    use std::net::{TcpStream, UdpSocket};
    use std::time::Duration;

    #[derive(Default)]
    pub(super) struct Ready {
        pub woken: bool,
    }

    fn millis(t: Duration) -> i32 {
        // Round up, so a sub-millisecond wait does not spin.
        let ms = t.as_millis() + u128::from(!t.subsec_nanos().is_multiple_of(1_000_000));
        ms.min(i32::MAX as u128) as i32
    }

    #[cfg(unix)]
    pub(super) fn wait(
        tcp: &TcpStream,
        want_write: bool,
        wake: &UdpSocket,
        timeout: Duration,
    ) -> io::Result<Ready> {
        use std::os::unix::io::AsRawFd;
        let mut ev = libc::POLLIN;
        if want_write {
            ev |= libc::POLLOUT;
        }
        let mut fds = [
            libc::pollfd {
                fd: tcp.as_raw_fd(),
                events: ev,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two pollfd for the duration of the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, millis(timeout)) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(Ready::default());
            }
            return Err(e);
        }
        Ok(Ready {
            woken: fds[1].revents != 0,
        })
    }

    #[cfg(windows)]
    pub(super) fn wait(
        tcp: &TcpStream,
        want_write: bool,
        wake: &UdpSocket,
        timeout: Duration,
    ) -> io::Result<Ready> {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            WSAPoll, POLLRDNORM, POLLWRNORM, SOCKET, SOCKET_ERROR, WSAPOLLFD,
        };
        let mut ev = POLLRDNORM;
        if want_write {
            ev |= POLLWRNORM;
        }
        let mut fds = [
            WSAPOLLFD {
                fd: tcp.as_raw_socket() as SOCKET,
                events: ev,
                revents: 0,
            },
            WSAPOLLFD {
                fd: wake.as_raw_socket() as SOCKET,
                events: POLLRDNORM,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two WSAPOLLFD for the duration of the call.
        let n = unsafe { WSAPoll(fds.as_mut_ptr(), 2, millis(timeout)) };
        if n == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(Ready {
            woken: fds[1].revents != 0,
        })
    }
}

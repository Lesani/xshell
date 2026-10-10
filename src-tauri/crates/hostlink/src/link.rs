//! One protocol connection to a Daemon over a byte stream (ssh's stdio). Threads, no runtime:
//! a reader that routes frames, a writer that drains a byte-bounded queue, and a reaper that
//! fails requests past their deadline. Completion is callback-based ([`Waiter`]); waiters are
//! never called while a link lock is held.

use crate::errors::HostError;
use serde_json::Value;
use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_protocol::correlate::Correlator;
use xshell_protocol::frame::{Frame, FrameDecoder};
use xshell_protocol::msg::{
    decode_server, encode_msg, ClientMsg, DecodeError, Hello, ProtocolRange, ServerMsg,
    TerminalInfo,
};
use xshell_protocol::negotiate::negotiate;
use xshell_protocol::CAPABILITIES;

pub type Waiter = Box<dyn FnOnce(Result<Value, HostError>) + Send>;

pub struct LinkIo {
    pub read: Box<dyn Read + Send>,
    pub write: Box<dyn Write + Send>,
}

/// What the reader delivers. Called on the reader thread, in wire order.
pub trait LinkEvents: Send + Sync {
    fn output(&self, terminal: Uuid, data: &[u8]);
    fn terminals(&self, list: Vec<TerminalInfo>);
    fn term_exit(&self, terminal: Uuid, code: i32);
    /// Exactly once, when the stream ends or breaks.
    fn closed(&self, why: String);
}

#[derive(Debug, Clone, Copy)]
pub struct LinkLimits {
    /// Bytes queued for the writer, not yet taken by it.
    pub queue_bytes: usize,
    /// Requests awaiting a response.
    pub max_pending: usize,
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            queue_bytes: 32 * 1024 * 1024,
            max_pending: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkError {
    /// The Daemon's hello shares no protocol version with ours.
    Incompatible {
        hello: Hello,
        message: String,
    },
    Failed(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Incompatible { message, .. } | LinkError::Failed(message) => {
                f.write_str(message)
            }
        }
    }
}

/// Shell startup files may print before the Daemon speaks; scan this far for its hello.
const NOISE_LIMIT: usize = 64 * 1024;
/// A hello frame: the kind byte, then the JSON every Daemon writes first.
const HELLO_MARK: &[u8] = b"\x00{\"t\":\"hello\"";

struct Queue {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

struct PendingReq {
    deadline: Instant,
    what: &'static str,
    waiter: Waiter,
}

struct Pending {
    corr: Correlator<PendingReq>,
    deadlines: BTreeSet<(Instant, u64)>,
    closed: bool,
}

pub struct Link {
    q: Mutex<Queue>,
    q_cv: Condvar,
    pending: Mutex<Pending>,
    p_cv: Condvar,
    limits: LinkLimits,
    closed: AtomicBool,
    /// The text of an `error` message from the Daemon, the reason it will close.
    remote_error: Mutex<Option<String>>,
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

fn what(msg: &ClientMsg) -> &'static str {
    match msg {
        ClientMsg::Hello(_) => "hello",
        ClientMsg::Call { .. } => "call",
        ClientMsg::TermOpen { .. } => "term.open",
        ClientMsg::TermAttach { .. } => "term.attach",
        ClientMsg::TermDetach { .. } => "term.detach",
        ClientMsg::TermInput { .. } => "term.input",
        ClientMsg::TermResize { .. } => "term.resize",
        ClientMsg::TermClose { .. } => "term.close",
        ClientMsg::TermUpdate { .. } => "term.update",
        ClientMsg::TermRelaunch { .. } => "term.relaunch",
        ClientMsg::DaemonUpgrade => "daemon.upgrade",
        ClientMsg::TermEvent { .. } => "term.event",
        ClientMsg::RingIdentity => "ring.identity",
        ClientMsg::RingJoin { .. } => "ring.join",
        ClientMsg::PushRegister { .. } => "push.register",
        ClientMsg::PushUnregister => "push.unregister",
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

type Handshake = Result<(Hello, Option<String>), LinkError>;

impl Link {
    pub fn establish(
        io: LinkIo,
        ours: ProtocolRange,
        version: &str,
        ev: Arc<dyn LinkEvents>,
        hello_timeout: Duration,
    ) -> Result<(Arc<Link>, Hello, Option<String>), LinkError> {
        Self::establish_with(io, ours, version, ev, hello_timeout, LinkLimits::default())
    }

    /// Send our hello, find theirs (skipping shell noise), check the protocol ranges, and
    /// wait for the first message after it: `terminals` (delivered before this returns) or
    /// an `error`, whose text becomes the failure.
    pub fn establish_with(
        io: LinkIo,
        ours: ProtocolRange,
        version: &str,
        ev: Arc<dyn LinkEvents>,
        hello_timeout: Duration,
        limits: LinkLimits,
    ) -> Result<(Arc<Link>, Hello, Option<String>), LinkError> {
        let link = Arc::new(Link {
            q: Mutex::new(Queue {
                frames: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            q_cv: Condvar::new(),
            pending: Mutex::new(Pending {
                corr: Correlator::new(),
                deadlines: BTreeSet::new(),
                closed: false,
            }),
            p_cv: Condvar::new(),
            limits,
            closed: AtomicBool::new(false),
            remote_error: Mutex::new(None),
        });
        let hello = ClientMsg::Hello(Hello {
            protocol: ours,
            version: version.into(),
            capabilities: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
        });
        let frame = encode_msg(&hello, None).map_err(|e| LinkError::Failed(e.to_string()))?;
        link.enqueue(frame)
            .map_err(|e| LinkError::Failed(e.message))?;

        let lw = link.clone();
        let mut write = io.write;
        let spawned = std::thread::Builder::new()
            .name("link-writer".into())
            .spawn(move || lw.writer_loop(&mut write));
        if let Err(e) = spawned {
            return Err(LinkError::Failed(e.to_string()));
        }
        let lr = link.clone();
        let (tx, rx) = mpsc::channel::<Handshake>();
        let mut read = io.read;
        let spawned = std::thread::Builder::new()
            .name("link-reader".into())
            .spawn(move || lr.reader_loop(&mut read, ours, ev, tx));
        if let Err(e) = spawned {
            link.close();
            return Err(LinkError::Failed(e.to_string()));
        }
        let lp = link.clone();
        let _ = std::thread::Builder::new()
            .name("link-reaper".into())
            .spawn(move || lp.reaper_loop());

        match rx.recv_timeout(hello_timeout) {
            Ok(Ok((h, noise))) => Ok((link, h, noise)),
            Ok(Err(e)) => {
                link.close();
                Err(e)
            }
            Err(_) => {
                link.close();
                Err(LinkError::Failed(format!(
                    "xshelld did not complete the handshake within {}s",
                    hello_timeout.as_secs_f32()
                )))
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn enqueue(&self, frame: Vec<u8>) -> Result<(), HostError> {
        let mut q = self.q.lock().unwrap();
        if q.closed {
            return Err(HostError::offline("not connected"));
        }
        if q.bytes + frame.len() > self.limits.queue_bytes {
            return Err(HostError::busy());
        }
        q.bytes += frame.len();
        q.frames.push_back(frame);
        drop(q);
        self.q_cv.notify_one();
        Ok(())
    }

    /// Send `msg` with an id. `w` gets the response, a timeout, or the connection loss. On
    /// `Err` the request was not sent and `w` is dropped without being called.
    pub fn request(&self, msg: ClientMsg, timeout: Duration, w: Waiter) -> Result<(), HostError> {
        if self.is_closed() {
            return Err(HostError::offline("not connected"));
        }
        let deadline = Instant::now() + timeout;
        let label = what(&msg);
        let id = {
            let mut p = self.pending.lock().unwrap();
            if p.closed {
                return Err(HostError::offline("not connected"));
            }
            if p.corr.len() >= self.limits.max_pending {
                return Err(HostError::busy());
            }
            let id = p.corr.register(PendingReq {
                deadline,
                what: label,
                waiter: w,
            });
            p.deadlines.insert((deadline, id));
            id
        };
        let sent = encode_msg(&msg, Some(id))
            .map_err(|e| HostError::invalid(e.to_string()))
            .and_then(|f| self.enqueue(f));
        if let Err(e) = sent {
            let mut p = self.pending.lock().unwrap();
            if p.corr.complete(id).is_some() {
                p.deadlines.remove(&(deadline, id));
            }
            return Err(e);
        }
        self.p_cv.notify_one();
        Ok(())
    }

    /// Send `msg` without an id: the Daemon does not answer.
    pub fn notify(&self, msg: ClientMsg) -> Result<(), HostError> {
        let f = encode_msg(&msg, None).map_err(|e| HostError::invalid(e.to_string()))?;
        self.enqueue(f)
    }

    /// Stop writing and fail every pending request. The reader ends when the stream does
    /// (the owner kills the process).
    pub fn close(&self) {
        self.shutdown("connection closed");
    }

    fn shutdown(&self, why: &str) {
        self.closed.store(true, Ordering::SeqCst);
        {
            let mut q = self.q.lock().unwrap();
            q.closed = true;
            q.frames.clear();
            q.bytes = 0;
        }
        self.q_cv.notify_all();
        let waiters: Vec<PendingReq> = {
            let mut p = self.pending.lock().unwrap();
            p.closed = true;
            p.deadlines.clear();
            p.corr.drain()
        };
        self.p_cv.notify_all();
        for w in waiters {
            (w.waiter)(Err(HostError::offline(why)));
        }
    }

    fn complete(&self, id: u64, r: Result<Value, String>) {
        let req = {
            let mut p = self.pending.lock().unwrap();
            let req = p.corr.complete(id);
            if let Some(r) = &req {
                p.deadlines.remove(&(r.deadline, id));
            }
            req
        };
        // A late reply to a timed-out request finds nothing and is dropped.
        if let Some(req) = req {
            (req.waiter)(r.map_err(HostError::from_daemon));
        }
    }

    fn writer_loop(&self, w: &mut dyn Write) {
        loop {
            let batch: Vec<Vec<u8>> = {
                let mut q = self.q.lock().unwrap();
                while q.frames.is_empty() && !q.closed {
                    q = self.q_cv.wait(q).unwrap();
                }
                if q.frames.is_empty() {
                    return; // closed and drained
                }
                q.bytes = 0;
                q.frames.drain(..).collect()
            };
            let mut res = Ok(());
            for f in &batch {
                res = w.write_all(f);
                if res.is_err() {
                    break;
                }
            }
            if res.and_then(|_| w.flush()).is_err() {
                // The peer is gone; the reader reports why when the stream ends.
                let mut q = self.q.lock().unwrap();
                q.closed = true;
                q.frames.clear();
                q.bytes = 0;
                return;
            }
        }
    }

    fn reaper_loop(&self) {
        loop {
            let expired: Vec<PendingReq> = {
                let mut p = self.pending.lock().unwrap();
                loop {
                    if p.closed {
                        return;
                    }
                    let now = Instant::now();
                    match p.deadlines.first().copied() {
                        Some((d, _)) if d <= now => break,
                        Some((d, _)) => p = self.p_cv.wait_timeout(p, d - now).unwrap().0,
                        None => p = self.p_cv.wait(p).unwrap(),
                    }
                }
                let now = Instant::now();
                let mut out = Vec::new();
                while let Some(&(d, id)) = p.deadlines.first() {
                    if d > now {
                        break;
                    }
                    p.deadlines.remove(&(d, id));
                    if let Some(r) = p.corr.complete(id) {
                        out.push(r);
                    }
                }
                out
            };
            for r in expired {
                let what = r.what;
                (r.waiter)(Err(HostError::timeout(format!(
                    "{what} timed out waiting for xshelld"
                ))));
            }
        }
    }

    fn reader_loop(
        self: Arc<Self>,
        r: &mut dyn Read,
        ours: ProtocolRange,
        ev: Arc<dyn LinkEvents>,
        handshake: mpsc::Sender<Handshake>,
    ) {
        let mut handshake = Some(handshake);
        let why = match self.read_all(r, ours, &*ev, &mut handshake) {
            Ok(()) => "connection lost".to_string(),
            Err(e) => e,
        };
        let why = match self.remote_error.lock().unwrap().clone() {
            Some(m) => m,
            None => why,
        };
        if let Some(tx) = handshake.take() {
            let _ = tx.send(Err(LinkError::Failed(why.clone())));
        }
        let waiter_msg = if why == "connection lost" {
            why.clone()
        } else {
            format!("connection lost: {why}")
        };
        self.shutdown(&waiter_msg);
        ev.closed(why);
    }

    /// Returns `Ok` on EOF, `Err(reason)` on a broken stream.
    fn read_all(
        &self,
        r: &mut dyn Read,
        ours: ProtocolRange,
        ev: &dyn LinkEvents,
        handshake: &mut Option<mpsc::Sender<Handshake>>,
    ) -> Result<(), String> {
        let mut buf = vec![0u8; 64 * 1024];
        // Phase 1: find the hello frame.
        let mut scan: Vec<u8> = Vec::new();
        let start = loop {
            if let Some(i) = find(&scan, HELLO_MARK).filter(|&i| i >= 4) {
                break i - 4;
            }
            if scan.len() > NOISE_LIMIT {
                return Err(format!(
                    "xshelld did not answer; the host printed: {}",
                    noise_text(&scan[..NOISE_LIMIT.min(scan.len())])
                ));
            }
            let n = read_some(r, &mut buf)?;
            if n == 0 {
                let noise = noise_text(&scan);
                return Err(if noise.is_empty() {
                    "connection closed before the handshake".into()
                } else {
                    format!("connection closed before the handshake: {noise}")
                });
            }
            scan.extend_from_slice(&buf[..n]);
        };
        let noise = (start > 0).then(|| String::from_utf8_lossy(&scan[..start]).into_owned());
        let mut dec = FrameDecoder::new();
        dec.feed(&scan[start..]);
        drop(scan);

        let mut hello: Option<Hello> = None;
        loop {
            while let Some(f) = dec
                .next_frame()
                .map_err(|e| format!("protocol error: {e}"))?
            {
                match hello {
                    None => {
                        let h = match f {
                            Frame::Json(j) => match decode_server(&j) {
                                Ok(ServerMsg::Hello(h)) => h,
                                _ => return Err("protocol error: expected hello".into()),
                            },
                            _ => return Err("protocol error: expected hello".into()),
                        };
                        if negotiate(ours, h.protocol).is_err() {
                            let message = crate::version::incompatible_message(ours, &h);
                            if let Some(tx) = handshake.take() {
                                let _ = tx.send(Err(LinkError::Incompatible {
                                    hello: h,
                                    message: message.clone(),
                                }));
                            }
                            return Err(message);
                        }
                        hello = Some(h);
                    }
                    Some(ref h) => {
                        let first_json = matches!(f, Frame::Json(_));
                        self.dispatch(f, ev)?;
                        if first_json {
                            if let Some(tx) = handshake.take() {
                                let err = self.remote_error.lock().unwrap().clone();
                                let _ = tx.send(match err {
                                    Some(m) => Err(LinkError::Failed(m)),
                                    None => Ok((h.clone(), noise.clone())),
                                });
                            }
                        }
                    }
                }
            }
            let n = read_some(r, &mut buf)?;
            if n == 0 {
                return Ok(());
            }
            dec.feed(&buf[..n]);
        }
    }

    fn dispatch(&self, f: Frame, ev: &dyn LinkEvents) -> Result<(), String> {
        match f {
            Frame::Output { terminal, data } => ev.output(terminal, &data),
            Frame::Unknown { .. } => {}
            Frame::Json(j) => match decode_server(&j) {
                Ok(ServerMsg::Res(r)) => self.complete(r.id, r.outcome.into_result()),
                Ok(ServerMsg::Terminals { list }) => ev.terminals(list),
                Ok(ServerMsg::TermExit { terminal, code }) => ev.term_exit(terminal, code),
                Ok(ServerMsg::Error { message, .. }) => {
                    *self.remote_error.lock().unwrap() = Some(message);
                }
                Ok(ServerMsg::Hello(_)) => {}
                // A newer Daemon's message type: additive, skip it.
                Err(DecodeError::UnknownType { .. }) => {}
                Err(e) => return Err(format!("protocol error: {e}")),
            },
        }
        Ok(())
    }
}

fn read_some(r: &mut dyn Read, buf: &mut [u8]) -> Result<usize, String> {
    loop {
        match r.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
}

fn noise_text(b: &[u8]) -> String {
    crate::process::strip_ansi(&String::from_utf8_lossy(b))
        .trim()
        .to_string()
}

#[cfg(test)]
pub(crate) mod testpeer {
    //! A scripted Daemon over in-memory pipes.
    use super::*;
    use std::io::{BufReader, PipeReader, PipeWriter};
    use xshell_protocol::frame::{encode_output, read_frame, MAX_FRAME_LEN};
    use xshell_protocol::msg::encode_res;

    pub struct Peer {
        pub r: BufReader<PipeReader>,
        pub w: PipeWriter,
    }

    pub fn pair() -> (LinkIo, Peer) {
        let (dr, pw) = std::io::pipe().unwrap();
        let (pr, dw) = std::io::pipe().unwrap();
        (
            LinkIo {
                read: Box::new(dr),
                write: Box::new(dw),
            },
            Peer {
                r: BufReader::new(pr),
                w: pw,
            },
        )
    }

    pub fn hello_frame(min: u32, max: u32, version: &str) -> Vec<u8> {
        encode_msg(
            &ServerMsg::Hello(Hello {
                protocol: ProtocolRange { min, max },
                version: version.into(),
                capabilities: vec![],
            }),
            None,
        )
        .unwrap()
    }

    pub fn msg_frame(m: &ServerMsg) -> Vec<u8> {
        encode_msg(m, None).unwrap()
    }

    pub fn res_frame(id: u64, r: Result<Value, String>) -> Vec<u8> {
        encode_res(id, r)
    }

    pub fn out_frame(t: Uuid, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        encode_output(&t, data, &mut v).unwrap();
        v
    }

    pub fn terminals_frame(list: Vec<TerminalInfo>) -> Vec<u8> {
        msg_frame(&ServerMsg::Terminals { list })
    }

    impl Peer {
        pub fn write(&mut self, b: &[u8]) {
            self.w.write_all(b).unwrap();
            self.w.flush().unwrap();
        }

        /// A hello 1..1 and an empty list: what a Daemon sends on connect.
        pub fn greet(&mut self) {
            let mut b = hello_frame(1, 1, "1.5.0");
            b.extend(terminals_frame(vec![]));
            self.write(&b);
        }

        /// The next JSON message from the Desktop.
        pub fn read_json(&mut self) -> Value {
            loop {
                match read_frame(&mut self.r, MAX_FRAME_LEN).unwrap() {
                    Some(Frame::Json(j)) => return serde_json::from_slice(&j).unwrap(),
                    Some(_) => continue,
                    None => panic!("desktop closed the stream"),
                }
            }
        }

        /// The next message whose `t` is `t`, skipping others (hello).
        pub fn expect(&mut self, t: &str) -> Value {
            loop {
                let v = self.read_json();
                if v["t"] == t {
                    return v;
                }
            }
        }

        pub fn reply(&mut self, id: u64, r: Result<Value, String>) {
            self.write(&res_frame(id, r));
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Ev {
        Out(Uuid, Vec<u8>),
        List(Vec<TerminalInfo>),
        Exit(Uuid, i32),
        Closed(String),
    }

    #[derive(Default)]
    pub struct Rec {
        pub evs: Mutex<Vec<Ev>>,
        cv: Condvar,
    }

    impl Rec {
        pub fn new() -> Arc<Rec> {
            Arc::new(Rec::default())
        }

        pub fn wait_for(&self, pred: impl Fn(&[Ev]) -> bool) -> Vec<Ev> {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut g = self.evs.lock().unwrap();
            loop {
                if pred(&g) {
                    return g.clone();
                }
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "timed out; events: {:?}", *g);
                g = self.cv.wait_timeout(g, left).unwrap().0;
            }
        }

        fn push(&self, e: Ev) {
            self.evs.lock().unwrap().push(e);
            self.cv.notify_all();
        }
    }

    impl LinkEvents for Rec {
        fn output(&self, t: Uuid, data: &[u8]) {
            self.push(Ev::Out(t, data.to_vec()));
        }
        fn terminals(&self, list: Vec<TerminalInfo>) {
            self.push(Ev::List(list));
        }
        fn term_exit(&self, t: Uuid, code: i32) {
            self.push(Ev::Exit(t, code));
        }
        fn closed(&self, why: String) {
            self.push(Ev::Closed(why));
        }
    }

    /// Collects one waiter result.
    pub fn slot() -> (Waiter, mpsc::Receiver<Result<Value, HostError>>) {
        let (tx, rx) = mpsc::channel();
        (
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
            rx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testpeer::*;
    use super::*;
    use crate::errors::HostErrorCode;
    use serde_json::json;
    use xshell_core::launch::LaunchSpec;

    const R11: ProtocolRange = ProtocolRange { min: 1, max: 1 };
    const T5: Duration = Duration::from_secs(5);

    fn up(io: LinkIo, rec: &Arc<Rec>) -> Arc<Link> {
        Link::establish(io, R11, "1.5.0", rec.clone(), T5)
            .unwrap()
            .0
    }

    fn info(t: Uuid) -> TerminalInfo {
        TerminalInfo {
            terminal: t,
            spec: LaunchSpec::default(),
            meta: Default::default(),
            created_at_ms: 1,
            pid: Some(2),
            exit_code: None,
            agent_status: None,
            status_at_ms: None,
            last_line: None,
        }
    }

    #[test]
    fn handshake_ok() {
        let (io, mut peer) = pair();
        peer.greet();
        let rec = Rec::new();
        let (_link, hello, noise) = Link::establish(io, R11, "1.5.0", rec.clone(), T5).unwrap();
        assert_eq!(hello.version, "1.5.0");
        assert_eq!(negotiate(R11, hello.protocol).unwrap(), 1);
        assert_eq!(noise, None);
        let h = peer.read_json();
        assert_eq!(h["t"], "hello");
        assert_eq!(h["version"], "1.5.0");
        assert_eq!(h["protocol"], json!({"min":1,"max":1}));
        // The first list was delivered before establish returned.
        assert_eq!(rec.evs.lock().unwrap().as_slice(), &[Ev::List(vec![])]);
    }

    #[test]
    fn handshake_skips_shell_noise() {
        let (io, mut peer) = pair();
        peer.write(b"Welcome!\n");
        peer.greet();
        let (_l, _h, noise) = Link::establish(io, R11, "1.5.0", Rec::new(), T5).unwrap();
        assert_eq!(noise.as_deref(), Some("Welcome!\n"));
    }

    #[test]
    fn handshake_no_overlap() {
        let (io, mut peer) = pair();
        peer.write(&hello_frame(2, 3, "9.0.0"));
        let e = Link::establish(io, R11, "1.5.0", Rec::new(), T5).unwrap_err();
        let LinkError::Incompatible { hello, message } = e else {
            panic!("{e:?}")
        };
        assert_eq!(hello.version, "9.0.0");
        assert!(
            message.contains("1..1") && message.contains("2..3"),
            "{message}"
        );
    }

    #[test]
    fn error_frame_text_surfaces() {
        let (io, mut peer) = pair();
        let mut b = hello_frame(1, 1, "1.5.0");
        b.extend(msg_frame(&ServerMsg::Error {
            code: "protocol_mismatch".into(),
            message: "m".into(),
        }));
        peer.write(&b);
        drop(peer);
        let e = Link::establish(io, R11, "1.5.0", Rec::new(), T5).unwrap_err();
        assert_eq!(e, LinkError::Failed("m".into()));
    }

    #[test]
    fn eof_before_hello_reports_noise() {
        let (io, mut peer) = pair();
        peer.write(b"sh: 1: xshelld: not found\n");
        drop(peer);
        let e = Link::establish(io, R11, "1.5.0", Rec::new(), T5).unwrap_err();
        assert!(e.to_string().contains("xshelld: not found"), "{e}");
    }

    #[test]
    fn handshake_timeout() {
        let (io, _peer) = pair();
        let start = Instant::now();
        let e = Link::establish(io, R11, "1.5.0", Rec::new(), Duration::from_millis(200));
        assert!(matches!(e, Err(LinkError::Failed(_))));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn calls_correlate_out_of_order() {
        let (io, mut peer) = pair();
        peer.greet();
        let link = up(io, &Rec::new());
        let (w1, r1) = slot();
        let (w2, r2) = slot();
        let call = |m: &str| ClientMsg::Call {
            method: m.into(),
            params: json!({}),
        };
        link.request(call("a"), T5, w1).unwrap();
        link.request(call("b"), T5, w2).unwrap();
        let a = peer.expect("call");
        let b = peer.expect("call");
        assert_eq!(a["method"], "a");
        peer.reply(b["id"].as_u64().unwrap(), Ok(json!("B")));
        peer.reply(a["id"].as_u64().unwrap(), Ok(json!("A")));
        assert_eq!(r1.recv_timeout(T5).unwrap(), Ok(json!("A")));
        assert_eq!(r2.recv_timeout(T5).unwrap(), Ok(json!("B")));
    }

    #[test]
    fn ok_null_is_ok_and_err_is_remote() {
        let (io, mut peer) = pair();
        peer.greet();
        let link = up(io, &Rec::new());
        let (w, r) = slot();
        link.request(ClientMsg::DaemonUpgrade, T5, w).unwrap();
        let id = peer.expect("daemon.upgrade")["id"].as_u64().unwrap();
        peer.reply(id, Ok(Value::Null));
        assert_eq!(r.recv_timeout(T5).unwrap(), Ok(Value::Null));
        let (w, r) = slot();
        link.request(ClientMsg::DaemonUpgrade, T5, w).unwrap();
        let id = peer.expect("daemon.upgrade")["id"].as_u64().unwrap();
        peer.reply(id, Err("boom".into()));
        assert_eq!(r.recv_timeout(T5).unwrap(), Err(HostError::remote("boom")));
    }

    #[test]
    fn pending_fail_on_eof() {
        let (io, mut peer) = pair();
        peer.greet();
        let rec = Rec::new();
        let link = up(io, &rec);
        let (w, r) = slot();
        link.request(ClientMsg::DaemonUpgrade, T5, w).unwrap();
        peer.expect("daemon.upgrade");
        drop(peer);
        let e = r.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Offline);
        assert!(e.message.contains("connection lost"), "{e:?}");
        rec.wait_for(|e| e.iter().any(|e| matches!(e, Ev::Closed(_))));
        assert!(link.is_closed());
        assert_eq!(
            link.notify(ClientMsg::DaemonUpgrade).unwrap_err().code,
            HostErrorCode::Offline
        );
    }

    #[test]
    fn call_timeout_then_late_reply_ignored() {
        let (io, mut peer) = pair();
        peer.greet();
        let link = up(io, &Rec::new());
        let (w, r) = slot();
        link.request(ClientMsg::DaemonUpgrade, Duration::from_millis(100), w)
            .unwrap();
        let id = peer.expect("daemon.upgrade")["id"].as_u64().unwrap();
        let e = r.recv_timeout(T5).unwrap().unwrap_err();
        assert_eq!(e.code, HostErrorCode::Timeout);
        peer.reply(id, Ok(json!(1)));
        // The link still works, and the late reply reached nobody.
        let (w2, r2) = slot();
        link.request(ClientMsg::DaemonUpgrade, T5, w2).unwrap();
        let id2 = peer.expect("daemon.upgrade")["id"].as_u64().unwrap();
        assert_ne!(id, id2);
        peer.reply(id2, Ok(json!(2)));
        assert_eq!(r2.recv_timeout(T5).unwrap(), Ok(json!(2)));
        assert!(r.try_recv().is_err());
    }

    #[test]
    fn output_routed_by_uuid() {
        let (io, mut peer) = pair();
        peer.greet();
        let rec = Rec::new();
        let _link = up(io, &rec);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut bytes = out_frame(a, b"A1");
        bytes.extend(out_frame(b, b"B1"));
        bytes.extend(out_frame(a, b"A2"));
        peer.write(&bytes);
        let evs = rec.wait_for(|e| e.len() >= 4);
        assert_eq!(
            &evs[1..],
            &[
                Ev::Out(a, b"A1".to_vec()),
                Ev::Out(b, b"B1".to_vec()),
                Ev::Out(a, b"A2".to_vec())
            ]
        );
    }

    #[test]
    fn terminals_and_exit_delivered() {
        let (io, mut peer) = pair();
        peer.greet();
        let rec = Rec::new();
        let _link = up(io, &rec);
        let t = Uuid::new_v4();
        let mut b = out_frame(t, b"bye");
        b.extend(msg_frame(&ServerMsg::TermExit {
            terminal: t,
            code: 3,
        }));
        b.extend(terminals_frame(vec![info(t)]));
        peer.write(&b);
        drop(peer);
        let evs = rec.wait_for(|e| e.iter().any(|e| matches!(e, Ev::Closed(_))));
        assert_eq!(
            evs,
            vec![
                Ev::List(vec![]),
                Ev::Out(t, b"bye".to_vec()),
                Ev::Exit(t, 3),
                Ev::List(vec![info(t)]),
                Ev::Closed("connection lost".into()),
            ]
        );
    }

    #[test]
    fn unknown_kind_ignored() {
        let (io, mut peer) = pair();
        peer.greet();
        let rec = Rec::new();
        let link = up(io, &rec);
        let mut b = vec![0, 0, 0, 3, 7, b'z', b'z'];
        b.extend(terminals_frame(vec![]));
        peer.write(&b);
        peer.write(&encode_raw_json(br#"{"t":"future.thing","x":1}"#));
        let (w, r) = slot();
        link.request(ClientMsg::DaemonUpgrade, T5, w).unwrap();
        let id = peer.expect("daemon.upgrade")["id"].as_u64().unwrap();
        peer.reply(id, Ok(json!(true)));
        assert_eq!(r.recv_timeout(T5).unwrap(), Ok(json!(true)));
        assert!(!link.is_closed());
    }

    fn encode_raw_json(j: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        xshell_protocol::frame::encode_json(j, &mut v).unwrap();
        v
    }

    #[test]
    fn malformed_frame_closes_with_protocol_error() {
        let (io, mut peer) = pair();
        peer.greet();
        let rec = Rec::new();
        let link = up(io, &rec);
        peer.write(&encode_raw_json(b"not json"));
        let evs = rec.wait_for(|e| e.iter().any(|e| matches!(e, Ev::Closed(_))));
        let Some(Ev::Closed(why)) = evs.last() else {
            panic!()
        };
        assert!(why.starts_with("protocol error: "), "{why}");
        assert!(link.is_closed());
    }

    #[test]
    fn input_has_no_id() {
        let (io, mut peer) = pair();
        peer.greet();
        let link = up(io, &Rec::new());
        let t = Uuid::new_v4();
        link.notify(ClientMsg::TermInput {
            terminal: t,
            data: "x".into(),
        })
        .unwrap();
        let v = peer.expect("term.input");
        assert!(v.get("id").is_none(), "{v}");
        assert_eq!(v["data"], "x");
    }

    /// A `Write` that blocks inside `write` until the gate opens, and says when it blocked.
    struct Gated {
        entered: mpsc::Sender<()>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        inner: Box<dyn Write + Send>,
    }

    impl Write for Gated {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            let (m, cv) = &*self.gate;
            let mut open = m.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            self.inner.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    #[test]
    fn full_queue_returns_busy() {
        let (mut io, mut peer) = pair();
        let (etx, erx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        io.write = Box::new(Gated {
            entered: etx,
            gate: gate.clone(),
            inner: io.write,
        });
        peer.greet();
        let input = |n: u8| ClientMsg::TermInput {
            terminal: Uuid::nil(),
            data: format!("{n:03}"),
        };
        let frame_len = encode_msg(&input(0), None).unwrap().len();
        let limits = LinkLimits {
            queue_bytes: 3 * frame_len,
            max_pending: 2,
        };
        let (link, _, _) = Link::establish_with(io, R11, "1.5.0", Rec::new(), T5, limits).unwrap();
        // The writer took the hello and is blocked writing it: the queue is empty.
        erx.recv_timeout(T5).unwrap();
        let start = Instant::now();
        for n in 0..3 {
            link.notify(input(n)).unwrap();
        }
        assert_eq!(link.notify(input(3)).unwrap_err().code, HostErrorCode::Busy);
        // Pending-request cap: the waiter of a refused request is dropped, not called.
        let (w1, _r1) = slot();
        let (w2, _r2) = slot();
        let (w3, r3) = slot();
        // These need queue room too: free it by letting the writer go.
        {
            let (m, cv) = &*gate;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        for n in 0..3 {
            assert_eq!(peer.expect("term.input")["data"], format!("{n:03}"));
        }
        link.request(ClientMsg::DaemonUpgrade, T5, w1).unwrap();
        link.request(ClientMsg::DaemonUpgrade, T5, w2).unwrap();
        let e = link.request(ClientMsg::DaemonUpgrade, T5, w3).unwrap_err();
        assert_eq!(e.code, HostErrorCode::Busy);
        assert!(r3.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}

//! A TCP proxy in front of the workerd Relay that injects faults at exact protocol points.
//! It forwards every byte unchanged (the Relay sees the proxy's own origin in the `Host`
//! header, which is what the clients sign), and parses only the client's WebSocket frames
//! on Ring connections (`/v1/ring/…`), so a fault starts right after (or at) a named frame:
//!
//! - [`Fault::DeafAfter`]: the frame reaches the Relay, every Relay answer after it on that
//!   connection is dropped (a lost acknowledgment after the Relay stored a publication);
//! - [`Fault::HoldFrom`]: the frame and everything after it on that connection are held
//!   until [`FaultProxy::release`] (a publication that has not landed yet);
//! - [`Fault::RefuseAuth`]: an `auth` from one of the listed keys never reaches the Relay;
//!   the proxy answers it as a Relay refusal (`error` frame, close 4003).
//!
//! [`FaultProxy::set_down`] closes every new connection at once (Relay unreachable) and
//! [`FaultProxy::cut`] closes every open one.
#![allow(dead_code)]
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Debug)]
pub enum Fault {
    DeafAfter(&'static str),
    HoldFrom(&'static str),
    RefuseAuth {
        keys: Vec<String>,
        code: &'static str,
        detail: Option<&'static str>,
    },
}

/// One client frame the proxy saw on a Ring connection.
#[derive(Clone, Debug)]
pub struct Seen {
    pub t: String,
    pub sign_key: Option<String>,
    pub at: Instant,
    /// The proxy answered it as a refusal (it never reached the Relay).
    pub refused: bool,
}

#[derive(Default)]
struct Shared {
    fault: Mutex<Option<Fault>>,
    down: AtomicBool,
    conns: Mutex<Vec<Arc<Conn>>>,
    seen: Mutex<Vec<Seen>>,
    /// Bytes held by [`Fault::HoldFrom`], per connection, in order.
    held: Mutex<Vec<(Arc<Conn>, Vec<u8>)>>,
}

struct Conn {
    client: TcpStream,
    upstream: TcpStream,
    /// Drop the Relay's bytes from now on.
    deaf: AtomicBool,
    /// Hold the client's bytes from now on.
    holding: AtomicBool,
    closed: AtomicBool,
    write: Mutex<()>,
}

impl Conn {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.client.shutdown(Shutdown::Both);
        let _ = self.upstream.shutdown(Shutdown::Both);
    }
}

pub struct FaultProxy {
    port: u16,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

impl FaultProxy {
    /// A proxy on a free loopback port forwarding to `upstream` (`ws://127.0.0.1:<port>`).
    pub fn start(upstream: &str) -> Self {
        let target = upstream
            .trim_start_matches("ws://")
            .trim_end_matches('/')
            .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (shared.clone(), stop.clone());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                if st.load(Ordering::Acquire) {
                    return;
                }
                let Ok(client) = client else { continue };
                if s.down.load(Ordering::Acquire) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let Ok(upstream) = TcpStream::connect(&target) else {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                };
                let _ = client.set_nodelay(true);
                let _ = upstream.set_nodelay(true);
                let conn = Arc::new(Conn {
                    client,
                    upstream,
                    deaf: AtomicBool::new(false),
                    holding: AtomicBool::new(false),
                    closed: AtomicBool::new(false),
                    write: Mutex::new(()),
                });
                s.conns.lock().unwrap().push(conn.clone());
                let (c1, s1) = (conn.clone(), s.clone());
                std::thread::spawn(move || upward(c1, s1));
                std::thread::spawn(move || downward(conn));
            }
        });
        Self { port, shared, stop }
    }

    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    pub fn set_fault(&self, fault: Option<Fault>) {
        *self.shared.fault.lock().unwrap() = fault;
    }

    pub fn set_down(&self, down: bool) {
        self.shared.down.store(down, Ordering::Release);
    }

    /// Closes every open connection.
    pub fn cut(&self) {
        for c in self.shared.conns.lock().unwrap().drain(..) {
            c.close();
        }
    }

    /// Sends what [`Fault::HoldFrom`] held, in order, and stops holding.
    pub fn release(&self) {
        self.set_fault(None);
        // Under the hold's lock: a frame arriving meanwhile goes after the held bytes.
        let mut held = self.shared.held.lock().unwrap();
        for (conn, bytes) in held.drain(..) {
            let _ = (&conn.upstream).write_all(&bytes);
            conn.holding.store(false, Ordering::Release);
        }
        for conn in self.shared.conns.lock().unwrap().iter() {
            conn.holding.store(false, Ordering::Release);
        }
    }

    /// Whether something is held right now.
    pub fn holding(&self) -> bool {
        !self.shared.held.lock().unwrap().is_empty()
    }

    /// The client frames seen on Ring connections so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    /// `auth` frames from `key` (refused by the proxy or forwarded).
    pub fn auths(&self, key: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.t == "auth" && s.sign_key.as_deref() == Some(key))
            .collect()
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.cut();
    }
}

/// Relay → client: unchanged, unless the connection went deaf.
fn downward(conn: Arc<Conn>) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match (&conn.upstream).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if conn.deaf.load(Ordering::Acquire) {
            continue;
        }
        let _w = conn.write.lock().unwrap();
        if conn.closed.load(Ordering::Acquire) || (&conn.client).write_all(&buf[..n]).is_err() {
            break;
        }
    }
    conn.close();
}

/// Client → Relay: the HTTP upgrade request, then frame by frame.
fn upward(conn: Arc<Conn>, shared: Arc<Shared>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut ring: Option<bool> = None;
    loop {
        let n = match (&conn.client).read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        if ring.is_none() {
            let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head: Vec<u8> = pending.drain(..end + 4).collect();
            let line = String::from_utf8_lossy(&head);
            ring = Some(line.lines().next().is_some_and(|l| l.contains("/v1/ring/")));
            if !forward(&conn, &shared, &head) {
                break;
            }
        }
        if ring != Some(true) {
            if !pending.is_empty() && !forward(&conn, &shared, &pending) {
                break;
            }
            pending.clear();
            continue;
        }
        // Whole frames only; a partial frame waits for more bytes.
        while let Some((len, payload)) = frame(&pending) {
            let raw: Vec<u8> = pending.drain(..len).collect();
            let value: Option<serde_json::Value> = serde_json::from_slice(&payload).ok();
            let t = value
                .as_ref()
                .and_then(|v| v["t"].as_str())
                .unwrap_or("")
                .to_string();
            let sign_key = value
                .as_ref()
                .and_then(|v| v["signKey"].as_str())
                .map(str::to_string);
            let fault = shared.fault.lock().unwrap().clone();
            let refuse = match &fault {
                Some(Fault::RefuseAuth { keys, code, detail })
                    if t == "auth" && sign_key.as_ref().is_some_and(|k| keys.contains(k)) =>
                {
                    Some((*code, *detail))
                }
                _ => None,
            };
            if !t.is_empty() && t != "ping" {
                shared.seen.lock().unwrap().push(Seen {
                    t: t.clone(),
                    sign_key: sign_key.clone(),
                    at: Instant::now(),
                    refused: refuse.is_some(),
                });
            }
            if let Some((code, detail)) = refuse {
                // Answer as the Relay would, and close: the Relay never saw this auth.
                conn.deaf.store(true, Ordering::Release);
                let mut error = serde_json::json!({"t": "error", "code": code});
                if let Some(d) = detail {
                    error["detail"] = d.into();
                }
                {
                    let _w = conn.write.lock().unwrap();
                    let _ =
                        (&conn.client).write_all(&server_frame(0x1, error.to_string().as_bytes()));
                    let mut close = 4003u16.to_be_bytes().to_vec();
                    close.extend_from_slice(code.as_bytes());
                    let _ = (&conn.client).write_all(&server_frame(0x8, &close));
                }
                conn.close();
                return;
            }
            if let Some(Fault::HoldFrom(at)) = &fault {
                if t == *at {
                    conn.holding.store(true, Ordering::Release);
                }
            }
            if !forward(&conn, &shared, &raw) {
                conn.close();
                return;
            }
            if let Some(Fault::DeafAfter(at)) = &fault {
                if t == *at {
                    conn.deaf.store(true, Ordering::Release);
                }
            }
        }
    }
    conn.close();
}

/// Upstream, or into the hold.
fn forward(conn: &Arc<Conn>, shared: &Shared, bytes: &[u8]) -> bool {
    let mut held = shared.held.lock().unwrap();
    if conn.holding.load(Ordering::Acquire) {
        match held.iter_mut().find(|(c, _)| Arc::ptr_eq(c, conn)) {
            Some((_, b)) => b.extend_from_slice(bytes),
            None => held.push((conn.clone(), bytes.to_vec())),
        }
        return true;
    }
    drop(held);
    (&conn.upstream).write_all(bytes).is_ok()
}

/// One complete client frame at the start of `b`: its length and unmasked payload.
fn frame(b: &[u8]) -> Option<(usize, Vec<u8>)> {
    if b.len() < 2 {
        return None;
    }
    let masked = b[1] & 0x80 != 0;
    let (len, mut at) = match b[1] & 0x7f {
        126 => {
            if b.len() < 4 {
                return None;
            }
            (u16::from_be_bytes([b[2], b[3]]) as usize, 4)
        }
        127 => {
            if b.len() < 10 {
                return None;
            }
            (
                u64::from_be_bytes(b[2..10].try_into().unwrap()) as usize,
                10,
            )
        }
        n => (n as usize, 2),
    };
    let mask = if masked {
        if b.len() < at + 4 {
            return None;
        }
        let m = [b[at], b[at + 1], b[at + 2], b[at + 3]];
        at += 4;
        Some(m)
    } else {
        None
    };
    if b.len() < at + len {
        return None;
    }
    let payload = b[at..at + len]
        .iter()
        .enumerate()
        .map(|(i, x)| mask.map_or(*x, |m| x ^ m[i % 4]))
        .collect();
    Some((at + len, payload))
}

/// An unmasked server frame.
fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | opcode];
    if payload.len() < 126 {
        f.push(payload.len() as u8);
    } else {
        f.push(126);
        f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

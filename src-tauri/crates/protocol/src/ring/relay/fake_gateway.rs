//! A fake Push Gateway (test support only): plain HTTP/1.1 on 127.0.0.1, `POST /v1/push`
//! only. It records every request body verbatim and answers from a script: one-shot replies
//! queued with [`FakeGateway::respond`], a delay for every reply, and a hold that keeps
//! requests waiting until it is released. By default it answers `200 {"delivered":true}`.

use serde_json::Value;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// One reply: HTTP status and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn delivered() -> Reply {
        Reply {
            status: 200,
            body: r#"{"delivered":true}"#.into(),
        }
    }

    /// A gateway refusal: `{"error":{"code","message"}}`.
    pub fn refusal(status: u16, code: &str) -> Reply {
        Reply {
            status,
            body: serde_json::json!({"error": {"code": code, "message": code}}).to_string(),
        }
    }
}

#[derive(Default)]
struct St {
    /// `(path, body)` of every request, in arrival order.
    requests: Vec<(String, String)>,
    script: VecDeque<(Reply, Option<Duration>)>,
    delay: Duration,
    hold: bool,
}

struct Shared {
    st: Mutex<St>,
    cv: Condvar,
    stop: AtomicBool,
}

/// See the module docs. Dropping it stops accepting.
pub struct FakeGateway {
    shared: Arc<Shared>,
    addr: SocketAddr,
}

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, St> {
    s.st.lock().unwrap_or_else(|e| e.into_inner())
}

impl FakeGateway {
    pub fn start() -> FakeGateway {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake gateway");
        let addr = listener.local_addr().expect("fake gateway address");
        listener.set_nonblocking(true).expect("nonblocking");
        let shared = Arc::new(Shared {
            st: Mutex::new(St::default()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        thread::spawn(move || {
            while !s.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((tcp, _)) => {
                        let s = s.clone();
                        thread::spawn(move || serve(&s, tcp));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        FakeGateway { shared, addr }
    }

    /// The base URL a Relay is configured with (`http://127.0.0.1:<port>`).
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Answers the next request (in arrival order) with `reply`, once.
    pub fn respond(&self, reply: Reply) {
        lock(&self.shared).script.push_back((reply, None));
    }

    /// Answers the next request with `reply` after `delay` (instead of the general delay),
    /// once.
    pub fn respond_after(&self, reply: Reply, delay: Duration) {
        lock(&self.shared).script.push_back((reply, Some(delay)));
    }

    /// Waits this long before every reply.
    pub fn set_delay(&self, d: Duration) {
        lock(&self.shared).delay = d;
    }

    /// While on, requests are recorded at once but answered only once it is off again.
    pub fn hold(&self, on: bool) {
        lock(&self.shared).hold = on;
        self.shared.cv.notify_all();
    }

    /// The bodies of every `POST /v1/push` so far, parsed.
    pub fn requests(&self) -> Vec<Value> {
        lock(&self.shared)
            .requests
            .iter()
            .filter(|(p, _)| p == "/v1/push")
            .map(|(_, b)| serde_json::from_str(b).unwrap_or(Value::Null))
            .collect()
    }

    /// The raw bodies of every request, with their paths.
    pub fn raw_requests(&self) -> Vec<(String, String)> {
        lock(&self.shared).requests.clone()
    }

    /// Waits until at least `n` pushes arrived (or `timeout`); the pushes so far.
    pub fn wait_requests(&self, n: usize, timeout: Duration) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let r = self.requests();
            if r.len() >= n || Instant::now() >= deadline {
                return r;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.hold(false);
    }
}

/// Reads one request (head and `Content-Length` body) and answers it.
fn serve(s: &Shared, mut tcp: TcpStream) {
    let _ = tcp.set_nonblocking(false);
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
        match tcp.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, path) = (
        first.next().unwrap_or("").to_string(),
        first.next().unwrap_or("").to_string(),
    );
    let len = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        match tcp.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).into_owned();
    let reply = if method == "POST" && path == "/v1/push" {
        let (reply, delay) = {
            let mut st = lock(s);
            st.requests.push((path, body));
            let (reply, own) = st
                .script
                .pop_front()
                .unwrap_or_else(|| (Reply::delivered(), None));
            let delay = own.unwrap_or(st.delay);
            while st.hold && !s.stop.load(Ordering::Acquire) {
                st = s.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            (reply, delay)
        };
        thread::sleep(delay);
        reply
    } else {
        Reply::refusal(404, "not_found")
    };
    let reason = if reply.status == 200 { "OK" } else { "Error" };
    let _ = write!(
        tcp,
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    let _ = tcp.flush();
}

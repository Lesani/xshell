//! A loopback stand-in for the release server (`get.xshell.sh`): plain HTTP/1.1 on
//! 127.0.0.1, one thread per connection, routes set by the test, a log of every request
//! path, gates that hold a route's answer until released, and status and redirect answers.
//! No test Daemon or Desktop may reach the real release server: the ones that need no
//! stub get [`closed_get_url`].
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A local URL nothing listens on: every request to it fails at once.
pub fn closed_get_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

/// Point this test process's `XSHELL_GET_URL` at a closed local port, once: every Daemon
/// it starts (in process, or as a child that inherits the environment, such as the Local
/// Host a Manager launches) then has no way to the real release server unless a test gives
/// it a stub. Called by `TestHome::new`; sites that start Daemons without a `TestHome` set
/// the variable or `Config::get_base` themselves.
pub fn no_release_server() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("XSHELL_GET_URL", closed_get_url()));
}

#[derive(Clone)]
pub enum Route {
    /// 200 with this body.
    Body(Arc<Vec<u8>>),
    /// This status with an empty body.
    Status(u16),
    /// 302 to this location (a path on the stub, or a full URL).
    Redirect(String),
}

#[derive(Default)]
struct State {
    routes: HashMap<String, Route>,
    hits: Vec<String>,
    gates: HashMap<String, bool>,
}

pub struct GetStub {
    port: u16,
    state: Arc<(Mutex<State>, Condvar)>,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl GetStub {
    pub fn start() -> GetStub {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let state: Arc<(Mutex<State>, Condvar)> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        let accept = std::thread::spawn(move || {
            for c in l.incoming() {
                if sp.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(c) = c else { continue };
                let st = st.clone();
                std::thread::spawn(move || serve(c, &st));
            }
        });
        GetStub {
            port,
            state,
            stop,
            accept: Some(accept),
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn set(&self, path: &str, route: Route) {
        let (m, cv) = &*self.state;
        m.lock().unwrap().routes.insert(path.into(), route);
        cv.notify_all();
    }

    pub fn body(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.set(path, Route::Body(Arc::new(body.into())));
    }

    pub fn remove(&self, path: &str) {
        self.state.0.lock().unwrap().routes.remove(path);
    }

    /// Every request path so far, in order.
    pub fn hits(&self) -> Vec<String> {
        self.state.0.lock().unwrap().hits.clone()
    }

    pub fn count(&self, path: &str) -> usize {
        self.hits().iter().filter(|h| *h == path).count()
    }

    /// Requests to `path` wait (after being logged) until [`GetStub::open`].
    pub fn gate(&self, path: &str) {
        self.state.0.lock().unwrap().gates.insert(path.into(), true);
    }

    pub fn open(&self, path: &str) {
        let (m, cv) = &*self.state;
        m.lock().unwrap().gates.remove(path);
        cv.notify_all();
    }

    /// Wait until `path` was requested `n` times.
    pub fn wait_hits(&self, path: &str, n: usize, within: Duration) {
        let until = Instant::now() + within;
        while self.count(path) < n {
            assert!(
                Instant::now() < until,
                "{path} not requested {n} times: {:?}",
                self.hits()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Serve `version` as the latest release with `bytes` as its build for `triple` and
    /// `sha256` (the real digest when `None`) in its checksum file.
    pub fn release(&self, version: &str, triple: &str, bytes: &[u8], sha256: Option<&str>) {
        use sha2::Digest;
        let digest = sha256
            .map(str::to_string)
            .unwrap_or_else(|| format!("{:x}", sha2::Sha256::digest(bytes)));
        self.body("/latest", format!("{version}\n"));
        let asset = Self::asset(version, triple);
        self.body(
            &format!("{asset}.sha256"),
            format!("{digest}  xshelld-{triple}\n"),
        );
        self.body(&asset, bytes.to_vec());
    }

    /// The path of `version`'s build for `triple`.
    pub fn asset(version: &str, triple: &str) -> String {
        format!("/release/v{version}/xshelld-{triple}")
    }
}

impl Drop for GetStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        {
            let (m, cv) = &*self.state;
            m.lock().unwrap().gates.clear();
            cv.notify_all();
        }
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn serve(c: TcpStream, state: &(Mutex<State>, Condvar)) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(30)));
    let mut r = BufReader::new(match c.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let route = {
        let (m, cv) = state;
        let mut s = m.lock().unwrap();
        s.hits.push(path.clone());
        while s.gates.get(&path).copied().unwrap_or(false) {
            s = cv.wait(s).unwrap();
        }
        s.routes.get(&path).cloned()
    };
    let mut w = c;
    let _ = match route {
        Some(Route::Body(b)) => w
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    b.len()
                )
                .as_bytes(),
            )
            .and_then(|()| w.write_all(&b)),
        Some(Route::Status(code)) => w.write_all(
            format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        ),
        Some(Route::Redirect(to)) => w.write_all(
            format!(
                "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            )
            .as_bytes(),
        ),
        None => w.write_all(
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        ),
    };
    let _ = w.flush();
}

//! Dialing a Relay: TCP, TLS through rustls (ring provider, webpki roots unless the caller
//! brings its own `ClientConfig`), then the WebSocket upgrade through tungstenite. The stream
//! runs in one of two modes: blocking with an absolute deadline (dialing, the handshake,
//! authentication) or non-blocking under the IO loop's `poll`.

use super::super::url::RelayUrl;
use super::super::RingError;
use super::wire::MAX_FRAME;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Instant;
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::WebSocket;

/// The default TLS configuration: rustls with the `ring` provider and the Mozilla roots.
pub fn default_tls() -> Result<Arc<ClientConfig>, RingError> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| RingError::Connect(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// A TCP stream that, while it has a deadline, blocks at most until then and fails with
/// `TimedOut` after; without one it is whatever mode the socket is in.
pub(crate) struct Sock {
    tcp: TcpStream,
    deadline: Option<Instant>,
    budget: Budget,
}

/// A cap on bytes read from the operating system; reads past it report `WouldBlock`. It sits
/// under rustls, so TLS records that carry no plaintext (empty application data, key
/// updates) count against it too, and rustls keeps its state across the `WouldBlock`.
#[derive(Default)]
struct Budget {
    left: Option<usize>,
    spent: bool,
}

impl Sock {
    fn remaining(&self) -> io::Result<Option<std::time::Duration>> {
        match self.deadline {
            None => Ok(None),
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    Err(io::ErrorKind::TimedOut.into())
                } else {
                    Ok(Some(left))
                }
            }
        }
    }
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let buf = match self.budget.left {
            Some(0) => {
                self.budget.spent = true;
                return Err(io::ErrorKind::WouldBlock.into());
            }
            Some(n) => {
                let len = buf.len().min(n);
                &mut buf[..len]
            }
            None => buf,
        };
        if let Some(left) = self.remaining()? {
            self.tcp.set_read_timeout(Some(left))?;
        }
        let n = self.tcp.read(buf)?;
        if let Some(left) = &mut self.budget.left {
            *left -= n;
        }
        Ok(n)
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(left) = self.remaining()? {
            self.tcp.set_write_timeout(Some(left))?;
        }
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

/// A connection to a Relay, with or without TLS.
pub(crate) struct Conn {
    stream: Stream,
}

enum Stream {
    Plain(Sock),
    Tls(Box<StreamOwned<ClientConnection, Sock>>),
}

impl Conn {
    fn sock(&self) -> &Sock {
        match &self.stream {
            Stream::Plain(s) => s,
            Stream::Tls(t) => &t.sock,
        }
    }

    fn sock_mut(&mut self) -> &mut Sock {
        match &mut self.stream {
            Stream::Plain(s) => s,
            Stream::Tls(t) => &mut t.sock,
        }
    }

    /// Caps the bytes the next reads may take from the socket (`None`: no cap).
    pub(crate) fn set_read_budget(&mut self, budget: Option<usize>) {
        self.sock_mut().budget = Budget {
            left: budget,
            spent: false,
        };
    }

    /// Whether a read hit the cap since it was set.
    pub(crate) fn read_budget_spent(&self) -> bool {
        self.sock().budget.spent
    }

    pub(crate) fn tcp(&self) -> &TcpStream {
        &self.sock().tcp
    }

    /// Blocking until `deadline`.
    #[cfg_attr(not(feature = "test-relay"), allow(dead_code))] // the contract's raw connections
    pub(crate) fn set_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        let s = self.sock_mut();
        s.tcp.set_nonblocking(false)?;
        s.deadline = Some(deadline);
        Ok(())
    }

    /// Non-blocking, for the IO loop.
    pub(crate) fn set_nonblocking(&mut self) -> io::Result<()> {
        let s = self.sock_mut();
        s.deadline = None;
        s.tcp.set_read_timeout(None)?;
        s.tcp.set_write_timeout(None)?;
        s.tcp.set_nonblocking(true)
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.stream {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.stream {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.stream {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(t) => t.flush(),
        }
    }
}

pub(crate) fn ws_config(write_buffer_max: usize) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
        // Small reads, so the per-turn read budget is meaningful.
        .read_buffer_size(16 * 1024)
        // Write through at once; buffer only what the socket refuses, up to the cap.
        .write_buffer_size(0)
        .max_write_buffer_size(write_buffer_max.max(MAX_FRAME + 1024))
}

pub(crate) fn is_would_block(e: &io::Error) -> bool {
    // Windows reports an expired socket timeout as TimedOut, Unix as WouldBlock.
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn connect_tcp(url: &RelayUrl, deadline: Instant) -> Result<TcpStream, RingError> {
    let addrs: Vec<SocketAddr> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| RingError::Connect(format!("resolving {}: {e}", url.host)))?
        .collect();
    let mut last = RingError::Connect(format!("{} has no address", url.host));
    for addr in addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(RingError::Timeout);
        }
        match TcpStream::connect_timeout(&addr, left) {
            Ok(tcp) => {
                let _ = tcp.set_nodelay(true);
                return Ok(tcp);
            }
            Err(e) => last = RingError::Connect(format!("{addr}: {e}")),
        }
    }
    Err(last)
}

/// Dials `endpoint` on `url`'s host, blocking until `deadline` at most. The returned socket
/// still has the deadline set.
pub(crate) fn dial(
    url: &RelayUrl,
    endpoint: &str,
    tls: Option<Arc<ClientConfig>>,
    deadline: Instant,
    write_buffer_max: usize,
) -> Result<WebSocket<Conn>, RingError> {
    let tcp = connect_tcp(url, deadline)?;
    let sock = Sock {
        tcp,
        deadline: Some(deadline),
        budget: Budget::default(),
    };
    let conn = if url.tls {
        let cfg = match tls {
            Some(c) => c,
            None => default_tls()?,
        };
        let name = ServerName::try_from(url.host.clone())
            .map_err(|e| RingError::Connect(format!("server name {}: {e}", url.host)))?;
        let tls =
            ClientConnection::new(cfg, name).map_err(|e| RingError::Connect(e.to_string()))?;
        Stream::Tls(Box::new(StreamOwned::new(tls, sock)))
    } else {
        Stream::Plain(sock)
    };
    let conn = Conn { stream: conn };
    let mut r =
        tungstenite::client::client_with_config(endpoint, conn, Some(ws_config(write_buffer_max)));
    loop {
        match r {
            Ok((ws, _)) => return Ok(ws),
            Err(HandshakeError::Interrupted(mid)) => r = mid.handshake(),
            Err(HandshakeError::Failure(e)) => return Err(map_ws_error(e)),
        }
    }
}

pub(crate) fn map_ws_error(e: tungstenite::Error) -> RingError {
    match e {
        tungstenite::Error::Io(io) if io.kind() == io::ErrorKind::TimedOut => RingError::Timeout,
        tungstenite::Error::Http(resp) => {
            RingError::Connect(format!("relay answered HTTP {}", resp.status()))
        }
        other => RingError::Connect(other.to_string()),
    }
}

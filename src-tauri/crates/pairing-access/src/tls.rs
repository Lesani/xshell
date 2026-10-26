//! Standard TLS 1.3, with a single opposite leaf as the only trust anchor. The explicit
//! post-handshake equality/chain check runs before application data is accepted or sent.
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
use xshell_protocol::pairing_access::{AccessError, Certificate, CERT_MAX, SERVER_NAME};
use zeroize::Zeroizing;

pub const HANDSHAKE: Duration = Duration::from_secs(3);
pub const READ: Duration = Duration::from_secs(2);
pub const WRITE: Duration = Duration::from_secs(1);

/// No Debug/Serialize implementation: private TLS material never crosses a public seam.
pub struct Identity {
    pub certificate: Certificate,
    key: Zeroizing<Vec<u8>>,
}
impl Identity {
    pub fn generate(server: bool, expires_at: u64) -> Result<Self, AccessError> {
        let now = time::OffsetDateTime::now_utc();
        let mut params = rcgen::CertificateParams::new(if server {
            vec![SERVER_NAME.into()]
        } else {
            vec![]
        })
        .map_err(|_| AccessError::InvalidRequest)?;
        params.not_before = now - time::Duration::seconds(30);
        params.not_after = time::OffsetDateTime::from_unix_timestamp(expires_at as i64)
            .map_err(|_| AccessError::InvalidRequest)?;
        params.extended_key_usages = vec![if server {
            rcgen::ExtendedKeyUsagePurpose::ServerAuth
        } else {
            rcgen::ExtendedKeyUsagePurpose::ClientAuth
        }];
        let key = rcgen::KeyPair::generate().map_err(|_| AccessError::InvalidRequest)?;
        let certificate = params
            .self_signed(&key)
            .map_err(|_| AccessError::InvalidRequest)?;
        Ok(Self {
            certificate: Certificate(certificate.der().to_vec()),
            key: Zeroizing::new(key.serialize_der()),
        })
    }
    fn certified_key(&self) -> Result<rustls::sign::CertifiedKey, AccessError> {
        // Use rustls's standard ring signer with borrowed DER. PrivateKeyDer implements
        // Zeroize but does not wipe on drop; avoid making an unguarded serialized copy.
        let der = PrivatePkcs8KeyDer::from(self.key.as_slice()).into();
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&der)
            .map_err(|_| AccessError::InvalidRequest)?;
        let key = rustls::sign::CertifiedKey::new(self.chain(), signing_key);
        key.keys_match().map_err(|_| AccessError::InvalidRequest)?;
        Ok(key)
    }
    fn chain(&self) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from(self.certificate.0.clone())]
    }
}
pub fn fingerprint(certificate: &Certificate) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, &certificate.0)
        .as_ref()
        .try_into()
        .unwrap()
}
fn roots(peer: &Certificate) -> Result<RootCertStore, AccessError> {
    if peer.0.is_empty() || peer.0.len() > CERT_MAX {
        return Err(AccessError::InvalidRequest);
    }
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(peer.0.clone()))
        .map_err(|_| AccessError::InvalidRequest)?;
    Ok(roots)
}
pub fn client_config(
    identity: &Identity,
    peer: &Certificate,
) -> Result<Arc<ClientConfig>, AccessError> {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| AccessError::InvalidRequest)?
            .with_root_certificates(roots(peer)?)
            .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                identity.certified_key()?,
            )));
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_early_data = false;
    Ok(Arc::new(config))
}
pub fn check_peer(
    chain: Option<&[CertificateDer<'_>]>,
    peer: &Certificate,
) -> Result<(), AccessError> {
    match chain {
        Some([leaf]) if leaf.len() <= CERT_MAX && leaf.as_ref() == peer.0 => Ok(()),
        _ => Err(AccessError::Unauthorized),
    }
}
/// A deadline is checked and its *remaining* duration installed on EVERY underlying read
/// and write, including rustls complete_io loops. TCP timeouts alone are sliding deadlines.
pub struct DeadlineSocket {
    socket: TcpStream,
    deadline: Instant,
}
impl DeadlineSocket {
    pub fn new(socket: TcpStream) -> Self {
        Self {
            socket,
            deadline: Instant::now() + HANDSHAKE,
        }
    }
    pub fn deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "pairing access deadline"))
    }
}
impl Read for DeadlineSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(buf)
    }
}
impl Write for DeadlineSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.socket.flush()
    }
}
pub type ClientStream = rustls::StreamOwned<ClientConnection, DeadlineSocket>;
pub fn connect(
    endpoint: std::net::SocketAddr,
    config: Arc<ClientConfig>,
    peer: &Certificate,
) -> Result<ClientStream, AccessError> {
    let deadline = Instant::now() + HANDSHAKE;
    let socket = TcpStream::connect_timeout(&endpoint, HANDSHAKE)
        .map_err(|_| AccessError::TransportClosedOutcomeUnknown)?;
    let connection = ClientConnection::new(
        config,
        ServerName::try_from(SERVER_NAME).unwrap().to_owned(),
    )
    .map_err(|_| AccessError::Unauthorized)?;
    let mut stream = rustls::StreamOwned::new(connection, DeadlineSocket::new(socket));
    stream.sock.deadline(deadline);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|_| AccessError::Unauthorized)?;
    }
    check_peer(stream.conn.peer_certificates(), peer)?;
    Ok(stream)
}

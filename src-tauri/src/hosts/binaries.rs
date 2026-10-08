//! Where the Desktop gets `xshelld` binaries for a Host's platform: next to the executable,
//! the cargo target dir (debug builds), then the GitHub release of this Desktop's version.

use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::Notify;
use xshell_hostlink::{BinarySource, CancelToken, ChainSource, DirSource};

const MAX_BINARY: u64 = 64 * 1024 * 1024;

pub fn release_repo() -> &'static str {
    option_env!("XSHELL_RELEASE_REPO").unwrap_or("MertPROJ/xshell")
}

pub fn release_base(repo: &str) -> String {
    format!("https://github.com/{repo}/releases/download")
}

pub fn asset_url(base: &str, version: &str, triple: &str) -> String {
    format!("{base}/v{version}/xshelld-{triple}")
}

pub fn sha256_url(base: &str, version: &str, triple: &str) -> String {
    format!("{}.sha256", asset_url(base, version, triple))
}

/// The digest from a `sha256sum` line (`<hex>  name`) or a bare hex digest.
pub fn parse_sha256(text: &str) -> Result<String, String> {
    let hex = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hex)
    } else {
        Err(format!("malformed sha256 sidecar: {:?}", text.trim()))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn verify_sha256(bytes: &[u8], sidecar: &str) -> Result<(), String> {
    let want = parse_sha256(sidecar)?;
    let got = sha256_hex(bytes);
    if got == want {
        Ok(())
    } else {
        Err(format!("checksum mismatch: expected {want}, got {got}"))
    }
}

/// Run `f` until it finishes or `cancel` fires (`None`).
async fn cancellable<F: Future>(f: F, cancel: Arc<Notify>) -> Option<F::Output> {
    let mut f = pin!(f);
    let mut stop = pin!(cancel.notified());
    std::future::poll_fn(|cx| {
        if stop.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        f.as_mut().poll(cx).map(Some)
    })
    .await
}

/// Downloads the release asset and its required `.sha256` sidecar, caching verified
/// binaries under `cache_dir/xshelld/<version>/`.
pub struct HttpReleaseSource {
    pub cache_dir: PathBuf,
    pub base: String,
    pub connect_timeout: Duration,
    pub timeout: Duration,
}

impl HttpReleaseSource {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            base: release_base(release_repo()),
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(120),
        }
    }

    fn cached(&self, version: &str, triple: &str) -> PathBuf {
        self.cache_dir
            .join("xshelld")
            .join(version)
            .join(format!("xshelld-{triple}"))
    }

    async fn download(&self, version: &str, triple: &str) -> Result<Vec<u8>, String> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            // As the updater does; fails only if another provider won the race.
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let client = reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .timeout(self.timeout)
            .user_agent(concat!("xshell/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        let get = |url: String| {
            let client = client.clone();
            async move {
                client
                    .get(&url)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| format!("{url}: {e}"))
            }
        };
        let sidecar = get(sha256_url(&self.base, version, triple))
            .await?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        let mut resp = get(asset_url(&self.base, version, triple)).await?;
        if resp.content_length().is_some_and(|n| n > MAX_BINARY) {
            return Err("the release asset is larger than 64 MiB".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 > MAX_BINARY {
                return Err("the release asset is larger than 64 MiB".into());
            }
        }
        verify_sha256(&bytes, &sidecar)?;
        Ok(bytes)
    }

    fn store(&self, path: &Path, bytes: &[u8]) {
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = dir.join(format!(".download.{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

impl BinarySource for HttpReleaseSource {
    fn fetch(&self, triple: &str, version: &str, cancel: &CancelToken) -> Result<Vec<u8>, String> {
        let path = self.cached(version, triple);
        if let Ok(b) = std::fs::read(&path) {
            return Ok(b);
        }
        let stop = Arc::new(Notify::new());
        let s = stop.clone();
        let _hook = cancel.on_cancel(move || s.notify_one());
        // The supervisor thread is not a runtime worker, so blocking here is allowed.
        let r = tauri::async_runtime::block_on(cancellable(self.download(version, triple), stop))
            .unwrap_or_else(|| Err("cancelled".into()))?;
        self.store(&path, &r);
        Ok(r)
    }
}

/// Debug builds: a daemon built with `cargo build -p xshelld --profile release-daemon
/// [--target <triple>]` in this workspace.
pub struct CargoArtifactSource(pub PathBuf);

/// The triple whose binary a plain (no `--target`) build of this workspace produces.
pub fn own_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

impl CargoArtifactSource {
    fn candidates(&self, triple: &str) -> Vec<PathBuf> {
        let mut v = vec![self.0.join(triple).join("release-daemon").join("xshelld")];
        if own_triple() == Some(triple) {
            v.push(self.0.join("release-daemon").join("xshelld"));
        }
        v
    }
}

impl BinarySource for CargoArtifactSource {
    fn fetch(&self, triple: &str, _v: &str, _c: &CancelToken) -> Result<Vec<u8>, String> {
        let c = self.candidates(triple);
        for p in &c {
            if let Ok(b) = std::fs::read(p) {
                return Ok(b);
            }
        }
        Err(format!(
            "no dev build at {}",
            c.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        ))
    }
}

/// Next to the executable first (shipped or copied binaries), then (debug builds) the
/// workspace's own build output, then the release download.
pub fn production_source(exe_dir: Option<PathBuf>, cache_dir: PathBuf) -> ChainSource {
    let mut v: Vec<Box<dyn BinarySource>> = Vec::new();
    if let Some(d) = &exe_dir {
        v.push(Box::new(DirSource(d.clone())));
        if cfg!(debug_assertions) {
            if let Some(target) = d.parent() {
                v.push(Box::new(CargoArtifactSource(target.to_path_buf())));
            }
        }
    }
    v.push(Box::new(HttpReleaseSource::new(cache_dir)));
    ChainSource(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn release_asset_urls() {
        let base = release_base("MertPROJ/xshell");
        assert_eq!(
            asset_url(&base, "1.5.0", "x86_64-unknown-linux-musl"),
            "https://github.com/MertPROJ/xshell/releases/download/v1.5.0/xshelld-x86_64-unknown-linux-musl"
        );
        assert_eq!(
            sha256_url(&base, "1.5.0", "x86_64-unknown-linux-musl"),
            "https://github.com/MertPROJ/xshell/releases/download/v1.5.0/xshelld-x86_64-unknown-linux-musl.sha256"
        );
    }

    #[test]
    fn sha256_sidecar_parse_verify() {
        let digest = sha256_hex(b"abc");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        verify_sha256(
            b"abc",
            &format!("{digest}  xshelld-x86_64-unknown-linux-musl\n"),
        )
        .unwrap();
        verify_sha256(b"abc", &digest.to_uppercase()).unwrap();
        assert!(verify_sha256(b"abd", &digest)
            .unwrap_err()
            .contains("mismatch"));
        assert!(verify_sha256(b"abc", "nothex").is_err());
    }

    #[test]
    fn cargo_artifact_source_layout() {
        let t = tempfile::tempdir().unwrap();
        let triple = "aarch64-unknown-linux-musl";
        let cross = t.path().join(triple).join("release-daemon");
        std::fs::create_dir_all(&cross).unwrap();
        std::fs::write(cross.join("xshelld"), b"CROSS").unwrap();
        let s = CargoArtifactSource(t.path().into());
        let c = CancelToken::new();
        assert_eq!(s.fetch(triple, "1", &c).unwrap(), b"CROSS");
        assert!(
            s.fetch("x86_64-apple-darwin", "1", &c).is_err()
                || own_triple() == Some("x86_64-apple-darwin")
        );
        // A plain build serves only this machine's own triple.
        let native = t.path().join("release-daemon");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(native.join("xshelld"), b"NATIVE").unwrap();
        if let Some(own) = own_triple().filter(|o| *o != triple) {
            assert_eq!(s.fetch(own, "1", &c).unwrap(), b"NATIVE");
        }
        let other = ["x86_64-unknown-linux-musl", "x86_64-apple-darwin"]
            .into_iter()
            .find(|t| Some(*t) != own_triple())
            .unwrap();
        assert!(s.fetch(other, "1", &c).is_err());
    }

    #[test]
    fn cache_hit_skips_download() {
        let t = tempfile::tempdir().unwrap();
        let mut s = HttpReleaseSource::new(t.path().into());
        s.base = "http://127.0.0.1:9".into(); // nothing listens on discard
        let p = s.cached("1.5.0", "x86_64-unknown-linux-musl");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"CACHED").unwrap();
        assert_eq!(
            s.fetch("x86_64-unknown-linux-musl", "1.5.0", &CancelToken::new())
                .unwrap(),
            b"CACHED"
        );
    }

    /// A server that accepts and never answers; the receiver hears of each connection.
    fn stalled_server() -> (std::sync::mpsc::Receiver<()>, String) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in l.incoming() {
                held.push(s);
                if tx.send(()).is_err() {
                    return;
                }
            }
        });
        (rx, base)
    }

    #[test]
    fn stalled_download_is_cancellable() {
        let (accepted, base) = stalled_server();
        let t = tempfile::tempdir().unwrap();
        let mut s = HttpReleaseSource::new(t.path().into());
        s.base = base;
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            accepted.recv_timeout(Duration::from_secs(5)).unwrap();
            c2.cancel();
        });
        let start = Instant::now();
        let e = s
            .fetch("x86_64-unknown-linux-musl", "1.5.0", &cancel)
            .unwrap_err();
        assert_eq!(e, "cancelled");
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_during_stalled_download() {
        use xshell_core::protocol::msg::TerminalInfo;
        use xshell_hostlink::transport::{CommandSpec, LocalShellTransport, Transport};
        use xshell_hostlink::{
            HostConfig, HostStatus, Manager, ManagerConfig, Observer, Phase, TransportFactory,
        };

        struct NotInstalled;
        impl Transport for NotInstalled {
            fn command(&self, _: &str) -> CommandSpec {
                LocalShellTransport::default().command("echo 'Linux x86_64'; echo @@XSHELL@@")
            }
            fn describe(&self) -> String {
                "fake".into()
            }
        }
        impl TransportFactory for NotInstalled {
            fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
                Box::new(NotInstalled)
            }
        }
        #[derive(Default)]
        struct Probing(std::sync::Mutex<bool>);
        impl Observer for Probing {
            fn status(&self, s: &HostStatus) {
                if s.phase == Some(Phase::Probing) {
                    *self.0.lock().unwrap() = true;
                }
            }
            fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
        }

        let (accepted, base) = stalled_server();
        let t = tempfile::tempdir().unwrap();
        let mut src = HttpReleaseSource::new(t.path().into());
        src.base = base;
        let obs = Arc::new(Probing::default());
        let m = Manager::new(ManagerConfig::new(
            "1.5.0",
            Arc::new(NotInstalled),
            Arc::new(src),
            obs.clone(),
        ));
        m.configure(vec![HostConfig {
            id: "h_aaaaaaaa".into(),
            name: "a".into(),
            ssh_target: "a".into(),
            color: None,
            daemon_command: None,
        }])
        .unwrap();
        // The probe finished and the download is stuck on the silent server.
        accepted.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(*obs.0.lock().unwrap());
        let start = Instant::now();
        m.shutdown();
        assert!(
            start.elapsed() <= Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }
}

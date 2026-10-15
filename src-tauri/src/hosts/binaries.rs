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
use xshell_hostlink::{BinarySource, CancelToken, ChainSource, DirSource, Fetched, Origin, Wanted};
use xshell_protocol::build::{install_dir_name, scan_stamp};

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
/// binaries under `cache_dir/xshelld/<source>/<version>+<build>/` (the release source's
/// digest, then the install directory name). The URL names the plain version; the build the
/// binary carries is the release's (its stamp), checked by the installer.
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

    /// Keyed by the release source, the version, the expected build and the target (R5).
    fn cached(&self, want: &Wanted) -> PathBuf {
        self.cache_dir
            .join("xshelld")
            .join(&sha256_hex(self.base.as_bytes())[..12])
            .join(install_dir_name(&want.version, want.build.as_ref()))
            .join(format!("xshelld-{}", want.triple))
    }

    /// Whether `bytes` carry the build `want` expects (anything, when it expects none).
    fn stamp_matches(bytes: &[u8], want: &Wanted) -> bool {
        match &want.build {
            None => true,
            Some(b) => scan_stamp(bytes).ok().flatten().is_some_and(|f| f.same(b)),
        }
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

    /// The cached binary, only if it matches the digest stored next to it and carries the
    /// expected build. Anything else (missing digest, mismatch) is deleted, so the caller
    /// downloads again.
    fn read_cached(&self, path: &Path, want: &Wanted) -> Option<Vec<u8>> {
        let digest = digest_path(path);
        let bytes = std::fs::read(path).ok();
        let sidecar = std::fs::read_to_string(&digest).ok();
        match (bytes, sidecar) {
            (Some(b), Some(d))
                if verify_sha256(&b, &d).is_ok() && Self::stamp_matches(&b, want) =>
            {
                Some(b)
            }
            (None, None) => None,
            _ => {
                let _ = std::fs::remove_file(path);
                let _ = std::fs::remove_file(&digest);
                None
            }
        }
    }

    /// Publish a verified download: each file goes through its own exclusive temp file and
    /// an atomic rename, so concurrent fetches (other triples, other windows) never mix.
    fn store(&self, path: &Path, bytes: &[u8]) {
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let digest = format!("{}\n", sha256_hex(bytes));
        if publish(&digest_path(path), digest.as_bytes()).is_ok() {
            let _ = publish(path, bytes);
        }
    }
}

impl BinarySource for HttpReleaseSource {
    fn fetch(&self, want: &Wanted, cancel: &CancelToken) -> Result<Fetched, String> {
        let origin = Origin::Release(asset_url(&self.base, &want.version, &want.triple));
        let path = self.cached(want);
        if let Some(bytes) = self.read_cached(&path, want) {
            return Ok(Fetched { bytes, origin });
        }
        let stop = Arc::new(Notify::new());
        let s = stop.clone();
        let _hook = cancel.on_cancel(move || s.notify_one());
        // The supervisor thread is not a runtime worker, so blocking here is allowed.
        let bytes = tauri::async_runtime::block_on(cancellable(
            self.download(&want.version, &want.triple),
            stop,
        ))
        .unwrap_or_else(|| Err("cancelled".into()))?;
        // Another build than expected is never cached; the installer refuses it.
        if Self::stamp_matches(&bytes, want) {
            self.store(&path, &bytes);
        }
        Ok(Fetched { bytes, origin })
    }
}

fn digest_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".sha256");
    PathBuf::from(s)
}

/// Write `bytes` to a fresh temp file next to `path` (unique per process, call and name;
/// created exclusively), then rename it over `path`.
fn publish(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let res = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()))
        .and_then(|_| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
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
    /// Each path with the `--target` it was built with (`None`: a plain build).
    fn candidates(&self, triple: &str) -> Vec<(PathBuf, Option<String>)> {
        let mut v = vec![(
            self.0.join(triple).join("release-daemon").join("xshelld"),
            Some(triple.to_string()),
        )];
        if own_triple() == Some(triple) {
            v.push((self.0.join("release-daemon").join("xshelld"), None));
        }
        v
    }
}

impl BinarySource for CargoArtifactSource {
    fn fetch(&self, want: &Wanted, _c: &CancelToken) -> Result<Fetched, String> {
        let c = self.candidates(&want.triple);
        for (p, target) in &c {
            if let Ok(bytes) = std::fs::read(p) {
                return Ok(Fetched {
                    bytes,
                    origin: Origin::DevBuild {
                        path: p.clone(),
                        target: target.clone(),
                    },
                });
            }
        }
        Err(format!(
            "no dev build at {}",
            c.iter()
                .map(|(p, _)| p.display().to_string())
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
    use xshell_protocol::build::BuildId;

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
        let got = s.fetch(&want(triple, "1"), &c).unwrap();
        assert_eq!(got.bytes, b"CROSS");
        assert_eq!(
            got.origin,
            Origin::DevBuild {
                path: cross.join("xshelld"),
                target: Some(triple.into())
            }
        );
        assert!(
            s.fetch(&want("x86_64-apple-darwin", "1"), &c).is_err()
                || own_triple() == Some("x86_64-apple-darwin")
        );
        // A plain build serves only this machine's own triple.
        let native = t.path().join("release-daemon");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(native.join("xshelld"), b"NATIVE").unwrap();
        if let Some(own) = own_triple().filter(|o| *o != triple) {
            let got = s.fetch(&want(own, "1"), &c).unwrap();
            assert_eq!(got.bytes, b"NATIVE");
            assert_eq!(
                got.origin,
                Origin::DevBuild {
                    path: native.join("xshelld"),
                    target: None
                }
            );
        }
        let other = ["x86_64-unknown-linux-musl", "x86_64-apple-darwin"]
            .into_iter()
            .find(|t| Some(*t) != own_triple())
            .unwrap();
        assert!(s.fetch(&want(other, "1"), &c).is_err());
    }

    #[test]
    fn cache_is_verified_on_read() {
        let t = tempfile::tempdir().unwrap();
        let mut s = HttpReleaseSource::new(t.path().into());
        s.base = "http://127.0.0.1:9".into(); // nothing listens on discard
        let triple = "x86_64-unknown-linux-musl";
        let c = CancelToken::new();
        let w = want(triple, "1.5.0");
        let p = s.cached(&w);
        // A verified download is served from the cache.
        s.store(&p, b"CACHED");
        let got = s.fetch(&w, &c).unwrap();
        assert_eq!(got.bytes, b"CACHED");
        assert_eq!(
            got.origin,
            Origin::Release(asset_url(&s.base, "1.5.0", triple))
        );
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // Tampered bytes, or bytes without a digest, are deleted and fetched again.
        std::fs::write(&p, b"EVIL").unwrap();
        assert!(s.fetch(&w, &c).is_err());
        assert!(!p.exists() && !digest_path(&p).exists());
        std::fs::write(&p, b"NO DIGEST").unwrap();
        assert!(s.fetch(&w, &c).is_err());
        assert!(!p.exists());
    }

    fn want(triple: &str, version: &str) -> Wanted {
        Wanted {
            triple: triple.into(),
            version: version.into(),
            build: None,
        }
    }

    fn build(commit: &str) -> BuildId {
        BuildId {
            commit: commit.into(),
            dirty: None,
            time: 1,
        }
    }

    /// The cache is keyed by (release source, version, build, triple), and a cached binary
    /// must carry the expected build.
    #[test]
    fn cache_key_and_stamp() {
        let t = tempfile::tempdir().unwrap();
        let mut s = HttpReleaseSource::new(t.path().into());
        s.base = "http://127.0.0.1:9".into();
        let ours = build("85ee920a1b2c");
        let w = Wanted {
            build: Some(ours.clone()),
            ..want("x86_64-unknown-linux-musl", "1.5.0")
        };
        let p = s.cached(&w);
        let src = sha256_hex(b"http://127.0.0.1:9")[..12].to_string();
        assert_eq!(
            p,
            t.path()
                .join("xshelld")
                .join(&src)
                .join("1.5.0+85ee920a1b2c")
                .join("xshelld-x86_64-unknown-linux-musl")
        );
        // Every part of the key changes the path.
        let other = |w2: Wanted| assert_ne!(s.cached(&w2), p);
        other(Wanted {
            build: None,
            ..w.clone()
        });
        other(Wanted {
            build: Some(build("000000000000")),
            ..w.clone()
        });
        other(Wanted {
            version: "1.5.1".into(),
            ..w.clone()
        });
        other(Wanted {
            triple: "aarch64-unknown-linux-musl".into(),
            ..w.clone()
        });
        let mut s2 = HttpReleaseSource::new(t.path().into());
        s2.base = "http://127.0.0.1:10".into();
        assert_ne!(s2.cached(&w), p);
        // A verified file of another build is not served (and is dropped).
        let stamped = |b: &BuildId| {
            format!("x @@XSHELL-BUILD-V1:{}:{}@@ y", b.display(), b.time).into_bytes()
        };
        s.store(&p, &stamped(&build("000000000000")));
        assert!(s.read_cached(&p, &w).is_none());
        assert!(!p.exists());
        s.store(&p, &stamped(&ours));
        assert_eq!(
            s.fetch(&w, &CancelToken::new()).unwrap().bytes,
            stamped(&ours)
        );
    }

    /// A stale workspace build is refused with the rebuild command, never replaced by a
    /// download (the chain does not fall through on the build).
    #[cfg(unix)]
    #[test]
    fn stale_dev_build_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let native = t.path().join("release-daemon");
        std::fs::create_dir_all(&native).unwrap();
        let Some(own) = own_triple() else { return };
        std::fs::write(
            native.join("xshelld"),
            b"@@XSHELL-BUILD-V1:000000000000:1@@",
        )
        .unwrap();
        let w = Wanted {
            build: Some(build("85ee920a1b2c")),
            ..want(own, "1.5.0")
        };
        let f = CargoArtifactSource(t.path().into())
            .fetch(&w, &CancelToken::new())
            .unwrap();
        let e = xshell_hostlink::install::check_fetched(&f, &w).unwrap_err();
        assert_eq!(
            e,
            format!(
                "the bundled xshelld at {} reports 000000000000; this Desktop expects 85ee920a1b2c; rebuild it: cargo build -p xshelld --profile release-daemon",
                native.join("xshelld").display()
            )
        );
        let r = Fetched {
            bytes: f.bytes,
            origin: Origin::Release("u".into()),
        };
        assert_eq!(
            xshell_hostlink::install::check_fetched(&r, &w).unwrap_err(),
            "the v1.5.0 release’s xshelld reports 000000000000; this Desktop expects 85ee920a1b2c"
        );
    }

    #[test]
    fn concurrent_stores_never_mix() {
        let t = tempfile::tempdir().unwrap();
        let s = Arc::new(HttpReleaseSource::new(t.path().into()));
        let joins: Vec<_> = (0..8u8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || {
                    let triple = if i % 2 == 0 { "a-triple" } else { "b-triple" };
                    let bytes = vec![i % 2; 256 * 1024];
                    let p = s.cached(&want(triple, "1.5.0"));
                    for _ in 0..10 {
                        s.store(&p, &bytes);
                    }
                })
            })
            .collect();
        for j in joins {
            j.join().unwrap();
        }
        for (triple, byte) in [("a-triple", 0u8), ("b-triple", 1u8)] {
            let w = want(triple, "1.5.0");
            let b = s.read_cached(&s.cached(&w), &w).expect("verified");
            assert!(b.len() == 256 * 1024 && b.iter().all(|x| *x == byte));
        }
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
            .fetch(&want("x86_64-unknown-linux-musl", "1.5.0"), &cancel)
            .unwrap_err();
        assert_eq!(e, "cancelled");
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_during_stalled_download() {
        use xshell_hostlink::transport::{CommandSpec, LocalShellTransport, Transport};
        use xshell_hostlink::{
            HostConfig, HostStatus, Manager, ManagerConfig, Observer, Phase, TransportFactory,
        };
        use xshell_protocol::msg::TerminalInfo;

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
            launch_prefixes: Default::default(),
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

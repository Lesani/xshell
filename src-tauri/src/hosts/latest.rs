//! The latest xshell release, as the release server says (D3 of the update-from-get design):
//! read at start and every 3 hours on a thread of its own, never holding up startup. Ring
//! Hosts that update themselves (`daemon.update-latest`) are offered that release when it is
//! newer than theirs. An unknown answer (offline, an error) keeps the last value this run
//! knew; with none known, nothing is offered to them.
//!
//! The base is `https://get.xshell.sh`, overridden by `XSHELL_GET_URL` (tests and end-to-end
//! runs point it at a local stub). The default base is followed over https only; an override
//! never goes through a proxy.
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;
use xshell_hostlink::Manager;

/// The release server.
pub const GET_BASE_DEFAULT: &str = "https://get.xshell.sh";
/// How often the latest release is asked for again.
const EVERY: Duration = Duration::from_secs(3 * 60 * 60);
/// The longest `/latest` answer: one version line.
const LATEST_MAX: usize = 64;

/// The release server's base and whether it is followed over https only (the default base).
/// An empty `XSHELL_GET_URL` counts as unset.
pub fn get_base(env: &dyn Fn(&str) -> Option<OsString>) -> (String, bool) {
    match env("XSHELL_GET_URL")
        .and_then(|v| v.into_string().ok())
        .filter(|v| !v.is_empty())
    {
        Some(v) => (v, false),
        None => (GET_BASE_DEFAULT.to_string(), true),
    }
}

/// A client for the release server: connect 10 s, whole request 30 s, at most 5 redirects.
fn client(https_only: bool) -> Result<reqwest::Client, String> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // As the binary download does; fails only if another provider won the race.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .https_only(https_only)
        .user_agent(concat!("xshell/", env!("CARGO_PKG_VERSION")));
    if !https_only {
        b = b.no_proxy();
    }
    b.build().map_err(|e| e.to_string())
}

/// `<base>/latest`: a version line of at most 64 bytes.
async fn fetch_latest(client: &reqwest::Client, base: &str) -> Result<semver::Version, String> {
    let url = format!("{}/latest", base.trim_end_matches('/'));
    let mut res = client
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("{url}: {e}"))?;
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| format!("{url}: {e}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > LATEST_MAX {
            return Err(format!("{url}: the answer is too long"));
        }
    }
    let text = String::from_utf8_lossy(&body);
    semver::Version::parse(text.trim()).map_err(|e| format!("{url}: not a version: {e}"))
}

/// Ask once; only a version updates the Manager, so a failure keeps the last known value.
async fn refresh(
    manager: &Manager,
    client: &reqwest::Client,
    base: &str,
) -> Result<semver::Version, String> {
    let v = fetch_latest(client, base).await?;
    manager.set_latest_release(v.clone());
    Ok(v)
}

/// Start asking: now, then every 3 hours, on the thread `latest-release`.
pub fn spawn(manager: Arc<Manager>, (base, https_only): (String, bool)) {
    let spawned = std::thread::Builder::new()
        .name("latest-release".into())
        .spawn(move || {
            let client = match client(https_only) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("xshell: cannot ask for the latest release: {e}");
                    return;
                }
            };
            loop {
                if let Err(e) = tauri::async_runtime::block_on(refresh(&manager, &client, &base)) {
                    eprintln!("xshell: latest release unknown: {e}");
                }
                std::thread::sleep(EVERY);
            }
        });
    if let Err(e) = spawned {
        eprintln!("xshell: cannot ask for the latest release: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// Path → (status, extra header lines, body).
    type Routes = Arc<Mutex<HashMap<String, (u16, String, Vec<u8>)>>>;

    /// A loopback stand-in for the release server: path → (status, extra header, body);
    /// it logs every path asked for.
    struct Stub {
        base: String,
        hits: Arc<Mutex<Vec<String>>>,
        routes: Routes,
    }

    impl Stub {
        fn start() -> Stub {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", l.local_addr().unwrap());
            let hits: Arc<Mutex<Vec<String>>> = Arc::default();
            let routes: Routes = Arc::default();
            let (h, r) = (hits.clone(), routes.clone());
            std::thread::spawn(move || {
                for c in l.incoming() {
                    let Ok(c) = c else { return };
                    let mut rd = BufReader::new(c.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = rd.read_line(&mut line);
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    loop {
                        let mut hl = String::new();
                        if rd.read_line(&mut hl).unwrap_or(0) == 0 || hl.trim().is_empty() {
                            break;
                        }
                    }
                    h.lock().unwrap().push(path.clone());
                    let (code, header, body) = r.lock().unwrap().get(&path).cloned().unwrap_or((
                        404,
                        String::new(),
                        Vec::new(),
                    ));
                    let mut w = c;
                    let _ = w.write_all(
                        format!(
                            "HTTP/1.1 {code} X\r\n{header}content-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    let _ = w.write_all(&body);
                }
            });
            Stub { base, hits, routes }
        }

        fn body(&self, path: &str, body: &str) {
            self.routes
                .lock()
                .unwrap()
                .insert(path.into(), (200, String::new(), body.as_bytes().to_vec()));
        }

        fn status(&self, path: &str, code: u16) {
            self.routes
                .lock()
                .unwrap()
                .insert(path.into(), (code, String::new(), Vec::new()));
        }

        fn redirect(&self, path: &str, to: &str) {
            self.routes.lock().unwrap().insert(
                path.into(),
                (302, format!("location: {to}\r\n"), Vec::new()),
            );
        }

        fn hits(&self) -> Vec<String> {
            self.hits.lock().unwrap().clone()
        }
    }

    fn fetch(base: &str) -> Result<semver::Version, String> {
        let c = client(false).unwrap();
        tauri::async_runtime::block_on(fetch_latest(&c, base))
    }

    #[test]
    fn fetch_latest_parses_version_line() {
        let s = Stub::start();
        s.body("/latest", "1.6.0-rjw.5\n");
        assert_eq!(fetch(&s.base).unwrap().to_string(), "1.6.0-rjw.5");
        // A trailing slash on the base is fine.
        assert_eq!(
            fetch(&format!("{}/", s.base)).unwrap().to_string(),
            "1.6.0-rjw.5"
        );
    }

    #[test]
    fn fetch_latest_refuses_garbage_oversize_and_errors() {
        let s = Stub::start();
        s.body("/latest", "<html>hello</html>");
        assert!(fetch(&s.base).is_err());
        s.body("/latest", &"1".repeat(65));
        assert!(fetch(&s.base).unwrap_err().contains("too long"));
        s.status("/latest", 500);
        assert!(fetch(&s.base).is_err());
        s.status("/latest", 404);
        assert!(fetch(&s.base).is_err());
        // Nothing listens.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        assert!(fetch(&closed).is_err());
    }

    #[test]
    fn fetch_latest_redirects_at_most_five() {
        let s = Stub::start();
        s.body("/v", "1.6.0-rjw.5\n");
        let chain = |n: usize| {
            for i in 0..n {
                let from = if i == 0 {
                    "/latest".to_string()
                } else {
                    format!("/r{i}")
                };
                let to = if i + 1 == n {
                    "/v".to_string()
                } else {
                    format!("/r{}", i + 1)
                };
                s.redirect(&from, &to);
            }
        };
        chain(5);
        assert_eq!(fetch(&s.base).unwrap().to_string(), "1.6.0-rjw.5");
        chain(6);
        assert!(fetch(&s.base).is_err());
    }

    /// The default base's rule: a plain http URL is refused before any request is made.
    #[test]
    fn https_only_client_refuses_http() {
        let s = Stub::start();
        s.body("/latest", "1.6.0-rjw.5\n");
        let c = client(true).unwrap();
        assert!(tauri::async_runtime::block_on(fetch_latest(&c, &s.base)).is_err());
        assert!(s.hits().is_empty(), "{:?}", s.hits());
    }

    #[test]
    fn get_base_override() {
        let none = |_: &str| None;
        assert_eq!(get_base(&none), (GET_BASE_DEFAULT.to_string(), true));
        let empty = |_: &str| Some(OsString::new());
        assert_eq!(get_base(&empty), (GET_BASE_DEFAULT.to_string(), true));
        let set = |k: &str| (k == "XSHELL_GET_URL").then(|| OsString::from("http://127.0.0.1:9"));
        assert_eq!(get_base(&set), ("http://127.0.0.1:9".to_string(), false));
    }

    #[test]
    fn refresh_failure_keeps_last_value() {
        use xshell_hostlink::transport::{CommandSpec, LocalShellTransport, Transport};
        use xshell_hostlink::{HostConfig, HostStatus, ManagerConfig, Observer, TransportFactory};
        use xshell_protocol::msg::TerminalInfo;
        struct NoHost;
        impl Transport for NoHost {
            fn command(&self, _: &str) -> CommandSpec {
                LocalShellTransport::default().command("exit 1")
            }
            fn describe(&self) -> String {
                "none".into()
            }
        }
        impl TransportFactory for NoHost {
            fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
                Box::new(NoHost)
            }
        }
        struct Quiet;
        impl Observer for Quiet {
            fn status(&self, _: &HostStatus) {}
            fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
        }
        let t = tempfile::tempdir().unwrap();
        let m = Manager::new(ManagerConfig::new(
            "1.6.0-rjw.4",
            Arc::new(NoHost),
            Arc::new(super::super::binaries::production_source(
                None,
                t.path().into(),
            )),
            Arc::new(Quiet),
        ));
        let s = Stub::start();
        let c = client(false).unwrap();
        assert_eq!(m.latest_release(), None);
        s.body("/latest", "1.6.0-rjw.5\n");
        tauri::async_runtime::block_on(refresh(&m, &c, &s.base)).unwrap();
        assert_eq!(m.latest_release().unwrap().to_string(), "1.6.0-rjw.5");
        // Offline, an error or garbage: the last value stays.
        for bad in [Some(500), Some(404), None] {
            match bad {
                Some(code) => s.status("/latest", code),
                None => s.body("/latest", "garbage"),
            }
            assert!(tauri::async_runtime::block_on(refresh(&m, &c, &s.base)).is_err());
            assert_eq!(m.latest_release().unwrap().to_string(), "1.6.0-rjw.5");
        }
        s.body("/latest", "1.6.0-rjw.6\n");
        tauri::async_runtime::block_on(refresh(&m, &c, &s.base)).unwrap();
        assert_eq!(m.latest_release().unwrap().to_string(), "1.6.0-rjw.6");
        m.shutdown();
    }
}

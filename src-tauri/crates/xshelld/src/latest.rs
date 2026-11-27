//! `daemon.update-latest`: what the release server (`get.xshell.sh`, or `XSHELL_GET_URL`)
//! says is the latest release, and the downloaded build for this machine (ADR-0019,
//! amendment of 2026-11-27).
//!
//! Three requests, all under one deadline ([`WHOLE`]): `<base>/latest` (a version line),
//! then, when that version is newer than this Daemon's, `<base>/release/v<v>/xshelld-<triple>.sha256`
//! and the build itself. The default base is followed over https only; an override (tests,
//! end to end) may be plain http and never goes through a proxy. Redirects are bounded.
//! Every failure is a message that starts with one of the protocol's
//! `UPDATE_LATEST_*` prefixes, so a Desktop can tell them apart without reading the detail.
//! The checksum itself is checked by the staging worker that installs the bytes.
use std::time::{Duration, Instant};
use xshell_protocol::msg::{
    STAGE_MAX_BYTES, UPDATE_LATEST_BAD_BUILD, UPDATE_LATEST_NO_BUILD, UPDATE_LATEST_UNREACHABLE,
};

/// Connecting to the release server.
const CONNECT: Duration = Duration::from_secs(10);
/// All three requests together, bodies included.
const WHOLE: Duration = Duration::from_secs(120);
/// Redirects followed per request.
pub(crate) const MAX_REDIRECTS: u32 = 5;
/// The longest `/latest` body: one version line.
const LATEST_MAX: u64 = 64;
/// The longest `.sha256` body: a digest and a file name.
const SHA_MAX: u64 = 1024;

pub(crate) enum Latest {
    /// The latest release is not newer than this Daemon.
    Current,
    /// The newer release's build for this machine, not yet verified.
    Download {
        version: semver::Version,
        sha256: String,
        bytes: Vec<u8>,
    },
}

/// The agent for `base`: https only for the default base; an override never uses a proxy.
pub(crate) fn agent(https_only: bool, overridden: bool) -> ureq::Agent {
    let mut b = ureq::Agent::config_builder()
        .https_only(https_only)
        .max_redirects(MAX_REDIRECTS)
        .timeout_connect(Some(CONNECT))
        .timeout_global(Some(WHOLE));
    if overridden {
        b = b.proxy(None);
    }
    b.build().into()
}

/// Ask the release server at `base` for the latest release and, when it is newer than
/// `own`, download its build for `triple`.
pub(crate) fn fetch(base: &str, own: &semver::Version, triple: &str) -> Result<Latest, String> {
    let default = base == crate::GET_BASE_DEFAULT;
    let agent = agent(default, !default);
    let base = base.trim_end_matches('/');
    let until = Instant::now() + WHOLE;
    let unreachable = |e: String| format!("{UPDATE_LATEST_UNREACHABLE}: {e}");

    let text = get(&agent, &format!("{base}/latest"), LATEST_MAX, until).map_err(|e| match e {
        Fail::Http(e) => unreachable(e),
        Fail::Missing => unreachable("no /latest".into()),
        Fail::TooLarge => unreachable("the version answer is too long".into()),
    })?;
    let line = String::from_utf8_lossy(&text);
    let version = semver::Version::parse(line.trim())
        .map_err(|e| unreachable(format!("not a version {:?}: {e}", line.trim())))?;
    if !version.cmp_precedence(own).is_gt() {
        return Ok(Latest::Current);
    }

    let asset = format!("{base}/release/v{version}/xshelld-{triple}");
    let no_build = || format!("{UPDATE_LATEST_NO_BUILD}: {triple} for {version}");
    let bad = |e: &str| format!("{UPDATE_LATEST_BAD_BUILD}: {e}");
    let sums = get(&agent, &format!("{asset}.sha256"), SHA_MAX, until).map_err(|e| match e {
        Fail::Http(e) => unreachable(e),
        Fail::Missing => no_build(),
        Fail::TooLarge => bad("the checksum file is too long"),
    })?;
    // As `scripts/install.sh`: the first 64 characters are the digest.
    let sha256: String = String::from_utf8_lossy(&sums).chars().take(64).collect();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("the checksum file holds no SHA-256"));
    }
    let bytes = get(&agent, &asset, STAGE_MAX_BYTES, until).map_err(|e| match e {
        Fail::Http(e) => unreachable(e),
        Fail::Missing => no_build(),
        Fail::TooLarge => bad("the build is larger than 64 MiB"),
    })?;
    Ok(Latest::Download {
        version,
        sha256: sha256.to_ascii_lowercase(),
        bytes,
    })
}

enum Fail {
    /// Not reached, an error status other than 404, too many redirects, a timeout.
    Http(String),
    /// 404.
    Missing,
    /// The body is longer than allowed.
    TooLarge,
}

fn get(agent: &ureq::Agent, url: &str, limit: u64, until: Instant) -> Result<Vec<u8>, Fail> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Fail::Http("timed out".into()));
    }
    let map = |e: ureq::Error| match e {
        ureq::Error::StatusCode(404) => Fail::Missing,
        ureq::Error::BodyExceedsLimit(_) => Fail::TooLarge,
        e => Fail::Http(format!("{url}: {e}")),
    };
    let mut res = agent
        .get(url)
        .config()
        .timeout_global(Some(left))
        .build()
        .call()
        .map_err(map)?;
    res.body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The default rule (https only) refuses a plain http URL before connecting: proven
    /// against a local listener that counts connections, never against production.
    #[test]
    fn https_only_refuses_http_target() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        l.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let t = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match l.accept() {
                    Ok((mut c, _)) => {
                        h.fetch_add(1, Ordering::SeqCst);
                        let mut buf = [0u8; 1024];
                        let _ = c.read(&mut buf);
                        let _ = c.write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\n9.9.9\n",
                        );
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        let url = format!("http://127.0.0.1:{port}/latest");
        let until = Instant::now() + Duration::from_secs(10);
        let r = get(&agent(true, true), &url, LATEST_MAX, until);
        assert!(
            matches!(&r, Err(Fail::Http(e)) if e.contains("https only")),
            "{:?}",
            r.as_ref().map_err(|e| match e {
                Fail::Http(e) => e.clone(),
                Fail::Missing => "missing".into(),
                Fail::TooLarge => "too large".into(),
            })
        );
        // The same listener does answer a client allowed to use http: the refusal above was
        // the rule, not a dead listener.
        let ok = get(&agent(false, true), &url, LATEST_MAX, until).ok();
        assert_eq!(ok.as_deref(), Some(&b"9.9.9\n"[..]));
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "only the http-allowed request"
        );
    }
}

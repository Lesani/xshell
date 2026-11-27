//! Native staging never selects or starts a Daemon. Base64 is data on stdin, not a
//! PowerShell executable pipeline or a command-line-sized payload.
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::path::Path;

const MAX_BINARY: u64 = xshell_protocol::msg::STAGE_MAX_BYTES;

/// Checks a build received in pieces against its announced length and SHA-256: shared by
/// `stage-build` (stdin) and staging over a link (`daemon.stage-*`).
pub struct Verifier {
    hasher: Sha256,
    received: u64,
    length: u64,
    sha256: String,
}

impl Verifier {
    /// Refuses a length of 0 or above the limit, and a SHA-256 that is not 64 hex digits.
    pub fn new(sha256: &str, length: u64) -> io::Result<Verifier> {
        if length == 0
            || length > MAX_BINARY
            || sha256.len() != 64
            || !sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(io::Error::other("invalid staging length or SHA-256"));
        }
        Ok(Verifier {
            hasher: Sha256::new(),
            received: 0,
            length,
            sha256: sha256.to_ascii_lowercase(),
        })
    }

    /// Refused (nothing counted) when it would run past the announced length (C7).
    pub fn check(&self, more: u64) -> io::Result<()> {
        if self.received.saturating_add(more) > self.length {
            return Err(io::Error::other("more bytes than announced"));
        }
        Ok(())
    }

    pub fn update(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.check(bytes.len() as u64)?;
        self.hasher.update(bytes);
        self.received += bytes.len() as u64;
        Ok(())
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// Every byte arrived and the digest matches.
    pub fn finish(self) -> io::Result<()> {
        let digest = format!("{:x}", self.hasher.finalize());
        if self.received != self.length || digest != self.sha256 {
            return Err(io::Error::other("staging length or SHA-256 mismatch"));
        }
        Ok(())
    }
}

pub fn decode(input: &mut dyn Read, hash: &str, length: u64) -> io::Result<Vec<u8>> {
    let mut verifier = Verifier::new(hash, length)?;
    // At most one CRLF separator per base64 character; whitespace other than CR/LF is not encoding.
    let encoded_limit = length.div_ceil(3) * 4 * 3;
    let mut encoded = Vec::new();
    input.take(encoded_limit + 1).read_to_end(&mut encoded)?;
    if encoded.len() as u64 > encoded_limit {
        return Err(io::Error::other("staging input too large"));
    }
    encoded.retain(|b| !matches!(b, b'\r' | b'\n'));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(io::Error::other)?;
    verifier.update(&bytes)?;
    verifier.finish()?;
    Ok(bytes)
}

/// The version and build `bin --version` reports, run with an empty environment except
/// `PATH` and `HOME` and ended after 10 s. A just-written file may still be open for writing
/// in a child forked meanwhile (ETXTBSY): retried briefly.
#[cfg(unix)]
pub fn native_identity(
    bin: &Path,
) -> io::Result<(String, Option<xshell_protocol::build::BuildId>)> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut cmd = Command::new(bin);
    cmd.arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir("/");
    for k in ["PATH", "HOME"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    let mut tries = 0;
    let mut child = loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 100 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            r => break r?,
        }
    };
    let mut out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.by_ref().take(64 * 1024).read_to_end(&mut buf);
        buf
    });
    let until = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!(
                "{} --version did not answer in time",
                bin.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let buf = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(io::Error::other(format!(
            "{} --version failed ({status})",
            bin.display()
        )));
    }
    let text = String::from_utf8_lossy(&buf);
    let v = text
        .lines()
        .find_map(xshell_hostlink::install::parse_version_json)
        .ok_or_else(|| io::Error::other("the build does not report its version"))?;
    Ok((v.version, v.build))
}

pub fn stage(
    home: &Path,
    dir: &str,
    hash: &str,
    length: u64,
    input: &mut dyn Read,
) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = (home, dir, hash, length, input);
        Err(io::Error::other("native staging is Windows-only"))
    }
    #[cfg(windows)]
    {
        use std::io::Write;
        use xshell_core::{private_fs, server};
        use xshell_protocol::build::{install_dir_name, scan_stamp};
        if !server::valid_dir_name(dir) {
            return Err(io::Error::other("invalid install directory"));
        }
        let bytes = decode(input, hash, length)?;
        let stamp = scan_stamp(&bytes)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("unknown Daemon build stamp"))?;
        let root = server::windows_server_root(home)?;
        let target = root.join(dir);
        private_fs::ensure_dir(&target)?;
        let file = target.join("xshelld.exe");
        if let Some(existing) = private_fs::open_read(&file)? {
            let mut current = Vec::new();
            existing.take(MAX_BINARY + 1).read_to_end(&mut current)?;
            if current != bytes {
                return Err(io::Error::other(
                    "immutable Daemon image already exists with different bytes",
                ));
            }
            server::windows_install(home, dir)?;
            return Ok(());
        }
        let tmp = target.join(format!(".stage.{}.exe", uuid::Uuid::new_v4().simple()));
        let result = (|| {
            let mut out = private_fs::create_new(&tmp)?;
            out.write_all(&bytes)?;
            out.sync_all()?;
            drop(out);
            let id = server::native_identity(&tmp)?;
            if !id.build.same(&stamp) || install_dir_name(&id.version, Some(&id.build)) != dir {
                return Err(io::Error::other(
                    "staged Daemon does not match target directory",
                ));
            }
            // Hard-link publication is exclusive: it never replaces a running image or a competing install.
            std::fs::hard_link(&tmp, &file)
        })();
        let _ = std::fs::remove_file(tmp);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verifier_refuses_overrun_and_mismatch() {
        let bytes = b"0123456789";
        let hash = format!("{:x}", Sha256::digest(bytes));
        let mut v = Verifier::new(&hash, 10).unwrap();
        v.update(&bytes[..4]).unwrap();
        assert!(v.check(7).is_err(), "overrun refused before it is counted");
        assert_eq!(v.received(), 4);
        v.update(&bytes[4..]).unwrap();
        v.finish().unwrap();
        let mut v = Verifier::new(&hash, 10).unwrap();
        v.update(b"012345678X").unwrap();
        assert!(v.finish().is_err());
        let mut v = Verifier::new(&hash, 10).unwrap();
        v.update(&bytes[..9]).unwrap();
        assert!(v.finish().is_err(), "short");
        assert!(Verifier::new(&hash, 0).is_err());
        assert!(Verifier::new(&hash, MAX_BINARY + 1).is_err());
        assert!(Verifier::new("zz", 1).is_err());
    }

    #[test]
    fn staging_integrity_length_encoding_and_bounds() {
        let bytes = b"native image";
        let hash = format!("{:x}", Sha256::digest(bytes));
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let separated = format!("\r\n{}\n", encoded);
        assert_eq!(
            decode(&mut separated.as_bytes(), &hash, bytes.len() as u64).unwrap(),
            bytes
        );
        for (text, hash, len) in [
            (encoded.as_str(), hash.as_str(), 1),
            (
                encoded.as_str(),
                "0000000000000000000000000000000000000000000000000000000000000000",
                bytes.len() as u64,
            ),
            ("not base64", hash.as_str(), bytes.len() as u64),
            (encoded.as_str(), hash.as_str(), MAX_BINARY + 1),
            (encoded.as_str(), "bad", bytes.len() as u64),
        ] {
            assert!(decode(&mut text.as_bytes(), hash, len).is_err());
        }
    }
}

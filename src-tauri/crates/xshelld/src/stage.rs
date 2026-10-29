//! Native staging never selects or starts a Daemon. Base64 is data on stdin, not a
//! PowerShell executable pipeline or a command-line-sized payload.
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::path::Path;

const MAX_BINARY: u64 = 64 * 1024 * 1024;

pub fn decode(input: &mut dyn Read, hash: &str, length: u64) -> io::Result<Vec<u8>> {
    if length == 0
        || length > MAX_BINARY
        || hash.len() != 64
        || !hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::other("invalid staging length or SHA-256"));
    }
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
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if bytes.len() as u64 != length || !digest.eq_ignore_ascii_case(hash) {
        return Err(io::Error::other("staging length or SHA-256 mismatch"));
    }
    Ok(bytes)
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

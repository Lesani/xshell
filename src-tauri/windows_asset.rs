//! Shared by build.rs and the embedded BinarySource: exact identity and integrity checks.
use sha2::{Digest, Sha256};
use xshell_protocol::build::{scan_stamp, BuildId};

pub fn verify(bytes: &[u8], sha256: &str, expected: &BuildId) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > 64 * 1024 * 1024 {
        return Err("Windows Daemon asset must be at most 64 MiB".into());
    }
    let got = format!("{:x}", Sha256::digest(bytes));
    if !got.eq_ignore_ascii_case(sha256) {
        return Err("Windows Daemon asset SHA-256 mismatch".into());
    }
    let offset = bytes
        .get(0x3c..0x40)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    if bytes.get(..2) != Some(b"MZ")
        || offset.and_then(|o| bytes.get(o..o.checked_add(6)?)) != Some(b"PE\0\0\x64\x86")
    {
        return Err("Windows Daemon asset must be an x86_64 PE image".into());
    }
    let build = scan_stamp(bytes)?.ok_or("Windows Daemon asset has no known build stamp")?;
    if expected.dirty.is_some() || build.dirty.is_some() || !build.same(expected) {
        return Err("Windows Daemon asset must match this Desktop’s clean source build".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_asset_validation_without_a_production_asset() {
        // Only a test fixture: never emitted as an embedded build input.
        let mut bytes = vec![0; 128];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..70].copy_from_slice(b"PE\0\0\x64\x86");
        bytes.extend_from_slice(b"@@XSHELL-BUILD-V1:85ee920a1b2c:7@@");
        let build = BuildId {
            commit: "85ee920a1b2c".into(),
            dirty: None,
            time: 7,
        };
        let hash = format!("{:x}", Sha256::digest(&bytes));
        verify(&bytes, &hash, &build).unwrap();
        assert!(verify(&bytes, "bad", &build).is_err());
        assert!(verify(
            &bytes,
            &hash,
            &BuildId {
                commit: "000000000000".into(),
                ..build.clone()
            }
        )
        .is_err());
        assert!(verify(
            &bytes,
            &hash,
            &BuildId {
                dirty: Some("000000000000".into()),
                ..build
            }
        )
        .is_err());
        bytes[68] = 0;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        assert!(verify(
            &bytes,
            &hash,
            &BuildId {
                commit: "85ee920a1b2c".into(),
                dirty: None,
                time: 7
            }
        )
        .is_err());
    }
}

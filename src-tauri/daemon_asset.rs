//! Shared by build.rs and the embedded own-target BinarySource: the checks an embedded
//! Linux/macOS/Windows `xshelld` must pass (ADR-0017: the Desktop carries its own Daemon).
use sha2::{Digest, Sha256};
use xshell_protocol::build::{scan_stamp, BuildId};

/// `bytes` is this Desktop's own-target Daemon: at most 64 MiB, the given SHA-256, an
/// executable of `target_os`/`target_arch`, and stamped with `expected` (a dirty build only
/// with the identical dirty id, so a local end-to-end build of a dirty tree works).
pub fn verify_own(
    bytes: &[u8],
    sha256: &str,
    expected: &BuildId,
    target_os: &str,
    target_arch: &str,
) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > 64 * 1024 * 1024 {
        return Err("embedded Daemon must be at most 64 MiB".into());
    }
    let got = format!("{:x}", Sha256::digest(bytes));
    if !got.eq_ignore_ascii_case(sha256.trim()) {
        return Err("embedded Daemon SHA-256 mismatch".into());
    }
    match target_os {
        "linux" => {
            let machine = match target_arch {
                "x86_64" => 62u16,
                "aarch64" => 183,
                other => return Err(format!("no embedded Daemon for Linux {other}")),
            };
            let elf = bytes.get(..4) == Some(b"\x7fELF")
                && bytes.get(4) == Some(&2) // 64-bit
                && bytes.get(5) == Some(&1) // little-endian
                && bytes
                    .get(18..20)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    == Some(machine);
            if !elf {
                return Err(format!(
                    "embedded Daemon must be a 64-bit {target_arch} ELF executable"
                ));
            }
        }
        "macos" => {
            let cpu: u32 = match target_arch {
                "x86_64" => 0x0100_0007,
                "aarch64" => 0x0100_000c,
                other => return Err(format!("no embedded Daemon for macOS {other}")),
            };
            if !macho_has_cpu(bytes, cpu) {
                return Err(format!(
                    "embedded Daemon must be a 64-bit Mach-O executable with a {target_arch} slice"
                ));
            }
        }
        "windows" => {
            if target_arch != "x86_64" {
                return Err(format!("no embedded Daemon for Windows {target_arch}"));
            }
            // The PE, size, hash and clean exact-build checks of the Windows asset.
            return super::windows_asset::verify(bytes, sha256.trim(), expected);
        }
        other => return Err(format!("no embedded own-target Daemon on {other}")),
    }
    let build = scan_stamp(bytes)?.ok_or("embedded Daemon has no known build stamp")?;
    if !build.same(expected) {
        return Err(format!(
            "embedded Daemon is build {}, this Desktop is {}",
            build.display(),
            expected.display()
        ));
    }
    Ok(())
}

/// Whether `bytes` is a thin 64-bit little-endian Mach-O of `cpu`, or a fat binary with a
/// `cpu` slice.
fn macho_has_cpu(bytes: &[u8], cpu: u32) -> bool {
    let be = |o: usize| {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    let le = |o: usize| {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    match bytes.get(..4) {
        Some(b"\xcf\xfa\xed\xfe") => le(4) == Some(cpu),
        Some(b"\xca\xfe\xba\xbe") => {
            let n = be(4).unwrap_or(0).min(32) as usize;
            (0..n).any(|i| be(8 + i * 20) == Some(cpu))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf(machine: u16, stamp: &str) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b.extend_from_slice(stamp.as_bytes());
        b
    }

    fn id(commit: &str, dirty: Option<&str>) -> BuildId {
        BuildId {
            commit: commit.into(),
            dirty: dirty.map(Into::into),
            time: 7,
        }
    }

    #[test]
    fn own_asset_validation() {
        let sha = |b: &[u8]| format!("{:x}", Sha256::digest(b));
        let clean = elf(62, "@@XSHELL-BUILD-V1:85ee920a1b2c:7@@");
        let want = id("85ee920a1b2c", None);
        verify_own(&clean, &sha(&clean), &want, "linux", "x86_64").unwrap();
        // Bad SHA.
        assert!(verify_own(&clean, "00", &want, "linux", "x86_64")
            .unwrap_err()
            .contains("SHA-256"));
        // Wrong machine.
        let arm = elf(183, "@@XSHELL-BUILD-V1:85ee920a1b2c:7@@");
        assert!(verify_own(&arm, &sha(&arm), &want, "linux", "x86_64")
            .unwrap_err()
            .contains("ELF"));
        verify_own(&arm, &sha(&arm), &want, "linux", "aarch64").unwrap();
        // Not an ELF at all, and not for macOS.
        assert!(verify_own(&clean, &sha(&clean), &want, "macos", "aarch64").is_err());
        assert!(verify_own(&clean, &sha(&clean), &want, "windows", "x86_64").is_err());
        // Another commit.
        assert!(verify_own(
            &clean,
            &sha(&clean),
            &id("000000000000", None),
            "linux",
            "x86_64"
        )
        .unwrap_err()
        .contains("this Desktop is"));
        // Dirty: only the identical dirty id.
        let dirty = elf(62, "@@XSHELL-BUILD-V1:85ee920a1b2c+d.aaaaaaaaaaaa:7@@");
        let want_dirty = id("85ee920a1b2c", Some("aaaaaaaaaaaa"));
        verify_own(&dirty, &sha(&dirty), &want_dirty, "linux", "x86_64").unwrap();
        assert!(verify_own(
            &dirty,
            &sha(&dirty),
            &id("85ee920a1b2c", Some("bbbbbbbbbbbb")),
            "linux",
            "x86_64"
        )
        .is_err());
        assert!(verify_own(&dirty, &sha(&dirty), &want, "linux", "x86_64").is_err());
        // Mach-O: the CPU type must match (thin), or a slice must (fat).
        let mut macho = clean.clone();
        macho[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
        macho[4..8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
        verify_own(&macho, &sha(&macho), &want, "macos", "aarch64").unwrap();
        assert!(verify_own(&macho, &sha(&macho), &want, "macos", "x86_64").is_err());
        // A well-formed fat header: two slices, both the stamped bytes after the header.
        let fat_with = |second_cpu: u32| {
            let body = b"@@XSHELL-BUILD-V1:85ee920a1b2c:7@@".to_vec();
            let mut fat = vec![0u8; 64];
            fat[..4].copy_from_slice(b"\xca\xfe\xba\xbe");
            fat[4..8].copy_from_slice(&2u32.to_be_bytes());
            for (i, cpu) in [0x0100_0007u32, second_cpu].into_iter().enumerate() {
                let e = 8 + i * 20;
                fat[e..e + 4].copy_from_slice(&cpu.to_be_bytes());
                fat[e + 8..e + 12].copy_from_slice(&64u32.to_be_bytes());
                fat[e + 12..e + 16].copy_from_slice(&(body.len() as u32).to_be_bytes());
            }
            fat.extend_from_slice(&body);
            fat
        };
        let fat = fat_with(0x0100_000c);
        verify_own(&fat, &sha(&fat), &want, "macos", "x86_64").unwrap();
        verify_own(&fat, &sha(&fat), &want, "macos", "aarch64").unwrap();
        let fat = fat_with(0x0000_0012);
        assert!(verify_own(&fat, &sha(&fat), &want, "macos", "aarch64").is_err());
    }
}

mod daemon_asset;
mod windows_asset;

fn main() {
    // The build identity (Lesani/xshell#49): `XSHELL_BUILDINFO_STAMP`, see `src/build_id.rs`.
    xshell_buildinfo::emit();
    // The GitHub repo whose releases provide xshelld binaries (forks set their own).
    println!("cargo:rerun-if-env-changed=XSHELL_RELEASE_REPO");
    embedded_windows_asset();
    embedded_own_daemon();
    embedded_linux_asset();
    tauri_build::build()
}

fn embedded_windows_asset() {
    use std::path::{Path, PathBuf};
    println!("cargo:rerun-if-env-changed=XSHELL_WINDOWS_DAEMON_ASSET");
    println!("cargo:rerun-if-env-changed=XSHELL_WINDOWS_DAEMON_SHA256");
    let path = std::env::var_os("XSHELL_WINDOWS_DAEMON_ASSET");
    let hash = std::env::var("XSHELL_WINDOWS_DAEMON_SHA256").ok();
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let windows_target = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let generated = match (path, hash) {
        // A Windows Desktop serves its own embedded `xshelld.exe` (one copy of the bytes)
        // to SSH Windows Hosts too.
        (None, None) if windows_target => {
            "const EMBEDDED_WINDOWS_DAEMON: Option<(&[u8], &str)> = EMBEDDED_OWN_DAEMON;".into()
        }
        (None, None) => "const EMBEDDED_WINDOWS_DAEMON: Option<(&[u8], &str)> = None;".into(),
        (Some(path), Some(hash)) => {
            if windows_target {
                let own = std::env::var("XSHELL_DAEMON_SHA256").ok();
                if !own.is_some_and(|own| own.trim().eq_ignore_ascii_case(hash.trim())) {
                    panic!("on a Windows Desktop XSHELL_WINDOWS_DAEMON_* must be XSHELL_DAEMON_* (the same xshelld.exe)");
                }
            }
            let path = PathBuf::from(path)
                .canonicalize()
                .expect("Windows Daemon asset path");
            println!("cargo:rerun-if-changed={}", path.display());
            let stamp = xshell_buildinfo::resolve(Path::new(env!("CARGO_MANIFEST_DIR")), &|key| {
                std::env::var(key).ok()
            })
            .expect("Desktop source identity")
            .expect("embedded Windows Daemon requires a known source identity");
            let expected = xshell_protocol::build::BuildId {
                commit: stamp.commit,
                dirty: stamp.dirty,
                time: stamp.time,
            };
            let bytes = std::fs::read(&path).expect("read Windows Daemon asset");
            windows_asset::verify(&bytes, &hash, &expected)
                .expect("invalid embedded Windows Daemon");
            std::fs::write(out.join("windows-daemon.bin"), bytes)
                .expect("copy Windows Daemon asset");
            format!("const EMBEDDED_WINDOWS_DAEMON: Option<(&[u8], &str)> = Some((include_bytes!(concat!(env!(\"OUT_DIR\"), \"/windows-daemon.bin\")), {hash:?}));")
        }
        _ => panic!(
            "set both XSHELL_WINDOWS_DAEMON_ASSET and XSHELL_WINDOWS_DAEMON_SHA256, or neither"
        ),
    };
    std::fs::write(out.join("windows-daemon.rs"), generated).expect("write Windows Daemon include");
}

/// The Desktop's own-target `xshelld` (ADR-0017): `XSHELL_DAEMON_ASSET` (a path) and
/// `XSHELL_DAEMON_SHA256`, both or neither. Linux, macOS and x86_64 Windows; checked by
/// `daemon_asset::verify_own` against this Desktop's build, so a release never carries the
/// wrong Daemon.
fn embedded_own_daemon() {
    use std::path::{Path, PathBuf};
    println!("cargo:rerun-if-env-changed=XSHELL_DAEMON_ASSET");
    println!("cargo:rerun-if-env-changed=XSHELL_DAEMON_SHA256");
    let path = std::env::var_os("XSHELL_DAEMON_ASSET");
    let hash = std::env::var("XSHELL_DAEMON_SHA256").ok();
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let generated = match (path, hash) {
        (None, None) => "const EMBEDDED_OWN_DAEMON: Option<(&[u8], &str)> = None;".into(),
        (Some(path), Some(hash)) => {
            if !matches!(target_os.as_str(), "linux" | "macos")
                && !(target_os == "windows" && target_arch == "x86_64")
            {
                panic!("XSHELL_DAEMON_ASSET is for Linux, macOS and x86_64 Windows Desktops, not {target_os} {target_arch}");
            }
            let path = PathBuf::from(path)
                .canonicalize()
                .expect("own Daemon asset path");
            println!("cargo:rerun-if-changed={}", path.display());
            let stamp = xshell_buildinfo::resolve(Path::new(env!("CARGO_MANIFEST_DIR")), &|key| {
                std::env::var(key).ok()
            })
            .expect("Desktop source identity")
            .expect("an embedded Daemon requires a known source identity");
            let expected = xshell_protocol::build::BuildId {
                commit: stamp.commit,
                dirty: stamp.dirty,
                time: stamp.time,
            };
            let bytes = std::fs::read(&path).expect("read own Daemon asset");
            daemon_asset::verify_own(&bytes, &hash, &expected, &target_os, &target_arch)
                .expect("invalid embedded Daemon");
            std::fs::write(out.join("own-daemon.bin"), bytes).expect("copy own Daemon asset");
            format!("const EMBEDDED_OWN_DAEMON: Option<(&[u8], &str)> = Some((include_bytes!(concat!(env!(\"OUT_DIR\"), \"/own-daemon.bin\")), {hash:?}));")
        }
        _ => panic!("set both XSHELL_DAEMON_ASSET and XSHELL_DAEMON_SHA256, or neither"),
    };
    std::fs::write(out.join("own-daemon.rs"), generated).expect("write own Daemon include");
}

/// The x86_64 musl `xshelld` of this build (W5): `XSHELL_LINUX_DAEMON_ASSET` (a path) and
/// `XSHELL_LINUX_DAEMON_SHA256`, both or neither. For Desktops that are not Linux (a
/// Windows Desktop updates Linux machines over the Ring without a download); a Linux
/// Desktop embeds its own Daemon instead. Checked like the own Daemon: an x86_64 ELF of
/// this very build.
fn embedded_linux_asset() {
    use std::path::{Path, PathBuf};
    println!("cargo:rerun-if-env-changed=XSHELL_LINUX_DAEMON_ASSET");
    println!("cargo:rerun-if-env-changed=XSHELL_LINUX_DAEMON_SHA256");
    let path = std::env::var_os("XSHELL_LINUX_DAEMON_ASSET");
    let hash = std::env::var("XSHELL_LINUX_DAEMON_SHA256").ok();
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let generated = match (path, hash) {
        (None, None) => "const EMBEDDED_LINUX_DAEMON: Option<(&[u8], &str)> = None;".into(),
        (Some(path), Some(hash)) => {
            if target_os == "linux" {
                panic!(
                    "XSHELL_LINUX_DAEMON_ASSET is not for Linux Desktops: use XSHELL_DAEMON_ASSET"
                );
            }
            let path = PathBuf::from(path)
                .canonicalize()
                .expect("Linux Daemon asset path");
            println!("cargo:rerun-if-changed={}", path.display());
            let stamp = xshell_buildinfo::resolve(Path::new(env!("CARGO_MANIFEST_DIR")), &|key| {
                std::env::var(key).ok()
            })
            .expect("Desktop source identity")
            .expect("an embedded Linux Daemon requires a known source identity");
            let expected = xshell_protocol::build::BuildId {
                commit: stamp.commit,
                dirty: stamp.dirty,
                time: stamp.time,
            };
            let bytes = std::fs::read(&path).expect("read Linux Daemon asset");
            daemon_asset::verify_own(&bytes, &hash, &expected, "linux", "x86_64")
                .expect("invalid embedded Linux Daemon");
            std::fs::write(out.join("linux-daemon.bin"), bytes).expect("copy Linux Daemon asset");
            format!("const EMBEDDED_LINUX_DAEMON: Option<(&[u8], &str)> = Some((include_bytes!(concat!(env!(\"OUT_DIR\"), \"/linux-daemon.bin\")), {hash:?}));")
        }
        _ => {
            panic!("set both XSHELL_LINUX_DAEMON_ASSET and XSHELL_LINUX_DAEMON_SHA256, or neither")
        }
    };
    std::fs::write(out.join("linux-daemon.rs"), generated).expect("write Linux Daemon include");
}

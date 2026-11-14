mod daemon_asset;
mod windows_asset;

fn main() {
    // The build identity (Lesani/xshell#49): `XSHELL_BUILDINFO_STAMP`, see `src/build_id.rs`.
    xshell_buildinfo::emit();
    // The GitHub repo whose releases provide xshelld binaries (forks set their own).
    println!("cargo:rerun-if-env-changed=XSHELL_RELEASE_REPO");
    embedded_windows_asset();
    embedded_own_daemon();
    tauri_build::build()
}

fn embedded_windows_asset() {
    use std::path::{Path, PathBuf};
    println!("cargo:rerun-if-env-changed=XSHELL_WINDOWS_DAEMON_ASSET");
    println!("cargo:rerun-if-env-changed=XSHELL_WINDOWS_DAEMON_SHA256");
    let path = std::env::var_os("XSHELL_WINDOWS_DAEMON_ASSET");
    let hash = std::env::var("XSHELL_WINDOWS_DAEMON_SHA256").ok();
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let generated = match (path, hash) {
        (None, None) => "const EMBEDDED_WINDOWS_DAEMON: Option<(&[u8], &str)> = None;".into(),
        (Some(path), Some(hash)) => {
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
/// `XSHELL_DAEMON_SHA256`, both or neither. Linux and macOS only; checked by
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
            if !matches!(target_os.as_str(), "linux" | "macos") {
                panic!("XSHELL_DAEMON_ASSET is for Linux and macOS Desktops, not {target_os}");
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

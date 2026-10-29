mod windows_asset;

fn main() {
    // The build identity (Lesani/xshell#49): `XSHELL_BUILDINFO_STAMP`, see `src/build_id.rs`.
    xshell_buildinfo::emit();
    // The GitHub repo whose releases provide xshelld binaries (forks set their own).
    println!("cargo:rerun-if-env-changed=XSHELL_RELEASE_REPO");
    embedded_windows_asset();
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

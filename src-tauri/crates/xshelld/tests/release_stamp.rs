//! A `--profile release-daemon` build (LTO, stripped: what releases ship) keeps a build stamp
//! that `scan_stamp` finds and that matches `--version` (Lesani/xshell#49).

use std::path::Path;
use std::process::Command;
use xshell_protocol::build::{parse_stamp, scan_stamp};

#[test]
#[ignore = "builds xshelld with --profile release-daemon (minutes); run with --ignored"]
fn release_daemon_build_keeps_a_scannable_stamp() {
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Its own target directory: the running `cargo test` may hold the workspace's lock.
    let target = ws.join("target/release-daemon-stamp-check");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "xshelld",
            "--profile",
            "release-daemon",
            "--locked",
        ])
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("XSHELL_BUILD_COMMIT")
        .env_remove("XSHELL_BUILD_DIRTY")
        .env_remove("XSHELL_BUILD_TIME")
        .current_dir(&ws)
        .status()
        .unwrap();
    assert!(status.success());
    let bin = target
        .join("release-daemon")
        .join(format!("xshelld{}", std::env::consts::EXE_SUFFIX));
    let want = parse_stamp(env!("XSHELL_BUILDINFO_STAMP")).unwrap();
    let found = scan_stamp(&std::fs::read(&bin).unwrap());
    assert_eq!(found, Ok(want.clone()), "the stamp in {}", bin.display());
    let out = Command::new(&bin).arg("--version").output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["build"], serde_json::json!(want));
}

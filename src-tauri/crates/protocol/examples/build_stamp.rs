//! Checks the build identity stamped into a binary (Lesani/xshell#49), without running it, so
//! release CI can check binaries for other targets:
//!
//!     cargo run -p xshell-protocol --no-default-features --example build_stamp -- \
//!         <binary> <expected display or `unknown`> [<expected commit time>]
//!
//! Prints the identity found; exits 1 when it differs from the expected one or none is found.
//! A universal (fat) Mach-O binary passes only when every slice carries the expected stamp.

use std::process::ExitCode;
use xshell_protocol::build::scan_stamp;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, want, want_time) = match args.as_slice() {
        [p, w] => (p, w, None),
        [p, w, t] => (p, w, Some(t)),
        _ => {
            eprintln!("usage: build_stamp <binary> <expected display|unknown> [<time>]");
            return ExitCode::from(2);
        }
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let found = match scan_stamp(&bytes) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (display, time) = match &found {
        Some(b) => (b.display(), Some(b.time.to_string())),
        None => ("unknown".to_string(), None),
    };
    println!(
        "{path}: build {display} (time {})",
        time.as_deref().unwrap_or("-")
    );
    if display != *want || want_time.is_some_and(|t| time.as_ref() != Some(t)) {
        eprintln!(
            "{path}: expected build {want}{}",
            want_time
                .map(|t| format!(" (time {t})"))
                .unwrap_or_default()
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

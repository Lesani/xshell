//! `xshelld`, the Remote Host Daemon: serves one Host's Terminals and Host-side commands to
//! any number of Desktops over a per-user Unix socket. Unix only.
#![cfg(unix)]

pub mod cli;
pub mod connect;
pub mod env;
pub mod log;
pub mod paths;
pub mod server;

use cli::Command;
use std::path::PathBuf;

/// The `xshelld` binary: returns the process exit code.
pub fn main_entry() -> i32 {
    let cmd = match cli::parse(std::env::args_os().skip(1), &|k| std::env::var_os(k)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xshelld: {e}\n\n{}", cli::USAGE);
            return 2;
        }
    };
    let (opts, serve) = match cmd {
        Command::Version => {
            println!("{}", cli::version_json());
            return 0;
        }
        Command::Help => {
            print!("{}", cli::USAGE);
            return 0;
        }
        Command::Event(args) => {
            // The hook client: no login environment, no logging, always exit 0.
            let home = std::env::var_os("XSHELLD_HOME")
                .map(PathBuf::from)
                .or_else(dirs::home_dir);
            let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
            let sock = std::env::var_os("XSHELLD_SOCKET").map(PathBuf::from);
            let default = home.map(|h| paths::resolve(&h, xdg.as_deref(), sock.as_deref()).socket);
            return xshell_core::agent_status::event_main(&args, &|k| std::env::var_os(k), default);
        }
        Command::Serve(o) => (o, true),
        Command::Connect(o) => (o, false),
    };
    if serve {
        // Before any thread exists, so only the sigwait thread ever receives them.
        server::block_exit_signals();
        env::prepare();
    }
    let Some(home) = opts.home.clone().or_else(dirs::home_dir) else {
        eprintln!("xshelld: cannot determine the home directory; pass --home");
        return 1;
    };
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let paths = paths::resolve(&home, xdg.as_deref(), opts.socket.as_deref());
    if serve {
        let mut cfg = server::Config::new(home, paths);
        if let Some(t) = opts.idle_timeout {
            cfg.idle_timeout = t;
        }
        cfg.test_hook = crash_after_replacement_hook();
        server::run_serve(cfg)
    } else {
        connect::run_connect(&opts, &paths)
    }
}

/// Test hook: with `XSHELLD_TEST_CRASH_AFTER_REPLACEMENT=<file>`, `serve` aborts (a crash: no
/// cleanup) as soon as a Relaunch's replacement process exists, once `<file>` names its pid on
/// a line (at most 10 s), so a test knows the replacement is past its startup.
fn crash_after_replacement_hook() -> Option<server::TestHook> {
    let file = std::env::var_os("XSHELLD_TEST_CRASH_AFTER_REPLACEMENT")?;
    Some(server::TestHook(std::sync::Arc::new(move |_, point| {
        if let server::TestPoint::ReplacementSpawned { pid } = point {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let named = || {
                std::fs::read_to_string(&file)
                    .unwrap_or_default()
                    .lines()
                    .any(|l| l.trim() == pid.to_string())
            };
            while !named() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            std::process::abort();
        }
        false
    })))
}

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
        server::run_serve(cfg)
    } else {
        connect::run_connect(&opts, &paths)
    }
}

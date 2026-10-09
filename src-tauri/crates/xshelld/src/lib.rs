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
    }
    let Some(home) = opts.home.clone().or_else(dirs::home_dir) else {
        eprintln!("xshelld: cannot determine the home directory; pass --home");
        return 1;
    };
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let paths = paths::resolve(&home, xdg.as_deref(), opts.socket.as_deref());
    if !serve {
        return connect::run_connect(&opts, &paths);
    }
    let gui_bound = opts
        .parent_pid
        .map(|parent_pid| server::GuiBound { parent_pid });
    let stop = std::sync::Arc::new(server::StopLatch::default());
    if let Some(g) = gui_bound {
        // Nobody reads our output: it goes to the log, as for a `serve` `connect` starts.
        if let Err(e) = log::redirect_to_log(&paths.log) {
            crate::log!("WARN", "cannot log to {}: {e}", paths.log.display());
        }
        // Armed first: the parent may die while the login shell or the restore is slow.
        let s = stop.clone();
        if let Err(e) = server::parent::watch(g.parent_pid, move || s.trigger()) {
            crate::log!("ERROR", "cannot start: {e}");
            return 1;
        }
    }
    if let Err(e) = server::watch_exit_signals(stop.clone()) {
        crate::log!("ERROR", "cannot start: {e}");
        return 1;
    }
    env::prepare(gui_bound.is_some() || opts.interactive_env, &|| {
        stop.is_set()
    });
    if stop.is_set() {
        crate::log!("INFO", "stopped while starting; exiting");
        return 0;
    }
    let mut cfg = server::Config::new(home, paths);
    if let Some(t) = opts.idle_timeout {
        cfg.idle_timeout = t;
    }
    cfg.gui_bound = gui_bound;
    cfg.test_hook = test_hook();
    server::run_serve(cfg, stop)
}

/// The test hooks the environment asks for (see [`crash_after_replacement_hook`] and
/// [`restore_delay_hook`]).
fn test_hook() -> Option<server::TestHook> {
    let hooks: Vec<server::TestHook> = [crash_after_replacement_hook(), restore_delay_hook()]
        .into_iter()
        .flatten()
        .collect();
    if hooks.is_empty() {
        return None;
    }
    Some(server::TestHook(std::sync::Arc::new(move |id, point| {
        // Every hook runs, whatever an earlier one returned.
        let mut any = false;
        for h in &hooks {
            any |= (h.0)(id, point);
        }
        any
    })))
}

/// Test hook: with `XSHELLD_TEST_RESTORE_DELAY_MS=<ms>`, restore waits that long before
/// each persisted Terminal, so a test can end the parent while a start is in progress.
fn restore_delay_hook() -> Option<server::TestHook> {
    let ms: u64 = std::env::var("XSHELLD_TEST_RESTORE_DELAY_MS")
        .ok()?
        .parse()
        .ok()?;
    Some(server::TestHook(std::sync::Arc::new(move |_, point| {
        if point == server::TestPoint::Restore {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        false
    })))
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

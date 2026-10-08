//! End to end through the real `ssh`: auto-install from a binary directory, then terminals.
//! Ignored by default; CI runs it against a throwaway sshd with
//! `XSHELL_E2E_SSH_TARGET=<alias> XSHELL_E2E_BIN_DIR=<dir with xshelld-<triple>>`.
#![cfg(unix)]

mod common;
mod desktop;

use desktop::*;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_hostlink::install::kill_daemon_script;
use xshell_hostlink::process::run_script;
use xshell_hostlink::transport::sh_wrap;
use xshell_hostlink::{CancelToken, DirSource, HostErrorHint, SshTransport, StatusKind, Transport};

fn env() -> Option<(String, String)> {
    match (
        std::env::var("XSHELL_E2E_SSH_TARGET"),
        std::env::var("XSHELL_E2E_BIN_DIR"),
    ) {
        (Ok(t), Ok(d)) if !t.is_empty() && !d.is_empty() => Some((t, d)),
        _ => {
            eprintln!("skipped: set XSHELL_E2E_SSH_TARGET and XSHELL_E2E_BIN_DIR");
            None
        }
    }
}

fn ssh_desk(target: &str, bin_dir: &str) -> Desk {
    let t = target.to_string();
    let factory = Arc::new(Fixed(Arc::new(move || {
        Box::new(SshTransport::new(t.clone())) as Box<dyn Transport>
    })));
    let rec = Recorder::new();
    let cfg = manager_config(factory, Arc::new(DirSource(bin_dir.into())), rec.clone());
    let mut host = host_config(None);
    host.ssh_target = target.into();
    Desk::new(cfg, rec, host)
}

fn ssh_run(target: &str, script: &str) -> String {
    let out = run_script(
        &SshTransport::new(target),
        &sh_wrap(script),
        None,
        Duration::from_secs(30),
        &CancelToken::new(),
    )
    .expect("ssh runs");
    out.stdout
}

/// Ends the remote Daemon (pidfile SIGTERM) after the Desktop is gone, on failures too.
struct RemoteDaemonGuard(String);

impl Drop for RemoteDaemonGuard {
    fn drop(&mut self) {
        ssh_run(&self.0, &kill_daemon_script());
    }
}

#[test]
#[ignore]
fn ssh_auto_install_open_close() {
    let Some((target, bin_dir)) = env() else {
        return;
    };
    let _cleanup = RemoteDaemonGuard(target.clone());
    let d = ssh_desk(&target, &bin_dir);
    let s = d.wait_usable();
    assert_eq!(s.status, StatusKind::Connected, "{s:?}");
    assert_eq!(s.daemon_version.as_deref(), Some(VERSION));
    let check = format!("test -x \"$HOME/.xshell/server/{VERSION}/xshelld\" && echo installed");
    assert_eq!(ssh_run(&target, &check).trim(), "installed");

    let h = d.host();
    let home = call(&h, "get_home_dir", json!({})).unwrap();
    let t = Uuid::new_v4();
    let sink = VecSink::new();
    let spec = LaunchSpec {
        cwd: home.as_str().unwrap().into(),
        shell_mode: Some("raw".into()),
        shell_command: Some("/bin/sh".into()),
        ..Default::default()
    };
    assert!(open(&h, t, spec, sink.clone()).is_some());
    marker(&h, &sink, t, "overssh");
    let n = d.rec.list_count();
    close(&h, t);
    d.rec.wait_list_from(n, "without the terminal", |l| {
        l.iter().all(|i| i.terminal != t)
    });
    sink.wait_exit();
    d.m.shutdown();
}

#[test]
#[ignore]
fn ssh_unresolvable_target_hint() {
    let Some((_, bin_dir)) = env() else {
        return;
    };
    let d = ssh_desk("nope.invalid", &bin_dir);
    let s = d
        .rec
        .wait_status("offline", |s| s.status == StatusKind::Offline);
    assert_eq!(s.error_hint, Some(HostErrorHint::Unresolved), "{s:?}");
    d.m.shutdown();
}

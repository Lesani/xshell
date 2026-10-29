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

/// The update regression uses a fresh remote HOME and runtime directory for each case,
/// even when the sshd's dedicated user is shared with another test. No existing Daemon,
/// selection, Terminal or SSH configuration is read or stopped.
#[test]
#[ignore]
fn ssh_selected_updates_restore_uuid_with_exact_identity() {
    use xshell_hostlink::transport::sh_quote;
    let Some((target, bin_dir)) = env() else {
        return;
    };
    let triple = xshell_hostlink::install::target_triple("Linux", std::env::consts::ARCH).unwrap();
    let bytes =
        std::fs::read(std::path::Path::new(&bin_dir).join(format!("xshelld-{triple}"))).unwrap();
    let expected = xshell_protocol::build::scan_stamp(&bytes).unwrap().unwrap();
    struct Isolated {
        target: String,
        home: String,
    }
    impl Transport for Isolated {
        fn command(&self, remote: &str) -> xshell_hostlink::transport::CommandSpec {
            let home = sh_quote(&self.home);
            let script = format!(
                "export HOME={home} XSHELLD_HOME={home} XDG_RUNTIME_DIR={home}/run; {remote}"
            );
            SshTransport::new(&self.target).command(&format!("sh -c {}", sh_quote(&script)))
        }
        fn describe(&self) -> String {
            "isolated SSH update fixture".into()
        }
    }
    struct Cleanup(Arc<Isolated>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let script = format!("sh -c {}; for i in $(seq 1 100); do [ ! -f \"$XDG_RUNTIME_DIR/xshell/daemon.pid\" ] && break; sleep 0.1; done; rm -rf {}", sh_quote(&kill_daemon_script()), sh_quote(&self.0.home));
            let _ = run_script(
                &*self.0,
                &sh_wrap(&script),
                None,
                Duration::from_secs(30),
                &CancelToken::new(),
            );
        }
    }
    for older in [true, false] {
        let home = ssh_run(&target, "mktemp -d /tmp/x77-ssh.XXXXXXXX")
            .trim()
            .to_string();
        assert!(home.starts_with("/tmp/x77-ssh.") && !home.contains('\n'));
        let isolated = Arc::new(Isolated {
            target: target.clone(),
            home: home.clone(),
        });
        let _cleanup = Cleanup(isolated.clone());
        // The debug fixture retains a contiguous version literal (an optimized native
        // compiler may encode the five bytes as separate instruction immediates).
        let mut initial = if older {
            std::fs::read(common::bin()).unwrap()
        } else {
            bytes.clone()
        };
        let initial_build = xshell_protocol::build::scan_stamp(&initial)
            .unwrap()
            .unwrap();
        let old_build = format!(
            "@@XSHELL-BUILD-V1:{}:{}@@",
            initial_build.display(),
            initial_build.time
        );
        let version = VERSION.as_bytes();
        for i in 0..initial.len() {
            if initial[i..].starts_with(old_build.as_bytes()) {
                let j = i + "@@XSHELL-BUILD-V1:".len();
                initial[j] = if initial[j] == b'0' { b'1' } else { b'0' };
            }
            if older && initial[i..].starts_with(version) {
                assert_eq!(version, b"1.5.0");
                initial[i..i + 5].copy_from_slice(b"1.4.0");
            }
        }
        let upload = run_script(&*isolated, &sh_wrap("set -e; mkdir -m 700 -p \"$HOME/run\" \"$HOME/initial\"; cat > \"$HOME/initial/xshelld\"; chmod 700 \"$HOME/initial/xshelld\""), Some(initial), Duration::from_secs(60), &CancelToken::new()).unwrap();
        assert_eq!(upload.code, Some(0), "{}", upload.stderr);
        let factory = Arc::new(Fixed(Arc::new({
            let isolated = isolated.clone();
            move || {
                Box::new(Isolated {
                    target: isolated.target.clone(),
                    home: isolated.home.clone(),
                }) as Box<dyn Transport>
            }
        })));
        let rec = Recorder::new();
        let mut cfg = manager_config(
            factory,
            Arc::new(DirSource(bin_dir.clone().into())),
            rec.clone(),
        );
        cfg.desktop_build = Some(expected.clone());
        let mut host = host_config(Some(sh_quote(&format!("{home}/initial/xshelld"))));
        host.ssh_target = target.clone();
        let desk = Desk::new(cfg, rec, host);
        let first = desk.wait_usable();
        assert_eq!(
            first.status,
            if older {
                StatusKind::UpgradePending
            } else {
                StatusKind::BuildDiffers
            }
        );
        let h = desk.host();
        let terminal = Uuid::new_v4();
        let pid = open(
            &h,
            terminal,
            LaunchSpec {
                cwd: home.clone(),
                shell_mode: Some("raw".into()),
                shell_command: Some("/bin/sh".into()),
                ..Default::default()
            },
            VecSink::new(),
        )
        .unwrap();
        let from = desk.rec.count();
        upgrade(&h).unwrap();
        let (_, done) = desk
            .rec
            .wait_status_from(from, "exact update reconnect", |s| {
                s.status == StatusKind::Connected
                    && s.phase.is_none()
                    && s.link_generation > first.link_generation
            });
        assert_eq!(done.daemon_version.as_deref(), Some(VERSION));
        assert!(done.daemon_build.as_ref().unwrap().same(&expected));
        assert!(done.last_error.is_none(), "{done:?}");
        let restored = desk
            .rec
            .wait_list("same Terminal UUID after SSH update", |list| {
                list.iter()
                    .any(|t| t.terminal == terminal && t.pid.is_some() && t.pid != Some(pid))
            });
        assert_eq!(restored.len(), 1);
        desk.m.shutdown();
    }
}

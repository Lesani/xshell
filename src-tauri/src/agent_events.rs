//! The Desktop's agent event socket: Local Host Terminals run in this process, so their
//! agents' hooks report here, through the Desktop executable itself (`xshell event …`, see
//! `run_cli`). Unix only: on Windows, Local Host Terminals get their Agent Status once they
//! move to the GUI-bound Daemon (xshell#24), and launch without hooks until then.

use crate::local_pty::LocalPtys;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use xshell_core::agent_status::{serve_event_conn, AgentHooks};

/// A hook client that connects and then says nothing is dropped after this.
const CONN_TIMEOUT: Duration = Duration::from_secs(10);

/// `$XDG_RUNTIME_DIR/xshell/desktop-<pid>.sock`, else `~/.xshell/run/desktop-<pid>.sock`
/// (the base the Daemon uses for its own socket). Per process: two Desktops never share one.
pub fn socket_path(
    home: Option<&Path>,
    xdg_runtime_dir: Option<&Path>,
    pid: u32,
) -> Option<PathBuf> {
    let base = match xdg_runtime_dir.filter(|p| p.is_absolute()) {
        Some(x) => x.join("xshell"),
        None => home?.join(".xshell").join("run"),
    };
    Some(base.join(format!("desktop-{pid}.sock")))
}

/// Create `dir` (and parents) as 0700; an existing one must be ours, and is tightened.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not owned by us", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The listening socket; [`EventSocket::remove`] takes its file away on exit.
pub struct EventSocket {
    pub path: PathBuf,
}

impl EventSocket {
    pub fn remove(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Bind the event socket at `socket` (0600 in a 0700 dir), write the Claude Code settings
/// into `settings_dir` (0700) as `claude-settings.json` (0600), serve reports into `ptys`,
/// and only then let `ptys` launch agents with hooks naming `exe`. On any failure nothing is
/// left listening and agents launch without hooks, as before.
pub fn start(
    ptys: Arc<LocalPtys>,
    exe: PathBuf,
    settings_dir: &Path,
    socket: PathBuf,
) -> io::Result<EventSocket> {
    let dir = socket
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no directory"))?;
    ensure_private_dir(dir)?;
    match fs::remove_file(&socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(&socket)?;
    let sock = EventSocket {
        path: socket.clone(),
    };
    let hooks = (|| {
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        ensure_private_dir(settings_dir)?;
        let hooks = AgentHooks {
            exe,
            endpoint: socket.to_string_lossy().into_owned(),
            claude_settings: settings_dir.join("claude-settings.json"),
        };
        hooks.write_claude_settings()?;
        Ok::<_, io::Error>(hooks)
    })();
    let hooks = match hooks {
        Ok(h) => h,
        Err(e) => {
            sock.remove();
            return Err(e);
        }
    };
    let p = ptys.clone();
    std::thread::Builder::new()
        .name("agent-events".into())
        .spawn(move || {
            for s in listener.incoming().flatten() {
                let p = p.clone();
                let _ = std::thread::Builder::new()
                    .name("agent-event".into())
                    .spawn(move || {
                        let _ = s.set_read_timeout(Some(CONN_TIMEOUT));
                        let _ = s.set_write_timeout(Some(CONN_TIMEOUT));
                        serve_event_conn(s, env!("CARGO_PKG_VERSION"), |t, run, status| {
                            p.on_agent_event(t, run, status)
                        });
                    });
            }
        })
        .inspect_err(|_| sock.remove())?;
    ptys.set_hooks(hooks);
    Ok(sock)
}

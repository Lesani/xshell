//! `xshelld connect`: bridge stdio to the Daemon's socket, starting `serve` if none runs,
//! unless the machine's Daemon is GUI-bound (xshell runs it there, ADR-0005): then a missing
//! Daemon means xshell is closed, and `connect` exits with [`NOT_RUNNING_EXIT`] instead.
//! Holds no state and writes nothing to stdout but bridged bytes; diagnostics go to stderr,
//! which the Desktop shows as the ssh error text.

use crate::cli::Opts;
use crate::paths::{check_socket_path_len, read_mode, Mode, Paths};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use xshell_protocol::{NOT_RUNNING_EXIT, NOT_RUNNING_MESSAGE};

const SPAWN_WAIT: Duration = Duration::from_secs(10);

pub fn run_connect(opts: &Opts, paths: &Paths) -> i32 {
    match connect_or_spawn(paths, opts) {
        Ok(sock) => bridge(sock),
        Err(Refused::NotRunning) => {
            eprintln!("xshelld: {NOT_RUNNING_MESSAGE}");
            NOT_RUNNING_EXIT
        }
        Err(Refused::Io(e)) => {
            eprintln!("xshelld: {e}");
            1
        }
    }
}

enum Refused {
    /// No Daemon runs, and only xshell may start one here.
    NotRunning,
    Io(io::Error),
}

impl From<io::Error> for Refused {
    fn from(e: io::Error) -> Self {
        Refused::Io(e)
    }
}

fn connect_or_spawn(paths: &Paths, opts: &Opts) -> Result<UnixStream, Refused> {
    check_socket_path_len(&paths.socket)?;
    match UnixStream::connect(&paths.socket) {
        Ok(s) => return Ok(s),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e.into()),
    }
    if read_mode(&paths.mode) == Some(Mode::GuiBound) {
        return Err(Refused::NotRunning);
    }
    let mut child = Reaped(Some(spawn_detached_serve(paths, opts)?));
    let deadline = Instant::now() + SPAWN_WAIT;
    let mut delay = Duration::from_millis(10);
    let mut lost_race = false;
    loop {
        if let Ok(s) = UnixStream::connect(&paths.socket) {
            return Ok(s);
        }
        if !lost_race {
            if let Some(st) = child.try_wait()? {
                child.0 = None; // reaped
                                // 3: another `serve` won the lock; it will be listening shortly.
                if st.code() == Some(3) {
                    lost_race = true;
                } else {
                    return Err(io::Error::other(format!(
                        "xshelld serve exited with {st}; see {}",
                        paths.log.display()
                    ))
                    .into());
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "xshelld serve did not start listening within {SPAWN_WAIT:?}; see {}",
                    paths.log.display()
                ),
            )
            .into());
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}

/// A spawned `serve`, reaped whatever path we leave by: on drop an unreaped child is handed
/// to a thread that waits for it, so a racer that lost the lock (exit 3) never lingers as a
/// zombie for the lifetime of the bridge. The winner is waited for too, harmlessly: the
/// thread just blocks until `connect` itself exits.
struct Reaped(Option<Child>);

impl Reaped {
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        match &mut self.0 {
            Some(c) => c.try_wait(),
            None => Ok(None),
        }
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            if let Ok(None) = c.try_wait() {
                let _ = std::thread::Builder::new()
                    .name("reap-serve".into())
                    .spawn(move || {
                        let _ = c.wait();
                    });
            }
        }
    }
}

fn spawn_detached_serve(paths: &Paths, opts: &Opts) -> io::Result<Child> {
    let log = crate::log::open_log(&paths.log)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("serve");
    if let Some(h) = &opts.home {
        cmd.arg("--home").arg(h);
    }
    if let Some(s) = &opts.socket {
        cmd.arg("--socket").arg(s);
    }
    if let Some(t) = opts.idle_timeout {
        cmd.arg("--idle-timeout-ms").arg(t.as_millis().to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .current_dir("/");
    // A new session: the Daemon outlives this ssh session and its hangup.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Copy stdin → socket and socket → stdout until the Daemon side closes.
fn bridge(sock: UnixStream) -> i32 {
    let to_daemon = match sock.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("xshelld: {e}");
            return 1;
        }
    };
    let _ = std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut to_daemon = to_daemon;
            let mut stdin = io::stdin().lock();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if to_daemon.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = to_daemon.shutdown(Shutdown::Write);
        });
    let mut from_daemon = sock;
    let mut stdout = io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from_daemon.read(&mut buf) {
            Ok(0) => break,
            // Flush every chunk: stdout is line-buffered and frames rarely end in '\n'.
            Ok(n) => {
                if stdout
                    .write_all(&buf[..n])
                    .and_then(|_| stdout.flush())
                    .is_err()
                {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    0
}

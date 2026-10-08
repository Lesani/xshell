//! `xshelld connect`: bridge stdio to the Daemon's socket, starting `serve` if none runs.
//! Holds no state and writes nothing to stdout but bridged bytes; diagnostics go to stderr,
//! which the Desktop shows as the ssh error text.

use crate::cli::Opts;
use crate::paths::{check_socket_path_len, ensure_private_dir, Paths};
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SPAWN_WAIT: Duration = Duration::from_secs(10);
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

pub fn run_connect(opts: &Opts, paths: &Paths) -> i32 {
    match connect_or_spawn(paths, opts) {
        Ok(sock) => bridge(sock),
        Err(e) => {
            eprintln!("xshelld: {e}");
            1
        }
    }
}

fn connect_or_spawn(paths: &Paths, opts: &Opts) -> io::Result<UnixStream> {
    check_socket_path_len(&paths.socket)?;
    match UnixStream::connect(&paths.socket) {
        Ok(s) => return Ok(s),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e),
    }
    let mut child = spawn_detached_serve(paths, opts)?;
    let deadline = Instant::now() + SPAWN_WAIT;
    let mut delay = Duration::from_millis(10);
    let mut lost_race = false;
    loop {
        if let Ok(s) = UnixStream::connect(&paths.socket) {
            return Ok(s);
        }
        if !lost_race {
            if let Some(st) = child.try_wait()? {
                // 3: another `serve` won the lock; it will be listening shortly.
                if st.code() == Some(3) {
                    lost_race = true;
                } else {
                    return Err(io::Error::other(format!(
                        "xshelld serve exited with {st}; see {}",
                        paths.log.display()
                    )));
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
            ));
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}

fn spawn_detached_serve(paths: &Paths, opts: &Opts) -> io::Result<Child> {
    if let Some(dir) = paths.log.parent() {
        ensure_private_dir(dir)?;
    }
    if fs::metadata(&paths.log).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let mut old = paths.log.clone().into_os_string();
        old.push(".1");
        let _ = fs::rename(&paths.log, old);
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&paths.log)?;
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

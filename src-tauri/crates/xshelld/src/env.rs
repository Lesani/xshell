//! The environment every Terminal inherits. Runs once at the start of `serve`, before any
//! thread that reads the environment exists. `CommandBuilder::new` copies the process
//! environment, so what is set here reaches every Terminal.
//!
//! The login shell's PATH is merged in. A GUI-bound Daemon (ADR-0005), and a Persistent one
//! the app starts (`serve --interactive-env`), takes the PATH of an interactive login shell
//! instead, so entries set only in rc files (nvm, asdf, pyenv in
//! `.bashrc`/`.zshrc`) apply as they did when the app ran agents inside the user's shell;
//! it falls back to the login-only PATH.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MARKER: &str = "__XSHELL_PATH__";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5);

/// `interactive`: also source the rc files (GUI-bound or `--interactive-env`). `cancelled` aborts a slow shell.
pub fn prepare(interactive: bool, cancelled: &dyn Fn() -> bool) {
    if std::env::var_os("XSHELLD_LOGIN_ENV").as_deref() != Some("0".as_ref()) {
        let mut found = None;
        if interactive {
            match login_path(true, cancelled) {
                Ok(p) => found = Some(p),
                Err(e) => crate::log!("WARN", "interactive login PATH not used: {e}"),
            }
        }
        if found.is_none() && !cancelled() {
            match login_path(false, cancelled) {
                Ok(p) => found = Some(p),
                Err(e) => crate::log!("WARN", "login PATH not merged: {e}"),
            }
        }
        if let Some(login) = found {
            let cur = std::env::var("PATH").unwrap_or_default();
            let merged = merge_path(&cur, &login);
            if merged != cur {
                std::env::set_var("PATH", merged);
            }
        }
    }
    let term = std::env::var_os("TERM");
    if term.is_none() || term.as_deref() == Some("dumb".as_ref()) {
        std::env::set_var("TERM", "xterm-256color");
    }
    if std::env::var_os("COLORTERM").is_none() {
        std::env::set_var("COLORTERM", "truecolor");
    }
}

/// `$SHELL -l -c` (`-l -i -c` when `interactive`) prints the login PATH after a marker, so
/// profile noise is ignored. The shell runs in its own session: an interactive shell then
/// finds no terminal to take over, and a timeout ends it with everything it started.
fn login_path(interactive: bool, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    let shell = std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OsString::from("/bin/sh"));
    let flags = if interactive { "-l -i" } else { "-l" };
    let mut cmd = Command::new(&shell);
    cmd.arg("-l");
    if interactive {
        cmd.arg("-i");
    }
    cmd.arg("-c")
        .arg(format!("printf '\\n{MARKER}%s\\n' \"$PATH\""))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {shell:?}: {e}"))?;
    let mut out = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    // A profile that leaves a background job holding stdout must not hang us: the reader
    // gets its own thread and is abandoned on timeout.
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    // Its own session, so its own process group: ending the group ends whatever the shell
    // started too, also once the shell itself has exited.
    let group = child.id() as i32;
    let give_up = |child: &mut std::process::Child| {
        unsafe { libc::killpg(group, libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
    };
    let why = |what: &str| {
        if cancelled() {
            "startup was cancelled".to_string()
        } else {
            format!("{shell:?} {flags} {what}")
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline && !cancelled() => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                give_up(&mut child);
                return Err(why("timed out"));
            }
        }
    }
    // A background job of the rc files may hold stdout open after the shell exited.
    let s = loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(s) => break s,
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline && !cancelled() => {}
            Err(_) => {
                give_up(&mut child);
                return Err(why("kept stdout open"));
            }
        }
    };
    parse_marker(&s).ok_or_else(|| "no PATH marker in login shell output".to_string())
}

pub fn parse_marker(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix(MARKER))
        .map(|p| p.trim_end_matches('\r').to_string())
}

/// Prepend the login PATH entries that are missing from `cur`, keeping their order.
pub fn merge_path(cur: &str, login: &str) -> String {
    let have: Vec<&str> = cur.split(':').filter(|s| !s.is_empty()).collect();
    let mut add: Vec<&str> = Vec::new();
    for e in login.split(':').filter(|s| !s.is_empty()) {
        if !have.contains(&e) && !add.contains(&e) {
            add.push(e);
        }
    }
    if add.is_empty() {
        return cur.to_string();
    }
    let mut v = add;
    v.extend(have);
    v.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_path_parsing() {
        assert_eq!(
            parse_marker("junk\n__XSHELL_PATH__/a:/b\n").as_deref(),
            Some("/a:/b")
        );
        assert_eq!(parse_marker("nothing here\n"), None);
        assert_eq!(merge_path("/b:/c", "/a:/b:/d"), "/a:/d:/b:/c");
        assert_eq!(merge_path("/a:/b", "/b:/a"), "/a:/b");
        assert_eq!(merge_path("", "/a"), "/a");
    }
}

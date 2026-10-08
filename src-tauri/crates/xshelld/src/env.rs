//! The environment every Terminal inherits. Runs once at the start of `serve`/`connect`,
//! before any thread that reads the environment exists. `CommandBuilder::new` copies the
//! process environment, so what is set here reaches every Terminal.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MARKER: &str = "__XSHELL_PATH__";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5);

pub fn prepare() {
    if std::env::var_os("XSHELLD_LOGIN_ENV").as_deref() != Some("0".as_ref()) {
        match login_path() {
            Ok(login) => {
                let cur = std::env::var("PATH").unwrap_or_default();
                let merged = merge_path(&cur, &login);
                if merged != cur {
                    std::env::set_var("PATH", merged);
                }
            }
            Err(e) => crate::log!("WARN", "login PATH not merged: {e}"),
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

/// `$SHELL -l -c` prints the login PATH between markers (so profile noise is ignored).
fn login_path() -> Result<String, String> {
    let shell = std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OsString::from("/bin/sh"));
    let mut child = Command::new(&shell)
        .arg("-l")
        .arg("-c")
        .arg(format!("printf '\\n{MARKER}%s\\n' \"$PATH\""))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{shell:?} -l timed out"));
            }
        }
    }
    let s = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| format!("{shell:?} -l kept stdout open"))?;
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

//! Errors the frontend sees, and the hint that turns ssh's stderr into a remedy.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostErrorCode {
    UnknownHost,
    Offline,
    Incompatible,
    Timeout,
    /// The Daemon answered with an error string (the same text a local command returns).
    Remote,
    Busy,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostError {
    pub code: HostErrorCode,
    pub message: String,
}

impl HostError {
    pub fn new(code: HostErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn offline(m: impl Into<String>) -> Self {
        Self::new(HostErrorCode::Offline, m)
    }
    pub fn busy() -> Self {
        Self::new(HostErrorCode::Busy, "the connection's send queue is full")
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        Self::new(HostErrorCode::Invalid, m)
    }
    pub fn remote(m: impl Into<String>) -> Self {
        Self::new(HostErrorCode::Remote, m)
    }
    pub fn timeout(m: impl Into<String>) -> Self {
        Self::new(HostErrorCode::Timeout, m)
    }
    pub fn unknown_host(id: &str) -> Self {
        Self::new(HostErrorCode::UnknownHost, format!("unknown host {id}"))
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HostError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostErrorHint {
    HostKey,
    PermissionDenied,
    Unresolved,
    Unreachable,
    SshMissing,
    UnsupportedPlatform,
    BinaryUnavailable,
    DaemonCommandFailed,
}

/// Map an ssh failure to a hint. `stderr` is ssh's (and the remote command's) stderr.
pub fn classify_ssh_failure(
    stderr: &str,
    spawn_err: Option<&io::Error>,
    exit: Option<i32>,
) -> Option<HostErrorHint> {
    if spawn_err.is_some_and(|e| e.kind() == io::ErrorKind::NotFound) {
        return Some(HostErrorHint::SshMissing);
    }
    let s = stderr;
    let has = |needle: &str| s.contains(needle);
    if has("Host key verification failed")
        || has("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || has("host key is known for")
        || has("No matching host key")
    {
        return Some(HostErrorHint::HostKey);
    }
    if has("Permission denied (") || has("Too many authentication failures") {
        return Some(HostErrorHint::PermissionDenied);
    }
    if has("Could not resolve hostname")
        || has("Name or service not known")
        || has("nodename nor servname")
        || has("Temporary failure in name resolution")
    {
        return Some(HostErrorHint::Unresolved);
    }
    if has("Connection refused")
        || has("timed out")
        || has("No route to host")
        || has("Network is unreachable")
        || has("Connection reset")
        || has("Connection closed by")
    {
        return Some(HostErrorHint::Unreachable);
    }
    if exit == Some(127)
        || exit == Some(126)
        || has("not found")
        || has("No such file")
        || has("Permission denied")
    {
        return Some(HostErrorHint::DaemonCommandFailed);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use HostErrorHint::*;

    #[test]
    fn classify_ssh_failure_table() {
        let c = |s: &str| classify_ssh_failure(s, None, Some(255));
        assert_eq!(c("Host key verification failed.\r\n"), Some(HostKey));
        assert_eq!(
            c("user@h: Permission denied (publickey)."),
            Some(PermissionDenied)
        );
        assert_eq!(
            c("ssh: Could not resolve hostname nope.invalid: Name or service not known"),
            Some(Unresolved)
        );
        assert_eq!(
            c("ssh: connect to host h port 22: Connection refused"),
            Some(Unreachable)
        );
        assert_eq!(
            c("ssh: connect to host h port 22: Operation timed out"),
            Some(Unreachable)
        );
        assert_eq!(
            classify_ssh_failure("", None, Some(127)),
            Some(DaemonCommandFailed)
        );
        assert_eq!(
            classify_ssh_failure("sh: 1: /x/xshelld: not found", None, Some(2)),
            Some(DaemonCommandFailed)
        );
        let nf = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(classify_ssh_failure("", Some(&nf), None), Some(SshMissing));
        assert_eq!(classify_ssh_failure("weird", None, Some(1)), None);
    }

    #[test]
    fn host_error_json() {
        assert_eq!(
            serde_json::to_string(&HostError::offline("m")).unwrap(),
            r#"{"code":"offline","message":"m"}"#
        );
        assert_eq!(
            serde_json::to_string(&HostError::unknown_host("h")).unwrap(),
            r#"{"code":"unknown-host","message":"unknown host h"}"#
        );
        assert_eq!(
            serde_json::to_string(&HostErrorHint::DaemonCommandFailed).unwrap(),
            r#""daemon-command-failed""#
        );
    }
}

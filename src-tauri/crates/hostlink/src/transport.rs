//! How a remote command runs: the system `ssh` in production, `sh -c` locally in tests. One
//! function builds each command line, so tests substitute a transport and nothing else.

use crate::config::HostConfig;
use crate::dial::Dialer;
use std::ffi::OsString;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Added to the inherited environment.
    pub env: Vec<(OsString, OsString)>,
}

pub trait Transport: Send + Sync {
    /// The local command that runs `remote_cmd` (a POSIX `sh` command line) on the Host.
    fn command(&self, remote_cmd: &str) -> CommandSpec;
    fn describe(&self) -> String;
}

pub trait TransportFactory: Send + Sync {
    fn for_host(&self, cfg: &HostConfig) -> Box<dyn Transport>;

    /// A direct stream to this Host's Daemon (a local socket), used instead of running
    /// `connect` through `for_host`. Such a Host is never probed, installed or signalled by
    /// script.
    fn direct(&self, _cfg: &HostConfig) -> Option<Box<dyn Dialer>> {
        None
    }
}

/// `ssh -T -o BatchMode=yes … -- <target> <remote_cmd>`. The user's `~/.ssh/config` applies.
#[derive(Debug, Clone)]
pub struct SshTransport {
    pub ssh: OsString,
    pub target: String,
}

impl SshTransport {
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            ssh: "ssh".into(),
            target: target.into(),
        }
    }
}

impl Transport for SshTransport {
    fn command(&self, remote_cmd: &str) -> CommandSpec {
        let mut args: Vec<OsString> = [
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ConnectTimeout=15",
            "--",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.push(self.target.clone().into());
        args.push(remote_cmd.into());
        CommandSpec {
            program: self.ssh.clone(),
            args,
            env: Vec::new(),
        }
    }

    fn describe(&self) -> String {
        format!("ssh {}", self.target)
    }
}

/// `sh -c <remote_cmd>` on this machine, with extra environment (a temp `HOME`). Tests and CI.
#[derive(Debug, Clone, Default)]
pub struct LocalShellTransport {
    pub env: Vec<(OsString, OsString)>,
}

impl Transport for LocalShellTransport {
    fn command(&self, remote_cmd: &str) -> CommandSpec {
        CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), remote_cmd.into()],
            env: self.env.clone(),
        }
    }

    fn describe(&self) -> String {
        "local sh".into()
    }
}

/// POSIX single-quote escaping: the result is one shell word that expands to `s`.
pub fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Run `script` under `sh` whatever the remote login shell is (fish, csh…).
pub fn sh_wrap(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

/// The remote command that bridges to the Daemon. The override is the user's own shell text
/// (`~/bin/xd`, a path with variables…), so it is not quoted.
pub fn connect_command(override_: Option<&str>, version: &str) -> String {
    match override_ {
        Some(cmd) => format!("{cmd} connect"),
        None => format!("sh -c 'exec \"$HOME/.xshell/server/{version}/xshelld\" connect'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_args_exact() {
        let c = SshTransport::new("dev").command("C");
        assert_eq!(c.program, OsString::from("ssh"));
        let args: Vec<&str> = c.args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=15",
                "--",
                "dev",
                "C"
            ]
        );
        assert!(c.env.is_empty());
    }

    #[test]
    fn connect_command_default_and_override() {
        assert_eq!(
            connect_command(None, "1.5.0"),
            r#"sh -c 'exec "$HOME/.xshell/server/1.5.0/xshelld" connect'"#
        );
        assert_eq!(
            connect_command(Some("~/bin/xd"), "1.5.0"),
            "~/bin/xd connect"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sh_quote_roundtrip() {
        for s in ["it's", "a b", "$HOME", "\"q\"", "", "'"] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(s)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(sh_wrap("printf '%s' \"it's\""))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "it's");
    }
}

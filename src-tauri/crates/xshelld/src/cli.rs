//! Command line: `xshelld serve | connect [--home DIR] [--socket PATH]`, `xshelld --version`.
//! Each flag falls back to an environment variable. Hand-parsed: three commands, three flags.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve(Opts),
    Connect(Opts),
    Version,
    Help,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Opts {
    /// `--home` / `XSHELLD_HOME`: the home whose files the Daemon serves.
    pub home: Option<PathBuf>,
    /// `--socket` / `XSHELLD_SOCKET`.
    pub socket: Option<PathBuf>,
    /// `--idle-timeout-ms` / `XSHELLD_IDLE_TIMEOUT_MS`. Test-only, hidden from `--help`.
    pub idle_timeout: Option<Duration>,
}

pub const USAGE: &str = "\
usage: xshelld <command> [options]

commands:
  connect     bridge stdin/stdout to the Daemon, starting it if needed
  serve       run the Daemon in the foreground
  --version   print name, version and protocol range as one line of JSON

options:
  --home DIR      home directory to serve (env XSHELLD_HOME)
  --socket PATH   Daemon socket (env XSHELLD_SOCKET)
";

/// The `--version` line. The Desktop parses it, so keep exactly these keys. `os` and `arch`
/// use `uname -s` / `uname -m` spelling, so a Desktop that can only run `<cmd> --version`
/// (a Daemon command behind a restricted ssh) still learns the platform.
pub fn version_json() -> String {
    // Written by hand to keep this exact key order (serde_json sorts object keys).
    format!(
        r#"{{"name":"xshelld","version":{},"protocol":{{"min":{},"max":{}}},"os":{},"arch":{}}}"#,
        serde_json::Value::from(env!("CARGO_PKG_VERSION")),
        xshell_protocol::PROTOCOL_MIN,
        xshell_protocol::PROTOCOL_MAX,
        serde_json::Value::from(uname_os()),
        serde_json::Value::from(std::env::consts::ARCH),
    )
}

fn uname_os() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        other => other,
    }
}

fn parse_ms(v: &OsString, what: &str) -> Result<Duration, String> {
    v.to_str()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .ok_or_else(|| format!("{what}: expected milliseconds, got {v:?}"))
}

/// Parse the arguments after the program name. `env` looks up environment variables.
pub fn parse(
    args: impl IntoIterator<Item = OsString>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Command, String> {
    let mut args = args.into_iter();
    let cmd = match args.next() {
        None => return Ok(Command::Help),
        Some(c) => c,
    };
    let cmd = cmd.to_str().unwrap_or("");
    match cmd {
        "--version" | "-V" | "version" => return Ok(Command::Version),
        "--help" | "-h" | "help" => return Ok(Command::Help),
        "serve" | "connect" => {}
        other => return Err(format!("unknown command: {other}")),
    }
    let mut opts = Opts::default();
    while let Some(a) = args.next() {
        let s = a.to_string_lossy().into_owned();
        let (flag, inline) = match s.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(OsString::from(v))),
            _ => (s.clone(), None),
        };
        let mut value = || -> Result<OsString, String> {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--home" => opts.home = Some(value()?.into()),
            "--socket" => opts.socket = Some(value()?.into()),
            "--idle-timeout-ms" => opts.idle_timeout = Some(parse_ms(&value()?, &flag)?),
            _ => return Err(format!("unknown option: {s}")),
        }
    }
    if opts.home.is_none() {
        opts.home = env("XSHELLD_HOME").map(PathBuf::from);
    }
    if opts.socket.is_none() {
        opts.socket = env("XSHELLD_SOCKET").map(PathBuf::from);
    }
    if opts.idle_timeout.is_none() {
        if let Some(v) = env("XSHELLD_IDLE_TIMEOUT_MS") {
            opts.idle_timeout = Some(parse_ms(&v, "XSHELLD_IDLE_TIMEOUT_MS")?);
        }
    }
    Ok(if cmd == "serve" {
        Command::Serve(opts)
    } else {
        Command::Connect(opts)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn run(args: &[&str], env: &[(&str, &str)]) -> Result<Command, String> {
        let env: HashMap<String, OsString> = env
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        parse(args.iter().map(OsString::from), &|k| env.get(k).cloned())
    }

    #[test]
    fn parses_serve_flags_and_env() {
        let env = [
            ("XSHELLD_HOME", "/env/home"),
            ("XSHELLD_SOCKET", "/env/sock"),
            ("XSHELLD_IDLE_TIMEOUT_MS", "250"),
        ];
        assert_eq!(
            run(&["serve"], &env),
            Ok(Command::Serve(Opts {
                home: Some("/env/home".into()),
                socket: Some("/env/sock".into()),
                idle_timeout: Some(Duration::from_millis(250)),
            }))
        );
        assert_eq!(
            run(
                &[
                    "connect",
                    "--home",
                    "/h",
                    "--socket=/s",
                    "--idle-timeout-ms",
                    "5"
                ],
                &env
            ),
            Ok(Command::Connect(Opts {
                home: Some("/h".into()),
                socket: Some("/s".into()),
                idle_timeout: Some(Duration::from_millis(5)),
            }))
        );
        assert!(run(&["serve", "--bogus"], &[]).is_err());
        assert!(run(&["serve", "--home"], &[]).is_err());
        assert!(run(&["serve"], &[("XSHELLD_IDLE_TIMEOUT_MS", "x")]).is_err());
        assert!(run(&["frobnicate"], &[]).is_err());
        assert_eq!(run(&["--version"], &[]), Ok(Command::Version));
        assert_eq!(run(&[], &[]), Ok(Command::Help));
    }

    #[test]
    fn version_json_shape() {
        assert_eq!(
            version_json(),
            format!(
                r#"{{"name":"xshelld","version":"{}","protocol":{{"min":1,"max":1}},"os":"{}","arch":"{}"}}"#,
                env!("CARGO_PKG_VERSION"),
                uname_os(),
                std::env::consts::ARCH
            )
        );
        let v: serde_json::Value = serde_json::from_str(&version_json()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"name":"xshelld","version":env!("CARGO_PKG_VERSION"),
                "protocol":{"min":1,"max":1},"os":uname_os(),"arch":std::env::consts::ARCH})
        );
    }
}

//! Managed install: probe the Host (`uname` and the installed `xshelld --version`), and when
//! the Desktop's version is missing, upload the matching binary over the same transport. The
//! Host needs no internet. Probes are marker-based, so login-shell noise cannot confuse them.

use crate::cancel::CancelToken;
use crate::errors::{classify_ssh_failure, HostErrorHint};
use crate::process::{run_script, ProcError};
use crate::transport::{sh_wrap, Transport};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use xshell_core::protocol::msg::ProtocolRange;
use xshell_core::protocol::negotiate::negotiate;

pub const MARKER: &str = "@@XSHELL@@";
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonVersion {
    pub version: String,
    pub protocol: ProtocolRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub os: String,
    pub arch: String,
    pub installed: Option<DaemonVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub os: String,
    pub arch: String,
    pub triple: String,
    /// Whether this call uploaded the binary.
    pub uploaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallError {
    pub hint: Option<HostErrorHint>,
    pub message: String,
    pub cancelled: bool,
}

impl InstallError {
    fn new(hint: Option<HostErrorHint>, message: impl Into<String>) -> Self {
        Self {
            hint,
            message: message.into(),
            cancelled: false,
        }
    }
    fn cancelled() -> Self {
        Self {
            hint: None,
            message: "cancelled".into(),
            cancelled: true,
        }
    }
}

/// Versions go into shell scripts, so only a safe alphabet is accepted.
pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
}

fn server_dir(version: &str) -> String {
    format!("\"$HOME/.xshell/server/{version}\"")
}

/// `uname -sm`, the marker, then the installed binary's `--version` line (if any).
pub fn probe_script(version: &str) -> String {
    format!(
        "uname -sm; echo {MARKER}; f={}/xshelld; [ -x \"$f\" ] && \"$f\" --version 2>/dev/null; exit 0",
        server_dir(version)
    )
}

/// The exact remote command that probes a Daemon command override. Hosts that need an
/// override often restrict ssh to fixed commands (ForceCommand), so this is sent verbatim:
/// no shell wrapper, no `uname`. The platform comes from the `--version` JSON instead.
pub fn override_version_command(cmd: &str) -> String {
    format!("{cmd} --version")
}

/// What `<cmd> --version` told us about a Daemon command override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideProbe {
    pub installed: DaemonVersion,
    /// `uname -s` / `uname -m` spelling; absent from Daemons older than this field.
    pub os: Option<String>,
    pub arch: Option<String>,
}

pub fn parse_override_probe(stdout: &str) -> Option<OverrideProbe> {
    stdout.lines().find_map(|l| {
        let installed = parse_version_json(l)?;
        let v: Value = serde_json::from_str(l.trim()).ok()?;
        let field = |k: &str| v[k].as_str().map(String::from);
        Some(OverrideProbe {
            installed,
            os: field("os"),
            arch: field("arch"),
        })
    })
}

/// Run `<cmd> --version` on the Host exactly as written and parse its JSON line.
pub fn probe_override(
    t: &dyn Transport,
    cmd: &str,
    cancel: &CancelToken,
) -> Result<OverrideProbe, InstallError> {
    let out = run_script(
        t,
        &override_version_command(cmd),
        None,
        PROBE_TIMEOUT,
        cancel,
    )
    .map_err(|e| proc_failure(e, "the host probe"))?;
    parse_override_probe(&out.stdout).ok_or_else(|| {
        let msg = if !out.stderr.trim().is_empty() {
            out.stderr.trim().to_string()
        } else {
            format!(
                "`{}` printed no xshelld version (exit {:?})",
                override_version_command(cmd),
                out.code
            )
        };
        InstallError::new(
            classify_ssh_failure(&out.stderr, None, out.code)
                .or(Some(HostErrorHint::DaemonCommandFailed)),
            msg,
        )
    })
}

/// Write stdin to a temp file and move it into place, then print its `--version`.
pub fn upload_script(version: &str) -> String {
    format!(
        "set -e; umask 077; d={}; mkdir -p \"$d\"; t=\"$d/.xshelld.$$.tmp\"; \
         trap 'rm -f \"$t\"' EXIT; cat > \"$t\"; chmod 700 \"$t\"; mv -f \"$t\" \"$d/xshelld\"; \
         \"$d/xshelld\" --version",
        server_dir(version)
    )
}

/// SIGTERM the running Daemon named by its pidfile, if the pid really is an `xshelld`.
pub fn kill_daemon_script() -> String {
    r#"for d in "${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/xshell}" "$HOME/.xshell/run"; do
  [ -n "$d" ] && [ -f "$d/daemon.pid" ] || continue
  pid=$(cat "$d/daemon.pid" 2>/dev/null) || continue
  case "$pid" in ''|*[!0-9]*) continue;; esac
  if ps -p "$pid" -o comm= 2>/dev/null | grep -q xshelld; then kill -TERM "$pid"; fi
done
exit 0"#
        .to_string()
}

pub fn parse_version_json(line: &str) -> Option<DaemonVersion> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v["name"] != "xshelld" {
        return None;
    }
    Some(DaemonVersion {
        version: v["version"].as_str()?.to_string(),
        protocol: serde_json::from_value(v["protocol"].clone()).ok()?,
    })
}

pub fn parse_probe(stdout: &str) -> Result<Probe, String> {
    let lines: Vec<&str> = stdout.lines().map(|l| l.trim_end_matches('\r')).collect();
    let m = lines
        .iter()
        .position(|l| l.trim() == MARKER)
        .ok_or_else(|| "the host did not answer the probe".to_string())?;
    let uname = lines[..m]
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| "the host printed no uname output".to_string())?;
    let mut it = uname.split_whitespace();
    let (Some(os), Some(arch)) = (it.next(), it.next()) else {
        return Err(format!("unexpected uname output: {uname}"));
    };
    let installed = lines[m + 1..].iter().find_map(|l| parse_version_json(l));
    Ok(Probe {
        os: os.to_string(),
        arch: arch.to_string(),
        installed,
    })
}

pub fn target_triple(os: &str, arch: &str) -> Result<&'static str, String> {
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Ok("x86_64-unknown-linux-musl"),
        ("Linux", "aarch64" | "arm64") => Ok("aarch64-unknown-linux-musl"),
        ("Darwin", "arm64" | "aarch64") => Ok("aarch64-apple-darwin"),
        ("Darwin", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => Err(format!("unsupported host platform: {os} {arch}")),
    }
}

/// `linux` / `macos` for the status, from `uname -s`.
pub fn os_name(uname_s: &str) -> Option<&'static str> {
    match uname_s {
        "Linux" => Some("linux"),
        "Darwin" => Some("macos"),
        _ => None,
    }
}

pub fn needs_upload(p: &Probe, desktop_version: &str, ours: ProtocolRange) -> bool {
    match &p.installed {
        None => true,
        Some(d) => d.version != desktop_version || negotiate(ours, d.protocol).is_err(),
    }
}

pub trait BinarySource: Send + Sync {
    /// The `xshelld` binary for `triple` at `version`. Long fetches watch `cancel`.
    fn fetch(&self, triple: &str, version: &str, cancel: &CancelToken) -> Result<Vec<u8>, String>;
}

/// `<dir>/xshelld-<triple>`: binaries shipped next to the Desktop executable.
pub struct DirSource(pub PathBuf);

impl BinarySource for DirSource {
    fn fetch(&self, triple: &str, _v: &str, _c: &CancelToken) -> Result<Vec<u8>, String> {
        let p = self.0.join(format!("xshelld-{triple}"));
        std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))
    }
}

/// One file for every triple (tests).
pub struct FileSource(pub PathBuf);

impl BinarySource for FileSource {
    fn fetch(&self, _t: &str, _v: &str, _c: &CancelToken) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| format!("{}: {e}", self.0.display()))
    }
}

/// The first source that has the binary wins; otherwise every error, joined.
pub struct ChainSource(pub Vec<Box<dyn BinarySource>>);

impl BinarySource for ChainSource {
    fn fetch(&self, triple: &str, version: &str, cancel: &CancelToken) -> Result<Vec<u8>, String> {
        let mut errs = Vec::new();
        for s in &self.0 {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            match s.fetch(triple, version, cancel) {
                Ok(b) => return Ok(b),
                Err(e) => errs.push(e),
            }
        }
        Err(if errs.is_empty() {
            "no binary source configured".into()
        } else {
            errs.join("; ")
        })
    }
}

fn proc_failure(e: ProcError, what: &str) -> InstallError {
    match e {
        ProcError::Cancelled => InstallError::cancelled(),
        ProcError::Timeout => InstallError::new(
            Some(HostErrorHint::Unreachable),
            format!("{what} timed out"),
        ),
        ProcError::Spawn(e) => InstallError::new(
            classify_ssh_failure("", Some(&e), None),
            format!("cannot run the transport: {e}"),
        ),
    }
}

/// Run a marker probe; a missing marker means the transport itself failed.
pub fn run_probe(
    t: &dyn Transport,
    script: &str,
    cancel: &CancelToken,
) -> Result<Probe, InstallError> {
    let out = run_script(t, &sh_wrap(script), None, PROBE_TIMEOUT, cancel)
        .map_err(|e| proc_failure(e, "the host probe"))?;
    match parse_probe(&out.stdout) {
        Ok(p) => Ok(p),
        Err(e) => {
            let msg = if out.stderr.is_empty() {
                match out.code {
                    Some(c) => format!("{e} (exit {c})"),
                    None => e,
                }
            } else {
                out.stderr.clone()
            };
            Err(InstallError::new(
                classify_ssh_failure(&out.stderr, None, out.code),
                msg,
            ))
        }
    }
}

/// Make sure `~/.xshell/server/<version>/xshelld` is this Desktop's build. `on_upload` runs
/// just before an upload starts (the status moves to "installing").
pub fn ensure_installed(
    t: &dyn Transport,
    src: &dyn BinarySource,
    version: &str,
    ours: ProtocolRange,
    cancel: &CancelToken,
    on_upload: &dyn Fn(),
) -> Result<Installed, InstallError> {
    if !valid_version(version) {
        return Err(InstallError::new(
            None,
            format!("invalid version string {version:?}"),
        ));
    }
    let p = run_probe(t, &probe_script(version), cancel)?;
    let triple = target_triple(&p.os, &p.arch)
        .map_err(|e| InstallError::new(Some(HostErrorHint::UnsupportedPlatform), e))?;
    let mut done = Installed {
        os: p.os.clone(),
        arch: p.arch.clone(),
        triple: triple.to_string(),
        uploaded: false,
    };
    if !needs_upload(&p, version, ours) {
        return Ok(done);
    }
    let bin = src.fetch(triple, version, cancel).map_err(|e| {
        if cancel.is_cancelled() {
            InstallError::cancelled()
        } else {
            InstallError::new(
                Some(HostErrorHint::BinaryUnavailable),
                format!("no xshelld {version} for {triple}: {e}"),
            )
        }
    })?;
    if cancel.is_cancelled() {
        return Err(InstallError::cancelled());
    }
    on_upload();
    let out = run_script(
        t,
        &sh_wrap(&upload_script(version)),
        Some(bin),
        UPLOAD_TIMEOUT,
        cancel,
    )
    .map_err(|e| proc_failure(e, "the upload"))?;
    let reported = out.stdout.lines().rev().find_map(parse_version_json);
    match reported {
        Some(d) if d.version == version => {
            done.uploaded = true;
            Ok(done)
        }
        Some(d) => Err(InstallError::new(
            None,
            format!("installed xshelld reports version {}", d.version),
        )),
        None => Err(InstallError::new(
            classify_ssh_failure(&out.stderr, None, out.code),
            if out.stderr.is_empty() {
                format!("the upload failed (exit {:?})", out.code)
            } else {
                format!("the upload failed: {}", out.stderr)
            },
        )),
    }
}

#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) mod testutil {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Counts fetches, then delegates.
    pub struct Counting<S>(pub S, pub Arc<AtomicUsize>);

    impl<S: BinarySource> BinarySource for Counting<S> {
        fn fetch(&self, t: &str, v: &str, c: &CancelToken) -> Result<Vec<u8>, String> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.fetch(t, v, c)
        }
    }

    pub struct Fails(pub &'static str);

    impl BinarySource for Fails {
        fn fetch(&self, _: &str, _: &str, _: &CancelToken) -> Result<Vec<u8>, String> {
            Err(self.0.into())
        }
    }

    /// A fake `xshelld` that only answers `--version`.
    pub fn fake_daemon(version: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && echo '{{\"name\":\"xshelld\",\"version\":\"{version}\",\"protocol\":{{\"min\":1,\"max\":1}}}}'\n"
        )
        .into_bytes()
    }

    pub struct Bytes(pub Vec<u8>);

    impl BinarySource for Bytes {
        fn fetch(&self, _: &str, _: &str, _: &CancelToken) -> Result<Vec<u8>, String> {
            Ok(self.0.clone())
        }
    }
}

#[cfg(test)]
#[cfg_attr(not(unix), allow(unused_imports))]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const R11: ProtocolRange = ProtocolRange { min: 1, max: 1 };
    const JSON: &str = r#"{"name":"xshelld","version":"1.5.0","protocol":{"min":1,"max":1}}"#;

    #[test]
    fn parse_probe_installed() {
        let p = parse_probe(&format!("motd\nLinux x86_64\n{MARKER}\n{JSON}\n")).unwrap();
        assert_eq!(p.os, "Linux");
        assert_eq!(p.arch, "x86_64");
        assert_eq!(
            p.installed,
            Some(DaemonVersion {
                version: "1.5.0".into(),
                protocol: R11
            })
        );
    }

    #[test]
    fn parse_probe_missing_or_garbage() {
        let p = parse_probe(&format!("Darwin arm64\r\n{MARKER}\r\nsegfault\n")).unwrap();
        assert_eq!((p.os.as_str(), p.installed), ("Darwin", None));
        assert!(parse_probe("Linux x86_64\n").is_err());
        assert!(parse_probe(&format!("{MARKER}\n")).is_err());
    }

    #[test]
    fn target_triple_map() {
        assert_eq!(
            target_triple("Linux", "x86_64"),
            Ok("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            target_triple("Linux", "amd64"),
            Ok("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            target_triple("Linux", "aarch64"),
            Ok("aarch64-unknown-linux-musl")
        );
        assert_eq!(
            target_triple("Linux", "arm64"),
            Ok("aarch64-unknown-linux-musl")
        );
        assert_eq!(target_triple("Darwin", "arm64"), Ok("aarch64-apple-darwin"));
        assert_eq!(target_triple("Darwin", "x86_64"), Ok("x86_64-apple-darwin"));
        assert!(target_triple("FreeBSD", "amd64")
            .unwrap_err()
            .contains("unsupported"));
    }

    #[test]
    fn needs_upload_cases() {
        let p = |v: &str, min, max| Probe {
            os: "Linux".into(),
            arch: "x86_64".into(),
            installed: Some(DaemonVersion {
                version: v.into(),
                protocol: ProtocolRange { min, max },
            }),
        };
        assert!(!needs_upload(&p("1.5.0", 1, 1), "1.5.0", R11));
        assert!(needs_upload(&p("1.4.0", 1, 1), "1.5.0", R11));
        assert!(needs_upload(&p("1.5.0", 2, 2), "1.5.0", R11));
        let mut none = p("1.5.0", 1, 1);
        none.installed = None;
        assert!(needs_upload(&none, "1.5.0", R11));
    }

    #[test]
    fn version_alphabet() {
        assert!(valid_version("1.5.0-beta.1+g12"));
        assert!(!valid_version("1.5.0;rm"));
        assert!(!valid_version(""));
        assert!(!valid_version("1 2"));
    }

    #[test]
    fn source_error_is_binary_unavailable() {
        // A probe that succeeds without a transport: answer the probe from a local shell.
        #[cfg(unix)]
        {
            let home = tempfile::tempdir().unwrap();
            let t = local(home.path(), None);
            let e = ensure_installed(&t, &Fails("404"), "1.5.0", R11, &CancelToken::new(), &|| {})
                .unwrap_err();
            assert_eq!(e.hint, Some(HostErrorHint::BinaryUnavailable));
            assert!(e.message.contains("404"), "{}", e.message);
        }
    }

    #[test]
    fn chain_source_order() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("xshelld-x86_64-unknown-linux-musl"), b"B").unwrap();
        std::fs::write(a.path().join("xshelld-aarch64-apple-darwin"), b"A").unwrap();
        let chain = ChainSource(vec![
            Box::new(DirSource(a.path().into())),
            Box::new(DirSource(b.path().into())),
        ]);
        let c = CancelToken::new();
        assert_eq!(
            chain.fetch("x86_64-unknown-linux-musl", "1", &c).unwrap(),
            b"B"
        );
        assert_eq!(chain.fetch("aarch64-apple-darwin", "1", &c).unwrap(), b"A");
        let e = chain.fetch("x86_64-apple-darwin", "1", &c).unwrap_err();
        assert_eq!(e.matches("xshelld-x86_64-apple-darwin").count(), 2, "{e}");
        assert!(e.contains("; "), "{e}");
    }

    #[cfg(unix)]
    fn local(
        home: &std::path::Path,
        path_prefix: Option<&std::path::Path>,
    ) -> crate::transport::LocalShellTransport {
        let mut env = vec![("HOME".into(), home.as_os_str().to_owned())];
        if let Some(p) = path_prefix {
            let path = format!(
                "{}:{}",
                p.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            env.push(("PATH".into(), path.into()));
        }
        crate::transport::LocalShellTransport { env }
    }

    #[cfg(unix)]
    #[test]
    fn ensure_installed_into_temp_home() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let t = local(home.path(), None);
        let n = Arc::new(AtomicUsize::new(0));
        let src = Counting(Bytes(fake_daemon("1.5.0")), n.clone());
        let uploads = AtomicUsize::new(0);
        let on_upload = || {
            uploads.fetch_add(1, Ordering::SeqCst);
        };
        let c = CancelToken::new();
        let i = ensure_installed(&t, &src, "1.5.0", R11, &c, &on_upload).unwrap();
        assert!(i.uploaded);
        assert_eq!(uploads.load(Ordering::SeqCst), 1);
        let dir = home.path().join(".xshell/server/1.5.0");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("xshelld")), 0o700);
        assert_eq!(mode(&dir), 0o700);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".xshelld."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let again = ensure_installed(&t, &src, "1.5.0", R11, &c, &on_upload).unwrap();
        assert!(!again.uploaded);
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert_eq!(uploads.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_wrong_reported_version() {
        let home = tempfile::tempdir().unwrap();
        let t = local(home.path(), None);
        let e = ensure_installed(
            &t,
            &Bytes(fake_daemon("9.9.9")),
            "1.5.0",
            R11,
            &CancelToken::new(),
            &|| {},
        )
        .unwrap_err();
        assert!(e.message.contains("reports version 9.9.9"), "{}", e.message);
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_platform_hint() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let shim = home.path().join("shim");
        std::fs::create_dir(&shim).unwrap();
        let uname = shim.join("uname");
        std::fs::write(&uname, "#!/bin/sh\necho FreeBSD amd64\n").unwrap();
        std::fs::set_permissions(&uname, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = local(home.path(), Some(&shim));
        let e = ensure_installed(
            &t,
            &Bytes(fake_daemon("1.5.0")),
            "1.5.0",
            R11,
            &CancelToken::new(),
            &|| {},
        )
        .unwrap_err();
        assert_eq!(e.hint, Some(HostErrorHint::UnsupportedPlatform));
    }

    #[cfg(unix)]
    #[test]
    fn transport_failure_carries_stderr_hint() {
        // Not the probe at all: the "transport" fails like ssh would.
        struct Broken;
        impl Transport for Broken {
            fn command(&self, _: &str) -> crate::transport::CommandSpec {
                crate::transport::LocalShellTransport::default()
                    .command("echo 'Permission denied (publickey).' >&2; exit 255")
            }
            fn describe(&self) -> String {
                "broken".into()
            }
        }
        let e = run_probe(&Broken, "true", &CancelToken::new()).unwrap_err();
        assert_eq!(e.hint, Some(HostErrorHint::PermissionDenied));
        assert!(e.message.contains("Permission denied"), "{}", e.message);
    }
}

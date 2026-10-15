//! Managed install: probe the Host (`uname` and the installed `xshelld --version`), and when
//! the Desktop's build is missing, upload the matching binary over the same transport. The
//! Host needs no internet. Probes are marker-based, so login-shell noise cannot confuse them.
//!
//! Each build has its own directory, `~/.xshell/server/<version>+<build>` (plain `<version>`
//! when the Desktop's build is unknown; `xshell_protocol::build::install_dir_name`). Every
//! binary is checked against the build this Desktop expects: its embedded stamp before the
//! upload, its `--version` after it.

use crate::cancel::CancelToken;
use crate::errors::{classify_ssh_failure, HostErrorHint};
use crate::process::{run_script, ProcError};
use crate::transport::{sh_quote, sh_wrap, Transport};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use xshell_protocol::build::{install_dir_name, scan_stamp, BuildId};
use xshell_protocol::msg::ProtocolRange;
use xshell_protocol::negotiate::negotiate;

pub const MARKER: &str = "@@XSHELL@@";
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonVersion {
    pub version: String,
    pub protocol: ProtocolRange,
    /// The `build` key of `--version` (#49); `None` when null, missing (older Daemons) or
    /// malformed.
    pub build: Option<BuildId>,
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

/// Versions go into shell scripts and paths, so only a safe alphabet is accepted, and never
/// `.` or `..`.
pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v != "."
        && v != ".."
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
}

/// How a build shows in messages: its display form, or `unknown build`.
pub fn build_text(b: Option<&BuildId>) -> String {
    b.map_or_else(|| "unknown build".into(), BuildId::display)
}

/// `dir` is an install directory name ([`valid_version`] checked by the callers).
fn server_dir(dir: &str) -> String {
    format!("\"$HOME/.xshell/server/{dir}\"")
}

/// `uname -sm`, the marker, then the `--version` line (if any) of the binary installed in
/// directory `dir`.
pub fn probe_script(dir: &str) -> String {
    format!(
        "uname -sm; echo {MARKER}; f={}/xshelld; [ -x \"$f\" ] && \"$f\" --version 2>/dev/null; exit 0",
        server_dir(dir)
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

/// Write stdin to a temp file and move it into directory `dir`, then print its `--version`.
pub fn upload_script(dir: &str) -> String {
    format!(
        "set -e; umask 077; d={}; mkdir -p \"$d\"; t=\"$d/.xshelld.$$.tmp\"; \
         trap 'rm -f \"$t\"' EXIT; cat > \"$t\"; chmod 700 \"$t\"; mv -f \"$t\" \"$d/xshelld\"; \
         \"$d/xshelld\" --version",
        server_dir(dir)
    )
}

/// The refusal when the build to select is not installed.
pub const BUILD_NOT_INSTALLED: &str = "this Desktop’s xshelld is not installed on the host";

/// Record directory `dir` as the Host's `selected` build (`xshell_core::server`): a temp
/// file renamed over `~/.xshell/server/selected`, only if `dir` holds an executable
/// `xshelld`. `dir` passed [`valid_version`]; it is quoted anyway.
pub fn select_script(dir: &str) -> String {
    let q = sh_quote(dir);
    format!(
        "set -e; umask 077; r=\"$HOME/.xshell/server\"; d=\"$r/\"{q}; \
         [ -x \"$d/xshelld\" ] || {{ echo \"{BUILD_NOT_INSTALLED}\" >&2; exit 1; }}; \
         t=\"$r/.selected.$$.tmp\"; trap 'rm -f \"$t\"' EXIT; printf '%s\\n' {q} > \"$t\"; \
         mv -f \"$t\" \"$r/selected\""
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
        build: serde_json::from_value(v["build"].clone()).ok().flatten(),
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

/// Whether the binary in the probed directory must be (re)uploaded: missing, another
/// version, no common protocol, or (when this Desktop's build is known) another build.
pub fn needs_upload(
    p: &Probe,
    desktop_version: &str,
    build: Option<&BuildId>,
    ours: ProtocolRange,
) -> bool {
    match &p.installed {
        None => true,
        Some(d) => {
            d.version != desktop_version
                || negotiate(ours, d.protocol).is_err()
                || build.is_some_and(|b| !crate::version::same_build(d.build.as_ref(), b))
        }
    }
}

/// What a [`BinarySource`] is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub triple: String,
    pub version: String,
    /// The build this Desktop expects; `None` when its own is unknown (no check then).
    pub build: Option<BuildId>,
}

/// Where a fetched binary came from, for the message when its build is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Shipped with (or copied next to) the Desktop, or a test file.
    Bundled(PathBuf),
    /// A daemon built in this workspace (debug Desktops); `target` when cross-built.
    DevBuild {
        path: PathBuf,
        target: Option<String>,
    },
    /// A release download (its URL).
    Release(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub origin: Origin,
}

pub trait BinarySource: Send + Sync {
    /// The `xshelld` binary for `want`. Long fetches watch `cancel`. The build is checked by
    /// the caller ([`ensure_installed`]), so a source never falls through on it.
    fn fetch(&self, want: &Wanted, cancel: &CancelToken) -> Result<Fetched, String>;
}

/// `<dir>/xshelld-<triple>`: binaries shipped next to the Desktop executable.
pub struct DirSource(pub PathBuf);

impl BinarySource for DirSource {
    fn fetch(&self, want: &Wanted, _c: &CancelToken) -> Result<Fetched, String> {
        let p = self.0.join(format!("xshelld-{}", want.triple));
        let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(Fetched {
            bytes,
            origin: Origin::Bundled(p),
        })
    }
}

/// One file for every triple (tests).
pub struct FileSource(pub PathBuf);

impl BinarySource for FileSource {
    fn fetch(&self, _w: &Wanted, _c: &CancelToken) -> Result<Fetched, String> {
        let bytes = std::fs::read(&self.0).map_err(|e| format!("{}: {e}", self.0.display()))?;
        Ok(Fetched {
            bytes,
            origin: Origin::Bundled(self.0.clone()),
        })
    }
}

/// The first source that has the binary wins; otherwise every error, joined.
pub struct ChainSource(pub Vec<Box<dyn BinarySource>>);

impl BinarySource for ChainSource {
    fn fetch(&self, want: &Wanted, cancel: &CancelToken) -> Result<Fetched, String> {
        let mut errs = Vec::new();
        for s in &self.0 {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            match s.fetch(want, cancel) {
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

/// The refusal for a binary whose stamp names another build than `expected`.
pub fn stamp_mismatch(
    origin: &Origin,
    triple: &str,
    version: &str,
    found: &str,
    expected: &str,
) -> String {
    match origin {
        Origin::Release(_) => format!(
            "the v{version} release’s xshelld reports {found}; this Desktop expects {expected}"
        ),
        Origin::Bundled(path) => stale_bundled(path, found, expected, Some(triple)),
        Origin::DevBuild { path, target } => {
            stale_bundled(path, found, expected, target.as_deref())
        }
    }
}

fn stale_bundled(
    path: &std::path::Path,
    found: &str,
    expected: &str,
    target: Option<&str>,
) -> String {
    let target_arg = target.map(|t| format!(" --target {t}")).unwrap_or_default();
    format!(
        "the bundled xshelld at {} reports {found}; this Desktop expects {expected}; rebuild it: cargo build -p xshelld --profile release-daemon{target_arg}",
        path.display()
    )
}

/// Check the build stamped into `f` against `want.build` (when known), without running it:
/// it may be for another platform.
pub fn check_fetched(f: &Fetched, want: &Wanted) -> Result<(), String> {
    let Some(expected) = &want.build else {
        return Ok(());
    };
    let found = scan_stamp(&f.bytes).ok().flatten();
    if found.as_ref().is_some_and(|b| b.same(expected)) {
        return Ok(());
    }
    Err(stamp_mismatch(
        &f.origin,
        &want.triple,
        &want.version,
        &build_text(found.as_ref()),
        &expected.display(),
    ))
}

/// Make sure `~/.xshell/server/<dir>/xshelld` is this Desktop's build, where `<dir>` is
/// `install_dir_name(version, build)`. `on_upload` runs just before an upload starts (the
/// status moves to "installing"). Never touches the `selected` record.
pub fn ensure_installed(
    t: &dyn Transport,
    src: &dyn BinarySource,
    version: &str,
    build: Option<&BuildId>,
    ours: ProtocolRange,
    cancel: &CancelToken,
    on_upload: &dyn Fn(),
) -> Result<Installed, InstallError> {
    let dir = install_dir_name(version, build);
    if !valid_version(&dir) {
        return Err(InstallError::new(
            None,
            format!("invalid version string {dir:?}"),
        ));
    }
    let p = run_probe(t, &probe_script(&dir), cancel)?;
    let triple = target_triple(&p.os, &p.arch)
        .map_err(|e| InstallError::new(Some(HostErrorHint::UnsupportedPlatform), e))?;
    let mut done = Installed {
        os: p.os.clone(),
        arch: p.arch.clone(),
        triple: triple.to_string(),
        uploaded: false,
    };
    if !needs_upload(&p, version, build, ours) {
        return Ok(done);
    }
    let want = Wanted {
        triple: triple.to_string(),
        version: version.to_string(),
        build: build.cloned(),
    };
    let fetched = src.fetch(&want, cancel).map_err(|e| {
        if cancel.is_cancelled() {
            InstallError::cancelled()
        } else {
            InstallError::new(
                Some(HostErrorHint::BinaryUnavailable),
                format!("no xshelld {version} for {triple}: {e}"),
            )
        }
    })?;
    check_fetched(&fetched, &want)
        .map_err(|e| InstallError::new(Some(HostErrorHint::BinaryUnavailable), e))?;
    if cancel.is_cancelled() {
        return Err(InstallError::cancelled());
    }
    on_upload();
    let out = run_script(
        t,
        &sh_wrap(&upload_script(&dir)),
        Some(fetched.bytes),
        UPLOAD_TIMEOUT,
        cancel,
    )
    .map_err(|e| proc_failure(e, "the upload"))?;
    let reported = out.stdout.lines().rev().find_map(parse_version_json);
    match reported {
        Some(d) if d.version != version => Err(InstallError::new(
            None,
            format!("installed xshelld reports version {}", d.version),
        )),
        Some(d) if build.is_some_and(|b| !crate::version::same_build(d.build.as_ref(), b)) => {
            Err(InstallError::new(
                None,
                format!(
                    "installed xshelld reports {}; expected {}",
                    build_text(d.build.as_ref()),
                    build_text(build)
                ),
            ))
        }
        Some(_) => {
            done.uploaded = true;
            Ok(done)
        }
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

/// Record directory `dir` as the Host's `selected` build over the transport ([`select_script`]).
pub fn select_remote(
    t: &dyn Transport,
    dir: &str,
    cancel: &CancelToken,
) -> Result<(), InstallError> {
    if !valid_version(dir) {
        return Err(InstallError::new(
            None,
            format!("invalid version string {dir:?}"),
        ));
    }
    let out = run_script(
        t,
        &sh_wrap(&select_script(dir)),
        None,
        PROBE_TIMEOUT,
        cancel,
    )
    .map_err(|e| proc_failure(e, "selecting the build"))?;
    if out.code == Some(0) {
        return Ok(());
    }
    let stderr = out.stderr.trim();
    Err(InstallError::new(
        classify_ssh_failure(&out.stderr, None, out.code),
        if stderr.is_empty() {
            format!("selecting the build failed (exit {:?})", out.code)
        } else {
            stderr.to_string()
        },
    ))
}

/// `home/.xshell/server/<dir>/xshelld`: where a Daemon that outlives the app runs from, the
/// same place managed install puts it on a Remote Host. `dir` is an install directory name.
pub fn local_server_bin(home: &std::path::Path, dir: &str) -> PathBuf {
    xshell_core::server::server_root(home)
        .join(dir)
        .join("xshelld")
}

/// Copy the app's `xshelld` (`src`) to [`local_server_bin`] unless an identical private copy
/// is already there. Never touches the `selected` record. A Persistent Daemon must not run from inside the app: an AppImage's
/// mount ends with the app, and agent hooks call the Daemon's own path.
///
/// Every directory on the way is created 0700 and must be a real directory owned by this
/// user. The copy goes to a new, uniquely named file in the same directory and is renamed
/// over the old one, so a running Daemon keeps its binary and concurrent installs never see
/// a partial file.
#[cfg(unix)]
pub fn install_local(
    src: &std::path::Path,
    home: &std::path::Path,
    dir_name: &str,
) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    if !valid_version(dir_name) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("invalid version string {dir_name:?}"),
        ));
    }
    let dest = local_server_bin(home, dir_name);
    let uid = unsafe { libc::getuid() };
    let mut dir = home.to_path_buf();
    for part in [".xshell", "server", dir_name] {
        dir.push(part);
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let m = std::fs::symlink_metadata(&dir)?;
        if !m.file_type().is_dir() {
            return Err(Error::other(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        if m.uid() != uid {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!("{} is owned by uid {}, not by us", dir.display(), m.uid()),
            ));
        }
        if m.mode() & 0o077 != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let bytes = std::fs::read(src)?;
    if let Ok(m) = std::fs::symlink_metadata(&dest) {
        let same = m.file_type().is_file()
            && m.uid() == uid
            && m.mode() & 0o777 == 0o700
            && m.len() == bytes.len() as u64
            && std::fs::read(&dest).is_ok_and(|b| b == bytes);
        if same {
            return Ok(dest);
        }
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".xshelld.{}.{}.{nanos}.tmp",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        // The umask may have narrowed it.
        f.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &dest)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.map(|_| dest)
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
        fn fetch(&self, w: &Wanted, c: &CancelToken) -> Result<Fetched, String> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.fetch(w, c)
        }
    }

    pub struct Fails(pub &'static str);

    impl BinarySource for Fails {
        fn fetch(&self, _: &Wanted, _: &CancelToken) -> Result<Fetched, String> {
            Err(self.0.into())
        }
    }

    /// A fake `xshelld` that only answers `--version`.
    pub fn fake_daemon(version: &str) -> Vec<u8> {
        fake_daemon_build(version, None)
    }

    /// A fake `xshelld` of `build` that answers `--version` with it and carries its stamp
    /// (in a comment), as a real binary does.
    pub fn fake_daemon_build(version: &str, build: Option<&BuildId>) -> Vec<u8> {
        let (json, stamp) = match build {
            Some(b) => (
                format!(",\"build\":{}", serde_json::to_string(b).unwrap()),
                format!("# @@XSHELL-BUILD-V1:{}:{}@@\n", b.display(), b.time),
            ),
            None => (String::new(), String::new()),
        };
        format!(
            "#!/bin/sh\n{stamp}[ \"$1\" = --version ] && echo '{{\"name\":\"xshelld\",\"version\":\"{version}\",\"protocol\":{{\"min\":1,\"max\":1}}{json}}}'\n"
        )
        .into_bytes()
    }

    pub fn build(commit: &str) -> BuildId {
        BuildId {
            commit: commit.into(),
            dirty: None,
            time: 1_760_000_000,
        }
    }

    pub struct Bytes(pub Vec<u8>);

    impl BinarySource for Bytes {
        fn fetch(&self, _: &Wanted, _: &CancelToken) -> Result<Fetched, String> {
            Ok(Fetched {
                bytes: self.0.clone(),
                origin: Origin::Bundled("test-bytes".into()),
            })
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
                protocol: R11,
                build: None,
            })
        );
        // The trailing `build` key of a #49 Daemon.
        let with = r#"{"name":"xshelld","version":"1.5.0","protocol":{"min":1,"max":1},"os":"Linux","arch":"x86_64","build":{"commit":"85ee920a1b2c","dirty":null,"time":5}}"#;
        assert_eq!(
            parse_version_json(with).unwrap().build,
            Some(BuildId {
                time: 5,
                ..build("85ee920a1b2c")
            })
        );
        let null =
            r#"{"name":"xshelld","version":"1.5.0","protocol":{"min":1,"max":1},"build":null}"#;
        assert_eq!(parse_version_json(null).unwrap().build, None);
        let junk = r#"{"name":"xshelld","version":"1.5.0","protocol":{"min":1,"max":1},"build":7}"#;
        assert_eq!(parse_version_json(junk).unwrap().build, None);
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
                build: None,
            }),
        };
        assert!(!needs_upload(&p("1.5.0", 1, 1), "1.5.0", None, R11));
        assert!(needs_upload(&p("1.4.0", 1, 1), "1.5.0", None, R11));
        assert!(needs_upload(&p("1.5.0", 2, 2), "1.5.0", None, R11));
        let mut none = p("1.5.0", 1, 1);
        none.installed = None;
        assert!(needs_upload(&none, "1.5.0", None, R11));
        // With this Desktop's build known, the installed build must be it.
        let ours = build("85ee920a1b2c");
        let with = |b: Option<BuildId>| {
            let mut x = p("1.5.0", 1, 1);
            x.installed.as_mut().unwrap().build = b;
            x
        };
        assert!(!needs_upload(
            &with(Some(BuildId {
                time: 9,
                ..ours.clone()
            })),
            "1.5.0",
            Some(&ours),
            R11
        ));
        assert!(needs_upload(
            &with(Some(build("000000000000"))),
            "1.5.0",
            Some(&ours),
            R11
        ));
        assert!(needs_upload(&with(None), "1.5.0", Some(&ours), R11));
        // An unknown own build checks nothing more.
        assert!(!needs_upload(
            &with(Some(build("000000000000"))),
            "1.5.0",
            None,
            R11
        ));
    }

    #[test]
    fn scripts_use_the_build_directory() {
        let dir = install_dir_name("1.5.0", Some(&build("85ee920a1b2c")));
        assert_eq!(dir, "1.5.0+85ee920a1b2c");
        assert!(probe_script(&dir).contains("\"$HOME/.xshell/server/1.5.0+85ee920a1b2c\"/xshelld"));
        assert!(upload_script(&dir).contains("d=\"$HOME/.xshell/server/1.5.0+85ee920a1b2c\";"));
        assert!(select_script(&dir).contains("d=\"$r/\"'1.5.0+85ee920a1b2c'"));
    }

    #[test]
    fn stamp_check_messages() {
        let ours = build("85ee920a1b2c");
        let want = Wanted {
            triple: "aarch64-unknown-linux-musl".into(),
            version: "1.5.0".into(),
            build: Some(ours.clone()),
        };
        let other = build("000000000000");
        let f = |origin, b: Option<&BuildId>| Fetched {
            bytes: fake_daemon_build("1.5.0", b),
            origin,
        };
        // The expected build passes; no expectation passes anything.
        check_fetched(&f(Origin::Release("u".into()), Some(&ours)), &want).unwrap();
        let unknown = Wanted {
            build: None,
            ..want.clone()
        };
        check_fetched(&f(Origin::Release("u".into()), None), &unknown).unwrap();
        assert_eq!(
            check_fetched(&f(Origin::Release("u".into()), Some(&other)), &want).unwrap_err(),
            "the v1.5.0 release’s xshelld reports 000000000000; this Desktop expects 85ee920a1b2c"
        );
        // A binary from before #49 has no stamp: unknown build.
        assert_eq!(
            check_fetched(
                &f(
                    Origin::DevBuild {
                        path: "/t/release-daemon/xshelld".into(),
                        target: None
                    },
                    None
                ),
                &want
            )
            .unwrap_err(),
            "the bundled xshelld at /t/release-daemon/xshelld reports unknown build; this Desktop expects 85ee920a1b2c; rebuild it: cargo build -p xshelld --profile release-daemon"
        );
        let e = check_fetched(
            &f(
                Origin::DevBuild {
                    path: "/t/x/release-daemon/xshelld".into(),
                    target: Some("aarch64-unknown-linux-musl".into()),
                },
                Some(&other),
            ),
            &want,
        )
        .unwrap_err();
        assert!(e.ends_with("rebuild it: cargo build -p xshelld --profile release-daemon --target aarch64-unknown-linux-musl"), "{e}");
    }

    #[test]
    fn version_alphabet() {
        assert!(valid_version("1.5.0-beta.1+g12"));
        assert!(!valid_version("1.5.0;rm"));
        assert!(!valid_version(""));
        assert!(!valid_version("1 2"));
        assert!(!valid_version("."));
        assert!(!valid_version(".."));
        assert!(!valid_version("1/2"));
        assert!(valid_version("..1"));
    }

    #[cfg(unix)]
    fn mode_of(p: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn tmp_leftovers(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn install_local_copies_and_skips_identical() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("sidecar");
        std::fs::write(&src, b"binary-1").unwrap();
        let home = t.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let p = install_local(&src, &home, "1.5.0").unwrap();
        assert_eq!(p, home.join(".xshell/server/1.5.0/xshelld"));
        assert_eq!(std::fs::read(&p).unwrap(), b"binary-1");
        assert_eq!(mode_of(&p), 0o700);
        for d in [".xshell", ".xshell/server", ".xshell/server/1.5.0"] {
            assert_eq!(mode_of(&home.join(d)), 0o700, "{d}");
        }
        let ino = std::fs::metadata(&p).unwrap().ino();
        // Identical: left alone.
        install_local(&src, &home, "1.5.0").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), ino);
        assert!(tmp_leftovers(p.parent().unwrap()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn install_local_replaces_changed_bytes() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("sidecar");
        let home = t.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(&src, b"old").unwrap();
        let p = install_local(&src, &home, "1.5.0").unwrap();
        // A process keeps the old file open, as a running Daemon keeps its binary.
        let held = std::fs::File::open(&p).unwrap();
        let ino = held.metadata().unwrap().ino();
        // A dev build with the same version but other bytes replaces it, by rename.
        std::fs::write(&src, b"new build").unwrap();
        install_local(&src, &home, "1.5.0").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new build");
        assert_ne!(std::fs::metadata(&p).unwrap().ino(), ino);
        let mut old = String::new();
        use std::io::Read;
        (&held).read_to_string(&mut old).unwrap();
        assert_eq!(old, "old", "the running binary was overwritten in place");
        assert!(tmp_leftovers(p.parent().unwrap()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn install_local_repairs_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("sidecar");
        std::fs::write(&src, b"same").unwrap();
        let home = t.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let p = install_local(&src, &home, "1.5.0").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(p.parent().unwrap(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        install_local(&src, &home, "1.5.0").unwrap();
        assert_eq!(mode_of(&p), 0o700);
        assert_eq!(mode_of(p.parent().unwrap()), 0o700);
        assert_eq!(std::fs::read(&p).unwrap(), b"same");
    }

    #[cfg(unix)]
    #[test]
    fn install_local_refuses_symlinked_dir() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("sidecar");
        std::fs::write(&src, b"x").unwrap();
        let home = t.path().join("home");
        let elsewhere = t.path().join("elsewhere");
        std::fs::create_dir_all(home.join(".xshell")).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join(".xshell/server")).unwrap();
        let e = install_local(&src, &home, "1.5.0").unwrap_err();
        assert!(e.to_string().contains("not a directory"), "{e}");
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn install_local_rejects_bad_version() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("sidecar");
        std::fs::write(&src, b"x").unwrap();
        for v in ["", ".", "..", "1/2", "1;rm"] {
            let e = install_local(&src, t.path(), v).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{v:?}");
        }
        assert!(!t.path().join(".xshell").exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_local_concurrent() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let srcs: Vec<_> = (0..8)
            .map(|i| {
                let p = t.path().join(format!("sidecar{i}"));
                std::fs::write(&p, format!("build-{i}").repeat(4096)).unwrap();
                p
            })
            .collect();
        let handles: Vec<_> = srcs
            .iter()
            .cloned()
            .map(|s| {
                let home = home.clone();
                std::thread::spawn(move || install_local(&s, &home, "1.5.0").unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let p = local_server_bin(&home, "1.5.0");
        let got = std::fs::read_to_string(&p).unwrap();
        assert!(
            (0..8).any(|i| got == format!("build-{i}").repeat(4096)),
            "a mixed or partial file"
        );
        assert_eq!(mode_of(&p), 0o700);
        assert!(tmp_leftovers(p.parent().unwrap()).is_empty());
    }

    #[test]
    fn source_error_is_binary_unavailable() {
        // A probe that succeeds without a transport: answer the probe from a local shell.
        #[cfg(unix)]
        {
            let home = tempfile::tempdir().unwrap();
            let t = local(home.path(), None);
            let e = ensure_installed(
                &t,
                &Fails("404"),
                "1.5.0",
                None,
                R11,
                &CancelToken::new(),
                &|| {},
            )
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
        let w = |triple: &str| Wanted {
            triple: triple.into(),
            version: "1".into(),
            build: None,
        };
        let got = chain.fetch(&w("x86_64-unknown-linux-musl"), &c).unwrap();
        assert_eq!(got.bytes, b"B");
        assert_eq!(
            got.origin,
            Origin::Bundled(b.path().join("xshelld-x86_64-unknown-linux-musl"))
        );
        assert_eq!(
            chain.fetch(&w("aarch64-apple-darwin"), &c).unwrap().bytes,
            b"A"
        );
        let e = chain.fetch(&w("x86_64-apple-darwin"), &c).unwrap_err();
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
        let i = ensure_installed(&t, &src, "1.5.0", None, R11, &c, &on_upload).unwrap();
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
        let again = ensure_installed(&t, &src, "1.5.0", None, R11, &c, &on_upload).unwrap();
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
            None,
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
            None,
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

    /// With its build known, the Desktop installs into `<version>+<build>`, and only once.
    #[cfg(unix)]
    #[test]
    fn ensure_installed_keyed_by_build() {
        let home = tempfile::tempdir().unwrap();
        let t = local(home.path(), None);
        let ours = build("85ee920a1b2c");
        let n = Arc::new(AtomicUsize::new(0));
        let src = Counting(Bytes(fake_daemon_build("1.5.0", Some(&ours))), n.clone());
        // A plain-version install of another build is left alone.
        let plain = home.path().join(".xshell/server/1.5.0");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("xshelld"), "old").unwrap();
        let c = CancelToken::new();
        let i = ensure_installed(&t, &src, "1.5.0", Some(&ours), R11, &c, &|| {}).unwrap();
        assert!(i.uploaded);
        assert!(home
            .path()
            .join(".xshell/server/1.5.0+85ee920a1b2c/xshelld")
            .is_file());
        assert_eq!(std::fs::read(plain.join("xshelld")).unwrap(), b"old");
        assert!(!home.path().join(".xshell/server/selected").exists());
        let again = ensure_installed(&t, &src, "1.5.0", Some(&ours), R11, &c, &|| {}).unwrap();
        assert!(!again.uploaded);
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    /// A binary whose stamp names another build is never uploaded.
    #[cfg(unix)]
    #[test]
    fn stale_binary_is_not_uploaded() {
        let home = tempfile::tempdir().unwrap();
        let t = local(home.path(), None);
        let ours = build("85ee920a1b2c");
        let uploads = AtomicUsize::new(0);
        let e = ensure_installed(
            &t,
            &Bytes(fake_daemon_build("1.5.0", Some(&build("000000000000")))),
            "1.5.0",
            Some(&ours),
            R11,
            &CancelToken::new(),
            &|| {
                uploads.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap_err();
        assert_eq!(e.hint, Some(HostErrorHint::BinaryUnavailable));
        assert!(
            e.message
                .contains("reports 000000000000; this Desktop expects 85ee920a1b2c; rebuild it"),
            "{}",
            e.message
        );
        assert_eq!(uploads.load(Ordering::SeqCst), 0);
        assert!(!home
            .path()
            .join(".xshell/server/1.5.0+85ee920a1b2c")
            .exists());
    }

    /// The stamp may match while `--version` reports another build: refused after upload.
    #[cfg(unix)]
    #[test]
    fn rejects_wrong_reported_build() {
        let home = tempfile::tempdir().unwrap();
        let t = local(home.path(), None);
        let ours = build("85ee920a1b2c");
        let mut bytes = fake_daemon_build("1.5.0", Some(&build("000000000000")));
        bytes.extend(format!("# @@XSHELL-BUILD-V1:{}:1@@\n", ours.display()).into_bytes());
        // Two disagreeing stamps are no stamp at all.
        let e = ensure_installed(
            &t,
            &Bytes(bytes),
            "1.5.0",
            Some(&ours),
            R11,
            &CancelToken::new(),
            &|| {},
        )
        .unwrap_err();
        assert!(e.message.contains("unknown build"), "{}", e.message);
        // A matching stamp but another reported build.
        let script = format!(
            "#!/bin/sh\n# @@XSHELL-BUILD-V1:{}:1@@\necho '{{\"name\":\"xshelld\",\"version\":\"1.5.0\",\"protocol\":{{\"min\":1,\"max\":1}},\"build\":null}}'\n",
            ours.display()
        );
        let e = ensure_installed(
            &t,
            &Bytes(script.into_bytes()),
            "1.5.0",
            Some(&ours),
            R11,
            &CancelToken::new(),
            &|| {},
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "installed xshelld reports unknown build; expected 85ee920a1b2c"
        );
    }

    /// The select script writes the record only for an installed build, atomically.
    #[cfg(unix)]
    #[test]
    fn select_script_against_temp_home() {
        let home = tempfile::tempdir().unwrap();
        let t = local(home.path(), None);
        let c = CancelToken::new();
        let e = select_remote(&t, "1.5.0+85ee920a1b2c", &c).unwrap_err();
        assert_eq!(e.message, BUILD_NOT_INSTALLED);
        let ours = build("85ee920a1b2c");
        ensure_installed(
            &t,
            &Bytes(fake_daemon_build("1.5.0", Some(&ours))),
            "1.5.0",
            Some(&ours),
            R11,
            &c,
            &|| {},
        )
        .unwrap();
        select_remote(&t, "1.5.0+85ee920a1b2c", &c).unwrap();
        let root = home.path().join(".xshell/server");
        assert_eq!(
            std::fs::read_to_string(root.join("selected")).unwrap(),
            "1.5.0+85ee920a1b2c\n"
        );
        // What xshelld reads back.
        assert_eq!(
            xshell_core::server::resolve_selected(&root, "1.5.0"),
            Some(root.join("1.5.0+85ee920a1b2c/xshelld"))
        );
        let left: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert!(select_remote(&t, "1;rm", &c).is_err());
    }
}

//! `xshelld service install|uninstall|status|run|ensure` (ADR-0017): keep this machine's
//! Daemon running on its own, through the first of these that works here:
//!
//! 1. a systemd user unit (`Restart=always`, linger turned on where allowed);
//! 2. a `crontab` `@reboot` line (only when a cron daemon runs);
//! 3. a guarded block in the shell profile that starts the Daemon when none runs.
//!
//! Every mechanism starts the stable launcher `~/.xshell/service/xshelld`, a small `sh` script
//! that execs the selected build (`~/.xshell/server/selected`, else the newest installed), so
//! an update never has to rewrite the unit, the crontab or the profile.
//!
//! `service run` waits while a Daemon of this home is alive (one the Desktop started, say),
//! then execs `serve --interactive-env` through the launcher, keeping its pid, so a systemd
//! `Type=simple` unit watches the Daemon itself. `service ensure` starts `service run`
//! detached unless a Daemon is alive, and returns at once (the profile block).
//!
//! Every call to `systemctl`, `loginctl` and `crontab`, and every edit of a unit or profile
//! file, goes through [`Runner`], so tests use a fake one and never touch the real user's
//! services (A1). `XSHELL_SERVICE_MECHANISM` (or `--mechanism`) forces one mechanism; then
//! the others are neither inspected nor cleaned up.
//!
//! `run` and `ensure` set `XSHELL_SERVICE_GUARD=1` for everything they start; the profile
//! block does nothing while it is set, and `serve`'s login shells set it too, so starting the
//! Daemon never starts another one through the profile (A3).

use crate::cli::{Mechanism, ServiceAction, ServiceOpts};
use crate::paths::{daemon_alive, ensure_private_dir, resolve, Paths};
use serde::Serialize;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Set for everything `service run` / `service ensure` start; the profile block and
/// `service ensure` do nothing while it is set.
pub const GUARD_ENV: &str = "XSHELL_SERVICE_GUARD";
/// Forces a mechanism (`systemd|cron|profile|none`); cross-mechanism cleanup is skipped.
pub const MECHANISM_ENV: &str = "XSHELL_SERVICE_MECHANISM";
/// The mechanism that started this `serve` (informational, in the log).
pub const SERVICE_ENV: &str = "XSHELLD_SERVICE";
/// The cron line's marker comment.
const CRON_TAG: &str = "# xshell-daemon";
const PROFILE_BEGIN: &str = "# >>> xshell daemon >>>";
const PROFILE_END: &str = "# <<< xshell daemon <<<";
/// The systemd unit's file name.
pub const UNIT_NAME: &str = "xshelld.service";
/// Each `systemctl`/`loginctl`/`crontab` call is ended after this.
const CHILD_TIMEOUT: Duration = Duration::from_secs(5);

/// The output of one command the [`Runner`] ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Every side effect on the user's service managers and shell profile.
pub trait Runner {
    /// Run `prog args…` with `stdin` (none: `/dev/null`), ended after a few seconds.
    fn run(&self, prog: &str, args: &[&str], stdin: Option<&str>) -> io::Result<Ran>;
    /// A file's text; `None` when it does not exist.
    fn read(&self, path: &Path) -> io::Result<Option<String>>;
    /// Replace a file's text atomically (its directory is created).
    fn write(&self, path: &Path, text: &str) -> io::Result<()>;
    /// Remove a file; a missing one is fine.
    fn remove(&self, path: &Path) -> io::Result<()>;
    fn exists(&self, path: &Path) -> bool {
        matches!(self.read(path), Ok(Some(_)))
    }
}

/// The real system.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, prog: &str, args: &[&str], stdin: Option<&str>) -> io::Result<Ran> {
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(text.as_bytes());
        }
        let mut out = child.stdout.take().expect("piped");
        let mut err = child.stderr.take().expect("piped");
        let o = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let e = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let deadline = Instant::now() + CHILD_TIMEOUT;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break Some(st);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let Some(status) = status else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{prog} {} did not finish", args.join(" ")),
            ));
        };
        Ok(Ran {
            code: status.code(),
            stdout: o.join().unwrap_or_default(),
            stderr: e.join().unwrap_or_default(),
        })
    }

    fn read(&self, path: &Path) -> io::Result<Option<String>> {
        match fs::read_to_string(path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write(&self, path: &Path, text: &str) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Keep an existing file's mode (a profile may be 0644 or 0600); new files are 0644.
        let mode = fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o644);
        atomic_write(path, text.as_bytes(), mode)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Write `bytes` to a new temp file next to `path` (mode `mode`), sync it and rename it over.
fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".xshell.{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let r = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// What decides which mechanism applies on this machine.
pub trait Probe {
    /// systemd is PID 1 (`/run/systemd/system` is a directory) and the user manager answers.
    fn systemd(&self) -> bool;
    /// `crontab` runs and a cron daemon is running.
    fn cron(&self) -> bool;
}

/// The real machine, asked through `runner`.
pub struct SystemProbe<'a>(pub &'a dyn Runner);

impl Probe for SystemProbe<'_> {
    fn systemd(&self) -> bool {
        Path::new("/run/systemd/system").is_dir()
            && self
                .0
                .run("systemctl", &["--user", "show-environment"], None)
                .is_ok_and(|r| r.ok())
    }

    fn cron(&self) -> bool {
        // `crontab -l` exits 1 without a crontab; it must at least run.
        self.0.run("crontab", &["-l"], None).is_ok() && cron_daemon_running()
    }
}

/// Whether a cron daemon runs (Linux: a process named `cron`, `crond` or `cronie`; elsewhere
/// assumed, as macOS has one).
fn cron_daemon_running() -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    let Ok(dir) = fs::read_dir("/proc") else {
        return false;
    };
    dir.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            && fs::read_to_string(e.path().join("comm"))
                .is_ok_and(|c| matches!(c.trim(), "cron" | "crond" | "cronie"))
    })
}

/// systemd, else cron, else the shell profile.
pub fn detect(probe: &dyn Probe) -> Mechanism {
    if probe.systemd() {
        Mechanism::Systemd
    } else if probe.cron() {
        Mechanism::Cron
    } else {
        Mechanism::Profile
    }
}

/// Where one home's service files are.
#[derive(Debug, Clone)]
pub struct Layout {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, else `home/.config`.
    pub config_home: PathBuf,
    pub paths: Paths,
    /// Whether `--home` was given (then every mechanism passes it on).
    pub explicit_home: bool,
}

impl Layout {
    pub fn new(
        home: PathBuf,
        xdg_config_home: Option<PathBuf>,
        xdg_runtime: Option<&Path>,
    ) -> Self {
        let config_home = xdg_config_home
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let paths = resolve(&home, xdg_runtime, None);
        Self {
            home,
            config_home,
            paths,
            explicit_home: false,
        }
    }

    pub fn launcher(&self) -> PathBuf {
        launcher_path(&self.home)
    }

    pub fn unit(&self) -> PathBuf {
        self.config_home
            .join("systemd")
            .join("user")
            .join(UNIT_NAME)
    }

    /// `--home <home>` for the commands the mechanisms run.
    fn home_args(&self) -> Vec<String> {
        vec!["--home".into(), self.home.to_string_lossy().into_owned()]
    }
}

/// `home/.xshell/service/xshelld`.
pub fn launcher_path(home: &Path) -> PathBuf {
    home.join(".xshell").join("service").join("xshelld")
}

/// Single-quote `s` for `sh`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Quote `s` as one systemd `ExecStart` word.
fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The launcher script. It mirrors `xshell_hostlink::local::start_binary` (the one Rust
/// decision every other launch path uses): the build the `selected` record names; only when
/// there is no record, the newest installed build. Every argument is passed through, so a
/// `~/.local/bin/xshelld` link to it works as `xshelld`.
pub fn launcher_text(server_root: &Path) -> String {
    let root = sh_quote(&server_root.to_string_lossy());
    format!(
        r#"#!/bin/sh
# xshell: runs this machine's selected xshelld (written by `xshelld service install`).
root={root}
if [ -e "$root/selected" ]; then
  name=$(head -n 1 "$root/selected")
  case "$name" in
    ''|.*|*/*) echo "xshell: $root/selected names no valid build" >&2; exit 127 ;;
  esac
  bin="$root/$name/xshelld"
else
  bin=$(ls -1t "$root"/*/xshelld 2>/dev/null | head -n 1)
fi
if [ -z "$bin" ] || [ ! -x "$bin" ]; then
  echo "xshell: no installed xshelld under $root" >&2
  exit 127
fi
# may-start hook: wait_until_may_start (xshell_hostlink::local) allows at once today.
exec "$bin" "$@"
"#
    )
}

/// Write the launcher (0700, in a 0700 directory) unless it already says this. Returns
/// whether it changed.
pub fn write_launcher(l: &Layout) -> io::Result<bool> {
    let path = l.launcher();
    ensure_private_dir(path.parent().expect("launcher has a directory"))?;
    let text = launcher_text(&l.paths.server_root);
    if let Ok(m) = fs::symlink_metadata(&path) {
        if m.file_type().is_file()
            && m.permissions().mode() & 0o777 == 0o700
            && fs::read(&path).is_ok_and(|b| b == text.as_bytes())
        {
            return Ok(false);
        }
    }
    atomic_write(&path, text.as_bytes(), 0o700)?;
    Ok(true)
}

/// The systemd user unit.
pub fn unit_text(l: &Layout) -> String {
    let mut exec = vec![systemd_quote(&l.launcher().to_string_lossy())];
    exec.push("service".into());
    exec.push("run".into());
    for a in l.home_args() {
        exec.push(systemd_quote(&a));
    }
    format!(
        "[Unit]\nDescription=xshell Daemon (keeps this machine's terminals running)\n\n\
         [Service]\nType=simple\nExecStart={}\nRestart=always\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n",
        exec.join(" ")
    )
}

/// The crontab line.
pub fn cron_line(l: &Layout) -> String {
    format!(
        "@reboot {} service run --home {} >/dev/null 2>&1 {CRON_TAG}",
        sh_quote(&l.launcher().to_string_lossy()),
        sh_quote(&l.home.to_string_lossy())
    )
}

/// `crontab` text with exactly one xshell line, `line`, replacing any earlier one.
pub fn cron_merge(current: &str, line: &str) -> String {
    let mut out: Vec<&str> = current.lines().filter(|l| !l.contains(CRON_TAG)).collect();
    out.push(line);
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// `crontab` text without xshell lines.
pub fn cron_remove(current: &str) -> String {
    let out: Vec<&str> = current.lines().filter(|l| !l.contains(CRON_TAG)).collect();
    if out.is_empty() {
        return String::new();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The profile block.
pub fn profile_block(l: &Layout) -> String {
    let launcher = sh_quote(&l.launcher().to_string_lossy());
    format!(
        "{PROFILE_BEGIN}\n\
         if [ -z \"${{{GUARD_ENV}:-}}\" ] && [ -x {launcher} ]; then {launcher} service ensure --home {} >/dev/null 2>&1; fi\n\
         {PROFILE_END}\n",
        sh_quote(&l.home.to_string_lossy())
    )
}

/// `current` with one xshell block, `block`, in place of any earlier one (appended if none).
pub fn profile_merge(current: &str, block: &str) -> String {
    let stripped = profile_remove(current);
    let mut s = stripped;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(block);
    s
}

/// `current` without xshell blocks.
pub fn profile_remove(current: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in current.split_inclusive('\n') {
        let t = line.trim_end();
        if !inside && t == PROFILE_BEGIN {
            inside = true;
            continue;
        }
        if inside {
            if t == PROFILE_END {
                inside = false;
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

/// The profile files the block goes into: `~/.profile` (created when no login profile
/// exists), plus `.bash_profile`, `.bashrc`, `.zprofile` and `.zshrc` where they exist.
pub fn profile_targets(home: &Path, runner: &dyn Runner) -> Vec<PathBuf> {
    let p = |n: &str| home.join(n);
    let mut v = Vec::new();
    let login = [".profile", ".bash_profile", ".bash_login", ".zprofile"];
    if runner.exists(&p(".profile")) || !login.iter().any(|n| runner.exists(&p(n))) {
        v.push(p(".profile"));
    }
    for n in [".bash_profile", ".bashrc", ".zprofile", ".zshrc"] {
        if runner.exists(&p(n)) {
            v.push(p(n));
        }
    }
    v
}

/// Every profile file that may hold a block.
fn profile_candidates(home: &Path) -> Vec<PathBuf> {
    [
        ".profile",
        ".bash_profile",
        ".bashrc",
        ".zprofile",
        ".zshrc",
    ]
    .iter()
    .map(|n| home.join(n))
    .collect()
}

/// What `install` and `status` report (`--json`: one line).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    pub mechanism: &'static str,
    pub installed: Vec<&'static str>,
    pub active: bool,
    pub linger: Option<bool>,
    pub running: bool,
    pub launcher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    pub warnings: Vec<String>,
}

impl Report {
    /// One line for people.
    pub fn text(&self) -> String {
        let how = match self.mechanism {
            "systemd" => format!(
                "keeps running via systemd user unit (linger {})",
                match self.linger {
                    Some(true) => "on",
                    Some(false) => "off: it stops when you log out",
                    None => "unknown",
                }
            ),
            "cron" => "starts at boot via crontab @reboot".into(),
            "profile" => "starts when you log in (shell profile)".into(),
            _ => "nothing keeps it running".into(),
        };
        let run = if self.running {
            "running now"
        } else {
            "not running now"
        };
        let mut s = format!("xshelld {how}; {run}");
        for w in &self.warnings {
            s.push_str("\nwarning: ");
            s.push_str(w);
        }
        s
    }
}

/// What one mechanism has on this machine.
fn installed(l: &Layout, runner: &dyn Runner, m: Mechanism) -> bool {
    match m {
        Mechanism::Systemd => runner.exists(&l.unit()),
        Mechanism::Cron => runner
            .run("crontab", &["-l"], None)
            .is_ok_and(|r| r.ok() && r.stdout.lines().any(|x| x.contains(CRON_TAG))),
        Mechanism::Profile => profile_candidates(&l.home).iter().any(|p| {
            runner
                .read(p)
                .ok()
                .flatten()
                .is_some_and(|t| t.lines().any(|x| x.trim_end() == PROFILE_BEGIN))
        }),
        Mechanism::None => false,
    }
}

const ALL: [Mechanism; 3] = [Mechanism::Systemd, Mechanism::Cron, Mechanism::Profile];

/// Install `m`, return whether anything changed. Warnings are pushed to `warnings`.
fn apply(
    l: &Layout,
    runner: &dyn Runner,
    m: Mechanism,
    warnings: &mut Vec<String>,
) -> io::Result<bool> {
    match m {
        Mechanism::Systemd => {
            let unit = l.unit();
            let text = unit_text(l);
            let changed = runner.read(&unit)?.as_deref() != Some(text.as_str());
            if changed {
                runner.write(&unit, &text)?;
            }
            let ctl = |args: &[&str]| -> io::Result<()> {
                let mut a = vec!["--user"];
                a.extend_from_slice(args);
                let r = runner.run("systemctl", &a, None)?;
                if r.ok() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "systemctl {} failed: {}",
                        a.join(" "),
                        r.stderr.trim()
                    )))
                }
            };
            ctl(&["daemon-reload"])?;
            ctl(&["enable", UNIT_NAME])?;
            ctl(&["start", UNIT_NAME])?;
            match runner.run("loginctl", &["--no-ask-password", "enable-linger"], None) {
                Ok(r) if r.ok() => {}
                Ok(r) => warnings.push(format!(
                    "cannot turn on linger ({}); the Daemon stops when you log out",
                    r.stderr.trim()
                )),
                Err(e) => warnings.push(format!(
                    "cannot turn on linger ({e}); the Daemon stops when you log out"
                )),
            }
            Ok(changed)
        }
        Mechanism::Cron => {
            let r = runner.run("crontab", &["-l"], None)?;
            // Exit 1 with "no crontab for …": empty.
            let current = if r.ok() { r.stdout } else { String::new() };
            let merged = cron_merge(&current, &cron_line(l));
            if merged == current {
                return Ok(false);
            }
            let w = runner.run("crontab", &["-"], Some(&merged))?;
            if !w.ok() {
                return Err(io::Error::other(format!(
                    "crontab - failed: {}",
                    w.stderr.trim()
                )));
            }
            Ok(true)
        }
        Mechanism::Profile => {
            let block = profile_block(l);
            let mut changed = false;
            for p in profile_targets(&l.home, runner) {
                let current = runner.read(&p)?.unwrap_or_default();
                let merged = profile_merge(&current, &block);
                if merged != current {
                    runner.write(&p, &merged)?;
                    changed = true;
                }
            }
            Ok(changed)
        }
        Mechanism::None => Ok(false),
    }
}

/// Remove `m`'s entries. A running Daemon is left alone (the unit is not stopped).
fn remove(l: &Layout, runner: &dyn Runner, m: Mechanism) -> io::Result<bool> {
    match m {
        Mechanism::Systemd => {
            let unit = l.unit();
            if !runner.exists(&unit) {
                return Ok(false);
            }
            let _ = runner.run("systemctl", &["--user", "disable", UNIT_NAME], None);
            runner.remove(&unit)?;
            let _ = runner.run("systemctl", &["--user", "daemon-reload"], None);
            Ok(true)
        }
        Mechanism::Cron => {
            let Ok(r) = runner.run("crontab", &["-l"], None) else {
                return Ok(false);
            };
            if !r.ok() || !r.stdout.lines().any(|x| x.contains(CRON_TAG)) {
                return Ok(false);
            }
            let rest = cron_remove(&r.stdout);
            let w = if rest.is_empty() {
                runner.run("crontab", &["-r"], None)?
            } else {
                runner.run("crontab", &["-"], Some(&rest))?
            };
            if !w.ok() {
                return Err(io::Error::other(format!(
                    "crontab failed: {}",
                    w.stderr.trim()
                )));
            }
            Ok(true)
        }
        Mechanism::Profile => {
            let mut changed = false;
            for p in profile_candidates(&l.home) {
                if let Some(current) = runner.read(&p)? {
                    let rest = profile_remove(&current);
                    if rest != current {
                        runner.write(&p, &rest)?;
                        changed = true;
                    }
                }
            }
            Ok(changed)
        }
        Mechanism::None => Ok(false),
    }
}

/// The systemd unit's state and the user's linger.
fn systemd_state(runner: &dyn Runner) -> (bool, Option<bool>) {
    let active = runner
        .run("systemctl", &["--user", "is-active", UNIT_NAME], None)
        .is_ok_and(|r| r.stdout.trim() == "active");
    let user = std::env::var("USER").unwrap_or_default();
    let mut args = vec!["show-user"];
    if !user.is_empty() {
        args.push(&user);
    }
    args.extend(["-p", "Linger"]);
    let linger = runner
        .run("loginctl", &args, None)
        .ok()
        .filter(|r| r.ok())
        .and_then(|r| match r.stdout.trim() {
            "Linger=yes" => Some(true),
            "Linger=no" => Some(false),
            _ => None,
        });
    (active, linger)
}

/// The mechanisms to inspect: only the forced one, or all.
fn scope(forced: Option<Mechanism>) -> Vec<Mechanism> {
    match forced {
        Some(Mechanism::None) => vec![],
        Some(m) => vec![m],
        None => ALL.to_vec(),
    }
}

/// `service status`.
pub fn status(l: &Layout, runner: &dyn Runner, forced: Option<Mechanism>) -> Report {
    let installed: Vec<Mechanism> = scope(forced)
        .into_iter()
        .filter(|m| installed(l, runner, *m))
        .collect();
    let mechanism = forced
        .or_else(|| installed.first().copied())
        .unwrap_or(Mechanism::None);
    let (active, linger) = match mechanism {
        Mechanism::Systemd if installed.contains(&Mechanism::Systemd) => systemd_state(runner),
        Mechanism::Systemd => (false, None),
        m => (installed.contains(&m), None),
    };
    let mut warnings = Vec::new();
    if !l.launcher().is_file() && mechanism != Mechanism::None {
        warnings.push(format!("{} is missing", l.launcher().display()));
    }
    Report {
        mechanism: mechanism.as_str(),
        installed: installed.iter().map(|m| m.as_str()).collect(),
        active,
        linger,
        running: daemon_alive(&l.home, &l.paths.socket),
        launcher: l.launcher().display().to_string(),
        changed: None,
        warnings,
    }
}

/// `service install`: install this binary's build when it is not installed yet, refresh the
/// launcher, apply the mechanism, and (unless forced) remove the others. Idempotent.
pub fn install(
    l: &Layout,
    runner: &dyn Runner,
    probe: &dyn Probe,
    forced: Option<Mechanism>,
    own_bin: Option<&Path>,
) -> io::Result<Report> {
    let mut changed = false;
    if let Some(bin) = own_bin {
        changed |= install_own(&l.home, bin)?;
    }
    changed |= write_launcher(l)?;
    let mechanism = forced.unwrap_or_else(|| detect(probe));
    let mut warnings = Vec::new();
    changed |= apply(l, runner, mechanism, &mut warnings)?;
    if forced.is_none() {
        for other in ALL.into_iter().filter(|m| *m != mechanism) {
            match remove(l, runner, other) {
                Ok(c) => changed |= c,
                Err(e) => warnings.push(format!("cannot remove the {} entry: {e}", other.as_str())),
            }
        }
    }
    let mut r = status(l, runner, Some(mechanism));
    r.changed = Some(changed);
    r.warnings.extend(warnings);
    Ok(r)
}

/// `service uninstall`: remove the entries (only the forced mechanism's, when forced) and the
/// launcher. The running Daemon is left alone.
pub fn uninstall(l: &Layout, runner: &dyn Runner, forced: Option<Mechanism>) -> io::Result<bool> {
    let mut changed = false;
    for m in scope(forced) {
        changed |= remove(l, runner, m)?;
    }
    match fs::remove_file(l.launcher()) {
        Ok(()) => changed = true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(changed)
}

/// Install the running binary under `~/.xshell/server/<version>+<build>/` unless it already
/// runs from the server root. Returns whether a file was written.
fn install_own(home: &Path, bin: &Path) -> io::Result<bool> {
    let root = xshell_core::server::server_root(home);
    if bin.starts_with(&root) {
        return Ok(false);
    }
    let dir = xshell_protocol::build::install_dir_name(
        env!("CARGO_PKG_VERSION"),
        crate::build_id::BUILD.as_ref(),
    );
    let dest = root.join(&dir).join("xshelld");
    let before = fs::read(&dest).ok();
    xshell_hostlink::install::install_local(bin, home, &dir)?;
    Ok(before != fs::read(&dest).ok())
}

/// How often `service run` checks whether the Daemon it waits behind has gone. Test hook:
/// `XSHELL_SERVICE_POLL_MS`.
fn poll_interval() -> Duration {
    std::env::var("XSHELL_SERVICE_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(5))
}

/// `service run`: wait while a Daemon of this home is alive, then exec `serve
/// --interactive-env` of the build [`xshell_hostlink::local::start_binary`] picks now
/// (keeping this pid; never this binary's own path, A5). Returns only on failure.
pub fn run(l: &Layout, mechanism: &str) -> i32 {
    let poll = poll_interval();
    while daemon_alive(&l.home, &l.paths.socket) {
        std::thread::sleep(poll);
    }
    let bin = match xshell_hostlink::local::start_binary(&l.home) {
        Ok(Some(b)) => b,
        Ok(None) => {
            eprintln!(
                "xshelld: no installed xshelld under {}; run `xshelld service install`",
                l.paths.server_root.display()
            );
            return 1;
        }
        Err(e) => {
            eprintln!("xshelld: {e}");
            return 1;
        }
    };
    xshell_hostlink::local::wait_until_may_start(&l.home);
    let launcher = bin;
    let mut cmd = Command::new(&launcher);
    cmd.arg("serve").arg("--interactive-env");
    if l.explicit_home {
        cmd.arg("--home").arg(&l.home);
    }
    cmd.env(GUARD_ENV, "1").env(SERVICE_ENV, mechanism);
    let e = cmd.exec();
    eprintln!("xshelld: cannot start {}: {e}", launcher.display());
    1
}

/// `service ensure`: start `service run` detached unless a Daemon is alive. Returns at once.
pub fn ensure(l: &Layout) -> i32 {
    if std::env::var_os(GUARD_ENV).is_some_and(|v| !v.is_empty()) {
        return 0;
    }
    if daemon_alive(&l.home, &l.paths.socket) {
        return 0;
    }
    xshell_hostlink::local::wait_until_may_start(&l.home);
    let launcher = l.launcher();
    let mut cmd = if launcher.is_file() {
        let mut c = Command::new("/bin/sh");
        c.arg(&launcher);
        c
    } else {
        match std::env::current_exe() {
            Ok(exe) => Command::new(exe),
            Err(e) => {
                eprintln!("xshelld: {e}");
                return 1;
            }
        }
    };
    cmd.arg("service").arg("run");
    if l.explicit_home {
        cmd.arg("--home").arg(&l.home);
    }
    let log = match crate::log::open_log(&l.paths.log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("xshelld: cannot open {}: {e}", l.paths.log.display());
            return 1;
        }
    };
    let Ok(log2) = log.try_clone() else {
        return 1;
    };
    cmd.env(GUARD_ENV, "1")
        .env(SERVICE_ENV, "profile")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2)
        .current_dir("/");
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match cmd.spawn() {
        // Not waited for: it becomes the Daemon. Reaped by init once this process exits.
        Ok(_) => 0,
        Err(e) => {
            eprintln!("xshelld: cannot start the Daemon: {e}");
            1
        }
    }
}

/// The mechanism forced by `--mechanism` or the environment.
pub fn forced_mechanism(
    opts: &ServiceOpts,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Option<Mechanism>, String> {
    if let Some(m) = opts.mechanism {
        return Ok(Some(m));
    }
    for key in [MECHANISM_ENV, "XSHELLD_SERVICE_MECHANISM"] {
        if let Some(v) = env(key).filter(|v| !v.is_empty()) {
            let s = v.to_string_lossy();
            return Mechanism::parse(&s)
                .map(Some)
                .ok_or_else(|| format!("{key}: unknown mechanism {s:?}"));
        }
    }
    Ok(None)
}

/// `xshelld service …`.
pub fn main(action: ServiceAction, opts: &ServiceOpts) -> i32 {
    let env = |k: &str| std::env::var_os(k);
    let Some(home) = opts.home.clone().or_else(dirs::home_dir) else {
        eprintln!("xshelld: cannot determine the home directory; pass --home");
        return 1;
    };
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let mut l = Layout::new(
        home,
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        xdg.as_deref(),
    );
    l.explicit_home = opts.home.is_some();
    let forced = match forced_mechanism(opts, &env) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("xshelld: {e}");
            return 2;
        }
    };
    let print = |r: &Report| {
        if opts.json {
            println!("{}", serde_json::to_string(r).expect("a report serializes"));
        } else {
            println!("{}", r.text());
        }
    };
    let runner = SystemRunner;
    match action {
        ServiceAction::Install => {
            let own = std::env::current_exe().ok();
            match install(&l, &runner, &SystemProbe(&runner), forced, own.as_deref()) {
                Ok(r) => {
                    print(&r);
                    0
                }
                Err(e) => {
                    eprintln!("xshelld: service install failed: {e}");
                    1
                }
            }
        }
        ServiceAction::Uninstall => match uninstall(&l, &runner, forced) {
            Ok(changed) => {
                if opts.json {
                    println!("{}", serde_json::json!({ "changed": changed }));
                } else {
                    println!("xshelld service removed; a running Daemon keeps running");
                }
                0
            }
            Err(e) => {
                eprintln!("xshelld: service uninstall failed: {e}");
                1
            }
        },
        ServiceAction::Status => {
            print(&status(&l, &runner, forced));
            0
        }
        ServiceAction::Run => {
            let mech = env(SERVICE_ENV)
                .map(|v| v.to_string_lossy().into_owned())
                .or_else(|| forced.map(|m| m.as_str().to_string()))
                .unwrap_or_else(|| "service".into());
            run(&l, &mech)
        }
        ServiceAction::Ensure => ensure(&l),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Records every command; files live in memory; answers are scripted.
    #[derive(Default)]
    struct Fake {
        log: RefCell<Vec<String>>,
        files: RefCell<HashMap<PathBuf, String>>,
        crontab: RefCell<Option<String>>,
        fail: RefCell<Vec<String>>,
    }

    impl Fake {
        fn cmds(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl Runner for Fake {
        fn run(&self, prog: &str, args: &[&str], stdin: Option<&str>) -> io::Result<Ran> {
            let line = format!("{prog} {}", args.join(" "));
            self.log.borrow_mut().push(line.clone());
            if self
                .fail
                .borrow()
                .iter()
                .any(|f| line.starts_with(f.as_str()))
            {
                return Ok(Ran {
                    code: Some(1),
                    stderr: "denied".into(),
                    ..Default::default()
                });
            }
            Ok(match (prog, args) {
                ("crontab", ["-l"]) => match &*self.crontab.borrow() {
                    Some(t) => Ran {
                        code: Some(0),
                        stdout: t.clone(),
                        ..Default::default()
                    },
                    None => Ran {
                        code: Some(1),
                        stderr: "no crontab".into(),
                        ..Default::default()
                    },
                },
                ("crontab", ["-"]) => {
                    *self.crontab.borrow_mut() = Some(stdin.unwrap_or_default().into());
                    Ran {
                        code: Some(0),
                        ..Default::default()
                    }
                }
                ("crontab", ["-r"]) => {
                    *self.crontab.borrow_mut() = None;
                    Ran {
                        code: Some(0),
                        ..Default::default()
                    }
                }
                ("loginctl", [.., "Linger"]) => Ran {
                    code: Some(0),
                    stdout: "Linger=yes\n".into(),
                    ..Default::default()
                },
                ("systemctl", ["--user", "is-active", _]) => Ran {
                    code: Some(0),
                    stdout: "active\n".into(),
                    ..Default::default()
                },
                _ => Ran {
                    code: Some(0),
                    ..Default::default()
                },
            })
        }
        fn read(&self, path: &Path) -> io::Result<Option<String>> {
            Ok(self.files.borrow().get(path).cloned())
        }
        fn write(&self, path: &Path, text: &str) -> io::Result<()> {
            self.log
                .borrow_mut()
                .push(format!("write {}", path.display()));
            self.files.borrow_mut().insert(path.into(), text.into());
            Ok(())
        }
        fn remove(&self, path: &Path) -> io::Result<()> {
            self.log
                .borrow_mut()
                .push(format!("remove {}", path.display()));
            self.files.borrow_mut().remove(path);
            Ok(())
        }
    }

    struct P(bool, bool);
    impl Probe for P {
        fn systemd(&self) -> bool {
            self.0
        }
        fn cron(&self) -> bool {
            self.1
        }
    }

    fn layout(t: &Path) -> Layout {
        let mut l = Layout::new(t.join("home"), None, None);
        l.explicit_home = true;
        fs::create_dir_all(&l.home).unwrap();
        l
    }

    #[test]
    fn unit_text_restart_always_launcher() {
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        let u = unit_text(&l);
        assert!(u.contains("Restart=always"), "{u}");
        assert!(u.contains("Type=simple"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains(&format!(
            "ExecStart=\"{}\" service run \"--home\" \"{}\"",
            l.launcher().display(),
            l.home.display()
        )));
        assert_eq!(
            l.unit(),
            l.home.join(".config/systemd/user/xshelld.service")
        );
        assert_eq!(systemd_quote("a%b$c\"d"), "\"a%%b$$c\\\"d\"");
    }

    #[test]
    fn cron_merge_once_replace_remove() {
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        let line = cron_line(&l);
        assert!(line.starts_with("@reboot '"));
        assert!(line.ends_with(CRON_TAG));
        let base = "MAILTO=x\n0 * * * * backup\n";
        let once = cron_merge(base, &line);
        assert_eq!(once, format!("{base}{line}\n"));
        assert_eq!(cron_merge(&once, &line), once, "idempotent");
        let replaced = cron_merge(&once, "@reboot other # xshell-daemon");
        assert_eq!(replaced.matches(CRON_TAG).count(), 1);
        assert!(replaced.contains("@reboot other"));
        assert_eq!(cron_remove(&once), base);
        assert_eq!(cron_remove(&format!("{line}\n")), "");
    }

    #[test]
    fn profile_block_idempotent_replace_remove() {
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        let block = profile_block(&l);
        assert!(block.contains(&format!("[ -z \"${{{GUARD_ENV}:-}}\" ]")));
        assert!(block.contains("service ensure --home"));
        let base = "export A=1\n# keep me";
        let once = profile_merge(base, &block);
        assert_eq!(once, format!("{base}\n{block}"));
        assert_eq!(profile_merge(&once, &block), once);
        let other = block.replace("ensure", "ensure-old");
        let replaced = profile_merge(&profile_merge(&once, &other), &block);
        assert_eq!(replaced.matches(PROFILE_BEGIN).count(), 1);
        assert_eq!(replaced, once);
        assert_eq!(profile_remove(&once), format!("{base}\n"));
        assert_eq!(profile_remove(&block), "");
        assert_eq!(profile_merge("", &block), block);
    }

    #[test]
    fn profile_targets_create_profile_only_without_a_login_profile() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let f = Fake::default();
        assert_eq!(profile_targets(home, &f), vec![home.join(".profile")]);
        f.files
            .borrow_mut()
            .insert(home.join(".bash_profile"), String::new());
        f.files
            .borrow_mut()
            .insert(home.join(".bashrc"), String::new());
        assert_eq!(
            profile_targets(home, &f),
            vec![home.join(".bash_profile"), home.join(".bashrc")]
        );
        f.files
            .borrow_mut()
            .insert(home.join(".profile"), String::new());
        f.files
            .borrow_mut()
            .insert(home.join(".zshrc"), String::new());
        assert_eq!(
            profile_targets(home, &f),
            vec![
                home.join(".profile"),
                home.join(".bash_profile"),
                home.join(".bashrc"),
                home.join(".zshrc")
            ]
        );
    }

    #[test]
    fn detect_order() {
        assert_eq!(detect(&P(true, true)), Mechanism::Systemd);
        assert_eq!(detect(&P(false, true)), Mechanism::Cron);
        assert_eq!(detect(&P(false, false)), Mechanism::Profile);
    }

    #[test]
    fn forced_mechanism() {
        let none = |_: &str| None;
        let o = ServiceOpts::default();
        assert_eq!(super::forced_mechanism(&o, &none), Ok(None));
        let env = |k: &str| (k == MECHANISM_ENV).then(|| "profile".into());
        assert_eq!(
            super::forced_mechanism(&o, &env),
            Ok(Some(Mechanism::Profile))
        );
        let old = |k: &str| (k == "XSHELLD_SERVICE_MECHANISM").then(|| "none".into());
        assert_eq!(super::forced_mechanism(&o, &old), Ok(Some(Mechanism::None)));
        let bad = |k: &str| (k == MECHANISM_ENV).then(|| "launchd".into());
        assert!(super::forced_mechanism(&o, &bad).is_err());
        let flag = ServiceOpts {
            mechanism: Some(Mechanism::Cron),
            ..Default::default()
        };
        assert_eq!(
            super::forced_mechanism(&flag, &env),
            Ok(Some(Mechanism::Cron))
        );
        // Forced: only that mechanism is applied, and the others are never touched.
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        let f = Fake::default();
        let r = install(&l, &f, &P(true, true), Some(Mechanism::Profile), None).unwrap();
        assert_eq!(r.mechanism, "profile");
        assert!(
            f.cmds().iter().all(|c| c.starts_with("write ")),
            "{:?}",
            f.cmds()
        );
        let f = Fake::default();
        let r = install(&l, &f, &P(true, true), Some(Mechanism::None), None).unwrap();
        assert_eq!(r.mechanism, "none");
        assert!(f.cmds().is_empty(), "{:?}", f.cmds());
    }

    #[test]
    fn install_removes_other_mechanisms() {
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        let f = Fake::default();
        // Earlier entries of cron and the profile.
        *f.crontab.borrow_mut() = Some(format!("0 1 * * * x\n{}\n", cron_line(&l)));
        let prof = l.home.join(".profile");
        f.files
            .borrow_mut()
            .insert(prof.clone(), profile_merge("a\n", &profile_block(&l)));
        let r = install(&l, &f, &P(true, true), None, None).unwrap();
        assert_eq!(r.mechanism, "systemd");
        assert_eq!(r.installed, vec!["systemd"]);
        assert_eq!(r.changed, Some(true));
        assert_eq!(r.linger, Some(true));
        assert!(r.active);
        let cmds = f.cmds();
        for want in [
            format!("write {}", l.unit().display()),
            "systemctl --user daemon-reload".into(),
            "systemctl --user enable xshelld.service".into(),
            "systemctl --user start xshelld.service".into(),
            "loginctl --no-ask-password enable-linger".into(),
        ] {
            assert!(cmds.contains(&want), "{want} missing from {cmds:?}");
        }
        assert_eq!(f.crontab.borrow().as_deref(), Some("0 1 * * * x\n"));
        assert_eq!(f.files.borrow()[&prof], "a\n");
        // Again: nothing to change.
        let f2 = Fake {
            files: RefCell::new(f.files.borrow().clone()),
            crontab: RefCell::new(f.crontab.borrow().clone()),
            ..Default::default()
        };
        let r = install(&l, &f2, &P(true, true), None, None).unwrap();
        assert_eq!(r.changed, Some(false));
        // Linger refused: a warning, not a failure.
        let f3 = Fake::default();
        f3.fail
            .borrow_mut()
            .push("loginctl --no-ask-password".into());
        let r = install(&l, &f3, &P(true, false), None, None).unwrap();
        assert!(r.warnings.iter().any(|w| w.contains("linger")), "{r:?}");
        // Uninstall: unit disabled (not stopped) and removed, launcher gone.
        uninstall(&l, &f, None).unwrap();
        assert!(!f.files.borrow().contains_key(&l.unit()));
        assert!(f
            .cmds()
            .contains(&"systemctl --user disable xshelld.service".into()));
        assert!(!f.cmds().iter().any(|c| c.contains(" stop ")));
        assert!(!l.launcher().exists());
    }

    #[test]
    fn launcher_copy_atomic_private_skips_identical() {
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        assert!(write_launcher(&l).unwrap());
        let m = fs::metadata(l.launcher()).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o700);
        let dir = fs::metadata(l.launcher().parent().unwrap()).unwrap();
        assert_eq!(dir.permissions().mode() & 0o777, 0o700);
        assert!(!write_launcher(&l).unwrap(), "identical: unchanged");
        // Edited or loosened: rewritten.
        fs::write(l.launcher(), "#!/bin/sh\nexit 0\n").unwrap();
        assert!(write_launcher(&l).unwrap());
        fs::set_permissions(l.launcher(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(write_launcher(&l).unwrap());
        assert_eq!(
            fs::read_to_string(l.launcher()).unwrap(),
            launcher_text(&l.paths.server_root)
        );
        // No temp files left.
        let left: Vec<_> = fs::read_dir(l.launcher().parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("xshelld")]);
    }

    /// The launcher runs the selected build, else the newest installed one.
    #[test]
    fn run_target_selected_else_self() {
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        write_launcher(&l).unwrap();
        let root = &l.paths.server_root;
        let mk = |name: &str| {
            let d = root.join(name);
            fs::create_dir_all(&d).unwrap();
            let f = d.join("xshelld");
            fs::write(&f, format!("#!/bin/sh\necho {name} \"$@\"\n")).unwrap();
            fs::set_permissions(&f, fs::Permissions::from_mode(0o700)).unwrap();
        };
        let out = || {
            let o = Command::new("/bin/sh")
                .arg(l.launcher())
                .arg("serve")
                .output()
                .unwrap();
            (
                o.status.code(),
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
            )
        };
        // Nothing installed.
        assert_eq!(out().0, Some(127));
        mk("1.5.0+aaa");
        std::thread::sleep(Duration::from_millis(20));
        mk("1.5.0+bbb");
        // The newest.
        assert_eq!(out(), (Some(0), "1.5.0+bbb serve".into()));
        // The selected record wins.
        fs::write(root.join("selected"), "1.5.0+aaa\n").unwrap();
        assert_eq!(out(), (Some(0), "1.5.0+aaa serve".into()));
        // Arguments pass through unchanged.
        let o = Command::new("/bin/sh")
            .arg(l.launcher())
            .args(["a b", "--x=1"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&o.stdout).trim(),
            "1.5.0+aaa a b --x=1"
        );
        // A bad record is an error, never another build (only a missing record falls back).
        fs::write(root.join("selected"), "../x\n").unwrap();
        assert_eq!(out().0, Some(127));
        fs::write(root.join("selected"), "1.5.0+missing\n").unwrap();
        assert_eq!(out().0, Some(127));
        // The Rust decision agrees on every case.
        fs::remove_file(root.join("selected")).unwrap();
        let rust = || xshell_hostlink::local::start_binary(&l.home);
        assert_eq!(rust(), Ok(Some(root.join("1.5.0+bbb/xshelld"))));
        fs::write(root.join("selected"), "1.5.0+aaa\n").unwrap();
        fs::set_permissions(root.join("selected"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(rust(), Ok(Some(root.join("1.5.0+aaa/xshelld"))));
        fs::write(root.join("selected"), "1.5.0+missing\n").unwrap();
        assert!(rust().is_err());
    }

    #[test]
    fn status_reports_and_text() {
        let t = tempfile::tempdir().unwrap();
        let l = layout(t.path());
        let f = Fake::default();
        let r = status(&l, &f, Some(Mechanism::Profile));
        assert_eq!(r.mechanism, "profile");
        assert!(r.installed.is_empty());
        assert!(!r.active);
        assert!(!r.running);
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("changed").is_none());
        assert_eq!(json["linger"], serde_json::Value::Null);
        install(&l, &f, &P(false, false), Some(Mechanism::Profile), None).unwrap();
        let r = status(&l, &f, Some(Mechanism::Profile));
        assert_eq!(r.installed, vec!["profile"]);
        assert!(r.active);
        assert!(r.text().contains("shell profile"), "{}", r.text());
    }
}

#![cfg(windows)]
//! The GUI-bound Daemon on Windows (#24) as real processes: the named pipe, ConPTY
//! Terminals in their Job Objects, the ways it ends (the stop event, the parent's exit, the
//! Desktop's job), restore, Agent Status hooks over the pipe, and the hostlink dialers.
//!
//! Fake agents are `.cmd` files on the Daemon's PATH; `ping -n 600 127.0.0.1` is the sleeper.
//! A test that needs a parent other than the test process re-runs this binary as
//! [`helper_gui_parent`].

mod win;

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use win::*;
use xshell_core::launch::LaunchSpec;
use xshell_hostlink::{
    CancelToken, Dialer, LocalDaemon, LocalDaemonConfig, LocalDialer, NamedPipeDialer,
};
use xshell_protocol::msg::{AgentStatus, ClientMsg, ServerMsg};

/// A unique word a fake agent prints, so a test matches its output and not argv text.
fn nonce() -> String {
    format!("NONCE{}", &Uuid::new_v4().simple().to_string()[..12])
}

/// PowerShell starting `ping` detached (a console of its own, so only the Job reaches it)
/// and writing its pid to `pidfile`.
fn detached_ping(pidfile: &Path) -> String {
    format!(
        "powershell -NoProfile -NonInteractive -Command \"(Start-Process -FilePath ping \
         -ArgumentList '-n','600','127.0.0.1' -WindowStyle Hidden -PassThru).Id | \
         Out-File -Encoding ascii '{}'\"",
        pidfile.display()
    )
}

const SLEEP: &str = "ping -n 600 127.0.0.1 >NUL";

/// A fake `claude` that prints `word`, starts a detached `ping` (pid in `pidfile`), then
/// sleeps.
fn claude_with_descendant(h: &TestHome, word: &str, pidfile: &Path) {
    h.script(
        "claude",
        &format!("echo {word}\n{}\n{SLEEP}", detached_ping(pidfile)),
    );
}

fn exit_code(c: &mut Client, t: Uuid) -> i32 {
    match c.expect_msg(
        "term.exit",
        |m| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == t),
    ) {
        ServerMsg::TermExit { code, .. } => code,
        _ => unreachable!(),
    }
}

// ── The parent stand-in ───────────────────────────────────────────────────

/// Not a test unless `XSHELLD_WIN_HELPER` is set: then it is a Desktop stand-in. `job`
/// starts the Daemon through hostlink's `LocalDaemon` (inside its Job Object); `bare`
/// starts `serve --gui-bound` itself, with no job. It writes the Daemon's pid to
/// `$XSHELLD_WIN_HELPER_DIR\daemon.pid` and sleeps until killed.
#[test]
fn helper_gui_parent() {
    let Ok(mode) = std::env::var("XSHELLD_WIN_HELPER") else {
        return;
    };
    let dir = std::path::PathBuf::from(std::env::var_os("XSHELLD_WIN_HELPER_DIR").unwrap());
    let env = |k: &str| std::env::var_os(k).unwrap();
    let pid = match mode.as_str() {
        "job" => {
            let d = LocalDaemon::new(LocalDaemonConfig {
                bin: bin().into(),
                env: vec![],
                home: env("XSHELLD_HOME").into(),
                socket: env("XSHELLD_SOCKET").into(),
                version: "1.5.0".into(),
            })
            .unwrap();
            let pid = d.ensure_spawned().unwrap();
            // Kept alive (and with it the job) until this process is killed.
            std::mem::forget(d);
            pid
        }
        _ => {
            let c = Command::new(bin())
                .args(["serve", "--gui-bound", "--parent-pid"])
                .arg(std::process::id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(DAEMON_FLAGS)
                .spawn()
                .unwrap();
            let pid = c.id();
            std::mem::forget(c);
            pid
        }
    };
    fs::write(dir.join("daemon.pid"), pid.to_string()).unwrap();
    std::thread::sleep(Duration::from_secs(600));
}

/// A running [`helper_gui_parent`] and the pid of the Daemon it started.
struct Helper {
    child: std::process::Child,
    daemon: u32,
}

impl Helper {
    fn start(h: &TestHome, mode: &str, extra: &[(&str, &str)]) -> Helper {
        let dir = h.dir.path().join(format!("helper-{mode}"));
        fs::create_dir_all(&dir).unwrap();
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args([
            "--exact",
            "helper_gui_parent",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("XSHELLD_WIN_HELPER", mode)
        .env("XSHELLD_WIN_HELPER_DIR", &dir)
        .env("XSHELLD_HOME", h.home())
        .env("XSHELLD_SOCKET", &h.pipe)
        .env("PATH", h.path_env())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DAEMON_FLAGS);
        for (k, v) in extra {
            c.env(k, v);
        }
        let child = c.spawn().unwrap();
        let daemon = read_pid_file(&dir.join("daemon.pid"));
        Helper { child, daemon }
    }

    /// Kill the stand-in as a crash would (no cleanup of its own).
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.kill();
        if !wait_dead(self.daemon, Duration::from_secs(15)) {
            kill_pid(self.daemon);
        }
    }
}

// ── The pipe and ConPTY ───────────────────────────────────────────────────

#[test]
fn pipe_serves_hello_and_terminals() {
    let h = TestHome::new();
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    let (hello, list) = c.hello();
    assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
    assert!(list.is_empty());
    // A second Desktop at the same time.
    let mut c2 = Client::connect(&h.pipe);
    assert!(c2.hello().1.is_empty());
}

// ── First messages (#37) ──────────────────────────────────────────────────

const SID: &str = "11111111-2222-3333-4444-555555555555";

/// The cmd-shim npm writes for a package's bin, running `<rel>` (relative to the shim's
/// directory) with node. Verbatim npm `cmd-shim` output apart from the script path.
fn npm_shim(rel: &str) -> String {
    format!(
        "@ECHO off\r\n\
         GOTO start\r\n\
         :find_dp0\r\n\
         SET dp0=%~dp0\r\n\
         EXIT /b\r\n\
         :start\r\n\
         SETLOCAL\r\n\
         CALL :find_dp0\r\n\
         \r\n\
         IF EXIST \"%dp0%\\node.exe\" (\r\n\
         \x20 SET \"_prog=%dp0%\\node.exe\"\r\n\
         ) ELSE (\r\n\
         \x20 SET \"_prog=node\"\r\n\
         \x20 SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
         )\r\n\
         \r\n\
         endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\{rel}\" %*\r\n"
    )
}

/// A fake npm-installed agent `bin\<name>.cmd`: its script writes its argv (after node and
/// the script) as JSON to `%FAKE_ARGV%`, prints `%FAKE_NONCE%`, then idles. node.exe comes
/// from PATH (CI installs Node).
fn npm_agent(h: &TestHome, name: &str) {
    let rel = format!("node_modules\\fake-{name}\\cli.js");
    let js = h.bin_dir().join(&rel);
    fs::create_dir_all(js.parent().unwrap()).unwrap();
    fs::write(
        &js,
        "const fs = require('fs');\n\
         fs.writeFileSync(process.env.FAKE_ARGV, JSON.stringify(process.argv.slice(2)));\n\
         console.log(process.env.FAKE_NONCE);\n\
         setInterval(() => {}, 1 << 30);\n",
    )
    .unwrap();
    fs::write(h.bin_dir().join(format!("{name}.cmd")), npm_shim(&rel)).unwrap();
}

/// The Daemon environment the fake npm agents report through.
fn fake_env(argv: &Path, word: &str) -> Vec<(String, String)> {
    vec![
        ("FAKE_ARGV".into(), argv.to_string_lossy().into_owned()),
        ("FAKE_NONCE".into(), word.into()),
    ]
}

fn start_fake(h: &TestHome, argv: &Path, word: &str, extra: &[(&str, &str)]) -> Daemon {
    let env = fake_env(argv, word);
    let mut all: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    all.extend_from_slice(extra);
    Daemon::start_with(h, &all)
}

fn open_first(
    c: &mut Client,
    t: Uuid,
    launch: LaunchSpec,
    msg: &str,
) -> Result<serde_json::Value, String> {
    let ClientMsg::TermOpen { mut spec } = open_msg(t, launch) else {
        unreachable!()
    };
    spec.first_message = Some(msg.into());
    c.request(&ClientMsg::TermOpen { spec })
}

/// The argv a fake npm agent wrote, once it is complete JSON.
fn read_argv(p: &Path) -> Vec<String> {
    let mut got = None;
    assert!(
        wait_until(T, || {
            got = fs::read(p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok());
            got.is_some()
        }),
        "{} was never written",
        p.display()
    );
    got.unwrap()
}

/// Every character `cmd.exe` or a batch file would act on, a CRLF and an LF, non-ASCII text,
/// and a command that would create `canary` if anything parsed the message.
fn hostile_message(canary: &Path) -> String {
    format!(
        "a & b | c < d > e ^ f %PATH% !x! \"q\" 'y' \\\\tail\\\\ \r\nline2\nline3 é 🚀 & echo pwned > {}",
        canary.display()
    )
}

fn claude_new_chat(cwd: &Path) -> LaunchSpec {
    LaunchSpec {
        session_id: Some(SID.into()),
        ..claude_spec(cwd)
    }
}

#[test]
fn first_message_advertised_on_windows() {
    let h = TestHome::new();
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    let (hello, _) = c.hello();
    assert!(
        hello.capabilities.iter().any(|c| c == "term.first-message"),
        "{:?}",
        hello.capabilities
    );
}

/// Acceptance: the message reaches an npm-installed Claude Code as one argv word, byte for
/// byte, and nothing parses it.
#[test]
fn first_message_reaches_npm_claude_unchanged() {
    let h = TestHome::new();
    npm_agent(&h, "claude");
    let (argv, canary, word) = (
        h.dir.path().join("argv.json"),
        h.dir.path().join("canary.txt"),
        nonce(),
    );
    let _d = start_fake(&h, &argv, &word, &[]);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let msg = hostile_message(&canary);
    let t = Uuid::new_v4();
    open_first(&mut c, t, claude_new_chat(&h.project("p")), &msg).unwrap();
    c.attach(t);
    c.output_until(t, &word);
    let got = read_argv(&argv);
    assert_eq!(got.len(), 6, "{got:?}");
    assert_eq!(got[..3], ["--session-id", SID, "--settings"], "{got:?}");
    assert!(got[3].ends_with("claude-hooks.json"), "{got:?}");
    assert_eq!(got[4..], ["--".to_string(), msg], "{got:?}");
    assert!(!canary.exists());
}

/// The same for Codex, whose `-c` hook overrides carry quotes of their own.
#[test]
fn first_message_reaches_npm_codex_unchanged() {
    let h = TestHome::new();
    npm_agent(&h, "codex");
    let (argv, canary, word) = (
        h.dir.path().join("argv.json"),
        h.dir.path().join("canary.txt"),
        nonce(),
    );
    let _d = start_fake(&h, &argv, &word, &[]);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let msg = hostile_message(&canary);
    let t = Uuid::new_v4();
    let codex = LaunchSpec {
        agent: Some("codex".into()),
        ..claude_spec(&h.project("p"))
    };
    open_first(&mut c, t, codex, &msg).unwrap();
    c.attach(t);
    c.output_until(t, &word);
    let got = read_argv(&argv);
    assert_eq!(got.len(), 10, "{got:?}");
    // The notify command names the Daemon's own executable: read it back as Codex would.
    let notify: toml::Table = toml::from_str(&got[1]).unwrap();
    let exe = notify["notify"][0].as_str().unwrap().to_string();
    let want = xshell_core::agent_status::AgentHooks {
        exe: exe.into(),
        endpoint: String::new(),
        claude_settings: Default::default(),
    }
    .codex_overrides();
    assert_eq!(got[..8], want[..], "{got:?}");
    assert_eq!(got[8..], ["--".to_string(), msg], "{got:?}");
    assert!(!canary.exists());
}

/// A hand-written batch file can only run through `cmd.exe`: refused, nothing starts.
#[test]
fn first_message_refused_for_unresolvable_agent() {
    let h = TestHome::new();
    let canary = h.dir.path().join("ran.txt");
    h.script(
        "claude",
        &format!("echo ran> \"{}\"\n{SLEEP}", canary.display()),
    );
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let e = open_first(
        &mut c,
        Uuid::new_v4(),
        claude_new_chat(&h.project("p")),
        "hello & echo pwned",
    )
    .unwrap_err();
    assert_eq!(
        e,
        "a first message on Windows needs claude installed as an .exe or an npm package"
    );
    assert!(Client::connect(&h.pipe).hello().1.is_empty());
    assert!(h.state_ids().is_empty());
    std::thread::sleep(Duration::from_millis(500));
    assert!(!canary.exists());
}

/// Neither the Project directory (`cmd.exe` searches it first) nor PATH entries that would
/// resolve against it are searched: the shim on PATH runs.
#[test]
fn first_message_ignores_cwd_shadow() {
    let h = TestHome::new();
    npm_agent(&h, "claude");
    let p = h.project("p");
    let canary = h.dir.path().join("shadow.txt");
    fs::write(
        p.join("claude.cmd"),
        format!("@echo off\r\necho shadow> \"{}\"\r\n", canary.display()),
    )
    .unwrap();
    fs::create_dir_all(p.join("tools")).unwrap();
    fs::copy(bin(), p.join("tools").join("claude.exe")).unwrap();
    let path = format!("tools;.\\tools;;\\tools;C:tools;{}", h.path_env());
    let (argv, word) = (h.dir.path().join("argv.json"), nonce());
    let _d = start_fake(&h, &argv, &word, &[("PATH", &path)]);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    open_first(&mut c, t, claude_new_chat(&p), "hi there").unwrap();
    c.attach(t);
    c.output_until(t, &word);
    assert_eq!(read_argv(&argv)[4..], ["--", "hi there"]);
    assert!(!canary.exists());
}

/// The message is in no list or state file, and a restore runs the plain `cmd.exe /C`
/// launch without it.
#[test]
fn first_message_not_persisted_or_relaunched() {
    let h = TestHome::new();
    npm_agent(&h, "claude");
    let (argv, word) = (h.dir.path().join("argv.json"), nonce());
    let mut d = start_fake(&h, &argv, &word, &[]);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let secret = nonce();
    let msg = format!("remember {secret}");
    let t = Uuid::new_v4();
    open_first(&mut c, t, claude_new_chat(&h.project("p")), &msg).unwrap();
    c.attach(t);
    c.output_until(t, &word);
    assert_eq!(read_argv(&argv)[4..], ["--".to_string(), msg]);
    let state = h
        .home()
        .join(".xshell")
        .join("daemon")
        .join("terminals.json");
    assert!(!fs::read_to_string(&state).unwrap().contains(&secret));
    let (_, list) = Client::connect(&h.pipe).hello();
    assert!(!format!("{list:?}").contains(&secret));
    d.stop();
    assert_eq!(d.wait_exit(T), Some(0));
    drop(d);

    let (argv2, word2) = (h.dir.path().join("argv2.json"), nonce());
    let _d = start_fake(&h, &argv2, &word2, &[]);
    let mut c = Client::connect(&h.pipe);
    let (_, list) = c.hello();
    assert!(list.iter().any(|i| i.terminal == t), "{list:?}");
    let got = read_argv(&argv2);
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[..3], ["--session-id", SID, "--settings"], "{got:?}");
    assert!(!format!("{got:?}").contains(&secret));
}

/// A dangling `claude.exe` is no match, and its batch sibling is refused: neither runs.
#[test]
fn first_message_dangling_exe_never_runs_batch_sibling() {
    let h = TestHome::new();
    let canary = h.dir.path().join("batch.txt");
    h.script("claude", &format!("echo batch> \"{}\"", canary.display()));
    let link = h.bin_dir().join("claude.exe");
    if let Err(e) = std::os::windows::fs::symlink_file(h.dir.path().join("gone.exe"), &link) {
        eprintln!("skipped: no symlink privilege here: {e}");
        return;
    }
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let e = open_first(
        &mut c,
        Uuid::new_v4(),
        claude_new_chat(&h.project("p")),
        "hello & echo pwned",
    )
    .unwrap_err();
    assert_eq!(
        e,
        "a first message on Windows needs claude installed as an .exe or an npm package"
    );
    assert!(Client::connect(&h.pipe).hello().1.is_empty());
    std::thread::sleep(Duration::from_millis(500));
    assert!(!canary.exists());
}

/// A `claude.com` first match is refused: the launcher would run `claude.com.exe` instead.
#[test]
fn first_message_refuses_com() {
    let h = TestHome::new();
    fs::copy(bin(), h.bin_dir().join("claude.com")).unwrap();
    fs::copy(bin(), h.bin_dir().join("claude.com.exe")).unwrap();
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let e = open_first(
        &mut c,
        Uuid::new_v4(),
        claude_new_chat(&h.project("p")),
        "hi there",
    )
    .unwrap_err();
    assert_eq!(
        e,
        "a first message on Windows needs claude installed as an .exe or an npm package"
    );
    assert!(Client::connect(&h.pipe).hello().1.is_empty());
    assert!(h.state_ids().is_empty());
}

/// A3: a message whose command line would not fit CreateProcess's limit is refused before
/// anything starts; a maximum-size plain message fits and arrives whole.
#[test]
fn first_message_command_line_budget() {
    let h = TestHome::new();
    npm_agent(&h, "claude");
    let (argv, word) = (h.dir.path().join("argv.json"), nonce());
    let _d = start_fake(&h, &argv, &word, &[]);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let max = xshell_core::FIRST_MESSAGE_MAX_BYTES;
    let quotes = format!("{} ", "\"".repeat(max - 1));
    let e = open_first(
        &mut c,
        Uuid::new_v4(),
        claude_new_chat(&h.project("p")),
        &quotes,
    )
    .unwrap_err();
    assert_eq!(e, "a first message is too long for a Windows host");
    assert!(Client::connect(&h.pipe).hello().1.is_empty());
    assert!(!argv.exists());

    let plain = "x ".repeat(max / 2);
    let t = Uuid::new_v4();
    open_first(&mut c, t, claude_new_chat(&h.project("p")), &plain).unwrap();
    c.attach(t);
    c.output_until(t, &word);
    assert_eq!(read_argv(&argv)[4..], ["--".to_string(), plain]);
}

#[test]
fn open_attach_input_output_over_conpty() {
    let h = TestHome::new();
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    let pid = c.open(t, cmd_spec(&h.project("p")));
    assert!(alive(pid));
    c.attach(t);
    c.marker(t, 4242);
    // A shell that exits by itself: its exit code arrives (the console closes after the exit).
    c.input(t, "exit 3\r");
    assert_eq!(exit_code(&mut c, t), 3);
    assert!(wait_dead(pid, T));
}

/// M2: a process the agent starts at once, detached from its console, is still in the
/// Terminal's job, so closing the Terminal ends it.
#[test]
fn close_ends_terminal_and_detached_grandchild() {
    let h = TestHome::new();
    let word = nonce();
    let pidfile = h.dir.path().join("grandchild.pid");
    claude_with_descendant(&h, &word, &pidfile);
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    let pid = c.open(t, claude_spec(&h.project("p")));
    c.attach(t);
    c.output_until(t, &word);
    let gp = read_pid_file(&pidfile);
    let _reap = PidReaper(vec![gp]);
    assert!(alive(gp));
    c.close(t);
    c.terminals_where(|l| l.iter().all(|i| i.terminal != t));
    assert!(wait_dead(pid, T), "the agent survived term.close");
    assert!(
        wait_dead(gp, T),
        "the detached grandchild survived term.close"
    );
}

/// M3: the Terminal gets the PATH the Daemon was given, not the registry's.
#[test]
fn terminals_inherit_the_daemons_path() {
    let h = TestHome::new();
    let word = nonce();
    h.script("xshell-probe", &format!("echo {word}"));
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    c.open(t, cmd_spec(&h.project("p")));
    c.attach(t);
    c.input(t, "xshell-probe\r");
    c.output_until(t, &word);
}

/// A start that fails after the console exists (here: the launcher is missing) neither
/// blocks nor leaves anything behind: the Daemon keeps answering.
#[test]
fn failed_start_keeps_the_daemon_responsive() {
    let h = TestHome::new();
    let missing = h.dir.path().join("missing").join("xshelld.exe");
    let mut d = Daemon::start_with(
        &h,
        &[("XSHELLD_TEST_JOB_LAUNCHER", &missing.to_string_lossy())],
    );
    let mut c = Client::connect(&h.pipe);
    c.hello();
    for _ in 0..3 {
        let start = Instant::now();
        let e = c
            .request(&open_msg(Uuid::new_v4(), cmd_spec(&h.project("p"))))
            .unwrap_err();
        assert!(e.contains("failed to start"), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }
    let mut c2 = Client::connect(&h.pipe);
    let (_, list) = c2.hello();
    assert!(list.is_empty(), "{list:?}");
    d.stop();
    assert_eq!(d.wait_exit(Duration::from_secs(15)), Some(0));
}

/// When the agent exits by itself, what it left in its job (a detached descendant) ends
/// after the grace; the exit code is kept.
#[test]
fn agent_exit_ends_its_detached_descendant() {
    let h = TestHome::new();
    let word = nonce();
    let pidfile = h.dir.path().join("grandchild.pid");
    h.script(
        "claude",
        &format!("echo {word}\n{}\nexit /b 4", detached_ping(&pidfile)),
    );
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    c.open(t, claude_spec(&h.project("p")));
    c.attach(t);
    assert_eq!(exit_code(&mut c, t), 4);
    let gp = read_pid_file(&pidfile);
    let _reap = PidReaper(vec![gp]);
    assert!(wait_dead(gp, T), "the descendant outlived the agent's exit");
    let list = c.terminals_where(|l| l.iter().any(|i| i.terminal == t && i.exit_code == Some(4)));
    assert!(!list.is_empty());
}

/// Killing only the launcher (`xshelld job-exec`) still ends the agent's descendants.
#[test]
fn killing_job_exec_ends_descendant() {
    let h = TestHome::new();
    let word = nonce();
    let pidfile = h.dir.path().join("grandchild.pid");
    claude_with_descendant(&h, &word, &pidfile);
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    let pid = c.open(t, claude_spec(&h.project("p")));
    c.attach(t);
    c.output_until(t, &word);
    let gp = read_pid_file(&pidfile);
    let _reap = PidReaper(vec![gp]);
    kill_pid(pid);
    exit_code(&mut c, t);
    assert!(wait_dead(gp, T), "the descendant outlived its launcher");
}

// ── How it ends ───────────────────────────────────────────────────────────

/// The Desktop's quit: the stop event ends the Daemon in order, within the bound; its
/// Terminals end, their state stays, the pipe and the lock are released.
#[test]
fn stop_event_shutdown_is_bounded() {
    let h = TestHome::new();
    let word = nonce();
    let pidfile = h.dir.path().join("grandchild.pid");
    claude_with_descendant(&h, &word, &pidfile);
    let mut d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    let pid = c.open(t, claude_spec(&h.project("p")));
    c.attach(t);
    c.output_until(t, &word);
    let gp = read_pid_file(&pidfile);
    let _reap = PidReaper(vec![gp]);
    let start = Instant::now();
    d.stop();
    assert_eq!(d.wait_exit(Duration::from_secs(15)), Some(0));
    assert!(
        start.elapsed() < Duration::from_secs(12),
        "{:?}",
        start.elapsed()
    );
    assert!(wait_dead(pid, Duration::from_secs(5)));
    assert!(wait_dead(gp, Duration::from_secs(5)));
    c.expect_eof();
    assert_eq!(h.state_ids(), vec![t], "the state file keeps the Terminal");
    let e = xshell_core::pipe::connect(&h.pipe, Instant::now() + T, &|| false).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
    // The lock is free: the next start serves (and restores the Terminal).
    let _d2 = Daemon::start(&h);
    let mut c2 = Client::connect(&h.pipe);
    let (_, list) = c2.hello();
    assert!(list.iter().any(|i| i.terminal == t), "{list:?}");
}

#[test]
fn restores_terminals_on_next_gui_bound_start() {
    let h = TestHome::new();
    let mut d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    let old = c.open(t, cmd_spec(&h.project("p")));
    d.stop();
    assert_eq!(d.wait_exit(T), Some(0));
    assert!(wait_dead(old, Duration::from_secs(5)));
    drop(d);
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    let (_, list) = c.hello();
    let info = list.iter().find(|i| i.terminal == t).expect("restored");
    let pid = info.pid.expect("relaunched");
    assert_ne!(pid, old);
    assert!(alive(pid));
    c.attach(t);
    c.marker(t, 777);
}

#[test]
fn ends_when_parent_killed() {
    let h = TestHome::new();
    let mut p = Helper::start(&h, "bare", &[]);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    let pid = c.open(t, cmd_spec(&h.project("p")));
    c.attach(t);
    c.marker(t, 31);
    p.kill();
    assert!(wait_dead(p.daemon, T), "the Daemon outlived its parent");
    assert!(wait_dead(pid, Duration::from_secs(5)));
    assert!(
        h.log_text().contains("ended: shutting down"),
        "{}",
        h.log_text()
    );
    assert_eq!(h.state_ids(), vec![t], "an orderly exit keeps the state");
}

/// AC2: the Desktop dies (killed, no cleanup) and its parent watch is off: its Job Object
/// alone ends the Daemon, the agent and the agent's detached descendant.
#[test]
fn desktop_killed_job_ends_daemon_and_agents() {
    let h = TestHome::new();
    let word = nonce();
    let pidfile = h.dir.path().join("grandchild.pid");
    claude_with_descendant(&h, &word, &pidfile);
    let mut p = Helper::start(&h, "job", &[("XSHELLD_TEST_NO_PARENT_WATCH", "1")]);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    let pid = c.open(t, claude_spec(&h.project("p")));
    c.attach(t);
    c.output_until(t, &word);
    let gp = read_pid_file(&pidfile);
    let _reap = PidReaper(vec![gp, pid]);
    assert!(h.log_text().contains("parent watch disabled"));
    p.kill();
    assert!(
        wait_dead(p.daemon, T),
        "the Daemon outlived the Desktop's job"
    );
    assert!(wait_dead(pid, T), "the agent outlived the Desktop's job");
    assert!(
        wait_dead(gp, T),
        "the descendant outlived the Desktop's job"
    );
}

// ── Starting ──────────────────────────────────────────────────────────────

#[test]
fn second_serve_loses_lock_exit_3() {
    let h = TestHome::new();
    let _d = Daemon::start(&h);
    Client::connect(&h.pipe).hello();
    let mut c = h.cmd();
    c.env("XSHELLD_SOCKET", unique_pipe())
        .args(["serve", "--gui-bound", "--parent-pid"])
        .arg(std::process::id().to_string());
    assert_eq!(c.status().unwrap().code(), Some(3));
}

#[test]
fn squatted_pipe_name_fails_start() {
    let h = TestHome::new();
    let _squatter = xshell_core::pipe::PipeListener::bind(&h.pipe).unwrap();
    let mut d = Daemon::start(&h);
    assert_eq!(d.wait_exit(T), Some(1));
    assert!(h.log_text().contains("exists already"), "{}", h.log_text());
}

#[test]
fn persistent_serve_refused_on_windows() {
    let h = TestHome::new();
    assert_eq!(h.cmd().arg("serve").status().unwrap().code(), Some(2));
    assert_eq!(h.cmd().arg("connect").status().unwrap().code(), Some(1));
    assert!(xshell_core::pipe::connect(&h.pipe, Instant::now() + T, &|| false).is_err());
}

#[test]
fn gui_bound_writes_mode_marker_and_refuses_upgrade() {
    let h = TestHome::new();
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    assert_eq!(h.mode().as_deref(), Some("gui-bound"));
    let e = c.request(&ClientMsg::DaemonUpgrade).unwrap_err();
    assert!(e.contains("update xshell there"), "{e}");
}

#[test]
fn refuses_foreign_parent_pid() {
    let h = TestHome::new();
    let mut other = Command::new("ping")
        .args(["-n", "600", "127.0.0.1"])
        .stdout(Stdio::null())
        .creation_flags(DAEMON_FLAGS)
        .spawn()
        .unwrap();
    let mut c = h.cmd();
    c.args(["serve", "--gui-bound", "--parent-pid"])
        .arg(other.id().to_string());
    assert_eq!(c.status().unwrap().code(), Some(1));
    let _ = other.kill();
    let _ = other.wait();
}

// ── Agent Status over the pipe (#5) ───────────────────────────────────────

/// A fake Claude Code reports needs-you through the real hook client over the Daemon's
/// pipe, then exits by itself: ended, with its exit code.
#[test]
fn agent_hooks_report_status_over_pipe() {
    let h = TestHome::new();
    let word = nonce();
    let dir = h.dir.path();
    let go = dir.join("go");
    h.script(
        "claude",
        &format!(
            "echo %XSHELL_EVENT_SOCKET%> \"{sock}\"\n\
             echo %*> \"{args}\"\n\
             \"{bin}\" event - needs-you\n\
             echo {word}\n\
             :wait\n\
             if exist \"{go}\" goto done\n\
             ping -n 2 127.0.0.1 >NUL\n\
             goto wait\n\
             :done\n\
             exit /b 5",
            sock = dir.join("sock.txt").display(),
            args = dir.join("args.txt").display(),
            bin = bin(),
            go = go.display(),
        ),
    );
    let _d = Daemon::start(&h);
    let mut c = Client::connect(&h.pipe);
    c.hello();
    let t = Uuid::new_v4();
    c.open(t, claude_spec(&h.project("p")));
    c.attach(t);
    c.output_until(t, &word);
    c.terminals_where(|l| {
        l.iter()
            .any(|i| i.terminal == t && i.agent_status == Some(AgentStatus::NeedsYou))
    });
    let sock = fs::read_to_string(dir.join("sock.txt")).unwrap();
    assert_eq!(sock.trim(), h.pipe.display().to_string());
    let args = fs::read_to_string(dir.join("args.txt")).unwrap();
    assert!(
        args.contains("--settings") && args.contains("claude-hooks.json"),
        "{args}"
    );
    fs::write(&go, "").unwrap();
    assert_eq!(exit_code(&mut c, t), 5);
    c.terminals_where(|l| {
        l.iter()
            .any(|i| i.terminal == t && i.agent_status == Some(AgentStatus::Ended))
    });
}

#[test]
fn event_client_unknown_terminal_exit_zero() {
    let h = TestHome::new();
    let _d = Daemon::start(&h);
    Client::connect(&h.pipe).hello();
    let start = Instant::now();
    let out = Command::new(bin())
        .args([
            "event",
            &Uuid::new_v4().to_string(),
            "working",
            "-v",
            "--socket",
        ])
        .arg(&h.pipe)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown terminal"), "{err}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    // Nobody on the pipe: still 0, at once.
    let start = Instant::now();
    let st = Command::new(bin())
        .args(["event", &Uuid::new_v4().to_string(), "working", "--socket"])
        .arg(unique_pipe())
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(0));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

// ── The Desktop's side: hostlink ──────────────────────────────────────────

/// AC3: the Local Host's dialer starts the GUI-bound Daemon (in its job); a second Desktop
/// on the same pipe sees its Terminal and its output. AC2: ending it ends the Terminals and
/// the pipe.
#[test]
fn local_dialer_spawns_gui_bound_and_second_desktop_sees_terminal() {
    let h = TestHome::new();
    let daemon = Arc::new(
        LocalDaemon::new(LocalDaemonConfig {
            bin: bin().into(),
            env: h.daemon_env(),
            home: h.home(),
            socket: h.pipe.clone(),
            version: "1.5.0".into(),
        })
        .unwrap(),
    );
    let cancel = CancelToken::new();
    let a = LocalDialer {
        daemon: daemon.clone(),
    }
    .dial(&cancel)
    .unwrap_or_else(|e| panic!("{e:?}"));
    let _a_conn = a.conn;
    let mut ca = Client::from_io(a.io.read, a.io.write);
    ca.hello();
    assert!(daemon.gui_pid().is_some());
    let t = Uuid::new_v4();
    let pid = ca.open(t, cmd_spec(&h.project("p")));
    ca.attach(t);
    ca.marker(t, 55);

    let b = NamedPipeDialer {
        name: h.pipe.clone(),
    }
    .dial(&cancel)
    .unwrap_or_else(|e| panic!("{e:?}"));
    let _b_conn = b.conn;
    let mut cb = Client::from_io(b.io.read, b.io.write);
    let (_, list) = cb.hello();
    assert!(list.iter().any(|i| i.terminal == t), "{list:?}");
    cb.attach(t);
    ca.input(t, "set /a 66*1000+66\r");
    cb.output_until(t, "66066");

    // gui_bound_terminate_ends_terminals: the Desktop's quit.
    let gui = daemon.gui_pid().unwrap();
    daemon.terminate(Duration::from_secs(15));
    assert!(wait_dead(gui, Duration::from_secs(5)));
    assert!(wait_dead(pid, Duration::from_secs(5)));
    let e = xshell_core::pipe::connect(&h.pipe, Instant::now() + T, &|| false).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
}

#![cfg(unix)]
//! Agent Status from agent hooks, end to end against an in-process server and the real hook
//! client (`xshelld event`).
//!
//! The fake `claude` and `codex` log their argv and `XSHELL_*` environment, then run shell
//! lines the test writes to their control FIFO (`ctl.<pid>` in the working directory), in
//! their own environment, as a real agent's hook child would. Never Terminal input: the
//! input heuristics must not stand in for a hook that did not fire.

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;
use xshell_core::agent_status::{posix_quote, AgentStatus};
use xshell_core::launch::LaunchSpec;
use xshell_protocol::msg::{ClientMsg, TerminalInfo};
use xshelld::server::ServerHandle;

use AgentStatus::*;

const SID: &str = "11111111-2222-3333-4444-555555555555";

/// Fake `claude` and `codex`, first in `PATH` once per test binary.
fn hook_fake_agents() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let bin = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hook-agents");
        fs::create_dir_all(&bin).unwrap();
        for name in ["claude", "codex"] {
            let tmp = bin.join(format!(".{name}.{}", std::process::id()));
            fs::write(
                &tmp,
                "#!/bin/sh\ntrap '' HUP\nprintf '%s\\n' \"$@\" -- >> argv.log\n\
                 env | grep '^XSHELL_' >> env.log\nctl=\"$PWD/ctl.$$\"\nmkfifo \"$ctl\"\n\
                 echo $$ >> pids.log\necho \"ready $$.\"\n\
                 while :; do while IFS= read -r l; do eval \"$l\"; done < \"$ctl\"; done\n",
            )
            .unwrap();
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
            fs::rename(&tmp, bin.join(name)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", path);
    });
}

/// The user's own agent configuration, which nothing here may touch.
fn user_agent_config() -> Vec<(PathBuf, Option<SystemTime>)> {
    let home = dirs::home_dir().unwrap_or_default();
    [
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".claude.json",
        ".codex/config.toml",
    ]
    .iter()
    .map(|r| {
        let p = home.join(r);
        let m = fs::metadata(&p).and_then(|m| m.modified()).ok();
        (p, m)
    })
    .collect()
}

struct Env {
    h: TestHome,
    srv: ServerHandle,
    c: Client,
    cwd: PathBuf,
    fake: Fake,
    _reaper: FakeReaper,
    user_config: Vec<(PathBuf, Option<SystemTime>)>,
}

fn env() -> Env {
    hook_fake_agents();
    let user_config = user_agent_config();
    let h = TestHome::new();
    let cwd = h.project("app");
    let fake = Fake {
        bin: PathBuf::new(),
        argv_log: cwd.join("argv.log"),
        pids_log: cwd.join("pids.log"),
    };
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    Env {
        _reaper: FakeReaper(fake.pids_log.clone()),
        h,
        srv,
        c,
        cwd,
        fake,
        user_config,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert_eq!(
                user_agent_config(),
                self.user_config,
                "the user's agent configuration changed"
            );
        }
    }
}

fn spec(cwd: &Path, agent: &str, session: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        agent: Some(agent.into()),
        shell_mode: Some("claude".into()),
        session_id: session.map(str::to_string),
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

impl Env {
    /// Open `agent` and wait until its fake is listening on its control FIFO.
    fn open(&mut self, agent: &str, session: Option<&str>) -> Uuid {
        self.open_spec(spec(&self.cwd, agent, session))
    }

    fn open_spec(&mut self, s: LaunchSpec) -> Uuid {
        let n = self.fake.pids().len();
        let t = Uuid::new_v4();
        self.c.open(t, s);
        self.fake.wait_pids(n + 1);
        t
    }

    /// The newest fake's pid.
    fn pid(&self) -> i32 {
        *self.fake.pids().last().expect("a fake agent")
    }

    /// Run `line` in the newest fake agent's environment.
    fn run(&self, line: &str) {
        let fifo = self.cwd.join(format!("ctl.{}", self.pid()));
        let deadline = Instant::now() + T;
        loop {
            // Non-blocking: fails while the fake is between two reads, or gone.
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                Ok(mut f) => {
                    f.write_all(format!("{line}\n").as_bytes()).unwrap();
                    return;
                }
                Err(e) => {
                    assert!(Instant::now() < deadline, "{}: {e}", fifo.display());
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// The newest launch's raw argv.
    fn argv(&self) -> Vec<String> {
        self.fake.raw_launches().last().cloned().expect("a launch")
    }

    /// The Claude Code settings the newest launch got.
    fn claude_settings(&self) -> Value {
        let argv = self.argv();
        let i = argv
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings");
        serde_json::from_slice(&fs::read(&argv[i + 1]).unwrap()).unwrap()
    }

    /// Fire the Claude Code hook for `event`, as Claude Code would: its command, in the shell.
    fn claude_hook(&self, event: &str) {
        let v = self.claude_settings();
        let cmd = v["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("no {event} hook in {v}"))
            .to_string();
        self.run(&cmd);
    }

    /// Codex's `notify` program and arguments, read from the `-c` override as TOML.
    fn codex_notify(&self) -> Vec<String> {
        let argv = self.argv();
        let v = argv
            .iter()
            .find_map(|a| a.strip_prefix("notify="))
            .expect("a notify override");
        let t: toml::Table = toml::from_str(&format!("v = {v}")).unwrap();
        t["v"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    }

    /// Run Codex's `notify` with `payload` appended, as Codex does after a turn.
    fn codex_notify_with(&self, payload: &str) {
        let mut words: Vec<String> = self.codex_notify().iter().map(|w| posix_quote(w)).collect();
        words.push(posix_quote(payload));
        self.run(&words.join(" "));
    }

    /// The newest launch's `XSHELL_TERMINAL_ID`.
    fn terminal_id_env(&self) -> String {
        fs::read_to_string(self.cwd.join("env.log"))
            .unwrap()
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("XSHELL_TERMINAL_ID="))
            .expect("XSHELL_TERMINAL_ID")
            .to_string()
    }

    /// Wait until `c` is told `t` has `status`.
    fn wait_status(c: &mut Client, t: Uuid, status: Option<AgentStatus>) -> TerminalInfo {
        let list = c.terminals_where(|l| {
            l.iter()
                .any(|i| i.terminal == t && i.agent_status == status)
        });
        list.into_iter().find(|i| i.terminal == t).unwrap()
    }

    fn wait(&mut self, t: Uuid, status: Option<AgentStatus>) -> TerminalInfo {
        Self::wait_status(&mut self.c, t, status)
    }

    /// `t`'s status as a new connection is told.
    fn now(&self, t: Uuid) -> Option<AgentStatus> {
        let mut c = Client::connect(&self.srv.socket);
        let list = c.hello(range(1, 1)).1;
        list.iter()
            .find(|i| i.terminal == t)
            .expect("listed")
            .agent_status
    }

    /// Type `data` and wait until the Daemon handled it (requests on one connection are
    /// handled in order).
    fn type_in(&mut self, t: Uuid, data: &str) {
        self.c.input(t, data);
        self.c
            .request(&ClientMsg::TermResize {
                terminal: t,
                cols: 80,
                rows: 24,
            })
            .unwrap();
    }
}

#[test]
fn claude_hooks_drive_status_on_every_connection() {
    let mut e = env();
    let t = e.open("claude", Some(SID));
    let mut other = Client::connect(&e.srv.socket);
    other.hello(range(1, 1));
    assert_eq!(e.now(t), None);

    e.claude_hook("UserPromptSubmit");
    e.wait(t, Some(Working));
    Env::wait_status(&mut other, t, Some(Working));

    e.claude_hook("Notification");
    e.wait(t, Some(NeedsYou));
    Env::wait_status(&mut other, t, Some(NeedsYou));
    // Typing an answer, editing or Backspace does not clear it: the hooks do.
    for input in ["1", "abc", "\x7f", "\x1b[I"] {
        e.type_in(t, input);
    }
    assert_eq!(e.now(t), Some(NeedsYou));
    // The tool ran.
    e.claude_hook("PostToolUse");
    e.wait(t, Some(Working));
    Env::wait_status(&mut other, t, Some(Working));

    e.claude_hook("Stop");
    e.wait(t, Some(Finished));
    Env::wait_status(&mut other, t, Some(Finished));
    // An interrupt ends a turn without a Stop hook.
    e.claude_hook("UserPromptSubmit");
    e.wait(t, Some(Working));
    e.type_in(t, "\x1b");
    e.wait(t, Some(Finished));
}

#[test]
fn claude_hook_settings_match_the_verified_shape() {
    let mut e = env();
    e.open("claude", Some(SID));
    let v = e.claude_settings();
    let hooks = v["hooks"].as_object().unwrap();
    let mut events: Vec<&str> = hooks.keys().map(String::as_str).collect();
    events.sort();
    // Claude Code 2.1.295's hook events.
    assert_eq!(
        events,
        [
            "Notification",
            "PostToolUse",
            "SessionEnd",
            "Stop",
            "UserPromptSubmit"
        ]
    );
    assert_eq!(
        hooks["Notification"][0]["matcher"],
        "permission_prompt|elicitation_dialog|elicitation_url_dialog"
    );
    let path = e.argv()[e.argv().iter().position(|a| a == "--settings").unwrap() + 1].clone();
    assert_eq!(PathBuf::from(&path), e.h.paths().claude_hooks);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn claude_session_end_is_ended() {
    let mut e = env();
    let t = e.open("claude", Some(SID));
    e.claude_hook("SessionEnd");
    let info = e.wait(t, Some(Ended));
    // The process may outlive its agent (a wrapping shell); ended is final for the run.
    assert_eq!(info.exit_code, None);
    e.claude_hook("UserPromptSubmit");
    e.type_in(t, "\r");
    e.claude_hook("Stop");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(e.now(t), Some(Ended));
}

#[test]
fn codex_notify_reports_finished() {
    let mut e = env();
    let t = e.open("codex", None);
    let notify = e.codex_notify();
    assert_eq!(notify[0], bin());
    assert_eq!(&notify[1..], ["event", "-", "finished"]);
    // Enter starts a turn: Codex has no prompt hook.
    e.type_in(t, "\r");
    e.wait(t, Some(Working));
    // A payload of another type is no turn end.
    e.codex_notify_with(r#"{"type":"something-new"}"#);
    e.codex_notify_with(
        r#"{"type":"agent-turn-complete","thread-id":"x","last-assistant-message":"done"}"#,
    );
    e.wait(t, Some(Finished));
}

#[test]
fn codex_osc9_approval_is_needs_you() {
    let mut e = env();
    let t = e.open("codex", None);
    e.c.attach(t);
    e.run(r"printf '\033]9;Approval requested: ls -la\007'");
    e.wait(t, Some(NeedsYou));
    // Editing keys leave it; an answer key clears it.
    e.type_in(t, "h");
    e.type_in(t, "\t");
    assert_eq!(e.now(t), Some(NeedsYou));
    e.type_in(t, "y");
    e.wait(t, Some(Working));
    // Progress reports are not notifications.
    e.run(r"printf '\033]9;4;1;50\007'");
    e.codex_notify_with(r#"{"type":"agent-turn-complete"}"#);
    e.wait(t, Some(Finished));
    assert_eq!(e.now(t), Some(Finished));
}

#[test]
fn codex_osc9_split_chunks() {
    let mut e = env();
    let t = e.open("codex", None);
    e.run(
        r"printf '\033]9;Appro'; sleep 0.2; printf 'val requested: ls\033'; sleep 0.2; printf '\\'",
    );
    e.wait(t, Some(NeedsYou));
}

#[test]
fn exit_sets_ended() {
    let mut e = env();
    let t = e.open("claude", Some(SID));
    e.claude_hook("UserPromptSubmit");
    e.wait(t, Some(Working));
    e.run("exit 0");
    let info =
        e.c.terminals_where(|l| l.iter().any(|i| i.terminal == t && i.exit_code.is_some()));
    let i = info.iter().find(|i| i.terminal == t).unwrap();
    assert_eq!(i.agent_status, Some(Ended));
}

/// The agent runs inside a shell that stays: the wrapper reports the agent's end.
#[test]
fn wrapper_reports_agent_end() {
    let mut e = env();
    let t = e.open_spec(LaunchSpec {
        shell_id: Some("bash".into()),
        shell_command: Some("/bin/sh".into()),
        ..spec(&e.cwd, "codex", None)
    });
    e.c.attach(t);
    e.type_in(t, "\r");
    e.wait(t, Some(Working));
    e.run("exit 0");
    let info = e.wait(t, Some(Ended));
    assert_eq!(info.exit_code, None, "the shell stays");
    // Shell input never brings the ended run back.
    e.c.marker(t, "after-agent");
    e.type_in(t, "\r");
    e.type_in(t, "y");
    assert_eq!(e.now(t), Some(Ended));
}

fn event_cmd(h: &TestHome, args: &[&str]) -> std::process::Output {
    let t = Instant::now();
    let out = bin_cmd(h)
        .arg("event")
        .args(args)
        .env_remove("XSHELL_TERMINAL_ID")
        .env_remove("XSHELL_EVENT_SOCKET")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(5));
    out
}

#[test]
fn event_unknown_terminal_rejected_exit_zero() {
    let e = env();
    let sock = e.srv.socket.to_string_lossy().into_owned();
    let id = format!("{}.1", Uuid::new_v4());
    let out = event_cmd(&e.h, &["-v", "--socket", &sock, &id, "working"]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown terminal"), "{err}");
    // Silent without -v; still 0 with no Daemon at all and with garbage.
    let out = event_cmd(&e.h, &["--socket", &sock, &id, "working"]);
    assert!(out.status.success() && out.stderr.is_empty());
    for args in [&["-", "working"][..], &["x"], &[]] {
        assert!(event_cmd(&TestHome::new(), args).status.success());
    }
}

#[test]
fn event_for_raw_shell_rejected() {
    let mut e = env();
    let t = Uuid::new_v4();
    e.c.open(t, sh_spec(&e.cwd));
    let r = e.c.request(&ClientMsg::TermEvent {
        terminal: t,
        run: 0,
        status: Working,
    });
    assert_eq!(r, Err("not an agent terminal".into()));
    // The default socket is the Daemon's: no --socket needed.
    let out = event_cmd(&e.h, &["-v", &format!("{t}.0"), "working"]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not an agent terminal"), "{err}");
    assert_eq!(e.now(t), None);
}

#[test]
fn relaunch_resets_status() {
    let mut e = env();
    make_jsonl(&e.h, &e.cwd, SID);
    let t = e.open("claude", Some(SID));
    let old_id = e.terminal_id_env();
    assert!(old_id.starts_with(&format!("{t}.")), "{old_id}");
    e.claude_hook("Stop");
    e.wait(t, Some(Finished));
    let n = e.fake.pids().len();
    e.c.request(&ClientMsg::TermRelaunch {
        terminal: t,
        skip_permissions: true,
    })
    .unwrap();
    e.fake.wait_pids(n + 1);
    e.wait(t, None);
    let new_id = e.terminal_id_env();
    assert_ne!(new_id, old_id);
    // The old run's SessionEnd, delivered late, is refused.
    let old_run: u64 = old_id.rsplit_once('.').unwrap().1.parse().unwrap();
    assert_eq!(
        e.c.request(&ClientMsg::TermEvent {
            terminal: t,
            run: old_run,
            status: Ended,
        }),
        Err("stale run".into())
    );
    assert_eq!(e.now(t), None);
    // The new run reports.
    e.claude_hook("UserPromptSubmit");
    e.wait(t, Some(Working));
}

#[test]
fn restart_does_not_persist_status() {
    let mut e = env();
    make_jsonl(&e.h, &e.cwd, SID);
    let t = e.open("claude", Some(SID));
    e.claude_hook("Stop");
    e.wait(t, Some(Finished));
    let n = e.fake.pids().len();
    e.srv.stopper().shutdown();
    assert!(e.srv.wait_timeout(T).is_some());
    assert!(!fs::read_to_string(e.h.paths().state)
        .unwrap()
        .contains("agentStatus"));
    e.srv = start(&e.h, |_| {});
    e.fake.wait_pids(n + 1);
    assert_eq!(e.now(t), None);
}

#[test]
fn no_agent_config_written() {
    let mut e = env();
    e.open("claude", Some(SID));
    e.claude_hook("UserPromptSubmit");
    e.open("codex", None);
    let home = e.h.home();
    assert!(!home.join(".claude").exists());
    assert!(!home.join(".claude.json").exists());
    assert!(!home.join(".codex").exists());
    // The only file the hooks need is the Daemon's own.
    let daemon_dir: Vec<String> = fs::read_dir(home.join(".xshell/daemon"))
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        daemon_dir.contains(&"claude-hooks.json".to_string()),
        "{daemon_dir:?}"
    );
    // The user's real configuration is checked when `e` drops.
}

#[test]
fn hookless_daemon_launches_agents_as_before() {
    hook_fake_agents();
    let h = TestHome::new();
    let cwd = h.project("app");
    let srv = start(&h, |c| c.event_exe = None);
    let _reaper = FakeReaper(cwd.join("pids.log"));
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    c.open(Uuid::new_v4(), spec(&cwd, "claude", Some(SID)));
    let fake = Fake {
        bin: PathBuf::new(),
        argv_log: cwd.join("argv.log"),
        pids_log: cwd.join("pids.log"),
    };
    fake.wait_pids(1);
    assert_eq!(fake.raw_launches()[0], vec!["--session-id", SID]);
    assert!(!fs::read_to_string(cwd.join("env.log"))
        .unwrap_or_default()
        .contains("XSHELL_TERMINAL_ID"));
    assert!(!h.paths().claude_hooks.exists());
}

#![cfg(unix)]
//! Agent Status from agent hooks, end to end against an in-process server and the real hook
//! client (`xshelld event`).
//!
//! The fake `claude` and `codex` log their argv and `XSHELL_*` environment, then run the
//! numbered control files the test writes (`ctl.<pid>.<n>` in the working directory), one
//! at a time and in order, in their own environment, as a real agent's hook child would,
//! and acknowledge each (`ack.<pid>.<n>`) once it has finished. Never Terminal input: the
//! input heuristics must not stand in for a hook that did not fire. (A FIFO reopened per
//! command lost a command written while the fake was closing it after the previous one.)

mod common;

use common::*;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;
use xshell_core::agent_status::{posix_quote, AgentStatus};
use xshell_core::claude::encode_project_name;
use xshell_core::launch::LaunchSpec;
use xshell_protocol::msg::{ClientMsg, LastLine, Speaker, TerminalInfo};
use xshelld::server::{Role, ServerHandle, TestHook, TestPoint};

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
                "#!/bin/sh\ntrap '' HUP\nprintf '%s\\0' \"$#\" \"$@\" >> argv.log\n\
                 env | grep '^XSHELL_' >> env.log\necho $$ >> pids.log\necho \"ready $$.\"\n\
                 n=0\nwhile :; do\n  f=\"$PWD/ctl.$$.$n\"\n\
                   if [ -f \"$f\" ]; then . \"$f\"; : > \"$PWD/ack.$$.$n\"; n=$((n + 1));\n\
                   else sleep 0.02; fi\ndone\n",
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

/// The user's own agent configuration, which nothing here may touch: in the home the Daemon
/// serves and in this process's home (what its Terminals inherit). Both are the test's own
/// ([`TestHome`], [`isolate_home`]), never the developer's: a Claude Code session running
/// on the same machine rewrites the real `~/.claude.json` at any time (#39).
fn user_agent_config(served: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let process = isolate_home();
    [served, process]
        .iter()
        .flat_map(|home| {
            [
                ".claude/settings.json",
                ".claude/settings.local.json",
                ".claude.json",
                ".codex/config.toml",
            ]
            .map(|r| home.join(r))
        })
        .map(|p| {
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
    /// Control files handed to each fake (by pid) so far.
    sent: std::sync::Mutex<std::collections::HashMap<i32, u32>>,
}

fn env() -> Env {
    env_with(|_| {})
}

impl Drop for Env {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert_eq!(
                user_agent_config(&self.h.home()),
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

    /// Run `line` in the newest fake agent's environment and wait until it has finished (a
    /// hook command has its answer from the Daemon by then).
    fn run(&self, line: &str) {
        let ack = self.send(line);
        let deadline = Instant::now() + T;
        while !ack.exists() {
            assert!(Instant::now() < deadline, "{line:?} not run within {T:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Hand `line` to the newest fake agent; returns the file that acknowledges it. For a
    /// line that ends the fake, which never acknowledges.
    fn send(&self, line: &str) -> PathBuf {
        let pid = self.pid();
        let n = {
            let mut sent = self.sent.lock().unwrap();
            let n = sent.entry(pid).or_insert(0);
            *n += 1;
            *n - 1
        };
        // Written aside and renamed, so the fake never runs half a file.
        let tmp = self.cwd.join(format!(".ctl.{pid}.{n}"));
        fs::write(&tmp, format!("{line}\n")).unwrap();
        fs::rename(&tmp, self.cwd.join(format!("ctl.{pid}.{n}"))).unwrap();
        self.cwd.join(format!("ack.{pid}.{n}"))
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
    e.send("exit 0");
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
    e.send("exit 0");
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
        session_id: None,
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
            session_id: None,
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

// ── Status time and last line ─────────────────────────────────────────────────

const SID_B: &str = "bbbbbbbb-2222-3333-4444-555555555555";

/// Where a held read says it is held, and what releases it.
type Hold = (Sender<()>, Receiver<()>);

/// Counts the last-line worker's reads per Terminal, and can hold one read until released.
#[derive(Clone, Default)]
struct Reads {
    counts: Arc<(Mutex<HashMap<Uuid, usize>>, Condvar)>,
    /// Hold the next read: tell the first sender, then wait on the receiver.
    hold: Arc<Mutex<Option<Hold>>>,
}

impl Reads {
    fn hook(&self) -> TestHook {
        let me = self.clone();
        TestHook(Arc::new(move |id, p| {
            if p == TestPoint::LastLineRead {
                if let Some((held, release)) = me.hold.lock().unwrap().take() {
                    held.send(()).unwrap();
                    let _ = release.recv_timeout(T);
                }
                let (m, cv) = &*me.counts;
                *m.lock().unwrap().entry(id).or_default() += 1;
                cv.notify_all();
            }
            false
        }))
    }

    fn count(&self, t: Uuid) -> usize {
        self.counts.0.lock().unwrap().get(&t).copied().unwrap_or(0)
    }

    /// Wait until `t` has been read `n` times in all.
    fn wait(&self, t: Uuid, n: usize) {
        let (m, cv) = &*self.counts;
        let deadline = Instant::now() + T;
        let mut g = m.lock().unwrap();
        while g.get(&t).copied().unwrap_or(0) < n {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "{t} not read {n} times within {T:?}");
            g = cv.wait_timeout(g, left).unwrap().0;
        }
    }

    /// Wait until both reads (now and the retry) of a request made after `before` reads
    /// are done: the first one's result is stored by then.
    fn settle(&self, t: Uuid, before: usize) {
        self.wait(t, before + 2);
    }
}

/// [`env`] with a short last-line retry and the read counter installed.
fn env_reads() -> (Env, Reads) {
    let reads = Reads::default();
    let hook = reads.hook();
    let e = env_with(|c| {
        c.last_line_retry = Duration::from_millis(100);
        c.test_hook = Some(hook);
    });
    (e, reads)
}

/// [`env`] with the Daemon's configuration changed by `tweak`.
fn env_with(tweak: impl FnOnce(&mut xshelld::server::Config)) -> Env {
    hook_fake_agents();
    let h = TestHome::new();
    let user_config = user_agent_config(&h.home());
    let cwd = h.project("app");
    let fake = Fake {
        bin: PathBuf::new(),
        argv_log: cwd.join("argv.log"),
        pids_log: cwd.join("pids.log"),
    };
    let srv = start(&h, tweak);
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
        sent: Default::default(),
    }
}

fn user_msg(text: &str) -> Value {
    serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
}

fn agent_msg(text: &str) -> Value {
    serde_json::json!({"type": "assistant", "message": {"role": "assistant",
                       "content": [{"type": "text", "text": text}]}})
}

fn append(p: &Path, v: &Value) {
    use std::io::Write;
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .unwrap();
    writeln!(f, "{v}").unwrap();
}

fn line(from: Speaker, text: &str) -> Option<LastLine> {
    Some(LastLine {
        from,
        text: text.into(),
    })
}

impl Env {
    /// The session file of Claude session `sid` in this Env's Project.
    fn claude_file(&self, sid: &str) -> PathBuf {
        self.h
            .home()
            .join(".claude/projects")
            .join(encode_project_name(&self.cwd.to_string_lossy()))
            .join(format!("{sid}.jsonl"))
    }

    /// `t` as a new connection is told.
    fn info(&self, t: Uuid) -> TerminalInfo {
        let mut c = Client::connect(&self.srv.socket);
        let list = c.hello(range(1, 1)).1;
        list.into_iter().find(|i| i.terminal == t).expect("listed")
    }

    /// Wait until `c` is told `t` has `last`.
    fn wait_line(c: &mut Client, t: Uuid, last: &Option<LastLine>) -> TerminalInfo {
        let list = c.terminals_where(|l| l.iter().any(|i| i.terminal == t && i.last_line == *last));
        list.into_iter().find(|i| i.terminal == t).unwrap()
    }

    fn link(&mut self, t: Uuid, sid: &str) {
        self.c
            .request(&ClientMsg::TermUpdate {
                terminal: t,
                session_id: Some(sid.into()),
                meta: None,
            })
            .unwrap();
    }
}

#[test]
fn status_at_ms_changes_with_status_only() {
    let mut e = env();
    let t = e.open("claude", Some(SID));
    let i = e.info(t);
    assert_eq!((i.agent_status, i.status_at_ms), (None, None));
    let before = xshelld_now_ms();
    e.claude_hook("UserPromptSubmit");
    let working = e.wait(t, Some(Working)).status_at_ms.expect("stamped");
    assert!(working >= before, "{working} < {before}");
    // The same status again: the stamp stays.
    e.claude_hook("UserPromptSubmit");
    e.claude_hook("PostToolUse");
    assert_eq!(e.info(t).status_at_ms, Some(working));
    e.claude_hook("Stop");
    let finished = e.wait(t, Some(Finished)).status_at_ms.unwrap();
    assert!(finished > working);
    // A Relaunch's run has no status, and so no stamp, until it reports; then it stamps
    // after the replaced run.
    make_jsonl(&e.h, &e.cwd, SID);
    let n = e.fake.pids().len();
    e.c.request(&ClientMsg::TermRelaunch {
        terminal: t,
        skip_permissions: true,
    })
    .unwrap();
    e.fake.wait_pids(n + 1);
    let i = e.wait(t, None);
    assert_eq!(i.status_at_ms, None);
    e.claude_hook("UserPromptSubmit");
    let again = e.wait(t, Some(Working)).status_at_ms.unwrap();
    assert!(again > finished);
}

fn xshelld_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn claude_last_line_follows_session_file() {
    let mut e = env();
    let f = e.claude_file(SID);
    append(&f, &user_msg("fix the bug"));
    append(&f, &agent_msg("Fixed\nthe   bug."));
    let t = e.open("claude", Some(SID));
    let mut other = Client::connect(&e.srv.socket);
    other.hello(range(1, 1));
    // Read on open already.
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "Fixed the bug."));
    append(&f, &user_msg("now the tests"));
    e.claude_hook("UserPromptSubmit");
    let want = line(Speaker::User, "now the tests");
    Env::wait_line(&mut e.c, t, &want);
    Env::wait_line(&mut other, t, &want);
    append(
        &f,
        &serde_json::json!({"type": "assistant", "message": {"role": "assistant",
                            "content": [{"type": "tool_use", "name": "Bash"}]}}),
    );
    append(&f, &agent_msg("Tests pass."));
    e.claude_hook("Stop");
    let i = Env::wait_line(&mut other, t, &line(Speaker::Agent, "Tests pass."));
    assert_eq!(i.agent_status, Some(Finished));
}

#[test]
fn transcript_written_after_the_hook_is_read_on_retry() {
    let (mut e, reads) = env_reads();
    let f = e.claude_file(SID);
    append(&f, &agent_msg("one"));
    let t = e.open("claude", Some(SID));
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "one"));
    // Both reads of the open are done; the Stop hook fires before the transcript has the
    // turn's text, which only the retry sees.
    reads.settle(t, 0);
    let before = reads.count(t);
    let (held_tx, held_rx) = channel();
    let (release_tx, release_rx) = channel();
    *reads.hold.lock().unwrap() = Some((held_tx, release_rx));
    e.claude_hook("Stop");
    held_rx.recv_timeout(T).expect("the first read");
    append(&f, &agent_msg("two"));
    release_tx.send(()).unwrap();
    // Only the retry, read after "two" was written, can see it.
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "two"));
    assert!(reads.count(t) >= before + 2);
}

#[test]
fn repeated_report_refreshes_last_line_after_the_retry() {
    let (mut e, reads) = env_reads();
    let f = e.claude_file(SID);
    append(&f, &agent_msg("one"));
    let t = e.open("claude", Some(SID));
    reads.settle(t, 0);
    e.claude_hook("Stop");
    let i = e.wait(t, Some(Finished));
    assert_eq!(i.last_line, line(Speaker::Agent, "one"));
    let stamp = i.status_at_ms;
    assert!(stamp.is_some());
    // The Stop's read and its retry are over: only a new request reads again.
    reads.settle(t, 2);
    // Another turn finished: the status repeats, the line is new.
    append(&f, &user_msg("again"));
    append(&f, &agent_msg("two"));
    e.claude_hook("Stop");
    let i = Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "two"));
    assert_eq!(i.agent_status, Some(Finished));
    assert_eq!(i.status_at_ms, stamp, "a repeated status keeps its stamp");
}

#[test]
fn last_line_appears_after_term_update_links_session() {
    let mut e = env();
    let t = e.open("claude", None);
    append(&e.claude_file(SID_B), &agent_msg("linked"));
    assert_eq!(e.info(t).last_line, None);
    e.link(t, SID_B);
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "linked"));
}

#[test]
fn codex_last_line_with_linked_session() {
    let mut e = env();
    let t = e.open("codex", None);
    let sid = "0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee";
    let rollout =
        e.h.home()
            .join(".codex/sessions/2026/10/10")
            .join(format!("rollout-2026-10-10T10-00-00-{sid}.jsonl"));
    let ev = |kind: &str, text: &str| serde_json::json!({"type": "event_msg", "payload": {"type": kind, "message": text}});
    append(
        &rollout,
        &serde_json::json!({"type": "session_meta", "payload": {"id": sid}}),
    );
    append(&rollout, &ev("user_message", "build it"));
    // Unlinked (Codex starts without a session id): no line.
    assert_eq!(e.info(t).last_line, None);
    e.link(t, sid);
    Env::wait_line(&mut e.c, t, &line(Speaker::User, "build it"));
    append(&rollout, &ev("agent_message", "Built."));
    e.codex_notify_with(r#"{"type":"agent-turn-complete"}"#);
    let i = Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "Built."));
    assert_eq!(i.agent_status, Some(Finished));
}

#[test]
fn last_line_stays_inside_session_storage() {
    use std::os::unix::fs::symlink;
    let (mut e, reads) = env_reads();
    let t = e.open("claude", None);
    reads.settle(t, 0);
    let projects = e.h.home().join(".claude/projects");
    let enc = encode_project_name(&e.cwd.to_string_lossy());
    // `../escape` would name ~/.claude/projects/escape.jsonl.
    append(&projects.join("escape.jsonl"), &agent_msg("escaped"));
    append(&e.claude_file("inproject"), &agent_msg("control"));
    let outside = e.h.root().join("outside.jsonl");
    append(&outside, &agent_msg("secret"));
    symlink(&outside, projects.join(&enc).join("linked.jsonl")).unwrap();
    for sid in ["../escape", "linked"] {
        let before = reads.count(t);
        e.link(t, sid);
        reads.settle(t, before);
        assert_eq!(e.info(t).last_line, None, "{sid}");
    }
    // A symlinked Project directory pointing outside storage.
    let elsewhere = e.h.root().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    append(&elsewhere.join("viadir.jsonl"), &agent_msg("secret"));
    let real = projects.join(format!("{enc}.real"));
    fs::rename(projects.join(&enc), &real).unwrap();
    symlink(&elsewhere, projects.join(&enc)).unwrap();
    let before = reads.count(t);
    e.link(t, "viadir");
    reads.settle(t, before);
    assert_eq!(e.info(t).last_line, None);
    // The control: a plain file in the Project directory is read.
    fs::remove_file(projects.join(&enc)).unwrap();
    fs::rename(&real, projects.join(&enc)).unwrap();
    e.link(t, "inproject");
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "control"));
}

#[test]
fn last_line_read_for_a_replaced_session_is_dropped() {
    let (mut e, reads) = env_reads();
    append(&e.claude_file(SID), &agent_msg("line A"));
    append(&e.claude_file(SID_B), &agent_msg("line B"));
    // The open's read of session A is held until the Terminal is relinked to B.
    let (held_tx, held_rx) = channel();
    let (release_tx, release_rx) = channel();
    *reads.hold.lock().unwrap() = Some((held_tx, release_rx));
    let t = e.open("claude", Some(SID));
    held_rx.recv_timeout(T).expect("the read of session A");
    e.link(t, SID_B);
    release_tx.send(()).unwrap();
    let mut seen = Vec::new();
    let deadline = Instant::now() + T;
    loop {
        assert!(Instant::now() < deadline, "no line B: {seen:?}");
        let list = e.c.terminals();
        let i = list.iter().find(|i| i.terminal == t).unwrap();
        seen.push(i.last_line.clone());
        if i.last_line == line(Speaker::Agent, "line B") {
            break;
        }
    }
    assert!(
        !seen.contains(&line(Speaker::Agent, "line A")),
        "session A's line was published: {seen:?}"
    );
    reads.settle(t, 1);
    assert_eq!(e.info(t).last_line, line(Speaker::Agent, "line B"));
}

#[test]
fn mobile_connection_sees_status_at_and_last_line() {
    let mut e = env();
    append(&e.claude_file(SID), &agent_msg("for the phone"));
    let mut m = Client::in_process(&e.srv, Role::Mobile);
    let t = e.open("claude", Some(SID));
    e.claude_hook("Stop");
    let i = Env::wait_status(&mut m, t, Some(Finished));
    assert!(i.status_at_ms.is_some());
    Env::wait_line(&mut m, t, &line(Speaker::Agent, "for the phone"));
}

#[test]
fn restore_recomputes_last_line() {
    let mut e = env();
    append(&e.claude_file(SID), &agent_msg("before restart"));
    let t = e.open("claude", Some(SID));
    e.claude_hook("Stop");
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "before restart"));
    let n = e.fake.pids().len();
    e.srv.stopper().shutdown();
    assert!(e.srv.wait_timeout(T).is_some());
    let state = fs::read_to_string(e.h.paths().state).unwrap();
    assert!(!state.contains("lastLine") && !state.contains("statusAtMs"));
    e.srv = start(&e.h, |_| {});
    e.fake.wait_pids(n + 1);
    let want = line(Speaker::Agent, "before restart");
    let deadline = Instant::now() + T;
    let i = loop {
        let i = e.info(t);
        if i.last_line == want {
            break i;
        }
        assert!(Instant::now() < deadline, "no line after the restart");
        std::thread::sleep(Duration::from_millis(20));
    };
    // The status is not persisted.
    assert_eq!((i.agent_status, i.status_at_ms), (None, None));
}

/// A list filled to its budget, then every entry given the widest last line: the frame
/// still fits the list budget and the connection queue, and no connection is dropped.
#[test]
fn full_list_with_widest_last_lines_fits_the_queue() {
    use xshell_protocol::msg::{encode_msg, OpenSpec, ServerMsg, LAST_LINE_MAX_CHARS};
    // Room for a handful of entries, each reserving its optional fields.
    const LIST: usize = 40_000;
    let mut e = env_with(|c| {
        c.max_list_bytes = LIST;
        c.conn_total_cap = LIST;
    });
    // Four UTF-8 bytes per character, the most JSON spends on one.
    let widest = "\u{1D11E}".repeat(LAST_LINE_MAX_CHARS + 10);
    let want = line(Speaker::Agent, &"\u{1D11E}".repeat(LAST_LINE_MAX_CHARS));
    let mut opened = Vec::new();
    // Big entries first, then smaller ones into what is left.
    for title in [1000usize, 100, 0] {
        loop {
            let sid = format!("{:08x}-0000-4000-8000-000000000000", opened.len());
            append(&e.claude_file(&sid), &agent_msg(&widest));
            let t = Uuid::new_v4();
            let mut meta = serde_json::Map::new();
            meta.insert("title".into(), Value::String("x".repeat(title)));
            let r = e.c.request(&ClientMsg::TermOpen {
                spec: OpenSpec {
                    terminal: t,
                    launch: spec(&e.cwd, "claude", Some(&sid)),
                    cols: 80,
                    rows: 24,
                    meta,
                    first_message: None,
                    adopt_existing: false,
                },
            });
            match r {
                Ok(_) => {
                    opened.push(t);
                    e.fake.wait_pids(opened.len());
                }
                Err(err) => {
                    assert!(err.contains("terminal list too large"), "{err}");
                    break;
                }
            }
        }
    }
    assert!(opened.len() >= 4, "{}", opened.len());
    let full = |l: &[TerminalInfo]| {
        opened
            .iter()
            .all(|t| l.iter().any(|i| i.terminal == *t && i.last_line == want))
    };
    let list = e.c.terminals_where(full);
    let frame = encode_msg(&ServerMsg::Terminals { list }, None).unwrap();
    assert!(frame.len() <= LIST, "{} > {LIST}", frame.len());
    // Neither the connection that saw every list nor a new one was dropped.
    e.c.request(&ClientMsg::TermResize {
        terminal: opened[0],
        cols: 81,
        rows: 24,
    })
    .unwrap();
    let mut c = Client::connect(&e.srv.socket);
    let (_, list) = c.hello(range(1, 1));
    assert!(full(&list));
}

// ── The Daemon's own Codex link (xshell#36) ───────────────────────────────────

const CODEX_A: &str = "0199aaaa-0000-7000-8000-00000000000a";
const CODEX_B: &str = "0199bbbb-0000-7000-8000-00000000000b";
const CODEX_C: &str = "0199cccc-0000-7000-8000-00000000000c";

type Counts = Arc<(Mutex<HashMap<Uuid, usize>>, Condvar)>;

/// Counts the link checks per Terminal and the decisions that follow them, and can hold one
/// check (before its decision) until released.
#[derive(Clone, Default)]
struct Links {
    counts: Counts,
    done: Counts,
    hold: Arc<Mutex<Option<Hold>>>,
    /// While set, every `term.open` waits before the registry lock until this many have
    /// arrived (bounded), so they race for it.
    meet: Arc<(Mutex<Option<usize>>, Condvar)>,
}

fn bump(c: &Counts, id: Uuid) {
    let (m, cv) = &**c;
    *m.lock().unwrap().entry(id).or_default() += 1;
    cv.notify_all();
}

/// Wait until `c` has counted `n` for `t`.
fn wait_count(c: &Counts, t: Uuid, n: usize, what: &str) {
    let (m, cv) = &**c;
    let deadline = Instant::now() + T;
    let mut g = m.lock().unwrap();
    while g.get(&t).copied().unwrap_or(0) < n {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "{t} not {what} {n} times within {T:?}");
        g = cv.wait_timeout(g, left).unwrap().0;
    }
}

impl Links {
    fn hook(&self) -> TestHook {
        let me = self.clone();
        TestHook(Arc::new(move |id, p| {
            if p == TestPoint::LinkChecked {
                let hold = me.hold.lock().unwrap().take();
                if let Some((held, release)) = hold {
                    held.send(()).unwrap();
                    let _ = release.recv_timeout(T);
                }
                bump(&me.counts, id);
            } else if p == TestPoint::LinkDone {
                bump(&me.done, id);
            } else if p == TestPoint::OpenDecide {
                let (m, cv) = &*me.meet;
                let mut left = m.lock().unwrap();
                if let Some(n) = left.as_mut() {
                    *n = n.saturating_sub(1);
                    cv.notify_all();
                    let _ = cv
                        .wait_timeout_while(left, Duration::from_secs(10), |l| {
                            l.is_some_and(|n| n > 0)
                        })
                        .unwrap();
                }
            }
            false
        }))
    }

    fn count(&self, t: Uuid) -> usize {
        self.counts.0.lock().unwrap().get(&t).copied().unwrap_or(0)
    }

    /// Wait until `t`'s reports have been checked `n` times in all.
    fn wait(&self, t: Uuid, n: usize) {
        wait_count(&self.counts, t, n, "checked");
    }

    /// Wait until `n` checks of `t`'s reports have been decided in all: linked, kept,
    /// dropped or found replaced.
    fn wait_done(&self, t: Uuid, n: usize) {
        wait_count(&self.done, t, n, "decided");
    }

    /// The next `n` opens meet before any of them locks the registry.
    fn meet(&self, n: usize) {
        *self.meet.0.lock().unwrap() = Some(n);
    }

    /// Hold the next check; returns where it says it is held and what releases it.
    fn arm(&self) -> (Receiver<()>, Sender<()>) {
        let (held_tx, held_rx) = channel();
        let (release_tx, release_rx) = channel();
        *self.hold.lock().unwrap() = Some((held_tx, release_rx));
        (held_rx, release_tx)
    }
}

/// [`env`] with a short last-line retry (the link's retry too) and the check counter.
fn env_links_with(tweak: impl FnOnce(&mut xshelld::server::Config)) -> (Env, Links) {
    let links = Links::default();
    let hook = links.hook();
    let e = env_with(|c| {
        c.last_line_retry = Duration::from_millis(300);
        c.test_hook = Some(hook);
        tweak(c);
    });
    (e, links)
}

fn env_links() -> (Env, Links) {
    env_links_with(|_| {})
}

fn codex_event(kind: &str, text: &str) -> Value {
    serde_json::json!({"type": "event_msg", "payload": {"type": kind, "message": text}})
}

impl Env {
    /// Where Codex keeps the rollout of session `sid`.
    fn rollout_path(&self, sid: &str) -> PathBuf {
        self.h
            .home()
            .join(".codex/sessions/2026/10/10")
            .join(format!("rollout-2026-10-10T10-00-00-{sid}.jsonl"))
    }

    /// Write Codex's rollout for session `id` at `path`: its `session_meta` (in `cwd`, from
    /// `source`), then a user message and `said` by the agent.
    fn write_rollout(&self, path: &Path, id: &str, cwd: &Path, source: Value, said: &str) {
        append(
            path,
            &serde_json::json!({"type": "session_meta", "payload": {
                "id": id, "cwd": cwd, "source": source, "cli_version": "0.154.0"}}),
        );
        append(path, &codex_event("user_message", "build it"));
        append(path, &codex_event("agent_message", said));
    }

    /// A rollout for `sid` in this Env's Project, written by the Codex CLI.
    fn rollout(&self, sid: &str, said: &str) -> PathBuf {
        let p = self.rollout_path(sid);
        self.write_rollout(&p, sid, &self.cwd, "cli".into(), said);
        p
    }

    /// Codex's `notify` after a turn of session `sid`, through the real hook client.
    fn turn_end(&self, sid: &str) {
        let payload = serde_json::json!({"type": "agent-turn-complete", "thread-id": sid,
            "turn-id": "1", "cwd": self.cwd, "input-messages": ["build it"],
            "last-assistant-message": "done"});
        self.codex_notify_with(&payload.to_string());
    }

    /// The newest launch's run.
    fn run_id(&self) -> u64 {
        self.terminal_id_env()
            .rsplit_once('.')
            .unwrap()
            .1
            .parse()
            .unwrap()
    }

    /// A `term.event` reporting `sid` for run `run` of `t`, sent as the hook client would.
    fn report(&mut self, t: Uuid, run: u64, sid: &str) -> Result<Value, String> {
        self.c.request(&ClientMsg::TermEvent {
            terminal: t,
            run,
            status: Finished,
            session_id: Some(sid.into()),
        })
    }

    fn update(&mut self, t: Uuid, sid: Option<&str>, title: Option<&str>) -> Result<Value, String> {
        let meta = title.map(|title| {
            let mut m = serde_json::Map::new();
            m.insert("title".into(), title.into());
            m
        });
        self.c.request(&ClientMsg::TermUpdate {
            terminal: t,
            session_id: sid.map(str::to_owned),
            meta,
        })
    }

    /// Wait until `c` is told `t` runs session `sid`.
    fn wait_session(c: &mut Client, t: Uuid, sid: &str) -> TerminalInfo {
        let list = c.terminals_where(|l| {
            l.iter()
                .any(|i| i.terminal == t && i.spec.session_id.as_deref() == Some(sid))
        });
        list.into_iter().find(|i| i.terminal == t).unwrap()
    }

    fn session(&self, t: Uuid) -> Option<String> {
        self.info(t).spec.session_id
    }

    /// A Desktop connection subscribed to `t`'s conversation.
    fn subscriber(&self, t: Uuid) -> (Client, Value) {
        let mut s = Client::connect(&self.srv.socket);
        s.hello(range(1, 1));
        let page = s
            .request(&ClientMsg::SessionSubscribe {
                terminal: t,
                limit: None,
            })
            .expect("session.subscribe");
        (s, page)
    }
}

/// The next reset of `t`'s conversation `s` is sent.
fn next_reset(s: &mut Client, t: Uuid) -> (Option<String>, usize) {
    use xshell_protocol::msg::ServerMsg;
    let deadline = Instant::now() + T;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let m = s.try_msg(left, |m| {
            matches!(m, ServerMsg::SessionAppend { terminal, reset: true, .. } if *terminal == t)
        });
        match m {
            Some(ServerMsg::SessionAppend { session, items, .. }) => return (session, items.len()),
            Some(_) => {}
            None => panic!("no reset of {t}: {:?}", s.summary()),
        }
    }
}

#[test]
fn codex_notify_links_session_without_desktop() {
    let mut e = env();
    let t = e.open("codex", None);
    // A Chat View opened before the link: nothing linked yet.
    let (mut sub, page) = e.subscriber(t);
    assert_eq!(page["session"], Value::Null, "{page}");
    e.rollout(CODEX_A, "Built.");
    // No Desktop sends a term.update: Codex's notify alone links the session.
    e.turn_end(CODEX_A);
    let i = Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "Built."));
    assert_eq!(i.spec.session_id.as_deref(), Some(CODEX_A));
    assert_eq!(i.agent_status, Some(Finished));
    // Persisted.
    let state = e.h.state_json();
    assert_eq!(
        state["terminals"][0]["spec"]["sessionId"].as_str(),
        Some(CODEX_A),
        "{state}"
    );
    // The Chat View follows it.
    let (session, items) = next_reset(&mut sub, t);
    assert_eq!(session.as_deref(), Some(CODEX_A));
    assert_eq!(items, 2);
    // And a Relaunch resumes it.
    let n = e.fake.pids().len();
    e.c.request(&ClientMsg::TermRelaunch {
        terminal: t,
        skip_permissions: true,
    })
    .unwrap();
    e.fake.wait_pids(n + 1);
    let argv = e.argv();
    let at = argv
        .iter()
        .position(|a| a == "resume")
        .expect("codex resume");
    assert!(argv[at..].iter().any(|a| a == CODEX_A), "{argv:?}");
}

#[test]
fn codex_link_validation() {
    use std::os::unix::fs::symlink;
    let (mut e, links) = env_links();
    let t = e.open("codex", None);
    let run = e.run_id();
    let other = e.h.project("other");
    // A rollout outside storage, linked into it.
    let outside = e.h.root().join("outside.jsonl");
    e.write_rollout(
        &outside,
        "0199dddd-0000-7000-8000-symlinked000",
        &e.cwd,
        "cli".into(),
        "x",
    );
    let linked = e.rollout_path("0199dddd-0000-7000-8000-symlinked000");
    fs::create_dir_all(linked.parent().unwrap()).unwrap();
    symlink(&outside, &linked).unwrap();
    // Another Project's session.
    let elsewhere = "0199eeee-0000-7000-8000-elsewhere0000";
    e.write_rollout(
        &e.rollout_path(elsewhere),
        elsewhere,
        &other,
        "cli".into(),
        "x",
    );
    // A subagent's thread.
    let sub = "0199ffff-0000-7000-8000-subagent00000";
    let spawn = serde_json::json!({"subagent": {"thread_spawn": {"parent_thread_id": CODEX_A}}});
    e.write_rollout(&e.rollout_path(sub), sub, &e.cwd, spawn, "x");
    // A rollout named for one session whose meta names another.
    let renamed = "0199abab-0000-7000-8000-renamed00000";
    e.write_rollout(&e.rollout_path(renamed), CODEX_B, &e.cwd, "cli".into(), "x");
    let long = "x".repeat(201);
    for (sid, checks) in [
        ("../x", 1),
        ("-x", 1),
        (long.as_str(), 1),
        // Missing: looked for again at the retry, then dropped.
        ("0199aaaa-0000-7000-8000-missing00000", 2),
        ("0199dddd-0000-7000-8000-symlinked000", 1),
        (elsewhere, 1),
        (sub, 1),
        (renamed, 1),
    ] {
        let before = links.count(t);
        e.report(t, run, sid).unwrap();
        // Every check of this report is decided before the list is looked at.
        links.wait_done(t, before + checks);
        assert_eq!(links.count(t), before + checks, "{sid}");
        assert_eq!(e.session(t), None, "{sid}");
    }
    // A dropped report stays dropped: its rollout appearing later links nothing.
    e.rollout("0199aaaa-0000-7000-8000-missing00000", "late");
    e.codex_notify_with(r#"{"type":"agent-turn-complete"}"#);
    e.wait(t, Some(Finished));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(e.session(t), None);
    // The control: a valid report links.
    e.rollout(CODEX_A, "valid");
    e.report(t, run, CODEX_A).unwrap();
    Env::wait_session(&mut e.c, t, CODEX_A);
}

#[test]
fn codex_link_daemon_then_desktop() {
    let mut e = env();
    let t = e.open("codex", None);
    e.rollout(CODEX_A, "from A");
    e.rollout(CODEX_B, "from B");
    e.turn_end(CODEX_A);
    Env::wait_session(&mut e.c, t, CODEX_A);
    // The Desktop's guess of another session is refused, its title too.
    assert_eq!(
        e.update(t, Some(CODEX_B), Some("B's title")),
        Err(format!(
            "cannot link this chat to another session: its agent reported session {CODEX_A}"
        ))
    );
    let i = e.info(t);
    assert_eq!(i.spec.session_id.as_deref(), Some(CODEX_A));
    assert!(!i.meta.contains_key("title"));
    // The same session, and metadata alone, pass.
    e.update(t, Some(CODEX_A), Some("A's title")).unwrap();
    e.update(t, None, Some("renamed")).unwrap();
    let i = e.info(t);
    assert_eq!(i.spec.session_id.as_deref(), Some(CODEX_A));
    assert_eq!(i.meta["title"], "renamed");
    assert_eq!(i.last_line, line(Speaker::Agent, "from A"));
}

#[test]
fn codex_link_desktop_then_daemon() {
    let mut e = env();
    let t = e.open("codex", None);
    e.rollout(CODEX_A, "from A");
    e.rollout(CODEX_B, "from B");
    // The Desktop guesses B first.
    e.link(t, CODEX_B);
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "from B"));
    let (mut sub, page) = e.subscriber(t);
    assert_eq!(page["session"], CODEX_B, "{page}");
    // Codex reports A: the agent's report wins.
    e.turn_end(CODEX_A);
    let i = Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "from A"));
    assert_eq!(i.spec.session_id.as_deref(), Some(CODEX_A));
    let (session, _) = next_reset(&mut sub, t);
    assert_eq!(session.as_deref(), Some(CODEX_A));
    // The Desktop's guess, sent again, is refused now.
    assert!(e.update(t, Some(CODEX_B), None).is_err());
    assert_eq!(e.session(t).as_deref(), Some(CODEX_A));
}

#[test]
fn codex_link_follows_new_thread() {
    let mut e = env();
    let t = e.open("codex", None);
    e.rollout(CODEX_A, "from A");
    e.turn_end(CODEX_A);
    Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "from A"));
    // `/new` in Codex: the next turn is another thread, in the same directory.
    e.rollout(CODEX_C, "from C");
    e.turn_end(CODEX_C);
    let i = Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "from C"));
    assert_eq!(i.spec.session_id.as_deref(), Some(CODEX_C));
    // The Desktop may no longer name A.
    assert!(e.update(t, Some(CODEX_A), None).is_err());
}

#[test]
fn codex_link_rollout_written_late() {
    let (mut e, links) = env_links();
    let t = e.open("codex", None);
    // The notify comes before the rollout is on disk: the first check misses it.
    e.turn_end(CODEX_A);
    links.wait_done(t, 1);
    assert_eq!(e.session(t), None);
    e.rollout(CODEX_A, "late");
    // The retry finds it.
    let i = Env::wait_line(&mut e.c, t, &line(Speaker::Agent, "late"));
    assert_eq!(i.spec.session_id.as_deref(), Some(CODEX_A));
}

/// A newer report replaces one being checked: the older one is never linked.
#[test]
fn codex_link_newer_report_wins() {
    let (mut e, links) = env_links();
    let t = e.open("codex", None);
    let run = e.run_id();
    e.rollout(CODEX_A, "from A");
    e.rollout(CODEX_C, "from C");
    let (held_a, release_a) = links.arm();
    e.report(t, run, CODEX_A).unwrap();
    held_a.recv_timeout(T).expect("the check of A");
    // C arrives while A is being checked; C's check is held before its decision too.
    let (held_c, release_c) = links.arm();
    e.report(t, run, CODEX_C).unwrap();
    release_a.send(()).unwrap();
    held_c.recv_timeout(T).expect("the check of C");
    // A's decision is over (the one worker is at C's), and it applied nothing.
    links.wait_done(t, 1);
    assert_eq!(e.session(t), None, "the replaced report of A was linked");
    release_c.send(()).unwrap();
    Env::wait_session(&mut e.c, t, CODEX_C);
    links.wait_done(t, 2);
    links.wait(t, 2);
    assert_eq!(e.session(t).as_deref(), Some(CODEX_C));
    assert!(e.update(t, Some(CODEX_A), None).is_err());
}

/// A Relaunch resumes the agent's session and keeps it the agent's; a report of the run it
/// replaced changes nothing.
#[test]
fn codex_link_survives_relaunch_and_ignores_older_run() {
    let mut e = env();
    let t = e.open("codex", None);
    e.rollout(CODEX_A, "from A");
    e.rollout(CODEX_C, "from C");
    e.turn_end(CODEX_A);
    Env::wait_session(&mut e.c, t, CODEX_A);
    let old_run = e.run_id();
    let n = e.fake.pids().len();
    e.c.request(&ClientMsg::TermRelaunch {
        terminal: t,
        skip_permissions: true,
    })
    .unwrap();
    e.fake.wait_pids(n + 1);
    assert_ne!(e.run_id(), old_run);
    // Before the new run reports anything, the Desktop's other guess is still refused.
    assert!(e.update(t, Some(CODEX_C), None).is_err());
    // The old run's late report of another session is refused.
    assert_eq!(e.report(t, old_run, CODEX_C), Err("stale run".into()));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(e.session(t).as_deref(), Some(CODEX_A));
}

/// A link the list budget refuses is not the agent's: the Desktop may still link.
#[test]
fn codex_link_refused_by_budget_is_not_the_agents() {
    const BUDGET: usize = 12_000;
    let (mut e, links) = env_links_with(|c| c.max_terminal_bytes = BUDGET);
    let t = e.open("codex", None);
    let run = e.run_id();
    // The longest title that fits, then 60 bytes less: room for a short session id only.
    let (mut lo, mut hi) = (0usize, BUDGET);
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        match e.update(t, None, Some(&"t".repeat(mid))) {
            Ok(_) => lo = mid,
            Err(_) => hi = mid,
        }
    }
    e.update(t, None, Some(&"t".repeat(lo - 60))).unwrap();
    let long = format!("a{}", "b".repeat(199));
    e.rollout(&long, "long");
    e.report(t, run, &long).unwrap();
    links.wait_done(t, 1);
    assert_eq!(e.session(t), None);
    // Not linked, so not the agent's: the Desktop's link of a short id passes.
    e.update(t, Some(CODEX_B), None).unwrap();
    assert_eq!(e.session(t).as_deref(), Some(CODEX_B));
}

// ── One agent per session (xshell#41) ─────────────────────────────────────────

/// A `term.open` of `launch` under a new UUID that is answered with the Terminal already
/// running its session.
fn adopt_msg(launch: LaunchSpec) -> (Uuid, ClientMsg) {
    let t = Uuid::new_v4();
    let msg = ClientMsg::TermOpen {
        spec: xshell_protocol::msg::OpenSpec {
            terminal: t,
            launch,
            cols: 80,
            rows: 24,
            adopt_existing: true,
            ..Default::default()
        },
    };
    (t, msg)
}

/// A Codex chat the Daemon linked to `CODEX_A` is that session: a Desktop and a Mobile
/// resuming it at the same moment both get it, and no agent starts.
#[test]
fn codex_mobile_and_desktop_resume_linked_session_one_agent() {
    use xshell_protocol::msg::OpenReply;
    let (mut e, links) = env_links();
    let t = e.open("codex", None);
    e.rollout(CODEX_A, "Built.");
    e.turn_end(CODEX_A);
    Env::wait_session(&mut e.c, t, CODEX_A);
    let launches = e.fake.launches().len();
    let mut m = Client::in_process(&e.srv, Role::Mobile);
    let resume = spec(&e.cwd, "codex", Some(CODEX_A));
    links.meet(2);
    let (_, desk_msg) = adopt_msg(resume.clone());
    let (_, mob_msg) = adopt_msg(resume);
    let (id_d, id_m) = (e.c.request_id(), m.request_id());
    e.c.send(&desk_msg, Some(id_d));
    m.send(&mob_msg, Some(id_m));
    for r in [e.c.wait_res(id_d), m.wait_res(id_m)] {
        let r: OpenReply = serde_json::from_value(r.unwrap()).unwrap();
        assert!(r.existed, "{r:?}");
        assert_eq!(r.terminal, Some(t));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(e.fake.launches().len(), launches);
}

/// D6: a Codex report that is not checked yet holds no session; once it is linked, it does.
#[test]
fn codex_pending_report_does_not_match() {
    use xshell_protocol::msg::OpenReply;
    let (mut e, links) = env_links();
    let t = e.open("codex", None);
    let run = e.run_id();
    e.rollout(CODEX_A, "from A");
    let (held, release) = links.arm();
    e.report(t, run, CODEX_A).unwrap();
    held.recv_timeout(T).expect("the check of A");
    let (t2, msg) = adopt_msg(spec(&e.cwd, "codex", Some(CODEX_A)));
    let r: OpenReply = serde_json::from_value(e.c.request(&msg).unwrap()).unwrap();
    assert!(!r.existed, "{r:?}");
    assert_eq!(r.terminal, Some(t2));
    release.send(()).unwrap();
    Env::wait_session(&mut e.c, t, CODEX_A);
    links.wait_done(t, 1);
    // Both run it now (a link is never refused for that, D7): the older one is the session.
    let (_, msg) = adopt_msg(spec(&e.cwd, "codex", Some(CODEX_A)));
    let r: OpenReply = serde_json::from_value(e.c.request(&msg).unwrap()).unwrap();
    assert!(r.existed, "{r:?}");
    assert_eq!(r.terminal, Some(t));
}

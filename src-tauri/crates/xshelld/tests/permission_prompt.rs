#![cfg(unix)]
//! Permission Prompts (capability `agent.prompt`), end to end against an in-process server.
//!
//! The fake `claude` and `codex` print the prompt fixtures of `xshell-core`
//! (`crates/core/tests/fixtures/prompts`) when a control file says so (`ctl.<pid>.<n>` in the
//! working directory, run in order and acknowledged with `ack.<pid>.<n>`, as in
//! `agent_status.rs`). A reader in the background puts their tty in raw mode, logs every byte
//! it reads as hex (`keys.log`, one byte per line) and then sources `on-key.sh` if the working
//! directory has one (with the byte in `$k`); while a `hold` file exists it reads nothing.
//! Every SIGWINCH appends `stty size` to `sizes.log`. A Claude Terminal is made needs-you by
//! the report its hook would send (`term.event` with its run), a Codex one by OSC 9 in its
//! output.

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::AgentStatus;
use xshell_core::launch::LaunchSpec;
use xshell_protocol::msg::{
    ClientMsg, OpenSpec, PermissionPrompt, ServerMsg, TerminalInfo, PROMPT_ANSWERED,
    PROMPT_NO_OPTION,
};
use xshelld::server::{Config, PromptClock, Role, ServerHandle, TestHook, TestPoint};

const SID: &str = "11111111-2222-3333-4444-555555555555";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/prompts")
}

fn fixture(name: &str) -> PathBuf {
    let p = fixtures().join(format!("{name}.raw"));
    assert!(p.exists(), "{}", p.display());
    p
}

/// The labels a fixture's sidecar expects.
fn expected(name: &str) -> Vec<String> {
    let v: serde_json::Value =
        serde_json::from_slice(&fs::read(fixtures().join(format!("{name}.json"))).unwrap())
            .unwrap();
    v["expect"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

/// Fake `claude` and `codex`, first in `PATH` once per test binary.
fn prompt_fake_agents() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let bin = Path::new(env!("CARGO_TARGET_TMPDIR")).join("prompt-agents");
        fs::create_dir_all(&bin).unwrap();
        let script = r#"#!/bin/sh
trap '' HUP
echo $$ >> pids.log
echo "$$ $XSHELL_TERMINAL_ID" >> env.log
exec 3<&0
stty -icanon -echo min 1 time 0
trap 'stty size >> sizes.log' WINCH
(
  while :; do
    while [ -f hold ]; do sleep 0.02; done
    k=$(dd bs=1 count=1 <&3 2>/dev/null | od -An -tx1 | tr -d ' \n')
    [ -n "$k" ] || exit 0
    echo "$k" >> keys.log
    if [ -f on-key.sh ]; then . ./on-key.sh; fi
  done
) &
echo "ready $$."
n=0
while :; do
  f="$PWD/ctl.$$.$n"
  if [ -f "$f" ]; then . "$f"; : > "$PWD/ack.$$.$n"; n=$((n + 1)); else sleep 0.02; fi
done
"#;
        for name in ["claude", "codex"] {
            let tmp = bin.join(format!(".{name}.{}", std::process::id()));
            fs::write(&tmp, script).unwrap();
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

/// Holds the thread that reaches a [`TestPoint`] (the prompt worker's read by default)
/// while set.
#[derive(Clone, Default)]
struct Hold(Arc<(Mutex<Held>, Condvar)>);

/// The point to hold at, and how many threads reached it since it was set.
type Held = (Option<TestPoint>, usize);

impl Hold {
    fn hook(&self) -> TestHook {
        let me = self.clone();
        TestHook(Arc::new(move |_, p| {
            let (m, cv) = &*me.0;
            let mut g = m.lock().unwrap();
            if g.0 == Some(p) {
                g.1 += 1;
                cv.notify_all();
                let deadline = Instant::now() + T;
                while g.0 == Some(p) && Instant::now() < deadline {
                    g = cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
                }
            }
            false
        }))
    }

    fn set(&self, on: bool) {
        self.at(on.then_some(TestPoint::PromptRead));
    }

    fn at(&self, point: Option<TestPoint>) {
        let (m, cv) = &*self.0;
        let mut g = m.lock().unwrap();
        g.0 = point;
        g.1 = 0;
        cv.notify_all();
    }

    /// Wait until a read is held.
    fn wait_held(&self) {
        let (m, cv) = &*self.0;
        let deadline = Instant::now() + T;
        let mut g = m.lock().unwrap();
        while g.1 == 0 {
            assert!(Instant::now() < deadline, "no prompt read was held");
            g = cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
        }
    }
}

fn fast(c: &mut Config) {
    c.prompt_settle = Duration::from_millis(50);
    c.prompt_grace = Duration::from_secs(1);
    c.prompt_recheck = Duration::from_millis(400);
}

struct Env {
    h: TestHome,
    srv: ServerHandle,
    desk: Client,
    hold: Hold,
    /// The Terminals' working directories, which hold their fakes' logs.
    dirs: Vec<PathBuf>,
    sent: Mutex<std::collections::HashMap<i32, u32>>,
}

impl Drop for Env {
    fn drop(&mut self) {
        for d in &self.dirs {
            drop(FakeReaper(d.join("pids.log")));
        }
    }
}

fn env() -> Env {
    env_with(|_| {})
}

fn env_with(tweak: impl FnOnce(&mut Config)) -> Env {
    prompt_fake_agents();
    let h = TestHome::new();
    let hold = Hold::default();
    let hook = hold.hook();
    let srv = start(&h, |c| {
        fast(c);
        c.test_hook = Some(hook);
        tweak(c);
    });
    let mut desk = Client::connect(&srv.socket);
    desk.hello(range(1, 1));
    Env {
        h,
        srv,
        desk,
        hold,
        dirs: vec![],
        sent: Default::default(),
    }
}

/// One fake agent Terminal: its id and working directory.
#[derive(Clone)]
struct Term {
    id: Uuid,
    cwd: PathBuf,
}

impl Term {
    fn pids(&self) -> Vec<i32> {
        fs::read_to_string(self.cwd.join("pids.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    fn wait_pids(&self, n: usize) {
        let deadline = Instant::now() + T;
        while self.pids().len() < n {
            assert!(Instant::now() < deadline, "the fake agent did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pid(&self) -> i32 {
        *self.pids().last().expect("a fake agent")
    }

    /// The newest run's `XSHELL_TERMINAL_ID` run number.
    fn run(&self) -> u64 {
        let pid = self.pid().to_string();
        let log = fs::read_to_string(self.cwd.join("env.log")).unwrap();
        let line = log
            .lines()
            .rev()
            .find(|l| l.split(' ').next() == Some(pid.as_str()))
            .expect("the run's environment");
        line.rsplit_once('.').unwrap().1.parse().unwrap()
    }

    fn keys(&self) -> Vec<String> {
        fs::read_to_string(self.cwd.join("keys.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Wait until `keys.log` holds `want`, then check it stays so for a while.
    fn expect_keys(&self, want: &[&str]) {
        let deadline = Instant::now() + T;
        while self.keys().len() < want.len() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(self.keys(), want);
    }

    fn sizes(&self) -> Vec<String> {
        fs::read_to_string(self.cwd.join("sizes.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn on_key(&self, body: &str) {
        fs::write(self.cwd.join("on-key.sh"), body).unwrap();
    }
}

impl Env {
    fn open(&mut self, agent: &str) -> Term {
        self.open_agent(agent, false)
    }

    /// An agent Terminal; `held`: its fake reads no key until `hold` is removed.
    fn open_agent(&mut self, agent: &str, held: bool) -> Term {
        let t = self.open_spec(
            |cwd| {
                if held {
                    fs::write(cwd.join("hold"), "").unwrap();
                }
            },
            |cwd| LaunchSpec {
                agent: Some(agent.into()),
                shell_mode: Some("claude".into()),
                session_id: (agent == "claude").then(|| SID.to_string()),
                cwd: cwd.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        t.wait_pids(1);
        t
    }

    fn open_spec(
        &mut self,
        prep: impl FnOnce(&Path),
        spec: impl FnOnce(&Path) -> LaunchSpec,
    ) -> Term {
        let cwd = self.h.project(&format!("p{}", self.dirs.len()));
        make_jsonl(&self.h, &cwd, SID);
        prep(&cwd);
        self.dirs.push(cwd.clone());
        let id = Uuid::new_v4();
        self.desk
            .request(&ClientMsg::TermOpen {
                spec: OpenSpec {
                    terminal: id,
                    launch: spec(&cwd),
                    cols: 100,
                    rows: 30,
                    meta: Default::default(),
                    first_message: None,
                },
            })
            .unwrap();
        Term { id, cwd }
    }

    /// Run `line` in `t`'s newest fake and wait until it has.
    fn run(&self, t: &Term, line: &str) {
        let pid = t.pid();
        let n = {
            let mut sent = self.sent.lock().unwrap();
            let n = sent.entry(pid).or_insert(0);
            *n += 1;
            *n - 1
        };
        let tmp = t.cwd.join(format!(".ctl.{pid}.{n}"));
        fs::write(&tmp, format!("{line}\n")).unwrap();
        fs::rename(&tmp, t.cwd.join(format!("ctl.{pid}.{n}"))).unwrap();
        let ack = t.cwd.join(format!("ack.{pid}.{n}"));
        let deadline = Instant::now() + T;
        while !ack.exists() {
            assert!(Instant::now() < deadline, "{line:?} not run");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn show(&self, t: &Term, name: &str) {
        self.run(t, &format!("cat '{}'", fixture(name).display()));
    }

    /// What the agent's hook (Claude) or its OSC 9 (Codex) reports.
    fn status(&mut self, t: &Term, agent: &str, s: AgentStatus) {
        if agent == "codex" && s == AgentStatus::NeedsYou {
            self.run(t, r"printf '\033]9;Approval requested\007'");
        } else {
            let run = t.run();
            self.desk
                .request(&ClientMsg::TermEvent {
                    terminal: t.id,
                    run,
                    status: s,
                })
                .unwrap();
        }
    }

    fn mobile(&self) -> Client {
        Client::in_process(&self.srv, Role::Mobile)
    }

    /// `t` as a new Desktop connection is told.
    fn info(&self, t: &Term) -> TerminalInfo {
        let mut c = Client::connect(&self.srv.socket);
        let list = c.hello(range(1, 1)).1;
        list.into_iter()
            .find(|i| i.terminal == t.id)
            .expect("listed")
    }

    /// A prompt with `name`'s options on `t`, published, and the same on a Mobile.
    fn published(&mut self, t: &Term, agent: &str, name: &str) -> PermissionPrompt {
        self.show(t, name);
        self.status(t, agent, AgentStatus::NeedsYou);
        let p = wait_prompt(&mut self.desk, t, |p| {
            p.is_some_and(|p| !p.options.is_empty())
        })
        .unwrap();
        let labels: Vec<String> = p.options.iter().map(|o| o.label.clone()).collect();
        assert_eq!(labels, expected(name));
        p
    }
}

/// Wait until `c` is told `t`'s prompt satisfies `pred`.
fn wait_prompt(
    c: &mut Client,
    t: &Term,
    pred: impl Fn(Option<&PermissionPrompt>) -> bool,
) -> Option<PermissionPrompt> {
    let list = c.terminals_where(|l| {
        l.iter()
            .any(|i| i.terminal == t.id && pred(i.permission_prompt.as_ref()))
    });
    list.into_iter()
        .find(|i| i.terminal == t.id)
        .unwrap()
        .permission_prompt
}

fn answer(c: &mut Client, t: &Term, prompt: u64, option: u32) -> Result<serde_json::Value, String> {
    c.request(&ClientMsg::TermAnswer {
        terminal: t.id,
        prompt,
        option,
    })
}

/// Type `data` and wait until the Daemon handled it (requests on one connection are handled
/// in order).
fn type_in(c: &mut Client, t: &Term, data: &str) {
    c.input(t.id, data);
    c.request(&ClientMsg::TermDetach { terminal: t.id })
        .unwrap();
}

#[test]
fn claude_prompt_published_on_needs_you() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open("claude");
    // The screen alone is no prompt: the agent has not said it needs you.
    e.show(&t, "claude-bash-100x30");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(e.info(&t).permission_prompt, None);
    e.status(&t, "claude", AgentStatus::NeedsYou);
    let p = wait_prompt(&mut e.desk, &t, |p| p.is_some()).unwrap();
    assert!(p.text.contains("ls -la /tmp"), "{}", p.text);
    assert!(p.text.ends_with("Do you want to proceed?"), "{}", p.text);
    let labels: Vec<String> = p.options.iter().map(|o| o.label.clone()).collect();
    assert_eq!(labels, expected("claude-bash-100x30"));
    // The Mobile is told the same prompt.
    let mp = wait_prompt(&mut m, &t, |p| p.is_some()).unwrap();
    assert_eq!(mp, p);
    assert!(p.id < 1 << 53);
}

#[test]
fn prompt_found_when_screen_follows_hook() {
    let mut e = env();
    let t = e.open("claude");
    e.status(&t, "claude", AgentStatus::NeedsYou);
    std::thread::sleep(Duration::from_millis(300));
    e.show(&t, "claude-edit-100x30");
    let p = wait_prompt(&mut e.desk, &t, |p| {
        p.is_some_and(|p| !p.options.is_empty())
    })
    .unwrap();
    assert!(p.text.contains("Do you want to make this edit to main.rs?"));
}

#[test]
fn codex_prompt_from_osc9_screen() {
    let mut e = env();
    let t = e.open("codex");
    let p = e.published(&t, "codex", "codex-exec-100x30");
    assert!(p.text.contains("$ ls -la /tmp"), "{}", p.text);
    assert_eq!(e.info(&t).agent_status, Some(AgentStatus::NeedsYou));
}

#[test]
fn mobile_answer_types_key_once() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    wait_prompt(&mut m, &t, |q| q == Some(&p));
    assert_eq!(answer(&mut m, &t, p.id, 1), Ok(serde_json::Value::Null));
    t.expect_keys(&["32"]);
    wait_prompt(&mut m, &t, |q| q.is_none());
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
    // The key is typed once; asking again changes nothing.
    assert_eq!(answer(&mut m, &t, p.id, 1), Err(PROMPT_ANSWERED.into()));
    t.expect_keys(&["32"]);
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(e.info(&t).permission_prompt, None, "not published again");
}

#[test]
fn first_answer_wins_second_already_answered() {
    let mut e = env();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    let mut other = Client::connect(&e.srv.socket);
    other.hello(range(1, 1));
    assert_eq!(
        answer(&mut e.desk, &t, p.id, 0),
        Ok(serde_json::Value::Null)
    );
    assert_eq!(answer(&mut other, &t, p.id, 2), Err(PROMPT_ANSWERED.into()));
    t.expect_keys(&["31"]);

    // At the same time, from two threads.
    let u = e.open("codex");
    let p = e.published(&u, "codex", "codex-exec-100x30");
    let socket = e.srv.socket.clone();
    let threads: Vec<_> = (0..2u32)
        .map(|i| {
            let (socket, u) = (socket.clone(), u.clone());
            std::thread::spawn(move || {
                let mut c = Client::connect(&socket);
                c.hello(range(1, 1));
                answer(&mut c, &u, p.id, i)
            })
        })
        .collect();
    let mut results: Vec<_> = threads.into_iter().map(|h| h.join().unwrap()).collect();
    results.sort_by_key(|r| r.is_err());
    assert_eq!(
        results,
        vec![Ok(serde_json::Value::Null), Err(PROMPT_ANSWERED.into())]
    );
    let deadline = Instant::now() + T;
    while u.keys().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    let keys = u.keys();
    assert!(keys == ["31"] || keys == ["32"], "{keys:?}");
}

#[test]
fn desktop_typing_first_makes_answer_stale() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    wait_prompt(&mut m, &t, |q| q == Some(&p));
    type_in(&mut e.desk, &t, "1");
    wait_prompt(&mut m, &t, |q| q.is_none());
    assert_eq!(answer(&mut m, &t, p.id, 1), Err(PROMPT_ANSWERED.into()));
    t.expect_keys(&["31"]);
}

#[test]
fn non_answer_input_keeps_prompt() {
    let mut e = env();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    for input in [
        "\x1b[I",
        "\x1b[O",
        "\x1b[12;40R",
        "\x1b[A",
        "\x1bOB",
        "\x1b[I\x1b[O",
    ] {
        type_in(&mut e.desk, &t, input);
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(e.info(&t).permission_prompt, Some(p.clone()));
    // A focus report with Enter is typing.
    type_in(&mut e.desk, &t, "\x1b[I\r");
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
    assert_eq!(
        answer(&mut e.desk, &t, p.id, 0),
        Err(PROMPT_ANSWERED.into())
    );
}

#[test]
fn status_leaving_needs_you_clears_in_same_list() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    wait_prompt(&mut m, &t, |q| q == Some(&p));
    e.status(&t, "claude", AgentStatus::Working);
    for c in [&mut e.desk, &mut m] {
        let list = c.terminals_where(|l| {
            l.iter()
                .any(|i| i.terminal == t.id && i.agent_status == Some(AgentStatus::Working))
        });
        let i = list.iter().find(|i| i.terminal == t.id).unwrap();
        assert_eq!(
            i.permission_prompt, None,
            "the status and the prompt in one list"
        );
    }
    assert_eq!(answer(&mut m, &t, p.id, 0), Err(PROMPT_ANSWERED.into()));
}

#[test]
fn screen_change_clears_prompt_without_text_only() {
    let mut e = env();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    e.run(&t, r"printf '\033[2J\033[Hsomething else\r\n'");
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
    assert_eq!(
        answer(&mut e.desk, &t, p.id, 0),
        Err(PROMPT_ANSWERED.into())
    );
    // Past the grace period: still nothing (the episode is spent).
    std::thread::sleep(Duration::from_millis(1500));
    let i = e.info(&t);
    assert_eq!(i.agent_status, Some(AgentStatus::NeedsYou));
    assert_eq!(i.permission_prompt, None);
    assert!(t.keys().is_empty());
}

#[test]
fn unknown_screen_gives_text_only_prompt() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open("claude");
    e.show(&t, "unknown-elicitation");
    let asked = Instant::now();
    e.status(&t, "claude", AgentStatus::NeedsYou);
    let p = wait_prompt(&mut m, &t, |p| p.is_some()).unwrap();
    assert!(
        asked.elapsed() >= Duration::from_millis(900),
        "after the grace period"
    );
    assert_eq!(p.options, vec![]);
    assert!(
        p.text.contains("Which project should the issue go to?"),
        "{}",
        p.text
    );
    assert!(!p.text.contains('│'), "{}", p.text);
    assert_eq!(answer(&mut m, &t, p.id, 0), Err(PROMPT_NO_OPTION.into()));
    std::thread::sleep(Duration::from_millis(300));
    assert!(t.keys().is_empty());
}

#[test]
fn ignored_answer_republished_with_new_id() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open("claude");
    // The TUI reads the key and draws the same dialog again.
    t.on_key(&format!(
        "cat '{}'\n",
        fixture("claude-bash-100x30").display()
    ));
    let p = e.published(&t, "claude", "claude-bash-100x30");
    wait_prompt(&mut m, &t, |q| q == Some(&p));
    assert_eq!(answer(&mut m, &t, p.id, 0), Ok(serde_json::Value::Null));
    wait_prompt(&mut m, &t, |q| q.is_none());
    let again = wait_prompt(&mut m, &t, |q| q.is_some_and(|q| q.id > p.id)).unwrap();
    assert_eq!(again.options, p.options);
    assert_eq!(answer(&mut m, &t, p.id, 0), Err(PROMPT_ANSWERED.into()));
    t.expect_keys(&["31"]);
}

/// A1: an answer to an old id is fenced against the live screen, before the worker reads it:
/// prompt A leaves the screen and an identical-looking B follows.
#[test]
fn answer_fenced_by_screen_before_worker_settles() {
    let mut e = env();
    let t = e.open("claude");
    let a = e.published(&t, "claude", "claude-bash-100x30");
    e.hold.set(true);
    e.run(
        &t,
        &format!(
            r"printf '\033[2J\033[H'; sleep 0.1; cat '{}'",
            fixture("claude-bash-100x30").display()
        ),
    );
    e.hold.wait_held();
    // The worker has not published anything since A, but A is not answerable.
    assert_eq!(
        answer(&mut e.desk, &t, a.id, 0),
        Err(PROMPT_ANSWERED.into())
    );
    e.hold.set(false);
    let b = wait_prompt(&mut e.desk, &t, |q| q.is_some_and(|q| q.id > a.id)).unwrap();
    assert_eq!(b.options, a.options);
    assert_eq!(
        answer(&mut e.desk, &t, b.id, 0),
        Ok(serde_json::Value::Null)
    );
    t.expect_keys(&["31"]);
}

/// A2: a read taken before an answer never brings the answered prompt back.
#[test]
fn worker_read_before_answer_is_discarded() {
    let mut e = env();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    e.hold.set(true);
    // Output that changes nothing visible makes the worker read the screen.
    e.run(&t, r"printf '\033[?25l'");
    e.hold.wait_held();
    type_in(&mut e.desk, &t, "1");
    e.hold.set(false);
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(e.info(&t).permission_prompt, None, "resurrected");
    assert_eq!(
        answer(&mut e.desk, &t, p.id, 0),
        Err(PROMPT_ANSWERED.into())
    );
    t.expect_keys(&["31"]);
}

/// A6: while the TUI has not read the answer, the prompt stays answered; when it reads it
/// late and moves on, it stays so.
#[test]
fn late_consumption_is_not_republished() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open_agent("claude", true);
    t.on_key("printf '\\033[2J\\033[Hrunning\\r\\n'\n");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    assert_eq!(answer(&mut m, &t, p.id, 0), Ok(serde_json::Value::Null));
    wait_prompt(&mut m, &t, |q| q.is_none());
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        assert_eq!(
            e.info(&t).permission_prompt,
            None,
            "republished before it was read"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    fs::remove_file(t.cwd.join("hold")).unwrap();
    t.expect_keys(&["31"]);
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(e.info(&t).permission_prompt, None);
}

/// Story 18: an answer never takes the Terminal's size, also from a Mobile that recorded a
/// Terminal View size.
#[test]
fn answer_never_claims_size() {
    let mut e = env();
    let t = e.open("claude");
    e.desk.resize(t.id, 100, 30);
    e.desk.attach(t.id);
    let p = e.published(&t, "claude", "claude-bash-100x30");
    // The attach's redraw nudge has passed.
    std::thread::sleep(Duration::from_millis(500));
    let sizes = t.sizes();
    let mut m = e.mobile();
    m.attach(t.id);
    m.resize(t.id, 40, 20);
    assert_eq!(answer(&mut m, &t, p.id, 1), Ok(serde_json::Value::Null));
    t.expect_keys(&["32"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(t.sizes(), sizes, "a SIGWINCH");
    assert!(
        !m.log
            .iter()
            .any(|ev| matches!(ev, Ev::Msg(ServerMsg::TermSize { .. }))),
        "{:?}",
        m.summary()
    );
    let state = e.h.state_json();
    let s = state["terminals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["terminal"] == t.id.to_string())
        .unwrap();
    assert_eq!(
        (s["cols"].as_u64(), s["rows"].as_u64()),
        (Some(100), Some(30))
    );
}

#[test]
fn relaunch_drops_prompt() {
    let mut e = env();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    e.desk
        .request(&ClientMsg::TermRelaunch {
            terminal: t.id,
            skip_permissions: true,
        })
        .unwrap();
    t.wait_pids(2);
    let i = e.info(&t);
    assert_eq!(i.permission_prompt, None);
    assert_eq!(
        answer(&mut e.desk, &t, p.id, 0),
        Err(PROMPT_ANSWERED.into())
    );
    // The new run's prompt ids continue above the old run's.
    let q = e.published(&t, "claude", "claude-bash-100x30");
    assert!(q.id > p.id);
    assert!(t.keys().is_empty());
}

/// A prompt-id clock of this test's own, set to `ms`.
fn test_clock(ms: u64) -> (Arc<AtomicU64>, PromptClock) {
    let at = Arc::new(AtomicU64::new(ms));
    let c = at.clone();
    (at, PromptClock(Arc::new(move || c.load(Ordering::SeqCst))))
}

/// A7: prompt ids stay above every id handed out before a restart, with the clock set back.
#[test]
fn restart_with_clock_set_back_keeps_ids_increasing() {
    let (at, clock) = test_clock(4_000_000_000_000);
    let c2 = clock.clone();
    let mut e = env_with(|c| c.prompt_clock = Some(c2));
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    assert!(p.id >= 4_000_000_000_000);
    e.srv.stopper().shutdown();
    assert!(e.srv.wait_timeout(T).is_some());
    let state = e.h.state_json();
    assert!(state["terminals"][0]["promptIdFloor"].as_u64().unwrap() >= p.id);
    at.store(1_000, Ordering::SeqCst);
    let hook = e.hold.hook();
    e.srv = start(&e.h, |c| {
        fast(c);
        c.prompt_clock = Some(clock);
        c.test_hook = Some(hook);
    });
    e.desk = Client::connect(&e.srv.socket);
    e.desk.hello(range(1, 1));
    t.wait_pids(2);
    let q = e.published(&t, "claude", "claude-bash-100x30");
    assert!(q.id > p.id, "{} <= {}", q.id, p.id);
}

#[test]
fn shell_terminal_never_scanned() {
    let mut e = env();
    let t = e.open_spec(|_| {}, |cwd| raw_spec(cwd, Path::new("/bin/sh")));
    e.desk.input(
        t.id,
        &format!(
            "cat '{}'; printf '\\033]9;Approval requested\\007'\n",
            fixture("claude-bash-100x30").display()
        ),
    );
    std::thread::sleep(Duration::from_millis(1500));
    let i = e.info(&t);
    assert_eq!((i.agent_status, i.permission_prompt), (None, None));
    assert_eq!(answer(&mut e.desk, &t, 1, 0), Err(PROMPT_ANSWERED.into()));
}

/// Review 1: the screen is held from an answer's check until its key is queued, so output
/// that arrives in between is only applied after the key.
#[test]
fn answer_check_and_queueing_are_one_step() {
    let mut e = env();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    let mut m = e.mobile();
    e.hold.at(Some(TestPoint::AnswerChecked));
    let tt = t.clone();
    let answering = std::thread::spawn(move || answer(&mut m, &tt, p.id, 0));
    e.hold.wait_held();
    let before = e.srv.screen_rev(t.id).unwrap();
    // The TUI redraws while the answer is between its check and its key.
    e.run(&t, r"printf '\033[2J\033[Hredrawn\r\n'");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        e.srv.screen_rev(t.id),
        Some(before),
        "the screen moved under the answer"
    );
    e.hold.at(None);
    assert_eq!(answering.join().unwrap(), Ok(serde_json::Value::Null));
    t.expect_keys(&["31"]);
    let deadline = Instant::now() + T;
    while e.srv.screen_rev(t.id) == Some(before) {
        assert!(Instant::now() < deadline, "the redraw was never applied");
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
}

/// Review 2: typing before the first prompt was published answers that needs-you; only a
/// new needs-you brings buttons.
#[test]
fn typing_before_publication_gives_no_buttons_that_episode() {
    let mut e = env();
    let t = e.open("claude");
    e.status(&t, "claude", AgentStatus::NeedsYou);
    type_in(&mut e.desk, &t, "2");
    e.show(&t, "claude-bash-100x30");
    std::thread::sleep(Duration::from_millis(1500));
    let i = e.info(&t);
    assert_eq!(i.agent_status, Some(AgentStatus::NeedsYou));
    assert_eq!(i.permission_prompt, None, "buttons after the user typed");
    t.expect_keys(&["32"]);
    e.status(&t, "claude", AgentStatus::Working);
    e.status(&t, "claude", AgentStatus::NeedsYou);
    wait_prompt(&mut e.desk, &t, |q| {
        q.is_some_and(|q| !q.options.is_empty())
    });
}

/// Review 5: a new id is persisted before the prompt is listed or answerable, and that save
/// blocks nothing else; an older snapshot never replaces a newer state file.
#[test]
fn prompt_listed_only_after_its_id_floor_is_saved() {
    let (_at, clock) = test_clock(5_000_000_000_000);
    let mut e = env_with(|c| c.prompt_clock = Some(clock));
    let t = e.open("claude");
    e.hold.at(Some(TestPoint::PromptPersist));
    e.show(&t, "claude-bash-100x30");
    e.status(&t, "claude", AgentStatus::NeedsYou);
    e.hold.wait_held();
    let id = 5_000_000_000_000;
    assert_eq!(e.info(&t).permission_prompt, None);
    assert_eq!(answer(&mut e.desk, &t, id, 0), Err(PROMPT_ANSWERED.into()));
    // While that save is held: another Terminal opens (and is saved), a new connection is
    // served.
    let started = Instant::now();
    let u = e.open("codex");
    let mut c = Client::connect(&e.srv.socket);
    let list = c.hello(range(1, 1)).1;
    assert_eq!(list.len(), 2);
    assert!(started.elapsed() < Duration::from_secs(3));
    e.hold.at(None);
    let p = wait_prompt(&mut e.desk, &t, |q| q.is_some()).unwrap();
    assert_eq!(p.id, id);
    let state = e.h.state_json();
    let saved = state["terminals"].as_array().unwrap();
    assert_eq!(
        saved.len(),
        2,
        "the held, older snapshot replaced a newer one"
    );
    let floor = saved
        .iter()
        .find(|s| s["terminal"] == t.id.to_string())
        .unwrap()["promptIdFloor"]
        .as_u64()
        .unwrap();
    assert!(floor >= id);
    assert!(saved.iter().any(|s| s["terminal"] == u.id.to_string()));
    assert!(t.keys().is_empty());
}

/// Review 5: when the floor cannot be saved, nothing is listed (fail closed).
#[test]
fn prompt_not_listed_when_its_id_floor_cannot_be_saved() {
    let mut e = env();
    let t = e.open("claude");
    let dir = e.h.paths().state.parent().unwrap().to_path_buf();
    let mode = fs::metadata(&dir).unwrap().permissions().mode();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    e.show(&t, "claude-bash-100x30");
    e.status(&t, "claude", AgentStatus::NeedsYou);
    std::thread::sleep(Duration::from_millis(1500));
    let listed = e.info(&t).permission_prompt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
    assert_eq!(listed, None);
    // Saving works again: the next read lists it.
    e.run(&t, r"printf '\033[?25l'");
    wait_prompt(&mut e.desk, &t, |q| {
        q.is_some_and(|q| !q.options.is_empty())
    });
}

/// Review 6: a wrapped or prefixed agent is never scanned, whatever it prints and reports.
#[test]
fn wrapped_and_prefixed_agents_never_scanned() {
    let mut e = env();
    let codex = |cwd: &Path| LaunchSpec {
        agent: Some("codex".into()),
        shell_mode: Some("claude".into()),
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let wrapped = e.open_spec(
        |_| {},
        |cwd| LaunchSpec {
            shell_id: Some("bash".into()),
            shell_command: Some("/bin/sh".into()),
            ..codex(cwd)
        },
    );
    let prefixed = e.open_spec(
        |_| {},
        |cwd| LaunchSpec {
            launch_prefix: Some(vec!["env".into()]),
            ..codex(cwd)
        },
    );
    for t in [&wrapped, &prefixed] {
        t.wait_pids(1);
        e.show(t, "codex-exec-100x30");
        e.status(t, "codex", AgentStatus::NeedsYou);
    }
    std::thread::sleep(Duration::from_millis(1500));
    for t in [&wrapped, &prefixed] {
        let i = e.info(t);
        assert_eq!(i.agent_status, Some(AgentStatus::NeedsYou));
        assert_eq!(i.permission_prompt, None);
    }
}

/// Review 7: a digit typed at a Codex prompt (which also says working) is still rechecked:
/// the TUI ignored it and drew the prompt again, so it is published again.
#[test]
fn desktop_codex_digit_ignored_is_republished() {
    let mut e = env();
    let t = e.open("codex");
    t.on_key(&format!(
        "cat '{}'\n",
        fixture("codex-exec-100x30").display()
    ));
    let p = e.published(&t, "codex", "codex-exec-100x30");
    type_in(&mut e.desk, &t, "1");
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
    let again = wait_prompt(&mut e.desk, &t, |q| q.is_some_and(|q| q.id > p.id)).unwrap();
    assert_eq!(again.options, p.options);
    t.expect_keys(&["31"]);
}

/// Review (re-review 3): a prompt that left the screen by itself does not close the episode:
/// the next one, even identical-looking, is published under a new id.
#[test]
fn prompt_after_a_blank_screen_is_published() {
    let mut e = env();
    let t = e.open("claude");
    let a = e.published(&t, "claude", "claude-bash-100x30");
    e.run(&t, r"printf '\033[2J\033[H'");
    // The worker read the blank screen.
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
    std::thread::sleep(Duration::from_millis(200));
    e.show(&t, "claude-bash-100x30");
    let b = wait_prompt(&mut e.desk, &t, |q| q.is_some_and(|q| q.id > a.id)).unwrap();
    assert_eq!(b.options, a.options);
    assert_eq!(
        answer(&mut e.desk, &t, a.id, 0),
        Err(PROMPT_ANSWERED.into())
    );
    assert_eq!(
        answer(&mut e.desk, &t, b.id, 0),
        Ok(serde_json::Value::Null)
    );
    t.expect_keys(&["31"]);
}

/// Review (re-review 2): a command's prompt that contains a key-hint fragment, printed after
/// the dialog, ends it: no buttons, and the old prompt's answer is refused.
#[test]
fn hint_fragment_in_later_output_ends_the_prompt() {
    let mut e = env();
    let t = e.open("claude");
    let p = e.published(&t, "claude", "claude-bash-100x30");
    e.run(
        &t,
        r"printf '\r\nBuild finished; enter a number to select a target > '",
    );
    wait_prompt(&mut e.desk, &t, |q| q.is_none());
    assert_eq!(
        answer(&mut e.desk, &t, p.id, 0),
        Err(PROMPT_ANSWERED.into())
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(e.info(&t).permission_prompt, None);
    assert!(t.keys().is_empty());
}

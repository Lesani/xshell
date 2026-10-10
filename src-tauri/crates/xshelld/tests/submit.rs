#![cfg(unix)]
//! Chat View replies (`term.submit`, capability `term.submit`), end to end against an
//! in-process server.
//!
//! The shared fake `claude` and `codex` source the working directory's `agent.sh`: it puts
//! the tty in raw mode, appends every byte it reads to `input.bin`, appends `stty size` to
//! `sizes.log` on every SIGWINCH, and then runs control files (`ctl.<n>`, in order,
//! acknowledged with `ack.<n>`). The screens it shows are the composer and prompt fixtures of
//! `xshell-core` (`crates/core/tests/fixtures/prompts`), and it turns bracketed paste on and
//! off as the real agents do.

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::AgentStatus;
use xshell_core::launch::LaunchSpec;
use xshell_protocol::msg::{
    ClientMsg, OpenSpec, ServerMsg, SUBMIT_MAX_BYTES, SUBMIT_NEEDS_YOU, SUBMIT_NOT_CHAT,
    SUBMIT_NOT_READY, SUBMIT_NO_NONBLOCK, SUBMIT_STUCK, SUBMIT_UNCONFIRMED,
};
use xshelld::server::{Config, Role, ServerHandle, TestHook, TestPoint};

const SID: &str = "11111111-2222-3333-4444-555555555555";

const AGENT: &str = r#"exec 3<&0
stty raw -echo
trap 'stty size >> sizes.log' WINCH
cat <&3 >> input.bin &
echo $! >> pids.log
echo "$XSHELL_TERMINAL_ID" > terminal.id
: > started
n=0
while :; do
  if [ -f "ctl.$n" ]; then . "./ctl.$n"; : > "ack.$n"; n=$((n + 1)); else sleep 0.02; fi
done
"#;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn fixture(name: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/fixtures/prompts")
        .join(format!("{name}.raw"));
    assert!(p.exists(), "{}", p.display());
    p
}

/// The bytes a reply of `text` is typed as.
fn typed(text: &str) -> Vec<u8> {
    let mut v = b"\x1b[200~".to_vec();
    v.extend_from_slice(text.as_bytes());
    v.extend_from_slice(b"\x1b[201~\r");
    v
}

/// Holds the thread that reaches a [`TestPoint`] while it is set.
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
                    g = cv.wait_timeout(g, ms(50)).unwrap().0;
                }
            }
            false
        }))
    }

    fn at(&self, point: Option<TestPoint>) {
        let (m, cv) = &*self.0;
        let mut g = m.lock().unwrap();
        g.0 = point;
        g.1 = 0;
        cv.notify_all();
    }

    fn wait_held(&self) {
        let (m, cv) = &*self.0;
        let deadline = Instant::now() + T;
        let mut g = m.lock().unwrap();
        while g.1 == 0 {
            assert!(Instant::now() < deadline, "nothing was held");
            g = cv.wait_timeout(g, ms(50)).unwrap().0;
        }
    }
}

struct Env {
    h: TestHome,
    srv: ServerHandle,
    desk: Client,
    hold: Hold,
    reapers: Vec<FakeReaper>,
}

/// One fake agent Terminal.
#[derive(Clone)]
struct Term {
    id: Uuid,
    cwd: PathBuf,
    agent: &'static str,
}

impl Term {
    fn file(&self, name: &str) -> Vec<u8> {
        fs::read(self.cwd.join(name)).unwrap_or_default()
    }

    fn input(&self) -> Vec<u8> {
        self.file("input.bin")
    }

    fn sizes(&self) -> String {
        String::from_utf8(self.file("sizes.log")).unwrap()
    }

    /// Wait until `input.bin` is `want`, then check it stays so for a while.
    fn expect_input(&self, want: &[u8]) {
        let deadline = Instant::now() + T;
        while self.input().len() < want.len() && Instant::now() < deadline {
            std::thread::sleep(ms(20));
        }
        std::thread::sleep(ms(300));
        assert_eq!(
            String::from_utf8_lossy(&self.input()),
            String::from_utf8_lossy(want)
        );
    }

    /// Run `line` in the fake and wait until it has.
    fn run(&self, line: &str) {
        let n = (0..)
            .find(|n| !self.cwd.join(format!("ctl.{n}")).exists())
            .unwrap();
        let tmp = self.cwd.join(format!(".ctl.{n}"));
        fs::write(&tmp, format!("{line}\n")).unwrap();
        fs::rename(&tmp, self.cwd.join(format!("ctl.{n}"))).unwrap();
        let ack = self.cwd.join(format!("ack.{n}"));
        let deadline = Instant::now() + T;
        while !ack.exists() {
            assert!(Instant::now() < deadline, "{line:?} not run");
            std::thread::sleep(ms(10));
        }
    }

    /// Show fixture `name`, with bracketed paste on (`paste`) or off.
    fn show(&self, name: &str, paste: bool) {
        let mode = if paste { 'h' } else { 'l' };
        self.run(&format!(
            "cat '{}'; printf '\\033[?2004{mode}'",
            fixture(name).display()
        ));
    }

    fn composer(&self) -> &'static str {
        match self.agent {
            "codex" => "codex-composer-idle",
            _ => "claude-composer-idle",
        }
    }
}

fn env() -> Env {
    env_with(|_| {})
}

fn env_with(tweak: impl FnOnce(&mut Config)) -> Env {
    shared_fake_agents();
    let h = TestHome::new();
    let hold = Hold::default();
    let hook = hold.hook();
    let srv = start(&h, |c| {
        c.test_hook = Some(hook);
        tweak(c);
    });
    let desk = Client::in_process(&srv, Role::Desktop);
    Env {
        h,
        srv,
        desk,
        hold,
        reapers: vec![],
    }
}

impl Env {
    fn open(&mut self, agent: &'static str) -> Term {
        let spec = |cwd: &Path| LaunchSpec {
            agent: Some(agent.into()),
            shell_mode: Some("claude".into()),
            session_id: (agent == "claude").then(|| SID.to_string()),
            cwd: cwd.to_string_lossy().into_owned(),
            ..Default::default()
        };
        self.open_spec(agent, spec)
    }

    fn open_spec(&mut self, agent: &'static str, spec: impl FnOnce(&Path) -> LaunchSpec) -> Term {
        let cwd = self.h.project(&format!("p{}", self.reapers.len()));
        fs::write(cwd.join("agent.sh"), AGENT).unwrap();
        make_jsonl(&self.h, &cwd, SID);
        self.reapers.push(FakeReaper(cwd.join("pids.log")));
        let id = Uuid::new_v4();
        // The size the fixtures are drawn at.
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
        let deadline = Instant::now() + T;
        while !cwd.join("started").exists() {
            assert!(Instant::now() < deadline, "the fake agent did not start");
            std::thread::sleep(ms(20));
        }
        Term { id, cwd, agent }
    }

    /// `t` shows its agent's composer with bracketed paste on, and the Daemon sees it.
    fn ready(&mut self, agent: &'static str) -> Term {
        let t = self.open(agent);
        t.show(t.composer(), true);
        self.wait_ready(&t, Ok(()));
        t
    }

    /// Wait until the Daemon would answer a reply to `t` with `want` (its screen fed).
    fn wait_ready(&self, t: &Term, want: Result<(), &str>) {
        let want = want.map_err(str::to_string);
        let deadline = Instant::now() + T;
        loop {
            let got = self.srv.reply_ready(t.id).expect("listed");
            if got == want {
                return;
            }
            assert!(Instant::now() < deadline, "{got:?}, not {want:?}");
            std::thread::sleep(ms(20));
        }
    }

    fn mobile(&self) -> Client {
        Client::in_process(&self.srv, Role::Mobile)
    }

    /// What the agent's hook reports (Claude), or its own OSC 9 (Codex, needs you).
    fn status(&mut self, t: &Term, s: AgentStatus) {
        if t.agent == "codex" && s == AgentStatus::NeedsYou {
            t.run(r"printf '\033]9;Approval requested\007'");
            let deadline = Instant::now() + T;
            while self.info(t).agent_status != Some(s) {
                assert!(Instant::now() < deadline, "no needs-you");
                std::thread::sleep(ms(20));
            }
            return;
        }
        let run = self.run_of(t);
        self.desk
            .request(&ClientMsg::TermEvent {
                terminal: t.id,
                run,
                status: s,
                session_id: None,
            })
            .unwrap();
    }

    /// The Terminal's run, as its hooks are told (`XSHELL_TERMINAL_ID`, `<id>.<run>`).
    fn run_of(&self, t: &Term) -> u64 {
        let id = String::from_utf8(t.file("terminal.id")).unwrap();
        id.trim().rsplit_once('.').unwrap().1.parse().unwrap()
    }

    /// `t` as a new Desktop connection is told.
    fn info(&self, t: &Term) -> xshell_protocol::msg::TerminalInfo {
        let mut c = Client::connect(&self.srv.socket);
        let list = c.hello(range(1, 1)).1;
        list.into_iter()
            .find(|i| i.terminal == t.id)
            .expect("listed")
    }
}

fn submit(c: &mut Client, t: &Term, text: &str) -> Result<Value, String> {
    c.request(&ClientMsg::TermSubmit {
        terminal: t.id,
        text: text.into(),
    })
}

/// Send a `term.submit` without waiting for its answer; its request id.
fn send_submit(c: &mut Client, t: &Term, text: &str) -> u64 {
    let id = c.request_id();
    c.send(
        &ClientMsg::TermSubmit {
            terminal: t.id,
            text: text.into(),
        },
        Some(id),
    );
    id
}

#[test]
fn submit_types_paste_then_enter() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.ready("claude");
    assert_eq!(submit(&mut m, &t, "fix it\r\nplease 🦀"), Ok(Value::Null));
    t.expect_input(&typed("fix it\nplease 🦀"));
    // A Desktop may reply too, and the composer is still there for the next one.
    assert_eq!(submit(&mut e.desk, &t, "/compact"), Ok(Value::Null));
    let mut want = typed("fix it\nplease 🦀");
    want.extend(typed("/compact"));
    t.expect_input(&want);
}

#[test]
fn submit_to_codex_composer_marks_working() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.ready("codex");
    e.status(&t, AgentStatus::Finished);
    assert_eq!(e.info(&t).agent_status, Some(AgentStatus::Finished));
    assert_eq!(submit(&mut m, &t, "next step"), Ok(Value::Null));
    t.expect_input(&typed("next step"));
    let deadline = Instant::now() + T;
    while e.info(&t).agent_status != Some(AgentStatus::Working) {
        assert!(Instant::now() < deadline, "the turn did not start");
        std::thread::sleep(ms(20));
    }
}

#[test]
fn submit_refused_without_bracketed_paste() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.open("claude");
    // The composer, but bracketed paste off: `[200~` would show in it.
    t.show("claude-composer-idle", false);
    std::thread::sleep(ms(200));
    assert_eq!(submit(&mut m, &t, "hi"), Err(SUBMIT_NOT_READY.into()));
    // On, then off again.
    t.show("claude-composer-idle", true);
    e.wait_ready(&t, Ok(()));
    t.run(r"printf '\033[?2004l'");
    e.wait_ready(&t, Err(SUBMIT_NOT_READY));
    assert_eq!(submit(&mut m, &t, "hi"), Err(SUBMIT_NOT_READY.into()));
    // A reset turns it off too.
    t.show("claude-composer-idle", true);
    e.wait_ready(&t, Ok(()));
    t.run(&format!(
        r"printf '\033c'; cat '{}'",
        fixture("claude-composer-idle").display()
    ));
    e.wait_ready(&t, Err(SUBMIT_NOT_READY));
    assert_eq!(submit(&mut m, &t, "hi"), Err(SUBMIT_NOT_READY.into()));
    t.expect_input(b"");
}

/// A: readiness is the composer itself, not merely no needs-you and bracketed paste on.
#[test]
fn submit_refused_without_the_composer() {
    let mut e = env();
    let mut m = e.mobile();
    for (agent, screens) in [
        (
            "claude",
            &[
                "claude-model-picker",
                "claude-bash-mode",
                "unknown-elicitation",
            ][..],
        ),
        ("codex", &["codex-model-picker"][..]),
    ] {
        let t = e.open(agent);
        // Before the agent drew anything.
        t.run(r"printf '\033[?2004h'");
        e.wait_ready(&t, Err(SUBMIT_NOT_READY));
        assert_eq!(submit(&mut m, &t, "1"), Err(SUBMIT_NOT_READY.into()));
        for s in screens {
            t.show(s, true);
            std::thread::sleep(ms(150));
            e.wait_ready(&t, Err(SUBMIT_NOT_READY));
            assert_eq!(submit(&mut m, &t, "1"), Err(SUBMIT_NOT_READY.into()), "{s}");
        }
        // Back at the composer it is accepted.
        t.show(t.composer(), true);
        e.wait_ready(&t, Ok(()));
        assert_eq!(submit(&mut m, &t, "ok"), Ok(Value::Null));
        t.expect_input(&typed("ok"));
    }
}

#[test]
fn submit_refused_while_needs_you() {
    let mut e = env();
    let mut m = e.mobile();
    for agent in ["claude", "codex"] {
        let t = e.ready(agent);
        e.status(&t, AgentStatus::NeedsYou);
        assert_eq!(submit(&mut m, &t, "2"), Err(SUBMIT_NEEDS_YOU.into()));
        e.status(&t, AgentStatus::Working);
        e.wait_ready(&t, Ok(()));
        assert_eq!(submit(&mut m, &t, "go on"), Ok(Value::Null));
        t.expect_input(&typed("go on"));
    }
}

/// A Permission Prompt on the screen refuses a reply before any status says so, and one
/// that is current keeps refusing it after the status moved on, until it is gone.
#[test]
fn submit_refused_while_a_prompt_shows() {
    let mut e = env_with(|c| {
        c.prompt_settle = ms(50);
        c.prompt_grace = Duration::from_secs(1);
    });
    let mut m = e.mobile();
    let t = e.ready("claude");
    t.show("claude-bash-100x30", true);
    e.wait_ready(&t, Err(SUBMIT_NEEDS_YOU));
    assert_eq!(submit(&mut m, &t, "1"), Err(SUBMIT_NEEDS_YOU.into()));
    // Listed as the current prompt.
    e.status(&t, AgentStatus::NeedsYou);
    let deadline = Instant::now() + T;
    while e.info(&t).permission_prompt.is_none() {
        assert!(Instant::now() < deadline, "no prompt listed");
        std::thread::sleep(ms(20));
    }
    assert_eq!(submit(&mut m, &t, "1"), Err(SUBMIT_NEEDS_YOU.into()));
    // Answered: the composer is back.
    e.status(&t, AgentStatus::Working);
    t.show("claude-composer-working", true);
    e.wait_ready(&t, Ok(()));
    assert_eq!(submit(&mut m, &t, "and then"), Ok(Value::Null));
    t.expect_input(&typed("and then"));
}

/// The window between admission and the paste: the agent starts needing you, shows a
/// prompt, or leaves its composer; nothing is typed and the refusal says why.
#[test]
fn readiness_is_checked_again_before_the_paste() {
    let mut e = env();
    let mut m = e.mobile();
    type Change = fn(&mut Env, &Term);
    let changes: [(&str, Change, &str); 3] = [
        (
            "needs you",
            |e, t| e.status(t, AgentStatus::NeedsYou),
            SUBMIT_NEEDS_YOU,
        ),
        (
            "prompt",
            |e, t| {
                t.show("claude-bash-100x30", true);
                e.wait_ready(t, Err(SUBMIT_NEEDS_YOU));
            },
            SUBMIT_NEEDS_YOU,
        ),
        (
            "picker",
            |e, t| {
                t.show("claude-model-picker", true);
                e.wait_ready(t, Err(SUBMIT_NOT_READY));
            },
            SUBMIT_NOT_READY,
        ),
    ];
    for (what, change, refusal) in changes {
        let t = e.ready("claude");
        e.hold.at(Some(TestPoint::SubmitPaste));
        let id = send_submit(&mut m, &t, "yes");
        e.hold.wait_held();
        change(&mut e, &t);
        e.hold.at(None);
        assert_eq!(m.wait_res(id), Err(refusal.into()), "{what}");
        t.expect_input(b"");
        assert_ne!(
            e.info(&t).agent_status,
            Some(AgentStatus::Working),
            "{what}"
        );
    }
}

/// The window between the paste and Enter: Enter is not typed, and the answer says the
/// outcome is unknown (the text may sit in the composer). The status does not follow an
/// Enter that was never typed.
#[test]
fn readiness_is_checked_again_before_enter() {
    let mut e = env();
    let mut m = e.mobile();
    for agent in ["claude", "codex"] {
        let t = e.ready(agent);
        e.hold.at(Some(TestPoint::SubmitEnter));
        let id = send_submit(&mut m, &t, "3");
        e.hold.wait_held();
        let paste = b"\x1b[200~3\x1b[201~";
        t.expect_input(paste);
        e.status(&t, AgentStatus::NeedsYou);
        e.hold.at(None);
        assert_eq!(m.wait_res(id), Err(SUBMIT_UNCONFIRMED.into()), "{agent}");
        t.expect_input(paste);
        assert_eq!(e.info(&t).agent_status, Some(AgentStatus::NeedsYou));
    }
    // The composer went away instead (Claude: a picker).
    let t = e.ready("claude");
    e.hold.at(Some(TestPoint::SubmitEnter));
    let id = send_submit(&mut m, &t, "x");
    e.hold.wait_held();
    t.show("claude-model-picker", true);
    e.wait_ready(&t, Err(SUBMIT_NOT_READY));
    e.hold.at(None);
    assert_eq!(m.wait_res(id), Err(SUBMIT_UNCONFIRMED.into()));
    t.expect_input(b"\x1b[200~x\x1b[201~");
}

/// The answer comes once the reply is typed, Enter included, and the pause keeps Enter
/// apart from the paste.
#[test]
fn answer_follows_delivery() {
    let mut e = env_with(|c| c.submit_enter_delay = ms(300));
    let mut m = e.mobile();
    let t = e.ready("claude");
    e.hold.at(Some(TestPoint::Submitted));
    let started = Instant::now();
    let id = send_submit(&mut m, &t, "done?");
    e.hold.wait_held();
    assert!(started.elapsed() >= ms(300), "{:?}", started.elapsed());
    t.expect_input(&typed("done?"));
    assert!(m
        .try_msg(
            ms(300),
            |msg| matches!(msg, ServerMsg::Res(r) if r.id == id)
        )
        .is_none());
    e.hold.at(None);
    assert_eq!(m.wait_res(id), Ok(Value::Null));
}

/// A Mobile with a size recorded from its Terminal View replies: the size stays the
/// Desktop's (the CONTEXT rule, even with a recorded Mobile size).
#[test]
fn mobile_submit_never_resizes() {
    let mut e = env();
    let t = e.ready("claude");
    e.desk.resize(t.id, 100, 30);
    e.desk.attach(t.id);
    std::thread::sleep(ms(500));
    let mut m = e.mobile();
    m.attach(t.id);
    m.resize(t.id, 40, 20);
    std::thread::sleep(ms(300));
    let before = t.sizes();
    assert_eq!(submit(&mut m, &t, "one"), Ok(Value::Null));
    assert_eq!(submit(&mut m, &t, "two"), Ok(Value::Null));
    std::thread::sleep(ms(800));
    assert_eq!(t.sizes(), before);
    assert!(
        m.try_msg(ms(100), |msg| matches!(msg, ServerMsg::TermSize { .. }))
            .is_none(),
        "{:?}",
        m.summary()
    );
    t.run("stty size > now.size");
    assert_eq!(t.file("now.size"), b"30 100\n");
}

/// The real path: Chat View → Terminal View (typing takes the size) → back to the Chat
/// View (the size returns), then a reply: no further size change.
#[test]
fn submit_after_terminal_view_left_never_resizes() {
    let mut e = env();
    let t = e.ready("claude");
    e.desk.resize(t.id, 100, 30);
    e.desk.attach(t.id);
    std::thread::sleep(ms(500));
    let mut m = e.mobile();
    m.attach(t.id);
    m.resize(t.id, 40, 20);
    m.input(t.id, "x");
    let deadline = Instant::now() + T;
    while !t.sizes().ends_with("20 40\n") {
        assert!(Instant::now() < deadline, "{:?}", t.sizes());
        std::thread::sleep(ms(20));
    }
    m.request(&ClientMsg::TermDetach { terminal: t.id })
        .unwrap();
    while !t.sizes().ends_with("30 100\n") {
        assert!(Instant::now() < deadline, "{:?}", t.sizes());
        std::thread::sleep(ms(20));
    }
    // The agent redraws its composer at the size it has again.
    t.show("claude-composer-idle", true);
    e.wait_ready(&t, Ok(()));
    std::thread::sleep(ms(300));
    let before = t.sizes();
    assert_eq!(submit(&mut m, &t, "after"), Ok(Value::Null));
    std::thread::sleep(ms(800));
    assert_eq!(t.sizes(), before);
    let mut want = b"x".to_vec();
    want.extend(typed("after"));
    t.expect_input(&want);
}

#[test]
fn submit_validation_writes_nothing() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.ready("claude");
    let over = "x".repeat(SUBMIT_MAX_BYTES + 1);
    for (text, err) in [
        ("", "reply is blank"),
        (" \r\n\t", "reply is blank"),
        ("a\x1b[201~\rb", "reply contains control characters"),
        ("a\u{0}", "reply contains control characters"),
        ("a\u{9b}", "reply contains control characters"),
        (over.as_str(), "reply is too long"),
    ] {
        assert_eq!(submit(&mut m, &t, text), Err(err.into()), "{text:?}");
    }
    t.expect_input(b"");
    // Exactly the limit is typed.
    let max = "y".repeat(SUBMIT_MAX_BYTES);
    assert_eq!(submit(&mut m, &t, &max), Ok(Value::Null));
    t.expect_input(&typed(&max));
}

#[test]
fn submit_refused_for_shell_and_non_chat() {
    let mut e = env();
    let mut m = e.mobile();
    // A shell: a Mobile does not see it; a Desktop is told it is no chat.
    let cwd = e.h.project("shell");
    let shell = Uuid::new_v4();
    e.desk.open(shell, sh_spec(&cwd));
    let r = m.request(&ClientMsg::TermSubmit {
        terminal: shell,
        text: "ls".into(),
    });
    assert!(r.as_ref().unwrap_err().contains("forbidden"), "{r:?}");
    let r = e.desk.request(&ClientMsg::TermSubmit {
        terminal: shell,
        text: "ls".into(),
    });
    assert_eq!(r, Err(SUBMIT_NOT_CHAT.into()));
    // An agent without a Chat View.
    let cursor = e.open_spec("cursor", |cwd| LaunchSpec {
        agent: Some("cursor".into()),
        shell_mode: Some("claude".into()),
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    });
    cursor.show("claude-composer-idle", true);
    assert_eq!(
        submit(&mut e.desk, &cursor, "hi"),
        Err(SUBMIT_NOT_CHAT.into())
    );
    cursor.expect_input(b"");
    // Unknown Terminals.
    let r = m.request(&ClientMsg::TermSubmit {
        terminal: Uuid::new_v4(),
        text: "hi".into(),
    });
    assert!(r.unwrap_err().starts_with("unknown terminal"));
}

#[test]
fn submit_exited_refused() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.ready("claude");
    let cat: i32 = String::from_utf8(t.file("pids.log"))
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The fake ends (its reader too, so the output ends); it acknowledges nothing.
    let n = (0..)
        .find(|n| !t.cwd.join(format!("ctl.{n}")).exists())
        .unwrap();
    fs::write(
        t.cwd.join(format!("ctl.{n}")),
        format!("kill {cat}; exit 0\n"),
    )
    .unwrap();
    e.desk.expect_msg(
        "term.exit",
        |msg| matches!(msg, ServerMsg::TermExit { terminal, .. } if *terminal == t.id),
    );
    assert_eq!(
        submit(&mut m, &t, "anyone?"),
        Err("terminal has exited".into())
    );
}

/// Send `t`'s agent report `s` from `c` without waiting; the request id. (A connection made
/// while a check holds the Terminal would wait for it for its first `terminals` list.)
fn report_async(e: &Env, c: &mut Client, t: &Term, s: AgentStatus) -> u64 {
    let id = c.request_id();
    c.send(
        &ClientMsg::TermEvent {
            terminal: t.id,
            run: e.run_of(t),
            status: s,
            session_id: None,
        },
        Some(id),
    );
    id
}

/// A report that arrives after the check approved a piece of the paste waits for that write:
/// it can never come before it. Enter, checked after the report, is not typed.
#[test]
fn a_report_after_the_paste_check_comes_after_the_paste() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.ready("claude");
    let mut c = Client::in_process(&e.srv, Role::Desktop);
    e.hold.at(Some(TestPoint::SubmitChecked));
    let id = send_submit(&mut m, &t, "go");
    e.hold.wait_held();
    let rid = report_async(&e, &mut c, &t, AgentStatus::NeedsYou);
    // The report waits for the held check and its write.
    assert!(c
        .try_msg(
            ms(300),
            |msg| matches!(msg, ServerMsg::Res(r) if r.id == rid)
        )
        .is_none());
    assert!(t.input().is_empty());
    e.hold.at(None);
    assert_eq!(c.wait_res(rid), Ok(Value::Null));
    assert_eq!(m.wait_res(id), Err(SUBMIT_UNCONFIRMED.into()));
    t.expect_input(b"\x1b[200~go\x1b[201~");
    assert_eq!(e.info(&t).agent_status, Some(AgentStatus::NeedsYou));
}

/// The same for Enter: approved, it is typed before the report, which then wins: the
/// status the Enter started (Codex: working) never overwrites it.
#[test]
fn a_report_after_the_enter_check_comes_after_enter() {
    let mut e = env();
    let mut m = e.mobile();
    let t = e.ready("codex");
    let mut c = Client::in_process(&e.srv, Role::Desktop);
    e.hold.at(Some(TestPoint::SubmitEnter));
    let id = send_submit(&mut m, &t, "go");
    e.hold.wait_held();
    // Hold the Enter's check instead, with the locks taken.
    e.hold.at(Some(TestPoint::SubmitChecked));
    e.hold.wait_held();
    let rid = report_async(&e, &mut c, &t, AgentStatus::NeedsYou);
    assert!(c
        .try_msg(
            ms(300),
            |msg| matches!(msg, ServerMsg::Res(r) if r.id == rid)
        )
        .is_none());
    e.hold.at(None);
    assert_eq!(m.wait_res(id), Ok(Value::Null));
    assert_eq!(c.wait_res(rid), Ok(Value::Null));
    t.expect_input(&typed("go"));
    assert_eq!(e.info(&t).agent_status, Some(AgentStatus::NeedsYou));
}

/// A prompt raised after Enter was written, before the reply is answered, stays: the
/// Enter's own status change was made with Enter, not after it.
#[test]
fn a_prompt_after_enter_is_not_erased() {
    let mut e = env_with(|c| {
        c.prompt_settle = ms(50);
        c.prompt_grace = Duration::from_secs(1);
    });
    let mut m = e.mobile();
    let t = e.ready("codex");
    e.status(&t, AgentStatus::Finished);
    e.hold.at(Some(TestPoint::Submitted));
    let id = send_submit(&mut m, &t, "run it");
    e.hold.wait_held();
    t.expect_input(&typed("run it"));
    assert_eq!(e.info(&t).agent_status, Some(AgentStatus::Working));
    // Codex asks for approval meanwhile.
    t.show("codex-exec-100x30", true);
    e.status(&t, AgentStatus::NeedsYou);
    let deadline = Instant::now() + T;
    while e.info(&t).permission_prompt.is_none() {
        assert!(Instant::now() < deadline, "no prompt listed");
        std::thread::sleep(ms(20));
    }
    e.hold.at(None);
    assert_eq!(m.wait_res(id), Ok(Value::Null));
    std::thread::sleep(ms(300));
    let info = e.info(&t);
    assert_eq!(info.agent_status, Some(AgentStatus::NeedsYou));
    assert!(info.permission_prompt.is_some());
}

/// What a paste-aware agent makes of `input`: the pastes (between `ESC[200~` and
/// `ESC[201~`), the keys typed outside them, and whether it is still inside a paste.
fn as_agent_reads(input: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>, bool) {
    let (start, end) = (b"\x1b[200~", b"\x1b[201~");
    let (mut pastes, mut keys, mut inside) = (Vec::new(), Vec::new(), false);
    let mut i = 0;
    while i < input.len() {
        if !inside && input[i..].starts_with(start) {
            inside = true;
            pastes.push(Vec::new());
            i += start.len();
        } else if inside && input[i..].starts_with(end) {
            inside = false;
            i += end.len();
        } else {
            if inside {
                pastes.last_mut().unwrap().push(input[i]);
            } else {
                keys.push(input[i]);
            }
            i += 1;
        }
    }
    (pastes, keys, inside)
}

/// A reply stopped after its first piece is completed with the paste's end marker, so the
/// next key (an answer to the prompt that stopped it, a Desktop's key) is a key again.
#[test]
fn a_stopped_paste_is_closed_before_the_next_key() {
    let mut e = env_with(|c| {
        c.prompt_settle = ms(50);
        c.prompt_grace = Duration::from_secs(1);
    });
    let mut m = e.mobile();
    let t = e.ready("claude");
    let text = "z".repeat(3000);
    e.hold.at(Some(TestPoint::SubmitPaste));
    let id = send_submit(&mut m, &t, &text);
    e.hold.wait_held();
    // Let the first piece through its check and write, and hold the second.
    e.hold.at(Some(TestPoint::SubmitChecked));
    e.hold.wait_held();
    e.hold.at(Some(TestPoint::SubmitPaste));
    e.hold.wait_held();
    // A prompt and needs-you before the second piece.
    t.show("claude-bash-100x30", true);
    e.wait_ready(&t, Err(SUBMIT_NEEDS_YOU));
    e.hold.at(None);
    assert_eq!(m.wait_res(id), Err(SUBMIT_UNCONFIRMED.into()));
    e.status(&t, AgentStatus::NeedsYou);
    let deadline = Instant::now() + T;
    let p = loop {
        if let Some(p) = e.info(&t).permission_prompt {
            break p;
        }
        assert!(Instant::now() < deadline, "no prompt listed");
        std::thread::sleep(ms(20));
    };
    assert_eq!(
        m.request(&ClientMsg::TermAnswer {
            terminal: t.id,
            prompt: p.id,
            option: 0,
        }),
        Ok(Value::Null)
    );
    e.desk.input(t.id, "x");
    let deadline = Instant::now() + T;
    while !t.input().ends_with(b"1x") {
        assert!(
            Instant::now() < deadline,
            "{:?}",
            String::from_utf8_lossy(&t.input())
        );
        std::thread::sleep(ms(20));
    }
    let (pastes, keys, inside) = as_agent_reads(&t.input());
    assert!(!inside, "still inside the paste");
    assert_eq!(keys, b"1x", "the answer and the key are keys");
    assert_eq!(pastes.len(), 1);
    assert!(pastes[0].len() < text.len() && pastes[0].iter().all(|b| *b == b'z'));
}

/// Without a descriptor for writes that cannot block, the Terminal takes no replies (and
/// nothing is typed); its Terminal View input still works.
#[test]
fn no_nonblocking_descriptor_refuses_replies() {
    let mut e = env_with(|c| {
        c.test_hook = Some(TestHook(Arc::new(|_, p| p == TestPoint::ReplyDescriptor)));
    });
    let mut m = e.mobile();
    let t = e.ready("claude");
    assert_eq!(submit(&mut m, &t, "hi"), Err(SUBMIT_NO_NONBLOCK.into()));
    t.expect_input(b"");
    e.desk.input(t.id, "x");
    t.expect_input(b"x");
}

/// A PTY that stops taking input in the middle of a reply, and then its end marker too:
/// the reply's outcome is unknown, later replies are refused as stuck, and the Desktop can
/// still type.
#[test]
fn an_unclosable_paste_makes_replies_stuck() {
    let mut e = env_with(|c| c.submit_stall = ms(300));
    let mut m = e.mobile();
    let t = e.ready("claude");
    let reader: i32 = String::from_utf8(t.file("pids.log"))
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The agent stops reading: replies fill its tty until one cannot be finished.
    unsafe { libc::kill(reader, libc::SIGSTOP) };
    let big = "w".repeat(SUBMIT_MAX_BYTES);
    let mut outcome = Ok(Value::Null);
    for _ in 0..16 {
        outcome = submit(&mut m, &t, &big);
        if outcome.is_err() {
            break;
        }
    }
    assert_eq!(outcome, Err(SUBMIT_UNCONFIRMED.into()));
    assert_eq!(submit(&mut m, &t, "next"), Err(SUBMIT_STUCK.into()));
    assert_eq!(
        e.desk.request(&ClientMsg::TermInput {
            terminal: t.id,
            data: "k".into(),
        }),
        Ok(Value::Null)
    );
    unsafe { libc::kill(reader, libc::SIGCONT) };
    let deadline = Instant::now() + T;
    while !t.input().ends_with(b"k") {
        assert!(Instant::now() < deadline, "the Desktop's key never arrived");
        std::thread::sleep(ms(20));
    }
    assert_eq!(submit(&mut m, &t, "still"), Err(SUBMIT_STUCK.into()));
}

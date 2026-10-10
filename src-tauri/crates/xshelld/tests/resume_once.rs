#![cfg(unix)]
//! A Host runs an agent session in at most one live Terminal (capability
//! `term.open-existing`, xshell#41): two clients resuming one past session at the same moment
//! start one agent, and both are answered with its Terminal. Races are ordered with test
//! hooks (a rendezvous before the registry lock, a hold after the lookup), never with sleeps.

mod common;

use common::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use uuid::Uuid;
use xshell_core::launch::LaunchSpec;
use xshell_core::terminal::state::{save_atomic, Leader, PersistedTerminal};
use xshell_protocol::msg::{
    ClientMsg, OpenReply, OpenSpec, ServerMsg, TerminalInfo, SESSION_CLOSING, SESSION_OPEN,
};
use xshelld::server::{Cleanup, Config, Role, ServerHandle, TestHook, TestPoint};

const SID: &str = "11111111-2222-3333-4444-555555555555";
/// How long a request that must wait is given to show it does not.
const QUIET: Duration = Duration::from_millis(300);

/// A meeting point for two threads, bounded so a failed test never strands one.
struct Meet {
    left: Mutex<usize>,
    cv: Condvar,
}

impl Meet {
    fn new(n: usize) -> Arc<Meet> {
        Arc::new(Meet {
            left: Mutex::new(n),
            cv: Condvar::new(),
        })
    }

    fn arrive(&self) {
        let mut l = self.left.lock().unwrap();
        *l = l.saturating_sub(1);
        self.cv.notify_all();
        let _ = self
            .cv
            .wait_timeout_while(l, Duration::from_secs(10), |l| *l > 0)
            .unwrap();
    }
}

type Hold = (Sender<()>, Receiver<()>);

/// The hooks the tests order races with. Opens of UUIDs no test registered pass straight on.
#[derive(Clone, Default)]
struct Points {
    /// `OpenDecide` of these UUIDs waits until every UUID sharing the meeting has arrived.
    meet: Arc<Mutex<HashMap<Uuid, Arc<Meet>>>>,
    /// Every UUID whose `OpenDecide` ran.
    decided: Arc<Mutex<Option<Sender<Uuid>>>>,
    /// `OpenLooked` of this UUID reports itself held and waits for the release.
    looked: Arc<Mutex<HashMap<Uuid, Hold>>>,
    /// The Relaunch worker's `Waited` reports itself held and waits for the release.
    waited: Arc<Mutex<Option<Hold>>>,
    /// The Relaunch worker's `Waited` answers a timeout.
    fail_wait: Arc<Mutex<bool>>,
}

impl Points {
    fn hook(&self) -> TestHook {
        let me = self.clone();
        TestHook(Arc::new(move |id, p| {
            match p {
                TestPoint::OpenDecide => {
                    if let Some(tx) = me.decided.lock().unwrap().as_ref() {
                        let _ = tx.send(id);
                    }
                    let m = me.meet.lock().unwrap().get(&id).cloned();
                    if let Some(m) = m {
                        m.arrive();
                    }
                }
                TestPoint::OpenLooked => {
                    let hold = me.looked.lock().unwrap().remove(&id);
                    if let Some((held, release)) = hold {
                        let _ = held.send(());
                        let _ = release.recv_timeout(Duration::from_secs(30));
                    }
                }
                TestPoint::Waited { .. } => {
                    let hold = me.waited.lock().unwrap().take();
                    if let Some((held, release)) = hold {
                        let _ = held.send(());
                        let _ = release.recv_timeout(Duration::from_secs(30));
                    }
                    return *me.fail_wait.lock().unwrap();
                }
                _ => {}
            }
            false
        }))
    }

    /// The `OpenDecide`s of `ids` meet before any of them locks the registry.
    fn meet(&self, ids: &[Uuid]) {
        let m = Meet::new(ids.len());
        let mut g = self.meet.lock().unwrap();
        for id in ids {
            g.insert(*id, m.clone());
        }
    }

    /// Report every `OpenDecide` from now on.
    fn decisions(&self) -> Receiver<Uuid> {
        let (tx, rx) = channel();
        *self.decided.lock().unwrap() = Some(tx);
        rx
    }

    /// Hold `id`'s open after its lookup, with the registry locked; returns where it says
    /// it is held and what releases it.
    fn hold_looked(&self, id: Uuid) -> (Receiver<()>, Sender<()>) {
        let (held_tx, held_rx) = channel();
        let (release_tx, release_rx) = channel();
        self.looked
            .lock()
            .unwrap()
            .insert(id, (held_tx, release_rx));
        (held_rx, release_tx)
    }

    /// Hold the next Relaunch once it stopped waiting for the old process.
    fn hold_waited(&self) -> (Receiver<()>, Sender<()>) {
        let (held_tx, held_rx) = channel();
        let (release_tx, release_rx) = channel();
        *self.waited.lock().unwrap() = Some((held_tx, release_rx));
        (held_rx, release_tx)
    }
}

struct Env {
    desk: Client,
    mob_a: Client,
    mob_b: Client,
    srv: ServerHandle,
    /// A Project the Host knows from Claude history for `SID`.
    cwd: PathBuf,
    fake: Fake,
    points: Points,
    _reaper: FakeReaper,
    h: TestHome,
}

fn env() -> Env {
    env_with(|_| {})
}

fn env_with(tweak: impl FnOnce(&mut Config)) -> Env {
    let h = TestHome::new();
    let cwd = h.project("app");
    claude_history(&h, &cwd, SID);
    env_in(h, cwd, tweak)
}

fn env_in(h: TestHome, cwd: PathBuf, tweak: impl FnOnce(&mut Config)) -> Env {
    let fake = Fake::in_dir(&cwd);
    let points = Points::default();
    let hook = points.hook();
    let srv = start(&h, |c| {
        c.test_hook = Some(hook);
        tweak(c);
    });
    Env {
        desk: Client::in_process(&srv, Role::Desktop),
        mob_a: Client::in_process(&srv, Role::Mobile),
        mob_b: Client::in_process(&srv, Role::Mobile),
        srv,
        _reaper: FakeReaper(fake.pids_log.clone()),
        fake,
        points,
        cwd,
        h,
    }
}

/// A `term.open` of `launch` under `t`, answered with the Terminal that already runs its
/// session if there is one.
fn adopt_msg(t: Uuid, launch: LaunchSpec) -> ClientMsg {
    ClientMsg::TermOpen {
        spec: OpenSpec {
            terminal: t,
            launch,
            cols: 80,
            rows: 24,
            adopt_existing: true,
            ..Default::default()
        },
    }
}

fn open_reply(v: Value) -> OpenReply {
    serde_json::from_value(v).expect("a term.open reply")
}

#[track_caller]
fn session_open(r: Result<Value, String>) -> String {
    let e = r.expect_err("refused: the session is open");
    assert!(e.starts_with(SESSION_OPEN), "{e}");
    assert!(!e.contains("already exists"), "{e}");
    e
}

impl Env {
    fn claude(&self, sid: &str) -> LaunchSpec {
        claude_spec(&self.cwd, Some(sid))
    }

    /// Launches of an agent whose argv names `sid`.
    fn launches_of(&self, sid: &str) -> usize {
        self.fake
            .launches()
            .iter()
            .filter(|argv| argv.iter().any(|a| a == sid))
            .count()
    }

    /// Wait for `n` launches of `sid`, then check no more follow.
    #[track_caller]
    fn assert_launches(&self, sid: &str, n: usize) {
        let deadline = std::time::Instant::now() + T;
        while self.launches_of(sid) < n && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(QUIET);
        assert_eq!(self.launches_of(sid), n, "{:?}", self.fake.launches());
    }

    /// The list a new Desktop connection gets.
    fn list(&self) -> Vec<TerminalInfo> {
        let s = self.srv.connect_in_process(Role::Desktop).unwrap();
        let mut c = Client::from_io(s.try_clone().unwrap(), s);
        c.hello(range(1, 1)).1
    }

    fn listed_on(&self, sid: &str) -> Vec<TerminalInfo> {
        self.list()
            .into_iter()
            .filter(|i| i.spec.session_id.as_deref() == Some(sid))
            .collect()
    }

    /// Open `launch` from the Desktop under a new UUID; returns it and the reply.
    fn desk_open(&mut self, launch: LaunchSpec) -> (Uuid, OpenReply) {
        let t = Uuid::new_v4();
        let r = open_reply(self.desk.open(t, launch));
        assert_eq!(r.terminal, Some(t));
        assert!(!r.existed);
        (t, r)
    }
}

/// Send both opens, each behind the rendezvous, and answer both replies with their request.
fn race(
    a: (&mut Client, Uuid, LaunchSpec),
    b: (&mut Client, Uuid, LaunchSpec),
    points: &Points,
) -> [(Uuid, OpenReply); 2] {
    points.meet(&[a.1, b.1]);
    let ia = a.0.request_id();
    let ib = b.0.request_id();
    a.0.send(&adopt_msg(a.1, a.2), Some(ia));
    b.0.send(&adopt_msg(b.1, b.2), Some(ib));
    let ra = open_reply(a.0.wait_res(ia).expect("a's open"));
    let rb = open_reply(b.0.wait_res(ib).expect("b's open"));
    [(a.1, ra), (b.1, rb)]
}

/// Both answered with one Terminal, which exactly one of them started under its own UUID.
#[track_caller]
fn one_started(r: &[(Uuid, OpenReply); 2]) -> Uuid {
    let [(ta, ra), (tb, rb)] = r;
    assert_eq!(ra.terminal, rb.terminal, "{r:?}");
    assert_eq!(
        [ra.existed, rb.existed].iter().filter(|e| !**e).count(),
        1,
        "{r:?}"
    );
    let (started, reply) = if ra.existed { (tb, rb) } else { (ta, ra) };
    assert_eq!(reply.terminal, Some(*started));
    *started
}

// ── Races ────────────────────────────────────────────────────────────────

#[test]
fn two_mobiles_resume_one_session_start_one_agent() {
    let mut e = env();
    for _ in 0..10 {
        let sid = Uuid::new_v4().to_string();
        claude_history(&e.h, &e.cwd, &sid);
        let spec = e.claude(&sid);
        let r = race(
            (&mut e.mob_a, Uuid::new_v4(), spec.clone()),
            (&mut e.mob_b, Uuid::new_v4(), spec),
            &e.points,
        );
        let started = one_started(&r);
        e.assert_launches(&sid, 1);
        let listed = e.listed_on(&sid);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].terminal, started);
    }
}

#[test]
fn mobile_and_desktop_resume_one_session_start_one_agent() {
    let mut e = env();
    let spec = e.claude(SID);
    let r = race(
        (&mut e.desk, Uuid::new_v4(), spec.clone()),
        (&mut e.mob_a, Uuid::new_v4(), spec),
        &e.points,
    );
    let t = one_started(&r);
    e.assert_launches(SID, 1);
    assert_eq!(e.listed_on(SID).len(), 1);
    // Both attach to the Terminal they were answered with and see the one agent.
    e.desk.attach(t);
    e.desk.output_until(t, "pid ");
    e.mob_a.attach(t);
    e.mob_a.output_until(t, "pid ");
}

/// A4: an open holding the registry after its lookup keeps a second open of the session
/// from looking at all; that open then gets the first one's Terminal.
#[test]
fn second_resume_waits_for_the_first_under_the_lock() {
    let mut e = env();
    let decided = e.points.decisions();
    let (ta, tb) = (Uuid::new_v4(), Uuid::new_v4());
    let (held, release) = e.points.hold_looked(ta);
    let ia = e.mob_a.request_id();
    e.mob_a.send(&adopt_msg(ta, e.claude(SID)), Some(ia));
    held.recv_timeout(T).expect("a holds after its lookup");
    let ib = e.mob_b.request_id();
    e.mob_b.send(&adopt_msg(tb, e.claude(SID)), Some(ib));
    // B reached its open (its decide point ran) and is blocked on the registry.
    let deadline = std::time::Instant::now() + T;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match decided.recv_timeout(left) {
            Ok(id) if id == tb => break,
            Ok(_) => {}
            Err(_) => panic!("b never reached its open"),
        }
    }
    assert!(
        e.mob_b
            .try_msg(QUIET, |m| matches!(m, ServerMsg::Res(r) if r.id == ib))
            .is_none(),
        "b was answered while a held the registry"
    );
    release.send(()).unwrap();
    let ra = open_reply(e.mob_a.wait_res(ia).unwrap());
    assert!(!ra.existed);
    assert_eq!(ra.terminal, Some(ta));
    let rb = open_reply(e.mob_b.wait_res(ib).unwrap());
    assert!(rb.existed, "{rb:?}");
    assert_eq!(rb.terminal, Some(ta));
    assert_eq!(rb.pid, ra.pid);
    e.assert_launches(SID, 1);
}

// ── One open at a time ───────────────────────────────────────────────────

#[test]
fn resume_of_live_session_returns_it() {
    let mut e = env();
    let (t, first) = e.desk_open(e.claude(SID));
    e.assert_launches(SID, 1);
    let state = e.h.state_json();
    e.mob_a.drain_for(QUIET);
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
            .unwrap(),
    );
    assert_eq!(
        r,
        OpenReply {
            pid: first.pid,
            terminal: Some(t),
            existed: true
        }
    );
    // Nothing started, saved or listed (other changes of the one Terminal may be listed).
    assert!(e
        .mob_a
        .try_msg(
            QUIET,
            |m| matches!(m, ServerMsg::Terminals { list } if list.len() != 1)
        )
        .is_none());
    assert_eq!(e.h.state_json(), state);
    e.assert_launches(SID, 1);
    assert_eq!(e.list().len(), 1);
}

#[test]
fn resume_without_opt_in_is_refused_not_launched() {
    let mut e = env();
    let (t, _) = e.desk_open(e.claude(SID));
    // A Desktop is told which Terminal runs it.
    let err = session_open(e.desk.request(&open_msg(Uuid::new_v4(), e.claude(SID))));
    assert_eq!(err, format!("{SESSION_OPEN}: {t}"));
    let err = session_open(e.mob_a.request(&open_msg(Uuid::new_v4(), e.claude(SID))));
    assert_eq!(err, format!("{SESSION_OPEN}: {t}"));
    e.assert_launches(SID, 1);
    assert_eq!(e.list().len(), 1);
}

/// D10: reopening a listed UUID stays `already exists`, with or without the flag.
#[test]
fn same_uuid_still_already_exists() {
    let mut e = env();
    let (t, _) = e.desk_open(e.claude(SID));
    let err = e.desk.request(&adopt_msg(t, e.claude(SID))).unwrap_err();
    assert_eq!(err, format!("terminal {t} already exists"));
    let err = e.desk.request(&open_msg(t, e.claude(SID))).unwrap_err();
    assert_eq!(err, format!("terminal {t} already exists"));
    e.assert_launches(SID, 1);
}

#[test]
fn resume_after_exit_starts_new_terminal() {
    let mut e = env();
    let (t, first) = e.desk_open(e.claude(SID));
    let pid = first.pid.unwrap() as i32;
    e.fake.wait_pids(1);
    unsafe { libc::kill(pid, libc::SIGKILL) };
    e.desk.expect_msg(
        "term.exit",
        |m| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == t),
    );
    // Still listed, as exited.
    assert_eq!(e.listed_on(SID).len(), 1);
    let t2 = Uuid::new_v4();
    let r = open_reply(e.mob_a.request(&adopt_msg(t2, e.claude(SID))).unwrap());
    assert!(!r.existed);
    assert_eq!(r.terminal, Some(t2));
    e.assert_launches(SID, 2);
    assert_eq!(e.listed_on(SID).len(), 2);
}

/// D3: a session whose Terminal is still being closed is refused, retryably, until it left.
#[test]
fn resume_while_closing_is_refused_then_starts() {
    // The fake agents ignore SIGHUP: closing takes the whole grace.
    let mut e = env_with(|c| c.kill_grace = Duration::from_secs(2));
    let (t, _) = e.desk_open(e.claude(SID));
    e.fake.wait_pids(1);
    e.desk
        .request(&ClientMsg::TermClose { terminal: t })
        .unwrap();
    let err = e
        .mob_a
        .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
        .unwrap_err();
    assert_eq!(err, SESSION_CLOSING);
    let err = e
        .desk
        .request(&open_msg(Uuid::new_v4(), e.claude(SID)))
        .unwrap_err();
    assert_eq!(err, SESSION_CLOSING);
    assert_eq!(e.launches_of(SID), 1);
    e.desk.terminals_where(|l| l.is_empty());
    let t2 = Uuid::new_v4();
    let r = open_reply(e.mob_a.request(&adopt_msg(t2, e.claude(SID))).unwrap());
    assert!(!r.existed);
    assert_eq!(r.terminal, Some(t2));
    e.assert_launches(SID, 2);
}

#[test]
fn resume_during_relaunch_returns_the_relaunching_terminal() {
    let mut e = env();
    let (t, first) = e.desk_open(e.claude(SID));
    e.fake.wait_pids(1);
    let (held, release) = e.points.hold_waited();
    let id = e.desk.request_id();
    e.desk.send(
        &ClientMsg::TermRelaunch {
            terminal: t,
            skip_permissions: true,
        },
        Some(id),
    );
    held.recv_timeout(T).expect("the relaunch waited");
    // The old process is gone and its exit held: the Terminal is still the session.
    assert!(!alive(first.pid.unwrap() as i32));
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
            .unwrap(),
    );
    assert!(r.existed);
    assert_eq!(r.terminal, Some(t));
    release.send(()).unwrap();
    assert_eq!(e.desk.wait_res(id).unwrap()["relaunched"], json!(true));
    // The original and its replacement, nothing more.
    e.assert_launches(SID, 2);
    assert_eq!(e.list().len(), 1);
}

#[test]
fn resume_after_failed_relaunch_starts_new() {
    let mut e = env();
    let (t, _) = e.desk_open(e.claude(SID));
    e.fake.wait_pids(1);
    *e.points.fail_wait.lock().unwrap() = true;
    let err = e
        .desk
        .request(&ClientMsg::TermRelaunch {
            terminal: t,
            skip_permissions: true,
        })
        .unwrap_err();
    assert_eq!(err, "previous process did not exit");
    *e.points.fail_wait.lock().unwrap() = false;
    e.desk
        .terminals_where(|l| l.iter().any(|i| i.terminal == t && i.exit_code.is_some()));
    let t2 = Uuid::new_v4();
    let r = open_reply(e.mob_a.request(&adopt_msg(t2, e.claude(SID))).unwrap());
    assert!(!r.existed);
    assert_eq!(r.terminal, Some(t2));
    e.assert_launches(SID, 2);
}

// ── Restored Terminals ───────────────────────────────────────────────────

fn persisted(t: Uuid, spec: LaunchSpec, created_at_ms: u64) -> PersistedTerminal {
    PersistedTerminal {
        terminal: t,
        spec,
        meta: serde_json::Map::new(),
        cols: 80,
        rows: 24,
        created_at_ms,
        leader: None,
        prompt_id_floor: None,
    }
}

/// The state a Daemon left: a Project known from `SID`'s history and these Terminals.
fn restored(
    records: impl FnOnce(&std::path::Path) -> Vec<PersistedTerminal>,
) -> (TestHome, PathBuf) {
    let h = TestHome::new();
    let cwd = h.project("app");
    claude_history(&h, &cwd, SID);
    save_atomic(&h.paths().state, &records(&cwd)).unwrap();
    (h, cwd)
}

#[test]
fn restored_terminal_owns_its_session() {
    let t = Uuid::new_v4();
    let (h, cwd) = restored(|cwd| vec![persisted(t, claude_spec(cwd, Some(SID)), 1)]);
    let mut e = env_in(h, cwd, |_| {});
    e.assert_launches(SID, 1);
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
            .unwrap(),
    );
    assert!(r.existed);
    assert_eq!(r.terminal, Some(t));
    e.assert_launches(SID, 1);
}

/// D9: duplicates from an older state file are all kept, and the oldest is the session.
#[test]
fn restored_duplicates_are_kept_and_the_oldest_is_returned() {
    let (newer, older) = (Uuid::new_v4(), Uuid::new_v4());
    let (h, cwd) = restored(|cwd| {
        vec![
            persisted(newer, claude_spec(cwd, Some(SID)), 5),
            persisted(older, claude_spec(cwd, Some(SID)), 2),
        ]
    });
    let mut e = env_in(h, cwd, |_| {});
    e.assert_launches(SID, 2);
    assert_eq!(e.listed_on(SID).len(), 2);
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
            .unwrap(),
    );
    assert!(r.existed);
    assert_eq!(r.terminal, Some(older));
    e.assert_launches(SID, 2);
}

fn unkillable(_: &Leader, _: Duration) -> Cleanup {
    Cleanup::Unresolved
}

/// D4: a Terminal restored without a process (leftovers may still run) keeps its session
/// until it is closed.
#[test]
fn unresolved_restored_terminal_owns_its_session() {
    let t = Uuid::new_v4();
    let (h, cwd) = restored(|cwd| {
        let mut p = persisted(t, claude_spec(cwd, Some(SID)), 1);
        p.leader = Some(Leader {
            pid: 999_999,
            start_time: Some(42),
            groups: vec![],
        });
        vec![p]
    });
    let mut e = env_in(h, cwd, |c| c.cleanup_override = Some(unkillable));
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
            .unwrap(),
    );
    assert_eq!(
        r,
        OpenReply {
            pid: None,
            terminal: Some(t),
            existed: true
        }
    );
    e.assert_launches(SID, 0);
    // Closing it frees the session.
    e.mob_a
        .request(&ClientMsg::TermClose { terminal: t })
        .unwrap();
    e.mob_a.terminals_where(|l| l.is_empty());
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
            .unwrap(),
    );
    assert!(!r.existed);
    e.assert_launches(SID, 1);
}

// ── What counts as the same session ──────────────────────────────────────

#[test]
fn different_agent_or_raw_shell_never_matches() {
    let mut e = env();
    e.desk_open(e.claude(SID));
    // Another agent's session of the same id is another session.
    let codex = LaunchSpec {
        agent: Some("codex".into()),
        ..e.claude(SID)
    };
    let t = Uuid::new_v4();
    let r = open_reply(e.desk.request(&adopt_msg(t, codex)).unwrap());
    assert!(!r.existed);
    assert_eq!(r.terminal, Some(t));

    // A raw shell with a stray session id runs no session.
    let other = Uuid::new_v4().to_string();
    e.desk_open(LaunchSpec {
        session_id: Some(other.clone()),
        ..sh_spec(&e.cwd)
    });
    let t = Uuid::new_v4();
    let r = open_reply(e.desk.request(&adopt_msg(t, e.claude(&other))).unwrap());
    assert!(!r.existed);
    assert_eq!(r.terminal, Some(t));

    // An unset agent is Claude Code.
    let third = Uuid::new_v4().to_string();
    let (owner, _) = e.desk_open(LaunchSpec {
        agent: None,
        ..e.claude(&third)
    });
    let r = open_reply(
        e.desk
            .request(&adopt_msg(Uuid::new_v4(), e.claude(&third)))
            .unwrap(),
    );
    assert!(r.existed);
    assert_eq!(r.terminal, Some(owner));
}

#[test]
fn mobile_never_adopts_a_hidden_terminal() {
    let mut e = env();
    let (t, _) = e.desk_open(LaunchSpec {
        shell_command: Some("/bin/sh".into()),
        shell_id: Some("bash".into()),
        ..e.claude(SID)
    });
    let before = e.fake.launches().len();
    let err = session_open(e.mob_a.request(&adopt_msg(Uuid::new_v4(), e.claude(SID))));
    assert_eq!(err, SESSION_OPEN);
    assert!(!err.contains(&t.to_string()));
    std::thread::sleep(QUIET);
    assert_eq!(e.fake.launches().len(), before);
    // The Desktop sees it, and adopts it.
    let r = open_reply(
        e.desk
            .request(&adopt_msg(Uuid::new_v4(), e.claude(SID)))
            .unwrap(),
    );
    assert!(r.existed);
    assert_eq!(r.terminal, Some(t));
}

/// D12: adopting would drop the first message, so the open is refused.
#[test]
fn first_message_with_open_session_is_refused() {
    let mut e = env();
    // A new chat with a pre-set session id (no transcript yet), as the Desktop starts one.
    let sid = Uuid::new_v4().to_string();
    let (t, _) = e.desk_open(e.claude(&sid));
    for c in [&mut e.desk, &mut e.mob_a] {
        let err = session_open(c.request(&ClientMsg::TermOpen {
            spec: OpenSpec {
                terminal: Uuid::new_v4(),
                launch: claude_spec(&e.cwd, Some(&sid)),
                cols: 80,
                rows: 24,
                first_message: Some("hello".into()),
                adopt_existing: true,
                ..Default::default()
            },
        }));
        assert_eq!(err, format!("{SESSION_OPEN}: {t}"));
    }
    e.assert_launches(&sid, 1);
}

#[test]
fn new_chat_with_fresh_session_id_is_unaffected() {
    let mut e = env();
    let sid = Uuid::new_v4().to_string();
    let t = Uuid::new_v4();
    let r = open_reply(e.mob_a.request(&adopt_msg(t, e.claude(&sid))).unwrap());
    assert!(!r.existed);
    assert_eq!(r.terminal, Some(t));
    assert!(r.pid.is_some());
    // A new chat without a session id never matches anything.
    let t2 = Uuid::new_v4();
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(t2, claude_spec(&e.cwd, None)))
            .unwrap(),
    );
    assert_eq!(r.terminal, Some(t2));
    let t3 = Uuid::new_v4();
    let r = open_reply(
        e.mob_a
            .request(&adopt_msg(t3, claude_spec(&e.cwd, None)))
            .unwrap(),
    );
    assert_eq!(r.terminal, Some(t3));
    assert!(!r.existed);
}

/// D5: a new Codex chat holds no session until Codex reports one and it is linked.
#[test]
fn codex_unlinked_terminal_does_not_match() {
    let mut e = env();
    let cwd = e.cwd.clone();
    let codex = |sid: Option<&str>| LaunchSpec {
        agent: Some("codex".into()),
        ..claude_spec(&cwd, sid)
    };
    let (_, _) = e.desk_open(codex(None));
    let sid = "0199aaaa-0000-7000-8000-00000000000a";
    let t = Uuid::new_v4();
    let r = open_reply(e.desk.request(&adopt_msg(t, codex(Some(sid)))).unwrap());
    assert!(!r.existed);
    assert_eq!(r.terminal, Some(t));
}

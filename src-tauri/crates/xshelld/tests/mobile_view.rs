#![cfg(unix)]
//! The Mobile's Terminal View against an in-process server (capability `term.mobile`): output
//! paced per connection, the attach `res` with the size, `term.size` notices, a Mobile's
//! resize that never takes the size until it types, the size handed back when an owning
//! Mobile leaves, the attach nudge only when nobody else watches, and the shorter replay.
//!
//! The agent is the shared fake `claude` running `agent.sh` from its Project: most tests use
//! [`TICKER`], which prints `tick` every 20 ms and `size <rows> <cols>` on every SIGWINCH.
//! Rate assertions count frames with generous bounds; exact flush times are unit-tested in
//! the outbox.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_protocol::msg::{ClientMsg, ServerMsg};
use xshelld::server::{Config, Role, ServerHandle};

const SID: &str = "11111111-2222-3333-4444-555555555555";

const TICKER: &str = "trap 'echo \"size $(stty size)\"' WINCH\necho ready\n\
                      while :; do echo tick; sleep 0.02; done\n";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

struct Env {
    srv: ServerHandle,
    h: TestHome,
    /// The agent's working directory.
    cwd: PathBuf,
    desk: Client,
    t: Uuid,
    _reaper: FakeReaper,
}

/// A Desktop opened a Claude Terminal running `agent` (not attached yet).
fn env(agent: &str) -> Env {
    env_with(agent, |_| {})
}

fn env_with(agent: &str, tweak: impl FnOnce(&mut Config)) -> Env {
    let h = TestHome::new();
    let cwd = h.project("app");
    fs::write(cwd.join("agent.sh"), agent).unwrap();
    make_jsonl(&h, &cwd, SID);
    let fake = Fake::in_dir(&cwd);
    let srv = start(&h, tweak);
    let mut desk = Client::in_process(&srv, Role::Desktop);
    let t = Uuid::new_v4();
    desk.open(t, claude_spec(&cwd, Some(SID)));
    Env {
        srv,
        h,
        cwd,
        desk,
        t,
        _reaper: FakeReaper(fake.pids_log.clone()),
    }
}

impl Env {
    fn mobile(&self) -> Client {
        Client::in_process(&self.srv, Role::Mobile)
    }

    /// The Desktop attaches at `cols`×`rows` and waits until the agent runs and its own
    /// attach nudge has passed; later output starts fresh.
    fn desk_watching(&mut self, cols: u16, rows: u16) {
        let t = self.t;
        self.desk.resize(t, cols, rows);
        self.desk.attach(t);
        self.desk.output_until(t, "ready");
        self.desk.drain_for(ms(400));
        self.desk.skip_output(t);
        self.wait_persisted(cols, rows);
    }

    fn wait_persisted(&self, cols: u16, rows: u16) {
        let deadline = Instant::now() + T;
        loop {
            let s = self.h.state_json();
            let got = &s["terminals"][0];
            if got["cols"] == json!(cols) && got["rows"] == json!(rows) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "size {cols}x{rows} not persisted: {s}"
            );
            std::thread::sleep(ms(50));
        }
    }

    fn persisted(&self) -> (Value, Value) {
        let s = self.h.state_json();
        (
            s["terminals"][0]["cols"].clone(),
            s["terminals"][0]["rows"].clone(),
        )
    }
}

/// Attach with an id; the `res` and when it arrived.
fn attach_at(c: &mut Client, t: Uuid) -> (Value, Instant) {
    let id = c.request_id();
    c.send(&ClientMsg::TermAttach { terminal: t }, Some(id));
    let v = c.wait_res(id).unwrap();
    (v, res_at(c, id))
}

/// When the `res` with `id` arrived.
fn res_at(c: &Client, id: u64) -> Instant {
    let i = c
        .log
        .iter()
        .position(|e| matches!(e, Ev::Msg(ServerMsg::Res(r)) if r.id == id))
        .expect("res in log");
    c.at[i]
}

/// Arrival times of `t`'s output frames.
fn frames(c: &Client, t: Uuid) -> Vec<Instant> {
    c.log
        .iter()
        .zip(&c.at)
        .filter(|(e, _)| matches!(e, Ev::Out(id, _) if *id == t))
        .map(|(_, at)| *at)
        .collect()
}

fn in_window(times: &[Instant], from: Instant, to: Instant) -> Vec<Instant> {
    times
        .iter()
        .copied()
        .filter(|&a| a >= from && a < to)
        .collect()
}

fn min_gap(times: &[Instant]) -> Duration {
    times
        .windows(2)
        .map(|w| w[1] - w[0])
        .min()
        .unwrap_or(Duration::MAX)
}

fn sizes(c: &Client, t: Uuid) -> Vec<(u16, u16)> {
    c.log
        .iter()
        .filter_map(|e| match e {
            Ev::Msg(ServerMsg::TermSize {
                terminal,
                cols,
                rows,
            }) if *terminal == t => Some((*cols, *rows)),
            _ => None,
        })
        .collect()
}

fn expect_term_size(c: &mut Client, t: Uuid, cols: u16, rows: u16) {
    c.expect_msg(
        &format!("term.size {cols}x{rows}"),
        |m| matches!(m, ServerMsg::TermSize { terminal, cols: c, rows: r } if *terminal == t && *c == cols && *r == rows),
    );
}

fn no_size_line_after(c: &mut Client, t: Uuid, d: Duration) {
    c.skip_output(t);
    assert!(
        c.try_output_until(t, b"size ", d).is_none(),
        "the PTY was resized: {:?}",
        c.summary().iter().rev().take(5).collect::<Vec<_>>()
    );
}

// ---- Pacing ---------------------------------------------------------------------------------

#[test]
fn mobile_output_at_most_one_frame_per_second_idle() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    let (_, start) = attach_at(&mut m, e.t);
    m.drain_for(ms(4500));
    e.desk.drain_for(ms(10));
    let end = start + ms(4500);
    let mob = in_window(&frames(&m, e.t), start, end);
    // The replay at once, then about one frame per second.
    assert!((3..=6).contains(&mob.len()), "{} mobile frames", mob.len());
    assert!(min_gap(&mob) >= ms(800), "gap {:?}", min_gap(&mob));
    // Pacing is per connection: the Desktop on the same Terminal gets far more.
    let desk = in_window(&frames(&e.desk, e.t), start, end);
    assert!(desk.len() >= 20, "{} desktop frames", desk.len());
}

#[test]
fn mobile_output_rises_to_ten_per_second_for_three_seconds_after_input() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.drain_for(ms(1500));
    m.skip_output(e.t);
    let typed = Instant::now();
    m.input(e.t, "x");
    // The echo comes back quickly.
    assert!(
        m.try_output_until(e.t, b"x", ms(500)).is_some(),
        "no echo: {:?}",
        m.summary().iter().rev().take(3).collect::<Vec<_>>()
    );
    m.drain_for(ms(6000).saturating_sub(typed.elapsed()));
    let all = frames(&m, e.t);
    let first = in_window(&all, typed, typed + ms(3000));
    assert!(
        first.first().is_some_and(|f| *f - typed < ms(300)),
        "first frame after the input too late"
    );
    assert!(
        (15..=35).contains(&first.len()),
        "{} frames in the burst",
        first.len()
    );
    assert!(min_gap(&first) >= ms(50), "gap {:?}", min_gap(&first));
    // Back to idle.
    let later = in_window(&all, typed + ms(3300), typed + ms(6000));
    assert!(later.len() <= 4, "{} frames after the burst", later.len());
    assert!(min_gap(&later) >= ms(800), "gap {:?}", min_gap(&later));
}

#[test]
fn mobile_attach_replay_is_immediate() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    let (_, at) = attach_at(&mut m, e.t);
    m.output_until(e.t, "tick");
    assert!(frames(&m, e.t)[0] - at < ms(300));
    // Output is held now; a quick detach and re-attach still gets its replay at once.
    m.drain_for(ms(200));
    m.request(&ClientMsg::TermDetach { terminal: e.t }).unwrap();
    let n = frames(&m, e.t).len();
    let (_, at) = attach_at(&mut m, e.t);
    m.drain_for(ms(300));
    let after = &frames(&m, e.t)[n..];
    assert!(
        after.first().is_some_and(|f| *f - at < ms(300)),
        "replay held after the re-attach"
    );
}

#[test]
fn mobile_control_frames_not_delayed() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.drain_for(ms(1300));
    // Output is held for the next second; a new list goes through at once.
    let sent = Instant::now();
    e.desk
        .request(&ClientMsg::TermUpdate {
            terminal: e.t,
            session_id: None,
            meta: Some(serde_json::from_value(json!({ "title": "renamed" })).unwrap()),
        })
        .unwrap();
    m.terminals_where(|l| {
        l.iter()
            .any(|i| i.meta.get("title") == Some(&json!("renamed")))
    });
    let i = m
        .log
        .iter()
        .rposition(|e| matches!(e, Ev::Msg(ServerMsg::Terminals { .. })))
        .unwrap();
    assert!(m.at[i] - sent < ms(300), "list after {:?}", m.at[i] - sent);
}

#[test]
fn mobile_exit_follows_all_output() {
    let mut e = env(
        "read x\ni=0\nwhile [ $i -lt 2000 ]; do echo \"line $i\"; i=$((i+1)); done\n\
         echo LAST\nexit 3\n",
    );
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    e.desk.attach(e.t);
    e.desk.input(e.t, "\n");
    m.expect_msg(
        "term.exit",
        |x| matches!(x, ServerMsg::TermExit { terminal, code: 3 } if *terminal == e.t),
    );
    m.drain_for(ms(300));
    let exit = m
        .log
        .iter()
        .position(|x| matches!(x, Ev::Msg(ServerMsg::TermExit { .. })))
        .unwrap();
    let before: Vec<u8> = m.log[..exit]
        .iter()
        .filter_map(|x| match x {
            Ev::Out(t, d) if *t == e.t => Some(d.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(find(&before, b"line 1999").is_some());
    assert!(find(&before, b"LAST").is_some());
    assert!(
        !m.log[exit..].iter().any(|x| matches!(x, Ev::Out(..))),
        "output after term.exit"
    );
}

// ---- Size -----------------------------------------------------------------------------------

#[test]
fn attach_res_carries_size() {
    let mut e = env(TICKER);
    e.desk.resize(e.t, 100, 30);
    let want = json!({ "exitCode": null, "cols": 100, "rows": 30 });
    assert_eq!(e.desk.attach(e.t), want);
    let mut m = e.mobile();
    assert_eq!(attach_at(&mut m, e.t).0, want);
}

/// A Desktop at 100×30, and a Mobile that attached and sized its view to 40×20.
fn mobile_viewing() -> (Env, Client) {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.resize(e.t, 40, 20);
    (e, m)
}

#[test]
fn mobile_attach_and_resize_never_change_size() {
    let (mut e, mut m) = mobile_viewing();
    no_size_line_after(&mut e.desk, e.t, ms(1000));
    assert_eq!(e.persisted(), (json!(100), json!(30)));
    m.drain_for(ms(10));
    assert!(sizes(&m, e.t).is_empty(), "{:?}", sizes(&m, e.t));
}

#[test]
fn mobile_input_claims_its_size() {
    let (mut e, mut m) = mobile_viewing();
    m.input(e.t, "\r");
    e.desk.output_until(e.t, "size 20 40");
    expect_term_size(&mut m, e.t, 40, 20);
    e.wait_persisted(40, 20);
    // Resizing again while it holds the size applies (the keyboard opened).
    m.resize(e.t, 40, 12);
    e.desk.output_until(e.t, "size 12 40");
    expect_term_size(&mut m, e.t, 40, 12);
}

#[test]
fn desktop_input_takes_size_back_and_mobile_is_told() {
    let (mut e, mut m) = mobile_viewing();
    m.input(e.t, "\r");
    expect_term_size(&mut m, e.t, 40, 20);
    e.desk.input(e.t, "\r");
    e.desk.output_until(e.t, "size 30 100");
    expect_term_size(&mut m, e.t, 100, 30);
    e.wait_persisted(100, 30);
    // Two quick SIGWINCHs can both report the latest size: let a late report arrive first.
    e.desk.drain_for(ms(300));
    // The Mobile's resize is recorded only again.
    m.resize(e.t, 41, 20);
    no_size_line_after(&mut e.desk, e.t, ms(600));
}

/// A reply from the Chat View (input without a sized view) never resizes.
#[test]
fn mobile_chat_reply_never_resizes() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    m.input(e.t, "\r");
    no_size_line_after(&mut e.desk, e.t, ms(800));
    // Nor does a view it opens afterwards, until it types there.
    attach_at(&mut m, e.t);
    m.resize(e.t, 40, 20);
    no_size_line_after(&mut e.desk, e.t, ms(800));
}

#[test]
fn mobile_owner_detach_restores_desktop_size() {
    let (mut e, mut m) = mobile_viewing();
    m.input(e.t, "\r");
    e.desk.output_until(e.t, "size 20 40");
    m.request(&ClientMsg::TermDetach { terminal: e.t }).unwrap();
    e.desk.output_until(e.t, "size 30 100");
    e.wait_persisted(100, 30);
}

#[test]
fn mobile_owner_disconnect_restores_desktop_size() {
    let (mut e, mut m) = mobile_viewing();
    m.input(e.t, "\r");
    e.desk.output_until(e.t, "size 20 40");
    m.shutdown();
    e.desk.output_until(e.t, "size 30 100");
    e.wait_persisted(100, 30);
    let deadline = Instant::now() + T;
    while e.srv.size_tracked() != 1 {
        assert!(Instant::now() < deadline, "{}", e.srv.size_tracked());
        std::thread::sleep(ms(20));
    }
}

// ---- Nudge and replay -----------------------------------------------------------------------

#[test]
fn mobile_attach_does_not_nudge_when_watched() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    no_size_line_after(&mut e.desk, e.t, ms(1000));
}

#[test]
fn mobile_attach_nudges_when_alone() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    e.desk
        .request(&ClientMsg::TermDetach { terminal: e.t })
        .unwrap();
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.skip_output(e.t);
    assert!(
        m.try_output_until(e.t, b"size 29 100", ms(3000)).is_some(),
        "no redraw nudge: {:?}",
        m.summary().iter().rev().take(5).collect::<Vec<_>>()
    );
}

#[test]
fn mobile_replay_is_capped() {
    // About 600 KiB.
    let mut e = env("yes \"$(printf '%0100d' 0)\" | head -n 6000\necho END\n");
    e.desk.attach(e.t);
    assert!(e.desk.try_output_until(e.t, b"END", ms(20_000)).is_some());
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.output_until(e.t, "END");
    m.drain_for(ms(200));
    let got = m.out[&e.t].clone();
    assert!(got.starts_with(b"\x1bc"));
    assert!(got.len() <= 2 + 256 * 1024, "{}", got.len());
    assert!(got.len() > 200 * 1024, "{}", got.len());
    // A Desktop gets the whole buffer.
    let mut d2 = Client::in_process(&e.srv, Role::Desktop);
    attach_at(&mut d2, e.t);
    d2.output_until(e.t, "END");
    assert!(d2.out[&e.t].len() > 600_000, "{}", d2.out[&e.t].len());
}

#[test]
fn mobile_detach_stops_output() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.drain_for(ms(1200));
    assert_eq!(e.srv.attached(), 2);
    let id = m.request_id();
    m.send(&ClientMsg::TermDetach { terminal: e.t }, Some(id));
    m.wait_res(id).unwrap();
    let res = m
        .log
        .iter()
        .position(|x| matches!(x, Ev::Msg(ServerMsg::Res(r)) if r.id == id))
        .unwrap();
    m.drain_for(ms(1500));
    assert!(
        !m.log[res..].iter().any(|x| matches!(x, Ev::Out(..))),
        "output after the detach"
    );
    assert_eq!(e.srv.attached(), 1);
}

/// Held output is dropped on detach: a quick re-attach gets only the fresh replay.
#[test]
fn mobile_detach_drops_held_output() {
    let mut e = env(TICKER);
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.drain_for(ms(1300));
    let id = m.request_id();
    m.send(&ClientMsg::TermDetach { terminal: e.t }, Some(id));
    let (_, _) = attach_at(&mut m, e.t);
    m.wait_res(id).unwrap();
    m.drain_for(ms(500));
    let detached = m
        .log
        .iter()
        .position(|x| matches!(x, Ev::Msg(ServerMsg::Res(r)) if r.id == id))
        .unwrap();
    let first = m.log[detached..]
        .iter()
        .find_map(|x| match x {
            Ev::Out(t, d) if *t == e.t => Some(d.clone()),
            _ => None,
        })
        .expect("the replay");
    assert!(
        first.starts_with(b"\x1bc"),
        "{:?}",
        String::from_utf8_lossy(&first[..first.len().min(40)])
    );
}

/// A Mobile whose queue overflows while a Desktop watches gets a fresh tail of the replay;
/// nobody's PTY is nudged. The agent records every SIGWINCH in a file, which dropped output
/// cannot hide.
///
/// The overflow does not depend on the machine's speed: the Mobile's output is held for a
/// minute, so the whole 1 MiB flood queues against its 256 KiB cap; its input then makes the
/// held output due. The flood is printed by shell builtins in 64 KiB bursts, so the Desktop,
/// which drains as it goes, never comes near the cap (its overflow would nudge).
/// (`until read`: a SIGWINCH, such as the Desktop's attach nudge, interrupts `read`.)
#[test]
fn mobile_overflow_with_desktop_attached_does_not_nudge() {
    let mut e = env_with(
        "trap 'echo WINCH >> winch.log' WINCH\necho ready\n\
         s=b; while [ ${#s} -lt 4000 ]; do s=$s$s; done\nuntil read x; do :; done\ni=0\n\
         while [ $i -lt 256 ]; do printf '%s\\n' \"$s\"; i=$((i+1)); \
         if [ $((i % 16)) -eq 0 ]; then sleep 0.01; fi; done\necho DONE\n",
        |c| {
            c.conn_output_cap = 256 * 1024;
            c.mobile_frame_idle = Duration::from_secs(60);
            c.mobile_replay_cap = 64 * 1024;
        },
    );
    e.desk_watching(100, 30);
    let mut m = e.mobile();
    attach_at(&mut m, e.t);
    m.output_until(e.t, "ready");
    // From here on (the Desktop's own attach nudge is over), no SIGWINCH at all.
    let winch = e.cwd.join("winch.log");
    let _ = fs::remove_file(&winch);
    e.desk.quiet = true;
    e.desk.input(e.t, "\n");
    let slow = Duration::from_secs(30);
    assert!(
        e.desk.try_output_until(e.t, b"DONE", slow).is_some(),
        "no DONE at the Desktop"
    );
    let flooded = m.log.len();
    // Nothing reached the Mobile during the flood: its output is held.
    m.drain_for(ms(200));
    assert!(
        !m.log[flooded..].iter().any(|x| matches!(x, Ev::Out(..))),
        "the Mobile's output was not held"
    );
    m.skip_output(e.t);
    // Typing makes it due: the recovery's reset and tail, ending with the last line.
    m.input(e.t, "");
    let got = m
        .try_output_until(e.t, b"DONE", slow)
        .expect("no DONE at the Mobile");
    assert!(got.starts_with(b"\x1bc"), "no recovery replay");
    assert!(
        got.len() < 256 * 1024,
        "{} bytes: nothing was dropped",
        got.len()
    );
    // A nudge's SIGWINCH lands while the agent prints or right after.
    e.desk.drain_for(ms(1500));
    assert!(
        !winch.exists(),
        "the PTY was nudged: {:?}",
        fs::read_to_string(&winch)
    );
}

#[test]
fn relaunch_keeps_mobile_size_ownership_and_pacing() {
    let (mut e, mut m) = mobile_viewing();
    m.input(e.t, "\r");
    e.desk.output_until(e.t, "size 20 40");
    let r = e
        .desk
        .request(&ClientMsg::TermRelaunch {
            terminal: e.t,
            skip_permissions: true,
        })
        .unwrap();
    assert_eq!(r["relaunched"], json!(true));
    // The replacement starts at the Mobile's size, and the Mobile still holds it.
    e.desk.output_until(e.t, "size 20 40 args");
    m.output_until(e.t, "size 20 40 args");
    e.desk.output_until(e.t, "ready");
    e.desk.drain_for(ms(300));
    m.resize(e.t, 40, 12);
    e.desk.output_until(e.t, "size 12 40");
    expect_term_size(&mut m, e.t, 40, 12);
    // Still paced once the input's burst is over.
    m.drain_for(ms(3200));
    let from = Instant::now();
    m.drain_for(ms(2500));
    let late = in_window(&frames(&m, e.t), from, from + ms(2500));
    assert!(late.len() <= 4, "{} frames", late.len());
}

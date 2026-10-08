#![cfg(unix)]
//! Terminal lifecycle against an in-process server: open, attach and replay, shared input,
//! size, close, exit, disconnects, list broadcasts and slow consumers.

mod common;

use common::*;
use serde_json::{json, Map};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::protocol::frame::Frame;
use xshell_core::protocol::msg::{ClientMsg, ServerMsg};
use xshell_core::terminal::OVERFLOW_NOTICE;

fn client(srv: &xshelld::server::ServerHandle) -> Client {
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    c
}

fn exit_of(c: &mut Client, t: Uuid) -> i32 {
    match c.expect_msg(
        "term.exit",
        |m| matches!(m, ServerMsg::TermExit { terminal, .. } if *terminal == t),
    ) {
        ServerMsg::TermExit { code, .. } => code,
        _ => unreachable!(),
    }
}

/// The index in the log of `t`'s `term.exit`.
fn exit_index(c: &Client, t: Uuid) -> usize {
    c.log
        .iter()
        .position(|e| matches!(e, Ev::Msg(ServerMsg::TermExit { terminal, .. }) if *terminal == t))
        .expect("term.exit in log")
}

#[test]
fn open_attach_replay() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let ok = a.open(t, sh_spec(&h.project("p")));
    assert!(ok_pid(&ok) > 1);
    assert_eq!(a.attach(t), json!({"exitCode": null}));
    a.marker(t, "abc");

    let mut b = client(&srv);
    b.attach(t);
    let replay = b.output_until(t, "abc");
    assert!(
        replay.starts_with(b"\x1bc"),
        "{:?}",
        String::from_utf8_lossy(&replay)
    );
}

#[test]
fn output_before_attach_is_replayed() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let prog = h.script("early", "echo early\nexec sleep 1000");
    a.open(t, raw_spec(&h.project("p"), &prog));
    std::thread::sleep(Duration::from_millis(300));
    a.attach(t);
    a.output_until(t, "early");
}

#[test]
fn two_connections_same_terminal() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let mut b = client(&srv);
    let t = Uuid::new_v4();
    a.open(t, sh_spec(&h.project("p")));
    a.attach(t);
    b.attach(t);
    a.marker(t, "m1x");
    b.output_until(t, "m1x");
    b.marker(t, "m2x");
    a.output_until(t, "m2x");
}

#[test]
fn resize_follows_last_input() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let mut b = client(&srv);
    let t = Uuid::new_v4();
    a.open(t, sh_spec(&h.project("p")));
    a.attach(t);
    b.attach(t);
    a.resize(t, 100, 30);
    b.resize(t, 80, 20);
    b.expect_size(t, 20, 80);
    // Input makes A the owner again, which applies A's size.
    a.expect_size(t, 30, 100);
    // The size is persisted (debounced) so a restore uses it.
    let deadline = Instant::now() + T;
    loop {
        let s = h.state_json();
        if s["terminals"][0]["cols"] == json!(100) && s["terminals"][0]["rows"] == json!(30) {
            break;
        }
        assert!(Instant::now() < deadline, "size not persisted: {s}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn attach_nudges_redraw() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let prog = h.script(
        "winch",
        "trap 'echo WINCH' WINCH\necho ready\nwhile :; do sleep 0.05; done",
    );
    a.open(t, raw_spec(&h.project("p"), &prog));
    a.attach(t);
    a.output_until(t, "ready");
    // Let A's own attach nudge (if the trap was set in time) pass, then forget it.
    a.drain_for(Duration::from_millis(400));
    a.skip_output(t);
    // B's attach nudges again; A, still attached, sees the live repaint trigger.
    let mut b = client(&srv);
    b.attach(t);
    assert!(
        a.try_output_until(t, b"WINCH", Duration::from_secs(2))
            .is_some(),
        "no WINCH after the second attach: {:?}",
        a.summary()
    );
}

#[test]
fn close_ends_everywhere_with_exit_event() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let mut b = client(&srv);
    let t = Uuid::new_v4();
    let pid = ok_pid(&a.open(t, sh_spec(&h.project("p"))));
    a.attach(t);
    b.attach(t);
    assert_eq!(h.state_ids(), vec![t]);
    b.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    for c in [&mut a, &mut b] {
        exit_of(c, t);
        c.terminals_where(|l| l.iter().all(|i| i.terminal != t));
    }
    assert!(h.state_ids().is_empty());
    assert!(wait_dead(pid, T));
}

#[test]
fn close_kills_foreground_job_group() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    a.open(
        t,
        raw_spec(&h.project("p"), std::path::Path::new("/bin/sh")),
    );
    a.attach(t);
    a.marker(t, "ready");
    a.input(t, "sh -c 'echo PI\"\"D=$$; exec sleep 1000'\n");
    a.output_until(t, "PID=");
    let line = a.output_until(t, "\n");
    let job: i32 = String::from_utf8_lossy(&line).trim().parse().expect("pid");
    let _reaper = PidReaper(vec![job]);
    a.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    assert!(
        wait_dead(job, Duration::from_secs(3)),
        "foreground job survived"
    );
    exit_of(&mut a, t);
}

/// The shell dies on SIGHUP but its foreground job ignores it: that group is SIGKILLed
/// after the grace period even though its leader is gone.
#[test]
fn close_escalates_per_group() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    a.open(
        t,
        raw_spec(&h.project("p"), std::path::Path::new("/bin/sh")),
    );
    a.attach(t);
    a.marker(t, "ready");
    a.input(
        t,
        "sh -c 'trap \"\" HUP; echo PI\"\"D=$$; exec sleep 1000'\n",
    );
    a.output_until(t, "PID=");
    let line = a.output_until(t, "\n");
    let job: i32 = String::from_utf8_lossy(&line).trim().parse().expect("pid");
    let _reaper = PidReaper(vec![job]);
    a.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    assert!(
        wait_dead(job, Duration::from_secs(3)),
        "HUP-ignoring job survived"
    );
    exit_of(&mut a, t);
}

#[test]
fn close_escalates_to_sigkill() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let prog = h.script(
        "stubborn",
        "trap '' HUP TERM\necho ready\nwhile :; do sleep 1; done",
    );
    let pid = ok_pid(&a.open(t, raw_spec(&h.project("p"), &prog)));
    let _reaper = PidReaper(vec![pid]);
    a.attach(t);
    a.output_until(t, "ready");
    let t0 = Instant::now();
    a.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    exit_of(&mut a, t);
    assert!(t0.elapsed() < Duration::from_millis(300) + Duration::from_secs(2));
    assert!(wait_dead(pid, T));
}

#[test]
fn process_exit_broadcasts_and_stays_listed() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut watcher = client(&srv);
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let prog = h.script("seven", "exit 7");
    a.open(t, raw_spec(&h.project("p"), &prog));
    assert_eq!(exit_of(&mut watcher, t), 7);
    let list =
        watcher.terminals_where(|l| l.iter().any(|i| i.terminal == t && i.exit_code.is_some()));
    assert_eq!(
        list.iter().find(|i| i.terminal == t).unwrap().exit_code,
        Some(7)
    );
    assert_eq!(h.state_ids(), vec![t]);
    // The state file stops naming the ended process at once: its pid may be reused.
    let deadline = Instant::now() + T;
    while h.state_json()["terminals"][0]["leader"] != json!(null) {
        assert!(
            Instant::now() < deadline,
            "leader still persisted: {}",
            h.state_json()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Input to an exited Terminal is refused.
    let err = a
        .request(&ClientMsg::TermInput {
            terminal: t,
            data: "x".into(),
        })
        .unwrap_err();
    assert_eq!(err, "terminal has exited");
    // Closing it removes it without signalling anything.
    a.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    watcher.terminals_where(|l| l.is_empty());
    assert!(h.state_ids().is_empty());
}

/// Attach to an exited Terminal: reply, replay, then `term.exit`, in that order.
#[test]
fn attach_exited_sends_exit_after_replay() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let prog = h.script("seven", "echo bye\nexit 7");
    a.open(t, raw_spec(&h.project("p"), &prog));
    exit_of(&mut a, t);

    let mut b = client(&srv);
    let id = b.request_id();
    b.send(&ClientMsg::TermAttach { terminal: t }, Some(id));
    assert_eq!(b.wait_res(id).unwrap(), json!({"exitCode": 7}));
    b.output_until(t, "bye");
    assert_eq!(exit_of(&mut b, t), 7);
    let res = b
        .log
        .iter()
        .position(|e| matches!(e, Ev::Msg(ServerMsg::Res(r)) if r.id == id))
        .unwrap();
    let first_out = b
        .log
        .iter()
        .position(|e| matches!(e, Ev::Out(x, _) if *x == t))
        .unwrap();
    let exit = exit_index(&b, t);
    assert!(res < first_out && first_out < exit, "{:?}", b.summary());
}

/// `term.exit` follows everything the process wrote before exiting; no output after it.
#[test]
fn exit_follows_final_burst() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let prog = h.script(
        "burst",
        "read go\ni=0\nwhile [ $i -lt 2000 ]; do echo 0123456789012345678901234567890123456789; i=$((i+1)); done\necho EN\"\"D\nexit 4",
    );
    a.open(t, raw_spec(&h.project("p"), &prog));
    // Attached before it prints: everything must arrive live, ahead of `term.exit`.
    a.attach(t);
    a.input(t, "go\n");
    assert_eq!(exit_of(&mut a, t), 4);
    let exit = exit_index(&a, t);
    let before: Vec<u8> = a.log[..exit]
        .iter()
        .filter_map(|e| match e {
            Ev::Out(x, d) if *x == t => Some(d.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        find(&before, b"END").is_some(),
        "final output missing before term.exit"
    );
    a.drain_for(Duration::from_millis(300));
    assert!(
        !a.log[exit..]
            .iter()
            .any(|e| matches!(e, Ev::Out(x, _) if *x == t)),
        "output after term.exit"
    );
}

/// `term.exit` waits for the output of a descendant that outlives the leader. Linux only:
/// BSD kernels revoke the terminal from the session when its leader exits, so there a
/// descendant cannot write after the leader is gone.
#[cfg(target_os = "linux")]
#[test]
fn exit_follows_all_output() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    // Ignored before the fork, so the subshell survives the hangup the leader's exit sends
    // to its process group.
    let prog = h.script("late", "trap '' HUP\n(sleep 0.3; echo LATE) &\nexit 3");
    a.open(t, raw_spec(&h.project("p"), &prog));
    a.attach(t);
    assert_eq!(exit_of(&mut a, t), 3);
    let exit = exit_index(&a, t);
    assert!(
        a.out.get(&t).is_some_and(|o| find(o, b"LATE").is_some()),
        "{:?}",
        a.summary()
    );
    a.drain_for(Duration::from_millis(300));
    assert!(
        !a.log[exit..]
            .iter()
            .any(|e| matches!(e, Ev::Out(x, _) if *x == t)),
        "output after term.exit: {:?}",
        a.summary()
    );
}

#[test]
fn connection_drop_ends_nothing() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    let pid = ok_pid(&a.open(t, sh_spec(&h.project("p"))));
    a.attach(t);
    a.marker(t, "one");
    a.shutdown();
    drop(a);
    let mut c = Client::connect(&srv.socket);
    let (_, list) = c.hello(range(1, 1));
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].terminal, t);
    assert_eq!(list[0].pid, Some(pid as u32));
    c.attach(t);
    c.marker(t, "two");
}

#[test]
fn terminals_list_broadcast_on_change() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let mut b = client(&srv);
    let t = Uuid::new_v4();
    b.open(t, sh_spec(&h.project("p")));
    a.terminals_where(|l| l.iter().any(|i| i.terminal == t));
    let mut meta = Map::new();
    meta.insert("title".into(), json!("x"));
    b.request(&ClientMsg::TermUpdate {
        terminal: t,
        session_id: Some("s1".into()),
        meta: Some(meta),
    })
    .unwrap();
    let l = a.terminals_where(|l| l.iter().any(|i| i.meta.get("title") == Some(&json!("x"))));
    assert_eq!(l[0].spec.session_id.as_deref(), Some("s1"));
    assert_eq!(
        h.state_json()["terminals"][0]["spec"]["sessionId"],
        json!("s1")
    );
    // A null deletes the key.
    let mut meta = Map::new();
    meta.insert("title".into(), json!(null));
    b.request(&ClientMsg::TermUpdate {
        terminal: t,
        session_id: None,
        meta: Some(meta),
    })
    .unwrap();
    a.terminals_where(|l| l.iter().any(|i| i.terminal == t && i.meta.is_empty()));
    b.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    a.terminals_where(|l| l.is_empty());
}

#[test]
fn open_duplicate_uuid_rejected() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    a.open(t, sh_spec(&h.project("p")));
    let err = a
        .request(&open_msg(t, sh_spec(&h.project("p"))))
        .unwrap_err();
    assert!(err.contains("already exists"), "{err}");
}

#[test]
fn open_missing_cwd_rejected() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let err = a
        .request(&open_msg(
            Uuid::new_v4(),
            sh_spec(std::path::Path::new("/nonexistent")),
        ))
        .unwrap_err();
    assert!(err.contains("does not exist"), "{err}");
    let mut b = Client::connect(&srv.socket);
    assert!(b.hello(range(1, 1)).1.is_empty());
    assert!(!h.paths().state.exists() || h.state_ids().is_empty());
}

fn yes_terminal(h: &TestHome, b: &mut Client) -> Uuid {
    let t = Uuid::new_v4();
    let prog = h.script("yes", "exec yes xshell");
    b.open(t, raw_spec(&h.project("p"), &prog));
    t
}

#[test]
fn stalled_connection_does_not_block_others() {
    let h = TestHome::new();
    let srv = start(&h, |c| {
        c.conn_output_cap = 64 * 1024;
        c.replay_capacity = 32 * 1024;
        c.write_stall_timeout = Duration::from_secs(30);
    });
    let mut b = client(&srv);
    b.quiet = true;
    let t = yes_terminal(&h, &mut b);
    let mut a = RawClient::connect(&srv.socket);
    a.hello();
    a.attach(t);
    b.attach(t);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        b.output_until(t, "xshell\r\n");
        b.drain_for(Duration::from_millis(50));
    }
    let c0 = Instant::now();
    get_home(&mut b);
    assert!(c0.elapsed() < Duration::from_secs(1));
    // A resumes: it must see the overflow notice, or a confirmed EOF if it was dropped.
    let deadline = Instant::now() + T;
    let (mut seen, mut disconnected) = (false, false);
    let mut tail: Vec<u8> = Vec::new();
    while !seen && !disconnected {
        let left = deadline.saturating_duration_since(Instant::now());
        match a.read(left) {
            Got::Frame(Frame::Output { data, .. }) => {
                tail.extend_from_slice(&data);
                seen = find(&tail, OVERFLOW_NOTICE).is_some();
                let keep = tail.len().saturating_sub(OVERFLOW_NOTICE.len());
                tail.drain(..keep);
            }
            Got::Frame(_) => {}
            Got::Eof => disconnected = true,
            Got::Timeout => break,
        }
    }
    assert!(
        seen || disconnected,
        "the stalled reader got neither the overflow notice nor a disconnect"
    );
    b.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    exit_of(&mut b, t);
}

#[test]
fn stalled_connection_is_dropped() {
    let h = TestHome::new();
    let srv = start(&h, |c| {
        c.write_stall_timeout = Duration::from_millis(500);
        c.conn_output_cap = 64 * 1024;
        c.replay_capacity = 32 * 1024;
    });
    let mut b = client(&srv);
    b.quiet = true;
    let t = yes_terminal(&h, &mut b);
    let mut a = RawClient::connect(&srv.socket);
    a.hello();
    a.attach(t);
    b.attach(t);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        b.output_until(t, "xshell\r\n");
        b.drain_for(Duration::from_millis(50));
    }
    let (eof, _) = a.drain_until_eof(T, b"");
    assert!(eof, "stalled connection was not dropped");
    get_home(&mut b);
    b.request(&ClientMsg::TermClose { terminal: t }).unwrap();
    exit_of(&mut b, t);
}

/// A peer that never reads and keeps sending requests is disconnected once its replies
/// exceed the total cap; others are unaffected.
#[test]
fn control_flood_disconnects_peer() {
    let h = TestHome::new();
    let srv = start(&h, |c| {
        c.conn_total_cap = 256 * 1024;
        c.write_stall_timeout = Duration::from_secs(30);
    });
    let mut a = RawClient::connect(&srv.socket);
    a.hello();
    let req = xshell_core::protocol::msg::encode_msg(
        &ClientMsg::Call {
            method: "get_home_dir".into(),
            params: json!({}),
        },
        Some(1),
    )
    .unwrap();
    // Write until the Daemon hangs up. A watchdog cuts the socket so a bug fails the test
    // instead of hanging it.
    use std::io::Write;
    let watchdog = a.sock.try_clone().unwrap();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(20) {
            if done2.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = watchdog.shutdown(std::net::Shutdown::Both);
    });
    let t0 = Instant::now();
    let mut cut = false;
    while t0.elapsed() < Duration::from_secs(20) {
        if a.sock.write_all(&req).is_err() {
            cut = t0.elapsed() < Duration::from_secs(19);
            break;
        }
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(cut, "flooding peer was never disconnected");
    let (eof, _) = a.drain_until_eof(T, b"");
    assert!(eof);
    let mut b = client(&srv);
    get_home(&mut b);
}

/// One Terminal's metadata, or the whole list, over budget is refused before anything
/// changes, so a `terminals` list can never grow past the frame limit.
#[test]
fn terminal_list_budget_enforced() {
    let h = TestHome::new();
    let srv = start(&h, |c| {
        c.max_terminal_bytes = 2000;
        c.max_list_bytes = 3000;
    });
    let mut a = client(&srv);
    let open_with = |a: &mut Client, title_len: usize| {
        let t = Uuid::new_v4();
        let mut meta = Map::new();
        meta.insert("title".into(), json!("x".repeat(title_len)));
        let r = a.request(&ClientMsg::TermOpen {
            spec: xshell_core::protocol::msg::OpenSpec {
                terminal: t,
                launch: sh_spec(&h.project("p")),
                cols: 80,
                rows: 24,
                meta,
            },
        });
        (t, r)
    };
    let (_, r) = open_with(&mut a, 2500);
    let e = r.unwrap_err();
    assert!(e.contains("terminal metadata too large"), "{e}");
    let (t1, r) = open_with(&mut a, 1200);
    r.unwrap();
    let (_, r) = open_with(&mut a, 1200);
    let e = r.unwrap_err();
    assert!(e.contains("terminal list too large"), "{e}");
    let (t2, r) = open_with(&mut a, 0);
    r.unwrap();
    let mut meta = Map::new();
    meta.insert("title".into(), json!("y".repeat(1200)));
    let e = a
        .request(&ClientMsg::TermUpdate {
            terminal: t2,
            session_id: Some("s".into()),
            meta: Some(meta),
        })
        .unwrap_err();
    assert!(e.contains("terminal list too large"), "{e}");
    // Nothing changed: two Terminals, t2 without the rejected update.
    let mut b = Client::connect(&srv.socket);
    let (_, list) = b.hello(range(1, 1));
    let mut ids: Vec<Uuid> = list.iter().map(|i| i.terminal).collect();
    ids.sort();
    let mut want = vec![t1, t2];
    want.sort();
    assert_eq!(ids, want);
    let e2 = list.iter().find(|i| i.terminal == t2).unwrap();
    assert_eq!(e2.meta.get("title"), Some(&json!("")));
    assert_eq!(e2.spec.session_id, None);
}

/// A connection that resized a Terminal without attaching is forgotten by its size arbiter
/// when it disconnects.
#[test]
fn disconnect_forgets_size_without_attach() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut a = client(&srv);
    let t = Uuid::new_v4();
    a.open(t, sh_spec(&h.project("p")));
    for _ in 0..3 {
        let mut r = client(&srv);
        r.resize(t, 100, 30);
        r.input(t, "");
        r.shutdown();
    }
    let deadline = Instant::now() + T;
    while srv.size_tracked() != 0 {
        assert!(
            Instant::now() < deadline,
            "{} size entries left",
            srv.size_tracked()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

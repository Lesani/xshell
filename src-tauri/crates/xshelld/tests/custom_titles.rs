#![cfg(unix)]
//! Custom-name writes through the real protocol, with actual state-file faults.
mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use uuid::Uuid;
use xshell_protocol::msg::{ClientMsg, ServerMsg, TerminalInfo};
use xshell_protocol::terminal_meta::CUSTOM_TITLE_KEY;
use xshelld::server::{ServerHandle, TestHook, TestPoint};

fn update(c: &mut Client, t: Uuid, meta: Value) -> Result<Value, String> {
    c.request(&ClientMsg::TermUpdate {
        terminal: t,
        session_id: None,
        meta: Some(meta.as_object().unwrap().clone()),
    })
}

fn current(s: &ServerHandle, t: Uuid) -> TerminalInfo {
    let mut c = Client::connect(&s.socket);
    c.hello(range(1, 1))
        .1
        .into_iter()
        .find(|i| i.terminal == t)
        .unwrap()
}

fn connect(s: &ServerHandle) -> Client {
    let mut c = Client::connect(&s.socket);
    c.hello(range(1, 1));
    c
}

fn assert_name(c: &mut Client, t: Uuid, name: Option<&str>) {
    c.terminals_where(|list| {
        list.iter().any(|i| {
            i.terminal == t && i.meta.get(CUSTOM_TITLE_KEY).and_then(Value::as_str) == name
        })
    });
}

#[test]
fn custom_title_save_automatic_update_and_combined_clear_reach_two_desktops() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let (mut a, mut b) = (connect(&srv), connect(&srv));
    let t = Uuid::new_v4();
    a.open(t, sh_spec(&h.project("app")));
    update(
        &mut a,
        t,
        json!({"title":"automatic", "customTitle":"  Review  queue 🦀  "}),
    )
    .unwrap();
    for c in [&mut a, &mut b] {
        assert_name(c, t, Some("Review  queue 🦀"));
    }
    update(&mut b, t, json!({"title":"latest"})).unwrap();
    assert_eq!(
        current(&srv, t).meta,
        json!({"title":"latest", "customTitle":"Review  queue 🦀"})
            .as_object()
            .unwrap()
            .clone()
    );
    update(&mut a, t, json!({"customTitle":null})).unwrap();
    for c in [&mut a, &mut b] {
        assert_name(c, t, None);
    }
    assert_eq!(current(&srv, t).meta["title"], "latest");
    update(&mut a, t, json!({"customTitle":"again"})).unwrap();
    update(&mut b, t, json!({"title":null, "customTitle":null})).unwrap();
    assert!(current(&srv, t).meta.is_empty());
    assert_eq!(h.state_json()["terminals"][0]["meta"], json!({}));
}

#[test]
fn invalid_custom_title_patch_is_atomic_including_session_and_budget() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = connect(&srv);
    let t = Uuid::new_v4();
    c.open(t, sh_spec(&h.project("app")));
    update(&mut c, t, json!({"title":"old", "customTitle":"kept"})).unwrap();
    let bytes = fs::read(h.paths().state).unwrap();
    let old = current(&srv, t);
    for invalid in [
        json!(true),
        json!(9),
        json!({}),
        json!([]),
        json!("\nname"),
        json!("name\u{85}"),
        json!("  "),
        json!("🦀".repeat(257)),
    ] {
        let r = c.request(&ClientMsg::TermUpdate {
            terminal: t,
            session_id: Some("new-session".into()),
            meta: Some(
                json!({"customTitle":invalid,"title":"wrong"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        });
        assert!(r.is_err());
        assert_eq!(current(&srv, t), old);
        assert_eq!(fs::read(h.paths().state).unwrap(), bytes);
    }
    assert!(update(
        &mut c,
        t,
        json!({"customTitle":"valid", "other":"x".repeat(300_000)})
    )
    .unwrap_err()
    .contains("too large"));
    assert_eq!(current(&srv, t), old);
    assert_eq!(fs::read(h.paths().state).unwrap(), bytes);
}

#[test]
fn preinstall_refusal_preserves_record_file_and_broadcast_for_rename_clear_and_equal_retry() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let (mut c, mut watcher) = (connect(&srv), connect(&srv));
    let t = Uuid::new_v4();
    c.open(t, sh_spec(&h.project("app")));
    // Clearing absent is still a write, not a no-op success.
    for initial in [None, Some("kept")] {
        if let Some(name) = initial {
            update(&mut c, t, json!({"customTitle":name})).unwrap();
        }
        // This observer is not attached: detach is a no-op synchronous barrier.
        // Consume all queued events through its response before marking the log.
        watcher
            .request(&ClientMsg::TermDetach { terminal: t })
            .unwrap();
        let mark = watcher.log.len();
        let bytes = fs::read(h.paths().state).unwrap();
        let old = current(&srv, t);
        let tmp = h.paths().state.with_file_name("terminals.json.tmp");
        fs::create_dir(&tmp).unwrap();
        for patch in [
            json!({"customTitle":"new", "title":"wrong"}),
            json!({"customTitle":null}),
            json!({"customTitle":initial}),
        ] {
            let r = c.request(&ClientMsg::TermUpdate {
                terminal: t,
                session_id: Some("wrong-session".into()),
                meta: Some(patch.as_object().unwrap().clone()),
            });
            let error = r.unwrap_err();
            assert!(error.contains("cannot save"), "{error}");
            assert!(!error.starts_with(xshell_protocol::CUSTOM_TITLE_NOT_DURABLE));
            assert_eq!(current(&srv, t), old);
            assert_eq!(fs::read(h.paths().state).unwrap(), bytes);
        }
        watcher
            .request(&ClientMsg::TermDetach { terminal: t })
            .unwrap();
        assert!(!watcher.log[mark..]
            .iter()
            .any(|e| matches!(e, Ev::Msg(ServerMsg::Terminals { .. }))));
        fs::remove_dir(tmp).unwrap();
        update(&mut c, t, json!({"customTitle":initial})).unwrap();
    }
}

#[test]
fn postinstall_sync_error_lists_installed_value_and_equal_retry_confirms_durability() {
    let h = TestHome::new();
    let fail = Arc::new(AtomicBool::new(false));
    let hook_fail = fail.clone();
    let srv = start(&h, |c| {
        c.test_hook = Some(TestHook(Arc::new(move |_, p| {
            p == TestPoint::CustomTitleDirectorySync && hook_fail.load(Ordering::SeqCst)
        })))
    });
    let (mut c, mut watcher) = (connect(&srv), connect(&srv));
    let t = Uuid::new_v4();
    c.open(t, sh_spec(&h.project("app")));
    update(&mut c, t, json!({"customTitle":"old"})).unwrap();
    for (patch, expected) in [
        (
            json!({"customTitle":"new", "title":"automatic"}),
            Some("new"),
        ),
        (json!({"customTitle":null}), None),
    ] {
        fail.store(true, Ordering::SeqCst);
        let error = update(&mut c, t, patch.clone()).unwrap_err();
        assert!(
            error.starts_with(xshell_protocol::CUSTOM_TITLE_NOT_DURABLE),
            "{error}"
        );
        assert_name(&mut watcher, t, expected);
        let live = current(&srv, t);
        assert_eq!(
            live.meta.get(CUSTOM_TITLE_KEY).and_then(Value::as_str),
            expected
        );
        assert_eq!(h.state_json()["terminals"][0]["meta"], json!(live.meta));
        // Equal retry must really reach the sync hook again, not short-circuit equality.
        assert!(update(&mut c, t, patch.clone())
            .unwrap_err()
            .starts_with(xshell_protocol::CUSTOM_TITLE_NOT_DURABLE));
        fail.store(false, Ordering::SeqCst);
        update(&mut c, t, patch).unwrap();
        assert_eq!(current(&srv, t).meta, live.meta);
        assert_eq!(h.state_json()["terminals"][0]["meta"], json!(live.meta));
    }
}

#[test]
fn concurrent_custom_title_writers_converge_to_disk_and_reconnect_list() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let (mut a, mut b) = (connect(&srv), connect(&srv));
    let t = Uuid::new_v4();
    a.open(t, sh_spec(&h.project("app")));
    let barrier = Arc::new(Barrier::new(2));
    let other = barrier.clone();
    let thread = std::thread::spawn(move || {
        other.wait();
        update(&mut b, t, json!({"customTitle":"B"})).unwrap();
        b
    });
    barrier.wait();
    update(&mut a, t, json!({"customTitle":"A"})).unwrap();
    let mut b = thread.join().unwrap();
    let meta = current(&srv, t).meta;
    let name = meta[CUSTOM_TITLE_KEY].as_str().unwrap();
    assert!(["A", "B"].contains(&name));
    for c in [&mut a, &mut b] {
        assert_name(c, t, Some(name));
    }
    assert_eq!(h.state_json()["terminals"][0]["meta"], json!(meta));
}

#[test]
fn hidden_capability_refuses_custom_key_but_not_automatic_updates() {
    let h = TestHome::new();
    let srv = start(&h, |c| {
        c.hide_capabilities
            .push(xshell_protocol::cap::TERM_CUSTOM_TITLE.into())
    });
    let mut c = Client::connect(&srv.socket);
    assert!(!c
        .hello(range(1, 1))
        .0
        .capabilities
        .iter()
        .any(|s| s == xshell_protocol::cap::TERM_CUSTOM_TITLE));
    let t = Uuid::new_v4();
    c.open(t, sh_spec(&h.project("app")));
    for v in [json!("name"), Value::Null] {
        assert!(update(&mut c, t, json!({"customTitle":v}))
            .unwrap_err()
            .contains("does not support"));
    }
    update(&mut c, t, json!({"title":"automatic"})).unwrap();
    assert_eq!(
        current(&srv, t).meta,
        json!({"title":"automatic"}).as_object().unwrap().clone()
    );
}

#[test]
fn custom_title_open_metadata_is_refused_before_spawn_or_adoption() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = connect(&srv);
    for v in [json!("name"), Value::Null, json!(7), json!("bad\n")] {
        let mut msg = open_msg(Uuid::new_v4(), sh_spec(&h.project("app")));
        if let ClientMsg::TermOpen { spec } = &mut msg {
            spec.meta.insert(CUSTOM_TITLE_KEY.into(), v);
            spec.adopt_existing = true;
        }
        assert!(c
            .request(&msg)
            .unwrap_err()
            .contains("reserved for term.update"));
        assert!(h.state_ids().is_empty());
    }
    let mut fresh = Client::connect(&srv.socket);
    assert!(fresh.hello(range(1, 1)).1.is_empty());
}

#![cfg(unix)]
//! Handshake, framing errors and `call`, against an in-process server.

mod common;

use common::*;
use serde_json::json;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_protocol::frame::{encode_json, Frame};
use xshell_protocol::msg::{ClientMsg, ServerMsg};

#[test]
fn hello_negotiates_and_sends_terminals() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    let (hello, list) = c.hello(range(1, 5));
    assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(hello.protocol, range(1, 1));
    assert!(hello.capabilities.iter().any(|c| c == "term"));
    assert!(hello.capabilities.iter().any(|c| c == "term.relaunch"));
    assert!(list.is_empty());
}

#[test]
fn hello_no_overlap_rejected() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    c.send(
        &ClientMsg::Hello(xshell_protocol::msg::Hello {
            protocol: range(2, 3),
            version: "future".into(),
            capabilities: vec![],
        }),
        None,
    );
    c.expect_msg("hello", |m| matches!(m, ServerMsg::Hello(_)));
    let m = c.expect_msg("error", |m| matches!(m, ServerMsg::Error { .. }));
    let ServerMsg::Error { code, message } = m else {
        unreachable!()
    };
    assert_eq!(code, "protocol_mismatch");
    assert!(
        message.contains("1..1") && message.contains("2..3"),
        "{message}"
    );
    c.expect_eof();

    let mut ok = Client::connect(&srv.socket);
    ok.hello(range(1, 1));
}

#[test]
fn first_message_must_be_hello() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    c.send(
        &ClientMsg::Call {
            method: "get_home_dir".into(),
            params: json!({}),
        },
        Some(1),
    );
    let m = c.expect_msg("error", |m| matches!(m, ServerMsg::Error { .. }));
    assert!(matches!(m, ServerMsg::Error { code, .. } if code == "expected_hello"));
    c.expect_eof();
}

#[test]
fn unknown_type_gets_err_not_close() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    let mut f = Vec::new();
    encode_json(br#"{"t":"future.thing","id":9}"#, &mut f).unwrap();
    c.send_raw(&f);
    let err = c.wait_res(9).unwrap_err();
    assert!(err.contains("unknown message type"), "{err}");
    // Bad fields of a known type: an err under the same id, connection kept.
    let mut f = Vec::new();
    encode_json(br#"{"t":"term.resize","id":10,"terminal":"bad"}"#, &mut f).unwrap();
    c.send_raw(&f);
    assert!(c
        .wait_res(10)
        .unwrap_err()
        .starts_with("invalid term.resize"));
    assert_eq!(get_home(&mut c), json!(h.home().to_string_lossy()));
}

#[test]
fn malformed_frame_closes_only_that_connection() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut b = Client::connect(&srv.socket);
    b.hello(range(1, 1));
    let t = Uuid::new_v4();
    b.open(t, sh_spec(&h.project("p")));
    b.attach(t);

    let mut kind0_bad_json = Vec::new();
    encode_json(b"{not json", &mut kind0_bad_json).unwrap();
    let mut short_output = 11u32.to_be_bytes().to_vec();
    short_output.push(1);
    short_output.extend([0u8; 10]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("oversized header", vec![0xFF, 0xFF, 0xFF, 0xFF]),
        ("kind-0 bad json", kind0_bad_json),
        ("short kind-1", short_output),
    ];
    for (i, (what, bytes)) in cases.into_iter().enumerate() {
        let mut a = Client::connect(&srv.socket);
        a.hello(range(1, 1));
        a.send_raw(&bytes);
        a.expect_eof();
        b.marker(t, &format!("ok{i}x"));
        assert_eq!(
            get_home(&mut b),
            json!(h.home().to_string_lossy()),
            "{what}"
        );
    }
    // A well-formed kind-1 frame from a Desktop is a protocol error too.
    let mut a = Client::connect(&srv.socket);
    a.hello(range(1, 1));
    a.send_frame(&Frame::Output {
        terminal: t,
        data: b"x".to_vec(),
    });
    a.expect_eof();
    b.marker(t, "fin");
}

#[test]
fn call_roundtrip_get_home_dir() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    assert_eq!(get_home(&mut c), json!(h.home().to_string_lossy()));
    let err = c.call("nope", json!({})).unwrap_err();
    assert!(err.contains("unknown method"), "{err}");
    // Dropped files land in the Daemon's private temp dir, not the shared /tmp.
    let p = c
        .call(
            "save_dropped_file",
            json!({"bytesBase64": "aGk=", "name": "a.png"}),
        )
        .unwrap();
    let p = std::path::PathBuf::from(p.as_str().unwrap());
    assert!(p.starts_with(h.run().join("xshell").join("tmp")), "{p:?}");
    assert_eq!(fs::read(&p).unwrap(), b"hi");
}

/// A call that blocks (reading a FIFO nobody writes yet) must not hold up Terminal input on
/// the same connection.
#[test]
fn slow_call_does_not_block_input() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    let t = Uuid::new_v4();
    c.open(t, sh_spec(&h.project("p")));
    c.attach(t);
    c.marker(t, "ready");

    let fifo = h.root().join("fifo");
    let cpath = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let id = c.request_id();
    c.send(
        &ClientMsg::Call {
            method: "read_text_file".into(),
            params: json!({"path": fifo}),
        },
        Some(id),
    );
    let t0 = Instant::now();
    c.marker(t, "during");
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert!(
        c.try_msg(
            Duration::from_millis(1),
            |m| matches!(m, ServerMsg::Res(r) if r.id == id)
        )
        .is_none(),
        "the call finished before its input was written"
    );
    fs::write(&fifo, "done").unwrap();
    assert_eq!(c.wait_res(id).unwrap(), json!("done"));
}

#[test]
fn too_many_calls_rejected() {
    let h = TestHome::new();
    let srv = start(&h, |c| c.max_calls_per_conn = 1);
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    let fifo = h.root().join("fifo");
    let cpath = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let id = c.request_id();
    c.send(
        &ClientMsg::Call {
            method: "read_text_file".into(),
            params: json!({"path": fifo}),
        },
        Some(id),
    );
    // The first call is in flight, so the second is refused.
    let deadline = Instant::now() + T;
    let err = loop {
        match c.call("get_home_dir", json!({})) {
            Err(e) => break e,
            Ok(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(_) => panic!("second call was never refused"),
        }
    };
    assert_eq!(err, "too many concurrent calls");
    fs::write(&fifo, "x").unwrap();
    assert_eq!(c.wait_res(id).unwrap(), json!("x"));
}

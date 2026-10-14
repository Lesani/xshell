#![cfg(unix)]
//! File transfers through a Daemon's `call`: an upload, its read back, and a refused
//! precondition, as a Desktop sees them over the wire.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use xshell_core::transfer::{FILE_CHANGED, MAX_CHUNK};

fn b64(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    // The test client has no base64 crate: a small encoder.
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                let _ = s.write_char(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

#[test]
fn desktop_upload_read_round_trip() {
    let h = TestHome::new();
    let srv = start(&h, |_| {});
    let mut c = Client::connect(&srv.socket);
    c.hello(range(1, 1));
    let dir = h.project("up");
    let d = dir.to_string_lossy().into_owned();
    let body: Vec<u8> = (0..MAX_CHUNK + 5).map(|i| (i % 241) as u8).collect();

    let b = c
        .call(
            "upload_begin",
            json!({ "dir": d, "name": "f.bin", "size": body.len(), "onClash": "fail" }),
        )
        .unwrap();
    let id = b["upload_id"].as_str().unwrap().to_string();
    let mut off = 0usize;
    for piece in body.chunks(MAX_CHUNK as usize) {
        let w = c
            .call(
                "upload_chunk",
                json!({ "uploadId": id, "offset": off, "bytesBase64": b64(piece) }),
            )
            .unwrap();
        off += piece.len();
        assert_eq!(w["written"], json!(off));
    }
    let done = c.call("upload_commit", json!({ "uploadId": id })).unwrap();
    let path = dir.join("f.bin");
    assert_eq!(done["path"], json!(path.to_string_lossy()));
    assert_eq!(fs::read(&path).unwrap(), body);

    let p = path.to_string_lossy().into_owned();
    let st = c
        .call("file_stat", json!({ "path": p, "version": true }))
        .unwrap();
    assert_eq!(st["size"], json!(body.len()));
    assert_eq!(st["version"], done["version"]);
    let r = c
        .call(
            "file_read",
            json!({ "path": p, "offset": MAX_CHUNK, "len": MAX_CHUNK }),
        )
        .unwrap();
    assert_eq!(r["bytes_base64"], json!(b64(&body[MAX_CHUNK as usize..])));
    assert_eq!(r["eof"], Value::Bool(true));

    // A stale precondition: the exact error text, and the session stays for an "Overwrite".
    let b = c
        .call(
            "upload_begin",
            json!({ "dir": d, "name": "f.bin", "size": 2, "onClash": "replace" }),
        )
        .unwrap();
    let id = b["upload_id"].as_str().unwrap().to_string();
    c.call(
        "upload_chunk",
        json!({ "uploadId": id, "offset": 0, "bytesBase64": b64(b"hi") }),
    )
    .unwrap();
    let e = c
        .call(
            "upload_commit",
            json!({ "uploadId": id, "ifVersion": "sha256:00" }),
        )
        .unwrap_err();
    assert_eq!(
        e,
        format!("{FILE_CHANGED}: {}", done["version"].as_str().unwrap())
    );
    c.call(
        "upload_commit",
        json!({ "uploadId": id, "ifVersion": null }),
    )
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"hi");

    // make_dir, and the Daemon's exit drops an unfinished upload with its part file.
    let sub = dir.join("sub");
    c.call("make_dir", json!({ "path": sub })).unwrap();
    assert!(sub.is_dir());
    c.call(
        "upload_begin",
        json!({ "dir": sub, "name": "half", "size": 9, "onClash": "fail" }),
    )
    .unwrap();
    assert_eq!(fs::read_dir(&sub).unwrap().count(), 1);
    srv.shutdown();
    assert_eq!(fs::read_dir(&sub).unwrap().count(), 0);
}

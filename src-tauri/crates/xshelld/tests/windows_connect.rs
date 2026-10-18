#![cfg(windows)]
//! The attach-only Windows SSH bridge, exercised through real binary stdio.
mod win;

use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::process::{ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;
use win::*;
use xshell_core::pipe::{self, PipeListener, PipeStream};
use xshell_protocol::msg::ClientMsg;
use xshell_protocol::{NOT_RUNNING_EXIT, NOT_RUNNING_MESSAGE};

const CLEANUP: Duration = Duration::from_secs(5);
const STALL: Duration = Duration::from_millis(400);

// All hang-sensitive operations, including the test's own readers/writers/accepts, have
// an independent parent watchdog. ConnectProc registers each child before doing any I/O.
fn bounded(name: &str) -> bool {
    in_child(name, Duration::from_secs(90))
}
fn daemon(h: &TestHome) -> Daemon {
    let daemon = Daemon::start(h);
    child_note("daemon.pid", &daemon.pid().to_string());
    daemon
}
fn peer(listener: &PipeListener) -> PipeStream {
    let p = listener.accept().unwrap();
    p.set_read_timeout(Some(CLEANUP)).unwrap();
    p.set_write_timeout(Some(CLEANUP)).unwrap();
    p
}
fn exit(bridge: &mut ConnectProc, code: i32) {
    assert_eq!(bridge.wait_exit(CLEANUP), Some(code));
    let diagnostic = bridge.stderr();
    if code == 0 {
        assert!(diagnostic.is_empty(), "{diagnostic}");
    } else {
        assert!(!diagnostic.is_empty());
    }
}
fn empty_stdout(bridge: &mut ConnectProc) {
    let mut out = Vec::new();
    bridge.take_stdout().read_to_end(&mut out).unwrap();
    assert!(out.is_empty());
}
fn peer_eof(peer: &PipeStream) {
    assert_eq!(peer.read_within(&mut [0; 1], Some(CLEANUP)).unwrap(), 0);
}
fn no_spawn(h: &TestHome) {
    let paths = xshelld::paths::resolve_windows(&h.home(), &h.pipe, None);
    for path in [
        &paths.lock,
        &paths.pid,
        &paths.state,
        &paths.log,
        &paths.server_root,
        &paths.tmp,
    ] {
        assert!(!path.exists(), "connect created {}", path.display());
    }
    assert_eq!(
        pipe::connect(&h.pipe, Instant::now() + CLEANUP, &|| false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn connect_missing_daemon_exits_not_running_without_spawning() {
    if !bounded("connect_missing_daemon_exits_not_running_without_spawning") {
        return;
    }
    for marker in [None, Some("gui-bound"), Some("persistent")] {
        let h = TestHome::new();
        if let Some(marker) = marker {
            let mode = h.home().join(".xshell/daemon/mode");
            fs::create_dir_all(mode.parent().unwrap()).unwrap();
            fs::write(mode, marker).unwrap();
        }
        let mut bridge = ConnectProc::start(&h, None);
        let _held_stdin = bridge.take_stdin();
        assert_eq!(bridge.wait_exit(CLEANUP), Some(NOT_RUNNING_EXIT));
        assert_eq!(
            bridge.stderr().trim(),
            format!("xshelld: {NOT_RUNNING_MESSAGE}")
        );
        empty_stdout(&mut bridge);
        no_spawn(&h);
        if marker.is_none() {
            assert!(!h.home().join(".xshell").exists());
        }
    }
}

#[test]
fn connect_invalid_endpoint_is_not_not_running() {
    if !bounded("connect_invalid_endpoint_is_not_not_running") {
        return;
    }
    for endpoint in [r"C:\not-a-pipe", r"\\other-host\pipe\xshelld"] {
        let h = TestHome::new();
        let mut bridge = ConnectProc::spawn(
            h.cmd()
                .args(["connect", "--socket", endpoint])
                .stdin(Stdio::piped()),
            None,
        );
        let _held_stdin = bridge.take_stdin();
        exit(&mut bridge, 1);
        empty_stdout(&mut bridge);
        no_spawn(&h);
    }
}

#[test]
fn connect_refuses_foreign_pipe_owner() {
    if !bounded("connect_refuses_foreign_pipe_owner") {
        return;
    }
    let h = TestHome::new();
    let sid = pipe::current_user_sid().unwrap().to_string_sid().unwrap();
    let listener = match PipeListener::bind_with_sddl(&h.pipe, &format!("O:BAD:P(A;;GA;;;{sid})")) {
        Ok(listener) => listener,
        Err(e) if std::env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!("cannot create foreign-owner fixture: {e}")
        }
        Err(e) => {
            eprintln!("skipped: this token cannot make Administrators the owner: {e}");
            return;
        }
    };
    let mut bridge = ConnectProc::start(&h, None);
    let _held_stdin = bridge.take_stdin();
    exit(&mut bridge, 1);
    empty_stdout(&mut bridge);
    // No accept thread to strand: accept only after the child has opened and refused it.
    let p = peer(&listener);
    peer_eof(&p);
    assert!(!h.home().join(".xshell").exists());
}

fn payload(salt: u8) -> Vec<u8> {
    let mut bytes: Vec<u8> = (0..(3 << 20))
        .map(|i| ((i * 37 + i / 256) as u8).wrapping_add(salt))
        .collect();
    bytes.extend_from_slice(b"\0\r\n\x1a\xff\xfe");
    bytes.extend_from_slice("é 中文 🙂".as_bytes());
    bytes
}

fn fragmented(w: &mut impl Write, bytes: &[u8]) {
    let mut at = 0;
    let mut count = 0;
    while at < bytes.len() {
        let n = [1, 7, 4093, 65521][count % 4].min(bytes.len() - at);
        w.write_all(&bytes[at..at + n]).unwrap();
        at += n;
        count += 1;
    }
}

#[test]
fn connect_preserves_binary_bytes_both_directions() {
    if !bounded("connect_preserves_binary_bytes_both_directions") {
        return;
    }
    let h = TestHome::new();
    let listener = PipeListener::bind(&h.pipe).unwrap();
    let mut bridge = ConnectProc::start(&h, None);
    let p = peer(&listener);
    let mut input = bridge.take_stdin();
    let mut output = bridge.take_stdout();
    let to_daemon = payload(19);
    let to_stdout = payload(137);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let sent = to_daemon.clone();
    let writer = thread::spawn(move || {
        fragmented(&mut input, &sent);
        done_rx.recv_timeout(T).unwrap();
    });
    let sent = to_stdout.clone();
    let mut peer_writer = p.clone();
    let peer_writer = thread::spawn(move || fragmented(&mut peer_writer, &sent));
    let expected = to_stdout.len();
    let reader = thread::spawn(move || {
        let mut out = vec![0; expected];
        output.read_exact(&mut out).unwrap();
        (output, out)
    });
    let mut got = vec![0; to_daemon.len()];
    (&p).read_exact(&mut got).unwrap();
    assert_eq!(got, to_daemon);
    let (_output, got) = reader.join().unwrap();
    assert_eq!(got, to_stdout);
    peer_writer.join().unwrap();
    // Integrity is established before stdin EOF: full-close does not promise response drain.
    done_tx.send(()).unwrap();
    writer.join().unwrap();
    exit(&mut bridge, 0);
    peer_eof(&p);
}

#[test]
fn connect_streams_output_without_newline() {
    if !bounded("connect_streams_output_without_newline") {
        return;
    }
    let h = TestHome::new();
    let listener = PipeListener::bind(&h.pipe).unwrap();
    let mut bridge = ConnectProc::start(&h, None);
    let p = peer(&listener);
    let _input = bridge.take_stdin();
    let mut output = bridge.take_stdout();
    (&p).write_all(b"\0\xffno newline").unwrap();
    let mut got = [0; 12];
    output.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"\0\xffno newline");
    (&p).write_all(b"ack").unwrap();
    let mut ack = [0; 3];
    output.read_exact(&mut ack).unwrap();
    assert_eq!(&ack, b"ack");
    p.shutdown(Shutdown::Both).unwrap();
    exit(&mut bridge, 0);
}

#[derive(Clone)]
struct SharedInput(Arc<Mutex<Option<ChildStdin>>>);
impl Write for SharedInput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn client(bridge: &mut ConnectProc) -> (Client, SharedInput) {
    let input = SharedInput(Arc::new(Mutex::new(Some(bridge.take_stdin()))));
    (Client::from_io(bridge.take_stdout(), input.clone()), input)
}
fn detach(bridge: &mut ConnectProc, c: &mut Client, input: SharedInput) {
    input.0.lock().unwrap().take();
    exit(bridge, 0);
    c.expect_eof();
}

#[test]
fn connect_handshake_and_rpc_attach_existing_gui_bound_daemon() {
    if !bounded("connect_handshake_and_rpc_attach_existing_gui_bound_daemon") {
        return;
    }
    let h = TestHome::new();
    let d = daemon(&h);
    let mut direct = Client::connect(&h.pipe);
    direct.hello();
    let mut bridge = ConnectProc::start(&h, None);
    let (mut c, input) = client(&mut bridge);
    c.hello();
    let home = c
        .request(&ClientMsg::Call {
            method: "get_home_dir".into(),
            params: serde_json::json!({}),
        })
        .unwrap();
    assert_eq!(home, serde_json::json!(h.home().to_string_lossy()));
    detach(&mut bridge, &mut c, input);
    assert!(alive(d.pid()));
    assert_eq!(connect_pipe(&h.pipe).server_process_id().unwrap(), d.pid());
    direct
        .request(&ClientMsg::Call {
            method: "get_home_dir".into(),
            params: serde_json::json!({}),
        })
        .unwrap();
}

#[test]
fn connect_stdin_eof_closes_both_directions() {
    if !bounded("connect_stdin_eof_closes_both_directions") {
        return;
    }
    let h = TestHome::new();
    let listener = PipeListener::bind(&h.pipe).unwrap();
    let mut bridge = ConnectProc::start(&h, None);
    let p = peer(&listener);
    let mut input = bridge.take_stdin();
    let mut output = bridge.take_stdout();
    input.write_all(b"sync").unwrap();
    let mut got = [0; 4];
    (&p).read_exact(&mut got).unwrap();
    assert_eq!(&got, b"sync");
    (&p).write_all(b"sync").unwrap();
    output.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"sync");
    drop(input);
    exit(&mut bridge, 0);
    peer_eof(&p);
    assert_eq!(output.read(&mut [0; 1]).unwrap(), 0);
}

#[test]
fn connect_null_stdin_exits_without_waiting_for_daemon() {
    if !bounded("connect_null_stdin_exits_without_waiting_for_daemon") {
        return;
    }
    let h = TestHome::new();
    let listener = PipeListener::bind(&h.pipe).unwrap();
    let mut bridge = ConnectProc::spawn(h.cmd().arg("connect").stdin(Stdio::null()), None);
    let p = peer(&listener);
    exit(&mut bridge, 0);
    peer_eof(&p);
    empty_stdout(&mut bridge);
}

#[test]
fn connect_daemon_close_cancels_blocked_stdin() {
    if !bounded("connect_daemon_close_cancels_blocked_stdin") {
        return;
    }
    let h = TestHome::new();
    let listener = PipeListener::bind(&h.pipe).unwrap();
    let mut bridge = ConnectProc::start(&h, None);
    let p = peer(&listener);
    let _input = bridge.take_stdin();
    p.shutdown(Shutdown::Both).unwrap();
    exit(&mut bridge, 0);
}

#[test]
fn connect_broken_stdout_cancels_other_direction() {
    if !bounded("connect_broken_stdout_cancels_other_direction") {
        return;
    }
    let h = TestHome::new();
    let listener = PipeListener::bind(&h.pipe).unwrap();
    let mut bridge = ConnectProc::start(&h, None);
    let p = peer(&listener);
    let _input = bridge.take_stdin();
    drop(bridge.take_stdout());
    (&p).write_all(b"broken stdout").unwrap();
    exit(&mut bridge, 1);
    peer_eof(&p);
}

#[test]
fn connect_blocked_stdout_times_out_without_ending_daemon() {
    if !bounded("connect_blocked_stdout_times_out_without_ending_daemon") {
        return;
    }
    let h = TestHome::new();
    let d = daemon(&h);
    let mut direct = Client::connect(&h.pipe);
    direct.hello();
    let mut bridge = ConnectProc::start(&h, Some(STALL));
    let (mut c, input) = client(&mut bridge);
    c.hello();
    let terminal = Uuid::new_v4();
    let pid = c.open(terminal, cmd_spec(&h.home()));
    c.attach(terminal);
    c.marker(terminal, 31);
    // Replace the consuming protocol reader with a second connection whose stdout is
    // deliberately held unread. The first bridge must be reaped and its reader see EOF.
    detach(&mut bridge, &mut c, input);
    let mut blocked = ConnectProc::start(&h, Some(STALL));
    let mut input = blocked.take_stdin();
    let _unread = blocked.take_stdout();
    let hello = ClientMsg::Hello(xshell_protocol::msg::Hello {
        protocol: xshell_protocol::msg::ProtocolRange { min: 1, max: 1 },
        version: "test".into(),
        capabilities: vec!["term".into()],
        build: None,
    });
    input
        .write_all(&xshell_protocol::msg::encode_msg(&hello, None).unwrap())
        .unwrap();
    input
        .write_all(
            &xshell_protocol::msg::encode_msg(&ClientMsg::TermAttach { terminal }, Some(1))
                .unwrap(),
        )
        .unwrap();
    // Native cmd output only; no external process adoption, PowerShell or test agent.
    direct.input(terminal, "for /L %i in (1,1,20000) do @echo 012345678901234567890123456789012345678901234567890123456789\r");
    assert_eq!(blocked.wait_exit(CLEANUP), Some(1));
    assert!(!blocked.stderr().is_empty());
    assert!(alive(d.pid()));
    assert!(alive(pid));
    let mut reconnect = Client::connect(&h.pipe);
    let (_, list) = reconnect.hello();
    assert!(list
        .iter()
        .any(|t| t.terminal == terminal && t.pid == Some(pid)));
    reconnect.close(terminal);
    assert!(wait_dead(pid, T));
}

#[test]
fn connect_stalled_pipe_write_times_out() {
    if !bounded("connect_stalled_pipe_write_times_out") {
        return;
    }
    let h = TestHome::new();
    let listener = PipeListener::bind(&h.pipe).unwrap();
    let mut bridge = ConnectProc::start(&h, Some(STALL));
    let _p = peer(&listener);
    let mut input = bridge.take_stdin();
    let mut output = bridge.take_stdout();
    let writer = thread::spawn(move || input.write_all(&vec![7; 8 << 20]));
    // Keep stdout drained; only the Daemon pipe write may stall in this fixture.
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        output.read_to_end(&mut out).unwrap();
        out
    });
    exit(&mut bridge, 1);
    assert!(writer.join().unwrap().is_err());
    assert!(reader.join().unwrap().is_empty());
}

#[test]
fn connect_disconnect_and_reconnect_preserves_terminal_pid() {
    if !bounded("connect_disconnect_and_reconnect_preserves_terminal_pid") {
        return;
    }
    let h = TestHome::new();
    let d = daemon(&h);
    let mut direct = Client::connect(&h.pipe);
    direct.hello();
    let mut a = ConnectProc::start(&h, None);
    let (mut ca, ia) = client(&mut a);
    ca.hello();
    let terminal = Uuid::new_v4();
    let pid = ca.open(terminal, cmd_spec(&h.home()));
    ca.attach(terminal);
    ca.marker(terminal, 41);
    detach(&mut a, &mut ca, ia);
    assert!(alive(pid));
    assert!(alive(d.pid()));
    let mut b = ConnectProc::start(&h, None);
    let (mut cb, ib) = client(&mut b);
    let (_, list) = cb.hello();
    assert!(list
        .iter()
        .any(|t| t.terminal == terminal && t.pid == Some(pid)));
    let list = direct.terminals_where(|list| {
        list.iter()
            .any(|t| t.terminal == terminal && t.pid == Some(pid))
    });
    assert!(list.iter().any(|t| t.terminal == terminal));
    assert_eq!(connect_pipe(&h.pipe).server_process_id().unwrap(), d.pid());
    cb.attach(terminal);
    cb.marker(terminal, 43);
    cb.close(terminal);
    assert!(wait_dead(pid, T));
    detach(&mut b, &mut cb, ib);
}

#[test]
fn connect_gui_bound_shutdown_exits_with_stdin_open() {
    if !bounded("connect_gui_bound_shutdown_exits_with_stdin_open") {
        return;
    }
    let h = TestHome::new();
    let mut d = daemon(&h);
    let mut direct = Client::connect(&h.pipe);
    direct.hello();
    let mut bridge = ConnectProc::start(&h, None);
    let (mut c, _input) = client(&mut bridge);
    c.hello();
    let terminal = Uuid::new_v4();
    let pid = c.open(terminal, cmd_spec(&h.home()));
    c.attach(terminal);
    c.marker(terminal, 47);
    d.stop();
    assert_eq!(d.wait_exit(T), Some(0));
    exit(&mut bridge, 0);
    c.expect_eof();
    assert!(wait_dead(pid, T));
}

#![cfg(windows)]
//! Chat View replies (`term.submit`) through a real ConPTY (Lesani/xshell#40): the probes
//! that measure what a Windows Host gives the Daemon, before it offers replies.
//!
//! The facts they measure, logged as `FACT <tag> build=<n> …` lines (and appended to
//! `$XSHELL_PROBE_FACTS`):
//! - P1: whether the agent's `CSI ? 2004 h` reaches the Daemon's output, for a reader of VT
//!   input, a reader of console records and a Node raw-mode reader;
//! - P2: what bytes a VT reader gets for a reply, and where its reads split them;
//! - P3: whether `CSI ? 2004 l` propagates;
//! - P4: whether a reply of `SUBMIT_MAX_BYTES` arrives whole;
//! - P5: the `PIPE_NOWAIT` semantics (the xshelld unit test `pipe_nowait_write_semantics`).
//!
//! The Daemons here take replies through a debug-build test hook (`XSHELLD_TEST_SUBMIT=1`)
//! that does not offer `term.submit`: a Windows Host does not offer it yet.
//!
//! The fake agent is this test binary run as [`helper_console_agent`] (`claude.cmd` and
//! `codex.cmd` on the Daemon's PATH): it opens the console itself, sets its input mode as a
//! real agent's runtime would, draws the `xshell-core` prompt fixtures, and logs what it
//! reads to its working directory.

mod win;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;
use win::*;
use xshell_core::agent_status::HookAgent;
use xshell_core::launch::LaunchSpec;
use xshell_core::prompt::{composer, composer_images, extract, ScreenModel};
use xshell_protocol::msg::{
    AgentStatus, ClientMsg, SUBMIT_MAX_BYTES, SUBMIT_NEEDS_YOU, SUBMIT_NOT_READY,
    SUBMIT_NO_VT_INPUT, SUBMIT_STUCK, SUBMIT_UNCONFIRMED, SUBMIT_UNSUPPORTED,
};

const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The `xshell-core` prompt fixtures.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/prompts")
}

fn fixture(name: &str) -> PathBuf {
    let p = fixtures().join(format!("{name}.raw"));
    assert!(p.exists(), "{}", p.display());
    p
}

fn fixture_meta(name: &str) -> Value {
    serde_json::from_slice(&fs::read(fixtures().join(format!("{name}.json"))).unwrap()).unwrap()
}

/// The bytes a reply of `text` is typed as.
fn typed(text: &str) -> Vec<u8> {
    [START, text.as_bytes(), END, b"\r"].concat()
}

// ── The console fake agent ────────────────────────────────────────────────

/// Not a test unless `XSHELLD_WIN_FAKE` is set (`vt`, `vtw`, `vtw-records` or `records`):
/// then it is a fake agent on its console (see [`console`]).
#[test]
fn helper_console_agent() {
    let Ok(mode) = std::env::var("XSHELLD_WIN_FAKE") else {
        return;
    };
    console::run(&mode);
}

/// The input stream as an agent with bracketed paste reads it: pastes, typed keys, and an
/// Enter outside a paste submits what was typed.
#[derive(Default)]
struct Parse {
    buf: Vec<u8>,
    pos: usize,
    in_paste: bool,
    draft: Vec<u8>,
    submits: Vec<String>,
    starts: usize,
    ends: usize,
}

impl Parse {
    fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        loop {
            let rest = &self.buf[self.pos..];
            if rest.is_empty() {
                return;
            }
            if rest.starts_with(START) {
                (self.in_paste, self.pos, self.starts) = (true, self.pos + 6, self.starts + 1);
                continue;
            }
            if rest.starts_with(END) {
                (self.in_paste, self.pos, self.ends) = (false, self.pos + 6, self.ends + 1);
                continue;
            }
            if START.starts_with(rest) || END.starts_with(rest) {
                return; // the rest of a marker is still to come
            }
            self.pos += 1;
            match rest[0] {
                b'\r' if !self.in_paste => {
                    let s = String::from_utf8_lossy(&std::mem::take(&mut self.draft)).into_owned();
                    self.submits.push(s);
                }
                b => self.draft.push(b),
            }
        }
    }
}

/// The fake agent. In its working directory it writes `modes.json` (the console modes it
/// set, then `started`), `terminal.id`, and what it reads: `input.bin` and `reads.log` (one
/// line per read, its length) for a VT reader, `records.jsonl` (key events) and
/// `events.jsonl` (other records) for a records reader, `submits.jsonl` (each Enter outside
/// a paste: what it submits) and `sizes.log` (each change of its window). It runs the control
/// files `ctl.<n>` in order and acknowledges each with `ack.<n>`:
/// - `show <path>`: clear the screen and draw the fixture at `<path>`;
/// - `paste h|l`: bracketed paste on or off (`CSI ? 2004 h|l`);
/// - `stall <ms>`: the next read is followed by a pause of `<ms>`;
/// - `on-end <path>`: draw `<path>` as soon as the end of a paste is read;
/// - `on-end-mode vt|records`: set the input mode as soon as the end of a paste is read;
/// - `mode vt|records`: set the input mode now (`ENABLE_VIRTUAL_TERMINAL_INPUT` on or off);
/// - `flood <ms>`: set the window title, two long ones in turn, for `<ms>`; `flood.state`
///   says how long the write in progress has been blocked and the longest one was
///   (`<blocked ms> <longest ms> <writes>`, every 20 ms);
/// - `ctrl-break`: Ctrl+Break to every process on the console (the fake ignores it).
///
/// Readers: `vt` (`ReadFile`), `vtw` (`ReadConsoleW`) and `vtw-records` (`ReadConsoleW`,
/// but without VT input until `mode vt`) take VT input; `records` reads key events.
mod console {
    use super::Parse;
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::*;

    /// A console handle, shared by the fake's threads.
    #[derive(Clone, Copy)]
    struct H(usize);

    impl H {
        fn raw(self) -> HANDLE {
            self.0 as HANDLE
        }
    }

    fn open(name: &str) -> H {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(!h.is_null() && h as isize != -1, "open {name}");
        H(h as usize)
    }

    fn mode(h: H) -> u32 {
        let mut m = 0;
        unsafe { GetConsoleMode(h.raw(), &mut m) };
        m
    }

    fn out(h: H, bytes: &[u8]) {
        let mut done = 0;
        while done < bytes.len() {
            let mut n = 0u32;
            let ok = unsafe {
                WriteFile(
                    h.raw(),
                    bytes[done..].as_ptr(),
                    (bytes.len() - done) as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                std::process::exit(0);
            }
            done += n as usize;
        }
    }

    fn append(dir: &Path, name: &str, bytes: &[u8]) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(name))
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    fn show(h: H, path: &Path) {
        out(h, b"\x1b[H\x1b[2J");
        out(h, &fs::read(path).unwrap());
    }

    struct Shared {
        dir: PathBuf,
        conin: H,
        conout: H,
        stall: AtomicU64,
        on_end: Mutex<Option<PathBuf>>,
        on_end_mode: Mutex<Option<bool>>,
    }

    unsafe extern "system" fn ignore_ctrl(kind: u32) -> windows_sys::core::BOOL {
        (kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT) as windows_sys::core::BOOL
    }

    /// Set (`vt`) or clear the input's VT bit; the mode it has then.
    fn set_vt(conin: H, vt: bool) -> u32 {
        let m = mode(conin);
        let want = if vt {
            m | ENABLE_VIRTUAL_TERMINAL_INPUT
        } else {
            m & !ENABLE_VIRTUAL_TERMINAL_INPUT
        };
        unsafe { SetConsoleMode(conin.raw(), want) };
        mode(conin)
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    pub fn run(mode_name: &str) -> ! {
        let dir = std::env::current_dir().unwrap();
        let (conin, conout) = (open("CONIN$"), open("CONOUT$"));
        unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl), 1) };
        let vt = matches!(mode_name, "vt" | "vtw");
        unsafe {
            SetConsoleOutputCP(65001);
            SetConsoleMode(
                conout.raw(),
                mode(conout) | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            );
            let raw =
                mode(conin) & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            let want = if vt {
                raw | ENABLE_VIRTUAL_TERMINAL_INPUT
            } else {
                (raw & !ENABLE_VIRTUAL_TERMINAL_INPUT) | ENABLE_WINDOW_INPUT
            };
            SetConsoleMode(conin.raw(), want);
            if vt {
                SetConsoleCP(65001);
            }
        }
        let got = mode(conin);
        let modes = json!({
            "pid": std::process::id(),
            "reader": mode_name,
            "input": got,
            "output": mode(conout),
            "vtInput": got & ENABLE_VIRTUAL_TERMINAL_INPUT != 0,
            "lineInput": got & ENABLE_LINE_INPUT != 0,
            "processedInput": got & ENABLE_PROCESSED_INPUT != 0,
            "inputCodePage": unsafe { GetConsoleCP() },
            "outputCodePage": unsafe { GetConsoleOutputCP() },
        });
        fs::write(dir.join("modes.json"), modes.to_string()).unwrap();
        fs::write(
            dir.join("terminal.id"),
            std::env::var("XSHELL_TERMINAL_ID").unwrap_or_default(),
        )
        .unwrap();
        let sh = Arc::new(Shared {
            dir: dir.clone(),
            conin,
            conout,
            stall: AtomicU64::new(0),
            on_end: Mutex::new(None),
            on_end_mode: Mutex::new(None),
        });
        {
            let sh = sh.clone();
            match mode_name {
                "vt" => std::thread::spawn(move || read_vt(conin, &sh)),
                "vtw" | "vtw-records" => std::thread::spawn(move || read_vtw(conin, &sh)),
                _ => std::thread::spawn(move || read_records(conin, &sh)),
            };
        }
        {
            let sh = sh.clone();
            std::thread::spawn(move || watch_size(&sh));
        }
        fs::write(dir.join("started"), "").unwrap();
        let mut n = 0;
        loop {
            let ctl = dir.join(format!("ctl.{n}"));
            let Ok(line) = fs::read_to_string(&ctl) else {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            };
            let line = line.trim();
            let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
            match cmd {
                "show" => show(conout, Path::new(arg)),
                "paste" => out(conout, format!("\x1b[?2004{arg}").as_bytes()),
                "stall" => sh.stall.store(arg.parse().unwrap(), Ordering::SeqCst),
                "on-end" => *sh.on_end.lock().unwrap() = Some(PathBuf::from(arg)),
                "on-end-mode" => *sh.on_end_mode.lock().unwrap() = Some(arg == "vt"),
                "mode" => {
                    let m = set_vt(conin, arg == "vt");
                    append(&dir, "modes.log", format!("{m}\n").as_bytes());
                }
                "ctrl-break" => {
                    let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) };
                    append(&dir, "ctrl.log", format!("sent {ok}\n").as_bytes());
                }
                "flood" => {
                    // Output that never changes the screen: window titles, in turn.
                    let until = Instant::now() + Duration::from_millis(arg.parse().unwrap());
                    // When the write in progress started (Unix ms, 0: none), the longest
                    // write, and how many were made.
                    let since = Arc::new(AtomicU64::new(0));
                    let longest = Arc::new(AtomicU64::new(0));
                    let writes = Arc::new(AtomicU64::new(0));
                    let (s2, l2, w2) = (since.clone(), longest.clone(), writes.clone());
                    std::thread::spawn(move || {
                        let title = |c: char| format!("\x1b]0;{}\x07", c.to_string().repeat(400));
                        let (a, b) = (title('a'), title('b'));
                        while Instant::now() < until {
                            for t in [&a, &b] {
                                let t0 = now_ms();
                                s2.store(t0, Ordering::SeqCst);
                                out(conout, t.as_bytes());
                                s2.store(0, Ordering::SeqCst);
                                l2.fetch_max(now_ms() - t0, Ordering::SeqCst);
                                w2.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    });
                    let dir = dir.clone();
                    std::thread::spawn(move || loop {
                        let t0 = since.load(Ordering::SeqCst);
                        let blocked = if t0 == 0 {
                            0
                        } else {
                            now_ms().saturating_sub(t0)
                        };
                        let line = format!(
                            "{blocked} {} {}",
                            longest.load(Ordering::SeqCst),
                            writes.load(Ordering::SeqCst)
                        );
                        let tmp = dir.join(".flood.state");
                        if fs::write(&tmp, line).is_ok() {
                            let _ = fs::rename(&tmp, dir.join("flood.state"));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    });
                }
                _ => panic!("unknown control {line:?}"),
            }
            fs::write(dir.join(format!("ack.{n}")), "").unwrap();
            n += 1;
        }
    }

    fn after_read(sh: &Shared, parse: &mut Parse, ends_before: usize) {
        if parse.ends > ends_before {
            if let Some(vt) = sh.on_end_mode.lock().unwrap().take() {
                let m = set_vt(sh.conin, vt);
                append(&sh.dir, "on-end.log", format!("mode {m}\n").as_bytes());
            }
            if let Some(p) = sh.on_end.lock().unwrap().take() {
                show(sh.conout, &p);
                append(&sh.dir, "on-end.log", b"shown\n");
            }
        }
        for s in parse.submits.drain(..) {
            append(
                &sh.dir,
                "submits.jsonl",
                format!("{}\n", json!(s)).as_bytes(),
            );
        }
        let stall = sh.stall.swap(0, Ordering::SeqCst);
        if stall > 0 {
            std::thread::sleep(Duration::from_millis(stall));
        }
    }

    fn read_vt(conin: H, sh: &Shared) {
        let mut parse = Parse::default();
        let mut buf = vec![0u8; 4096];
        loop {
            let mut n = 0u32;
            let ok = unsafe {
                ReadFile(
                    conin.raw(),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                std::process::exit(0);
            }
            let got = &buf[..n as usize];
            append(&sh.dir, "input.bin", got);
            append(&sh.dir, "reads.log", format!("{n}\n").as_bytes());
            let ends = parse.ends;
            parse.feed(got);
            after_read(sh, &mut parse, ends);
        }
    }

    /// A VT reader as the W console API reads it (UTF-16, a character never cut by a code
    /// page): what it reads is logged as UTF-8.
    fn read_vtw(conin: H, sh: &Shared) {
        let mut parse = Parse::default();
        let mut buf = vec![0u16; 2048];
        let mut high: Option<u16> = None;
        loop {
            let mut n = 0u32;
            let ok = unsafe {
                ReadConsoleW(
                    conin.raw(),
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut n,
                    std::ptr::null(),
                )
            };
            if ok == 0 {
                std::process::exit(0);
            }
            let mut units: Vec<u16> = high.take().into_iter().collect();
            units.extend_from_slice(&buf[..n as usize]);
            if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
                high = units.pop();
            }
            let got = String::from_utf16_lossy(&units).into_bytes();
            append(&sh.dir, "input.bin", &got);
            append(&sh.dir, "reads.log", format!("{}\n", got.len()).as_bytes());
            let ends = parse.ends;
            parse.feed(&got);
            after_read(sh, &mut parse, ends);
        }
    }

    fn read_records(conin: H, sh: &Shared) {
        let mut parse = Parse::default();
        let mut recs = vec![INPUT_RECORD::default(); 64];
        loop {
            let mut n = 0u32;
            if unsafe { ReadConsoleInputW(conin.raw(), recs.as_mut_ptr(), 64, &mut n) } == 0 {
                std::process::exit(0);
            }
            let ends = parse.ends;
            for r in &recs[..n as usize] {
                if u32::from(r.EventType) == KEY_EVENT {
                    let k = unsafe { r.Event.KeyEvent };
                    let ch = unsafe { k.uChar.UnicodeChar };
                    let line = json!({
                        "down": k.bKeyDown != 0,
                        "vk": k.wVirtualKeyCode,
                        "sc": k.wVirtualScanCode,
                        "ch": ch,
                        "state": k.dwControlKeyState,
                        "repeat": k.wRepeatCount,
                    });
                    append(&sh.dir, "records.jsonl", format!("{line}\n").as_bytes());
                    if k.bKeyDown != 0 && ch != 0 {
                        let s = String::from_utf16_lossy(&[ch]);
                        parse.feed(s.as_bytes());
                    }
                } else {
                    let line = json!({ "type": r.EventType });
                    append(&sh.dir, "events.jsonl", format!("{line}\n").as_bytes());
                }
            }
            after_read(sh, &mut parse, ends);
        }
    }

    fn watch_size(sh: &Shared) {
        let size = || {
            let mut i = CONSOLE_SCREEN_BUFFER_INFO::default();
            unsafe { GetConsoleScreenBufferInfo(sh.conout.raw(), &mut i) };
            let w = i.srWindow;
            (w.Right - w.Left + 1, w.Bottom - w.Top + 1)
        };
        let now_file = |s: (i16, i16)| {
            let tmp = sh.dir.join(".size");
            fs::write(&tmp, format!("{} {}", s.0, s.1)).unwrap();
            fs::rename(&tmp, sh.dir.join("size")).unwrap();
        };
        let mut last = size();
        now_file(last);
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let now = size();
            if now != last {
                now_file(now);
                append(
                    &sh.dir,
                    "sizes.log",
                    format!("{} {}\n", now.0, now.1).as_bytes(),
                );
                last = now;
            }
        }
    }
}

// ── The harness ───────────────────────────────────────────────────────────

/// One fake agent Terminal: its working directory, where the fake logs.
struct Fake {
    t: Uuid,
    /// The Terminal's process: the launcher (`xshelld job-exec`) the fake runs under.
    pid: u32,
    dir: PathBuf,
    /// The fake's window when it started, and how long it took to reach the Terminal's size
    /// (`None`: it never did, within the wait).
    startup: (String, Option<Duration>),
}

impl Fake {
    fn file(&self, name: &str) -> Vec<u8> {
        fs::read(self.dir.join(name)).unwrap_or_default()
    }

    fn input(&self) -> Vec<u8> {
        self.file("input.bin")
    }

    /// Where the fake's reads of its input started.
    fn read_starts(&self) -> Vec<usize> {
        let mut at = 0;
        String::from_utf8(self.file("reads.log"))
            .unwrap()
            .lines()
            .map(|l| {
                let start = at;
                at += l.parse::<usize>().unwrap();
                start
            })
            .collect()
    }

    fn lines(&self, name: &str) -> Vec<Value> {
        String::from_utf8(self.file(name))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn submits(&self) -> Vec<String> {
        self.lines("submits.jsonl")
            .into_iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    /// The fake's window now, `cols rows`.
    fn size(&self) -> String {
        String::from_utf8(self.file("size")).unwrap()
    }

    fn modes(&self) -> Value {
        serde_json::from_slice(&self.file("modes.json")).unwrap()
    }

    /// Run control `line` in the fake and wait until it has.
    fn run(&self, line: &str) {
        let n = (0..)
            .find(|n| !self.dir.join(format!("ctl.{n}")).exists())
            .unwrap();
        let tmp = self.dir.join(format!(".ctl.{n}"));
        fs::write(&tmp, line).unwrap();
        fs::rename(&tmp, self.dir.join(format!("ctl.{n}"))).unwrap();
        assert!(
            wait_until(T, || self.dir.join(format!("ack.{n}")).exists()),
            "{line:?} not run"
        );
    }

    /// Wait until the fake read at least `n` bytes, then a while for more.
    fn wait_input(&self, n: usize) -> Vec<u8> {
        wait_until(T, || self.input().len() >= n);
        std::thread::sleep(ms(500));
        self.input()
    }

    fn wait_submits(&self, n: usize) -> Vec<String> {
        wait_until(T, || self.submits().len() >= n);
        std::thread::sleep(ms(300));
        self.submits()
    }
}

/// A Daemon that takes replies (the test hook) with the console fake as `claude` and
/// `codex`, reading as `reader` (`vt` or `records`), and a Desktop connection to it.
struct Probe {
    h: TestHome,
    d: Daemon,
    c: Client,
    n: usize,
    /// The Daemon's trace of readings and writes (`XSHELLD_TEST_SUBMIT_TRACE`).
    trace: PathBuf,
    /// While this file exists the Daemon does not read Terminal output
    /// (`XSHELLD_TEST_DRAIN_PAUSE`).
    pause: PathBuf,
}

impl Probe {
    fn new(reader: &str, env: &[(&str, &str)]) -> Probe {
        let h = TestHome::new();
        let exe = std::env::current_exe().unwrap();
        for agent in ["claude", "codex"] {
            h.script(
                agent,
                &format!(
                    "set XSHELLD_WIN_FAKE={reader}\n\"{}\" --exact helper_console_agent --nocapture --test-threads=1 >NUL 2>&1",
                    exe.display()
                ),
            );
        }
        Self::with(h, env)
    }

    fn with(h: TestHome, env: &[(&str, &str)]) -> Probe {
        let trace = h.dir.path().join("trace.log");
        let pause = h.dir.path().join("pause");
        let (ts, ps) = (
            trace.to_string_lossy().into_owned(),
            pause.to_string_lossy().into_owned(),
        );
        let mut all = vec![
            ("XSHELLD_TEST_SUBMIT", "1"),
            ("XSHELLD_TEST_SUBMIT_TRACE", ts.as_str()),
            ("XSHELLD_TEST_DRAIN_PAUSE", ps.as_str()),
        ];
        all.extend_from_slice(env);
        let d = Daemon::start_with(&h, &all);
        let mut c = Client::connect(&h.pipe);
        let (hello, _) = c.hello();
        // The probes' replies come through the test hook, never through an offer.
        assert!(
            !hello
                .capabilities
                .iter()
                .any(|c| c.starts_with("term.submit")),
            "{:?}",
            hello.capabilities
        );
        Probe {
            h,
            d,
            c,
            n: 0,
            trace,
            pause,
        }
    }

    /// The trace's lines, each without its time stamp.
    fn trace(&self) -> Vec<String> {
        fs::read_to_string(&self.trace)
            .unwrap_or_default()
            .lines()
            .map(|l| l.split_once(' ').map_or(l, |(_, r)| r).to_string())
            .collect()
    }

    /// The trace's readings of the input mode.
    fn readings(&self) -> Vec<Reading> {
        self.trace()
            .iter()
            .filter_map(|l| Reading::parse(l))
            .collect()
    }

    /// The Daemon's pid and pause file, for [`in_child`]'s parent.
    fn note_for_parent(&self) {
        child_note("daemon.pid", &self.d.pid().to_string());
        child_note("pause.path", &self.pause.to_string_lossy());
    }

    /// Open `agent` (direct, as the Desktop does) at `cols` x `rows` and wait for the fake.
    fn open(&mut self, agent: &str, cols: u16, rows: u16) -> Fake {
        self.n += 1;
        let dir = self.h.project(&format!("p{}", self.n));
        let spec = LaunchSpec {
            agent: (agent != "claude").then(|| agent.to_string()),
            ..claude_spec(&dir)
        };
        let t = Uuid::new_v4();
        let pid = self.c.open_sized(t, spec, cols, rows);
        self.c.attach(t);
        assert!(
            wait_until(T, || dir.join("started").exists()),
            "the fake agent did not start; output {:?}",
            String::from_utf8_lossy(self.c.output(t))
        );
        let mut f = Fake {
            t,
            pid,
            dir,
            startup: (String::new(), None),
        };
        // Measured: the console starts a row short (ConPTY says `CSI 8;rows-1;cols t`) and
        // gets the Terminal's size a moment later. A fixture drawn for the full size must
        // wait for it.
        f.startup.0 = f.size();
        let t0 = Instant::now();
        let want = format!("{cols} {rows}");
        f.startup.1 =
            wait_until(Duration::from_secs(10), || f.size() == want).then(|| t0.elapsed());
        // ConPTY renders the new size a frame or more after the console has it.
        self.c.settle(t, ms(500));
        f
    }

    /// Show fixture `name` (bracketed paste `h`, `l` or unchanged) and wait until the
    /// Daemon's output of it settles. The output since the show.
    fn show(&mut self, f: &Fake, name: &str, paste: Option<char>) -> Vec<u8> {
        let mark = self.c.output(f.t).len();
        f.run(&format!("show {}", fixture(name).display()));
        if let Some(m) = paste {
            f.run(&format!("paste {m}"));
        }
        self.c.settle(f.t, ms(700));
        self.c.output(f.t)[mark..].to_vec()
    }

    /// A fake showing its composer (bracketed paste on) that the Daemon takes replies for.
    fn ready(&mut self, agent: &str) -> Fake {
        let f = self.open(agent, 100, 30);
        let name = format!("{agent}-composer-idle");
        self.show(&f, &name, Some('h'));
        f
    }

    fn submit(&mut self, f: &Fake, text: &str) -> Result<Value, String> {
        self.submit_files(f, text, &[])
    }

    fn submit_files(&mut self, f: &Fake, text: &str, files: &[&str]) -> Result<Value, String> {
        self.c.request(&ClientMsg::TermSubmit {
            terminal: f.t,
            text: text.into(),
            files: files.iter().map(|s| s.to_string()).collect(),
        })
    }
}

/// One reading of the input mode in the Daemon's trace (`mode step=… leader=… raw=… vt=…
/// procs=… ms=…`, or `… error=… ms=…`).
#[derive(Debug, Clone)]
struct Reading {
    step: String,
    leader: Option<u32>,
    raw: Option<u32>,
    vt: Option<bool>,
    procs: Option<u32>,
    error: Option<String>,
    ms: u64,
}

impl Reading {
    fn parse(line: &str) -> Option<Reading> {
        let rest = line.strip_prefix("mode ")?;
        let field = |k: &str| {
            rest.split(' ')
                .find_map(|w| w.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
        };
        let (head, ms) = rest.rsplit_once(" ms=")?;
        Some(Reading {
            step: field("step")?.to_string(),
            leader: field("leader").and_then(|v| v.parse().ok()),
            raw: field("raw")
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()),
            vt: field("vt").map(|v| v == "1"),
            procs: field("procs").and_then(|v| v.parse().ok()),
            error: head.split_once(" error=").map(|(_, e)| e.to_string()),
            ms: ms.parse().ok()?,
        })
    }
}

impl Fake {
    /// The flood's state: how long the write in progress has been blocked, the longest
    /// write, and how many there were (ms, ms, count).
    fn flood(&self) -> (u64, u64, u64) {
        let s = String::from_utf8(self.file("flood.state")).unwrap_or_default();
        let v: Vec<u64> = s.split(' ').filter_map(|w| w.parse().ok()).collect();
        match v[..] {
            [a, b, c] => (a, b, c),
            _ => (0, 0, 0),
        }
    }
}

/// `p50` and `max` of `v`.
fn spread(mut v: Vec<u64>) -> (u64, u64) {
    v.sort_unstable();
    (
        v.get(v.len() / 2).copied().unwrap_or(0),
        v.last().copied().unwrap_or(0),
    )
}

/// Whether `out` turns bracketed paste on (`h`) or off (`l`).
fn shows_mode(out: &[u8], m: char) -> bool {
    find(out, format!("\x1b[?2004{m}").as_bytes()).is_some()
}

// ── W1: where the probes run ──────────────────────────────────────────────

#[test]
fn windows_build_reported() {
    let node = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    fact(
        "W1",
        &format!(
            "node={node:?} image={} imageVersion={} conpty=inbox",
            env("ImageOS"),
            env("ImageVersion")
        ),
    );
    assert!(
        windows_build() > 17763,
        "ConPTY needs Windows 10 1809 or later"
    );
}

// ── P1: does the agent's bracketed paste mode reach the Daemon? ───────────

/// P1-i, and a reply delivered (A1): a VT reader's `CSI ? 2004 h` reaches the output, and a
/// reply to it is typed and submitted.
#[test]
fn vt_reader_sees_bracketed_paste_mode() {
    let mut p = Probe::new("vt", &[]);
    let f = p.open("claude", 100, 30);
    let before = p.c.output(f.t).to_vec();
    let out = p.show(&f, "claude-composer-idle", Some('h'));
    let visible = shows_mode(&out, 'h');
    fact(
        "P1",
        &format!(
            "reader=vt visible={visible} beforeShow={} modes={}",
            shows_mode(&before, 'h'),
            f.modes()
        ),
    );
    assert_eq!(f.modes()["vtInput"], true, "{}", f.modes());
    let r = p.submit(&f, "hello from a probe");
    fact("P1", &format!("reader=vt reply={r:?}"));
    assert!(visible, "{}", esc(&out));
    assert_eq!(r, Ok(Value::Null));
    assert_eq!(f.wait_submits(1), ["hello from a probe"]);
}

/// The key events a records reader logged, down strokes only, as `vk:char`.
fn keys_down(f: &Fake) -> Vec<String> {
    f.lines("records.jsonl")
        .iter()
        .filter(|k| k["down"] == true)
        .map(|k| {
            let ch = k["ch"].as_u64().unwrap() as u32;
            let c = char::from_u32(ch).map_or(format!("U+{ch:04X}"), |c| format!("{c:?}"));
            format!("{}:{c}", k["vk"])
        })
        .collect()
}

/// P1-ii, measured against the plan's hypothesis: inbox ConPTY passes a records reader's
/// `CSI ? 2004 h` through too (builds 20348 and 26100), so bracketed paste mode does not
/// tell the Daemon that the agent reads VT input. Such an agent gets the reply as key
/// events, the paste markers dropped, and Enter. The keys of a multi-line reply are a fact
/// (how its newline arrives decides whether it submits early).
#[test]
fn records_reader_sees_bracketed_paste_mode_too() {
    // Measured without the input-mode gate, which refuses this reader (M2).
    let mut p = Probe::new("records", &[("XSHELLD_TEST_CONSOLE_MODE", "off")]);
    let f = p.open("claude", 100, 30);
    let out = p.show(&f, "claude-composer-idle", Some('h'));
    let visible = shows_mode(&out, 'h');
    fact(
        "P1",
        &format!("reader=records visible={visible} modes={}", f.modes()),
    );
    assert_eq!(f.modes()["vtInput"], false, "{}", f.modes());
    let r = p.submit(&f, "one line");
    wait_until(T, || keys_down(&f).len() > "one line".len());
    std::thread::sleep(ms(500));
    let one = keys_down(&f);
    fact("P1", &format!("reader=records reply={r:?} keys={one:?}"));
    let n = f.lines("records.jsonl").len();
    let r2 = p.submit(&f, "line one\nline two");
    wait_until(T, || f.lines("records.jsonl").len() > n);
    std::thread::sleep(ms(800));
    let two = keys_down(&f)[one.len()..].to_vec();
    fact(
        "P1",
        &format!(
            "reader=records multiline reply={r2:?} keys={two:?} submits={:?}",
            f.submits()
        ),
    );
    // Measured identically on builds 20348 and 26100 (CI run 38077981441).
    assert!(visible, "{}", esc(&out));
    assert_eq!(r, Ok(Value::Null));
    let key = |vk: u16, c: char| format!("{vk}:{c:?}");
    let word = |s: &str| {
        s.chars()
            .map(|c| key(c.to_ascii_uppercase() as u16, c))
            .collect::<Vec<_>>()
    };
    // The markers are dropped; the text comes as keys, then Enter.
    assert_eq!(one, [word("one line"), vec![key(13, '\r')]].concat());
    // A newline is Ctrl held with Enter (`ch` LF): not the Enter that submits.
    assert_eq!(r2, Ok(Value::Null));
    assert_eq!(
        two,
        [
            word("line one"),
            vec![key(17, '\0'), key(13, '\n')],
            word("line two"),
            vec![key(13, '\r')]
        ]
        .concat()
    );
    assert_eq!(f.submits(), ["one line", "line one\nline two"]);
}

/// P1-iii, an invariant and not a mode: a Node raw-mode reader (as Claude Code's runtime
/// reads) either never shows bracketed paste mode and gets nothing, or gets the reply
/// verbatim and submits it once.
#[test]
fn node_reader_is_safe() {
    let node = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(node) = node else {
        fact("P1", "reader=node skipped=no-node");
        return;
    };
    let version = String::from_utf8_lossy(&node.stdout).trim().to_string();
    let h = TestHome::new();
    let js = h.bin_dir().join("fake_agent.js");
    fs::write(&js, include_str!("win/fake_agent.js")).unwrap();
    h.script(
        "claude",
        &format!(
            "set FAKE_FIXTURE={}\nnode \"{}\"",
            fixture("claude-composer-idle").display(),
            js.display()
        ),
    );
    let mut p = Probe::with(h, &[]);
    let f = p.open("claude", 100, 30);
    p.c.settle(f.t, ms(1500));
    let visible = shows_mode(p.c.output(f.t), 'h');
    let text = "node line one\nline two é 🚀\tend";
    let r = p.submit(&f, text);
    let input = f.wait_input(if r.is_ok() { typed(text).len() } else { 1 });
    let submits = f.submits();
    let readings = p.readings();
    fact(
        "P1",
        &format!(
            "reader=node node={version} visible={visible} reply={r:?} modes={} input={} submits={submits:?}",
            String::from_utf8_lossy(&f.file("modes.json")),
            esc(&input)
        ),
    );
    // M7: the input mode Node's raw mode gives its console, as the gate read it.
    fact(
        "M7",
        &format!(
            "reader=node node={version} reply={r:?} modes={:?}",
            readings
                .iter()
                .map(|r| format!(
                    "{}:{:?}:{:?}",
                    r.step,
                    r.raw.map(|m| format!("{m:#06x}")),
                    r.vt
                ))
                .collect::<Vec<_>>()
        ),
    );
    // Replied exactly when the gate read VT input, before the paste and before Enter.
    let vt = |step: &str| {
        readings
            .iter()
            .filter(|x| x.step == step)
            .all(|x| x.vt == Some(true))
    };
    match r {
        Err(e) if e == SUBMIT_NO_VT_INPUT => {
            assert!(readings.iter().any(|x| x.vt != Some(true)), "{readings:?}");
            assert!(input.is_empty(), "{}", esc(&input));
        }
        Err(e) => {
            assert_eq!(e, SUBMIT_NOT_READY);
            assert!(input.is_empty(), "{}", esc(&input));
        }
        Ok(_) => {
            assert!(visible);
            assert!(
                vt("Paste") && vt("Enter") && readings.len() >= 2,
                "{readings:?}"
            );
            // Measured with Node 24.21 (libuv 1.52.1) on 20348 and 26100: raw mode gives
            // its console VT input (mode 0x0208).
            assert!(
                readings.iter().all(|r| r.raw == Some(0x0208)),
                "{readings:?}"
            );
            assert_eq!(esc(&input), esc(&typed(text)));
            assert_eq!(submits, [text]);
        }
    }
}

// ── P2, P3, P4: what a VT reader gets ─────────────────────────────────────

/// P3: an agent that turns bracketed paste off is refused again.
#[test]
fn bracketed_paste_off_propagates() {
    let mut p = Probe::new("vt", &[]);
    let f = p.ready("claude");
    let mark = p.c.output(f.t).len();
    f.run("paste l");
    p.c.settle(f.t, ms(700));
    let off = shows_mode(&p.c.output(f.t)[mark..], 'l');
    let r = p.submit(&f, "not now");
    fact("P3", &format!("reader=vt visible={off} reply={r:?}"));
    assert!(off);
    assert_eq!(r, Err(SUBMIT_NOT_READY.into()));
    std::thread::sleep(ms(500));
    assert!(f.input().is_empty(), "{}", esc(&f.input()));
}

/// P2: a VT reader (the W console API) reads the reply byte for byte, Enter in a read of
/// its own after the paste, and submits it once.
#[test]
fn reply_reaches_vt_reader_verbatim_and_once() {
    let mut p = Probe::new("vtw", &[]);
    let f = p.ready("claude");
    let text = "first line\nsecond é 🚀\ttab";
    let r = p.submit(&f, text);
    let want = typed(text);
    let input = f.wait_input(want.len());
    let starts = f.read_starts();
    fact(
        "P2",
        &format!(
            "reader=vtw reply={r:?} input={} reads={starts:?} submits={:?}",
            esc(&input),
            f.submits()
        ),
    );
    assert_eq!(r, Ok(Value::Null));
    assert_eq!(esc(&input), esc(&want));
    assert!(
        starts.contains(&(want.len() - 1)),
        "Enter shares a read with the paste: {starts:?}"
    );
    assert_eq!(f.submits(), [text]);
}

/// P2 for a VT reader of the A console API (`ReadFile` on `CONIN$`, input code page 65001,
/// as the plan's amendment A3 reads): the markers, ASCII text and Enter arrive as written;
/// what conhost makes of other characters on that API is a fact (measured: NULs on 20348,
/// U+FFFD for a surrogate pair on 26100), not the Daemon's to fix.
#[test]
fn reply_reaches_ansi_vt_reader_framed() {
    let mut p = Probe::new("vt", &[]);
    let f = p.ready("claude");
    let text = "first line\nsecond é 🚀\ttab";
    let r = p.submit(&f, text);
    let input = f.wait_input(typed(text).len() - 6);
    fact(
        "P2",
        &format!(
            "reader=vt-ansi reply={r:?} input={} reads={:?} modes={}",
            esc(&input),
            f.read_starts(),
            f.modes()
        ),
    );
    assert_eq!(r, Ok(Value::Null));
    assert!(
        input.starts_with(b"\x1b[200~first line\nsecond "),
        "{}",
        esc(&input)
    );
    assert!(input.ends_with(b"\ttab\x1b[201~\r"), "{}", esc(&input));
    assert_eq!(f.submits().len(), 1);
}

/// Text of `bytes` bytes or a little less, two- to four-byte characters across every
/// 1024-byte border, newlines and tabs among them.
fn long_text(bytes: usize) -> String {
    let mut s = String::new();
    for c in "aé€🚀\n\tb".chars().cycle() {
        if s.len() + c.len_utf8() > bytes {
            break;
        }
        s.push(c);
    }
    s
}

/// P4: a reply of the most bytes a reply has arrives whole and is submitted once.
#[test]
fn max_length_reply_arrives_whole() {
    let mut p = Probe::new("vtw", &[]);
    let f = p.ready("claude");
    let text = long_text(SUBMIT_MAX_BYTES);
    let r = p.submit(&f, &text);
    let want = typed(&text);
    let input = f.wait_input(want.len());
    let whole = input == want;
    let first_diff = input.iter().zip(&want).position(|(a, b)| a != b);
    fact(
        "P4",
        &format!(
            "reader=vtw reply={r:?} bytes={} got={} whole={whole} firstDiff={first_diff:?} reads={} submits={}",
            want.len(),
            input.len(),
            f.read_starts().len(),
            f.submits().len()
        ),
    );
    assert_eq!(r, Ok(Value::Null));
    assert!(whole, "first difference at {first_diff:?}");
    assert_eq!(f.submits(), [text]);
}

// ── The reply rules through ConPTY ────────────────────────────────────────

/// No reply while the agent's screen is not its composer (a model picker), though it takes
/// bracketed paste.
#[test]
fn refused_without_composer() {
    let mut p = Probe::new("vt", &[]);
    for (agent, name) in [
        ("claude", "claude-model-picker"),
        ("codex", "codex-model-picker"),
    ] {
        let f = p.open(agent, 100, 30);
        p.show(&f, name, Some('h'));
        assert_eq!(p.submit(&f, "hi"), Err(SUBMIT_NOT_READY.into()), "{name}");
        std::thread::sleep(ms(300));
        assert!(f.input().is_empty(), "{name}: {}", esc(&f.input()));
    }
}

/// No reply while a Permission Prompt shows: Enter would answer it.
#[test]
fn refused_while_prompt_shows() {
    let mut p = Probe::new("vt", &[]);
    for (agent, name) in [
        ("claude", "claude-bash-100x30"),
        ("codex", "codex-exec-100x30"),
    ] {
        let f = p.open(agent, 100, 30);
        p.show(&f, name, Some('h'));
        assert_eq!(p.submit(&f, "hi"), Err(SUBMIT_NEEDS_YOU.into()), "{name}");
        std::thread::sleep(ms(300));
        assert!(f.input().is_empty(), "{name}: {}", esc(&f.input()));
    }
}

/// No reply while the agent's hook says it needs you.
#[test]
fn refused_while_needs_you_by_hook() {
    let mut p = Probe::new("vt", &[]);
    let f = p.ready("claude");
    let id = String::from_utf8(f.file("terminal.id")).unwrap();
    let run: u64 = id.trim().rsplit_once('.').unwrap().1.parse().unwrap();
    p.c.request(&ClientMsg::TermEvent {
        terminal: f.t,
        run,
        status: AgentStatus::NeedsYou,
        session_id: None,
    })
    .unwrap();
    p.c.terminals_where(|l| {
        l.iter()
            .any(|i| i.terminal == f.t && i.agent_status == Some(AgentStatus::NeedsYou))
    });
    assert_eq!(p.submit(&f, "hi"), Err(SUBMIT_NEEDS_YOU.into()));
    std::thread::sleep(ms(300));
    assert!(f.input().is_empty(), "{}", esc(&f.input()));
}

/// A reply never takes the Terminal's size: the agent's window does not change.
#[test]
fn reply_never_resizes() {
    let mut p = Probe::new("vtw", &[]);
    let f = p.ready("claude");
    let (size, log) = (f.size(), f.file("sizes.log"));
    assert_eq!(p.submit(&f, "no resize"), Ok(Value::Null));
    f.wait_submits(1);
    std::thread::sleep(ms(500));
    fact(
        "W12",
        &format!(
            "startupWindow={:?} reachedAfter={:?} window={size}",
            f.startup.0, f.startup.1
        ),
    );
    assert_eq!(f.size(), size);
    assert_eq!(f.file("sizes.log"), log);
    assert_eq!(size, "100 30");
}

/// Readiness is checked again before Enter: the agent shows a Permission Prompt as soon as
/// it reads the end of the paste. Enter is then refused (no `\r`, outcome unknown) unless it
/// raced ahead of ConPTY's next frame; which one happened, and how fast, is a fact.
#[test]
fn readiness_checked_before_enter_conpty() {
    let mut p = Probe::new("vt", &[]);
    let f = p.ready("claude");
    f.run(&format!(
        "on-end {}",
        fixture("claude-bash-100x30").display()
    ));
    let t0 = Instant::now();
    let r = p.submit(&f, "then a prompt");
    let took = t0.elapsed();
    let input = f.wait_input(typed("then a prompt").len() - 1);
    let enter = input.ends_with(b"\r");
    fact(
        "W13",
        &format!(
            "reply={r:?} enterWritten={enter} answeredAfterMs={} shown={}",
            took.as_millis(),
            !f.file("on-end.log").is_empty()
        ),
    );
    match r {
        Err(e) => {
            assert_eq!(e, SUBMIT_UNCONFIRMED);
            assert!(!enter, "{}", esc(&input));
            assert!(f.submits().is_empty());
        }
        Ok(_) => assert!(enter),
    }
}

/// Files are Unix only: not offered, and a reply with one is refused before anything.
#[test]
fn files_refused_on_windows() {
    let mut p = Probe::new("vt", &[]);
    let f = p.ready("claude");
    assert_eq!(
        p.submit_files(&f, "look", &[r"C:\photo.jpg"]),
        Err(SUBMIT_UNSUPPORTED.into())
    );
    std::thread::sleep(ms(300));
    assert!(f.input().is_empty(), "{}", esc(&f.input()));
}

// ── A4: replies split at every byte ───────────────────────────────────────

/// A reply written one byte per write (markers and characters split, the test hook) reaches
/// a VT reader whole, also when the reader stops reading for a while in the middle.
#[test]
fn split_replies_reach_vt_reader_whole() {
    let mut p = Probe::new("vtw", &[("XSHELLD_TEST_SUBMIT_PIECE", "1")]);
    let f = p.ready("claude");
    let one = "aé🚀\nb\tc";
    assert_eq!(p.submit(&f, one), Ok(Value::Null));
    let want = typed(one);
    let input = f.wait_input(want.len());
    fact(
        "A4",
        &format!("split=1 input={} reads={:?}", esc(&input), f.read_starts()),
    );
    assert_eq!(esc(&input), esc(&want));
    // The reader pauses after its first read of the next reply.
    f.run("stall 1500");
    let two = "second ü€🦀\nend";
    assert_eq!(p.submit(&f, two), Ok(Value::Null));
    let want = [typed(one), typed(two)].concat();
    let input = f.wait_input(want.len());
    fact(
        "A4",
        &format!(
            "split=1 stalled input={} reads={:?}",
            esc(&input),
            f.read_starts()
        ),
    );
    assert_eq!(esc(&input), esc(&want));
    assert_eq!(f.wait_submits(2), [one, two]);
}

/// What completes a paste stopped after its first `off` bytes, as the Daemon writes it.
fn closed(paste: &[u8], off: usize) -> Vec<u8> {
    let end_at = paste.len() - END.len();
    if off >= end_at {
        return paste.to_vec();
    }
    let mut to = off.max(START.len());
    while to < end_at && (paste[to] & 0xC0) == 0x80 {
        to += 1;
    }
    [&paste[..to], END].concat()
}

/// A reply split at every byte and stopped inside the start marker, inside a character and
/// inside the end marker: the reader gets the paste completed (never more text, never
/// Enter), its frame closed.
#[test]
fn stopped_split_paste_is_closed_whole() {
    let text = "aé🚀b";
    let paste = [START, text.as_bytes(), END].concat();
    for off in [3, START.len() + 2, paste.len() - 3] {
        let stop = off.to_string();
        let mut p = Probe::new(
            "vtw",
            &[
                ("XSHELLD_TEST_SUBMIT_PIECE", "1"),
                ("XSHELLD_TEST_SUBMIT_STOP_AFTER", &stop),
            ],
        );
        let f = p.ready("claude");
        let r = p.submit(&f, text);
        let want = closed(&paste, off);
        let input = f.wait_input(want.len());
        let mut parse = Parse::default();
        parse.feed(&input);
        fact(
            "A4",
            &format!("stopAfter={off} reply={r:?} input={}", esc(&input)),
        );
        assert_eq!(r, Err(SUBMIT_UNCONFIRMED.into()), "{off}");
        assert_eq!(esc(&input), esc(&want), "{off}");
        assert!(!parse.in_paste && parse.starts == parse.ends, "{off}");
        assert!(f.submits().is_empty(), "{off}");
    }
}

// ── A5: a reader that stops, output that floods ───────────────────────────

/// Bounded: while the agent stops reading and floods its output, replies of the most bytes
/// are each answered within the stall limit and the Daemon stays responsive; each reply
/// the Daemon called delivered arrives whole once the agent reads again.
#[test]
fn full_input_and_output_backpressure_are_bounded() {
    let mut p = Probe::new("vtw", &[]);
    let f = p.ready("claude");
    f.run("stall 8000");
    // The first read happens on the first reply; the pause follows it.
    f.run("flood 8000");
    let mut outcomes = Vec::new();
    let mut delivered = Vec::new();
    for i in 0..4 {
        let text = format!("{i}{}", long_text(SUBMIT_MAX_BYTES - 1));
        let t0 = Instant::now();
        let r = p.submit(&f, &text);
        outcomes.push(format!("{r:?}@{}ms", t0.elapsed().as_millis()));
        assert!(t0.elapsed() < Duration::from_secs(25), "{outcomes:?}");
        if r.is_ok() {
            delivered.push(text);
        }
    }
    // Still responsive: a new Desktop is served and told the Terminal list.
    let t0 = Instant::now();
    let mut other = Client::connect(&p.h.pipe);
    assert!(other.hello().1.iter().any(|i| i.terminal == f.t));
    let responsive = t0.elapsed();
    wait_until(Duration::from_secs(40), || {
        f.submits().len() >= delivered.len()
    });
    let submits = f.submits();
    fact(
        "A5",
        &format!(
            "outcomes={outcomes:?} helloAnsweredMs={} submitted={} delivered={}",
            responsive.as_millis(),
            submits.len(),
            delivered.len()
        ),
    );
    // Measured on 20348 and 26100: conhost keeps taking input while the agent does not
    // read, so every reply is delivered (in well under a second) and arrives whole.
    assert_eq!(delivered.len(), 4, "{outcomes:?}");
    assert_eq!(submits, delivered);
}

// ── M: the input-mode gate (PR2a) ─────────────────────────────────────────
//
// Before a reply's paste and again before its Enter the Daemon reads the agent console's
// input mode with its `console-mode` helper (attached to the Terminal's console through its
// launcher), and replies only to a console that gives VT input.

/// M1: a VT reader is replied to; each reply reads the mode twice. FACT: how long a reading
/// takes (the helper's whole run, from the Daemon), over 10 replies.
#[test]
fn vt_reader_is_allowed() {
    let mut p = Probe::new("vtw", &[]);
    let f = p.ready("claude");
    let mut want = Vec::new();
    for i in 0..10 {
        let text = format!("reply {i}");
        assert_eq!(p.submit(&f, &text), Ok(Value::Null), "{i}");
        want.push(text);
    }
    assert_eq!(f.wait_submits(10), want);
    let readings = p.readings();
    let (p50, max) = spread(readings.iter().map(|r| r.ms).collect());
    fact(
        "M1",
        &format!(
            "readings={} p50Ms={p50} maxMs={max} procs={:?} raw={:?} leader={:?} terminalPid={}",
            readings.len(),
            readings.iter().map(|r| r.procs).collect::<Vec<_>>(),
            readings
                .first()
                .and_then(|r| r.raw)
                .map(|m| format!("{m:#06x}")),
            readings.first().and_then(|r| r.leader),
            f.pid
        ),
    );
    // Every reply read VT input before its paste and again before its Enter (and maybe
    // again before a retry: the pipe takes nothing while conhost has not read a piece).
    assert!(readings.len() >= 20, "{readings:?}");
    assert!(readings.iter().all(|r| r.vt == Some(true)), "{readings:?}");
    assert_eq!(readings.iter().filter(|r| r.step == "Enter").count(), 10);
    assert!(readings.iter().all(|r| r.leader == Some(f.pid)));
    // Measured on 20348 and 26100 (CI run 38083262893): p50 11 and 18 ms, at most 12 and
    // 24 ms, far under the helper's 2 s timeout. The console has four processes then: the
    // launcher, cmd.exe, the agent, and the helper itself.
    assert!(max < 1000, "a reading took {max} ms");
    assert!(readings.iter().all(|r| r.procs == Some(4)), "{readings:?}");
}

/// M2: an agent that reads key events is refused before anything is typed, and again at
/// once (no reading) while that refusal is recent.
#[test]
fn records_reader_is_refused_before_anything_is_typed() {
    let mut p = Probe::new("records", &[]);
    let f = p.ready("codex");
    // The console's own records before the reply (its window resized at the start).
    let events = f.file("events.jsonl");
    assert_eq!(p.submit(&f, "two\nlines"), Err(SUBMIT_NO_VT_INPUT.into()));
    let n = p.readings().len();
    let t0 = Instant::now();
    assert_eq!(p.submit(&f, "again"), Err(SUBMIT_NO_VT_INPUT.into()));
    let again = t0.elapsed();
    std::thread::sleep(ms(1000));
    let readings = p.readings();
    fact(
        "M2",
        &format!(
            "reader=records readings={readings:?} againMs={} modes={}",
            again.as_millis(),
            f.modes()
        ),
    );
    assert_eq!(n, 1, "{readings:?}");
    assert_eq!(readings.len(), 1, "read again within the refusal's TTL");
    assert_eq!(readings[0].vt, Some(false));
    assert!(again < ms(250), "{again:?}");
    assert!(
        f.file("records.jsonl").is_empty(),
        "{}",
        esc(&f.file("records.jsonl"))
    );
    assert_eq!(
        esc(&f.file("events.jsonl")),
        esc(&events),
        "the helper added records"
    );
    assert!(f.submits().is_empty());
}

/// M3: the agent stops taking VT input when it reads the end of the paste: the reading
/// before Enter sees it, and Enter is withheld.
#[test]
fn reader_switching_mode_mid_reply_gets_no_enter() {
    let mut p = Probe::new("vt", &[("XSHELLD_TEST_SUBMIT_ENTER_DELAY_MS", "500")]);
    let f = p.ready("claude");
    f.run("on-end-mode records");
    let r = p.submit(&f, "then key events");
    let input = f.wait_input(typed("then key events").len() - 1);
    let readings = p.readings();
    fact(
        "M3",
        &format!(
            "reply={r:?} input={} readings={readings:?} onEnd={}",
            esc(&input),
            String::from_utf8_lossy(&f.file("on-end.log"))
        ),
    );
    assert_eq!(r, Err(SUBMIT_UNCONFIRMED.into()));
    assert!(input.ends_with(END), "{}", esc(&input));
    assert!(!input.contains(&b'\r'), "{}", esc(&input));
    assert!(f.submits().is_empty());
    let last = readings.last().unwrap();
    assert_eq!((last.step.as_str(), last.vt), ("Enter", Some(false)));
}

/// M3b: an agent that turns VT input on is replied to once the refusal is no longer recent.
#[test]
fn records_reader_switched_to_vt_is_allowed_after_the_ttl() {
    let mut p = Probe::new("vtw-records", &[]);
    let f = p.ready("claude");
    assert_eq!(p.submit(&f, "not yet"), Err(SUBMIT_NO_VT_INPUT.into()));
    f.run("mode vt");
    // Still refused at once: the refusal is recent.
    assert_eq!(p.submit(&f, "not yet"), Err(SUBMIT_NO_VT_INPUT.into()));
    std::thread::sleep(ms(2200));
    assert_eq!(p.submit(&f, "now"), Ok(Value::Null));
    assert_eq!(f.wait_submits(1), ["now"]);
    assert!(f.input().ends_with(&typed("now")), "{}", esc(&f.input()));
}

/// M4: a helper that hangs, prints garbage, fails, or does not exist refuses the reply
/// within its timeout, and nothing is typed. One that hangs is ended.
#[test]
fn mode_helper_fails_closed() {
    for sim in ["garbage", "fail", "missing", "hang"] {
        let h = TestHome::new();
        let pidfile = h.dir.path().join("helper.pid");
        let env_sim = if sim == "hang" {
            format!("hang:{}", pidfile.display())
        } else {
            sim.to_string()
        };
        for agent in ["claude", "codex"] {
            let exe = std::env::current_exe().unwrap();
            h.script(
                agent,
                &format!(
                    "set XSHELLD_WIN_FAKE=vt\n\"{}\" --exact helper_console_agent --nocapture --test-threads=1 >NUL 2>&1",
                    exe.display()
                ),
            );
        }
        let mut p = Probe::with(h, &[("XSHELLD_TEST_CONSOLE_MODE", &env_sim)]);
        let f = p.ready("claude");
        let t0 = Instant::now();
        let r = p.submit(&f, "hi");
        let took = t0.elapsed();
        std::thread::sleep(ms(300));
        let readings = p.readings();
        fact(
            "M4",
            &format!(
                "sim={sim} reply={r:?} tookMs={} readings={readings:?}",
                took.as_millis()
            ),
        );
        assert_eq!(r, Err(SUBMIT_NO_VT_INPUT.into()), "{sim}");
        assert!(took < Duration::from_secs(3) + ms(1000), "{sim}: {took:?}");
        assert!(f.input().is_empty(), "{sim}: {}", esc(&f.input()));
        assert!(
            readings.iter().all(|r| r.error.is_some()),
            "{sim}: {readings:?}"
        );
        if sim == "hang" {
            let pid = read_pid_file(&pidfile);
            assert!(
                wait_dead(pid, Duration::from_secs(5)),
                "the hung helper {pid} lives"
            );
        }
    }
}

/// A3: the helper ignores Ctrl+Break on the console it attached to, and ends when that
/// console closes.
#[test]
fn mode_helper_ignores_ctrl_break_and_ends_with_the_console() {
    let h = TestHome::new();
    let pidfile = h.dir.path().join("helper.pid");
    let exe = std::env::current_exe().unwrap();
    h.script(
        "claude",
        &format!(
            "set XSHELLD_WIN_FAKE=vt\n\"{}\" --exact helper_console_agent --nocapture --test-threads=1 >NUL 2>&1",
            exe.display()
        ),
    );
    let sim = format!("hang:{}", pidfile.display());
    let mut p = Probe::with(
        h,
        &[
            ("XSHELLD_TEST_CONSOLE_MODE", &sim),
            ("XSHELLD_TEST_CONSOLE_MODE_TIMEOUT_MS", "60000"),
        ],
    );
    let f = p.ready("claude");
    let id = p.c.send_req(&ClientMsg::TermSubmit {
        terminal: f.t,
        text: "hi".into(),
        files: vec![],
    });
    let helper = read_pid_file(&pidfile);
    f.run("ctrl-break");
    std::thread::sleep(ms(1500));
    let survived = alive(helper);
    let t0 = Instant::now();
    p.c.close(f.t);
    let ended = wait_dead(helper, Duration::from_secs(15));
    let r = p.c.try_res(id, T);
    fact(
        "A3",
        &format!(
            "ctrl={} helperSurvivedCtrlBreak={survived} endedWithConsole={ended} afterMs={} reply={r:?}",
            String::from_utf8_lossy(&f.file("ctrl.log")).trim(),
            t0.elapsed().as_millis()
        ),
    );
    assert!(
        f.file("ctrl.log").starts_with(b"sent 1"),
        "Ctrl+Break not sent"
    );
    assert!(survived, "Ctrl+Break ended the helper");
    assert!(ended, "the helper outlived its console");
    assert!(matches!(r, Some(Err(_))), "{r:?}");
    assert!(f.input().is_empty(), "{}", esc(&f.input()));
}

/// M5: the mode is read through the launcher and `cmd.exe`, not from the fake itself: the
/// leader read is the Terminal's process, and the VT bit is the fake's own.
#[test]
fn mode_is_read_through_the_cmd_wrapper() {
    for reader in ["vt", "records"] {
        let mut p = Probe::new(reader, &[]);
        let f = p.ready("claude");
        let r = p.submit(&f, "x");
        let modes = f.modes();
        let own: u32 = modes["pid"].as_u64().unwrap() as u32;
        let readings = p.readings();
        // The fake's own console mode, read directly by the CLI.
        let direct = console_mode_cli(own);
        fact(
            "M5",
            &format!(
                "reader={reader} reply={r:?} leader={:?} fakePid={own} fakeVt={} viaLeader={:?} direct={direct:?}",
                readings.first().and_then(|r| r.leader),
                modes["vtInput"],
                readings.first().map(|r| (r.raw, r.procs)),
            ),
        );
        let first = &readings[0];
        assert_eq!(first.leader, Some(f.pid));
        assert_ne!(f.pid, own, "the fake is the Terminal's process itself");
        assert_eq!(first.vt, modes["vtInput"].as_bool(), "{readings:?}");
        assert_eq!(direct.0, 0, "{direct:?}");
        assert_eq!(
            direct.1.split_whitespace().nth(2).map(str::to_string),
            first.raw.map(|m| format!("mode=0x{m:08x}")),
            "the leader's console and the fake's are one: {direct:?}"
        );
    }
}

/// `xshelld console-mode <pid>` run as the Daemon runs it (detached): its exit code, its
/// output and how long it took.
fn console_mode_cli(pid: u32) -> (i32, String, Duration) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let t0 = Instant::now();
    let out = std::process::Command::new(bin())
        .args(["console-mode", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        t0.elapsed(),
    )
}

/// M6: the helper itself, 20 times against a reader of key events: it reads the mode
/// (VT off) and puts nothing in the console's input. It fails (exit 1, `error=attach:`) for
/// a process that does not exist and for one without a console.
#[test]
fn console_mode_cli_never_writes_input() {
    let mut p = Probe::new("records", &[("XSHELLD_TEST_CONSOLE_MODE", "off")]);
    let f = p.open("codex", 100, 30);
    // The console's own records (its window resized at the start).
    std::thread::sleep(ms(500));
    let events = f.file("events.jsonl");
    let mut times = Vec::new();
    let mut lines = Vec::new();
    for _ in 0..20 {
        let (code, out, took) = console_mode_cli(f.pid);
        assert_eq!(code, 0, "{out:?}");
        assert!(out.starts_with("xshelld-console-mode 1 mode=0x"), "{out:?}");
        times.push(took.as_millis() as u64);
        lines.push(out.trim().to_string());
    }
    std::thread::sleep(ms(1000));
    let (p50, max) = spread(times.clone());
    let gone = console_mode_cli(0x7fff_fff0);
    // A process with no console: a detached cmd.exe.
    use std::os::windows::process::CommandExt;
    let mut lone = std::process::Command::new("cmd.exe")
        .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0000_0008)
        .spawn()
        .unwrap();
    let none = console_mode_cli(lone.id());
    let _ = lone.kill();
    let _ = lone.wait();
    fact(
        "M6",
        &format!(
            "cliP50Ms={p50} cliMaxMs={max} first={:?} gone={gone:?} noConsole={none:?} records={} events={}",
            lines[0],
            f.lines("records.jsonl").len(),
            f.lines("events.jsonl").len()
        ),
    );
    for l in &lines {
        let m = u32::from_str_radix(&l["xshelld-console-mode 1 mode=0x".len()..][..8], 16).unwrap();
        assert_eq!(m & 0x0200, 0, "{l}");
    }
    assert!(
        f.file("records.jsonl").is_empty(),
        "{}",
        esc(&f.file("records.jsonl"))
    );
    assert_eq!(
        esc(&f.file("events.jsonl")),
        esc(&events),
        "the helper added records"
    );
    // Measured on 20348 and 26100: AttachConsole works on a pseudoconsole; it fails with
    // ERROR_INVALID_PARAMETER for a process that does not exist and ERROR_INVALID_HANDLE
    // for one without a console.
    assert_eq!(gone.0, 1, "{gone:?}");
    assert_eq!(gone.1, "xshelld-console-mode 1 error=attach:87\n");
    assert!(gone.2 < Duration::from_secs(1), "{gone:?}");
    assert_eq!(none.0, 1, "{none:?}");
    assert_eq!(none.1, "xshelld-console-mode 1 error=attach:6\n");
    assert!(max < 1000, "the helper took {max} ms");
}

/// M8: a reader of key events on a console someone set to VT input passes the gate (the
/// gate reads the console's mode, not the reader's API). FACT: what it gets.
#[test]
fn records_reader_on_a_vt_console_fact() {
    let mut p = Probe::new("records", &[]);
    let f = p.ready("claude");
    f.run("mode vt");
    let r = p.submit(&f, "one\ntwo");
    std::thread::sleep(ms(1500));
    fact(
        "M8",
        &format!(
            "reply={r:?} keys={:?} events={} submits={:?} readings={:?}",
            keys_down(&f),
            f.lines("events.jsonl").len(),
            f.submits(),
            p.readings()
        ),
    );
}

// ── B: output backpressure (Sol's P2) ─────────────────────────────────────
//
// The Daemon's reading of Terminal output is paused (a test hook), so conhost's output
// backs up and, the probes show, conhost stops taking input. Each probe runs in a child
// process the test ends after 90 s.

const B_DEADLINE: Duration = Duration::from_secs(90);

/// Whether the trace has `line` (exactly) after its `from`-th line.
fn traced(p: &Probe, from: usize, pred: impl Fn(&str) -> bool) -> bool {
    p.trace().iter().skip(from).any(|l| pred(l))
}

/// The `run` a Terminal's hooks report with.
fn run_of(f: &Fake) -> u64 {
    let id = String::from_utf8(f.file("terminal.id")).unwrap();
    id.trim().rsplit_once('.').unwrap().1.parse().unwrap()
}

/// Stall the Terminal's output until a write of the fake's flood has been blocked for 1 s.
fn stall_output(p: &Probe, f: &Fake) {
    f.run("flood 40000");
    fs::write(&p.pause, b"").unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || f.flood().0 >= 1000),
        "the output never blocked: {:?}",
        f.flood()
    );
}

/// The protocol answers while the output is stalled: a prompt answer (which takes the
/// screen and prompt locks a reply holds while it writes) and a hook report (the status
/// lock) on this connection within 1 s each, and a new connection's hello within 2 s.
/// Their times.
fn still_answers(p: &mut Probe, f: &Fake) -> String {
    let t0 = Instant::now();
    let a = p.c.request(&ClientMsg::TermAnswer {
        terminal: f.t,
        prompt: 999_999,
        option: 0,
    });
    let answer = t0.elapsed();
    assert!(a.is_err(), "{a:?}");
    let t0 = Instant::now();
    p.c.request(&ClientMsg::TermEvent {
        terminal: f.t,
        run: run_of(f),
        status: AgentStatus::Working,
        session_id: None,
    })
    .unwrap();
    let event = t0.elapsed();
    let t0 = Instant::now();
    let mut other = Client::connect(&p.h.pipe);
    assert!(other.hello().1.iter().any(|i| i.terminal == f.t));
    let hello = t0.elapsed();
    assert!(answer < ms(1000), "term.answer took {answer:?}");
    assert!(event < ms(1000), "term.event took {event:?}");
    assert!(hello < ms(2000), "hello took {hello:?}");
    format!(
        "answerMs={} eventMs={} helloMs={}",
        answer.as_millis(),
        event.as_millis(),
        hello.as_millis()
    )
}

/// B1: with the output stalled, conhost takes no input: a blocking write of 64 KiB (a
/// Desktop's keys) does not complete for over a second, and the agent reads nothing. While
/// that write is blocked the Daemon answers, takes more input and a reply; once the output
/// drains again, everything arrives in order and the reply is submitted once.
#[test]
fn reply_waits_out_output_backpressure() {
    if !in_child("reply_waits_out_output_backpressure", B_DEADLINE) {
        return;
    }
    let mut p = Probe::new("vtw", &[("XSHELLD_TEST_CONSOLE_MODE_TIMEOUT_MS", "30000")]);
    p.note_for_parent();
    let f = p.ready("claude");
    stall_output(&p, &f);
    let read_before = f.input().len();
    let mark = p.trace().len();
    let witness = "k".repeat(64 * 1024);
    p.c.input(f.t, &witness);
    assert!(
        wait_until(T, || traced(&p, mark, |l| l == "block start 65536")),
        "{:?}",
        p.trace()
    );
    let t0 = Instant::now();
    std::thread::sleep(ms(1000));
    // A2: the blocking write is still in progress after 1 s, and the agent read nothing.
    let blocked = !traced(&p, mark, |l| {
        l.starts_with("block done") || l.starts_with("block err")
    });
    let read_during = f.input().len() - read_before;
    let answers = still_answers(&mut p, &f);
    p.c.input(f.t, "x");
    let text = "after the stall\nsecond line";
    let id = p.c.send_req(&ClientMsg::TermSubmit {
        terminal: f.t,
        text: text.into(),
        files: vec![],
    });
    // Still blocked: everything above happened inside the blocked interval.
    let still = !traced(&p, mark, |l| {
        l.starts_with("block done") || l.starts_with("block err")
    });
    let interval = t0.elapsed();
    fs::remove_file(&p.pause).unwrap();
    let r = p.c.try_res(id, Duration::from_secs(40));
    let want = [witness.as_bytes(), b"x", &typed(text)].concat();
    let input = f.wait_input(read_before + want.len());
    let trace = p.trace();
    fact(
        "B1",
        &format!(
            "blockedAfter1s={blocked} stillBlockedAfterChecks={still} intervalMs={} readDuring={read_during} {answers} reply={r:?} flood={:?} readings={:?}",
            interval.as_millis(),
            f.flood(),
            p.readings()
        ),
    );
    assert!(
        blocked,
        "the input write completed while the output was stalled: {trace:?}"
    );
    assert_eq!(
        read_during, 0,
        "the agent read input while the output was stalled"
    );
    assert!(still, "the checks outlived the blocked interval: {trace:?}");
    assert_eq!(r, Some(Ok(Value::Null)));
    assert_eq!(input.len() - read_before, want.len());
    assert!(
        input[read_before..] == want[..],
        "the input is not in order"
    );
    assert_eq!(f.wait_submits(1), [format!("{witness}x{text}")]);
    assert!(trace.iter().any(|l| l == "block done"), "{trace:?}");
    assert!(!trace.iter().any(|l| l.contains("err=")), "{trace:?}");
}

/// B1b: a reply meets the stall in the middle of its paste (the output stops draining
/// before a piece) and waits it out: the pipe takes nothing (each try after a fresh
/// reading of the mode), and once the output drains the reply is delivered whole. FACT: what
/// the readings did while the console was stalled.
#[test]
fn reply_waits_out_backpressure_mid_paste() {
    if !in_child("reply_waits_out_backpressure_mid_paste", B_DEADLINE) {
        return;
    }
    let mut p = Probe::new(
        "vtw",
        &[
            ("XSHELLD_TEST_CONSOLE_MODE_TIMEOUT_MS", "30000"),
            ("XSHELLD_TEST_DRAIN_PAUSE_ON_PASTE", "1500"),
            ("XSHELLD_TEST_DRAIN_PAUSE_AFTER", "2048"),
            ("XSHELLD_TEST_SUBMIT_STALL_MS", "30000"),
        ],
    );
    p.note_for_parent();
    let f = p.ready("claude");
    f.run("flood 40000");
    let text = long_text(SUBMIT_MAX_BYTES);
    let id = p.c.send_req(&ClientMsg::TermSubmit {
        terminal: f.t,
        text: text.clone(),
        files: vec![],
    });
    assert!(wait_until(T, || traced(&p, 0, |l| l == "paused")));
    let paused_at = p.trace().len();
    assert!(
        wait_until(Duration::from_secs(20), || f.flood().0 >= 1000),
        "the output never blocked"
    );
    std::thread::sleep(ms(2000));
    let during = p.trace()[paused_at..].to_vec();
    let answers = still_answers(&mut p, &f);
    fs::remove_file(&p.pause).unwrap();
    let r = p.c.try_res(id, Duration::from_secs(60));
    let input = f.wait_input(typed(&text).len());
    fact(
        "B1b",
        &format!(
            "reply={r:?} {answers} duringPause={during:?} readings={:?}",
            p.readings()
                .iter()
                .map(|r| format!("{}:{:?}:{}ms:{:?}", r.step, r.vt, r.ms, r.error))
                .collect::<Vec<_>>()
        ),
    );
    assert_eq!(r, Some(Ok(Value::Null)));
    assert!(input == typed(&text), "not delivered whole");
    assert_eq!(f.wait_submits(1), [text]);
    // Measured on 20348 and 26100: the reading the stall made stale (taken with the helper's
    // timeout raised) waits until the output drains, then answers.
    let readings = p.readings();
    assert!(readings.iter().all(|r| r.vt == Some(true)), "{readings:?}");
    assert!(readings.iter().any(|r| r.ms >= 1000), "{readings:?}");
}

/// B2 (A4): a reply stopped by the stall in the middle of its paste, at an acknowledged
/// boundary: it gives up (unconfirmed), its paste cannot be completed while the output is
/// stalled (stuck: the next reply is refused), and a Desktop's key still arrives once the
/// output drains. The existing hazard D9 accepts, measured without the input-mode gate.
#[test]
fn reply_gives_up_under_output_backpressure_and_input_recovers() {
    if in_child(
        "reply_gives_up_under_output_backpressure_and_input_recovers",
        B_DEADLINE,
    ) {
        stopped_mid_paste(false);
    }
}

/// B2 with the gate, as a Windows Host runs it: the reading the stall makes stale cannot be
/// taken again while conhost is stalled (the helper does not answer), so the reply stops
/// there, within the helper's timeout, with the pipe still empty: its paste is completed (the
/// end marker reaches the agent once the output drains), and the next reply is refused by
/// that failed reading, not as stuck.
#[test]
fn gated_reply_stops_under_output_backpressure_and_completes_its_paste() {
    if in_child(
        "gated_reply_stops_under_output_backpressure_and_completes_its_paste",
        B_DEADLINE,
    ) {
        stopped_mid_paste(true);
    }
}

fn stopped_mid_paste(gate: bool) {
    let h = TestHome::new();
    let ack = h.dir.path().join("pause.ack");
    let exe = std::env::current_exe().unwrap();
    for agent in ["claude", "codex"] {
        h.script(
            agent,
            &format!(
                "set XSHELLD_WIN_FAKE=vtw\n\"{}\" --exact helper_console_agent --nocapture --test-threads=1 >NUL 2>&1",
                exe.display()
            ),
        );
    }
    let ack_s = ack.to_string_lossy().into_owned();
    let mut env = vec![
        ("XSHELLD_TEST_SUBMIT_STALL_MS", "2000"),
        // Long enough for the output to back up before the next piece.
        ("XSHELLD_TEST_DRAIN_PAUSE_ON_PASTE", "3000"),
        ("XSHELLD_TEST_DRAIN_PAUSE_AFTER", "2048"),
        ("XSHELLD_TEST_DRAIN_PAUSE_ACK", ack_s.as_str()),
    ];
    if !gate {
        env.push(("XSHELLD_TEST_CONSOLE_MODE", "off"));
    }
    let mut p = Probe::with(h, &env);
    p.note_for_parent();
    let f = p.ready("claude");
    f.run("flood 40000");
    let text = long_text(SUBMIT_MAX_BYTES);
    let paste = typed(&text);
    let id = p.c.send_req(&ClientMsg::TermSubmit {
        terminal: f.t,
        text: text.clone(),
        files: vec![],
    });
    // The boundary: the reply wrote `at` bytes; the agent has read exactly those.
    let mut at = None;
    assert!(wait_until(T, || {
        at = p.trace().iter().find_map(|l| {
            l.strip_prefix("pause at ")
                .and_then(|n| n.parse::<usize>().ok())
        });
        at.is_some()
    }));
    let at = at.unwrap();
    assert!(
        wait_until(T, || f.input().len() >= at),
        "the agent read {} of {at}",
        f.input().len()
    );
    assert_eq!(f.input(), paste[..at], "the prefix");
    fs::write(&ack, b"").unwrap();
    // The output backs up before the reply's next piece.
    assert!(
        wait_until(Duration::from_secs(20), || f.flood().0 >= 1000),
        "the output never blocked"
    );
    // Held through the delivery's stall deadline and the completion's.
    let t0 = Instant::now();
    let r = p.c.try_res(id, Duration::from_secs(60));
    let took = t0.elapsed();
    let next = p.submit(&f, "next");
    let mark = p.trace().len();
    let answers = still_answers(&mut p, &f);
    p.c.input(f.t, "k");
    assert!(wait_until(T, || traced(&p, mark, |l| l == "block start 1")));
    let read_stalled = f.input().len();
    fs::remove_file(&p.pause).unwrap();
    assert!(
        wait_until(T, || f.input().ends_with(b"k")),
        "the key never arrived"
    );
    std::thread::sleep(ms(500));
    let input = f.input();
    let mut other = Client::connect(&p.h.pipe);
    let listed = other.hello().1.iter().any(|i| i.terminal == f.t);
    fact(
        if gate { "B2g" } else { "B2" },
        &format!(
            "pauseAt={at} reply={r:?} afterMs={} next={next:?} {answers} readWhileStalled={read_stalled} got={} readings={:?}",
            took.as_millis(),
            input.len(),
            p.readings()
                .iter()
                .map(|r| format!("{}:{:?}:{}ms:{:?}", r.step, r.vt, r.ms, r.error))
                .collect::<Vec<_>>()
        ),
    );
    assert_eq!(r, Some(Err(SUBMIT_UNCONFIRMED.into())));
    assert_eq!(
        read_stalled, at,
        "the agent read input while the output was stalled"
    );
    assert!(!input.contains(&b'\r'));
    assert!(listed);
    assert!(p.trace().iter().any(|l| l == "block done"));
    let (typed_before, key) = input.split_at(input.len() - 1);
    assert_eq!(key, b"k");
    if gate {
        // Measured on 20348 and 26100: the helper does not answer while conhost is stalled,
        // so the reading before the next piece fails at its timeout; nothing more of the
        // text, the end marker at once.
        assert_eq!(next, Err(SUBMIT_NO_VT_INPUT.into()));
        let last = p.readings().last().cloned().unwrap();
        assert_eq!(
            last.error.as_deref(),
            Some("the console-mode helper did not answer in time"),
            "{last:?}"
        );
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(esc(typed_before), esc(&[&paste[..at], END].concat()));
    } else {
        // A prefix of the paste (at least to the boundary, never its end marker).
        assert_eq!(next, Err(SUBMIT_STUCK.into()));
        assert!(typed_before.len() >= at && typed_before.len() < paste.len() - END.len() - 1);
        assert!(
            typed_before == &paste[..typed_before.len()],
            "not a prefix of the paste"
        );
    }
}

/// A `nowait` line of the trace: whether it took nothing, the unread bytes, the free bytes
/// and the quota of the pipe after it.
type Nowait = (bool, Option<u64>, Option<u64>, Option<u64>);

/// The `nowait` lines of the trace from its `from`-th line.
fn nowaits(p: &Probe, from: usize) -> Vec<Nowait> {
    p.trace()
        .iter()
        .skip(from)
        .filter(|l| l.starts_with("nowait "))
        .map(|l| {
            let field = |k: &str| {
                l.split(' ')
                    .find_map(|w| w.strip_prefix(k))
                    .and_then(|v| v.parse::<u64>().ok())
            };
            (
                l.contains(" wouldblock "),
                field("avail="),
                field("free="),
                field("quota="),
            )
        })
        .collect()
}

/// B4 (Sol's review of PR2a, item 3): the Daemon stays responsive while a reply's own
/// non-blocking writes keep retrying against a full input pipe. Without the input-mode gate
/// (its helper does not answer while conhost is stalled, B3), so the write path itself is
/// what runs: the output stops draining at an acknowledged boundary of the paste, the next
/// pieces fill the pipe (its free quota stays the same: nothing is read from it),
/// and the reply retries. During the retries a prompt answer, a hook report and a new
/// connection are answered; the retries go on after them; once the output drains the reply
/// is delivered whole.
#[test]
fn daemon_answers_while_a_reply_retries_a_full_pipe() {
    if !in_child(
        "daemon_answers_while_a_reply_retries_a_full_pipe",
        B_DEADLINE,
    ) {
        return;
    }
    let h = TestHome::new();
    let ack = h.dir.path().join("pause.ack");
    let exe = std::env::current_exe().unwrap();
    for agent in ["claude", "codex"] {
        h.script(
            agent,
            &format!(
                "set XSHELLD_WIN_FAKE=vtw\n\"{}\" --exact helper_console_agent --nocapture --test-threads=1 >NUL 2>&1",
                exe.display()
            ),
        );
    }
    let ack_s = ack.to_string_lossy().into_owned();
    let mut p = Probe::with(
        h,
        &[
            ("XSHELLD_TEST_CONSOLE_MODE", "off"),
            ("XSHELLD_TEST_SUBMIT_PIECE", "4096"),
            ("XSHELLD_TEST_SUBMIT_STALL_MS", "60000"),
            // Long enough for the output to back up before the next piece.
            ("XSHELLD_TEST_DRAIN_PAUSE_ON_PASTE", "3000"),
            ("XSHELLD_TEST_DRAIN_PAUSE_AFTER", "4096"),
            ("XSHELLD_TEST_DRAIN_PAUSE_ACK", &ack_s),
        ],
    );
    p.note_for_parent();
    let f = p.ready("claude");
    f.run("flood 40000");
    let text = format!("{}\n", "b".repeat(99)).repeat(160);
    let paste = typed(&text);
    let id = p.c.send_req(&ClientMsg::TermSubmit {
        terminal: f.t,
        text: text.clone(),
        files: vec![],
    });
    let mut at = None;
    assert!(wait_until(T, || {
        at = p.trace().iter().find_map(|l| {
            l.strip_prefix("pause at ")
                .and_then(|n| n.parse::<usize>().ok())
        });
        at.is_some()
    }));
    let at = at.unwrap();
    assert!(wait_until(T, || f.input().len() >= at));
    assert!(f.input() == paste[..at], "the prefix");
    let paused = p.trace().len();
    fs::write(&ack, b"").unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || f.flood().0 >= 1000),
        "the output never blocked"
    );
    // The reply retries against a full pipe.
    // The retries against a pipe conhost no longer reads: each takes nothing, and the pipe's
    // free quota stays what it was at the first of them (nothing is read from it).
    // Measured: on 26100 the pipe is full (no free quota); on 20348 conhost's reader took
    // 256 bytes of the last piece before it blocked, and those 256 bytes stay free (a
    // non-blocking write takes nothing while the pipe holds unread data: P5).
    let stuck_free = |p: &Probe| {
        nowaits(p, paused)
            .into_iter()
            .find(|(wb, _, free, _)| *wb && free.is_some())
            .and_then(|(_, _, free, _)| free)
    };
    let full = |p: &Probe| {
        let first = stuck_free(p);
        let n = nowaits(p, paused);
        let after: Vec<_> = n.iter().skip_while(|(wb, _, _, _)| !*wb).collect();
        assert!(
            after.iter().all(|(wb, _, free, _)| *wb && *free == first),
            "the pipe was read from, or took a write: {n:?}"
        );
        after.len()
    };
    // At least 1 s of retries (each waits 10 ms first).
    assert!(
        wait_until(Duration::from_secs(20), || full(&p) >= 100),
        "no retries against a full pipe: {:?}",
        nowaits(&p, paused)
    );
    let before = full(&p);
    let read_before = f.input().len();
    let answers = still_answers(&mut p, &f);
    let pending = p.c.try_res(id, ms(0)).is_none();
    std::thread::sleep(ms(200));
    let after = full(&p);
    let read_during = f.input().len() - read_before;
    let sample: Vec<_> = nowaits(&p, paused).into_iter().rev().take(3).collect();
    fs::remove_file(&p.pause).unwrap();
    let r = p.c.try_res(id, Duration::from_secs(60));
    let input = f.wait_input(paste.len());
    fact(
        "B4",
        &format!(
            "pauseAt={at} {answers} retriesBefore={before} after={after} pending={pending} readDuring={read_during} freeQuota={:?} last={sample:?} reply={r:?} whole={}",
            stuck_free(&p),
            input == paste
        ),
    );
    assert!(pending, "the reply ended during the checks");
    assert!(after > before, "the retries stopped during the checks");
    assert_eq!(read_during, 0);
    assert_eq!(r, Some(Ok(Value::Null)));
    assert!(input == paste, "not delivered whole");
    assert_eq!(f.wait_submits(1), [text]);
}

/// M9: a reading is as old as the helper's start. A helper that samples the mode and then
/// answers only after `mode_fresh` (a debug-build simulation) allows no write: the reply is
/// refused with nothing typed.
#[test]
fn a_slow_helper_reading_allows_no_write() {
    let mut p = Probe::new("vtw", &[("XSHELLD_TEST_CONSOLE_MODE", "slow:300")]);
    let f = p.ready("claude");
    let r = p.submit(&f, "too slow");
    std::thread::sleep(ms(500));
    let readings = p.readings();
    fact(
        "M9",
        &format!(
            "reply={r:?} readings={:?}",
            readings
                .iter()
                .map(|r| format!("{}:{:?}:{}ms", r.step, r.vt, r.ms))
                .collect::<Vec<_>>()
        ),
    );
    assert_eq!(r, Err(SUBMIT_NO_VT_INPUT.into()));
    assert!(f.input().is_empty(), "{}", esc(&f.input()));
    assert!(readings.len() >= 2, "{readings:?}");
    assert!(
        readings.iter().all(|r| r.vt == Some(true) && r.ms >= 300),
        "{readings:?}"
    );
    assert!(p.trace().iter().any(|l| l == "stale Paste"));
}

/// B3: the helper while conhost is stalled. FACT: does it answer? Either way the reply is
/// safe: refused with nothing typed, or delivered once the output drains (or, read fine
/// but then not, unconfirmed without Enter), and the Daemon answers meanwhile.
#[test]
fn mode_helper_under_output_backpressure() {
    if !in_child("mode_helper_under_output_backpressure", B_DEADLINE) {
        return;
    }
    let mut p = Probe::new("vtw", &[]);
    p.note_for_parent();
    let f = p.ready("claude");
    stall_output(&p, &f);
    let id = p.c.send_req(&ClientMsg::TermSubmit {
        terminal: f.t,
        text: "during the stall".into(),
        files: vec![],
    });
    let t0 = Instant::now();
    let mut r = p.c.try_res(id, Duration::from_secs(5));
    let answered_stalled = r.is_some();
    let answers = still_answers(&mut p, &f);
    let during = p.readings();
    fs::remove_file(&p.pause).unwrap();
    if r.is_none() {
        r = p.c.try_res(id, Duration::from_secs(40));
    }
    let r = r.expect("no outcome");
    std::thread::sleep(ms(1500));
    let input = f.input();
    fact(
        "B3",
        &format!(
            "reply={r:?} answeredWhileStalled={answered_stalled} afterMs={} {answers} readingsWhileStalled={:?} input={}",
            t0.elapsed().as_millis(),
            during
                .iter()
                .map(|r| format!("{}:{:?}:{}ms:{:?}", r.step, r.vt, r.ms, r.error))
                .collect::<Vec<_>>(),
            esc(&input)
        ),
    );
    match &r {
        Ok(_) => {
            assert_eq!(f.wait_submits(1), ["during the stall"]);
            assert!(input.ends_with(&typed("during the stall")));
        }
        Err(e) if e == SUBMIT_NO_VT_INPUT => assert!(input.is_empty(), "{}", esc(&input)),
        Err(e) => {
            assert_eq!(e, SUBMIT_UNCONFIRMED);
            assert!(!input.contains(&b'\r'), "{}", esc(&input));
        }
    }
    // Measured on 20348 and 26100 (CI run 38083262893): the helper does not answer while
    // conhost's output is stalled (AttachConsole or GetConsoleMode waits for it), so the
    // reply is refused at the helper's timeout, with nothing typed.
    assert_eq!(r, Err(SUBMIT_NO_VT_INPUT.into()));
    assert!(answered_stalled);
    assert_eq!(during.len(), 1, "{during:?}");
    assert_eq!(
        during[0].error.as_deref(),
        Some("the console-mode helper did not answer in time")
    );
}

// ── W15: the composer and prompt fixtures through ConPTY ──────────────────

/// Every prompt fixture drawn by the console fake through a real ConPTY reads as its
/// source does: the same composer, images and Permission Prompt. The Daemon's output of
/// each is written to `$XSHELL_CONPTY_FIXTURE_OUT` (CI uploads it; `fixtures/prompts/conpty`
/// holds the committed ones).
#[test]
fn conpty_renders_fixtures() {
    let mut names: Vec<String> = fs::read_dir(fixtures())
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "raw").then(|| p.file_stem().unwrap().to_string_lossy().into())
        })
        .collect();
    names.sort();
    let out_dir = std::env::var_os("XSHELL_CONPTY_FIXTURE_OUT").map(PathBuf::from);
    if let Some(d) = &out_dir {
        fs::create_dir_all(d).unwrap();
    }
    let build = windows_build();
    let mut p = Probe::new("vt", &[]);
    let mut bad = Vec::new();
    for name in &names {
        let meta = fixture_meta(name);
        let agent = match meta["agent"].as_str().unwrap() {
            "codex" => HookAgent::Codex,
            _ => HookAgent::Claude,
        };
        let (cols, rows) = (
            meta["cols"].as_u64().unwrap() as u16,
            meta["rows"].as_u64().unwrap() as u16,
        );
        let f = p.open(meta["agent"].as_str().unwrap(), cols, rows);
        p.show(&f, name, None);
        let raw = p.c.output(f.t).to_vec();
        p.c.close(f.t);
        let mut m = ScreenModel::new(cols, rows);
        m.feed(&raw);
        let screen = m.rows();
        let found = extract(agent, &screen);
        let labels = |v: &Value| -> Vec<String> {
            v["expect"]["options"]
                .as_array()
                .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
                .unwrap_or_default()
        };
        let got = json!({
            "composer": composer(agent, &screen),
            "images": composer_images(agent, &screen),
            "options": found.as_ref().map(|f| f.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        });
        let want = json!({
            "composer": meta["composer"],
            "images": meta.get("images").cloned().unwrap_or(Value::Null),
            "options": labels(&meta),
        });
        if got != want {
            bad.push(format!(
                "{name}: got {got} want {want}\n{}",
                screen.join("\n")
            ));
        }
        if let Some(d) = &out_dir {
            fs::write(d.join(format!("{name}.raw")), &raw).unwrap();
            let mut side = meta.clone();
            side["synthetic"] = json!(true);
            side["via"] = json!("conpty");
            side["windowsBuild"] = json!(build);
            side["source"] = json!(format!(
                "{name}.raw drawn by the console fake through ConPTY on Windows build {build} \
                 (crates/xshelld/tests/windows_submit.rs conpty_renders_fixtures), as the Daemon read it"
            ));
            fs::write(
                d.join(format!("{name}.json")),
                serde_json::to_string_pretty(&side).unwrap() + "\n",
            )
            .unwrap();
        }
    }
    fact(
        "W15",
        &format!("fixtures={} mismatches={}", names.len(), bad.len()),
    );
    for b in &bad {
        fact("W15", &format!("mismatch {}", b.lines().next().unwrap()));
    }
    assert!(bad.is_empty(), "{}", bad.join("\n\n"));
}

// ── W17: real agents (HITL) ───────────────────────────────────────────────

/// For a Windows machine with a logged-in agent only (HITL H4): run with
/// `AGENT=claude|codex cargo test -p xshelld --test windows_submit hitl_real_agent_probe --
/// --ignored --nocapture`. It opens the real agent through a Daemon that takes replies (the
/// test hook), waits up to 60 s for its composer, and writes to `$XSHELL_HITL_OUT` (or a
/// temp directory it prints): the output, the screen, the bracketed paste mode and the
/// build; then one two-line reply, its outcome and the screen after.
#[test]
#[ignore]
fn hitl_real_agent_probe() {
    let agent = std::env::var("AGENT").unwrap_or_else(|_| "claude".into());
    let out = std::env::var_os("XSHELL_HITL_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("xshell-hitl-{}", Uuid::new_v4())));
    fs::create_dir_all(&out).unwrap();
    let h = TestHome::new();
    let mut p = Probe::with(h, &[]);
    let dir = std::env::current_dir().unwrap();
    let spec = LaunchSpec {
        agent: (agent != "claude").then(|| agent.clone()),
        ..claude_spec(&dir)
    };
    let (cols, rows) = (100, 30);
    let t = Uuid::new_v4();
    p.c.open_sized(t, spec, cols, rows);
    p.c.attach(t);
    let who = if agent == "codex" {
        HookAgent::Codex
    } else {
        HookAgent::Claude
    };
    let screen = |raw: &[u8]| {
        let mut m = ScreenModel::new(cols, rows);
        m.feed(raw);
        (m.rows(), m.bracketed_paste())
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !composer(who, &screen(p.c.output(t)).0) && Instant::now() < deadline {
        p.c.settle(t, ms(500));
    }
    p.c.settle(t, ms(1500));
    let raw = p.c.output(t).to_vec();
    let (rows_before, paste) = screen(&raw);
    fs::write(out.join(format!("{agent}-composer.raw")), &raw).unwrap();
    let r = p.c.request(&ClientMsg::TermSubmit {
        terminal: t,
        text: "Reply with the single word ok.\nThis is the second line.".into(),
        files: vec![],
    });
    p.c.settle(t, ms(3000));
    let after = p.c.output(t).to_vec();
    fs::write(out.join(format!("{agent}-after.raw")), &after).unwrap();
    let report = json!({
        "agent": agent,
        "build": windows_build(),
        "cols": cols,
        "rows": rows,
        "composer": composer(who, &rows_before),
        "bracketedPaste": paste,
        "screen": rows_before,
        "reply": format!("{r:?}"),
        "screenAfter": screen(&after).0,
    });
    fs::write(
        out.join(format!("{agent}-report.json")),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("HITL dumps in {}", out.display());
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    p.c.close(t);
}

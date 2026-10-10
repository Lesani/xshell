//! The last line of an agent Terminal: the newest user or agent text message of its
//! session, read from the tail of the session file inside the agent's session storage.

use crate::chat::{self, ChatAgent};
use crate::ctx::HostCtx;
use crate::paths::stays_inside;
use crate::{claude, codex};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use xshell_protocol::msg::{LastLine, Speaker, LAST_LINE_MAX_CHARS};
use xshell_protocol::LaunchSpec;

/// How much of a session file's end is read for its last line.
pub(crate) const TAIL_WINDOW: u64 = 256 * 1024;

/// The last line of the session `spec` runs: only for a Claude or Codex agent run directly,
/// on a valid session id, whose session file is a regular file (not a symlink) that resolves
/// inside the agent's session storage (`~/.claude/projects` or `~/.codex/sessions`).
pub fn last_line(ctx: &HostCtx, spec: &LaunchSpec) -> Option<LastLine> {
    let sf = chat::session_file(ctx, spec)?;
    let mut f = chat::open(&sf)?;
    match sf.agent {
        ChatAgent::Claude => claude::last_line_from(&mut f, TAIL_WINDOW),
        ChatAgent::Codex => codex::last_line_from(&mut f, TAIL_WINDOW),
    }
}

/// A test's action between the checks on a path and its open.
#[cfg(test)]
type BeforeOpen = Box<dyn Fn(&Path)>;

#[cfg(test)]
thread_local! {
    /// Test seam: runs between the checks on the path and its open.
    pub(crate) static BEFORE_OPEN: std::cell::RefCell<Option<BeforeOpen>> =
        const { std::cell::RefCell::new(None) };
}

/// `path`, below `root`, opened for reading if it is a regular file, not a symlink, whose
/// real path stays inside `root`.
///
/// The path is checked first (a cheap refusal), then opened without following a final
/// symlink and without blocking (a FIFO swapped in never stalls the reader), and the opened
/// file itself is checked: a regular file that is the same file as the path's canonical form,
/// which must lie inside the canonical `root`. A file swapped for a symlink in between fails
/// the open, and one reached through a swapped ancestor fails the identity or containment
/// check. Left: an ancestor swapped back and forth between the open and the canonical
/// lookup, or a hard link into storage, both of which need write access to the agent's
/// session storage on the Host (the same limit as the Mobile call checks in xshelld's
/// `role.rs`).
pub(crate) fn open_confined(root: &Path, path: &Path) -> Option<File> {
    let plain = path.starts_with(root)
        && path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_file())
        && stays_inside(root, path);
    if !plain {
        return None;
    }
    #[cfg(test)]
    BEFORE_OPEN.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            h(path)
        }
    });
    let f = open_no_follow(path)?;
    let canon = fs::canonicalize(path).ok()?;
    let real_root = fs::canonicalize(root).ok()?;
    if !canon.starts_with(&real_root) {
        return None;
    }
    let same = file_id(&f)? == file_id(&open_no_follow(&canon)?)?;
    same.then_some(f)
}

/// Open `path` for reading if it is a regular file: a final symlink is not followed and
/// the open never blocks (a FIFO).
pub(crate) fn open_no_follow(path: &Path) -> Option<File> {
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // The reparse point itself (a symlink or junction) is opened, never its target,
        // and is then refused for not being a regular file.
        o.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let f = o.open(path).ok()?;
    let m = f.metadata().ok()?;
    (m.file_type().is_file() && !m.file_type().is_symlink()).then_some(f)
}

/// What identifies an open file on its machine: device and inode, or volume serial number
/// and file index.
#[cfg(unix)]
pub(crate) fn file_id(f: &File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = f.metadata().ok()?;
    Some((m.dev(), m.ino()))
}

#[cfg(windows)]
pub(crate) fn file_id(f: &File) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is valid; the handle is open for the
    // duration of the call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) };
    (ok != 0).then(|| {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
}

/// One line of display text: control characters count as whitespace, whitespace runs
/// collapse to one space, the ends are trimmed and the text is cut to
/// [`LAST_LINE_MAX_CHARS`] characters. `None` when nothing is left.
pub fn normalize(text: &str) -> Option<String> {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = String::new();
    for (n, word) in text.split_whitespace().enumerate() {
        if n > 0 {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= LAST_LINE_MAX_CHARS {
            break;
        }
    }
    let out: String = out.chars().take(LAST_LINE_MAX_CHARS).collect();
    let out = out.trim_end().to_string();
    (!out.is_empty()).then_some(out)
}

/// The newest entry in the last `window` bytes of `f` that `pick` turns into non-empty
/// text. The first line is dropped when the window starts mid-file (it may be partial), and
/// lines that are not JSON (a line still being written) are skipped.
pub(crate) fn newest_in_tail(
    f: &mut File,
    window: u64,
    pick: impl Fn(&Value) -> Option<(Speaker, String)>,
) -> Option<LastLine> {
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(window);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(window).read_to_end(&mut buf).ok()?;
    let mut body = &buf[..];
    if start > 0 {
        let nl = body.iter().position(|&b| b == b'\n')?;
        body = &body[nl + 1..];
    }
    body.split(|&b| b == b'\n').rev().find_map(|line| {
        let json: Value = serde_json::from_slice(line).ok()?;
        let (from, text) = pick(&json)?;
        Some(LastLine {
            from,
            text: normalize(&text)?,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::encode_project_name;
    use crate::testutil::Fixture;
    use serde_json::json;

    const CWD: &str = "/work/alpha";

    fn claude_spec(sid: &str) -> LaunchSpec {
        LaunchSpec {
            agent: Some("claude".into()),
            session_id: Some(sid.into()),
            cwd: CWD.into(),
            ..Default::default()
        }
    }

    fn codex_spec(sid: &str) -> LaunchSpec {
        LaunchSpec {
            agent: Some("codex".into()),
            ..claude_spec(sid)
        }
    }

    fn claude_rel(sid: &str) -> String {
        format!(
            "home/.claude/projects/{}/{sid}.jsonl",
            encode_project_name(CWD)
        )
    }

    fn user(text: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": text}})
    }

    fn agent(text: &str) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant",
               "content": [{"type": "text", "text": text}]}})
    }

    fn line(from: Speaker, text: &str) -> Option<LastLine> {
        Some(LastLine {
            from,
            text: text.into(),
        })
    }

    #[test]
    fn claude_last_line_reads_newest_text_message() {
        let fx = Fixture::new();
        let p = fx.write_jsonl(
            claude_rel("s1"),
            &[
                user("first"),
                agent("an answer"),
                json!({"type": "assistant", "message": {"role": "assistant",
                       "content": [{"type": "tool_use", "name": "Bash"}]}}),
                json!({"type": "user", "message": {"role": "user",
                       "content": [{"type": "tool_result", "content": "out"}]}}),
                json!({"type": "summary", "summary": "s"}),
            ],
        );
        assert_eq!(claude::last_line_in(&p), line(Speaker::Agent, "an answer"));
        // A newer prompt wins; a meta entry after it does not count.
        fx.write_jsonl(
            claude_rel("s1"),
            &[
                agent("an answer"),
                user("do it"),
                json!({"type": "user", "isMeta": true,
                       "message": {"role": "user", "content": "caveat"}}),
            ],
        );
        assert_eq!(claude::last_line_in(&p), line(Speaker::User, "do it"));
        let ctx = fx.ctx();
        assert_eq!(
            last_line(&ctx, &claude_spec("s1")),
            line(Speaker::User, "do it")
        );
        assert_eq!(last_line(&ctx, &claude_spec("missing")), None);
    }

    #[test]
    fn claude_last_line_tail_window() {
        let fx = Fixture::new();
        let filler = "x".repeat(400);
        let mut lines = vec![agent("old answer")];
        lines.extend((0..10).map(|_| json!({"type": "progress", "data": filler})));
        lines.push(agent("new answer"));
        let p = fx.write_jsonl(claude_rel("s1"), &lines);
        // The window starts inside a filler line: it is dropped, the newest text is found.
        assert_eq!(
            claude::last_line_in_window(&p, 1000),
            line(Speaker::Agent, "new answer")
        );
        // "old answer" sits outside the window, behind a partial line.
        let mut lines = vec![agent("old answer")];
        lines.extend((0..10).map(|_| json!({"type": "progress", "data": filler})));
        let p = fx.write_jsonl(claude_rel("s2"), &lines);
        assert_eq!(claude::last_line_in_window(&p, 1000), None);
        assert_eq!(claude::last_line_in(&p), line(Speaker::Agent, "old answer"));
        // A trailing line still being written is skipped.
        fx.write(
            claude_rel("s3"),
            format!("{}\n{{\"type\":\"assistant\",\"mess", user("asked")),
        );
        let p = fx
            .home()
            .join(".claude/projects")
            .join(encode_project_name(CWD));
        assert_eq!(
            claude::last_line_in(&p.join("s3.jsonl")),
            line(Speaker::User, "asked")
        );
    }

    #[test]
    fn last_line_normalizes_and_caps() {
        assert_eq!(
            normalize("  one\n\ntwo\tthree  ").as_deref(),
            Some("one two three")
        );
        assert_eq!(normalize(" \n\t "), None);
        assert_eq!(
            normalize("\u{1b}[31mred\u{1b}[0m\u{7}!").as_deref(),
            Some("[31mred [0m !")
        );
        assert_eq!(normalize(""), None);
        let long = "é".repeat(LAST_LINE_MAX_CHARS + 50);
        let n = normalize(&long).unwrap();
        assert_eq!(n.chars().count(), LAST_LINE_MAX_CHARS);
        assert!(n.chars().all(|c| c == 'é'));
        // A cut right after a space leaves no trailing space.
        let spaced = format!("{} b", "a".repeat(LAST_LINE_MAX_CHARS - 1));
        assert_eq!(
            normalize(&spaced).unwrap(),
            "a".repeat(LAST_LINE_MAX_CHARS - 1)
        );
        // A whitespace-only text message is skipped for an older one.
        let fx = Fixture::new();
        let p = fx.write_jsonl(claude_rel("s1"), &[user("line\none"), agent(" \n ")]);
        assert_eq!(claude::last_line_in(&p), line(Speaker::User, "line one"));
    }

    fn codex_rollout(fx: &Fixture, day: &str, sid: &str, lines: &[Value]) -> std::path::PathBuf {
        fx.write_jsonl(
            format!("home/.codex/sessions/{day}/rollout-2026-01-01T10-00-00-{sid}.jsonl"),
            lines,
        )
    }

    fn codex_event(kind: &str, text: &str) -> Value {
        json!({"timestamp": "2026-01-01T10:00:01Z", "type": "event_msg",
               "payload": {"type": kind, "message": text}})
    }

    #[test]
    fn codex_last_line_from_rollout() {
        let fx = Fixture::new();
        let p = codex_rollout(
            &fx,
            "2026/01/01",
            "abc-1",
            &[
                json!({"type": "session_meta", "payload": {"id": "abc-1", "cwd": CWD}}),
                codex_event("user_message", "fix the build"),
                codex_event("agent_message", "Fixed:\n the build"),
                codex_event("token_count", ""),
                json!({"type": "response_item", "payload": {"type": "message"}}),
            ],
        );
        assert_eq!(
            codex::last_line_in(&p),
            line(Speaker::Agent, "Fixed: the build")
        );
        assert_eq!(
            last_line(&fx.ctx(), &codex_spec("abc-1")),
            line(Speaker::Agent, "Fixed: the build")
        );
    }

    #[test]
    fn codex_rollout_for_session_id() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        assert_eq!(codex::rollout_for(&ctx, "abc"), None);
        let old = codex_rollout(&fx, "2026/01/01", "abc", &[]);
        assert_eq!(codex::rollout_for(&ctx, "abc"), Some(old));
        let new = codex_rollout(&fx, "2026/02/01", "abc", &[]);
        codex_rollout(&fx, "2026/03/01", "xabc", &[]);
        codex_rollout(&fx, "2026/03/01", "abc-2", &[]);
        assert_eq!(codex::rollout_for(&ctx, "abc"), Some(new));
        assert_eq!(codex::rollout_for(&ctx, ""), None);
    }

    #[test]
    fn last_line_refuses_bad_id_and_shells() {
        let fx = Fixture::new();
        fx.write_jsonl(claude_rel("s1"), &[user("hi")]);
        let ctx = fx.ctx();
        assert!(last_line(&ctx, &claude_spec("s1")).is_some());
        // `agent: None` runs claude.
        let unset = LaunchSpec {
            agent: None,
            ..claude_spec("s1")
        };
        assert!(last_line(&ctx, &unset).is_some());
        for s in [
            claude_spec("../s1"),
            claude_spec(""),
            LaunchSpec {
                session_id: None,
                ..claude_spec("s1")
            },
            LaunchSpec {
                shell_mode: Some("raw".into()),
                ..claude_spec("s1")
            },
            LaunchSpec {
                launch_prefix: Some(vec!["env".into()]),
                ..claude_spec("s1")
            },
            LaunchSpec {
                shell_command: Some("bash".into()),
                ..claude_spec("s1")
            },
            LaunchSpec {
                agent: Some("cursor".into()),
                ..claude_spec("s1")
            },
            LaunchSpec {
                cwd: String::new(),
                ..claude_spec("s1")
            },
        ] {
            assert_eq!(last_line(&ctx, &s), None, "{s:?}");
        }
        let no_home = HostCtx {
            home: None,
            temp_dir: fx.dir.path().into(),
        };
        assert_eq!(last_line(&no_home, &claude_spec("s1")), None);
    }

    #[cfg(unix)]
    #[test]
    fn last_line_refuses_symlinks() {
        use std::os::unix::fs::symlink;
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let outside = fx.write_jsonl("elsewhere/secret.jsonl", &[user("secret")]);
        let outside_dir = outside.parent().unwrap().to_path_buf();
        let projects = fx.home().join(".claude/projects");
        let proj = projects.join(encode_project_name(CWD));
        fs::create_dir_all(&proj).unwrap();
        // A symlinked session file.
        symlink(&outside, proj.join("s1.jsonl")).unwrap();
        assert_eq!(last_line(&ctx, &claude_spec("s1")), None);
        // A symlink inside storage, to a file inside storage, is still refused.
        let real = fx.write_jsonl(claude_rel("real"), &[user("inside")]);
        symlink(&real, proj.join("s2.jsonl")).unwrap();
        assert_eq!(last_line(&ctx, &claude_spec("s2")), None);
        assert!(last_line(&ctx, &claude_spec("real")).is_some());
        // A symlinked Project directory pointing outside storage.
        fs::remove_dir_all(&proj).unwrap();
        fs::write(
            outside_dir.join("s3.jsonl"),
            format!("{}\n", user("secret")),
        )
        .unwrap();
        symlink(&outside_dir, &proj).unwrap();
        assert_eq!(last_line(&ctx, &claude_spec("s3")), None);
        // The same for Codex: a symlinked rollout, then a symlinked date directory.
        let sessions = fx.home().join(".codex/sessions");
        let codex_day = sessions.join("2026/01");
        fs::create_dir_all(&codex_day).unwrap();
        let rollout = |sid: &str| format!("rollout-2026-01-01T10-00-00-{sid}.jsonl");
        fs::write(
            outside_dir.join(rollout("c1")),
            format!("{}\n", codex_event("agent_message", "secret")),
        )
        .unwrap();
        let linked = codex_day.join(rollout("c1"));
        symlink(outside_dir.join(rollout("c1")), &linked).unwrap();
        assert_eq!(codex::rollout_for(&ctx, "c1"), Some(linked.clone()));
        assert_eq!(last_line(&ctx, &codex_spec("c1")), None);
        fs::remove_file(&linked).unwrap();
        symlink(&outside_dir, codex_day.join("02")).unwrap();
        assert_eq!(last_line(&ctx, &codex_spec("c1")), None);
        // Core's rollout walk does not enter the symlinked directory; the confinement check
        // refuses its file even when it is named directly.
        let through = codex_day.join("02").join(rollout("c1"));
        assert!(through.symlink_metadata().unwrap().is_file());
        assert!(open_confined(&sessions, &through).is_none());
        // The control: a plain rollout inside storage is read.
        codex_rollout(
            &fx,
            "2026/01/03",
            "c2",
            &[codex_event("agent_message", "inside")],
        );
        assert_eq!(
            last_line(&ctx, &codex_spec("c2")),
            line(Speaker::Agent, "inside")
        );
    }

    #[cfg(unix)]
    fn mkfifo(p: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// Run `f` between the path checks and the open of the next confined read on this
    /// thread.
    #[cfg(unix)]
    fn before_open(f: impl Fn(&Path) + 'static) {
        BEFORE_OPEN.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }

    #[cfg(unix)]
    fn no_hook() {
        BEFORE_OPEN.with(|h| *h.borrow_mut() = None);
    }

    #[cfg(unix)]
    #[test]
    fn last_line_fifo_returns_none_promptly() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let proj = fx
            .home()
            .join(".claude/projects")
            .join(encode_project_name(CWD));
        fs::create_dir_all(&proj).unwrap();
        // A FIFO in place of the session file: refused by the path check.
        mkfifo(&proj.join("f1.jsonl"));
        let t = std::time::Instant::now();
        assert_eq!(last_line(&ctx, &claude_spec("f1")), None);
        // Swapped in after the path check: the open does not block, the handle is refused.
        fx.write_jsonl(claude_rel("f2"), &[user("plain")]);
        let target = proj.join("f2.jsonl");
        before_open(move |p| {
            assert_eq!(p, target);
            fs::remove_file(p).unwrap();
            mkfifo(p);
        });
        assert_eq!(last_line(&ctx, &claude_spec("f2")), None);
        no_hook();
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
        // The public helpers refuse a FIFO the same way.
        assert_eq!(claude::last_line_in(&proj.join("f1.jsonl")), None);
    }

    #[cfg(unix)]
    #[test]
    fn last_line_swap_between_check_and_open() {
        use std::os::unix::fs::symlink;
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let outside = fx.write_jsonl("elsewhere/s1.jsonl", &[user("secret")]);
        let outside_dir = outside.parent().unwrap().to_path_buf();
        let projects = fx.home().join(".claude/projects");
        let proj = projects.join(encode_project_name(CWD));
        fx.write_jsonl(claude_rel("s1"), &[user("inside")]);
        assert_eq!(
            last_line(&ctx, &claude_spec("s1")),
            line(Speaker::User, "inside")
        );
        // The file becomes a symlink to an outside file: the open does not follow it.
        let o = outside.clone();
        before_open(move |p| {
            fs::remove_file(p).unwrap();
            symlink(&o, p).unwrap();
        });
        assert_eq!(last_line(&ctx, &claude_spec("s1")), None);
        no_hook();
        // The Project directory becomes a symlink to an outside directory holding a regular
        // file of the same name: the open follows the ancestor, the checks on the opened
        // file refuse it.
        fs::remove_file(proj.join("s1.jsonl")).unwrap();
        fx.write_jsonl(claude_rel("s1"), &[user("inside")]);
        let (p2, od) = (proj.clone(), outside_dir.clone());
        before_open(move |_| {
            fs::rename(&p2, p2.with_extension("moved")).unwrap();
            symlink(&od, &p2).unwrap();
        });
        assert_eq!(last_line(&ctx, &claude_spec("s1")), None);
        no_hook();
    }
}

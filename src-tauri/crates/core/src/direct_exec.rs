//! Starting an agent on Windows without `cmd.exe`, for a new chat's first message.
//!
//! A first message must reach the agent as one argv word, unchanged. `cmd.exe` and batch files
//! parse their command line again (`&`, `|`, `%VAR%`, and a newline ends the command), so a
//! launch that carries a first message runs a real executable instead:
//!
//! - `<bin>` is looked up the way `cmd.exe` would, in the `PATH` directories in order, trying
//!   the `PATHEXT` extensions in order in each, but never in the working directory (a Project
//!   could plant `claude.cmd`) and never in a `PATH` entry that is not fully qualified (empty,
//!   relative, `C:dir` or `\dir` entries would resolve against the working directory too).
//! - A first match that is an `.exe` runs directly. A `.com` is refused: the Daemon's launcher
//!   (Rust's `Command`) would try `<name>.com.exe` first.
//! - A first match that is a `.cmd` or `.bat` runs only if it is an npm or pnpm cmd-shim: then
//!   `node.exe` runs the shim's script. Anything else is refused.
//!
//! The search uses the `PATH` and `PATHEXT` a Windows Terminal starts with ([`terminal_env`]),
//! so the agent found is the one the Terminal would have run. The pure parts (the search, the
//! shim parser, the command-line budget) compile and are tested on every OS.

use portable_pty::CommandBuilder;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// A program to run without a shell: `program` (fully qualified, an `.exe`) with
/// `lead_args` before the agent's own arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectExec {
    pub program: PathBuf,
    pub lead_args: Vec<String>,
}

/// `PATHEXT` when the environment has none (Windows' default without the scripting hosts).
pub const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// CreateProcessW's limit for a command line, in UTF-16 units, its terminating NUL included.
pub const COMMAND_LINE_MAX: usize = 32_767;
/// The longest command line a first-message launch may have, in UTF-16 units: the limit with
/// a margin.
pub const COMMAND_LINE_BUDGET: usize = 32_000;
/// What the planner keeps free for the Daemon's launcher in front of the agent's command line
/// (`"<xshelld.exe>" job-exec Local\xshelld-term-<pid>-<id> -- `). The Daemon checks the
/// whole command line again before it starts anything.
pub const LAUNCHER_RESERVE: usize = 1_024;

/// The refusal for a first message whose command line does not fit [`COMMAND_LINE_BUDGET`].
pub const TOO_LONG: &str = "a first message is too long for a Windows host";

/// The refusal for a first message on a Daemon with no launcher (an in-process server): without
/// it portable-pty would search the program again, PATHEXT included.
pub const NO_LAUNCHER: &str = "a first message on Windows needs the xshelld launcher";

/// The refusal for an agent that cannot start without `cmd.exe`.
pub fn not_direct(bin: &str) -> String {
    format!("a first message on Windows needs {bin} installed as an .exe or an npm package")
}

/// The refusal for an npm-installed agent with no `node.exe` to run it.
pub fn no_node(bin: &str) -> String {
    format!("a first message on Windows needs node.exe on PATH to start {bin}")
}

// ── The search ──

/// Whether `entry` is a fully qualified Windows path: `X:\…` (or `X:/…`) or a UNC or device
/// path (`\\server\share…`, `\\?\…`, `\\.\…`). `C:dir` (relative to drive C's current
/// directory), `\dir` (relative to the current drive) and relative paths are not.
pub fn windows_fully_qualified(entry: &str) -> bool {
    let b = entry.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    match b {
        [d, b':', s, ..] => d.is_ascii_alphabetic() && sep(*s),
        [a, c, rest @ ..] if sep(*a) && sep(*c) => rest.first().is_some_and(|&r| !sep(r)),
        _ => false,
    }
}

/// Whether the search may probe the `PATH` entry `entry`: on Windows a fully qualified path,
/// elsewhere (tests) an absolute one.
fn qualified(entry: &str) -> bool {
    if cfg!(windows) {
        windows_fully_qualified(entry)
    } else {
        entry.starts_with('/')
    }
}

/// A `PATH` value split into its entries as Windows splits it: on `;`, with `"` quoting a part
/// of an entry that contains `;`; the quotes are removed and empty entries kept.
pub fn split_path_list(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in path.chars() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// The `PATHEXT` extensions in order, lower-cased (`.exe`): entries that do not start with a
/// dot are dropped.
pub fn split_pathext(pathext: &str) -> Vec<String> {
    pathext
        .split(';')
        .map(str::trim)
        .filter(|e| e.len() > 1 && e.starts_with('.') && !e[1..].contains(['.', '\\', '/']))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The search's view of the file system, injected so tests run on any OS.
pub struct Probe<'a> {
    /// Whether a file (not a directory) exists at the path.
    pub is_file: &'a dyn Fn(&Path) -> bool,
    /// A small text file's contents (a cmd-shim), `None` if unreadable.
    pub read: &'a dyn Fn(&Path) -> Option<String>,
}

/// The fully qualified directories of `path_entries`, in order.
fn search_dirs(path_entries: &[String]) -> impl Iterator<Item = &str> {
    path_entries
        .iter()
        .map(|e| e.trim())
        .filter(|e| qualified(e))
}

/// The first `name<ext>` in `path_entries` × `pathext`, as `cmd.exe` finds it minus the
/// working directory.
fn find_on_path(
    name: &str,
    path_entries: &[String],
    pathext: &[String],
    probe: &Probe,
) -> Option<(PathBuf, String)> {
    for dir in search_dirs(path_entries) {
        for ext in pathext {
            let p = Path::new(dir).join(format!("{name}{ext}"));
            if (probe.is_file)(&p) {
                return Some((p, ext.clone()));
            }
        }
    }
    None
}

/// The first `node.exe` in the fully qualified `path_entries`: never `node.cmd` or another
/// batch file.
fn find_node_exe(path_entries: &[String], probe: &Probe) -> Option<PathBuf> {
    search_dirs(path_entries)
        .map(|dir| Path::new(dir).join("node.exe"))
        .find(|p| (probe.is_file)(p))
}

/// What `bin` (a name without extension) runs as without `cmd.exe`, given the Terminal's
/// `PATH` entries and `PATHEXT` extensions (lower case, see [`split_pathext`]). Refused when
/// the first match is not an `.exe` or a recognised npm or pnpm cmd-shim.
pub fn resolve_direct(
    bin: &str,
    path_entries: &[String],
    pathext: &[String],
    probe: &Probe,
) -> Result<DirectExec, String> {
    let (found, ext) =
        find_on_path(bin, path_entries, pathext, probe).ok_or_else(|| not_direct(bin))?;
    match ext.as_str() {
        ".exe" => Ok(DirectExec {
            program: found,
            lead_args: Vec::new(),
        }),
        ".cmd" | ".bat" => {
            let dir = found.parent().ok_or_else(|| not_direct(bin))?;
            let text = (probe.read)(&found).ok_or_else(|| not_direct(bin))?;
            let shim = parse_npm_shim(&text, dir).ok_or_else(|| not_direct(bin))?;
            if !(probe.is_file)(&shim) {
                return Err(not_direct(bin));
            }
            let script = shim.to_str().ok_or_else(|| not_direct(bin))?.to_string();
            // The shim's own choice: `node.exe` next to it, else `node` on PATH, where only a
            // real `node.exe` is accepted.
            let local = dir.join("node.exe");
            let node = if (probe.is_file)(&local) {
                local
            } else {
                find_node_exe(path_entries, probe).ok_or_else(|| no_node(bin))?
            };
            Ok(DirectExec {
                program: node,
                lead_args: vec![script],
            })
        }
        _ => Err(not_direct(bin)),
    }
}

// ── npm and pnpm cmd-shims ──

/// The program tokens a shim's run line starts its script with: npm's `"%_prog%"` (set to
/// `%dp0%\node.exe` if it exists, else `node`), pnpm's `"%~dp0\node.exe"` and `node`.
const SHIM_PROGS: &[&str] = &[
    "\"%_prog%\"",
    "\"%~dp0\\node.exe\"",
    "\"%dp0%\\node.exe\"",
    "node",
];
/// How a shim names its own directory.
const SHIM_DIRS: &[&str] = &["%dp0%\\", "%~dp0\\"];

/// The script an npm (`cmd-shim`) or pnpm (`@zkochan/cmd-shim`) shim runs with node, resolved
/// against `shim_dir`. Every line that passes the shim's arguments on (`%*`) must be a run
/// line `<prog>  "<dir>\<rel>.js" %*`, with `<prog>` one of node's forms and `<dir>` the
/// shim's own directory (npm's line may follow `… &`), and all of them must name the same
/// script. Anything else, a hand-written batch file included, is `None`.
pub fn parse_npm_shim(text: &str, shim_dir: &Path) -> Option<PathBuf> {
    let mut rel: Option<&str> = None;
    for line in text.lines() {
        if !line.contains("%*") {
            continue;
        }
        let r = run_line_script(line)?;
        if rel.is_some_and(|prev| prev != r) {
            return None;
        }
        rel = Some(r);
    }
    let rel = rel?;
    let mut p = shim_dir.to_path_buf();
    for part in rel.split('\\') {
        match part {
            "" | "." => {}
            _ => p.push(part),
        }
    }
    Some(p)
}

/// The script path (relative to the shim's directory) of one run line, or `None`.
fn run_line_script(line: &str) -> Option<&str> {
    let t = line.trim().trim_start_matches('@');
    let rest = t.strip_suffix("%*")?.trim_end();
    let body = rest.strip_suffix('"')?;
    let open = body.rfind('"')?;
    let script = &body[open + 1..];
    let before = body[..open].trim_end();
    let lead = SHIM_PROGS.iter().find_map(|p| before.strip_suffix(p))?;
    let lead = lead.trim_end();
    // npm's line runs after `endLocal & … &`; a `&&` would make it conditional.
    if !(lead.is_empty() || lead.ends_with('&') && !lead.ends_with("&&")) {
        return None;
    }
    let rel = SHIM_DIRS.iter().find_map(|d| script.strip_prefix(d))?;
    let ok_char = |c: char| !c.is_control() && !"%\"!^&|<>:*?".contains(c);
    let is_js = rel.len() > 3 && rel.to_ascii_lowercase().ends_with(".js");
    (is_js && rel.chars().all(ok_char)).then_some(rel)
}

// ── The Terminal's environment ──

/// The `PATH` a Windows Terminal runs with: the Daemon's own entries first, then the
/// registry's entries it lacks (such as a tool installed since the Desktop started).
pub fn merge_path(ours: Option<&str>, registry: Option<&str>) -> Option<String> {
    match (ours, registry) {
        (Some(ours), Some(reg)) => {
            let mut all: Vec<&str> = ours.split(';').filter(|e| !e.is_empty()).collect();
            for e in reg.split(';').filter(|e| !e.is_empty()) {
                if !all.iter().any(|a| a.eq_ignore_ascii_case(e)) {
                    all.push(e);
                }
            }
            Some(all.join(";"))
        }
        (Some(one), None) | (None, Some(one)) => Some(one.to_string()),
        (None, None) => None,
    }
}

/// Puts a Windows Terminal's base environment on `cmd`, a fresh `CommandBuilder::new` (which
/// loaded the registry's environment): every variable of `ours` (the Daemon's environment)
/// over it, then `PATH` as [`merge_path`] makes it. The Terminal spawn and the first-message
/// search both build their environment here, so the search finds what the Terminal runs.
pub fn terminal_env(
    cmd: &mut CommandBuilder,
    ours: impl IntoIterator<Item = (OsString, OsString)>,
) {
    let registry = cmd
        .get_env("PATH")
        .map(|p| p.to_string_lossy().into_owned());
    let mut our_path = None;
    for (k, v) in ours {
        if k.eq_ignore_ascii_case("PATH") {
            our_path = v.to_str().map(str::to_string);
        }
        cmd.env(k, v);
    }
    if let Some(p) = merge_path(our_path.as_deref(), registry.as_deref()) {
        cmd.env("PATH", p);
    }
}

/// [`resolve_direct`] against the `PATH` and `PATHEXT` of `env` (built by [`terminal_env`])
/// and the real file system.
pub fn resolve_direct_in(bin: &str, env: &CommandBuilder) -> Result<DirectExec, String> {
    let get = |k: &str| env.get_env(k).map(|v| v.to_string_lossy().into_owned());
    let path = split_path_list(&get("PATH").unwrap_or_default());
    let pathext = get("PATHEXT").filter(|v| !v.trim().is_empty());
    let pathext = split_pathext(pathext.as_deref().unwrap_or(DEFAULT_PATHEXT));
    let read = |p: &Path| {
        let bytes = std::fs::read(p).ok()?;
        (bytes.len() <= 64 * 1024).then_some(())?;
        String::from_utf8(bytes).ok()
    };
    resolve_direct(
        bin,
        &path,
        &pathext,
        &Probe {
            is_file: &runnable_file,
            read: &read,
        },
    )
}

/// Whether `p` is a file CreateProcess can run as given: a regular file (following links), or
/// an app execution alias (the 0-byte reparse points in `WindowsApps`, whose target cannot be
/// read). A dangling link or any other unreadable entry is not.
pub fn runnable_file(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(m) => m.is_file(),
        Err(_) => is_app_exec_alias(p),
    }
}

/// Whether `p` itself is a reparse point tagged `IO_REPARSE_TAG_APPEXECLINK`.
#[cfg(windows)]
fn is_app_exec_alias(p: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        WIN32_FIND_DATAW,
    };
    const IO_REPARSE_TAG_APPEXECLINK: u32 = 0x8000_001B;
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    // A wildcard would make FindFirstFileW match other files.
    if wide.iter().any(|&c| c == b'*' as u16 || c == b'?' as u16) {
        return false;
    }
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    unsafe { FindClose(h) };
    data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        && data.dwReserved0 == IO_REPARSE_TAG_APPEXECLINK
}

#[cfg(not(windows))]
fn is_app_exec_alias(_: &Path) -> bool {
    false
}

/// What `bin` runs as in a new Windows Terminal, without `cmd.exe` (see the module docs).
pub fn resolve_direct_env(bin: &str) -> Result<DirectExec, String> {
    let mut env = CommandBuilder::new("cmd.exe");
    terminal_env(&mut env, std::env::vars_os());
    resolve_direct_in(bin, &env)
}

// ── The command-line budget ──

/// The length in UTF-16 units of one argument as portable-pty quotes it (MSVC `ArgvQuote`
/// rules). Rust's own `Command`, which the launcher uses, never quotes longer.
fn quoted_units(arg: &str) -> usize {
    let units: Vec<u16> = arg.encode_utf16().collect();
    let special = |c: u16| {
        [b' ', b'\t', b'\n', 0x0b, b'"']
            .iter()
            .any(|&s| c == s as u16)
    };
    if !units.is_empty() && !units.iter().any(|&c| special(c)) {
        return units.len();
    }
    let mut n = 2;
    let mut backslashes = 0;
    for &c in &units {
        if c == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        n += if c == b'"' as u16 {
            backslashes * 2 + 2
        } else {
            backslashes + 1
        };
        backslashes = 0;
    }
    n + backslashes * 2
}

/// The length of the command line CreateProcessW gets for `argv`, in UTF-16 units with the
/// terminating NUL.
pub fn command_line_units<S: AsRef<OsStr>>(argv: &[S]) -> usize {
    let words: usize = argv
        .iter()
        .map(|a| quoted_units(&a.as_ref().to_string_lossy()))
        .sum();
    words + argv.len().saturating_sub(1) + 1
}

/// Whether `argv`'s command line fits [`COMMAND_LINE_BUDGET`].
pub fn fits_command_line<S: AsRef<OsStr>>(argv: &[S]) -> bool {
    command_line_units(argv) <= COMMAND_LINE_BUDGET
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    /// The cmd-shim npm writes for a global `npm i -g @anthropic-ai/claude-code`.
    const NPM_SHIM: &str = "@ECHO off\r\n\
GOTO start\r\n\
:find_dp0\r\n\
SET dp0=%~dp0\r\n\
EXIT /b\r\n\
:start\r\n\
SETLOCAL\r\n\
CALL :find_dp0\r\n\
\r\n\
IF EXIST \"%dp0%\\node.exe\" (\r\n\
  SET \"_prog=%dp0%\\node.exe\"\r\n\
) ELSE (\r\n\
  SET \"_prog=node\"\r\n\
  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
)\r\n\
\r\n\
endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\cli.js\" %*\r\n";

    /// The shim pnpm writes for a global `pnpm add -g @openai/codex`.
    const PNPM_SHIM: &str = "@SETLOCAL\r\n\
@IF NOT DEFINED NODE_PATH (\r\n\
  @SET \"NODE_PATH=C:\\pnpm\\global\\5\\node_modules\"\r\n\
) ELSE (\r\n\
  @SET \"NODE_PATH=C:\\pnpm\\global\\5\\node_modules;%NODE_PATH%\"\r\n\
)\r\n\
@IF EXIST \"%~dp0\\node.exe\" (\r\n\
  \"%~dp0\\node.exe\"  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n\
) ELSE (\r\n\
  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
  node  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n\
)\r\n";

    /// The fake agents of the Windows tests: hand-written batch files.
    const HANDWRITTEN: &str =
        "@echo off\r\necho %*> \"C:\\t\\args.txt\"\r\nping -n 600 127.0.0.1 >NUL\r\n";

    fn exts() -> Vec<String> {
        split_pathext(DEFAULT_PATHEXT)
    }

    struct Tree {
        dir: tempfile::TempDir,
    }

    impl Tree {
        fn new() -> Tree {
            Tree {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn put(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        }
        fn d(&self, rel: &str) -> String {
            let p = self.dir.path().join(rel);
            fs::create_dir_all(&p).unwrap();
            p.to_string_lossy().into_owned()
        }
    }

    /// [`resolve_direct`] over the real file system, recording every path it probes.
    fn resolve(
        bin: &str,
        path: &[String],
        probed: &RefCell<Vec<PathBuf>>,
    ) -> Result<DirectExec, String> {
        let is_file = |p: &Path| {
            probed.borrow_mut().push(p.to_path_buf());
            p.is_file()
        };
        let read = |p: &Path| fs::read_to_string(p).ok();
        resolve_direct(
            bin,
            path,
            &exts(),
            &Probe {
                is_file: &is_file,
                read: &read,
            },
        )
    }

    fn r(bin: &str, path: &[String]) -> Result<DirectExec, String> {
        resolve(bin, path, &RefCell::new(Vec::new()))
    }

    fn exe(p: PathBuf) -> DirectExec {
        DirectExec {
            program: p,
            lead_args: vec![],
        }
    }

    #[test]
    fn exe_wins_over_cmd_in_same_dir_per_pathext() {
        let t = Tree::new();
        let a = t.d("a");
        let e = t.put("a/claude.exe", "");
        t.put("a/claude.cmd", NPM_SHIM);
        assert_eq!(r("claude", std::slice::from_ref(&a)), Ok(exe(e)));
        // PATHEXT's order decides, not a fixed preference: .COM comes before .EXE, and a
        // .com is refused (the launcher would try `claude.com.exe` first).
        t.put("a/claude.com", "");
        assert_eq!(r("claude", &[a]), Err(not_direct("claude")));
    }

    #[test]
    fn earlier_path_dir_wins() {
        let t = Tree::new();
        let (a, b) = (t.d("a"), t.d("b"));
        t.put("a/codex.cmd", HANDWRITTEN);
        let e = t.put("b/codex.exe", "");
        // The first match is a hand-written batch file: refused, not skipped for the .exe.
        assert_eq!(
            r("codex", &[a.clone(), b.clone()]),
            Err(not_direct("codex"))
        );
        assert_eq!(r("codex", &[b, a]), Ok(exe(e)));
    }

    #[test]
    fn not_found_refused() {
        let t = Tree::new();
        assert_eq!(r("claude", &[t.d("a")]), Err(not_direct("claude")));
        assert_eq!(r("claude", &[]), Err(not_direct("claude")));
    }

    #[test]
    fn npm_shim_with_local_node() {
        let t = Tree::new();
        let npm = t.d("npm");
        t.put("npm/claude.cmd", NPM_SHIM);
        let node = t.put("npm/node.exe", "");
        let cli = t.put("npm/node_modules/@anthropic-ai/claude-code/cli.js", "");
        // A node.exe earlier on PATH loses to the shim's own.
        t.put("first/node.exe", "");
        assert_eq!(
            r("claude", &[t.d("first"), npm]),
            Ok(DirectExec {
                program: node,
                lead_args: vec![cli.to_string_lossy().into_owned()],
            })
        );
    }

    #[test]
    fn npm_shim_with_path_node() {
        let t = Tree::new();
        let npm = t.d("npm");
        t.put("npm/claude.cmd", NPM_SHIM);
        let cli = t.put("npm/node_modules/@anthropic-ai/claude-code/cli.js", "");
        let node = t.put("nodejs/node.exe", "");
        let got = r("claude", &[npm.clone(), t.d("nodejs")]).unwrap();
        assert_eq!(got.program, node);
        assert_eq!(got.lead_args, vec![cli.to_string_lossy().into_owned()]);
        // No node.exe anywhere: refused.
        let t2 = Tree::new();
        let npm2 = t2.d("npm");
        t2.put("npm/claude.cmd", NPM_SHIM);
        t2.put("npm/node_modules/@anthropic-ai/claude-code/cli.js", "");
        assert_eq!(r("claude", &[npm2]), Err(no_node("claude")));
    }

    #[test]
    fn pnpm_shim() {
        let t = Tree::new();
        let bin = t.d("pnpm");
        t.put("pnpm/codex.cmd", PNPM_SHIM);
        let js = t.put("pnpm/global/5/node_modules/@openai/codex/bin/codex.js", "");
        let node = t.put("node/node.exe", "");
        let got = r("codex", &[bin, t.d("node")]).unwrap();
        assert_eq!(got.program, node);
        assert_eq!(got.lead_args, vec![js.to_string_lossy().into_owned()]);
    }

    #[test]
    fn shim_script_must_exist() {
        let t = Tree::new();
        let npm = t.d("npm");
        t.put("npm/claude.cmd", NPM_SHIM);
        t.put("npm/node.exe", "");
        assert_eq!(r("claude", &[npm]), Err(not_direct("claude")));
    }

    #[test]
    fn handwritten_cmd_refused() {
        let t = Tree::new();
        let a = t.d("a");
        t.put("a/claude.cmd", HANDWRITTEN);
        t.put("a/node.exe", "");
        assert_eq!(r("claude", &[a]), Err(not_direct("claude")));
        // Shim-like lines that do something else.
        let d = Path::new("/s");
        for text in [
            "",
            "@echo off\r\nnode \"%~dp0\\x.js\"\r\n", // no %*
            "\"%_prog%\" \"%dp0%\\x.js\" %* & echo hi\r\n", // %* not last
            "\"%_prog%\" --inspect \"%dp0%\\x.js\" %*\r\n", // extra node option
            "\"%_prog%\" \"%dp0%\\x.cmd\" %*\r\n",   // not a script
            "\"%_prog%\" \"%dp0%\\%FOO%\\x.js\" %*\r\n", // expansion in path
            "\"%_prog%\" \"C:\\abs\\x.js\" %*\r\n",  // not the shim's dir
            "\"%_prog%\" \"%dp0%\\x.js\" %*\r\necho %*\r\n", // another %* use
            "\"%_prog%\" \"%dp0%\\a.js\" %*\r\n\"%_prog%\" \"%dp0%\\b.js\" %*\r\n", // disagree
            "set X=1 && \"%_prog%\" \"%dp0%\\x.js\" %*\r\n", // not after `&`
            "\"%~dp0\\other.exe\" \"%~dp0\\x.js\" %*\r\n", // not node
        ] {
            assert_eq!(parse_npm_shim(text, d), None, "{text:?}");
        }
        assert_eq!(
            parse_npm_shim("\"%_prog%\"  \"%dp0%\\a\\.\\b.JS\" %*", d),
            Some(Path::new("/s").join("a").join("b.JS"))
        );
    }

    #[test]
    fn bat_and_ps1_refused() {
        let t = Tree::new();
        let a = t.d("a");
        t.put("a/claude.bat", HANDWRITTEN);
        assert_eq!(
            r("claude", std::slice::from_ref(&a)),
            Err(not_direct("claude"))
        );
        // A PATHEXT that lists scripting hosts: a .ps1 or .js match is refused too.
        let b = t.d("b");
        t.put("b/codex.ps1", "Write-Host hi");
        t.put("b/codex.exe", "");
        let ext = split_pathext(".PS1;.COM;.EXE;.JS");
        let is_file = |p: &Path| p.is_file();
        let read = |p: &Path| fs::read_to_string(p).ok();
        let probe = Probe {
            is_file: &is_file,
            read: &read,
        };
        assert_eq!(
            resolve_direct("codex", &[b], &ext, &probe),
            Err(not_direct("codex"))
        );
    }

    #[test]
    fn node_cmd_not_accepted_as_node() {
        let t = Tree::new();
        let npm = t.d("npm");
        t.put("npm/claude.cmd", NPM_SHIM);
        t.put("npm/node_modules/@anthropic-ai/claude-code/cli.js", "");
        t.put("shims/node.cmd", HANDWRITTEN);
        t.put("shims/node.bat", HANDWRITTEN);
        assert_eq!(
            r("claude", &[npm.clone(), t.d("shims")]),
            Err(no_node("claude"))
        );
        let node = t.put("nodejs/node.exe", "");
        let got = r("claude", &[npm, t.d("shims"), t.d("nodejs")]).unwrap();
        assert_eq!(got.program, node);
    }

    /// Empty, relative, drive-relative and root-relative entries are never probed, so nothing
    /// resolves against a working directory (a Project's planted `tools\claude.exe`).
    #[test]
    fn unqualified_path_entries_skipped() {
        let t = Tree::new();
        t.put("project/tools/claude.exe", "");
        t.put("project/claude.cmd", HANDWRITTEN);
        let real = t.put("bin/claude.exe", "");
        // A relative entry would resolve against some working directory; none is ever used.
        let entries: Vec<String> = ["", "tools", ".\\tools", ".", "C:tools", "\\tools", "  "]
            .iter()
            .map(|s| s.to_string())
            .chain([t.d("bin")])
            .collect();
        let probed = RefCell::new(Vec::new());
        assert_eq!(resolve("claude", &entries, &probed), Ok(exe(real)));
        let probed = probed.into_inner();
        assert!(!probed.is_empty());
        for p in &probed {
            assert!(p.is_absolute(), "{p:?}");
            assert!(p.starts_with(t.dir.path().join("bin")), "{p:?}");
        }
        // Only unqualified entries: not found, nothing probed.
        let probed = RefCell::new(Vec::new());
        assert_eq!(
            resolve("claude", &entries[..7], &probed),
            Err(not_direct("claude"))
        );
        assert!(probed.into_inner().is_empty());
    }

    #[test]
    fn windows_fully_qualified_rules() {
        for ok in [
            "C:\\Program Files\\nodejs",
            "c:/tools",
            "Z:\\",
            "\\\\server\\share\\bin",
            "//server/share",
            "\\\\?\\C:\\x",
            "\\\\.\\C:\\x",
        ] {
            assert!(windows_fully_qualified(ok), "{ok}");
        }
        for bad in [
            "",
            "tools",
            ".\\tools",
            "..\\x",
            "C:",
            "C:tools",
            "\\tools",
            "/tools",
            "\\\\",
            "\\\\\\x",
            "%USERPROFILE%\\bin",
            "1:\\x",
            "~\\bin",
        ] {
            assert!(!windows_fully_qualified(bad), "{bad}");
        }
    }

    #[test]
    fn path_list_and_pathext_parsing() {
        assert_eq!(
            split_path_list("C:\\a;;\"C:\\b;c\";rel"),
            vec!["C:\\a", "", "C:\\b;c", "rel"]
        );
        assert_eq!(split_path_list(""), vec![""]);
        assert_eq!(
            split_pathext(".COM;.EXE;;BAT;.CMD; .Js ;.a.b"),
            vec![".com", ".exe", ".cmd", ".js"]
        );
    }

    #[test]
    fn merge_path_ours_first_then_registry() {
        assert_eq!(
            merge_path(Some("C:\\a;;C:\\B"), Some("c:\\b;C:\\reg;")).as_deref(),
            Some("C:\\a;C:\\B;C:\\reg")
        );
        assert_eq!(merge_path(Some("C:\\a"), None).as_deref(), Some("C:\\a"));
        assert_eq!(merge_path(None, Some("C:\\r")).as_deref(), Some("C:\\r"));
        assert_eq!(merge_path(None, None), None);
    }

    /// The search sees the Terminal's PATH: a tool only in the registry's PATH (installed
    /// since the Daemon started) is found, after the Daemon's own entries.
    #[test]
    fn search_uses_the_terminal_env_with_registry_path() {
        let t = Tree::new();
        let (ours, reg) = (t.d("ours"), t.d("reg"));
        let e = t.put("reg/claude.exe", "");
        // A fresh builder stands in for the registry's environment.
        let mut env = CommandBuilder::new("cmd.exe");
        env.env("PATH", &reg);
        env.env_remove("PATHEXT");
        terminal_env(&mut env, [(OsString::from("PATH"), OsString::from(&ours))]);
        assert_eq!(
            env.get_env("PATH").unwrap().to_string_lossy(),
            format!("{ours};{reg}")
        );
        assert_eq!(resolve_direct_in("claude", &env), Ok(exe(e)));
        // The Daemon's own entry wins over the registry's.
        let mine = t.put("ours/claude.exe", "");
        assert_eq!(resolve_direct_in("claude", &env), Ok(exe(mine)));
        // The Terminal's PATHEXT, too.
        terminal_env(
            &mut env,
            [(OsString::from("PATHEXT"), OsString::from(".CMD"))],
        );
        assert_eq!(resolve_direct_in("claude", &env), Err(not_direct("claude")));
    }

    /// A dangling `claude.exe` is not a match: the search goes on to its `.cmd` sibling, which
    /// is refused, so nothing would run a batch file (or the dangling link).
    #[cfg(unix)]
    #[test]
    fn dangling_exe_not_runnable() {
        let t = Tree::new();
        let a = t.d("a");
        std::os::unix::fs::symlink(
            t.dir.path().join("gone.exe"),
            t.dir.path().join("a/claude.exe"),
        )
        .unwrap();
        let dangling = t.dir.path().join("a/claude.exe");
        assert!(std::fs::symlink_metadata(&dangling).is_ok());
        assert!(!runnable_file(&dangling));
        assert!(!runnable_file(&t.dir.path().join("a")));
        t.put("a/claude.cmd", HANDWRITTEN);
        let mut env = CommandBuilder::new("cmd.exe");
        env.env("PATH", &a);
        env.env_remove("PATHEXT");
        assert_eq!(resolve_direct_in("claude", &env), Err(not_direct("claude")));
        // A real file behind the link is runnable.
        let real = t.put("gone.exe", "");
        assert!(runnable_file(&dangling) && runnable_file(&real));
        assert_eq!(resolve_direct_in("claude", &env), Ok(exe(dangling)));
    }

    fn line(argv: &[&str]) -> String {
        // The reference: portable-pty's ArgvQuote, written out.
        let mut s = String::new();
        for (i, a) in argv.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            if !a.is_empty() && !a.contains([' ', '\t', '\n', '\x0b', '"']) {
                s.push_str(a);
                continue;
            }
            s.push('"');
            let mut bs = 0;
            for c in a.chars() {
                if c == '\\' {
                    bs += 1;
                    continue;
                }
                let n = if c == '"' { bs * 2 + 1 } else { bs };
                s.push_str(&"\\".repeat(n));
                s.push(c);
                bs = 0;
            }
            s.push_str(&"\\".repeat(bs * 2));
            s.push('"');
        }
        s
    }

    #[test]
    fn command_line_units_match_argv_quote() {
        let cases: &[&[&str]] = &[
            &["C:\\n\\node.exe", "C:\\x y\\cli.js", "--", "a b"],
            &["p", ""],
            &["p", "a\\\\\"b", "tail\\\\", "x\\ y\\", "\"\"", "é 🚀\nz"],
            &["p", "plain\\path\\"],
        ];
        for argv in cases {
            let want = line(argv).encode_utf16().count() + 1;
            assert_eq!(command_line_units(argv), want, "{argv:?}");
        }
    }

    #[test]
    fn budget_refuses_quote_heavy_max_message() {
        let max = crate::launch::FIRST_MESSAGE_MAX_BYTES;
        let quotes = format!("{} ", "\"".repeat(max - 1));
        assert!(!fits_command_line(&["C:\\n\\node.exe", "--", &quotes]));
        let plain = "x ".repeat(max / 2);
        assert!(fits_command_line(&["C:\\n\\node.exe", "--", &plain]));
        const {
            assert!(
                COMMAND_LINE_BUDGET < COMMAND_LINE_MAX
                    && LAUNCHER_RESERVE < COMMAND_LINE_BUDGET / 16
            )
        };
    }
}

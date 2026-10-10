//! Connection roles: what a connection may do, set by the transport that carried it and never
//! by a message. A Desktop may do anything. A Mobile is a remote control for agents, not a
//! remote shell (ADR-0004): it may open, attach to, type into, resize, close and Relaunch
//! agent Terminals and read session data, but never open a shell Terminal, pass a shell
//! command or launch prefix, use the file, git or host-probe methods of `call`, edit a
//! Terminal's record or upgrade the Daemon.
//!
//! Message-level rules run in [`check`], before anything is looked up or locked. Rules about a
//! Terminal run in [`listed`], on the exact instance the handler then acts on, under the same
//! registry lock as its lookup.
//!
//! A Mobile is never told about a Terminal it may not act on: `terminals` lists and
//! `term.exit` leave them out ([`sees`]).

use super::registry::Registry;
use super::terminal::Terminal;
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use xshell_core::claude::encode_project_name;
use xshell_core::launch::LaunchSpec;
use xshell_core::paths::stays_inside;
use xshell_core::sessions::valid_session_id;
use xshell_core::sessions::CodexProjectInfo;
use xshell_core::{antigravity, claude, codex, cursor, opencode, HostCtx};
use xshell_protocol::msg::ClientMsg;

/// Who is on the other end of a connection. The local socket and `xshelld connect` (SSH)
/// carry Desktops; the Relay will carry Mobiles (and Desktops), mapped from the Roster role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Desktop,
    Mobile,
}

/// The start of every refusal's `err`.
pub(crate) const FORBIDDEN: &str = "forbidden for mobile";

fn forbidden(what: impl std::fmt::Display) -> String {
    format!("{FORBIDDEN}: {what}")
}

/// The `call` methods a Mobile may use: session reads and usage stats, plus pasting a file
/// into the Daemon's drop directory. Each also has its parameters checked in [`check_call`].
pub(crate) const MOBILE_CALLS: &[&str] = &[
    "list_claude_projects",
    "get_sessions",
    "get_all_recent_sessions",
    "get_session_messages",
    "list_project_session_ids",
    "detect_session_branch",
    "list_codex_projects",
    "list_cursor_projects",
    "list_opencode_projects",
    "list_antigravity_projects",
    "probe_statusline_setup",
    "get_global_rate_limits",
    "get_claude_cost_summary",
    "get_codex_usage",
    "save_dropped_file",
];

/// Every core method a Mobile may not call: files, git, files read at a client-chosen path
/// (skills, memories, agent contexts) and host probes.
#[cfg(test)]
pub(crate) const MOBILE_REFUSED_CALLS: &[&str] = &[
    "read_text_file",
    "list_dir",
    "search_dir",
    "get_git_status",
    "get_git_log",
    "git_diff",
    "git_stage",
    "git_unstage",
    "git_discard",
    "list_git_branches",
    "git_checkout",
    "get_project_skills",
    "get_project_memories",
    "get_codex_context",
    "get_cursor_context",
    "get_opencode_context",
    "get_antigravity_context",
    "get_username",
    "get_home_dir",
    "detect_agent_binary",
];

/// Whether `cwd` is a Project the Host knows from any agent's session history. Paths compare
/// after resolving symlinks, falling back to the exact string.
pub(crate) fn known_project(ctx: &HostCtx, cwd: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let want = fs::canonicalize(cwd).ok();
    let same = |p: &String| *p == cwd || want.is_some() && fs::canonicalize(p).ok() == want;
    let paths = |v: Vec<CodexProjectInfo>| v.into_iter().map(|p| p.path).collect::<Vec<_>>();
    // Claude first, and each agent's history only read if the ones before it missed.
    let agents: [&dyn Fn() -> Vec<String>; 5] = [
        &|| {
            claude::list_claude_projects(ctx)
                .into_iter()
                .map(|p| p.path)
                .collect()
        },
        &|| paths(codex::list_codex_projects(ctx)),
        &|| paths(cursor::list_cursor_projects(ctx)),
        &|| paths(opencode::list_opencode_projects(ctx)),
        &|| paths(antigravity::list_antigravity_projects(ctx)),
    ];
    agents.iter().any(|list| list().iter().any(same))
}

/// A Mobile's `term.open`: Claude or Codex, run directly, in a known Project, on a session id
/// that is safe to pass on. `skipPermissions` is allowed (ADR-0004).
pub(crate) fn check_open(ctx: &HostCtx, spec: &LaunchSpec) -> Result<(), String> {
    if !spec.is_direct_agent() {
        return Err(forbidden("term.open of a shell or a wrapped agent"));
    }
    if !matches!(spec.agent.as_deref(), Some("claude" | "codex")) {
        return Err(forbidden(format!(
            "term.open of agent {}",
            spec.agent.as_deref().unwrap_or("(none)")
        )));
    }
    check_session_id(spec)?;
    if !known_project(ctx, &spec.cwd) {
        return Err(forbidden("term.open outside a known Project"));
    }
    Ok(())
}

fn check_session_id(spec: &LaunchSpec) -> Result<(), String> {
    match spec.session_id.as_deref() {
        Some(s) if !valid_session_id(s) => Err(forbidden("session id")),
        _ => Ok(()),
    }
}

/// A Mobile's Relaunch, on the spec it would start: the Terminal's current one, which a
/// Desktop's `term.update` may have changed since it opened. A Relaunch only changes
/// `skipPermissions`; one that ever changes more must go through [`check_open`] instead.
pub(crate) fn check_relaunch(role: Role, spec: &LaunchSpec) -> Result<(), String> {
    if role == Role::Desktop {
        return Ok(());
    }
    if !spec.is_direct_agent() {
        return Err(forbidden("relaunch of a shell or a wrapped agent"));
    }
    check_session_id(spec)
}

/// Whether `role` is told about, and may act on, a Terminal running `spec`. A Mobile sees
/// only direct agent Terminals (ADR-0004); a Desktop sees all. Visibility must never change
/// during a listed Terminal's life (`term.update` and Relaunch keep `is_direct_agent`):
/// output subscriptions are checked only on attach.
pub(crate) fn sees(role: Role, spec: &LaunchSpec) -> bool {
    role == Role::Desktop || spec.is_direct_agent()
}

/// The Terminal listed under `id`, if `role` may act on it. Lookup and check are one step on
/// one registry lock, so the check always covers the instance the caller acts on: a
/// Terminal listed later under the same UUID is checked anew.
pub(crate) fn listed<'a>(
    reg: &'a Registry,
    role: Role,
    id: &Uuid,
) -> Result<&'a Arc<Terminal>, String> {
    let t = reg
        .terminals
        .get(id)
        .ok_or_else(|| format!("unknown terminal {id}"))?;
    if !sees(role, &t.spec()) {
        return Err(forbidden("a shell or a wrapped agent Terminal"));
    }
    Ok(t)
}

/// Whether `role` may send `msg`, judged from the message alone. Messages about an existing
/// Terminal are judged in [`listed`] instead. No `_` arm: a new message must be classified.
pub(crate) fn check(role: Role, ctx: &HostCtx, msg: &ClientMsg) -> Result<(), String> {
    if role == Role::Desktop {
        return Ok(());
    }
    match msg {
        // Answered "already said hello"; the role stays.
        ClientMsg::Hello(_) => Ok(()),
        ClientMsg::Call { method, params } => check_call(ctx, method, params),
        ClientMsg::TermOpen { spec } => check_open(ctx, &spec.launch),
        ClientMsg::TermAttach { .. }
        | ClientMsg::TermDetach { .. }
        | ClientMsg::TermInput { .. }
        | ClientMsg::TermResize { .. }
        | ClientMsg::TermClose { .. }
        | ClientMsg::TermRelaunch { .. } => Ok(()),
        // Desktop bookkeeping; a session id set here would reach a later Relaunch.
        ClientMsg::TermUpdate { .. } => Err(forbidden("term.update")),
        ClientMsg::DaemonUpgrade => Err(forbidden("daemon.upgrade")),
        // Agent hooks report from the Host itself, over the local socket, never from a phone.
        ClientMsg::TermEvent { .. } => Err(forbidden("term.event")),
        // Ring membership is the Desktop's to manage.
        ClientMsg::RingIdentity => Err(forbidden("ring.identity")),
        ClientMsg::RingJoin { .. } => Err(forbidden("ring.join")),
        // A Mobile's own push registration; the handler refuses anyone else.
        ClientMsg::PushRegister { .. } | ClientMsg::PushUnregister => Ok(()),
    }
}

/// A Mobile's `call`: the method must be in [`MOBILE_CALLS`], and every parameter that
/// becomes a path must stay inside the agent's session storage or name a known Project.
fn check_call(ctx: &HostCtx, method: &str, params: &Value) -> Result<(), String> {
    let refuse = || forbidden(format!("call {method}"));
    if !MOBILE_CALLS.contains(&method) {
        return Err(refuse());
    }
    let arg = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| forbidden(format!("call {method} without {key}")))
    };
    match method {
        "list_claude_projects"
        | "list_codex_projects"
        | "list_cursor_projects"
        | "list_opencode_projects"
        | "list_antigravity_projects"
        | "probe_statusline_setup"
        | "get_global_rate_limits"
        | "get_claude_cost_summary"
        | "get_codex_usage" => Ok(()),
        "get_all_recent_sessions" => every_project_plain(ctx, method),
        "get_sessions" => {
            let dir = in_claude_projects(ctx, method, &[arg("encodedName")?])?;
            plain_sessions(method, dir.as_deref())
        }
        "get_session_messages" => {
            let file = format!("{}.jsonl", arg("sessionId")?);
            in_claude_projects(ctx, method, &[arg("encodedName")?, &file]).map(drop)
        }
        "list_project_session_ids" | "detect_session_branch" => {
            let cwd = arg("cwd")?;
            if !known_project(ctx, cwd) {
                return Err(forbidden(format!("call {method} outside a known Project")));
            }
            let dir = in_claude_projects(ctx, method, &[&encode_project_name(cwd)])?;
            // Listing ids reads only names; branch detection reads the files.
            match method {
                "detect_session_branch" => plain_sessions(method, dir.as_deref()),
                _ => Ok(()),
            }
        }
        // Always written to the Daemon's drop directory; the name only ends the file name.
        "save_dropped_file" => match arg("name")? {
            "" => Ok(()),
            n if single_component(n) => Ok(()),
            _ => Err(forbidden(format!("call {method} with a path as name"))),
        },
        _ => Err(refuse()),
    }
}

/// One plain path component: no separator, no `.` or `..`, not absolute, no NUL.
fn single_component(s: &str) -> bool {
    !s.contains(['/', '\\', '\0'])
        && matches!(
            Path::new(s).components().collect::<Vec<_>>()[..],
            [Component::Normal(_)]
        )
}

fn outside_storage(method: &str) -> String {
    forbidden(format!("call {method} outside session storage"))
}

/// `~/.claude/projects`, where Claude Code keeps one directory per Project.
fn claude_root(ctx: &HostCtx) -> Option<PathBuf> {
    ctx.home().map(|h| h.join(".claude").join("projects"))
}

// These checks look before core reads, so a symlink swapped in between is followed. Doing
// that needs write access to the agent's session storage on the Host, which already gives
// more than a Mobile could get through it.

/// `parts` joined under `~/.claude/projects`, if they are plain components and the path
/// resolves (symlinks included) inside it. `None` when there is no home: core reads nothing.
fn in_claude_projects(
    ctx: &HostCtx,
    method: &str,
    parts: &[&str],
) -> Result<Option<PathBuf>, String> {
    if !parts.iter().all(|p| single_component(p)) {
        return Err(outside_storage(method));
    }
    let Some(root) = claude_root(ctx) else {
        return Ok(None);
    };
    let path: PathBuf = parts.iter().fold(root.clone(), |p, c| p.join(c));
    if stays_inside(&root, &path) {
        Ok(Some(path))
    } else {
        Err(outside_storage(method))
    }
}

/// Core reads every `*.jsonl` entry of a Claude Project directory, following symlinks:
/// none of them may be one. A missing directory has none.
fn plain_sessions(method: &str, dir: Option<&Path>) -> Result<(), String> {
    let Some(dir) = dir else {
        return Ok(());
    };
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(outside_storage(method)),
    };
    for e in entries {
        let plain = e.is_ok_and(|e| {
            e.path().extension().is_none_or(|x| x != "jsonl")
                || e.file_type().is_ok_and(|t| !t.is_symlink())
        });
        if !plain {
            return Err(outside_storage(method));
        }
    }
    Ok(())
}

/// [`plain_sessions`] for every Project directory core reads across all Projects: the real
/// directories in `~/.claude/projects` (core skips symlinked ones).
fn every_project_plain(ctx: &HostCtx, method: &str) -> Result<(), String> {
    let Some(root) = claude_root(ctx) else {
        return Ok(());
    };
    let entries = match fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(outside_storage(method)),
    };
    for e in entries {
        let e = e.map_err(|_| outside_storage(method))?;
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            plain_sessions(method, Some(&e.path()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::registry::Daemon;
    use super::super::terminal;
    use super::super::Config;
    use super::*;
    use serde_json::{json, Map};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Condvar, Mutex};
    use xshell_core::terminal::state::PersistedTerminal;

    fn agent(cwd: &str) -> LaunchSpec {
        LaunchSpec {
            agent: Some("claude".into()),
            shell_mode: Some("claude".into()),
            cwd: cwd.into(),
            ..Default::default()
        }
    }

    fn raw_shell(cwd: &str) -> LaunchSpec {
        LaunchSpec {
            shell_mode: Some("raw".into()),
            shell_command: Some("/bin/sh".into()),
            cwd: cwd.into(),
            ..Default::default()
        }
    }

    /// A temp home with Claude history in `claude_cwd` and Codex history in `codex_cwd`.
    fn history(claude_cwd: &Path, codex_cwd: &Path) -> (tempfile::TempDir, HostCtx) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = claude_cwd.to_string_lossy();
        let p = home
            .join(".claude/projects")
            .join(encode_project_name(&cwd));
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("s1.jsonl"), json!({ "cwd": cwd }).to_string() + "\n").unwrap();
        let c = home.join(".codex/sessions/2026/01/01");
        fs::create_dir_all(&c).unwrap();
        let first = json!({ "type": "session_meta", "payload": { "cwd": codex_cwd } });
        fs::write(c.join("rollout-1.jsonl"), first.to_string() + "\n").unwrap();
        let ctx = HostCtx::with_home(home, dir.path().join("tmp"));
        (dir, ctx)
    }

    #[test]
    fn sees_rules() {
        let a = agent("/w");
        let wrapped = LaunchSpec {
            shell_command: Some("/bin/sh".into()),
            shell_id: Some("bash".into()),
            ..agent("/w")
        };
        let prefixed = LaunchSpec {
            launch_prefix: Some(vec!["bash".into(), "-c".into(), "exec sh -i".into()]),
            ..agent("/w")
        };
        for s in [&a, &raw_shell("/w"), &wrapped, &prefixed] {
            assert!(sees(Role::Desktop, s));
        }
        assert!(sees(Role::Mobile, &a));
        for s in [&raw_shell("/w"), &wrapped, &prefixed] {
            assert!(!sees(Role::Mobile, s));
        }
    }

    #[test]
    fn every_core_method_classified() {
        let allowed: HashSet<_> = MOBILE_CALLS.iter().collect();
        let refused: HashSet<_> = MOBILE_REFUSED_CALLS.iter().collect();
        assert!(allowed.is_disjoint(&refused));
        let all: HashSet<_> = xshell_core::METHODS.iter().collect();
        let classified: HashSet<_> = allowed.union(&refused).copied().collect();
        assert_eq!(classified, all);
        assert_eq!(MOBILE_CALLS.len() + MOBILE_REFUSED_CALLS.len(), all.len());
    }

    #[test]
    fn every_allowed_call_has_a_parameter_rule() {
        // A method in MOBILE_CALLS without its own arm falls through to a refusal.
        let ctx = HostCtx::with_home("/nonexistent-home", "/tmp");
        let p = json!({ "encodedName": "x", "sessionId": "s", "limit": 1, "name": "a.png",
                        "cwd": "", "currentSessionId": "s", "knownSessionIds": [] });
        for m in MOBILE_CALLS {
            let r = check_call(&ctx, m, &p);
            // The cwd methods need a known Project; everything else passes.
            if matches!(*m, "list_project_session_ids" | "detect_session_branch") {
                assert!(r.unwrap_err().contains("known Project"), "{m}");
            } else {
                assert_eq!(r, Ok(()), "{m}");
            }
        }
        for m in MOBILE_REFUSED_CALLS.iter().chain(&["no_such_method"]) {
            assert_eq!(
                check_call(&ctx, m, &p),
                Err(format!("{FORBIDDEN}: call {m}"))
            );
        }
    }

    #[test]
    fn open_rules_reject_prefix_shell_id_agent() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let (_h, ctx) = history(&cwd, Path::new("/nowhere"));
        let ok = agent(&cwd.to_string_lossy());
        assert_eq!(check_open(&ctx, &ok), Ok(()));
        assert_eq!(
            check_open(
                &ctx,
                &LaunchSpec {
                    agent: Some("codex".into()),
                    ..ok.clone()
                }
            ),
            Ok(())
        );
        assert_eq!(
            check_open(
                &ctx,
                &LaunchSpec {
                    skip_permissions: Some(true),
                    ..ok.clone()
                }
            ),
            Ok(())
        );
        for s in [
            LaunchSpec {
                launch_prefix: Some(vec!["env".into()]),
                ..ok.clone()
            },
            LaunchSpec {
                shell_id: Some("bash".into()),
                ..ok.clone()
            },
            LaunchSpec {
                shell_command: Some("bash".into()),
                ..ok.clone()
            },
            LaunchSpec {
                agent: Some("cursor".into()),
                ..ok.clone()
            },
            LaunchSpec {
                agent: None,
                ..ok.clone()
            },
            LaunchSpec {
                session_id: Some("-cx".into()),
                ..ok.clone()
            },
            LaunchSpec {
                cwd: String::new(),
                ..ok.clone()
            },
            LaunchSpec {
                cwd: "/elsewhere".into(),
                ..ok.clone()
            },
            raw_shell(&cwd.to_string_lossy()),
        ] {
            let e = check_open(&ctx, &s).unwrap_err();
            assert!(e.starts_with(FORBIDDEN), "{s:?}: {e}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn known_project_canonical_match() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let (a, b) = (real.join("a"), real.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        std::os::unix::fs::symlink(&a, real.join("link-a")).unwrap();
        let (_h, ctx) = history(&real.join("link-a"), &b);
        let s = |p: &Path| p.to_string_lossy().into_owned();
        // Claude history names the symlink; the real path matches it, and vice versa.
        assert!(known_project(&ctx, &s(&a)));
        assert!(known_project(&ctx, &s(&real.join("link-a"))));
        assert!(known_project(&ctx, &s(&b)));
        assert!(known_project(&ctx, &format!("{}/", s(&b))));
        assert!(!known_project(&ctx, &s(&real)));
        assert!(!known_project(&ctx, ""));
        // A path that no longer exists still matches its exact string.
        let gone = real.join("gone");
        let (_h2, ctx2) = history(&gone, Path::new("/nowhere"));
        assert!(known_project(&ctx2, &s(&gone)));
        assert!(!known_project(&ctx2, &format!("{}x", s(&gone))));
    }

    #[cfg(unix)]
    #[test]
    fn session_storage_containment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cwd = root.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let (_h, ctx) = history(&cwd, Path::new("/nowhere"));
        let enc = encode_project_name(&cwd.to_string_lossy());
        let projects = ctx.home().unwrap().join(".claude/projects");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x.jsonl"), "{}\n").unwrap();
        std::os::unix::fs::symlink(&outside, projects.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("x.jsonl"), projects.join(&enc).join("y.jsonl"))
            .unwrap();

        let msgs = |e: &str, s: &str| {
            check_call(
                &ctx,
                "get_session_messages",
                &json!({ "encodedName": e, "sessionId": s, "limit": 5 }),
            )
        };
        assert_eq!(msgs(&enc, "s1"), Ok(()));
        assert_eq!(msgs(&enc, "missing"), Ok(()));
        for (e, s) in [
            (&*outside.to_string_lossy(), "x"),
            ("..", "x"),
            ("../../outside", "x"),
            (&*enc, "../../../outside/x"),
            (&*enc, "/abs"),
            ("escape", "x"),
            // An escaping parent with nothing at the leaf.
            ("escape", "missing"),
            (&*enc, "y"),
            ("", "s1"),
        ] {
            let r = msgs(e, s).unwrap_err();
            assert!(r.starts_with(FORBIDDEN), "{e} {s}: {r}");
        }
        let sessions = |e: &str| check_call(&ctx, "get_sessions", &json!({ "encodedName": e }));
        assert!(sessions("escape").is_err());
        assert!(sessions("a/b").is_err());
        assert!(check_call(&ctx, "get_sessions", &json!({})).is_err());
        let ids = |c: &Path| check_call(&ctx, "list_project_session_ids", &json!({ "cwd": c }));
        assert_eq!(ids(&cwd), Ok(()));
        assert!(ids(&outside).is_err());
        let branch = || {
            check_call(
                &ctx,
                "detect_session_branch",
                &json!({ "cwd": cwd, "currentSessionId": "s1", "knownSessionIds": [] }),
            )
        };
        let recent = || check_call(&ctx, "get_all_recent_sessions", &json!({ "limit": 5 }));
        // The symlinked y.jsonl in the Project directory: every read of the files refuses.
        assert!(sessions(&enc).is_err());
        assert!(branch().is_err());
        assert!(recent().is_err());
        fs::remove_file(projects.join(&enc).join("y.jsonl")).unwrap();
        assert_eq!(sessions(&enc), Ok(()));
        assert_eq!(branch(), Ok(()));
        assert_eq!(recent(), Ok(()));
        // A symlinked Project directory is skipped by core's bulk read, so it passes there.
        assert!(projects.join("escape").is_symlink());

        // Only "not found" counts as absent: a path through a file refuses.
        let s1 = projects.join(&enc).join("s1.jsonl");
        assert!(!stays_inside(&projects, &s1.join("x")));
        // No storage at all: nothing to escape to.
        assert!(stays_inside(&root.join("none"), &root.join("none/a/b")));
        let drop = |n: &str| {
            check_call(
                &ctx,
                "save_dropped_file",
                &json!({ "bytesBase64": "", "name": n }),
            )
        };
        assert_eq!(drop("shot.png"), Ok(()));
        assert_eq!(drop(""), Ok(()));
        for n in ["../x", "/etc/x", "a\\b", "..", "."] {
            assert!(drop(n).is_err(), "{n}");
        }
    }

    #[test]
    fn only_messages_about_a_terminal_wait_for_lookup() {
        let ctx = HostCtx::with_home("/nonexistent-home", "/tmp");
        let t = Uuid::new_v4();
        let upd = ClientMsg::TermUpdate {
            terminal: t,
            session_id: None,
            meta: None,
        };
        let join = ClientMsg::RingJoin {
            rosters: vec![],
            expect: None,
        };
        for m in [
            &upd,
            &ClientMsg::DaemonUpgrade,
            &ClientMsg::RingIdentity,
            &join,
        ] {
            assert!(check(Role::Mobile, &ctx, m)
                .unwrap_err()
                .starts_with(FORBIDDEN));
            assert_eq!(check(Role::Desktop, &ctx, m), Ok(()));
        }
        assert_eq!(
            check(Role::Mobile, &ctx, &ClientMsg::TermClose { terminal: t }),
            Ok(())
        );
        // A Mobile's own push registration passes; the handler refuses everyone else.
        let reg = ClientMsg::PushRegister {
            blob: "xpb1.a.b".into(),
            seal_key: "k".into(),
            triggers: xshell_protocol::msg::PushTriggers {
                needs_you: true,
                finished: true,
            },
        };
        for m in [&reg, &ClientMsg::PushUnregister] {
            assert_eq!(check(Role::Mobile, &ctx, m), Ok(()));
        }
    }

    pub(crate) fn daemon(dir: &Path) -> Arc<Daemon> {
        let paths = crate::paths::resolve(dir, None, None);
        Arc::new(Daemon {
            cfg: Config::new(dir.into(), paths),
            ctx: Arc::new(HostCtx::with_home(dir, dir)),
            reg: Mutex::new(Registry::default()),
            exiting: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            exit: Mutex::new(None),
            exit_cv: Condvar::new(),
            lock_file: Mutex::new(None),
            next_conn: AtomicU64::new(1),
            hooks: None,
            next_run: AtomicU64::new(1),
            escalations: Default::default(),
            ring: crate::server::ring::Ring::new(
                dir.join("ring"),
                std::time::Duration::from_secs(1),
                Default::default(),
                std::time::Duration::from_secs(60),
            ),
            push: Arc::new(crate::server::push::Push::new(
                dir.join("ring"),
                crate::server::push::PushConfig {
                    window: std::time::Duration::from_secs(10),
                    timeout: std::time::Duration::from_secs(20),
                    retry: std::time::Duration::from_secs(2),
                    hooks: Default::default(),
                },
            )),
            last_lines: crate::server::last_line::LastLines::new(std::time::Duration::from_secs(1)),
        })
    }

    fn refusal(reg: &Registry, role: Role, id: &Uuid) -> String {
        listed(reg, role, id).err().expect("refused")
    }

    /// A listed Terminal without a process, carrying `spec`.
    fn insert(d: &Arc<Daemon>, id: Uuid, spec: LaunchSpec) -> Arc<Terminal> {
        let t = terminal::unresolved(
            d,
            PersistedTerminal {
                terminal: id,
                spec,
                meta: Map::new(),
                cols: 80,
                rows: 24,
                created_at_ms: 0,
                leader: None,
            },
        );
        d.reg.lock().unwrap().terminals.insert(id, t.clone());
        t
    }

    #[test]
    fn absent_target_then_shell_created() {
        let dir = tempfile::tempdir().unwrap();
        let d = daemon(dir.path());
        let id = Uuid::new_v4();
        let e = refusal(&d.reg.lock().unwrap(), Role::Mobile, &id);
        assert_eq!(e, format!("unknown terminal {id}"));
        insert(&d, id, raw_shell("/"));
        let reg = d.reg.lock().unwrap();
        assert!(refusal(&reg, Role::Mobile, &id).starts_with(FORBIDDEN));
        assert!(listed(&reg, Role::Desktop, &id).is_ok());
    }

    #[test]
    fn uuid_reuse_is_checked_on_the_new_instance() {
        let dir = tempfile::tempdir().unwrap();
        let d = daemon(dir.path());
        let id = Uuid::new_v4();
        let a = insert(&d, id, agent("/"));
        {
            let reg = d.reg.lock().unwrap();
            let got = listed(&reg, Role::Mobile, &id).unwrap();
            assert!(Arc::ptr_eq(got, &a));
        }
        // Closed and reopened (or replaced) under the same UUID as a shell.
        insert(&d, id, raw_shell("/"));
        {
            let reg = d.reg.lock().unwrap();
            assert!(refusal(&reg, Role::Mobile, &id).starts_with(FORBIDDEN));
        }
        // And the other way round: the agent that replaces it is reachable again.
        let b = insert(&d, id, agent("/"));
        let reg = d.reg.lock().unwrap();
        assert!(Arc::ptr_eq(listed(&reg, Role::Mobile, &id).unwrap(), &b));
    }

    #[test]
    fn relaunch_rechecks_the_current_spec() {
        assert_eq!(check_relaunch(Role::Mobile, &agent("/")), Ok(()));
        let bad_sid = LaunchSpec {
            session_id: Some("-cx".into()),
            ..agent("/")
        };
        assert!(check_relaunch(Role::Mobile, &bad_sid).is_err());
        assert_eq!(check_relaunch(Role::Desktop, &bad_sid), Ok(()));
        let prefixed = LaunchSpec {
            launch_prefix: Some(vec!["env".into()]),
            ..agent("/")
        };
        assert!(check_relaunch(Role::Mobile, &prefixed).is_err());
    }
}

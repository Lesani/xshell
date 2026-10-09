//! `dispatch` routes every method, maps JSON params like Tauri does, and returns exactly what
//! the direct core call returns.

use serde_json::{json, to_value, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use xshell_core::{
    claude, codex, cursor, dispatch, files, git, memories, opencode, sessions, skills, HostCtx,
    METHODS,
};

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("home")).unwrap();
        fs::create_dir_all(dir.path().join("tmp")).unwrap();
        Fixture { dir }
    }
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }
    fn ctx(&self) -> HostCtx {
        HostCtx::with_home(self.home(), self.dir.path().join("tmp"))
    }
    fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PathBuf {
        let p = self.dir.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, contents).unwrap();
        p
    }
    fn write_jsonl(&self, rel: impl AsRef<Path>, lines: &[Value]) -> PathBuf {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        self.write(rel, body)
    }
}

fn call(ctx: &HostCtx, method: &str, params: Value) -> Result<Value, String> {
    dispatch(ctx, method, params)
}

// A Claude project at `/work/proj` with two sessions; `child` was branched from `parent`.
const CWD: &str = "/work/proj";

fn claude_project(fx: &Fixture) {
    let dir = format!("home/.claude/projects/{}", claude::encode_project_name(CWD));
    fx.write_jsonl(
        format!("{dir}/parent.jsonl"),
        &[
            json!({"type": "user", "cwd": CWD, "timestamp": "2026-01-02T10:00:00Z", "uuid": "u1",
                   "message": {"role": "user", "content": "first prompt"}}),
            json!({"type": "assistant", "cwd": CWD, "timestamp": "2026-01-02T10:00:05Z", "uuid": "u2",
                   "message": {"id": "m1", "role": "assistant", "model": "claude-opus-4-7",
                               "content": [{"type": "text", "text": "an answer"}],
                               "usage": {"input_tokens": 10, "output_tokens": 5}}}),
        ],
    );
    fx.write_jsonl(
        format!("{dir}/child.jsonl"),
        &[
            json!({"type": "user", "cwd": CWD, "timestamp": "2026-01-03T10:00:00Z", "uuid": "u1",
                   "forkedFrom": {"sessionId": "parent"},
                   "message": {"role": "user", "content": "first prompt"}}),
        ],
    );
}

// A repository with one commit and a modified, unstaged README, or None without git. Its
// config is local so core's own `git` calls (which inherit the user's config) behave.
fn git_repo(dir: &Path) -> Option<PathBuf> {
    let run = |args: &[&str]| {
        Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
            ])
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return None;
    }
    fs::create_dir_all(dir).unwrap();
    let hooks = dir.join(".git").join("no-hooks");
    let ok = run(&["init", "-q"])
        && run(&["config", "commit.gpgsign", "false"])
        && run(&["config", "core.excludesFile", ""])
        && run(&["config", "core.hooksPath", &hooks.to_string_lossy()])
        && {
            fs::write(dir.join("README.md"), "hello\n").unwrap();
            run(&["add", "README.md"])
        }
        && run(&["commit", "-q", "-m", "init"])
        && run(&["branch", "feature"]);
    assert!(ok, "git is installed but setting up a test repo failed");
    fs::write(dir.join("README.md"), "hello\nworld\n").unwrap();
    Some(dir.to_path_buf())
}

#[test]
fn unknown_method_is_rejected() {
    let fx = Fixture::new();
    assert_eq!(
        call(&fx.ctx(), "rm_rf", json!({})),
        Err("unknown method `rm_rf`".to_string())
    );
    // Desktop-only and terminal commands are not served.
    for m in ["open_url", "read_image_base64", "spawn_terminal"] {
        assert_eq!(
            call(&fx.ctx(), m, json!({})),
            Err(format!("unknown method `{m}`"))
        );
    }
}

#[test]
fn methods_list_has_no_duplicates_and_35_entries() {
    let set: HashSet<&str> = METHODS.iter().copied().collect();
    assert_eq!(METHODS.len(), 35);
    assert_eq!(set.len(), 35);
}

#[test]
fn every_listed_method_is_routed() {
    let fx = Fixture::new();
    for m in METHODS {
        if let Err(e) = call(&fx.ctx(), m, json!({})) {
            assert!(!e.starts_with("unknown method"), "{m}: {e}");
        }
    }
}

#[test]
fn null_params_accepted_for_argless_methods() {
    let fx = Fixture::new();
    let ctx = fx.ctx();
    for m in [
        "list_claude_projects",
        "get_username",
        "get_home_dir",
        "probe_statusline_setup",
        "get_global_rate_limits",
        "get_claude_cost_summary",
        "get_codex_usage",
        "list_codex_projects",
        "list_cursor_projects",
        "list_opencode_projects",
        "list_antigravity_projects",
    ] {
        assert_eq!(call(&ctx, m, Value::Null), call(&ctx, m, json!({})), "{m}");
        assert!(call(&ctx, m, Value::Null).is_ok(), "{m}");
    }
    // A method with required params reports them missing, as with `{}`.
    let e = call(&ctx, "get_sessions", Value::Null).unwrap_err();
    assert!(
        e.starts_with("invalid args for command `get_sessions`:"),
        "{e}"
    );
}

#[test]
fn argless_methods_equal_direct_calls() {
    let fx = Fixture::new();
    claude_project(&fx);
    let ctx = fx.ctx();
    assert_eq!(
        call(&ctx, "list_claude_projects", json!({})).unwrap(),
        to_value(claude::list_claude_projects(&ctx)).unwrap()
    );
    assert_eq!(
        call(&ctx, "list_codex_projects", json!({})).unwrap(),
        to_value(codex::list_codex_projects(&ctx)).unwrap()
    );
    assert_eq!(
        call(&ctx, "list_cursor_projects", json!({})).unwrap(),
        to_value(cursor::list_cursor_projects(&ctx)).unwrap()
    );
    assert_eq!(
        call(&ctx, "get_username", json!({})).unwrap(),
        to_value(files::get_username()).unwrap()
    );
}

#[test]
fn get_home_dir_returns_ctx_home() {
    let fx = Fixture::new();
    assert_eq!(
        call(&fx.ctx(), "get_home_dir", json!({})).unwrap(),
        json!(fx.home().to_string_lossy())
    );
    let no_home = HostCtx {
        home: None,
        temp_dir: fx.dir.path().join("tmp"),
    };
    assert_eq!(
        call(&no_home, "get_home_dir", json!({})).unwrap(),
        json!("")
    );
}

#[test]
fn get_sessions_round_trip_equals_direct_call() {
    let fx = Fixture::new();
    claude_project(&fx);
    let ctx = fx.ctx();
    let enc = claude::encode_project_name(CWD);
    let got = call(&ctx, "get_sessions", json!({"encodedName": enc})).unwrap();
    assert_eq!(got, to_value(sessions::get_sessions(&ctx, enc)).unwrap());
    assert_eq!(got.as_array().unwrap().len(), 2);
}

#[test]
fn camel_case_keys_required_like_tauri() {
    let fx = Fixture::new();
    let ctx = fx.ctx();
    let e = call(&ctx, "get_sessions", json!({"encoded_name": "x"})).unwrap_err();
    assert!(
        e.starts_with("invalid args for command `get_sessions`:"),
        "{e}"
    );
    assert!(call(&ctx, "get_sessions", json!({"encodedName": "x"})).is_ok());
}

#[test]
fn limit_and_message_params_equal_direct_calls() {
    let fx = Fixture::new();
    claude_project(&fx);
    let ctx = fx.ctx();
    assert_eq!(
        call(&ctx, "get_all_recent_sessions", json!({"limit": 1})).unwrap(),
        to_value(sessions::get_all_recent_sessions(&ctx, 1)).unwrap()
    );
    let enc = claude::encode_project_name(CWD);
    let got = call(
        &ctx,
        "get_session_messages",
        json!({"encodedName": enc, "sessionId": "parent", "limit": 5}),
    )
    .unwrap();
    assert_eq!(
        got,
        to_value(claude::get_session_messages(&ctx, enc, "parent".into(), 5)).unwrap()
    );
    assert_eq!(got.as_array().unwrap().len(), 2);
}

#[test]
fn cwd_and_branch_detection_params_equal_direct_calls() {
    let fx = Fixture::new();
    claude_project(&fx);
    let ctx = fx.ctx();
    let ids = call(&ctx, "list_project_session_ids", json!({"cwd": CWD})).unwrap();
    assert_eq!(
        ids,
        to_value(claude::list_project_session_ids(&ctx, CWD.into())).unwrap()
    );
    let got = call(
        &ctx,
        "detect_session_branch",
        json!({"cwd": CWD, "currentSessionId": "parent", "knownSessionIds": ["parent"]}),
    )
    .unwrap();
    assert_eq!(
        got,
        to_value(claude::detect_session_branch(
            &ctx,
            CWD.into(),
            "parent".into(),
            vec!["parent".into()]
        ))
        .unwrap()
    );
    assert_eq!(got["new_session_id"], json!("child"));
}

#[test]
fn project_path_params_equal_direct_calls() {
    let fx = Fixture::new();
    let project = fx.dir.path().join("proj");
    fx.write("proj/CLAUDE.md", "# Project rules\n");
    fx.write("proj/AGENTS.md", "# Agent rules\n");
    fx.write("proj/.cursor/rules/style.mdc", "Use tabs.\n");
    fx.write(
        "proj/opencode.json",
        r#"{"mcp": {"x": {"type": "local", "command": ["x"]}}}"#,
    );
    fx.write(
        "proj/.claude/skills/demo/SKILL.md",
        "---\ndescription: a demo skill\n---\n",
    );
    let ctx = fx.ctx();
    let pp = project.to_string_lossy().into_owned();
    let p = json!({"projectPath": pp});
    assert_eq!(
        call(&ctx, "get_project_skills", p.clone()).unwrap(),
        to_value(skills::get_project_skills(&ctx, pp.clone())).unwrap()
    );
    assert_eq!(
        call(&ctx, "get_project_memories", p.clone()).unwrap(),
        to_value(memories::get_project_memories(&ctx, pp.clone())).unwrap()
    );
    assert_eq!(
        call(&ctx, "get_codex_context", p.clone()).unwrap(),
        to_value(codex::get_codex_context(&ctx, pp.clone())).unwrap()
    );
    assert_eq!(
        call(&ctx, "get_cursor_context", p.clone()).unwrap(),
        to_value(cursor::get_cursor_context(&ctx, pp.clone())).unwrap()
    );
    let oc = call(&ctx, "get_opencode_context", p.clone()).unwrap();
    assert_eq!(
        oc,
        to_value(opencode::get_opencode_context(&ctx, pp.clone())).unwrap()
    );
    assert_eq!(oc["present"], json!(true));
}

#[test]
fn file_params_equal_direct_calls() {
    let fx = Fixture::new();
    fx.write("tree/Alpha.txt", "a");
    fx.write("tree/sub/alpha-2.txt", "b");
    fx.write("tree/beta.txt", "c");
    let ctx = fx.ctx();
    let root = fx.dir.path().join("tree").to_string_lossy().into_owned();
    assert_eq!(
        call(&ctx, "list_dir", json!({"path": root})).unwrap(),
        to_value(files::list_dir(root.clone()).unwrap()).unwrap()
    );
    let got = call(
        &ctx,
        "search_dir",
        json!({"root": root, "query": "ALPHA", "limit": 1}),
    )
    .unwrap();
    assert_eq!(
        got,
        to_value(files::search_dir(root.clone(), "ALPHA".into(), Some(1))).unwrap()
    );
    assert_eq!(got.as_array().unwrap().len(), 1);
    let file = fx
        .dir
        .path()
        .join("tree/beta.txt")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        call(&ctx, "read_text_file", json!({"path": file})).unwrap(),
        json!("c")
    );
}

#[test]
fn optional_params_may_be_omitted() {
    let fx = Fixture::new();
    fx.write("tree/alpha.txt", "a");
    let ctx = fx.ctx();
    let root = fx.dir.path().join("tree").to_string_lossy().into_owned();
    let got = call(&ctx, "search_dir", json!({"root": root, "query": "alpha"})).unwrap();
    assert_eq!(
        got,
        to_value(files::search_dir(root, "alpha".into(), None)).unwrap()
    );
    let cwd = fx.dir.path().to_string_lossy().into_owned();
    assert_eq!(
        call(&ctx, "get_git_log", json!({"cwd": cwd})).unwrap(),
        to_value(git::get_git_log(cwd, None)).unwrap()
    );
}

#[test]
fn unknown_keys_are_ignored() {
    let fx = Fixture::new();
    let ctx = fx.ctx();
    assert_eq!(
        call(
            &ctx,
            "get_sessions",
            json!({"encodedName": "x", "extra": 1})
        ),
        Ok(json!([]))
    );
    assert!(call(&ctx, "list_claude_projects", json!({"anything": true})).is_ok());
}

/// Replaces git's clock-relative fields ("0 seconds ago") so two calls made a second apart
/// still compare equal; asserts each one was a non-empty string first.
fn strip_relative_times(mut v: Value) -> Value {
    if let Some(items) = v.as_array_mut() {
        for item in items {
            for key in ["relative_time", "last_commit_relative"] {
                if let Some(field) = item.get_mut(key) {
                    assert!(field.as_str().is_some_and(|s| !s.is_empty()), "{key} empty");
                    *field = Value::Null;
                }
            }
        }
    }
    v
}

#[test]
fn git_params_equal_direct_calls() {
    let fx = Fixture::new();
    let Some(repo) = git_repo(&fx.dir.path().join("repo")) else {
        eprintln!("git not available; skipping");
        return;
    };
    let ctx = fx.ctx();
    let cwd = repo.to_string_lossy().into_owned();
    let status = || call(&ctx, "get_git_status", json!({"cwd": cwd})).unwrap();
    assert_eq!(
        status(),
        to_value(git::get_git_status(cwd.clone())).unwrap()
    );
    assert_eq!(status()["files"][0]["unstaged"], json!("M"));

    let log = call(&ctx, "get_git_log", json!({"cwd": cwd, "limit": 1})).unwrap();
    assert_eq!(
        strip_relative_times(log.clone()),
        strip_relative_times(to_value(git::get_git_log(cwd.clone(), Some(1))).unwrap())
    );
    assert_eq!(log.as_array().unwrap().len(), 1);

    let diff = call(
        &ctx,
        "git_diff",
        json!({"cwd": cwd, "path": "README.md", "mode": "unstaged"}),
    )
    .unwrap();
    assert_eq!(
        diff,
        to_value(git::git_diff(cwd.clone(), "README.md".into(), "unstaged".into()).unwrap())
            .unwrap()
    );
    assert!(diff.as_str().unwrap().contains("+world"));

    assert_eq!(
        strip_relative_times(call(&ctx, "list_git_branches", json!({"cwd": cwd})).unwrap()),
        strip_relative_times(to_value(git::list_git_branches(cwd.clone())).unwrap())
    );

    // Unit results are null; the effect matches the direct call's.
    assert_eq!(
        call(
            &ctx,
            "git_stage",
            json!({"cwd": cwd, "paths": ["README.md"]})
        ),
        Ok(Value::Null)
    );
    assert_eq!(status()["files"][0]["staged"], json!("M"));
    assert_eq!(
        call(
            &ctx,
            "git_unstage",
            json!({"cwd": cwd, "paths": ["README.md"]})
        ),
        Ok(Value::Null)
    );
    assert_eq!(status()["files"][0]["staged"], json!(" "));
    assert_eq!(
        call(
            &ctx,
            "git_discard",
            json!({"cwd": cwd, "path": "README.md", "mode": "unstaged"})
        ),
        Ok(Value::Null)
    );
    assert_eq!(status()["files"], json!([]));
    assert_eq!(
        call(
            &ctx,
            "git_checkout",
            json!({"cwd": cwd, "branch": "feature"})
        ),
        Ok(Value::Null)
    );
    assert_eq!(status()["branch"], json!("feature"));
}

#[test]
fn unit_and_none_results_are_null() {
    let fx = Fixture::new();
    let ctx = fx.ctx();
    let cwd = fx.dir.path().to_string_lossy().into_owned();
    // No paths: git_stage returns Ok(()) without running git.
    assert_eq!(
        call(&ctx, "git_stage", json!({"cwd": cwd, "paths": []})),
        Ok(Value::Null)
    );
    assert_eq!(
        call(
            &ctx,
            "detect_session_branch",
            json!({"cwd": CWD, "currentSessionId": "a", "knownSessionIds": []})
        ),
        Ok(Value::Null)
    );
}

#[test]
fn result_errors_pass_through() {
    let fx = Fixture::new();
    let ctx = fx.ctx();
    let missing = fx
        .dir
        .path()
        .join("nope.txt")
        .to_string_lossy()
        .into_owned();
    let e = call(&ctx, "read_text_file", json!({"path": missing})).unwrap_err();
    assert!(e.starts_with("Failed to read file:"), "{e}");
    assert_eq!(
        call(&ctx, "detect_agent_binary", json!({"binary": "rm"})),
        Err("Unknown agent binary: rm".to_string())
    );
}

#[test]
fn save_dropped_file_round_trip() {
    let fx = Fixture::new();
    let ctx = fx.ctx();
    // "hello" in base64; the name is sanitised to [A-Za-z0-9 ._-].
    let got = call(
        &ctx,
        "save_dropped_file",
        json!({"bytesBase64": "aGVsbG8=", "name": "a/b?.png"}),
    )
    .unwrap();
    let path = PathBuf::from(got.as_str().unwrap());
    assert_eq!(
        path.parent().unwrap(),
        ctx.temp_dir.join("xshell-clipboard")
    );
    assert!(path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with("-ab.png"));
    assert_eq!(fs::read(&path).unwrap(), b"hello");
}

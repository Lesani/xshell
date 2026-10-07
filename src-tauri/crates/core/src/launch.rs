use crate::claude::encode_project_name;
use crate::ctx::HostCtx;
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a terminal tab asks to run: the command-building inputs of `spawn_terminal`, without
/// the PTY size or the output channels. A remote `term.open` wraps this rather than grows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSpec {
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub cwd: String,
    pub shell_mode: Option<String>,
    pub shell_command: Option<String>,
    pub shell_id: Option<String>,
    pub fullscreen_rendering: Option<bool>,
    pub force_sync_output: Option<bool>,
}

/// The process a [`LaunchSpec`] resolves to, as plain data. `env` holds only the variables
/// xshell sets on top of the inherited environment, in the order they are set.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
}

impl CommandPlan {
    /// The PTY command for this plan. `CommandBuilder::new` copies the process environment,
    /// so the plan's `env` entries are overrides on top of it.
    pub fn to_command_builder(&self) -> CommandBuilder {
        let mut cmd = CommandBuilder::new(&self.program);
        for a in &self.args {
            cmd.arg(a);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.cwd(&self.cwd);
        cmd
    }
}

// The Git Bash preset sends bare `bash.exe`, which on Windows resolves via PATH and gets
// shadowed by C:\Windows\System32\bash.exe (the WSL launcher) — that dies with a cryptic
// `execvpe(/bin/bash) failed` when no WSL distro is installed. Probe known Git for Windows
// install locations and return the first existing match so we spawn the real Git Bash.
pub fn resolve_gitbash_path() -> Option<PathBuf> {
    let candidates: &[(&str, &[&str])] = &[
        ("ProgramFiles", &["Git", "bin", "bash.exe"]),
        ("ProgramFiles(x86)", &["Git", "bin", "bash.exe"]),
        ("LOCALAPPDATA", &["Programs", "Git", "bin", "bash.exe"]),
    ];
    for (env_var, parts) in candidates {
        if let Ok(base) = std::env::var(env_var) {
            let mut p = PathBuf::from(base);
            for part in *parts {
                p.push(part);
            }
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

// PowerShell's command precedence ranks `.ps1` external scripts above `.cmd` shims, so a
// bare `claude` resolves to the unsigned npm `claude.ps1` — which an `AllSigned` execution
// policy refuses to run (issue #41). Resolve the `.cmd` shim explicitly for the PowerShell
// host: batch shims aren't subject to execution policy. Falls back to the bare name when no
// .cmd is on PATH (e.g. a native .exe install), where bare resolution is safe anyway.
#[cfg(windows)]
fn resolve_cmd_shim(bin: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("where");
    cmd.arg(format!("{}.cmd", bin));
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|s| !s.is_empty())
}
#[cfg(not(windows))]
fn resolve_cmd_shim(_bin: &str) -> Option<String> {
    None
}

/// Which agent CLI a tab hosts. Each agent's resume form differs; everything else (shell
/// wrapping, PTY plumbing) is agent-agnostic and shared.
pub fn agent_binary(agent: Option<&str>) -> &'static str {
    match agent {
        Some("codex") => "codex",
        Some("cursor") => "cursor-agent",
        Some("opencode") => "opencode",
        Some("antigravity") => "agy",
        _ => "claude",
    }
}

// Resume args per agent:
//  - Claude: new chats arrive with a pre-allocated UUID and no JSONL on disk → use
//    `--session-id` so Claude creates the session under our UUID (leaving customTitle
//    empty so ai-title can fire). Existing sessions have a JSONL → `--resume`.
//  - Codex:  `codex resume <id>`.
//  - Cursor: `cursor-agent --resume=<id>`.
//  - opencode: `opencode --session <id>` (resolved against the project cwd we spawn in).
//  - Antigravity: `agy --conversation <id>` (conversations are workspace-scoped, so the
//    project cwd we spawn in must match the conversation's recorded workspace — it does,
//    since the session row carries that workspace as its project path).
// New non-Claude chats carry no session_id (they start unlinked) → spawn bare.
pub fn resume_args(
    ctx: &HostCtx,
    agent_bin: &str,
    session_id: Option<&str>,
    cwd: &str,
) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(sid) = session_id {
        match agent_bin {
            "codex" => {
                v.push("resume".into());
                v.push(sid.to_string());
            }
            "cursor-agent" => {
                v.push(format!("--resume={}", sid));
            }
            "opencode" => {
                v.push("--session".into());
                v.push(sid.to_string());
            }
            "agy" => {
                v.push("--conversation".into());
                v.push(sid.to_string());
            }
            _ => {
                let jsonl_exists = ctx
                    .claude_projects_dir()
                    .map(|d| {
                        d.join(encode_project_name(cwd))
                            .join(format!("{}.jsonl", sid))
                            .exists()
                    })
                    .unwrap_or(false);
                v.push(if jsonl_exists {
                    "--resume".into()
                } else {
                    "--session-id".into()
                });
                v.push(sid.to_string());
            }
        }
    }
    v
}

/// Resolve a terminal tab's launch request to the program, arguments, environment overrides
/// and working directory to spawn in its PTY.
pub fn plan_command(ctx: &HostCtx, spec: &LaunchSpec) -> Result<CommandPlan, String> {
    let mode = spec.shell_mode.as_deref().unwrap_or("claude");
    let agent_bin = agent_binary(spec.agent.as_deref());
    // The resume check uses the raw cwd, before the empty-cwd fallback below.
    let agent_args = resume_args(ctx, agent_bin, spec.session_id.as_deref(), &spec.cwd);
    let shell_kind = spec.shell_id.as_deref().unwrap_or("");
    // Override the frontend-supplied `bash.exe` for the Git Bash preset with an absolute path
    // — see resolve_gitbash_path() for why. Surface a clear error if Git for Windows isn't
    // installed, instead of letting WSL emit its execvpe message.
    let gitbash_resolved: Option<String> = if shell_kind == "gitbash" {
        Some(resolve_gitbash_path().ok_or_else(|| "Git Bash not found. Install Git for Windows or choose a different shell preset.".to_string())?.to_string_lossy().into_owned())
    } else {
        None
    };
    let effective_shell = gitbash_resolved
        .as_deref()
        .or(spec.shell_command.as_deref());
    let (program, args): (String, Vec<String>) = if mode == "raw" {
        // Raw shell: spawn the chosen shell directly (no claude wrapping).
        let shell = effective_shell.unwrap_or(if cfg!(windows) {
            "powershell.exe"
        } else {
            "bash"
        });
        (shell.to_string(), vec![])
    } else if let Some(shell) = effective_shell.filter(|s| !s.is_empty()) {
        // Agent mode with an explicit host shell: launch the shell and run the agent inside it
        // so the user's preferred shell wraps the session (and stays alive after the agent exits).
        match shell_kind {
            "powershell" | "pwsh" => {
                // & 'claude' 'arg1' 'arg2' — single-quoted to avoid PS expansion surprises.
                // Prefer the .cmd shim over the bare name so AllSigned policies don't block
                // the unsigned .ps1 shim (see resolve_cmd_shim / issue #41).
                let exec = resolve_cmd_shim(agent_bin).unwrap_or_else(|| agent_bin.to_string());
                let mut s = format!("& '{}'", exec.replace('\'', "''"));
                for a in &agent_args {
                    s.push(' ');
                    s.push('\'');
                    s.push_str(&a.replace('\'', "''"));
                    s.push('\'');
                }
                (
                    shell.to_string(),
                    vec!["-NoLogo".into(), "-NoExit".into(), "-Command".into(), s],
                )
            }
            "cmd" => {
                let mut args = vec!["/K".to_string(), agent_bin.to_string()];
                args.extend(agent_args.iter().cloned());
                (shell.to_string(), args)
            }
            "gitbash" | "bash" | "zsh" | "fish" => {
                // bash -i -c "claude arg1 arg2; exec bash -i"
                fn q(s: &str) -> String {
                    format!("'{}'", s.replace('\'', "'\\''"))
                }
                let mut s = String::from(agent_bin);
                for a in &agent_args {
                    s.push(' ');
                    s.push_str(&q(a));
                }
                // Keep the shell alive after the agent exits so the user retains a prompt.
                let basename = std::path::Path::new(shell)
                    .file_stem()
                    .and_then(|o| o.to_str())
                    .unwrap_or("bash");
                s.push_str(&format!("; exec {} -i", basename));
                (shell.to_string(), vec!["-i".into(), "-c".into(), s])
            }
            _ => {
                // Unknown shell_id — fall back to the pre-existing OS-default behavior.
                os_default(agent_bin, &agent_args)
            }
        }
    } else {
        os_default(agent_bin, &agent_args)
    };
    let mut env: Vec<(String, String)> = Vec::new();
    // Tag the terminal so Claude Code's OTEL telemetry attributes sessions to this app
    // (telemetry reads `terminal.type` from TERM_PROGRAM; without this we'd land in the
    // Unknown bucket). Always set — no user-facing toggle.
    env.push(("TERM_PROGRAM".into(), "xshell.sh".into()));
    // Claude Code's flicker-free / alternate-screen-buffer renderer is opt-in via env var.
    // Default ON for any claude-mode spawn; raw shells don't get it (no claude process to read it).
    // Inherited by the wrapping shell → claude child, so setting it here is sufficient.
    if mode != "raw" && agent_bin == "claude" && spec.fullscreen_rendering.unwrap_or(true) {
        env.push(("CLAUDE_CODE_NO_FLICKER".into(), "1".into()));
    }
    // Force synchronized output mode (DEC 2026). Claude's auto-detection looks at $TERM
    // and won't enable sync output for plain xterm-256color, but xterm.js v5+ supports it
    // natively. With this flag, claude wraps each TUI frame in \x1b[?2026h..\x1b[?2026l
    // so xterm renders only complete frames — fixes the "flying letters" residue we get
    // when xterm sees half-drawn frames. Requires Claude Code ≥ 2.1.129.
    if mode != "raw" && agent_bin == "claude" && spec.force_sync_output.unwrap_or(true) {
        env.push(("CLAUDE_CODE_FORCE_SYNC_OUTPUT".into(), "1".into()));
    }
    // Empty cwd → fall back to the user's home directory (raw shells launched from home view).
    let cwd = if spec.cwd.is_empty() {
        ctx.home
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string())
    } else {
        spec.cwd.clone()
    };
    Ok(CommandPlan {
        program,
        args,
        env,
        cwd,
    })
}

// No host shell chosen: on Windows run the agent through `cmd.exe /C`, elsewhere spawn it
// directly.
fn os_default(agent_bin: &str, agent_args: &[String]) -> (String, Vec<String>) {
    if cfg!(windows) {
        let mut args = vec!["/C".to_string(), agent_bin.to_string()];
        args.extend(agent_args.iter().cloned());
        ("cmd.exe".to_string(), args)
    } else {
        (agent_bin.to_string(), agent_args.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::ffi::{OsStr, OsString};

    fn spec(cwd: &str) -> LaunchSpec {
        LaunchSpec {
            cwd: cwd.to_string(),
            ..Default::default()
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn env(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, val)| (k.to_string(), val.to_string()))
            .collect()
    }

    const CLAUDE_ENV: &[(&str, &str)] = &[
        ("TERM_PROGRAM", "xshell.sh"),
        ("CLAUDE_CODE_NO_FLICKER", "1"),
        ("CLAUDE_CODE_FORCE_SYNC_OUTPUT", "1"),
    ];

    // Checks the PTY command built from a plan: argv is program + args, the cwd is set, and
    // every override is present. Inherited variables are not asserted either way.
    fn assert_builder(plan: &CommandPlan) {
        let cmd = plan.to_command_builder();
        let mut argv: Vec<OsString> = vec![OsString::from(&plan.program)];
        argv.extend(plan.args.iter().map(OsString::from));
        assert_eq!(cmd.get_argv(), &argv);
        assert_eq!(cmd.get_cwd(), Some(&OsString::from(&plan.cwd)));
        for (k, v) in &plan.env {
            assert_eq!(cmd.get_env(k), Some(OsStr::new(v)), "{k}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn launch_claude_new_session_uses_session_id() {
        let fx = Fixture::new();
        let plan = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                session_id: Some("sid-1".into()),
                ..spec("/work/app")
            },
        )
        .unwrap();
        assert_eq!(plan.program, "claude");
        assert_eq!(plan.args, strings(&["--session-id", "sid-1"]));
        assert_eq!(plan.env, env(CLAUDE_ENV));
        assert_eq!(plan.cwd, "/work/app");
        assert_builder(&plan);
    }

    #[cfg(unix)]
    #[test]
    fn launch_claude_existing_session_uses_resume() {
        let fx = Fixture::new();
        let cwd = "/work/my_app";
        fx.write(
            format!(
                "home/.claude/projects/{}/sid-2.jsonl",
                encode_project_name(cwd)
            ),
            "{}\n",
        );
        let plan = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                session_id: Some("sid-2".into()),
                ..spec(cwd)
            },
        )
        .unwrap();
        assert_eq!(plan.program, "claude");
        assert_eq!(plan.args, strings(&["--resume", "sid-2"]));
        assert_eq!(plan.env, env(CLAUDE_ENV));
        assert_builder(&plan);
    }

    #[test]
    fn launch_resume_args_per_agent() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let cases: &[(&str, &str, &[&str])] = &[
            ("codex", "codex", &["resume", "id1"]),
            ("cursor", "cursor-agent", &["--resume=id1"]),
            ("opencode", "opencode", &["--session", "id1"]),
            ("antigravity", "agy", &["--conversation", "id1"]),
        ];
        for (agent, bin, args) in cases {
            assert_eq!(agent_binary(Some(agent)), *bin);
            assert_eq!(
                resume_args(&ctx, bin, Some("id1"), "/w"),
                strings(args),
                "{agent}"
            );
            let plan = plan_command(
                &ctx,
                &LaunchSpec {
                    agent: Some(agent.to_string()),
                    session_id: Some("id1".into()),
                    ..spec("/w")
                },
            )
            .unwrap();
            // Only Claude reads the two renderer variables.
            assert_eq!(plan.env, env(&[("TERM_PROGRAM", "xshell.sh")]), "{agent}");
            if !cfg!(windows) {
                assert_eq!(plan.program, *bin);
                assert_eq!(plan.args, strings(args));
            }
        }
        assert_eq!(agent_binary(None), "claude");
        assert_eq!(agent_binary(Some("unknown")), "claude");
        // New non-Claude chats carry no session id and spawn bare.
        assert!(resume_args(&ctx, "codex", None, "/w").is_empty());
    }

    #[test]
    fn launch_flags_can_disable_claude_env() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let base = spec("/w");
        let off = |nf: Option<bool>, fs: Option<bool>| {
            plan_command(
                &ctx,
                &LaunchSpec {
                    fullscreen_rendering: nf,
                    force_sync_output: fs,
                    ..base.clone()
                },
            )
            .unwrap()
            .env
        };
        assert_eq!(off(None, None), env(CLAUDE_ENV));
        assert_eq!(off(Some(true), Some(true)), env(CLAUDE_ENV));
        assert_eq!(
            off(Some(false), None),
            env(&[
                ("TERM_PROGRAM", "xshell.sh"),
                ("CLAUDE_CODE_FORCE_SYNC_OUTPUT", "1")
            ])
        );
        assert_eq!(
            off(None, Some(false)),
            env(&[
                ("TERM_PROGRAM", "xshell.sh"),
                ("CLAUDE_CODE_NO_FLICKER", "1")
            ])
        );
        assert_eq!(
            off(Some(false), Some(false)),
            env(&[("TERM_PROGRAM", "xshell.sh")])
        );
    }

    #[test]
    fn launch_raw_mode_spawns_shell_directly() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let raw = LaunchSpec {
            shell_mode: Some("raw".into()),
            session_id: Some("ignored".into()),
            ..spec("/w")
        };
        let plan = plan_command(
            &ctx,
            &LaunchSpec {
                shell_command: Some("zsh".into()),
                ..raw.clone()
            },
        )
        .unwrap();
        assert_eq!(plan.program, "zsh");
        assert!(plan.args.is_empty());
        // Raw shells host no claude process, so only the terminal tag is set.
        assert_eq!(plan.env, env(&[("TERM_PROGRAM", "xshell.sh")]));
        assert_builder(&plan);

        let default = plan_command(&ctx, &raw).unwrap();
        let expected = if cfg!(windows) {
            "powershell.exe"
        } else {
            "bash"
        };
        assert_eq!(default.program, expected);
        assert!(default.args.is_empty());
        assert_builder(&default);
    }

    #[cfg(unix)]
    #[test]
    fn launch_bash_wrapper_quotes_args_and_keeps_shell() {
        let fx = Fixture::new();
        let cwd = "/w";
        fx.write(
            format!(
                "home/.claude/projects/{}/a'b.jsonl",
                encode_project_name(cwd)
            ),
            "{}\n",
        );
        let plan = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                session_id: Some("a'b".into()),
                shell_id: Some("bash".into()),
                shell_command: Some("/usr/bin/bash".into()),
                ..spec(cwd)
            },
        )
        .unwrap();
        assert_eq!(plan.program, "/usr/bin/bash");
        assert_eq!(
            plan.args,
            strings(&["-i", "-c", "claude '--resume' 'a'\\''b'; exec bash -i"])
        );
        assert_eq!(plan.env, env(CLAUDE_ENV));
        assert_builder(&plan);

        // zsh keeps zsh alive afterwards, and an empty session list runs the bare agent.
        let zsh = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                shell_id: Some("zsh".into()),
                shell_command: Some("zsh".into()),
                agent: Some("codex".into()),
                ..spec(cwd)
            },
        )
        .unwrap();
        assert_eq!(zsh.args, strings(&["-i", "-c", "codex; exec zsh -i"]));
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_powershell_wrapper() {
        let fx = Fixture::new();
        let plan = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                session_id: Some("sid".into()),
                shell_id: Some("pwsh".into()),
                shell_command: Some("pwsh".into()),
                ..spec("/w")
            },
        )
        .unwrap();
        assert_eq!(plan.program, "pwsh");
        assert_eq!(
            plan.args,
            strings(&[
                "-NoLogo",
                "-NoExit",
                "-Command",
                "& 'claude' '--session-id' 'sid'"
            ])
        );
    }

    #[test]
    fn launch_cmd_wrapper() {
        let fx = Fixture::new();
        let plan = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                agent: Some("codex".into()),
                session_id: Some("id1".into()),
                shell_id: Some("cmd".into()),
                shell_command: Some("cmd.exe".into()),
                ..spec("/w")
            },
        )
        .unwrap();
        assert_eq!(plan.program, "cmd.exe");
        assert_eq!(plan.args, strings(&["/K", "codex", "resume", "id1"]));
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_gitbash_without_git_for_windows_errors() {
        let fx = Fixture::new();
        let err = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                shell_id: Some("gitbash".into()),
                shell_command: Some("bash.exe".into()),
                ..spec("/w")
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Git Bash not found. Install Git for Windows or choose a different shell preset."
        );
    }

    #[test]
    fn launch_empty_cwd_falls_back_to_home_then_dot() {
        let fx = Fixture::new();
        let plan = plan_command(&fx.ctx(), &spec("")).unwrap();
        assert_eq!(plan.cwd, fx.home().to_string_lossy());
        assert_builder(&plan);

        let no_home = HostCtx {
            home: None,
            temp_dir: fx.dir.path().join("tmp"),
        };
        assert_eq!(plan_command(&no_home, &spec("")).unwrap().cwd, ".");
    }

    #[test]
    fn launch_spec_deserializes_frontend_shape() {
        // The `spawn_terminal` invoke arguments minus id, cols, rows and the channels.
        let v = serde_json::json!({
            "sessionId": "sid",
            "cwd": "/w",
            "shellMode": "claude",
            "shellCommand": "bash",
            "shellId": "bash",
            "agent": "codex",
            "fullscreenRendering": false,
            "forceSyncOutput": true,
        });
        let s: LaunchSpec = serde_json::from_value(v).unwrap();
        assert_eq!(
            s,
            LaunchSpec {
                agent: Some("codex".into()),
                session_id: Some("sid".into()),
                cwd: "/w".into(),
                shell_mode: Some("claude".into()),
                shell_command: Some("bash".into()),
                shell_id: Some("bash".into()),
                fullscreen_rendering: Some(false),
                force_sync_output: Some(true),
            }
        );
        // Optional fields may be missing.
        let s: LaunchSpec = serde_json::from_value(serde_json::json!({"cwd": ""})).unwrap();
        assert_eq!(s, LaunchSpec::default());
    }
}

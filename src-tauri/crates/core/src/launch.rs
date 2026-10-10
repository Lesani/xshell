use crate::agent_status::{hook_agent, HookAgent};
use crate::claude::encode_project_name;
use crate::ctx::HostCtx;
use portable_pty::CommandBuilder;
use std::path::PathBuf;

pub use crate::agent_status::TerminalHooks;
pub use xshell_protocol::LaunchSpec;

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

/// The flag that makes an agent CLI run tools without asking, for the agents that have one.
pub fn permission_flag(agent_bin: &str) -> Option<&'static str> {
    match agent_bin {
        "claude" => Some("--dangerously-skip-permissions"),
        "codex" => Some("--dangerously-bypass-approvals-and-sandbox"),
        _ => None,
    }
}

/// The spec a Relaunch starts: `spec` with `skip_permissions` set to `skip`. Refused for raw
/// shells, agents without a [`permission_flag`] and Terminals with no session to resume, since
/// a Relaunch that cannot resume would silently drop the conversation.
pub fn relaunch_spec(spec: &LaunchSpec, skip: bool) -> Result<LaunchSpec, String> {
    if spec.shell_mode.as_deref() == Some("raw") {
        return Err("a raw shell has no permission prompts to skip".into());
    }
    let agent_bin = agent_binary(spec.agent.as_deref());
    if permission_flag(agent_bin).is_none() {
        return Err(format!(
            "{agent_bin} has no flag to skip permission prompts"
        ));
    }
    if spec.session_id.as_deref().is_none_or(str::is_empty) {
        return Err("no session to resume".into());
    }
    Ok(LaunchSpec {
        skip_permissions: Some(skip),
        ..spec.clone()
    })
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
    plan_command_with(ctx, spec, None)
}

/// [`plan_command`], with the agent set up to report its Agent Status through `hooks`. Only
/// agents with hooks (Claude Code, Codex) outside raw shells get them; the rest plan as
/// without. Claude Code gets `--settings <hooks file>`, Codex its `-c` overrides, both the
/// `XSHELL_TERMINAL_ID`/`XSHELL_EVENT_SOCKET` variables, and a shell wrapper reports the
/// agent's end itself, since the shell outlives it.
pub fn plan_command_with(
    ctx: &HostCtx,
    spec: &LaunchSpec,
    hooks: Option<TerminalHooks>,
) -> Result<CommandPlan, String> {
    let mode = spec.shell_mode.as_deref().unwrap_or("claude");
    let agent_bin = agent_binary(spec.agent.as_deref());
    let hooked = hook_agent(spec).zip(hooks);
    // The resume check uses the raw cwd, before the empty-cwd fallback below.
    let mut agent_args = resume_args(ctx, agent_bin, spec.session_id.as_deref(), &spec.cwd);
    if mode != "raw" && spec.skip_permissions == Some(true) {
        if let Some(flag) = permission_flag(agent_bin) {
            // `codex resume` is a subcommand with its own options, so the flag goes after it.
            let at = usize::from(agent_args.first().is_some_and(|a| a == "resume"));
            agent_args.insert(at, flag.to_string());
        }
    }
    match hooked {
        Some((HookAgent::Claude, h)) => {
            agent_args.push("--settings".into());
            agent_args.push(h.hooks.claude_settings.to_string_lossy().into_owned());
        }
        // Last: `codex resume [flag] <id>` takes `-c` after its positionals too.
        Some((HookAgent::Codex, h)) => agent_args.extend(h.hooks.codex_overrides()),
        None => {}
    }
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
    // The agent invocation: the launch prefix's words, if any, then the agent. `exec` is what
    // the shell runs, `exec_args` everything after it.
    let prefix: &[String] = spec.launch_prefix.as_deref().unwrap_or_default();
    let (exec, exec_args): (&str, Vec<String>) = match prefix.split_first() {
        Some((head, rest)) => {
            let mut v = rest.to_vec();
            v.push(agent_bin.to_string());
            v.extend(agent_args.iter().cloned());
            (head.as_str(), v)
        }
        None => (agent_bin, agent_args),
    };
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
                // A prefix is run as given: it is the user's own command, not an npm shim.
                let exec = if prefix.is_empty() {
                    resolve_cmd_shim(agent_bin).unwrap_or_else(|| agent_bin.to_string())
                } else {
                    exec.to_string()
                };
                let mut s = format!("& '{}'", exec.replace('\'', "''"));
                for a in &exec_args {
                    s.push(' ');
                    s.push('\'');
                    s.push_str(&a.replace('\'', "''"));
                    s.push('\'');
                }
                if let Some((_, h)) = hooked {
                    s.push_str("; ");
                    s.push_str(&h.hooks.ended_command_powershell());
                }
                (
                    shell.to_string(),
                    vec!["-NoLogo".into(), "-NoExit".into(), "-Command".into(), s],
                )
            }
            "cmd" => {
                // cmd /K call <agent> <args…> & <exe> event - ended: the shell outlives the
                // agent, so it reports the agent's end. `call` first keeps cmd from stripping
                // the quotes around a quoted path later on the line (its rule for a command
                // line that starts with a quote).
                let mut args = vec!["/K".to_string()];
                if hooked.is_some() {
                    args.push("call".into());
                }
                args.push(exec.to_string());
                args.extend(exec_args.iter().cloned());
                if let Some((_, h)) = hooked {
                    args.extend(h.hooks.ended_args_cmd());
                }
                (shell.to_string(), args)
            }
            "gitbash" | "bash" | "zsh" | "fish" => {
                // bash -i -c "claude arg1 arg2; exec bash -i"
                fn q(s: &str) -> String {
                    format!("'{}'", s.replace('\'', "'\\''"))
                }
                let mut s = if prefix.is_empty() {
                    String::from(agent_bin)
                } else {
                    q(exec)
                };
                for a in &exec_args {
                    s.push(' ');
                    s.push_str(&q(a));
                }
                // The shell outlives the agent: it reports the agent's end.
                if let Some((_, h)) = hooked {
                    s.push_str("; ");
                    s.push_str(&h.hooks.ended_command_posix());
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
                os_default(exec, &exec_args)
            }
        }
    } else {
        os_default(exec, &exec_args)
    };
    let mut env: Vec<(String, String)> = Vec::new();
    // Tag the terminal so Claude Code's OTEL telemetry attributes sessions to this app
    // (telemetry reads `terminal.type` from TERM_PROGRAM; without this we'd land in the
    // Unknown bucket). Always set — no user-facing toggle.
    env.push(("TERM_PROGRAM".into(), "xshell.sh".into()));
    // Describe the terminal xterm.js emulates. A GUI-launched app inherits no TERM (or the
    // TERM of the terminal that started it), and without one terminfo consumers fall back to
    // a dumb terminal: TUIs render monochrome and `clear`/`tput` fail.
    env.push(("TERM".into(), "xterm-256color".into()));
    env.push(("COLORTERM".into(), "truecolor".into()));
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
    if let Some((_, h)) = hooked {
        env.extend(h.env());
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
fn os_default(exec: &str, exec_args: &[String]) -> (String, Vec<String>) {
    if cfg!(windows) {
        let mut args = vec!["/C".to_string(), exec.to_string()];
        args.extend(exec_args.iter().cloned());
        ("cmd.exe".to_string(), args)
    } else {
        (exec.to_string(), exec_args.to_vec())
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

    // Every plan carries the terminal variables first, then `extra`.
    fn plan_env(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v = env(&[
            ("TERM_PROGRAM", "xshell.sh"),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ]);
        v.extend(env(extra));
        v
    }

    const CLAUDE_ENV: &[(&str, &str)] = &[
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
        assert_eq!(plan.env, plan_env(CLAUDE_ENV));
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
        assert_eq!(plan.env, plan_env(CLAUDE_ENV));
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
            assert_eq!(plan.env, plan_env(&[]), "{agent}");
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
        assert_eq!(off(None, None), plan_env(CLAUDE_ENV));
        assert_eq!(off(Some(true), Some(true)), plan_env(CLAUDE_ENV));
        assert_eq!(
            off(Some(false), None),
            plan_env(&[("CLAUDE_CODE_FORCE_SYNC_OUTPUT", "1")])
        );
        assert_eq!(
            off(None, Some(false)),
            plan_env(&[("CLAUDE_CODE_NO_FLICKER", "1")])
        );
        assert_eq!(off(Some(false), Some(false)), plan_env(&[]));
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
        // Raw shells host no claude process, so only the terminal variables are set.
        assert_eq!(plan.env, plan_env(&[]));
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
        assert_eq!(plan.env, plan_env(CLAUDE_ENV));
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
            "skipPermissions": true,
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
                skip_permissions: Some(true),
                launch_prefix: None,
            }
        );
        // Optional fields may be missing.
        let s: LaunchSpec = serde_json::from_value(serde_json::json!({"cwd": ""})).unwrap();
        assert_eq!(s, LaunchSpec::default());
    }

    fn skipping(s: LaunchSpec) -> LaunchSpec {
        LaunchSpec {
            skip_permissions: Some(true),
            ..s
        }
    }

    #[cfg(unix)]
    #[test]
    fn launch_skip_permissions_claude_flag() {
        let fx = Fixture::new();
        let cwd = "/work/app";
        let s = skipping(LaunchSpec {
            session_id: Some("sid".into()),
            ..spec(cwd)
        });
        let plan = plan_command(&fx.ctx(), &s).unwrap();
        assert_eq!(
            plan.args,
            strings(&["--dangerously-skip-permissions", "--session-id", "sid"])
        );
        assert_builder(&plan);

        fx.write(
            format!(
                "home/.claude/projects/{}/sid.jsonl",
                encode_project_name(cwd)
            ),
            "{}\n",
        );
        let plan = plan_command(&fx.ctx(), &s).unwrap();
        assert_eq!(
            plan.args,
            strings(&["--dangerously-skip-permissions", "--resume", "sid"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn launch_skip_permissions_codex_after_subcommand() {
        let fx = Fixture::new();
        let codex = |sid: Option<&str>| {
            plan_command(
                &fx.ctx(),
                &skipping(LaunchSpec {
                    agent: Some("codex".into()),
                    session_id: sid.map(str::to_string),
                    ..spec("/w")
                }),
            )
            .unwrap()
            .args
        };
        assert_eq!(
            codex(Some("id1")),
            strings(&[
                "resume",
                "--dangerously-bypass-approvals-and-sandbox",
                "id1"
            ])
        );
        // A new codex chat has no `resume` subcommand: the flag is the only argument.
        assert_eq!(
            codex(None),
            strings(&["--dangerously-bypass-approvals-and-sandbox"])
        );
    }

    #[test]
    fn launch_skip_permissions_ignored_unsupported_and_raw() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        for agent in ["cursor", "opencode", "antigravity"] {
            let s = LaunchSpec {
                agent: Some(agent.into()),
                session_id: Some("id1".into()),
                ..spec("/w")
            };
            assert_eq!(
                plan_command(&ctx, &skipping(s.clone())).unwrap(),
                plan_command(&ctx, &s).unwrap(),
                "{agent}"
            );
        }
        let raw = LaunchSpec {
            shell_mode: Some("raw".into()),
            shell_command: Some("zsh".into()),
            ..spec("/w")
        };
        let plan = plan_command(&ctx, &skipping(raw)).unwrap();
        assert_eq!(plan.program, "zsh");
        assert!(plan.args.is_empty());
    }

    #[test]
    fn launch_skip_permissions_false_none_unchanged() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        for agent in ["claude", "codex"] {
            let s = LaunchSpec {
                agent: Some(agent.into()),
                session_id: Some("id1".into()),
                ..spec("/w")
            };
            let off = LaunchSpec {
                skip_permissions: Some(false),
                ..s.clone()
            };
            let expected = resume_args(&ctx, agent_binary(Some(agent)), Some("id1"), "/w");
            assert_eq!(
                plan_command(&ctx, &s).unwrap(),
                plan_command(&ctx, &off).unwrap()
            );
            if !cfg!(windows) {
                assert_eq!(plan_command(&ctx, &s).unwrap().args, expected, "{agent}");
            }
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_skip_permissions_in_wrappers() {
        let fx = Fixture::new();
        let cwd = "/w";
        fx.write(
            format!(
                "home/.claude/projects/{}/sid.jsonl",
                encode_project_name(cwd)
            ),
            "{}\n",
        );
        let wrapped = |shell_id: &str, shell: &str, agent: &str| {
            plan_command(
                &fx.ctx(),
                &skipping(LaunchSpec {
                    agent: Some(agent.into()),
                    session_id: Some("sid".into()),
                    shell_id: Some(shell_id.into()),
                    shell_command: Some(shell.into()),
                    ..spec(cwd)
                }),
            )
            .unwrap()
            .args
        };
        assert_eq!(
            wrapped("bash", "bash", "claude"),
            strings(&[
                "-i",
                "-c",
                "claude '--dangerously-skip-permissions' '--resume' 'sid'; exec bash -i"
            ])
        );
        assert_eq!(
            wrapped("pwsh", "pwsh", "claude"),
            strings(&[
                "-NoLogo",
                "-NoExit",
                "-Command",
                "& 'claude' '--dangerously-skip-permissions' '--resume' 'sid'"
            ])
        );
        assert_eq!(
            wrapped("cmd", "cmd.exe", "codex"),
            strings(&[
                "/K",
                "codex",
                "resume",
                "--dangerously-bypass-approvals-and-sandbox",
                "sid"
            ])
        );
    }

    #[cfg(unix)]
    fn prefixed(s: LaunchSpec) -> LaunchSpec {
        LaunchSpec {
            launch_prefix: Some(strings(&["/opt/proxy exec", "--quiet"])),
            ..s
        }
    }

    #[cfg(unix)]
    #[test]
    fn launch_prefix_runs_agent_under_it() {
        let fx = Fixture::new();
        let plan = plan_command(
            &fx.ctx(),
            &skipping(prefixed(LaunchSpec {
                agent: Some("codex".into()),
                session_id: Some("id1".into()),
                ..spec("/w")
            })),
        )
        .unwrap();
        assert_eq!(plan.program, "/opt/proxy exec");
        assert_eq!(
            plan.args,
            strings(&[
                "--quiet",
                "codex",
                "resume",
                "--dangerously-bypass-approvals-and-sandbox",
                "id1"
            ])
        );
        // The prefix is transparent to the agent's environment.
        assert_eq!(plan.env, plan_env(&[]));
        assert_builder(&plan);

        // An empty prefix is no prefix.
        let bare = plan_command(
            &fx.ctx(),
            &LaunchSpec {
                launch_prefix: Some(vec![]),
                ..spec("/w")
            },
        )
        .unwrap();
        assert_eq!(bare.program, "claude");
        assert!(bare.args.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_prefix_in_wrappers_and_raw() {
        let fx = Fixture::new();
        let wrapped = |shell_id: &str, shell: &str| {
            plan_command(
                &fx.ctx(),
                &prefixed(LaunchSpec {
                    session_id: Some("sid".into()),
                    shell_id: Some(shell_id.into()),
                    shell_command: Some(shell.into()),
                    ..spec("/w")
                }),
            )
            .unwrap()
            .args
        };
        assert_eq!(
            wrapped("bash", "bash"),
            strings(&[
                "-i",
                "-c",
                "'/opt/proxy exec' '--quiet' 'claude' '--session-id' 'sid'; exec bash -i"
            ])
        );
        assert_eq!(
            wrapped("pwsh", "pwsh"),
            strings(&[
                "-NoLogo",
                "-NoExit",
                "-Command",
                "& '/opt/proxy exec' '--quiet' 'claude' '--session-id' 'sid'"
            ])
        );
        assert_eq!(
            wrapped("cmd", "cmd.exe"),
            strings(&[
                "/K",
                "/opt/proxy exec",
                "--quiet",
                "claude",
                "--session-id",
                "sid"
            ])
        );

        // A raw shell runs no agent, so there is nothing to prefix.
        let raw = plan_command(
            &fx.ctx(),
            &prefixed(LaunchSpec {
                shell_mode: Some("raw".into()),
                shell_command: Some("zsh".into()),
                ..spec("/w")
            }),
        )
        .unwrap();
        assert_eq!(raw.program, "zsh");
        assert!(raw.args.is_empty());
    }

    #[test]
    fn launch_prefix_serde() {
        let s: LaunchSpec =
            serde_json::from_value(serde_json::json!({"cwd": "/w", "launchPrefix": ["p", "-x"]}))
                .unwrap();
        assert_eq!(s.launch_prefix, Some(strings(&["p", "-x"])));
        // Absent stays absent on the wire, so older Daemons read the spec unchanged.
        let v = serde_json::to_value(LaunchSpec::default()).unwrap();
        assert!(v.get("launchPrefix").is_none());
    }

    #[test]
    fn relaunch_spec_validation() {
        let base = LaunchSpec {
            agent: Some("codex".into()),
            session_id: Some("id1".into()),
            shell_id: Some("bash".into()),
            shell_command: Some("bash".into()),
            fullscreen_rendering: Some(false),
            force_sync_output: Some(true),
            ..spec("/w")
        };
        assert_eq!(
            relaunch_spec(&base, true),
            Ok(LaunchSpec {
                skip_permissions: Some(true),
                ..base.clone()
            })
        );
        assert_eq!(
            relaunch_spec(&skipping(base.clone()), false)
                .unwrap()
                .skip_permissions,
            Some(false)
        );
        // Claude is the default agent.
        let claude = LaunchSpec {
            agent: None,
            ..base.clone()
        };
        assert!(relaunch_spec(&claude, true).is_ok());

        let refused = |s: LaunchSpec| relaunch_spec(&s, true).unwrap_err();
        assert_eq!(
            refused(LaunchSpec {
                shell_mode: Some("raw".into()),
                ..base.clone()
            }),
            "a raw shell has no permission prompts to skip"
        );
        assert_eq!(
            refused(LaunchSpec {
                agent: Some("cursor".into()),
                ..base.clone()
            }),
            "cursor-agent has no flag to skip permission prompts"
        );
        for sid in [None, Some(String::new())] {
            assert_eq!(
                refused(LaunchSpec {
                    session_id: sid,
                    ..base.clone()
                }),
                "no session to resume"
            );
        }
    }

    #[test]
    fn relaunch_keeps_direct_agent() {
        // Who sees a Terminal (a Mobile only direct agents) must not change under its UUID.
        let agent = LaunchSpec {
            agent: Some("claude".into()),
            shell_mode: Some("claude".into()),
            session_id: Some("id1".into()),
            ..spec("/w")
        };
        let wrapped = LaunchSpec {
            shell_command: Some("/bin/sh".into()),
            shell_id: Some("bash".into()),
            ..agent.clone()
        };
        let prefixed = LaunchSpec {
            launch_prefix: Some(strings(&["env"])),
            ..agent.clone()
        };
        for s in [&agent, &wrapped, &prefixed] {
            for skip in [true, false] {
                let next = relaunch_spec(s, skip).unwrap();
                assert_eq!(next.is_direct_agent(), s.is_direct_agent(), "{s:?}");
            }
        }
        assert!(agent.is_direct_agent());
        assert!(!wrapped.is_direct_agent() && !prefixed.is_direct_agent());
    }

    // ── Agent hooks ──

    fn hooks() -> crate::agent_status::AgentHooks {
        crate::agent_status::AgentHooks {
            exe: "/opt/x shell/xshelld".into(),
            endpoint: "/run/x/daemon.sock".into(),
            claude_settings: "/h/.xshell/daemon/claude-hooks.json".into(),
        }
    }

    const TID: &str = "6f1c1a8e-0000-4000-8000-000000000001";

    fn hooked(ctx: &HostCtx, s: &LaunchSpec, h: &crate::agent_status::AgentHooks) -> CommandPlan {
        plan_command_with(
            ctx,
            s,
            Some(TerminalHooks {
                hooks: h,
                terminal: TID.parse().unwrap(),
                run: 3,
            }),
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn hook_env() -> Vec<(String, String)> {
        env(&[
            ("XSHELL_TERMINAL_ID", &format!("{TID}.3")),
            ("XSHELL_EVENT_SOCKET", "/run/x/daemon.sock"),
        ])
    }

    #[cfg(unix)]
    fn codex_overrides() -> Vec<String> {
        hooks().codex_overrides()
    }

    #[cfg(unix)]
    #[test]
    fn launch_claude_with_hooks_adds_settings_and_env() {
        let fx = Fixture::new();
        let h = hooks();
        let plan = hooked(
            &fx.ctx(),
            &LaunchSpec {
                session_id: Some("sid".into()),
                ..spec("/w")
            },
            &h,
        );
        assert_eq!(plan.program, "claude");
        assert_eq!(
            plan.args,
            strings(&[
                "--session-id",
                "sid",
                "--settings",
                "/h/.xshell/daemon/claude-hooks.json"
            ])
        );
        let mut want = plan_env(CLAUDE_ENV);
        want.extend(hook_env());
        assert_eq!(plan.env, want);
        assert_builder(&plan);
    }

    #[cfg(unix)]
    #[test]
    fn launch_codex_with_hooks_appends_overrides_after_resume() {
        let fx = Fixture::new();
        let h = hooks();
        let plan = hooked(
            &fx.ctx(),
            &LaunchSpec {
                agent: Some("codex".into()),
                session_id: Some("id1".into()),
                ..spec("/w")
            },
            &h,
        );
        let mut want = strings(&["resume", "id1"]);
        want.extend(codex_overrides());
        assert_eq!(plan.args, want);
        let mut env = plan_env(&[]);
        env.extend(hook_env());
        assert_eq!(plan.env, env);
        // A new chat: the overrides are the only arguments.
        let bare = hooked(
            &fx.ctx(),
            &LaunchSpec {
                agent: Some("codex".into()),
                ..spec("/w")
            },
            &h,
        );
        assert_eq!(bare.args, codex_overrides());
    }

    #[cfg(unix)]
    #[test]
    fn launch_codex_hooks_with_skip_permissions() {
        let fx = Fixture::new();
        let h = hooks();
        let plan = hooked(
            &fx.ctx(),
            &skipping(LaunchSpec {
                agent: Some("codex".into()),
                session_id: Some("id1".into()),
                ..spec("/w")
            }),
            &h,
        );
        let mut want = strings(&[
            "resume",
            "--dangerously-bypass-approvals-and-sandbox",
            "id1",
        ]);
        want.extend(codex_overrides());
        assert_eq!(plan.args, want);
        let claude = hooked(&fx.ctx(), &skipping(spec("/w")), &h);
        assert_eq!(
            claude.args,
            strings(&[
                "--dangerously-skip-permissions",
                "--settings",
                "/h/.xshell/daemon/claude-hooks.json"
            ])
        );
    }

    #[test]
    fn launch_hooks_ignored_for_raw_and_other_agents() {
        let fx = Fixture::new();
        let ctx = fx.ctx();
        let h = hooks();
        let raw = LaunchSpec {
            shell_mode: Some("raw".into()),
            shell_command: Some("zsh".into()),
            ..spec("/w")
        };
        assert_eq!(hooked(&ctx, &raw, &h), plan_command(&ctx, &raw).unwrap());
        for agent in ["cursor", "opencode", "antigravity"] {
            let s = LaunchSpec {
                agent: Some(agent.into()),
                session_id: Some("id1".into()),
                ..spec("/w")
            };
            assert_eq!(
                hooked(&ctx, &s, &h),
                plan_command(&ctx, &s).unwrap(),
                "{agent}"
            );
        }
    }

    #[test]
    fn launch_cmd_wrapper_reports_agent_end() {
        let fx = Fixture::new();
        let h = hooks();
        let plan = hooked(
            &fx.ctx(),
            &LaunchSpec {
                agent: Some("codex".into()),
                shell_id: Some("cmd".into()),
                shell_command: Some("cmd.exe".into()),
                ..spec("/w")
            },
            &h,
        );
        assert_eq!(plan.program, "cmd.exe");
        let mut want = strings(&["/K", "call", "codex"]);
        want.extend(h.codex_overrides());
        want.extend(strings(&[
            "&",
            "/opt/x shell/xshelld",
            "event",
            "-",
            "ended",
        ]));
        assert_eq!(plan.args, want);
        assert_eq!(
            h.ended_args_cmd(),
            strings(&["&", "/opt/x shell/xshelld", "event", "-", "ended"])
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_hooks_inside_shell_wrapper() {
        let fx = Fixture::new();
        let h = hooks();
        let wrapped = |shell_id: &str, shell: &str, agent: &str| {
            hooked(
                &fx.ctx(),
                &LaunchSpec {
                    agent: Some(agent.into()),
                    shell_id: Some(shell_id.into()),
                    shell_command: Some(shell.into()),
                    ..spec("/w")
                },
                &h,
            )
        };
        let bash = wrapped("bash", "/bin/bash", "claude");
        assert_eq!(
            bash.args,
            strings(&[
                "-i",
                "-c",
                "claude '--settings' '/h/.xshell/daemon/claude-hooks.json'; \
                 '/opt/x shell/xshelld' event - ended; exec bash -i"
            ])
        );
        assert!(bash.env.ends_with(&hook_env()));
        let zsh = wrapped("zsh", "zsh", "codex");
        let s = &zsh.args[2];
        assert!(s.starts_with("codex '-c' 'notify=["), "{s}");
        assert!(
            s.ends_with("; '/opt/x shell/xshelld' event - ended; exec zsh -i"),
            "{s}"
        );
        let pwsh = wrapped("pwsh", "pwsh", "claude");
        assert_eq!(
            pwsh.args[3],
            "& 'claude' '--settings' '/h/.xshell/daemon/claude-hooks.json'; \
             & '/opt/x shell/xshelld' 'event' '-' 'ended'"
        );
    }
}

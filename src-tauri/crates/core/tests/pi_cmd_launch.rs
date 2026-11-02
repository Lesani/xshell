#![cfg(windows)]
//! Execute real CMD boundaries from the compiled planner. No Pi, provider, Daemon or UI.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::agent_status::{
    AgentHooks, TerminalHooks, EVENT_EXE_ENV, EVENT_SOCKET_ENV, TERMINAL_ID_ENV,
};
use xshell_core::ctx::HostCtx;
use xshell_core::launch::{plan_command_with, LaunchSpec};

fn execute_cmd_case(shape: &str, poison: Option<(&str, bool)>) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path();
    // Relative owned paths keep the no-whitespace attack shape even if TEMP has spaces.
    // They name real fixture files under this test's private working directory.
    fs::create_dir(cwd.join("owned")).unwrap();
    fs::write(cwd.join("owned/pi-status.js"), "// fixture only").unwrap();
    fs::write(
        cwd.join("owned/helper.cmd"),
        "@echo %*>helper-args.txt\r\nexit /b 0\r\n",
    )
    .unwrap();
    let probe = "x86-probe.cmd";
    fs::write(cwd.join(probe), "@echo unintended>probe-ran\r\n").unwrap();
    fs::write(
        cwd.join("logger.ps1"),
        "ConvertTo-Json -Compress -InputObject @($args) | Set-Content -Encoding UTF8 $env:X86_ARGV\n\
         ConvertTo-Json -Compress -InputObject @($env:XSHELL_EVENT_EXE, $env:XSHELL_EVENT_SOCKET, $env:XSHELL_TERMINAL_ID) | Set-Content -Encoding UTF8 $env:X86_HOOK_ENV\n",
    )
    .unwrap();
    fs::write(
        cwd.join("pi.cmd"),
        "@echo off\r\npowershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File logger.ps1 %*\r\necho ready>cmd-ready\r\nexit /b 0\r\n",
    )
    .unwrap();
    let mut hooks = AgentHooks {
        // CMD command position requires native separators, unlike a file-valued argv.
        exe: PathBuf::from("owned").join("helper.cmd"),
        endpoint: "private-test-endpoint".into(),
        claude_settings: cwd.join("unused.json"),
        pi_extension: Some(PathBuf::from("owned/pi-status.js")),
    };
    if let Some((directory, helper)) = poison {
        fs::create_dir(cwd.join(directory)).unwrap();
        let path = Path::new(directory).join(if helper { "helper.cmd" } else { "pi-status.js" });
        fs::write(cwd.join(&path), "fixture").unwrap();
        if helper {
            hooks.exe = path;
        } else {
            hooks.pi_extension = Some(path);
        }
    }
    let mut spec = LaunchSpec {
        agent: Some("pi".into()),
        session_id: Some("session".into()),
        cwd: cwd.to_string_lossy().into_owned(),
        ..Default::default()
    };
    match shape {
        "explicit-cmd" => {
            spec.shell_id = Some("cmd".into());
            spec.shell_command = Some("cmd.exe".into());
        }
        "prefix-fallback" => spec.launch_prefix = Some(vec!["call".into()]),
        "unknown-shell-fallback" => {
            spec.shell_id = Some("unknown-shell".into());
            spec.shell_command = Some("unused-shell".into());
        }
        _ => unreachable!(),
    }
    let plan = plan_command_with(
        &HostCtx::with_home(cwd, cwd),
        &spec,
        Some(TerminalHooks {
            hooks: &hooks,
            terminal: Uuid::new_v4(),
            run: 7,
        }),
    )
    .unwrap();
    if poison.is_some() {
        assert_eq!(
            plan,
            plan_command_with(&HostCtx::with_home(cwd, cwd), &spec, None).unwrap(),
            "declining hooks must retain exactly the preexisting launch shape"
        );
    }
    // Use the actual launch builder's full environment, including inherited-hook removal.
    let builder = plan.to_command_builder();
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(cwd)
        .env_clear()
        .envs(builder.iter_full_env_as_str())
        .env("X86_ARGV", cwd.join("argv.json"))
        .env("X86_HOOK_ENV", cwd.join("hook-env.json"))
        .env("X86_EXPAND", "EXPANDED")
        .env("PATH", std::env::var_os("PATH").unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::from(
            fs::File::create(cwd.join("stdout.log")).unwrap(),
        ))
        .stderr(Stdio::from(
            fs::File::create(cwd.join("stderr.log")).unwrap(),
        ));
    let diagnostics =
        || {
            let read = |name: &str| {
                String::from_utf8_lossy(&fs::read(cwd.join(name)).unwrap_or_default()).into_owned()
            };
            format!(
            "{shape} / {poison:?}; program={:?}; args={:?}; cwd={cwd:?}; stdout={:?}; stderr={:?}",
            plan.program, plan.args, read("stdout.log"), read("stderr.log")
        )
        };
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        // /K intentionally stays alive. Only send its exit after the logger has returned,
        // so the nested PowerShell process cannot consume the shell's exit input.
        if cwd.join("cmd-ready").exists() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"exit\r\n");
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("CMD fixture did not exit: {}", diagnostics());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "CMD fixture failed ({status}): {}",
        diagnostics()
    );
    let json = |name: &str| -> serde_json::Value {
        let text = fs::read_to_string(cwd.join(name)).unwrap();
        serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap()
    };
    let mut expected = vec!["--session-id", "session"];
    if poison.is_none() {
        expected.extend(["--extension", "owned/pi-status.js"]);
    }
    assert_eq!(
        json("argv.json"),
        serde_json::json!(expected),
        "{shape} / {poison:?}"
    );
    assert!(
        !cwd.join("probe-ran").exists(),
        "CMD executed part of an owned path"
    );
    let helper_args = cwd.join("helper-args.txt");
    if poison.is_none() && shape == "explicit-cmd" {
        assert_eq!(
            fs::read_to_string(&helper_args)
                .unwrap_or_else(|e| panic!("missing helper argv: {e}; {}", diagnostics()))
                .trim(),
            "event - ended",
            "{}",
            diagnostics()
        );
    } else {
        assert!(
            !helper_args.exists(),
            "unexpected helper: {}",
            diagnostics()
        );
    }
    if poison.is_some() {
        assert_eq!(json("hook-env.json"), serde_json::json!([null, null, null]));
        for key in [EVENT_EXE_ENV, EVENT_SOCKET_ENV, TERMINAL_ID_ENV] {
            assert!(builder.get_env(key).is_none(), "{key} leaked");
        }
    }
}

#[test]
fn pi_cmd_no_space_ampersand_owned_paths_cannot_execute_commands() {
    for shape in ["explicit-cmd", "prefix-fallback", "unknown-shell-fallback"] {
        for helper in [false, true] {
            execute_cmd_case(shape, Some(("A&x86-probe.cmd&Z", helper)));
        }
    }
}

#[test]
fn pi_cmd_percent_expansion_owned_paths_are_not_reinterpreted() {
    for shape in ["explicit-cmd", "prefix-fallback", "unknown-shell-fallback"] {
        for helper in [false, true] {
            execute_cmd_case(shape, Some(("A%X86_EXPAND%Z", helper)));
        }
    }
}

#[test]
fn pi_cmd_safe_literal_paths_preserve_exact_extension_argv() {
    for shape in ["explicit-cmd", "prefix-fallback", "unknown-shell-fallback"] {
        execute_cmd_case(shape, None);
    }
}

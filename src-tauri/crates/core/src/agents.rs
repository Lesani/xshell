use serde::{Deserialize, Serialize};

// ── Agent binary detection ────────────────────────────────────────────
// Settings → Agents shows whether each supported CLI agent is installed on this machine.
// Resolution goes through `where`/`which` instead of the PTY shell because npm installs
// agents as `.cmd`/`.ps1` shims on Windows — `where` resolves those reliably, a spawned
// shell lookup would not. The version probe then runs the binary once with `--version`.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentBinaryProbe {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
}

pub fn detect_agent_binary(binary: String) -> Result<AgentBinaryProbe, String> {
    // The name ends up in a process invocation — only accept known agent binaries.
    if binary != "claude"
        && binary != "codex"
        && binary != "cursor-agent"
        && binary != "opencode"
        && binary != "agy"
    {
        return Err(format!("Unknown agent binary: {}", binary));
    }
    use std::process::Command;

    let mut lookup = if cfg!(target_os = "windows") {
        Command::new("where")
    } else {
        Command::new("which")
    };
    lookup.arg(&binary);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        lookup.creation_flags(0x08000000);
    } // CREATE_NO_WINDOW

    // `where` can return multiple matches (e.g. claude.cmd + claude.ps1) — the first line is
    // the one PATH order would pick, same as what a terminal would run.
    let path = lookup
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty());

    let Some(path) = path else {
        return Ok(AgentBinaryProbe {
            installed: false,
            path: None,
            version: None,
        });
    };

    // Version probe is best-effort: a missing/failing `--version` still counts as installed.
    // On Windows the resolved path is usually an npm `.cmd` shim, which CreateProcess can't
    // exec directly — route through cmd.exe.
    #[cfg(target_os = "windows")]
    let mut ver_cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", &binary, "--version"]);
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut ver_cmd = {
        let mut c = Command::new(&binary);
        c.arg("--version");
        c
    };

    let version = ver_cmd
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty());

    Ok(AgentBinaryProbe {
        installed: true,
        path: Some(path),
        version,
    })
}

use serde::{Deserialize, Serialize};

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
    /// Start the agent with its "skip permission prompts" flag (see `permission_flag` in `xshell-core`).
    /// `None` is off; agents without such a flag and raw shells ignore it.
    pub skip_permissions: Option<bool>,
    /// A command the agent runs under, as argv words: `[prefix..., agent, agent args...]`
    /// (e.g. a proxy wrapper that sets up the environment and then execs the agent). Raw
    /// shells ignore it. Remote Terminals get it from the Host's configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_prefix: Option<Vec<String>>,
}

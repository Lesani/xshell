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

impl LaunchSpec {
    /// Whether this spec runs an agent directly. A raw shell, a wrapping shell (it stays open
    /// after the agent exits) or a launch prefix (client-supplied argv run before the agent)
    /// is not a direct agent. The Daemon and the Mobile share this one definition.
    pub fn is_direct_agent(&self) -> bool {
        let unset = |v: &Option<String>| v.as_deref().is_none_or(str::is_empty);
        matches!(self.shell_mode.as_deref(), None | Some("claude"))
            && unset(&self.shell_command)
            && unset(&self.shell_id)
            && self.launch_prefix.is_none()
    }

    /// The agent a direct-agent spec runs: `agent`, or `"claude"` when it is unset or empty
    /// (the launcher's default). `None` when the spec is not a direct agent.
    pub fn direct_agent(&self) -> Option<&str> {
        if !self.is_direct_agent() {
            return None;
        }
        Some(
            self.agent
                .as_deref()
                .filter(|a| !a.is_empty())
                .unwrap_or("claude"),
        )
    }

    /// The agent session this spec runs, as (agent, session id): `agent` (or `"claude"` when
    /// it is unset or empty) and a non-empty `session_id`. `None` for a raw shell and for a
    /// spec without a session id. Wrapped agents and launch prefixes count too: they resume
    /// the same session. A Host runs one session in at most one live Terminal (capability
    /// `term.open-existing`); the Daemon and the Mobile share this one key.
    pub fn agent_session(&self) -> Option<(&str, &str)> {
        if self.shell_mode.as_deref() == Some("raw") {
            return None;
        }
        let sid = self.session_id.as_deref().filter(|s| !s.is_empty())?;
        let agent = self
            .agent
            .as_deref()
            .filter(|a| !a.is_empty())
            .unwrap_or("claude");
        Some((agent, sid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(cwd: &str) -> LaunchSpec {
        LaunchSpec {
            agent: Some("claude".into()),
            session_id: Some("s1".into()),
            cwd: cwd.into(),
            ..Default::default()
        }
    }

    fn raw_shell(cwd: &str) -> LaunchSpec {
        LaunchSpec {
            cwd: cwd.into(),
            shell_mode: Some("raw".into()),
            ..Default::default()
        }
    }

    fn not_direct() -> Vec<LaunchSpec> {
        vec![
            raw_shell("/p"),
            LaunchSpec {
                shell_mode: Some("raw".into()),
                ..agent("/p")
            },
            LaunchSpec {
                shell_mode: Some("weird".into()),
                ..agent("/p")
            },
            LaunchSpec {
                shell_command: Some("bash".into()),
                ..agent("/p")
            },
            LaunchSpec {
                shell_id: Some("bash".into()),
                ..agent("/p")
            },
            LaunchSpec {
                launch_prefix: Some(vec!["env".into()]),
                ..agent("/p")
            },
            LaunchSpec {
                launch_prefix: Some(vec![]),
                ..agent("/p")
            },
        ]
    }

    #[test]
    fn direct_agent_rules() {
        assert!(agent("/p").is_direct_agent());
        assert!(LaunchSpec {
            shell_mode: Some("claude".into()),
            shell_command: Some(String::new()),
            shell_id: Some(String::new()),
            ..agent("/p")
        }
        .is_direct_agent());
        for s in not_direct() {
            assert!(!s.is_direct_agent(), "{s:?}");
        }
    }

    #[test]
    fn direct_agent_defaults_to_claude() {
        let unset = LaunchSpec {
            agent: None,
            ..agent("/p")
        };
        assert_eq!(unset.direct_agent(), Some("claude"));
        let empty = LaunchSpec {
            agent: Some(String::new()),
            ..agent("/p")
        };
        assert_eq!(empty.direct_agent(), Some("claude"));
        assert_eq!(agent("/p").direct_agent(), Some("claude"));
        let codex = LaunchSpec {
            agent: Some("codex".into()),
            ..agent("/p")
        };
        assert_eq!(codex.direct_agent(), Some("codex"));
        for s in not_direct() {
            assert_eq!(s.direct_agent(), None, "{s:?}");
        }
    }

    #[test]
    fn agent_session_rules() {
        assert_eq!(agent("/p").agent_session(), Some(("claude", "s1")));
        // A raw shell never runs a session, even with a stray id.
        assert_eq!(
            LaunchSpec {
                shell_mode: Some("raw".into()),
                ..agent("/p")
            }
            .agent_session(),
            None
        );
        for sid in [None, Some(String::new())] {
            let s = LaunchSpec {
                session_id: sid,
                ..agent("/p")
            };
            assert_eq!(s.agent_session(), None, "{s:?}");
        }
        for a in [None, Some(String::new())] {
            let s = LaunchSpec {
                agent: a,
                ..agent("/p")
            };
            assert_eq!(s.agent_session(), Some(("claude", "s1")), "{s:?}");
        }
        let codex = LaunchSpec {
            agent: Some("codex".into()),
            ..agent("/p")
        };
        assert_eq!(codex.agent_session(), Some(("codex", "s1")));
        // Wrapped and prefixed launches resume the same session.
        let wrapped = LaunchSpec {
            shell_id: Some("bash".into()),
            ..agent("/p")
        };
        assert_eq!(wrapped.agent_session(), Some(("claude", "s1")));
        let prefixed = LaunchSpec {
            launch_prefix: Some(vec!["env".into()]),
            ..agent("/p")
        };
        assert_eq!(prefixed.agent_session(), Some(("claude", "s1")));
    }
}

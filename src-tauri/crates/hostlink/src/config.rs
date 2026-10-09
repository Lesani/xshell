//! A configured Remote Host, as the frontend stores it in `settings.json` and pushes it in.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use xshell_core::launch::LaunchSpec;

/// The one definition of a Host id. Generated ids are `h_` plus 8 characters of `[a-z0-9]`.
/// The frontend's `parseProjectKey` uses the same pattern; a Vitest reads this line.
pub const HOST_ID_PATTERN: &str = "^h_[a-z0-9]{8}$";

/// The Local Host's id on the wire (`hosts:status`, `hosts:terminals`, `host_term_*`) when
/// its Terminals run in a Daemon ([`crate::Manager::set_local`]). Never a configured Host:
/// it does not match [`HOST_ID_PATTERN`], so `validate` refuses it.
pub const LOCAL_HOST_ID: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    pub id: String,
    pub name: String,
    pub ssh_target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Runs an existing Daemon instead of the managed install: `<daemonCommand> connect`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_command: Option<String>,
    /// Per agent id (`claude`, `codex`, ...): a command the agent is launched under on this
    /// Host, e.g. a proxy wrapper. Shell words; the agent and its arguments follow them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launch_prefixes: BTreeMap<String, String>,
}

/// The agent ids a launch prefix can be set for (the frontend's `AgentId`).
pub const PREFIX_AGENTS: &[&str] = &["claude", "codex", "cursor", "opencode", "antigravity"];

impl HostConfig {
    /// The override, trimmed; blank counts as none.
    pub fn daemon_override(&self) -> Option<&str> {
        self.daemon_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The launch prefix for the agent `spec` starts, as argv words. `None` for raw shells and
    /// agents without one (blank counts as none). The agent id defaults to `claude`, as in
    /// [`xshell_core::launch::agent_binary`].
    pub fn launch_prefix(&self, spec: &LaunchSpec) -> Option<Vec<String>> {
        if spec.shell_mode.as_deref() == Some("raw") {
            return None;
        }
        let agent = spec.agent.as_deref().unwrap_or("claude");
        let words = shlex::split(self.launch_prefixes.get(agent)?)?;
        (!words.is_empty()).then_some(words)
    }

    /// Whether a change from `old` needs a new connection (the name and color do not).
    pub fn connection_differs(&self, old: &HostConfig) -> bool {
        self.ssh_target != old.ssh_target || self.daemon_override() != old.daemon_override()
    }
}

/// `HOST_ID_PATTERN`, by hand (no regex dependency).
pub fn is_host_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("h_") else {
        return false;
    };
    rest.len() == 8
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

pub fn validate_one(h: &HostConfig) -> Result<(), String> {
    if !is_host_id(&h.id) {
        return Err(format!(
            "invalid host id {:?}: expected {HOST_ID_PATTERN}",
            h.id
        ));
    }
    let t = &h.ssh_target;
    if t.is_empty() {
        return Err(format!("host {}: the SSH target is empty", h.id));
    }
    if t.chars().count() > 255 {
        return Err(format!(
            "host {}: the SSH target is over 255 characters",
            h.id
        ));
    }
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "host {}: the SSH target contains whitespace or control characters",
            h.id
        ));
    }
    if t.starts_with('-') {
        return Err(format!("host {}: the SSH target starts with '-'", h.id));
    }
    if let Some(c) = &h.daemon_command {
        let c = c.trim();
        if c.is_empty() {
            return Err(format!("host {}: the daemon command is blank", h.id));
        }
        if c.contains('\n') || c.contains('\r') {
            return Err(format!(
                "host {}: the daemon command has a line break",
                h.id
            ));
        }
    }
    for (agent, prefix) in &h.launch_prefixes {
        if !PREFIX_AGENTS.contains(&agent.as_str()) {
            return Err(format!(
                "host {}: launch prefix for unknown agent {agent:?}",
                h.id
            ));
        }
        if prefix.contains('\n') || prefix.contains('\r') {
            return Err(format!(
                "host {}: the {agent} launch prefix has a line break",
                h.id
            ));
        }
        if shlex::split(prefix).is_none() {
            return Err(format!(
                "host {}: the {agent} launch prefix has an unclosed quote",
                h.id
            ));
        }
    }
    Ok(())
}

pub fn validate(hosts: &[HostConfig]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for h in hosts {
        validate_one(h)?;
        if !seen.insert(h.id.as_str()) {
            return Err(format!("duplicate host id {}", h.id));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_host(id: &str, target: &str) -> HostConfig {
    HostConfig {
        id: id.into(),
        name: "Test".into(),
        ssh_target: target.into(),
        color: None,
        daemon_command: None,
        launch_prefixes: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_ids_targets_overrides() {
        let ok = test_host("h_ab12cd34", "user@host");
        assert!(validate(std::slice::from_ref(&ok)).is_ok());
        for bad in [
            "local",
            "h_ABCDEFGH",
            "h_abc",
            "h_abcdefghi",
            "x_abcdefgh",
            "",
        ] {
            assert!(validate(&[test_host(bad, "dev")]).is_err(), "{bad}");
        }
        assert!(validate(&[ok.clone(), ok.clone()]).is_err());
        for bad in ["-oProxyCommand=x", "a b", "", "a\tb", "a\u{7}b"] {
            assert!(
                validate(&[test_host("h_ab12cd34", bad)]).is_err(),
                "{bad:?}"
            );
        }
        assert!(validate(&[test_host("h_ab12cd34", &"x".repeat(256))]).is_err());
        let mut o = ok.clone();
        o.daemon_command = Some("x\ny".into());
        assert!(validate(&[o.clone()]).is_err());
        o.daemon_command = Some("   ".into());
        assert!(validate(&[o.clone()]).is_err());
        o.daemon_command = Some(" ~/bin/xd ".into());
        assert!(validate(&[o.clone()]).is_ok());
        assert_eq!(o.daemon_override(), Some("~/bin/xd"));
    }

    #[test]
    fn launch_prefix_validation() {
        let mut h = test_host("h_ab12cd34", "dev");
        h.launch_prefixes
            .insert("claude".into(), "vamoto-headroom-exec".into());
        h.launch_prefixes
            .insert("codex".into(), "'my wrapper' --flag".into());
        assert!(validate_one(&h).is_ok());
        for (agent, bad) in [("claude", "a\nb"), ("claude", "'unclosed"), ("pi", "wrap")] {
            let mut b = h.clone();
            b.launch_prefixes.insert(agent.into(), bad.into());
            assert!(validate_one(&b).is_err(), "{agent} {bad:?}");
        }
    }

    #[test]
    fn launch_prefix_per_agent() {
        let mut h = test_host("h_ab12cd34", "dev");
        h.launch_prefixes
            .insert("claude".into(), "vamoto-headroom-exec".into());
        h.launch_prefixes
            .insert("codex".into(), "'my wrapper' --flag".into());
        h.launch_prefixes.insert("cursor".into(), "  ".into());
        let spec = |agent: Option<&str>, mode: Option<&str>| LaunchSpec {
            agent: agent.map(Into::into),
            shell_mode: mode.map(Into::into),
            ..Default::default()
        };
        let words = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            h.launch_prefix(&spec(None, None)),
            words(&["vamoto-headroom-exec"])
        );
        assert_eq!(
            h.launch_prefix(&spec(Some("claude"), Some("claude"))),
            words(&["vamoto-headroom-exec"])
        );
        assert_eq!(
            h.launch_prefix(&spec(Some("codex"), None)),
            words(&["my wrapper", "--flag"])
        );
        assert_eq!(h.launch_prefix(&spec(Some("cursor"), None)), None);
        assert_eq!(h.launch_prefix(&spec(Some("opencode"), None)), None);
        assert_eq!(h.launch_prefix(&spec(None, Some("raw"))), None);
    }

    #[test]
    fn json_shape() {
        let h: HostConfig =
            serde_json::from_str(r#"{"id":"h_ab12cd34","name":"Dev","sshTarget":"dev"}"#).unwrap();
        assert_eq!(h.daemon_command, None);
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            r#"{"id":"h_ab12cd34","name":"Dev","sshTarget":"dev"}"#
        );
        let h: HostConfig = serde_json::from_str(
            r#"{"id":"h_ab12cd34","name":"Dev","sshTarget":"dev","launchPrefixes":{"claude":"w"}}"#,
        )
        .unwrap();
        assert_eq!(
            h.launch_prefixes.get("claude").map(String::as_str),
            Some("w")
        );
    }

    #[test]
    fn connection_change_detection() {
        let a = test_host("h_ab12cd34", "dev");
        let mut b = a.clone();
        b.name = "Other".into();
        b.color = Some("#fff".into());
        assert!(!b.connection_differs(&a));
        b.ssh_target = "dev2".into();
        assert!(b.connection_differs(&a));
        let mut c = a.clone();
        c.daemon_command = Some("~/xd".into());
        assert!(c.connection_differs(&a));
        // A launch prefix applies to the next launch; the connection stays.
        let mut d = a.clone();
        d.launch_prefixes.insert("claude".into(), "w".into());
        assert!(!d.connection_differs(&a));
    }
}

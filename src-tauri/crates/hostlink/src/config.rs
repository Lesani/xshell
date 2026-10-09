//! A configured Remote Host, as the frontend stores it in `settings.json` and pushes it in.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The one definition of a Host id. Generated ids are `h_` plus 8 characters of `[a-z0-9]`.
/// The frontend's `parseProjectKey` uses the same pattern; a Vitest reads this line.
pub const HOST_ID_PATTERN: &str = "^h_[a-z0-9]{8}$";

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
}

impl HostConfig {
    /// The override, trimmed; blank counts as none.
    pub fn daemon_override(&self) -> Option<&str> {
        self.daemon_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
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
    fn json_shape() {
        let h: HostConfig =
            serde_json::from_str(r#"{"id":"h_ab12cd34","name":"Dev","sshTarget":"dev"}"#).unwrap();
        assert_eq!(h.daemon_command, None);
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            r#"{"id":"h_ab12cd34","name":"Dev","sshTarget":"dev"}"#
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
    }
}

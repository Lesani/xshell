//! The values the frontend sees: `hosts:status` payloads, snapshots and test results.

use crate::errors::HostErrorHint;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
use xshell_core::protocol::msg::TerminalInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusKind {
    Reconnecting,
    Connected,
    UpgradePending,
    Offline,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Probing,
    Installing,
    Upgrading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IncompatibleReason {
    DaemonOlder,
    DaemonNewer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub host: String,
    pub status: StatusKind,
    pub phase: Option<Phase>,
    pub last_error: Option<String>,
    pub error_hint: Option<HostErrorHint>,
    pub daemon_version: Option<String>,
    pub desktop_version: String,
    pub protocol: Option<u32>,
    /// `linux` or `macos`, from the last probe.
    pub os: Option<String>,
    pub arch: Option<String>,
    pub incompatible_reason: Option<IncompatibleReason>,
    /// ms since the epoch.
    pub next_retry_at: Option<u64>,
    /// When this `status` value began, ms since the epoch.
    pub since_ms: u64,
    /// Changes whenever the Host's connection settings are replaced; Tabs re-attach then.
    pub config_generation: u64,
}

impl HostStatus {
    pub fn initial(host: &str, desktop_version: &str, config_generation: u64) -> Self {
        Self {
            host: host.into(),
            status: StatusKind::Reconnecting,
            phase: None,
            last_error: None,
            error_hint: None,
            daemon_version: None,
            desktop_version: desktop_version.into(),
            protocol: None,
            os: None,
            arch: None,
            incompatible_reason: None,
            next_retry_at: None,
            since_ms: now_ms(),
            config_generation,
        }
    }

    /// Change the kind, restarting `sinceMs` only when it really changes.
    pub fn set_kind(&mut self, k: StatusKind) {
        if self.status != k {
            self.status = k;
            self.since_ms = now_ms();
        }
    }

    pub fn usable(&self) -> bool {
        matches!(
            self.status,
            StatusKind::Connected | StatusKind::UpgradePending
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshot {
    pub status: HostStatus,
    /// `None`: never connected this run.
    pub terminals: Option<Vec<TerminalInfo>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostTestResult {
    pub ok: bool,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub triple: Option<String>,
    pub installed_version: Option<String>,
    pub error: Option<String>,
    pub error_hint: Option<HostErrorHint>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_shape() {
        let mut s = HostStatus::initial("h_ab12cd34", "1.5.0", 7);
        s.since_ms = 1;
        s.phase = Some(Phase::Probing);
        s.incompatible_reason = Some(IncompatibleReason::DaemonOlder);
        s.error_hint = Some(HostErrorHint::HostKey);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "host":"h_ab12cd34","status":"reconnecting","phase":"probing","lastError":null,
                "errorHint":"host-key","daemonVersion":null,"desktopVersion":"1.5.0","protocol":null,
                "os":null,"arch":null,"incompatibleReason":"daemon-older","nextRetryAt":null,
                "sinceMs":1,"configGeneration":7
            })
        );
        s.set_kind(StatusKind::UpgradePending);
        assert_eq!(
            serde_json::to_value(s.status).unwrap(),
            serde_json::json!("upgrade-pending")
        );
        assert!(s.usable());
    }
}

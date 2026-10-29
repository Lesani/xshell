//! The values the frontend sees: `hosts:status` payloads, snapshots and test results.

use crate::errors::HostErrorHint;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
use xshell_protocol::build::BuildId;
use xshell_protocol::msg::TerminalInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusKind {
    Reconnecting,
    Connected,
    UpgradePending,
    /// The managed Daemon runs this version from another build, or from an unknown one
    /// (#49): usable, and "Replace with this Desktop's build" is offered. Nothing switches by
    /// itself, so Desktops on different builds never take turns replacing each other.
    BuildDiffers,
    Offline,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Probing,
    Installing,
    Upgrading,
    Preparing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IncompatibleReason {
    DaemonOlder,
    DaemonNewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateAction {
    Update,
    Replace,
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
    /// The connected (or last) Daemon's build; `None` when unknown or never connected.
    pub daemon_build: Option<BuildId>,
    /// This Desktop's build; `None` when it was built without git.
    pub desktop_build: Option<BuildId>,
    pub protocol: Option<u32>,
    /// The connected Daemon's hello capabilities; empty while not connected.
    pub daemon_capabilities: Vec<String>,
    /// `linux`, `macos` or `windows`, from the last exact platform probe.
    pub os: Option<String>,
    pub arch: Option<String>,
    pub incompatible_reason: Option<IncompatibleReason>,
    /// ms since the epoch.
    pub next_retry_at: Option<u64>,
    /// When this `status` value began, ms since the epoch.
    pub since_ms: u64,
    /// Changes whenever the Host's connection settings are replaced; Tabs re-attach then.
    pub config_generation: u64,
    /// The link generation that last became usable: a request meant for that connection
    /// names it ([`crate::HostHandle::ring_identity`]), including a bound update confirmation.
    pub link_generation: u64,
    /// Retained during reconnect: capability absence alone is not an old-owner diagnosis.
    pub update_unavailable: Option<String>,
    pub update_action: Option<UpdateAction>,
}

impl HostStatus {
    pub fn initial(
        host: &str,
        desktop_version: &str,
        desktop_build: Option<BuildId>,
        config_generation: u64,
    ) -> Self {
        Self {
            host: host.into(),
            status: StatusKind::Reconnecting,
            phase: None,
            last_error: None,
            error_hint: None,
            daemon_version: None,
            desktop_version: desktop_version.into(),
            daemon_build: None,
            desktop_build,
            protocol: None,
            daemon_capabilities: Vec::new(),
            os: None,
            arch: None,
            incompatible_reason: None,
            next_retry_at: None,
            since_ms: now_ms(),
            config_generation,
            link_generation: 0,
            update_unavailable: None,
            update_action: None,
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
            StatusKind::Connected | StatusKind::UpgradePending | StatusKind::BuildDiffers
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
        let mut s = HostStatus::initial("h_ab12cd34", "1.5.0", None, 7);
        s.since_ms = 1;
        s.phase = Some(Phase::Probing);
        s.incompatible_reason = Some(IncompatibleReason::DaemonOlder);
        s.error_hint = Some(HostErrorHint::HostKey);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "host":"h_ab12cd34","status":"reconnecting","phase":"probing","lastError":null,
                "errorHint":"host-key","daemonVersion":null,"desktopVersion":"1.5.0",
                "daemonBuild":null,"desktopBuild":null,"protocol":null,
                "daemonCapabilities":[],"os":null,"arch":null,"incompatibleReason":"daemon-older","nextRetryAt":null,
                "sinceMs":1,"configGeneration":7,"linkGeneration":0,"updateUnavailable":null,"updateAction":null
            })
        );
        s.set_kind(StatusKind::UpgradePending);
        assert_eq!(
            serde_json::to_value(s.status).unwrap(),
            serde_json::json!("upgrade-pending")
        );
        assert!(s.usable());
        s.set_kind(StatusKind::BuildDiffers);
        assert_eq!(
            serde_json::to_value(s.status).unwrap(),
            serde_json::json!("build-differs")
        );
        assert!(s.usable());
        let b = BuildId {
            commit: "85ee920a1b2c".into(),
            dirty: None,
            time: 1,
        };
        s.desktop_build = Some(b.clone());
        s.daemon_build = Some(b);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v["desktopBuild"],
            serde_json::json!({"commit":"85ee920a1b2c","dirty":null,"time":1})
        );
        assert_eq!(v["daemonBuild"], v["desktopBuild"]);
        for k in [
            StatusKind::Offline,
            StatusKind::Reconnecting,
            StatusKind::Incompatible,
        ] {
            s.set_kind(k);
            assert!(!s.usable(), "{k:?}");
        }
    }
}

//! The values the frontend sees: `hosts:status` payloads, snapshots and test results.

use crate::errors::HostErrorHint;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use xshell_protocol::build::BuildId;
use xshell_protocol::msg::TerminalInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Probing,
    Installing,
    Upgrading,
    Preparing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IncompatibleReason {
    DaemonOlder,
    DaemonNewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateAction {
    Update,
    Replace,
}

/// Who supplies the offered update: the Host downloads the latest release itself
/// (`daemon.update-latest`), or this Desktop sends its own build over the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateSource {
    Release,
    Desktop,
}

/// Evidence that this Host cannot accept the remote update path. Not a connectivity error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateUnavailable {
    WindowsOwner,
    DesktopOwned,
}

/// Access-route limitation, independent of evidence about who owns this Daemon (#102).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteUpdateUnavailable {
    /// A Ring Host whose xshell cannot take an update over the Ring (no `daemon.stage`,
    /// or no common protocol): update xshell on that machine itself (`install.sh`).
    RingUpdateUnsupported,
    IdentityBoundAlias,
}

impl UpdateUnavailable {
    pub(crate) fn from_hello(
        os: Option<&str>,
        hello: &xshell_protocol::msg::Hello,
        relay: bool,
    ) -> Option<Self> {
        let has = |cap| hello.capabilities.iter().any(|c| c == cap);
        // Over the Ring a Daemon its Desktop owns is updated on that machine (not "too old",
        // Windows included), unless it takes a staged build (rjw.9 D5: a Windows one whose
        // Desktop restarts it into the selected build is updated like a Linux one).
        // The Daemon says so itself: whether its OS is known yet does not matter.
        if relay && has(xshell_protocol::cap::DAEMON_DESKTOP_OWNED) {
            return (!has(xshell_protocol::cap::DAEMON_STAGE)).then_some(Self::DesktopOwned);
        }
        if os == Some("windows") {
            // Selected restart is explicitly authorized by the owning Windows Desktop.
            return (!has(xshell_protocol::cap::DAEMON_UPGRADE_SELECTED))
                .then_some(Self::WindowsOwner);
        }
        has(xshell_protocol::cap::DAEMON_DESKTOP_OWNED).then_some(Self::DesktopOwned)
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::DesktopOwned => "The Desktop on this Host manages its Daemon; update xshell there instead.",
            Self::WindowsOwner => "To update this Host remotely, first update and reopen xshell on that Windows Host. Its owning Desktop must support Daemon updates.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    /// Transport-only hint for internal HostSync observers; never serialized as authority.
    #[serde(skip)]
    pub(crate) raw_action_target: Option<crate::handle::InputTarget>,
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
    pub update_unavailable: Option<UpdateUnavailable>,
    pub update_action: Option<UpdateAction>,
    /// The version the offered update leads to (the latest release, or this Desktop's);
    /// `None` when nothing is offered. Defaulted so a broker follower reads an older owner's
    /// publication.
    #[serde(default)]
    pub update_version: Option<String>,
    /// Who supplies the offered update; `None` when nothing is offered.
    #[serde(default)]
    pub update_source: Option<UpdateSource>,
    pub route_update_unavailable: Option<RouteUpdateUnavailable>,
}

impl HostStatus {
    pub fn initial(
        host: &str,
        desktop_version: &str,
        desktop_build: Option<BuildId>,
        config_generation: u64,
    ) -> Self {
        Self {
            raw_action_target: None,
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
            update_version: None,
            update_source: None,
            route_update_unavailable: None,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshot {
    pub status: HostStatus,
    /// `None`: never connected this run.
    pub terminals: Option<Vec<TerminalInfo>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Over the Ring a machine with the xshell app is updated there, on every system: a
    /// Windows one is not reported as updatable (or "too old") because its Daemon offers a
    /// selected restart to its own Desktop.
    #[test]
    fn desktop_owned_over_the_ring_on_every_system() {
        let hello = |caps: &[&str]| xshell_protocol::msg::Hello {
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            ..serde_json::from_value(
                serde_json::json!({"protocol":{"min":1,"max":1},"version":"1.6.0"}),
            )
            .unwrap()
        };
        let owned = hello(&[
            xshell_protocol::cap::DAEMON_DESKTOP_OWNED,
            xshell_protocol::cap::DAEMON_UPGRADE_SELECTED,
        ]);
        for os in [Some("windows"), Some("linux"), Some("macos"), None] {
            assert_eq!(
                UpdateUnavailable::from_hello(os, &owned, true),
                Some(UpdateUnavailable::DesktopOwned),
                "{os:?}"
            );
        }
        // Over SSH nothing changes: the owning Windows Desktop authorizes a selected restart.
        assert_eq!(
            UpdateUnavailable::from_hello(Some("windows"), &owned, false),
            None
        );
        assert_eq!(
            UpdateUnavailable::from_hello(Some("linux"), &owned, false),
            Some(UpdateUnavailable::DesktopOwned)
        );
        // An always-on Daemon is not Desktop-owned on either route.
        assert_eq!(
            UpdateUnavailable::from_hello(Some("linux"), &hello(&[]), true),
            None
        );
    }

    /// rjw.9 D5: a Desktop-owned Daemon that takes a staged build (a Windows one this build
    /// or later, restarted by its Desktop) is updated over the Ring like any other, on every
    /// route; without `daemon.stage` (rjw.8 and before) it stays Desktop-owned over the Ring.
    #[test]
    fn from_hello_matrix_with_stage() {
        use xshell_protocol::cap::{
            DAEMON_DESKTOP_OWNED as OWNED, DAEMON_STAGE as STAGE, DAEMON_UPDATE_LATEST as LATEST,
            DAEMON_UPGRADE_SELECTED as SELECTED,
        };
        let hello = |caps: &[&str]| xshell_protocol::msg::Hello {
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            ..serde_json::from_value(
                serde_json::json!({"protocol":{"min":1,"max":1},"version":"1.6.0"}),
            )
            .unwrap()
        };
        let windows_now = hello(&[OWNED, SELECTED, STAGE, LATEST]);
        let windows_rjw8 = hello(&[OWNED, SELECTED]);
        let windows_no_restart = hello(&[OWNED]);
        let linux_always_on = hello(&[SELECTED, STAGE, LATEST]);
        let linux_gui = hello(&[OWNED]);
        type Case<'a> = (
            &'a str,
            Option<&'a str>,
            &'a xshell_protocol::msg::Hello,
            bool,
            Option<UpdateUnavailable>,
        );
        let cases: &[Case] = &[
            (
                "windows now, Ring",
                Some("windows"),
                &windows_now,
                true,
                None,
            ),
            (
                "windows now, Ring, os unknown",
                None,
                &windows_now,
                true,
                None,
            ),
            (
                "windows now, SSH",
                Some("windows"),
                &windows_now,
                false,
                None,
            ),
            (
                "windows rjw.8, Ring",
                Some("windows"),
                &windows_rjw8,
                true,
                Some(UpdateUnavailable::DesktopOwned),
            ),
            (
                "windows rjw.8, SSH",
                Some("windows"),
                &windows_rjw8,
                false,
                None,
            ),
            (
                "windows without restart, SSH",
                Some("windows"),
                &windows_no_restart,
                false,
                Some(UpdateUnavailable::WindowsOwner),
            ),
            (
                "windows without restart, Ring",
                Some("windows"),
                &windows_no_restart,
                true,
                Some(UpdateUnavailable::DesktopOwned),
            ),
            (
                "linux always-on, Ring",
                Some("linux"),
                &linux_always_on,
                true,
                None,
            ),
            (
                "linux always-on, SSH",
                Some("linux"),
                &linux_always_on,
                false,
                None,
            ),
            (
                "linux GUI-bound, Ring",
                Some("linux"),
                &linux_gui,
                true,
                Some(UpdateUnavailable::DesktopOwned),
            ),
            (
                "linux GUI-bound, SSH",
                Some("linux"),
                &linux_gui,
                false,
                Some(UpdateUnavailable::DesktopOwned),
            ),
            (
                "macos GUI-bound, Ring",
                Some("macos"),
                &linux_gui,
                true,
                Some(UpdateUnavailable::DesktopOwned),
            ),
        ];
        for (what, os, hello, relay, want) in cases {
            assert_eq!(
                UpdateUnavailable::from_hello(*os, hello, *relay),
                *want,
                "{what}"
            );
        }
    }

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
                "sinceMs":1,"configGeneration":7,"linkGeneration":0,"updateUnavailable":null,"updateAction":null,"updateVersion":null,"updateSource":null,
                "routeUpdateUnavailable":null
            })
        );
        let mut offered = s.clone();
        offered.update_action = Some(UpdateAction::Update);
        offered.update_version = Some("1.6.0-rjw.5".into());
        offered.update_source = Some(UpdateSource::Release);
        let v = serde_json::to_value(&offered).unwrap();
        assert_eq!(
            (&v["updateAction"], &v["updateVersion"], &v["updateSource"]),
            (
                &serde_json::json!("update"),
                &serde_json::json!("1.6.0-rjw.5"),
                &serde_json::json!("release")
            )
        );
        assert_eq!(
            serde_json::to_value(UpdateSource::Desktop).unwrap(),
            serde_json::json!("desktop")
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

    /// A broker follower reads the publication of an owner from before `updateVersion` and
    /// `updateSource`.
    #[test]
    fn status_without_update_fields_deserializes() {
        let s = HostStatus::initial("h_ab12cd34", "1.5.0", None, 7);
        let mut v = serde_json::to_value(&s).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("updateVersion").unwrap();
        o.remove("updateSource").unwrap();
        let back: HostStatus = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
        assert_eq!((back.update_version, back.update_source), (None, None));
    }
}

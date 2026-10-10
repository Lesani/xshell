//! The Desktop's side of the Ring (no Tauri): its keys and the Roster it signs, kept in one
//! private file ([`store`]), and its Relay connection, through which Settings → Mobile shows
//! the Ring's members ([`desktop::DesktopRing`]); and the worker that keeps every Host's
//! Daemon in the Ring ([`sync`]).

pub mod desktop;
pub mod store;
pub mod sync;

pub use desktop::{
    ConfiguredHosts, DesktopRing, DesktopRingConfig, HostOutcome, HostRef, LocalIdentity,
    MemberView, PairingEvent, PairingFlow, PairingObserver, PhoneOffer, PresenceView, RingObserver,
    RingView, REMOVE_IN_USE, REMOVE_SELF,
};
pub use store::HostMember;
pub use sync::{HostRingState, HostSync, SyncWorker};

/// The Hosted Relay, preset when a Ring is created (defined in `xshell-protocol`, which
/// `xshelld pair` shares).
pub use xshell_protocol::ring::url::HOSTED_RELAY_URL;

/// The Relay a new Ring starts on: `XSHELL_RELAY_URL` when set (development against a
/// local Relay), else the Hosted Relay.
pub fn default_relay_url(env: &dyn Fn(&str) -> Option<String>) -> String {
    env("XSHELL_RELAY_URL")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HOSTED_RELAY_URL.to_string())
}

/// The Host ids the frontend's settings file (`settings.json`, which every window of the
/// app loads and saves) lists under `hosts`. A missing file lists none; `None`: the file
/// cannot be read or is not understood.
pub fn persisted_host_ids(
    settings: &std::path::Path,
) -> Option<std::collections::BTreeSet<String>> {
    let raw = match std::fs::read(settings) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Default::default()),
        Err(_) => return None,
    };
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    match v.get("hosts") {
        None | Some(serde_json::Value::Null) => Some(Default::default()),
        Some(serde_json::Value::Array(list)) => list
            .iter()
            .map(|h| h.get("id").and_then(|id| id.as_str()).map(str::to_string))
            .collect(),
        Some(_) => None,
    }
}

/// Which Hosts count as configured for [`DesktopRing::set_configured_hosts`]: those `known`
/// names (this window's Hosts, in memory) and those the settings file on disk lists (saved by
/// any window). An unreadable settings file counts every Host as configured, so nothing is
/// removed on a guess.
///
/// Several windows (app instances) can run at once, each with its own Hosts in memory and
/// one shared settings file. A Host added in another window between this answer and the
/// removal is benign: its Host worker adds the Daemon back when it connects.
pub fn configured_hosts(
    settings: std::path::PathBuf,
    known: impl Fn(&str) -> bool + Send + Sync + 'static,
) -> ConfiguredHosts {
    std::sync::Arc::new(move |id: &str| {
        known(id) || persisted_host_ids(&settings).is_none_or(|ids| ids.contains(id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_host_ids_reads_the_settings_file() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        assert_eq!(persisted_host_ids(&p), Some(Default::default()), "no file");
        std::fs::write(&p, r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(persisted_host_ids(&p), Some(Default::default()), "no hosts");
        std::fs::write(
            &p,
            r#"{"hosts":[{"id":"h_aaaaaaaa","name":"A"},{"id":"h_bbbbbbbb"}]}"#,
        )
        .unwrap();
        let ids = persisted_host_ids(&p).unwrap();
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            ["h_aaaaaaaa", "h_bbbbbbbb"]
        );
        std::fs::write(&p, b"{torn").unwrap();
        assert_eq!(persisted_host_ids(&p), None);
        std::fs::write(&p, r#"{"hosts":[{"name":"no id"}]}"#).unwrap();
        assert_eq!(persisted_host_ids(&p), None);
    }

    #[test]
    fn configured_hosts_asks_memory_then_disk() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        let f = configured_hosts(p.clone(), |id| id == "h_mem");
        assert!(f("h_mem"));
        assert!(!f("h_disk"));
        std::fs::write(&p, r#"{"hosts":[{"id":"h_disk"}]}"#).unwrap();
        assert!(f("h_disk"), "saved by another window");
        assert!(!f("h_gone"));
        std::fs::write(&p, b"{torn").unwrap();
        assert!(f("h_gone"), "unreadable: everything counts as configured");
    }

    #[test]
    fn relay_url_defaults_to_hosted_unless_overridden() {
        assert_eq!(default_relay_url(&|_| None), HOSTED_RELAY_URL);
        assert_eq!(default_relay_url(&|_| Some(" ".into())), HOSTED_RELAY_URL);
        assert_eq!(
            default_relay_url(&|k| (k == "XSHELL_RELAY_URL").then(|| "ws://127.0.0.1:8787".into())),
            "ws://127.0.0.1:8787"
        );
    }
}

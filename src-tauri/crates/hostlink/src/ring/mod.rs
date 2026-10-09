//! The Desktop's side of the Ring (no Tauri): its keys and the Roster it signs, kept in one
//! private file ([`store`]), and its Relay connection, through which Settings → Mobile shows
//! the Ring's members ([`desktop::DesktopRing`]).

pub mod desktop;
pub mod store;

pub use desktop::{
    DesktopRing, DesktopRingConfig, LocalIdentity, MemberView, PresenceView, RingObserver, RingView,
};

/// The Hosted Relay, preset when a Ring is created. A placeholder until the domain is
/// confirmed before release.
pub const HOSTED_RELAY_URL: &str = "wss://relay.xshell.app";

/// The Relay a new Ring starts on: `XSHELL_RELAY_URL` when set (development against a
/// local Relay), else the Hosted Relay.
pub fn default_relay_url(env: &dyn Fn(&str) -> Option<String>) -> String {
    env("XSHELL_RELAY_URL")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HOSTED_RELAY_URL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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

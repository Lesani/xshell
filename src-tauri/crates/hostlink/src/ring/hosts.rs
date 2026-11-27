//! Verified Daemon identity and the elected Desktop's private routing generation.
use super::store::RingState;
use crate::cancel::CancelToken;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, Weak};
use xshell_protocol::ring::relay::sessions::Sessions;
use xshell_protocol::ring::relay::{Connector, MemberPresence};
use xshell_protocol::ring::{NoiseKey, RingId, Role, RosterChain, SignKey};

/// All 32 signing-key bytes, never a name or a case-folded base64 spelling.
pub fn relay_host_id(key: &SignKey) -> String {
    use std::fmt::Write;
    let mut id = String::from("r_");
    for byte in key.as_bytes() {
        write!(id, "{byte:02x}").expect("write to String");
    }
    id
}

/// Constructed only from effective membership of a locally verified chain. Display
/// snapshots are deliberately not deserializable into this authorization capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRingHost {
    pub(crate) ring_id: RingId,
    pub(crate) head_hash: String,
    pub(crate) sign_key: SignKey,
    pub(crate) noise_key: NoiseKey,
    pub(crate) name: String,
    pub(crate) enrollment: Option<String>,
}
impl VerifiedRingHost {
    pub fn project(chain: &RosterChain) -> Vec<Self> {
        chain
            .head()
            .roster()
            .members
            .iter()
            .filter_map(|m| {
                let m = chain.authorized_member(&m.sign_key)?;
                (m.role == Role::Daemon).then(|| Self {
                    ring_id: chain.ring_id().clone(),
                    head_hash: chain.head().hash(),
                    sign_key: m.sign_key,
                    noise_key: m.noise_key,
                    name: m.name.clone(),
                    enrollment: chain
                        .head()
                        .roster()
                        .membership()
                        .ok()
                        .and_then(|membership| {
                            membership.unit_for(&m.sign_key).map(|unit| unit.id.clone())
                        }),
                })
            })
            .collect()
    }
    pub fn id(&self) -> String {
        relay_host_id(&self.sign_key)
    }
    fn authorized(&self, chain: &RosterChain) -> bool {
        self.ring_id == *chain.ring_id()
            && Self::project(chain).iter().any(|h| {
                h.sign_key == self.sign_key
                    && h.noise_key == self.noise_key
                    && h.enrollment == self.enrollment
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteKind {
    Local,
    Ssh,
    Relay,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Reachability {
    Unknown,
    Online,
    NeverConnected,
    Closed { reason: String, at: Option<u64> },
    Unreachable { at: Option<u64> },
}
impl From<&MemberPresence> for Reachability {
    fn from(p: &MemberPresence) -> Self {
        match p {
            MemberPresence::Online { .. } => Self::Online,
            MemberPresence::NeverConnected => Self::NeverConnected,
            MemberPresence::Closed { reason, at } => Self::Closed {
                reason: reason.clone(),
                at: *at,
            },
            MemberPresence::Unreachable { at } => Self::Unreachable { at: *at },
        }
    }
}
/// Effective availability, independent of the saved SSH schema and Link usability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableHost {
    pub id: String,
    pub aliases: Vec<String>,
    pub name: String,
    pub color: Option<String>,
    pub daemon_key: Option<SignKey>,
    pub routes: Vec<RouteKind>,
    pub active_route: RouteKind,
    pub reachability: Reachability,
}

pub(crate) struct OwnerRouting {
    pub sessions: Sessions,
    pub generation: u64,
    pub stopped: CancelToken,
    chain: Mutex<RosterChain>,
    connector: Mutex<Weak<Connector>>,
    terminal: Mutex<std::collections::HashSet<String>>,
}
impl OwnerRouting {
    pub fn new(sessions: Sessions, generation: u64, chain: RosterChain) -> Arc<Self> {
        sessions.sweep_chain(&chain);
        Arc::new(Self {
            sessions,
            generation,
            stopped: CancelToken::new(),
            chain: Mutex::new(chain),
            connector: Mutex::new(Weak::new()),
            terminal: Mutex::new(Default::default()),
        })
    }
    pub fn attach(&self, connector: &Arc<Connector>) {
        self.sessions.attach(connector);
        *self.connector.lock().unwrap() = Arc::downgrade(connector);
    }
    pub fn adopt(&self, chain: &RosterChain) {
        self.terminal
            .lock()
            .unwrap()
            .extend(chain.terminal_enrollments().iter().cloned());
        let effective = {
            let mut current = self.chain.lock().unwrap();
            if chain.ring_id() == current.ring_id()
                && chain.head().version() >= current.head().version()
                && chain.get(current.head().version()) == Some(current.head())
            {
                *current = chain.clone();
            }
            current.clone()
        };
        self.sessions.sweep_chain(&effective);
    }
    pub fn authorized(&self, host: &VerifiedRingHost) -> bool {
        !self.stopped.is_cancelled()
            && !host
                .enrollment
                .as_ref()
                .is_some_and(|id| self.terminal.lock().unwrap().contains(id))
            && host.authorized(&self.chain.lock().unwrap())
    }
    pub fn revoked_key(&self, key: &SignKey) -> bool {
        let chain = self.chain.lock().unwrap();
        let terminal = self.terminal.lock().unwrap();
        let ended = chain
            .head()
            .roster()
            .membership()
            .ok()
            .and_then(|m| m.unit_for(key).map(|unit| terminal.contains(&unit.id)))
            .unwrap_or(false);
        ended
            || (chain.authorized_member(key).is_none()
                && chain.versions().iter().any(|r| r.member(key).is_some()))
    }
    pub fn chain(&self) -> RosterChain {
        self.chain.lock().unwrap().clone()
    }
    pub fn reachability(&self, host: &VerifiedRingHost) -> Reachability {
        if self.stopped.is_cancelled() {
            return Reachability::Unknown;
        }
        self.connector
            .lock()
            .unwrap()
            .upgrade()
            .and_then(|c| c.members())
            .and_then(|members| {
                members
                    .into_iter()
                    .find(|m| m.member.sign_key == host.sign_key)
            })
            .map(|m| Reachability::from(&m.presence))
            .unwrap_or(Reachability::Unknown)
    }
    pub fn stop(&self) {
        self.sessions.stop();
        self.stopped.cancel();
    }
}
#[derive(Clone)]
pub(crate) struct RingHosts {
    pub state: RingState,
    pub owner: Option<Arc<OwnerRouting>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{is_effective_host_id, is_host_id};
    use xshell_protocol::ring::{DeviceKeys, Member, SignedRoster};
    #[test]
    fn relay_host_id_uses_all_bytes_and_only_exact_lowercase_hex_is_effective() {
        // RFC8032 public key: a fixed, valid signing-key vector, not a truncated hash.
        let bytes = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        let id = relay_host_id(&SignKey::from_bytes(bytes).unwrap());
        assert_eq!(
            id,
            "r_d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert!(is_effective_host_id(&id));
        assert!(!is_host_id(&id), "saved SSH IDs remain strict");
        assert!(is_effective_host_id("h_ab12cd34"));
        for invalid in [
            id.to_uppercase(),
            id[..65].into(),
            format!("{id}0"),
            id.replace('a', "g"),
            "local".into(),
        ] {
            assert!(!is_effective_host_id(&invalid), "{invalid}");
        }
    }
    #[test]
    fn verified_projection_excludes_desktop_mobile_and_removed_daemon_keys() {
        let desktop = DeviceKeys::generate().unwrap();
        let daemon = DeviceKeys::generate().unwrap();
        let mobile = DeviceKeys::generate().unwrap();
        let genesis = SignedRoster::genesis(
            &desktop,
            desktop.noise_key(),
            "same",
            "ws://localhost:8787",
            1,
        )
        .unwrap();
        let next = genesis
            .next(&desktop, 2, |d| {
                d.add(Member::new(
                    "same",
                    Role::Daemon,
                    daemon.sign_key(),
                    daemon.noise_key(),
                    2,
                ));
                d.add(Member::new(
                    "same",
                    Role::Mobile,
                    mobile.sign_key(),
                    mobile.noise_key(),
                    2,
                ));
            })
            .unwrap();
        let mut chain = RosterChain::from_chain(vec![genesis, next]).unwrap();
        let projected = VerifiedRingHost::project(&chain);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].sign_key, daemon.sign_key());
        assert_eq!(
            projected[0].enrollment, None,
            "standalone v1 has no Computer requirement"
        );
        let removed = chain
            .head()
            .next(&desktop, 3, |d| {
                d.remove(&daemon.sign_key());
            })
            .unwrap();
        chain.accept(&[removed]).unwrap();
        assert!(!projected[0].authorized(&chain));
        assert!(VerifiedRingHost::project(&chain).is_empty());
    }
}

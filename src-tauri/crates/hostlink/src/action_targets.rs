//! Current public action tokens. Raw Link generations stay private to transport admission.
use crate::handle::InputTarget;
use crate::ring::hosts::{AvailableHost, RouteKind};
use crate::{HostError, HostSnapshot, HostStatus};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use xshell_protocol::ring::SignKey;

const MAX_JSON_INTEGER: u64 = (1 << 53) - 1;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BrokerTarget {
    epoch: Uuid,
    target: InputTarget,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    identity: Option<SignKey>,
    direct: Option<InputTarget>,
    broker: Option<BrokerTarget>,
}
struct Entry {
    key: Key,
    token: u64,
}
#[derive(Default)]
struct State {
    next: u64,
    origins: HashMap<String, u64>,
    direct: HashMap<String, InputTarget>,
    rows: HashMap<String, AvailableHost>,
    aliases: HashMap<String, String>,
    broker: HashMap<SignKey, BrokerTarget>,
    entries: HashMap<String, Entry>,
}
#[derive(Clone, Default)]
pub(crate) struct ActionTargets(Arc<Mutex<State>>);
fn changed() -> HostError {
    HostError::offline("the action connection changed")
}
impl State {
    fn representative(&self, id: &str) -> String {
        self.aliases.get(id).cloned().unwrap_or_else(|| id.into())
    }
    fn key(&self, id: &str) -> Key {
        let row = self.rows.get(id);
        let identity = row.and_then(|row| row.daemon_key);
        Key {
            identity,
            direct: if row.is_some_and(|r| r.active_route == RouteKind::Relay) {
                None
            } else {
                self.direct.get(id).copied()
            },
            broker: identity.and_then(|key| self.broker.get(&key).copied()),
        }
    }
    fn current(&mut self, id: &str) -> Option<&Entry> {
        if !self.rows.contains_key(id) && !self.origins.contains_key(id) {
            return None;
        }
        let key = self.key(id);
        if self.entries.get(id).is_none_or(|entry| entry.key != key) {
            self.next = self
                .next
                .checked_add(1)
                .filter(|next| *next <= MAX_JSON_INTEGER)?;
            self.entries.insert(
                id.into(),
                Entry {
                    key,
                    token: self.next,
                },
            );
        }
        self.entries.get(id)
    }
}
impl ActionTargets {
    pub fn register(&self, id: &str, config: u64) {
        let mut st = self.0.lock().unwrap();
        st.origins.insert(id.into(), config);
        st.direct.remove(id);
    }
    pub fn reconcile(&self, rows: &[AvailableHost], roots: &[HostSnapshot]) {
        let mut st = self.0.lock().unwrap();
        let root_ids: HashSet<_> = roots.iter().map(|s| s.status.host.clone()).collect();
        for root in roots {
            let status = &root.status;
            if st
                .origins
                .get(&status.host)
                .is_none_or(|generation| *generation <= status.config_generation)
            {
                let target = InputTarget {
                    config_generation: status.config_generation,
                    link_generation: status.link_generation,
                };
                st.origins
                    .insert(status.host.clone(), status.config_generation);
                if st.direct.get(&status.host).is_none_or(|old| {
                    old.config_generation != target.config_generation
                        || old.link_generation <= target.link_generation
                }) {
                    st.direct.insert(status.host.clone(), target);
                }
            }
        }
        st.rows = rows.iter().cloned().map(|r| (r.id.clone(), r)).collect();
        st.aliases = rows
            .iter()
            .flat_map(|r| r.aliases.iter().map(|a| (a.clone(), r.id.clone())))
            .collect();
        st.origins.retain(|id, _| root_ids.contains(id));
        st.direct.retain(|id, _| root_ids.contains(id));
        let active: HashSet<_> = root_ids
            .iter()
            .cloned()
            .chain(rows.iter().map(|r| r.id.clone()))
            .collect();
        st.entries.retain(|id, _| active.contains(id));
    }
    pub fn broker(&self, epoch: Option<Uuid>, rows: &[AvailableHost], snapshots: &[HostSnapshot]) {
        let mut st = self.0.lock().unwrap();
        st.broker = rows
            .iter()
            .filter_map(|row| {
                let key = row.daemon_key?;
                let epoch = epoch?;
                let status = &snapshots.iter().find(|s| s.status.host == row.id)?.status;
                Some((
                    key,
                    BrokerTarget {
                        epoch,
                        target: InputTarget {
                            config_generation: status.config_generation,
                            link_generation: status.link_generation,
                        },
                    },
                ))
            })
            .collect();
    }
    pub fn direct_status(&self, status: &HostStatus) -> Option<HostStatus> {
        let mut st = self.0.lock().unwrap();
        if st.origins.get(&status.host) != Some(&status.config_generation) {
            return None;
        }
        if st.direct.get(&status.host).is_some_and(|old| {
            old.config_generation == status.config_generation
                && old.link_generation > status.link_generation
        }) {
            return None;
        }
        st.direct.insert(
            status.host.clone(),
            InputTarget {
                config_generation: status.config_generation,
                link_generation: status.link_generation,
            },
        );
        let id = st.representative(&status.host);
        let entry = st.current(&id)?;
        let mut status = status.clone();
        status.raw_action_target = Some(InputTarget {
            config_generation: status.config_generation,
            link_generation: status.link_generation,
        });
        status.config_generation = entry.token;
        status.link_generation = entry.token;
        Some(status)
    }
    pub fn broker_status(
        &self,
        status: &HostStatus,
        epoch: Option<Uuid>,
        canonical: SignKey,
    ) -> Option<HostStatus> {
        let mut st = self.0.lock().unwrap();
        let id = st.representative(&status.host);
        let entry = st.current(&id)?;
        let source = epoch.map(|epoch| BrokerTarget {
            epoch,
            target: InputTarget {
                config_generation: status.config_generation,
                link_generation: status.link_generation,
            },
        });
        if entry.key.identity != Some(canonical) || entry.key.broker != source {
            return None;
        }
        let mut status = status.clone();
        status.raw_action_target = None;
        status.config_generation = entry.token;
        status.link_generation = entry.token;
        Some(status)
    }
    pub fn direct(
        &self,
        id: &str,
        public: InputTarget,
        actual: InputTarget,
    ) -> Result<InputTarget, HostError> {
        let mut st = self.0.lock().unwrap();
        let id = st.representative(id);
        let entry = st.current(&id).ok_or_else(changed)?;
        if public.config_generation != entry.token
            || public.link_generation != entry.token
            || entry.key.direct != Some(actual)
        {
            return Err(changed());
        }
        Ok(actual)
    }
    pub fn remote(
        &self,
        id: &str,
        canonical: SignKey,
        public: InputTarget,
        epoch: Uuid,
        actual: InputTarget,
    ) -> Result<InputTarget, HostError> {
        let mut st = self.0.lock().unwrap();
        let id = st.representative(id);
        let entry = st.current(&id).ok_or_else(changed)?;
        if public.config_generation != entry.token
            || public.link_generation != entry.token
            || entry.key.identity != Some(canonical)
            || entry.key.broker
                != Some(BrokerTarget {
                    epoch,
                    target: actual,
                })
        {
            return Err(changed());
        }
        Ok(actual)
    }
}

pub(crate) struct PublicObserver {
    pub targets: ActionTargets,
    pub inner: Arc<dyn crate::Observer>,
}
impl crate::Observer for PublicObserver {
    fn status(&self, status: &HostStatus) {
        if let Some(status) = self.targets.direct_status(status) {
            self.inner.status(&status);
        }
    }
    fn terminals(&self, host: &str, list: &[xshell_protocol::msg::TerminalInfo]) {
        self.inner.terminals(host, list);
    }
    fn renamed(&self, host: &str) {
        self.inner.renamed(host);
    }
    fn available(&self, hosts: &[AvailableHost]) {
        self.inner.available(hosts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::hosts::Reachability;
    fn row(key: SignKey) -> AvailableHost {
        AvailableHost {
            id: "local".into(),
            aliases: vec!["local".into(), crate::ring::hosts::relay_host_id(&key)],
            name: "Local".into(),
            color: None,
            daemon_key: Some(key),
            routes: vec![RouteKind::Local, RouteKind::Relay],
            active_route: RouteKind::Local,
            reachability: Reachability::Online,
        }
    }
    #[test]
    fn public_tokens_bind_distinct_routes_epoch_identity_and_are_stable_bounded_json_integers() {
        let map = ActionTargets::default();
        let k = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let rows = vec![row(k)];
        let mut raw = HostStatus::initial("local", "test", None, 23);
        raw.link_generation = 41;
        let root = HostSnapshot {
            status: raw.clone(),
            terminals: None,
        };
        map.register("local", 23);
        map.reconcile(&rows, std::slice::from_ref(&root));
        let mut owner = raw.clone();
        owner.config_generation = 7;
        owner.link_generation = 3;
        let epoch = Uuid::new_v4();
        map.broker(
            Some(epoch),
            &rows,
            &[HostSnapshot {
                status: owner.clone(),
                terminals: None,
            }],
        );
        let public = map.direct_status(&raw).unwrap();
        let target = InputTarget {
            config_generation: public.config_generation,
            link_generation: public.link_generation,
        };
        let actual = InputTarget {
            config_generation: 23,
            link_generation: 41,
        };
        let remote = InputTarget {
            config_generation: 7,
            link_generation: 3,
        };
        assert!(target.config_generation <= MAX_JSON_INTEGER);
        assert!(map.direct("local", target, actual).is_ok());
        assert!(map
            .remote(&rows[0].aliases[1], k, target, epoch, remote)
            .is_ok());
        assert!(map
            .remote(&rows[0].aliases[1], k, target, Uuid::new_v4(), remote)
            .is_err());
        for _ in 0..100 {
            assert_eq!(
                map.direct_status(&raw).unwrap().config_generation,
                target.config_generation
            );
        }
        let next = Uuid::new_v4();
        map.broker(
            Some(next),
            &rows,
            &[HostSnapshot {
                status: owner,
                terminals: None,
            }],
        );
        assert!(map
            .remote(&rows[0].aliases[1], k, target, next, remote)
            .is_err());
        assert!(map.direct("local", target, actual).is_err());
        let current = map.direct_status(&raw).unwrap();
        assert_ne!(current.config_generation, target.config_generation);
        let json = serde_json::to_value(current).unwrap();
        assert!(json.get("rawActionTarget").is_none());
        assert_eq!(map.0.lock().unwrap().entries.len(), 1);
        let k2 = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        map.reconcile(&[row(k2)], &[root]);
        assert!(map
            .remote(&rows[0].aliases[1], k, target, next, remote)
            .is_err());
    }
    #[test]
    fn configuring_root_can_publish_after_an_earlier_projection_omitted_the_new_handle() {
        let map = ActionTargets::default();
        map.register("local", 1);
        map.reconcile(&[], &[]);
        let raw = HostStatus::initial("local", "test", None, 1);
        map.reconcile(
            &[],
            &[HostSnapshot {
                status: raw.clone(),
                terminals: None,
            }],
        );
        assert!(map.direct_status(&raw).is_some());
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn stale_direct_and_other_identity_broker_publications_cannot_borrow_current_tokens() {
        let map = ActionTargets::default();
        let key = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let foreign = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let mut raw = HostStatus::initial("local", "test", None, 1);
        raw.link_generation = 9;
        raw.set_kind(crate::StatusKind::Connected);
        let row = AvailableHost {
            id: "local".into(),
            aliases: vec!["local".into()],
            name: "Local".into(),
            color: None,
            daemon_key: Some(key),
            routes: vec![RouteKind::Local],
            active_route: RouteKind::Local,
            reachability: crate::ring::hosts::Reachability::Online,
        };
        map.register("local", 1);
        map.reconcile(
            std::slice::from_ref(&row),
            &[HostSnapshot {
                status: raw.clone(),
                terminals: None,
            }],
        );
        let epoch = Uuid::new_v4();
        map.broker(
            Some(epoch),
            &[row],
            &[HostSnapshot {
                status: raw.clone(),
                terminals: None,
            }],
        );
        let current = map.direct_status(&raw).unwrap();
        let mut old = raw.clone();
        old.link_generation = 8;
        assert!(map.direct_status(&old).is_none());
        assert_eq!(
            map.direct_status(&raw).unwrap().config_generation,
            current.config_generation
        );
        assert!(map.broker_status(&raw, Some(Uuid::new_v4()), key).is_none());
        assert!(map.broker_status(&raw, Some(epoch), foreign).is_none());
        assert!(map.broker_status(&old, Some(epoch), key).is_none());
        let sync = crate::ring::HostSync::new();
        sync.observe(&current);
        assert_eq!(sync.seen("local").unwrap().0.link_gen, 9);
        map.broker(None, &[], &[]);
        sync.observe(&map.direct_status(&raw).unwrap());
        assert_eq!(sync.seen("local").unwrap().0.link_gen, 9);
    }
}

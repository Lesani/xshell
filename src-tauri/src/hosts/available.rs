//! One sequence domain for the command snapshot and available-host events. Publishing and
//! reading share the same lock: a delayed command can never overwrite a newer event.
use serde::Serialize;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tauri::State;
use xshell_hostlink::ring::hosts::AvailableHost;
use xshell_hostlink::Manager;

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableSnapshot {
    sequence: u64,
    hosts: Vec<AvailableHost>,
}

#[derive(Default)]
pub struct AvailableState {
    snapshot: Mutex<AvailableSnapshot>,
    manager: OnceLock<Weak<Manager>>,
}

impl AvailableState {
    pub fn bind_manager(&self, manager: &Arc<Manager>) {
        let _ = self.manager.set(Arc::downgrade(manager));
    }

    fn current(&self) -> Option<Vec<AvailableHost>> {
        self.manager
            .get()?
            .upgrade()
            .map(|manager| manager.available_hosts())
    }
    pub fn publish(&self, hosts: &[AvailableHost], emit: impl FnOnce(&AvailableSnapshot)) {
        let mut snapshot = self.snapshot.lock().unwrap();
        snapshot.sequence += 1;
        // Read the current projection inside this barrier, not a possibly delayed callback.
        snapshot.hosts = self.current().unwrap_or_else(|| hosts.to_vec());
        emit(&snapshot);
    }

    fn snapshot(&self) -> AvailableSnapshot {
        let mut snapshot = self.snapshot.lock().unwrap();
        if let Some(hosts) = self.current() {
            if hosts != snapshot.hosts {
                snapshot.sequence += 1;
                snapshot.hosts = hosts;
            }
        }
        snapshot.clone()
    }
}

#[tauri::command]
pub fn hosts_available(state: State<'_, super::Hosts>) -> AvailableSnapshot {
    state.available.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_available_reads_current_manager_projection_not_a_delayed_observer_payload() {
        struct NoTransport;
        impl xshell_hostlink::TransportFactory for NoTransport {
            fn for_host(
                &self,
                _: &xshell_hostlink::HostConfig,
            ) -> Box<dyn xshell_hostlink::Transport> {
                panic!("available discovery must not create a transport");
            }
        }
        struct NoObserver;
        impl xshell_hostlink::Observer for NoObserver {
            fn status(&self, _: &xshell_hostlink::HostStatus) {}
            fn terminals(&self, _: &str, _: &[xshell_protocol::msg::TerminalInfo]) {}
        }
        let manager = Arc::new(Manager::new(xshell_hostlink::ManagerConfig::new(
            "1.5.0",
            Arc::new(NoTransport),
            Arc::new(xshell_hostlink::DirSource(std::path::PathBuf::new())),
            Arc::new(NoObserver),
        )));
        let state = AvailableState::default();
        state.bind_manager(&manager);
        let stale = AvailableHost {
            id: format!("r_{}", "a".repeat(64)),
            aliases: vec![],
            name: "stale callback".into(),
            color: None,
            daemon_key: None,
            routes: vec![xshell_hostlink::ring::hosts::RouteKind::Relay],
            active_route: xshell_hostlink::ring::hosts::RouteKind::Relay,
            reachability: xshell_hostlink::ring::hosts::Reachability::Unknown,
        };
        state.publish(&[stale], |event| assert!(event.hosts.is_empty()));
        assert!(state.snapshot().hosts.is_empty());
        manager.shutdown();
    }

    #[test]
    fn hosts_available_command_and_events_share_a_sequence_barrier() {
        let state = AvailableState::default();
        let old = state.snapshot();
        state.publish(&[], |event| assert_eq!(event.sequence, 1));
        assert_eq!(old.sequence, 0);
        assert_eq!(state.snapshot().sequence, 1);
        state.publish(&[], |event| assert_eq!(event.sequence, 2));
        assert_eq!(
            serde_json::to_value(state.snapshot()).unwrap(),
            serde_json::json!({"sequence":2,"hosts":[]})
        );
    }
}

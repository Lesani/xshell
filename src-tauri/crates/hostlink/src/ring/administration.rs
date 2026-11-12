//! Pre-membership administration only. No wire message can confirm or release a proof.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_OPERATIONS: usize = 32;
const MAX_TOMBSTONES: usize = 64;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AdministrationRequest {
    Start { operation: Uuid, expires_at: u64 },
    Status { operation: Uuid },
    Cancel { operation: Uuid },
    RequestOwnerFocus {},
    InvalidateOrigin {},
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdministrationStatus {
    /// The prefix has no Pair driver or native confirmation dispatcher yet.
    ComputerPending,
    Cancelled,
    Expired,
    Unknown,
    OwnerFocusRequested,
    OriginInvalidated,
}
struct Operation {
    until: Instant,
    expires_at: u64,
}
#[derive(Default)]
pub(super) struct Administration {
    operations: HashMap<(Uuid, Uuid), Operation>,
    tombstones: HashSet<(Uuid, Uuid)>,
    focus_requested: bool,
    invalidated: HashSet<Uuid>,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
impl Administration {
    pub fn request(
        &mut self,
        client: Uuid,
        request: AdministrationRequest,
    ) -> Result<AdministrationStatus, crate::HostError> {
        self.request_at(client, request, now(), Instant::now())
    }
    fn request_at(
        &mut self,
        client: Uuid,
        request: AdministrationRequest,
        unix_now: u64,
        monotonic_now: Instant,
    ) -> Result<AdministrationStatus, crate::HostError> {
        use AdministrationRequest as R;
        use AdministrationStatus as S;
        if self.invalidated.contains(&client) {
            return Ok(S::OriginInvalidated);
        }
        match request {
            R::Start {
                operation,
                expires_at,
            } => {
                let key = (client, operation);
                if self.tombstones.contains(&key) {
                    return Ok(S::Cancelled);
                }
                if let Some(existing) = self.operations.get(&key) {
                    if monotonic_now >= existing.until || unix_now >= existing.expires_at {
                        return Ok(S::Expired);
                    }
                    if existing.expires_at != expires_at {
                        return Err(crate::HostError::invalid("operation deadline changed"));
                    }
                    return Ok(S::ComputerPending);
                }
                let left = expires_at.saturating_sub(unix_now);
                if left > 600 {
                    return Err(crate::HostError::invalid(
                        "administration deadline exceeds ten minutes",
                    ));
                }
                // IDs, including already-expired starts, remain fenced until disconnect.
                // Saturation refuses new admission rather than forgetting terminal IDs.
                if self.operations.len() >= MAX_OPERATIONS {
                    return Err(crate::HostError::busy());
                }
                self.operations.insert(
                    key,
                    Operation {
                        until: monotonic_now + Duration::from_secs(left),
                        expires_at,
                    },
                );
                Ok(if left == 0 {
                    S::Expired
                } else {
                    S::ComputerPending
                })
            }
            R::Status { operation } => {
                let key = (client, operation);
                Ok(if self.tombstones.contains(&key) {
                    S::Cancelled
                } else if let Some(op) = self.operations.get(&key) {
                    if monotonic_now >= op.until || unix_now >= op.expires_at {
                        S::Expired
                    } else {
                        S::ComputerPending
                    }
                } else {
                    S::Unknown
                })
            }
            R::Cancel { operation } => {
                let key = (client, operation);
                if !self.tombstones.contains(&key) && self.tombstones.len() >= MAX_TOMBSTONES {
                    return Err(crate::HostError::busy());
                }
                self.operations.remove(&key);
                self.tombstones.insert(key);
                Ok(S::Cancelled)
            }
            R::RequestOwnerFocus {} => {
                self.focus_requested = true;
                Ok(S::OwnerFocusRequested)
            }
            R::InvalidateOrigin {} => {
                self.disconnect(client);
                self.invalidated.insert(client);
                Ok(S::OriginInvalidated)
            }
        }
    }
    pub fn disconnect(&mut self, client: Uuid) {
        self.invalidated.remove(&client);
        self.operations.retain(|(owner, _), _| *owner != client);
        self.tombstones.retain(|(owner, _)| *owner != client);
    }
    pub fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_requested)
    }
}

/// Blocking short-request facade over the existing protected broker, not a second election
/// or a Host/Call tunnel. Connection loss discards this request origin and its pending work.
#[cfg(unix)]
pub struct AdministrationClient {
    stream: super::broker_ipc::native::Stream,
    assembly: std::sync::Arc<super::broker_ipc::Assembly>,
    epoch: Uuid,
    next: u64,
}
#[cfg(unix)]
impl AdministrationClient {
    pub fn connect(desktop_store: &std::path::Path, until: Instant) -> Result<Self, String> {
        let io_until = until.min(Instant::now() + Duration::from_secs(2));
        use super::broker::{ClientMessage, Event};
        use sha2::{Digest, Sha256};
        let dir = desktop_store.canonicalize().map_err(|e| e.to_string())?;
        xshell_core::local_ipc::private_dir(&dir).map_err(|e| e.to_string())?;
        let profile = format!("{:x}", Sha256::digest(dir.as_os_str().as_encoded_bytes()));
        let mut stream = super::broker_ipc::native::connect(
            &dir.join("host-broker.sock"),
            io_until
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or("administration caller deadline")?,
        )
        .map_err(|e| e.to_string())?;
        let assembly = std::sync::Arc::new(super::broker_ipc::Assembly::default());
        let mut bounded = super::broker_ipc::native::DeadlineStream {
            stream: &mut stream,
            until: io_until,
        };
        super::broker_ipc::write_control(
            &mut bounded,
            &ClientMessage::Hello {
                version: 1,
                profile_id: profile.clone(),
            },
        )
        .map_err(|e| e.to_string())?;
        let epoch = match super::broker_ipc::read_limited(&mut bounded, &assembly, 4096)
            .map_err(|e| e.to_string())?
        {
            Event::Welcome {
                version: 1,
                profile_id,
                owner_epoch,
                ..
            } if profile_id == profile => owner_epoch,
            _ => return Err("administration broker hello refused".into()),
        };
        Ok(Self {
            stream,
            assembly,
            epoch,
            next: 0,
        })
    }
    pub fn request(
        &mut self,
        request: AdministrationRequest,
        until: Instant,
    ) -> Result<AdministrationStatus, String> {
        use super::broker::{ClientMessage, Event};
        self.next += 1;
        let id = self.next;
        let mut bounded = super::broker_ipc::native::DeadlineStream {
            stream: &mut self.stream,
            until: until.min(Instant::now() + Duration::from_secs(2)),
        };
        super::broker_ipc::write_control(
            &mut bounded,
            &ClientMessage::Administration {
                owner_epoch: self.epoch,
                request_id: id,
                request,
            },
        )
        .map_err(|e| e.to_string())?;
        loop {
            match super::broker_ipc::read(&mut bounded, &self.assembly)
                .map_err(|e| e.to_string())?
            {
                Event::Reply { request_id, result } if request_id == id => {
                    return serde_json::from_value(result.map_err(|e| e.message)?)
                        .map_err(|e| e.to_string())
                }
                Event::Snapshot { .. } => {}
                _ => return Err("unexpected administration broker reply".into()),
            }
        }
    }
}
#[cfg(unix)]
impl Drop for AdministrationClient {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initially_expired_start_still_consumes_its_id_until_disconnect() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let began = Instant::now();
        assert_eq!(
            state
                .request_at(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: 999
                    },
                    1000,
                    began
                )
                .unwrap(),
            AdministrationStatus::Expired
        );
        assert_eq!(
            state
                .request_at(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: 1700
                    },
                    1601,
                    began + Duration::from_secs(601)
                )
                .unwrap(),
            AdministrationStatus::Expired
        );
        state.disconnect(client);
        assert_eq!(
            state
                .request_at(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: 1700
                    },
                    1601,
                    began + Duration::from_secs(601)
                )
                .unwrap(),
            AdministrationStatus::ComputerPending
        );
    }
    #[test]
    fn expired_id_beyond_old_pruning_window_cannot_restart_with_new_deadline() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let began = Instant::now();
        state
            .request_at(
                client,
                AdministrationRequest::Start {
                    operation,
                    expires_at: 1001,
                },
                1000,
                began,
            )
            .unwrap();
        let replay = state.request_at(
            client,
            AdministrationRequest::Start {
                operation,
                expires_at: 1100,
            },
            1010,
            began + Duration::from_secs(10),
        );
        assert!(!matches!(replay, Ok(AdministrationStatus::ComputerPending)));
    }
    #[test]
    fn cancelled_id_beyond_old_tombstone_lifetime_cannot_restart() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let began = Instant::now();
        state
            .request_at(
                client,
                AdministrationRequest::Cancel { operation },
                1000,
                began,
            )
            .unwrap();
        assert_eq!(
            state
                .request_at(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: 1700
                    },
                    1601,
                    began + Duration::from_secs(601)
                )
                .unwrap(),
            AdministrationStatus::Cancelled
        );
    }
    #[test]
    fn expired_history_saturation_refuses_admission_instead_of_forgetting() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let began = Instant::now();
        for _ in 0..MAX_OPERATIONS {
            state
                .request_at(
                    client,
                    AdministrationRequest::Start {
                        operation: Uuid::new_v4(),
                        expires_at: 1001,
                    },
                    1000,
                    began,
                )
                .unwrap();
        }
        assert!(state
            .request_at(
                client,
                AdministrationRequest::Start {
                    operation: Uuid::new_v4(),
                    expires_at: 1700
                },
                1601,
                began + Duration::from_secs(601)
            )
            .is_err());
        assert_eq!(state.operations.len(), MAX_OPERATIONS);
        state.disconnect(client);
        assert!(state
            .request_at(
                client,
                AdministrationRequest::Start {
                    operation: Uuid::new_v4(),
                    expires_at: 1700
                },
                1601,
                began + Duration::from_secs(601)
            )
            .is_ok());
    }
    #[test]
    fn cancel_before_start_and_disconnect_cannot_rearm_origin() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let operation = Uuid::new_v4();
        assert_eq!(
            state
                .request(client, AdministrationRequest::Cancel { operation })
                .unwrap(),
            AdministrationStatus::Cancelled
        );
        assert_eq!(
            state
                .request(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: now() + 30
                    }
                )
                .unwrap(),
            AdministrationStatus::Cancelled
        );
        state.disconnect(client);
        assert_eq!(
            state
                .request(client, AdministrationRequest::Status { operation })
                .unwrap(),
            AdministrationStatus::Unknown
        );
    }
    #[test]
    fn invalidation_fences_delayed_start_and_deadlines_cannot_be_renewed() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let expiry = now() + 30;
        assert_eq!(
            state
                .request(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: expiry
                    }
                )
                .unwrap(),
            AdministrationStatus::ComputerPending
        );
        let original = state.operations[&(client, operation)].until;
        state
            .request(
                client,
                AdministrationRequest::Start {
                    operation,
                    expires_at: expiry,
                },
            )
            .unwrap();
        assert_eq!(state.operations[&(client, operation)].until, original);
        assert!(state
            .request(
                client,
                AdministrationRequest::Start {
                    operation,
                    expires_at: expiry + 1
                }
            )
            .is_err());
        state
            .operations
            .get_mut(&(client, operation))
            .unwrap()
            .until = Instant::now();
        assert_eq!(
            state
                .request(client, AdministrationRequest::Status { operation })
                .unwrap(),
            AdministrationStatus::Expired
        );
        state
            .request(client, AdministrationRequest::InvalidateOrigin {})
            .unwrap();
        assert_eq!(
            state
                .request(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: expiry
                    }
                )
                .unwrap(),
            AdministrationStatus::OriginInvalidated
        );
    }
    #[test]
    fn bounded_admission_and_tombstones() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        for _ in 0..MAX_OPERATIONS {
            state
                .request(
                    client,
                    AdministrationRequest::Start {
                        operation: Uuid::new_v4(),
                        expires_at: now() + 30,
                    },
                )
                .unwrap();
        }
        assert!(state
            .request(
                client,
                AdministrationRequest::Start {
                    operation: Uuid::new_v4(),
                    expires_at: now() + 30
                }
            )
            .is_err());
        for _ in 0..MAX_TOMBSTONES {
            state
                .request(
                    client,
                    AdministrationRequest::Cancel {
                        operation: Uuid::new_v4(),
                    },
                )
                .unwrap();
        }
        assert!(state
            .request(
                client,
                AdministrationRequest::Cancel {
                    operation: Uuid::new_v4()
                }
            )
            .is_err());
    }
    #[test]
    fn no_confirm_sign_or_claimed_native_role_on_wire() {
        for action in ["confirm", "decline", "sign", "native-confirm"] {
            assert!(serde_json::from_value::<AdministrationRequest>(
                serde_json::json!({"action":action})
            )
            .is_err());
        }
        assert!(serde_json::from_value::<AdministrationRequest>(
            serde_json::json!({"action":"request-owner-focus", "gui":true})
        )
        .is_err());
    }
}

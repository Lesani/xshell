//! Pre-membership administration only. No wire message can confirm or release a proof.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub mod monotonic;

const MAX_OPERATIONS: usize = 32;
const MAX_TOMBSTONES: usize = 64;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AdministrationRequest {
    Start {
        operation: Uuid,
        expires_at: u64,
        offer: super::adoption::JoinOfferInput,
        monotonic_expires_at_ns: u64,
    },
    Status {
        operation: Uuid,
    },
    Cancel {
        operation: Uuid,
    },
    RequestOwnerFocus {},
    InvalidateOrigin {},
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdministrationStatus {
    ComputerPending,
    SlotReady,
    AwaitingConfirmation,
    Completing,
    Installed,
    Declined,
    Failed,
    ReconciliationRequired,
    Cancelled,
    Expired,
    Unknown,
    OwnerFocusRequested,
    OriginInvalidated,
}
struct Operation {
    until: Instant,
    expires_at: u64,
    monotonic_expires_at_ns: u64,
    offer: super::adoption::JoinOfferInput,
    dispatched: bool,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    controller: std::sync::Weak<super::adoption::JoinOffer>,
    status: AdministrationStatus,
}
impl Drop for Operation {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
pub(super) struct Dispatch {
    pub client: Uuid,
    pub operation: Uuid,
    pub offer: super::adoption::JoinOfferInput,
    pub until: Instant,
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
        let earlier_instant = Instant::now();
        let tick = if matches!(request, AdministrationRequest::Start { .. }) {
            monotonic::now_ns()
                .map_err(|_| crate::HostError::invalid("Unix monotonic clock unavailable"))?
        } else {
            0
        };
        self.request_at_with_clock(client, request, now(), earlier_instant, tick)
    }
    #[cfg(test)]
    fn test_request(
        &mut self,
        client: Uuid,
        request: AdministrationRequest,
    ) -> Result<AdministrationStatus, crate::HostError> {
        self.request_at(client, request, now(), Instant::now())
    }
    #[cfg(test)]
    fn request_at(
        &mut self,
        client: Uuid,
        request: AdministrationRequest,
        unix_now: u64,
        monotonic_now: Instant,
    ) -> Result<AdministrationStatus, crate::HostError> {
        self.request_at_with_clock(client, request, unix_now, monotonic_now, 0)
    }
    fn request_at_with_clock(
        &mut self,
        client: Uuid,
        request: AdministrationRequest,
        unix_now: u64,
        monotonic_now: Instant,
        shared_tick: u64,
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
                offer,
                monotonic_expires_at_ns,
            } => {
                let key = (client, operation);
                if self.tombstones.contains(&key) {
                    return Ok(S::Cancelled);
                }
                if let Some(existing) = self.operations.get_mut(&key) {
                    if monotonic_now >= existing.until || unix_now >= existing.expires_at {
                        return Ok(S::Expired);
                    }
                    if existing.expires_at != expires_at
                        || existing.monotonic_expires_at_ns != monotonic_expires_at_ns
                        || !existing.offer.same_offer(&offer)
                    {
                        return Err(crate::HostError::invalid("operation offer changed"));
                    }
                    return Ok(existing.status.clone());
                }
                let origin_until =
                    monotonic::receive_at(monotonic_expires_at_ns, monotonic_now, shared_tick)
                        .map_err(|_| {
                            crate::HostError::invalid("invalid/expired origin monotonic deadline")
                        })?;
                let left = expires_at.saturating_sub(unix_now);
                if left > 600 || offer.expires_at != expires_at || operation.is_nil() {
                    return Err(crate::HostError::invalid(
                        "invalid administration offer/deadline",
                    ));
                }
                // Validate shape independently of the test clock and terminal handling.
                if xshell_protocol::ring::b64::decode_array::<32>(&offer.operation).is_err()
                    || offer.relay_url.len() > 2048
                    || xshell_protocol::ring::url::RelayUrl::parse(&offer.relay_url).is_err()
                {
                    return Err(crate::HostError::invalid("invalid administration offer"));
                }
                if self.operations.len() >= MAX_OPERATIONS {
                    return Err(crate::HostError::busy());
                }
                let status = if left == 0 {
                    S::Expired
                } else {
                    S::ComputerPending
                };
                self.operations.insert(
                    key,
                    Operation {
                        until: origin_until.min(monotonic_now + Duration::from_secs(left)),
                        expires_at,
                        monotonic_expires_at_ns,
                        offer,
                        dispatched: false,
                        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        controller: std::sync::Weak::new(),
                        status: status.clone(),
                    },
                );
                Ok(status)
            }
            R::Status { operation } => {
                let key = (client, operation);
                Ok(if self.tombstones.contains(&key) {
                    S::Cancelled
                } else if let Some(op) = self.operations.get_mut(&key) {
                    // A receipt is truth even if the original offer has just expired.
                    if matches!(op.status, S::Installed) {
                        op.status.clone()
                    } else if monotonic_now >= op.until || unix_now >= op.expires_at {
                        op.cancel.store(true, std::sync::atomic::Ordering::Release);
                        S::Expired
                    } else {
                        op.status.clone()
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
    pub(super) fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
    pub fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_requested)
    }
    pub(super) fn take_pending(&mut self) -> Option<Dispatch> {
        let (&(client, operation), op) = self
            .operations
            .iter_mut()
            .find(|(_, op)| !op.dispatched && Instant::now() < op.until && now() < op.expires_at)?;
        op.dispatched = true;
        Some(Dispatch {
            client,
            operation,
            offer: op.offer.clone(),
            until: op.until,
            cancel: op.cancel.clone(),
        })
    }
    pub(super) fn attach(
        &mut self,
        dispatch: &Dispatch,
        controller: &std::sync::Arc<super::adoption::JoinOffer>,
    ) {
        if let Some(op) = self
            .operations
            .get_mut(&(dispatch.client, dispatch.operation))
        {
            op.controller = std::sync::Arc::downgrade(controller);
        }
    }
    pub(super) fn update(&mut self, dispatch: &Dispatch, status: AdministrationStatus) {
        if let Some(op) = self
            .operations
            .get_mut(&(dispatch.client, dispatch.operation))
        {
            op.status = status;
        }
    }
    pub(super) fn controller(
        &self,
        operation: Uuid,
    ) -> Option<std::sync::Arc<super::adoption::JoinOffer>> {
        self.operations
            .iter()
            .find(|((_, id), _)| *id == operation)?
            .1
            .controller
            .upgrade()
    }
    pub(super) fn controllers(&self) -> Vec<(Uuid, std::sync::Arc<super::adoption::JoinOffer>)> {
        self.operations
            .iter()
            .filter_map(|((_, id), op)| op.controller.upgrade().map(|c| (*id, c)))
            .collect()
    }
    /// Only the Join currently installing its own first membership can defer retirement.
    pub(super) fn installing_transition(&self, operation: &str) -> bool {
        self.operations.values().any(|op| {
            op.offer.operation == operation
                && Instant::now() < op.until
                && (matches!(op.status, AdministrationStatus::Installed)
                    || op.controller.upgrade().is_some_and(|join| {
                        matches!(
                            join.join_status(),
                            super::adoption::JoinStatus::Completing
                                | super::adoption::JoinStatus::Installed { .. }
                        )
                    }))
        })
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

/// Dispatch is owner-local; neither broker messages nor this worker confirm anything.
#[cfg(unix)]
pub(super) fn dispatch_pending(
    ring: &std::sync::Arc<super::DesktopRing>,
    administration: &std::sync::Arc<std::sync::Mutex<Administration>>,
) {
    use super::adoption::JoinStatus as J;
    use std::sync::atomic::Ordering;
    // Pending startup metadata is denial, not a descriptor or signing identity.
    let Some(profile) = ring
        .store
        .load_profile()
        .ok()
        .and_then(|(p, _)| p)
        .and_then(|p| p.companion)
    else {
        return;
    };
    let Some(dispatch) = administration.lock().unwrap().take_pending() else {
        return;
    };
    let state = administration.clone();
    let ring = ring.clone();
    let failed_origin = (dispatch.client, dispatch.operation);
    let failed_state = state.clone();
    let spawned = std::thread::Builder::new()
        .name("computer-pair-origin".into())
        .spawn(move || {
            let result = (|| {
                profile.validate()?;
                if dispatch.cancel.load(Ordering::Acquire) || Instant::now() >= dispatch.until {
                    return Err("origin ended before dispatch".to_string());
                }
                let client = super::companion_ipc::CompanionClient::new(std::path::PathBuf::from(
                    &profile.endpoint,
                ));
                loop {
                    if dispatch.cancel.load(Ordering::Acquire)
                        || Instant::now() >= dispatch.until
                        || !ring.pairing_ipc_owner()
                    {
                        return Err("origin ended before counterpart readiness".to_string());
                    }
                    match client.request_until(
                        xshell_protocol::ring::companion::CompanionRequest::PairDriverReady,
                        dispatch.until.min(Instant::now() + Duration::from_secs(2)),
                        dispatch.cancel.clone(),
                    ) {
                        Ok(xshell_protocol::ring::companion::CompanionResponse::Descriptor {
                            profile: Some(ready),
                        }) if ready == profile => break,
                        Ok(_) => return Err("counterpart readiness profile changed".into()),
                        Err(_) => std::thread::sleep(
                            Duration::from_millis(50)
                                .min(dispatch.until.saturating_duration_since(Instant::now())),
                        ),
                    }
                }
                let controller = std::sync::Arc::new(ring.begin_registered_origin_join(
                    &client,
                    dispatch.offer.clone(),
                    dispatch.until,
                    dispatch.cancel.clone(),
                )?);
                state.lock().unwrap().attach(&dispatch, &controller);
                ring.adoption_test_point(
                    super::desktop::AdoptionTestPoint::OriginControllerAttached,
                );
                loop {
                    if dispatch.cancel.load(Ordering::Acquire)
                        || Instant::now() >= dispatch.until
                        || !ring.pairing_ipc_owner()
                    {
                        controller.cancel_join();
                    }
                    let (status, ended) = match controller.join_status() {
                        J::Waiting => (
                            if controller.is_ready() {
                                AdministrationStatus::SlotReady
                            } else {
                                AdministrationStatus::ComputerPending
                            },
                            false,
                        ),
                        J::AwaitingConfirmation(_) => {
                            (AdministrationStatus::AwaitingConfirmation, false)
                        }
                        J::Completing => (AdministrationStatus::Completing, false),
                        J::Installed { .. } => (AdministrationStatus::Installed, true),
                        J::Declined => (AdministrationStatus::Declined, true),
                        J::Failed { .. } => (AdministrationStatus::Failed, true),
                        J::ReconciliationRequired { .. } => {
                            (AdministrationStatus::ReconciliationRequired, true)
                        }
                    };
                    state.lock().unwrap().update(&dispatch, status);
                    if ended {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })();
            ring.adoption_test_point(super::desktop::AdoptionTestPoint::OriginDispatchEnded);
            if result.is_err() {
                state
                    .lock()
                    .unwrap()
                    .update(&dispatch, AdministrationStatus::Failed);
            }
        });
    if spawned.is_err() {
        if let Some(operation) = failed_state
            .lock()
            .unwrap()
            .operations
            .get_mut(&failed_origin)
        {
            operation.cancel.store(true, Ordering::Release);
            operation.status = AdministrationStatus::Failed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_monotonic_deadline() -> u64 {
        600_000_000_000
    }

    fn test_offer(expires_at: u64) -> crate::ring::adoption::JoinOfferInput {
        crate::ring::adoption::JoinOfferInput {
            operation: xshell_protocol::ring::b64::encode(&[1; 32]),
            code: xshell_protocol::ring::device_code::DeviceCode::from_bytes([2; 12]),
            relay_url: "ws://127.0.0.1:9".into(),
            expires_at,
        }
    }

    #[test]
    fn duplicate_offer_dispatch_is_one_shot_and_rebound_fields_refuse() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let expires_at = now() + 30;
        let offer = test_offer(expires_at);
        let start = AdministrationRequest::Start {
            operation,
            expires_at,
            offer: offer.clone(),
            monotonic_expires_at_ns: test_monotonic_deadline(),
        };
        assert_eq!(
            state.test_request(client, start.clone()).unwrap(),
            AdministrationStatus::ComputerPending
        );
        if let AdministrationRequest::Start {
            monotonic_expires_at_ns,
            ..
        } = &start
        {
            let mut rebound = start.clone();
            if let AdministrationRequest::Start {
                monotonic_expires_at_ns: changed,
                ..
            } = &mut rebound
            {
                *changed = monotonic_expires_at_ns.saturating_add(1);
            }
            assert!(state.test_request(client, rebound).is_err());
        }
        let original = state.operations[&(client, operation)].until;
        let dispatch = state.take_pending().unwrap();
        for _ in 0..3 {
            assert_eq!(
                state.test_request(client, start.clone()).unwrap(),
                AdministrationStatus::ComputerPending
            );
            assert!(state.take_pending().is_none());
        }
        assert_eq!(state.operations[&(client, operation)].until, original);
        assert_eq!(dispatch.offer.operation, offer.operation);
        assert!(dispatch.offer.code.format() == offer.code.format());
        for field in 0..3 {
            let mut rebound = offer.clone();
            match field {
                0 => {
                    rebound.code =
                        xshell_protocol::ring::device_code::DeviceCode::from_bytes([4; 12])
                }
                1 => rebound.operation = xshell_protocol::ring::b64::encode(&[4; 32]),
                _ => rebound.relay_url = "ws://127.0.0.1:10".into(),
            }
            assert!(state
                .test_request(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at,
                        offer: rebound,
                        monotonic_expires_at_ns: test_monotonic_deadline(),
                    }
                )
                .is_err());
        }
        let words = offer.code.format();
        assert!(!format!("{start:?}").contains(words.as_str()));
        assert!(!serde_json::to_string(
            &state
                .test_request(client, AdministrationRequest::Status { operation })
                .unwrap()
        )
        .unwrap()
        .contains(words.as_str()));
        state
            .test_request(client, AdministrationRequest::Cancel { operation })
            .unwrap();
        assert!(dispatch.cancel.load(std::sync::atomic::Ordering::Acquire));
        state.update(&dispatch, AdministrationStatus::SlotReady);
        assert_eq!(
            state.test_request(client, start).unwrap(),
            AdministrationStatus::Cancelled
        );
        assert!(state.take_pending().is_none());
    }
    #[test]
    fn receipt_retention_is_exact_operation_deadline_read_and_disconnect_bounded() {
        let mut state = Administration::default();
        let client = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let expires_at = now() + 30;
        let offer = test_offer(expires_at);
        state
            .test_request(
                client,
                AdministrationRequest::Start {
                    operation,
                    expires_at,
                    offer: offer.clone(),
                    monotonic_expires_at_ns: test_monotonic_deadline(),
                },
            )
            .unwrap();
        let dispatch = state.take_pending().unwrap();
        // Administration-state unit only: real receipts are separately exercised by actual CLI.
        state.update(&dispatch, AdministrationStatus::Installed);
        assert!(state.installing_transition(&offer.operation));
        assert!(!state.installing_transition(&xshell_protocol::ring::b64::encode(&[5; 32])));
        assert_eq!(
            state
                .test_request(client, AdministrationRequest::Status { operation })
                .unwrap(),
            AdministrationStatus::Installed
        );
        // Queueing a reply is not proof the origin read it. Its existing Cancel /
        // disconnect acknowledges terminal delivery; the absolute deadline still caps it.
        assert!(state.installing_transition(&offer.operation));
        state
            .operations
            .get_mut(&(client, operation))
            .unwrap()
            .until = Instant::now();
        assert!(!state.installing_transition(&offer.operation));
        state.disconnect(client);
        assert!(dispatch.cancel.load(std::sync::atomic::Ordering::Acquire));
        state.update(&dispatch, AdministrationStatus::Installed);
        assert_eq!(
            state
                .test_request(client, AdministrationRequest::Status { operation })
                .unwrap(),
            AdministrationStatus::Unknown
        );
    }
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
                        expires_at: 999,
                        offer: test_offer(999),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
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
                        expires_at: 1700,
                        offer: test_offer(1700),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
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
                        expires_at: 1700,
                        offer: test_offer(1700),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
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
                    offer: test_offer(1001),
                    monotonic_expires_at_ns: test_monotonic_deadline(),
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
                offer: test_offer(1100),
                monotonic_expires_at_ns: test_monotonic_deadline(),
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
                        expires_at: 1700,
                        offer: test_offer(1700),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
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
                        offer: test_offer(1001),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
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
                    expires_at: 1700,
                    offer: test_offer(1700),
                    monotonic_expires_at_ns: test_monotonic_deadline(),
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
                    expires_at: 1700,
                    offer: test_offer(1700),
                    monotonic_expires_at_ns: test_monotonic_deadline(),
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
                .test_request(client, AdministrationRequest::Cancel { operation })
                .unwrap(),
            AdministrationStatus::Cancelled
        );
        assert_eq!(
            state
                .test_request(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: now() + 30,
                        offer: test_offer(now() + 30),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
                    }
                )
                .unwrap(),
            AdministrationStatus::Cancelled
        );
        state.disconnect(client);
        assert_eq!(
            state
                .test_request(client, AdministrationRequest::Status { operation })
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
                .test_request(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: expiry,
                        offer: test_offer(expiry),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
                    }
                )
                .unwrap(),
            AdministrationStatus::ComputerPending
        );
        let original = state.operations[&(client, operation)].until;
        state
            .test_request(
                client,
                AdministrationRequest::Start {
                    operation,
                    expires_at: expiry,
                    offer: test_offer(expiry),
                    monotonic_expires_at_ns: test_monotonic_deadline(),
                },
            )
            .unwrap();
        assert_eq!(state.operations[&(client, operation)].until, original);
        assert!(state
            .test_request(
                client,
                AdministrationRequest::Start {
                    operation,
                    expires_at: expiry + 1,
                    offer: test_offer(expiry + 1),
                    monotonic_expires_at_ns: test_monotonic_deadline(),
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
                .test_request(client, AdministrationRequest::Status { operation })
                .unwrap(),
            AdministrationStatus::Expired
        );
        state
            .test_request(client, AdministrationRequest::InvalidateOrigin {})
            .unwrap();
        assert_eq!(
            state
                .test_request(
                    client,
                    AdministrationRequest::Start {
                        operation,
                        expires_at: expiry,
                        offer: test_offer(expiry),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
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
                .test_request(
                    client,
                    AdministrationRequest::Start {
                        operation: Uuid::new_v4(),
                        expires_at: now() + 30,
                        offer: test_offer(now() + 30),
                        monotonic_expires_at_ns: test_monotonic_deadline(),
                    },
                )
                .unwrap();
        }
        assert!(state
            .test_request(
                client,
                AdministrationRequest::Start {
                    operation: Uuid::new_v4(),
                    expires_at: now() + 30,
                    offer: test_offer(now() + 30),
                    monotonic_expires_at_ns: test_monotonic_deadline(),
                }
            )
            .is_err());
        for _ in 0..MAX_TOMBSTONES {
            state
                .test_request(
                    client,
                    AdministrationRequest::Cancel {
                        operation: Uuid::new_v4(),
                    },
                )
                .unwrap();
        }
        assert!(state
            .test_request(
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

//! One elected owner of Host work; clients own waiters and output subscriptions, not Links.
use super::broker_ipc::{self, Assembly, Outbox};
use super::hosts::AvailableHost;
use crate::handle::{InputTarget, UpdateTarget};
use crate::{HostError, HostErrorCode, HostHandle, HostSnapshot, Manager, TermSink, Waiter};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;
use xshell_core::terminal::replay::ReplayBuffer;
use xshell_protocol::msg::{OpenReply, OpenSpec};
use xshell_protocol::ring::SignKey;

const VERSION: u32 = 1;
const MAX_PENDING: usize = 256;
const MAX_ATTACHMENTS: usize = 64;
const REPLAY: usize = 2 * 1024 * 1024;
type Subscription = (Uuid, Uuid);
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum ClientMessage {
    Hello {
        version: u32,
        profile_id: String,
    },
    Request(Box<BrokerRequest>),
    Administration {
        owner_epoch: Uuid,
        request_id: u64,
        request: super::administration::AdministrationRequest,
    },
    Cancel {
        owner_epoch: Uuid,
        request_id: u64,
    },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct BrokerRequest {
    owner_epoch: Uuid,
    request_id: u64,
    host: String,
    canonical: SignKey,
    expected: InputTarget,
    operation: BrokerOperation,
}
/// This is not a Host-frame tunnel. Membership/lifecycle/consent never pass through Call.
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub(crate) enum BrokerOperation {
    CompanionPending {
        request: xshell_protocol::ring::companion::CompanionAdminRequest,
    },
    Call {
        method: String,
        params: Value,
    },
    Open {
        spec: OpenSpec,
        subscription: Uuid,
    },
    Attach {
        terminal: Uuid,
        subscription: Uuid,
    },
    Detach {
        terminal: Uuid,
        subscription: Uuid,
    },
    Input {
        terminal: Uuid,
        data: String,
        handled_completion_id: Option<Uuid>,
    },
    Resize {
        terminal: Uuid,
        cols: u16,
        rows: u16,
    },
    Close {
        terminal: Uuid,
    },
    Update {
        terminal: Uuid,
        session_id: Option<String>,
        meta: Option<Map<String, Value>>,
    },
    Relaunch {
        terminal: Uuid,
        skip: bool,
    },
    Kick,
}
impl BrokerOperation {
    fn mutation(&self) -> bool {
        !matches!(self, Self::Attach { .. } | Self::Detach { .. } | Self::Kick)
    }
    fn timeout(&self) -> Duration {
        Duration::from_secs(match self {
            Self::Call { method, .. } if method == "file_stat" || method == "upload_commit" => 300,
            Self::Call { method, .. } if method == "save_dropped_file" => 120,
            Self::Call { .. } => 60,
            _ => 30,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum Event {
    Welcome {
        version: u32,
        profile_id: String,
        owner_epoch: Uuid,
        client_id: Uuid,
    },
    Snapshot {
        sequence: u64,
        hosts: Vec<AvailableHost>,
        snapshots: Vec<HostSnapshot>,
    },
    Reply {
        request_id: u64,
        result: Result<Value, HostError>,
    },
    AttachReady {
        subscription: Uuid,
        stream_epoch: u64,
    },
    Data {
        subscription: Uuid,
        stream_epoch: u64,
        bytes: String,
    },
    Reset {
        subscription: Uuid,
        stream_epoch: u64,
    },
    Exit {
        subscription: Uuid,
        stream_epoch: u64,
        code: i32,
        bytes: u64,
    },
    Resync {
        subscription: Uuid,
    },
}
struct ViewerSize {
    size: (u16, u16),
    interaction: u64,
}
struct Subscriber {
    session: Weak<Session>,
    subscription: Uuid,
    delivered: u64,
    size: Option<(u16, u16)>,
    interaction: u64,
}
struct StreamState {
    replay: ReplayBuffer,
    epoch: u64,
    subscribers: HashMap<Subscription, Subscriber>,
    exited: Option<i32>,
    admission: Option<Result<Option<i32>, HostError>>,
    waiters: Vec<(Weak<Session>, u64)>,
}
/// The only sink ever installed for this Host/Terminal, including Open's included Attach.
struct BrokerTerminalSink {
    state: Mutex<StreamState>,
    hub_viewer: Uuid,
    _replay: crate::attachments::ReplayPermit,
}
impl BrokerTerminalSink {
    fn new(budget: &crate::attachments::ReplayBudget) -> Result<Arc<Self>, HostError> {
        Ok(Arc::new(Self {
            hub_viewer: Uuid::new_v4(),
            _replay: budget.reserve()?,
            state: Mutex::new(StreamState {
                replay: ReplayBuffer::new(REPLAY),
                epoch: 1,
                subscribers: HashMap::new(),
                exited: None,
                admission: None,
                waiters: Vec::new(),
            }),
        }))
    }
    fn data_to(st: &mut StreamState, key: Subscription, bytes: &[u8]) -> bool {
        let epoch = st.epoch;
        let Some(viewer) = st.subscribers.get_mut(&key) else {
            return false;
        };
        let Some(session) = viewer.session.upgrade() else {
            return false;
        };
        for chunk in bytes.chunks(broker_ipc::CHUNK) {
            let event = Event::Data {
                subscription: viewer.subscription,
                stream_epoch: epoch,
                bytes: base64::engine::general_purpose::STANDARD.encode(chunk),
            };
            if !session.emit(&event, true) {
                let _ = session.emit(
                    &Event::Resync {
                        subscription: viewer.subscription,
                    },
                    false,
                );
                return false;
            }
            viewer.delivered += chunk.len() as u64;
        }
        true
    }
    /// Replay cut, barrier and live admission share the sink lock.
    fn subscribe(&self, session: &Arc<Session>, subscription: Uuid) -> Result<(), HostError> {
        let mut st = self.state.lock().unwrap();
        let key = (session.id, subscription);
        if st.subscribers.contains_key(&key) {
            return Err(HostError::invalid("duplicate subscription"));
        }
        if st.subscribers.len() >= MAX_ATTACHMENTS {
            return Err(HostError::busy());
        }
        let replay = st.replay.snapshot();
        let epoch = st.epoch;
        if !session.emit(
            &Event::AttachReady {
                subscription,
                stream_epoch: epoch,
            },
            false,
        ) {
            return Err(HostError::offline("broker client disconnected"));
        }
        st.subscribers.insert(
            key,
            Subscriber {
                session: Arc::downgrade(session),
                subscription,
                delivered: 0,
                size: None,
                interaction: 0,
            },
        );
        if !Self::data_to(&mut st, key, &replay) {
            st.subscribers.remove(&key);
            return Err(HostError::busy());
        }
        if let Some(code) = st.exited {
            let bytes = st.subscribers[&key].delivered;
            session.emit(
                &Event::Exit {
                    subscription,
                    stream_epoch: epoch,
                    code,
                    bytes,
                },
                false,
            );
        }
        Ok(())
    }
    fn attached(&self, session: &Arc<Session>, request_id: u64) {
        let ready = {
            let mut st = self.state.lock().unwrap();
            if let Some(result) = &st.admission {
                Some(result.clone())
            } else {
                st.waiters.push((Arc::downgrade(session), request_id));
                None
            }
        };
        if let Some(result) = ready {
            session.finish(
                request_id,
                result.map(|code| serde_json::json!({"exitCode": code})),
            );
        }
    }
    fn complete(&self, result: Result<Option<i32>, HostError>) {
        let waiters = {
            let mut st = self.state.lock().unwrap();
            st.admission = Some(result.clone());
            if result.is_err() {
                st.subscribers.clear();
            }
            std::mem::take(&mut st.waiters)
        };
        for (session, id) in waiters {
            if let Some(session) = session.upgrade() {
                session.finish(
                    id,
                    result
                        .clone()
                        .map(|code| serde_json::json!({"exitCode": code})),
                );
            }
        }
    }
    fn remove(&self, client: Uuid, subscription: Option<Uuid>) -> (bool, Option<ViewerSize>) {
        let mut st = self.state.lock().unwrap();
        st.subscribers
            .retain(|(id, sub), _| *id != client || subscription.is_some_and(|s| s != *sub));
        let size = st
            .subscribers
            .values()
            .filter(|s| s.interaction > 0)
            .max_by_key(|s| s.interaction)
            .and_then(|s| {
                s.size.map(|size| ViewerSize {
                    size,
                    interaction: s.interaction,
                })
            });
        (st.subscribers.is_empty(), size)
    }
    fn size_for(&self, client: Uuid) -> Option<(u16, u16)> {
        self.state
            .lock()
            .unwrap()
            .subscribers
            .iter()
            .filter(|((id, _), _)| *id == client)
            .max_by_key(|(_, s)| s.interaction)
            .and_then(|(_, s)| s.size)
    }
    fn interaction(
        &self,
        client: Uuid,
        size: Option<(u16, u16)>,
        sequence: u64,
    ) -> Option<(u16, u16)> {
        let mut st = self.state.lock().unwrap();
        // Input and Resize apply to this client's most recently attached viewer of the UUID.
        let sub = st
            .subscribers
            .iter()
            .filter(|((id, _), _)| *id == client)
            .max_by_key(|(_, s)| s.interaction)
            .map(|(k, _)| *k)?;
        let viewer = st.subscribers.get_mut(&sub)?;
        viewer.interaction = sequence;
        if let Some(size) = size {
            viewer.size = Some(size);
        }
        viewer.size
    }
}
impl TermSink for BrokerTerminalSink {
    fn data(&self, bytes: &[u8]) -> bool {
        let mut st = self.state.lock().unwrap();
        st.replay.push(bytes);
        let keys: Vec<_> = st.subscribers.keys().copied().collect();
        for key in keys {
            if !Self::data_to(&mut st, key, bytes) {
                st.subscribers.remove(&key);
            }
        }
        !st.subscribers.is_empty()
    }
    fn exit(&self, code: i32, _: u64) {
        let mut st = self.state.lock().unwrap();
        st.exited = Some(code);
        for viewer in st.subscribers.values() {
            if let Some(session) = viewer.session.upgrade() {
                session.emit(
                    &Event::Exit {
                        subscription: viewer.subscription,
                        stream_epoch: st.epoch,
                        code,
                        bytes: viewer.delivered,
                    },
                    false,
                );
            }
        }
    }
    fn replay_snapshot(&self) -> Option<Vec<u8>> {
        Some(self.state.lock().unwrap().replay.snapshot())
    }
    fn generation_reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.replay = ReplayBuffer::new(REPLAY);
        st.epoch += 1;
        st.exited = None;
        for viewer in st.subscribers.values() {
            if let Some(session) = viewer.session.upgrade() {
                session.emit(
                    &Event::Reset {
                        subscription: viewer.subscription,
                        stream_epoch: st.epoch,
                    },
                    false,
                );
            }
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct AttachmentSlot {
    canonical: SignKey,
    config_generation: u64,
    terminal: Uuid,
}
struct AttachmentTarget {
    slot: AttachmentSlot,
    representative: String,
}
#[cfg(test)]
fn test_slot(host: &str, handle: &HostHandle, terminal: Uuid) -> AttachmentTarget {
    static KEY: std::sync::OnceLock<SignKey> = std::sync::OnceLock::new();
    AttachmentTarget {
        slot: AttachmentSlot {
            canonical: *KEY.get_or_init(|| {
                xshell_protocol::ring::DeviceKeys::generate()
                    .unwrap()
                    .sign_key()
            }),
            config_generation: handle.status().config_generation,
            terminal,
        },
        representative: host.into(),
    }
}
struct Terminal {
    representative: String,
    handle: Arc<HostHandle>,
    sink: Arc<BrokerTerminalSink>,
}
struct SnapshotFrame {
    sequence: u64,
    hosts: Vec<AvailableHost>,
    snapshots: Vec<HostSnapshot>,
}
pub(crate) struct Broker {
    pub epoch: Uuid,
    profile: String,
    manager: Weak<Manager>,
    sessions: Mutex<Vec<Weak<Session>>>,
    terminals: Mutex<HashMap<AttachmentSlot, Terminal>>,
    sequence: AtomicU64,
    snapshot: Mutex<Option<Arc<SnapshotFrame>>>,
    stopped: AtomicBool,
    pub assembly: Arc<Assembly>,
    pub(super) administration: Mutex<super::administration::Administration>,
    replay_budget: crate::attachments::ReplayBudget,
}
impl Broker {
    pub fn new(profile: String, manager: &Arc<Manager>) -> Arc<Self> {
        Arc::new(Self {
            epoch: Uuid::new_v4(),
            profile,
            manager: Arc::downgrade(manager),
            sessions: Mutex::new(Vec::new()),
            terminals: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(0),
            snapshot: Mutex::new(None),
            stopped: AtomicBool::new(false),
            assembly: Arc::new(Assembly::default()),
            administration: Mutex::new(super::administration::Administration::default()),
            replay_budget: manager.replay_budget(),
        })
    }
    fn current_snapshot(&self) -> Option<Arc<SnapshotFrame>> {
        let mut previous = self.snapshot.lock().unwrap();
        let manager = self.manager.upgrade()?;
        let hosts = manager.available_hosts();
        let snapshots = hosts
            .iter()
            .filter_map(|h| manager.host(&h.id).map(|h| h.snapshot()))
            .collect::<Vec<_>>();
        if let Some(frame) = previous.as_ref() {
            if frame.hosts == hosts && frame.snapshots == snapshots {
                return Some(frame.clone());
            }
        }
        let frame = Arc::new(SnapshotFrame {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
            hosts,
            snapshots,
        });
        *previous = Some(frame.clone());
        Some(frame)
    }
    fn send_snapshot(&self, session: &Session, frame: &SnapshotFrame) {
        if session.last_snapshot.load(Ordering::Acquire) >= frame.sequence {
            return;
        }
        if session.emit(
            &Event::Snapshot {
                sequence: frame.sequence,
                hosts: frame.hosts.clone(),
                snapshots: frame.snapshots.clone(),
            },
            false,
        ) {
            session
                .last_snapshot
                .store(frame.sequence, Ordering::Release);
        }
    }
    fn snapshot(&self, session: &Session) {
        if let Some(frame) = self.current_snapshot() {
            self.send_snapshot(session, &frame);
        }
    }
    pub fn tick(&self) {
        self.retire_empty();
        let Some(frame) = self.current_snapshot() else {
            return;
        };
        let sessions: Vec<_> = {
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|s| s.strong_count() > 0);
            sessions.iter().filter_map(Weak::upgrade).collect()
        };
        for session in sessions {
            if session.greeted.load(Ordering::Acquire) && !session.stopped.load(Ordering::Acquire) {
                self.send_snapshot(&session, &frame);
            }
        }
    }
    fn session(self: &Arc<Self>, out: Arc<Outbox>) -> Arc<Session> {
        let session = Arc::new(Session {
            id: Uuid::new_v4(),
            broker: Arc::downgrade(self),
            out,
            pending: Mutex::new(HashMap::new()),
            last_snapshot: AtomicU64::new(0),
            greeted: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        self.sessions.lock().unwrap().push(Arc::downgrade(&session));
        session
    }
    fn sink(
        &self,
        target: AttachmentTarget,
        handle: &Arc<HostHandle>,
        session: &Arc<Session>,
        subscription: Uuid,
        opening: bool,
    ) -> Result<(Arc<BrokerTerminalSink>, bool), HostError> {
        self.retire_empty();
        if handle.status().config_generation != target.slot.config_generation {
            return Err(HostError::offline("attachment configuration changed"));
        }
        let mut terminals = self.terminals.lock().unwrap();
        let key = target.slot;
        if let Some(t) = terminals.get(&key) {
            if !Arc::ptr_eq(&t.handle.sh, &handle.sh) {
                return Err(HostError::offline("attachment route changed"));
            }
            if opening {
                return Err(HostError::invalid(
                    "Terminal already has an output attachment; reconcile before Open",
                ));
            }
            let empty = t.sink.state.lock().unwrap().subscribers.is_empty();
            if empty {
                t.sink.generation_reset();
                t.sink.state.lock().unwrap().admission = None;
            }
            t.sink.subscribe(session, subscription)?;
            return Ok((t.sink.clone(), empty));
        }
        if terminals.len() >= MAX_ATTACHMENTS {
            return Err(HostError::busy());
        }
        let sink = BrokerTerminalSink::new(&self.replay_budget)?;
        sink.subscribe(session, subscription)?;
        terminals.insert(
            key,
            Terminal {
                representative: target.representative,
                handle: handle.clone(),
                sink: sink.clone(),
            },
        );
        Ok((sink, true))
    }
    fn retire_empty(&self) {
        let mut retired = Vec::new();
        {
            let mut terminals = self.terminals.lock().unwrap();
            terminals.retain(|slot, terminal| {
                if !terminal.sink.state.lock().unwrap().subscribers.is_empty() {
                    return true;
                }
                retired.push((
                    terminal.handle.clone(),
                    slot.terminal,
                    terminal.sink.hub_viewer,
                ));
                false
            });
        }
        // The detached hub key is unique to the retired slot, not its representative or
        // UUID; a concurrent replacement and independent Direct viewers remain intact.
        for (handle, terminal, key) in retired {
            let _ = handle.access_term_detach(key, terminal);
        }
    }
    fn detach_subscription(&self, client: Uuid, terminal: Uuid, subscription: Uuid) {
        let hosts: Vec<_> = self
            .terminals
            .lock()
            .unwrap()
            .iter()
            .filter(|(slot, t)| {
                slot.terminal == terminal
                    && t.sink
                        .state
                        .lock()
                        .unwrap()
                        .subscribers
                        .contains_key(&(client, subscription))
            })
            .map(|(_, terminal)| terminal.representative.clone())
            .collect();
        for host in hosts {
            self.detach(client, Some((&host, terminal, subscription)));
        }
    }
    fn detach(&self, client: Uuid, terminal: Option<(&str, Uuid, Uuid)>) {
        let mut retired = Vec::new();
        let mut resized = Vec::new();
        {
            let mut terminals = self.terminals.lock().unwrap();
            terminals.retain(|slot, t| {
                if terminal
                    .is_some_and(|(h, uuid, _)| h != t.representative || uuid != slot.terminal)
                {
                    return true;
                }
                let (empty, size) = t.sink.remove(client, terminal.map(|(_, _, sub)| sub));
                if empty {
                    retired.push((t.handle.clone(), slot.terminal, t.sink.hub_viewer));
                } else if let Some(size) = size {
                    resized.push((t.handle.clone(), slot.terminal, size, t.sink.hub_viewer));
                }
                !empty
            });
        }
        for (handle, id, key) in retired {
            let _ = handle.access_term_detach(key, id);
        }
        for (handle, id, size, key) in resized {
            handle.restore_viewer_size(key, id, size.size, size.interaction);
        }
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for session in sessions {
            session.disconnect();
        }
    }
}
struct Session {
    id: Uuid,
    broker: Weak<Broker>,
    out: Arc<Outbox>,
    pending: Mutex<HashMap<u64, bool>>,
    last_snapshot: AtomicU64,
    greeted: AtomicBool,
    stopped: AtomicBool,
}
impl Session {
    fn emit(&self, event: &Event, output: bool) -> bool {
        if self.out.send(event, output).is_err() {
            if !output {
                self.stopped.store(true, Ordering::Release);
                self.out.stop();
            }
            return false;
        }
        true
    }
    fn finish(&self, request_id: u64, result: Result<Value, HostError>) {
        if self.pending.lock().unwrap().remove(&request_id).is_some() {
            self.emit(&Event::Reply { request_id, result }, false);
        }
    }
    fn disconnect(&self) {
        self.stopped.store(true, Ordering::Release);
        self.out.stop();
        self.pending.lock().unwrap().clear();
        if let Some(broker) = self.broker.upgrade() {
            broker.administration.lock().unwrap().disconnect(self.id);
            broker.detach(self.id, None);
        }
    }
    fn receive(self: &Arc<Self>, message: ClientMessage) -> Result<(), HostError> {
        let broker = self
            .broker
            .upgrade()
            .ok_or_else(|| HostError::offline("broker retired"))?;
        if broker.stopped.load(Ordering::Acquire) {
            return Err(HostError::offline("broker retired"));
        }
        match message {
            ClientMessage::Hello {
                version,
                profile_id,
            } => {
                if version != VERSION
                    || profile_id != broker.profile
                    || self.greeted.load(Ordering::Acquire)
                {
                    return Err(HostError::invalid("broker profile/version/hello mismatch"));
                }
                self.emit(
                    &Event::Welcome {
                        version: VERSION,
                        profile_id,
                        owner_epoch: broker.epoch,
                        client_id: self.id,
                    },
                    false,
                );
                broker.snapshot(self);
                self.greeted.store(true, Ordering::Release);
                Ok(())
            }
            ClientMessage::Cancel {
                owner_epoch,
                request_id,
            } => {
                if !self.greeted.load(Ordering::Acquire) || owner_epoch != broker.epoch {
                    return Err(HostError::invalid("broker epoch mismatch"));
                }
                let mutation = self
                    .pending
                    .lock()
                    .unwrap()
                    .get(&request_id)
                    .copied()
                    .unwrap_or(false);
                let error = HostError::timeout("broker request cancelled");
                self.finish(
                    request_id,
                    Err(if mutation {
                        error.mutation_outcome()
                    } else {
                        error
                    }),
                );
                Ok(())
            }
            ClientMessage::Administration {
                owner_epoch,
                request_id,
                request,
            } => {
                if !self.greeted.load(Ordering::Acquire)
                    || owner_epoch != broker.epoch
                    || self.stopped.load(Ordering::Acquire)
                {
                    return Err(HostError::invalid("administration epoch/origin mismatch"));
                }
                // A short synchronous admission/status transaction, never a human-wait worker.
                let result = {
                    let mut administration = broker.administration.lock().unwrap();
                    // Serialize admission with disconnect; an already queued start cannot
                    // recreate an origin after its disconnect cleanup has run.
                    if self.stopped.load(Ordering::Acquire) {
                        return Err(HostError::offline("administration origin disconnected"));
                    }
                    administration.request(self.id, request).and_then(|status| {
                        serde_json::to_value(status)
                            .map_err(|_| HostError::invalid("administration status encoding"))
                    })
                };
                self.emit(&Event::Reply { request_id, result }, false);
                Ok(())
            }
            ClientMessage::Request(request) => {
                if !self.greeted.load(Ordering::Acquire) || request.owner_epoch != broker.epoch {
                    return Err(HostError::invalid("broker epoch mismatch"));
                }
                let id = request.request_id;
                {
                    let mut pending = self.pending.lock().unwrap();
                    if pending.contains_key(&id) {
                        return Err(HostError::invalid("duplicate pending broker request ID"));
                    }
                    if pending.len() >= MAX_PENDING {
                        self.emit(
                            &Event::Reply {
                                request_id: id,
                                result: Err(HostError::busy()),
                            },
                            false,
                        );
                        return Ok(());
                    }
                    pending.insert(id, request.operation.mutation());
                }
                if let Err(error) = self.dispatch(&broker, *request) {
                    self.finish(id, Err(error));
                }
                Ok(())
            }
        }
    }
    fn dispatch(
        self: &Arc<Self>,
        broker: &Broker,
        request: BrokerRequest,
    ) -> Result<(), HostError> {
        if let BrokerOperation::Detach {
            terminal,
            subscription,
        } = &request.operation
        {
            // Cleanup is scoped to this authenticated connection's subscription, even
            // after alias withdrawal or a representative/route-generation change.
            broker.detach_subscription(self.id, *terminal, *subscription);
            self.finish(request.request_id, Ok(Value::Null));
            return Ok(());
        }
        let manager = broker
            .manager
            .upgrade()
            .ok_or_else(|| HostError::offline("owner Manager gone"))?;
        let row = manager
            .available_hosts()
            .into_iter()
            .find(|h| h.aliases.contains(&request.host) && h.daemon_key == Some(request.canonical))
            .ok_or_else(|| HostError::offline("Host identity no longer available"))?;
        let raw = manager
            .host(&request.host)
            .ok_or_else(|| HostError::unknown_host(&request.host))?;
        let raw = if raw
            .relay_route()
            .is_some_and(|route| route.host.sign_key == request.canonical)
        {
            raw
        } else {
            HostHandle::for_identity(raw, request.canonical)
        };
        let h = HostHandle::for_generation(raw.clone(), request.expected);
        let id = request.request_id;
        let session = self.clone();
        let done: Waiter = Box::new(move |result| session.finish(id, result));
        match request.operation {
            BrokerOperation::CompanionPending { request } => {
                h.companion_pending(request, done);
            }
            BrokerOperation::Call { method, params } => {
                if method.starts_with("daemon.") || method.starts_with("ring.") {
                    return Err(HostError::invalid(
                        "broker Call cannot manage membership or Daemon lifecycle",
                    ));
                }
                h.call(method, params, done);
            }
            BrokerOperation::Open { spec, subscription } => {
                let terminal = spec.terminal;
                let (sink, _) = broker.sink(
                    AttachmentTarget {
                        slot: AttachmentSlot {
                            canonical: request.canonical,
                            config_generation: request.expected.config_generation,
                            terminal,
                        },
                        representative: row.id.clone(),
                    },
                    &raw,
                    self,
                    subscription,
                    true,
                )?;
                let session = self.clone();
                let attached = sink.clone();
                let host = row.id;
                sink.interaction(
                    self.id,
                    Some((spec.cols, spec.rows)),
                    crate::attachments::interaction(),
                );
                h.access_term_open(
                    sink.hub_viewer,
                    spec,
                    sink,
                    Box::new(move |r| {
                        attached.complete(r.as_ref().map(|_| None).map_err(Clone::clone));
                        if r.is_err()
                            || r.as_ref().is_ok_and(|r| {
                                r.existed && r.terminal.is_some_and(|id| id != terminal)
                            })
                        {
                            if let Some(broker) = session.broker.upgrade() {
                                broker.detach(session.id, Some((&host, terminal, subscription)));
                            }
                        }
                        session.finish(
                            id,
                            r.and_then(|r| {
                                serde_json::to_value(r)
                                    .map_err(|e| HostError::invalid(e.to_string()))
                            }),
                        );
                    }),
                );
            }
            BrokerOperation::Attach {
                terminal,
                subscription,
            } => {
                let (sink, fresh) = broker.sink(
                    AttachmentTarget {
                        slot: AttachmentSlot {
                            canonical: request.canonical,
                            config_generation: request.expected.config_generation,
                            terminal,
                        },
                        representative: row.id.clone(),
                    },
                    &raw,
                    self,
                    subscription,
                    false,
                )?;
                if fresh {
                    let session = self.clone();
                    let attached = sink.clone();
                    let host = row.id;
                    h.access_term_attach(
                        sink.hub_viewer,
                        terminal,
                        sink,
                        Box::new(move |r| {
                            attached.complete(r.clone());
                            if r.is_err() {
                                if let Some(broker) = session.broker.upgrade() {
                                    broker
                                        .detach(session.id, Some((&host, terminal, subscription)));
                                }
                            }
                            session.finish(id, r.map(|code| serde_json::json!({"exitCode": code})));
                        }),
                    );
                } else {
                    sink.attached(self, id);
                }
            }
            BrokerOperation::Detach {
                terminal,
                subscription,
            } => {
                broker.detach_subscription(self.id, terminal, subscription);
                done(Ok(Value::Null));
            }
            BrokerOperation::Input {
                terminal,
                data,
                handled_completion_id,
            } => {
                let sink = broker
                    .terminals
                    .lock()
                    .unwrap()
                    .get(&AttachmentSlot {
                        canonical: request.canonical,
                        config_generation: request.expected.config_generation,
                        terminal,
                    })
                    .map(|t| t.sink.clone());
                if let Some(sink) = sink {
                    let key = sink.hub_viewer;
                    if let Some((cols, rows)) = sink.size_for(self.id) {
                        let sequence = h.access_term_resize(key, terminal, cols, rows)?;
                        sink.interaction(self.id, None, sequence);
                    }
                    let result = h.access_term_input(
                        key,
                        terminal,
                        data,
                        handled_completion_id,
                        Some(request.expected),
                    );
                    if let Ok(sequence) = result {
                        sink.interaction(self.id, None, sequence);
                    }
                    done(result.map(|_| Value::Null));
                } else {
                    done(
                        h.term_input(
                            terminal,
                            data,
                            handled_completion_id,
                            Some(request.expected),
                        )
                        .map(|_| Value::Null),
                    );
                }
            }
            BrokerOperation::Resize {
                terminal,
                cols,
                rows,
            } => {
                let sink = broker
                    .terminals
                    .lock()
                    .unwrap()
                    .get(&AttachmentSlot {
                        canonical: request.canonical,
                        config_generation: request.expected.config_generation,
                        terminal,
                    })
                    .map(|t| t.sink.clone());
                if let Some(sink) = sink {
                    let result = h.access_term_resize(sink.hub_viewer, terminal, cols, rows);
                    if let Ok(sequence) = result {
                        sink.interaction(self.id, Some((cols, rows)), sequence);
                    }
                    done(result.map(|_| Value::Null));
                } else {
                    done(h.term_resize(terminal, cols, rows).map(|_| Value::Null));
                }
            }
            BrokerOperation::Close { terminal } => h.term_close(terminal, done),
            BrokerOperation::Update {
                terminal,
                session_id,
                meta,
            } => h.term_update(terminal, session_id, meta, done),
            BrokerOperation::Relaunch { terminal, skip } => {
                let session = self.clone();
                h.term_relaunch(
                    terminal,
                    skip,
                    Box::new(move |r| {
                        session.finish(id, r.map(|pid| serde_json::json!({"pid": pid})))
                    }),
                );
            }
            BrokerOperation::Kick => {
                raw.kick();
                done(Ok(Value::Null));
            }
        }
        Ok(())
    }
}

enum Delivery {
    Data(Vec<u8>),
    Reset,
    Exit(i32, u64),
    Resync,
}
struct DeliveryState {
    queue: VecDeque<Delivery>,
    stopped: bool,
}
struct DeliveryQueue {
    state: Mutex<DeliveryState>,
    cv: Condvar,
    bytes: Arc<AtomicUsize>,
}
impl DeliveryQueue {
    fn clear(&self, state: &mut DeliveryState) {
        for delivery in state.queue.drain(..) {
            if let Delivery::Data(bytes) = delivery {
                self.bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            }
        }
    }
}
/// A slow native channel cannot stall the duplex reader or other clients' callbacks.
struct ProxySink {
    queue: Arc<DeliveryQueue>,
}
impl ProxySink {
    fn new(sink: Arc<dyn TermSink>, bytes: Arc<AtomicUsize>) -> Arc<Self> {
        let queue = Arc::new(DeliveryQueue {
            state: Mutex::new(DeliveryState {
                queue: VecDeque::new(),
                stopped: false,
            }),
            cv: Condvar::new(),
            bytes,
        });
        let worker = queue.clone();
        std::thread::spawn(move || loop {
            let delivery = {
                let mut state = worker.state.lock().unwrap();
                while state.queue.is_empty() && !state.stopped {
                    state = worker.cv.wait(state).unwrap();
                }
                let Some(delivery) = state.queue.pop_front() else {
                    return;
                };
                delivery
            };
            match delivery {
                Delivery::Data(bytes) => {
                    let live = sink.data(&bytes);
                    worker.bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
                    if !live {
                        let mut state = worker.state.lock().unwrap();
                        state.stopped = true;
                        worker.clear(&mut state);
                        return;
                    }
                }
                Delivery::Reset => sink.generation_reset(),
                Delivery::Exit(code, bytes) => sink.exit(code, bytes),
                Delivery::Resync => {
                    sink.resync_required();
                    return;
                }
            }
        });
        Arc::new(Self { queue })
    }
    fn control(&self, delivery: Delivery) {
        let mut state = self.queue.state.lock().unwrap();
        if state.stopped {
            return;
        }
        if state.queue.len() >= 4096 {
            self.queue.clear(&mut state);
            state.queue.push_back(Delivery::Resync);
            state.stopped = true;
        } else {
            state.queue.push_back(delivery);
        }
        self.queue.cv.notify_one();
    }
}
impl TermSink for ProxySink {
    fn data(&self, bytes: &[u8]) -> bool {
        let mut state = self.queue.state.lock().unwrap();
        if state.stopped {
            return false;
        }
        if state.queue.len() >= 4096
            || self
                .queue
                .bytes
                .try_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    (queued + bytes.len() <= 8 * 1024 * 1024).then_some(queued + bytes.len())
                })
                .is_err()
        {
            self.queue.clear(&mut state);
            state.queue.push_back(Delivery::Resync);
            state.stopped = true;
            self.queue.cv.notify_one();
            return false;
        }
        state.queue.push_back(Delivery::Data(bytes.to_vec()));
        self.queue.cv.notify_one();
        true
    }
    fn exit(&self, code: i32, bytes: u64) {
        self.control(Delivery::Exit(code, bytes));
    }
    fn generation_reset(&self) {
        self.control(Delivery::Reset);
    }
    fn resync_required(&self) {
        let mut state = self.queue.state.lock().unwrap();
        if state.stopped {
            return;
        }
        self.queue.clear(&mut state);
        state.queue.push_back(Delivery::Resync);
        state.stopped = true;
        self.queue.cv.notify_one();
    }
}
impl Drop for ProxySink {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap();
        state.stopped = true;
        self.queue.cv.notify_one();
    }
}

pub(crate) fn queued_sink(sink: Arc<dyn TermSink>, bytes: Arc<AtomicUsize>) -> Arc<dyn TermSink> {
    ProxySink::new(sink, bytes)
}

struct Pending {
    waiter: Waiter,
    mutation: bool,
    until: Instant,
}
struct Desired {
    host: String,
    canonical: SignKey,
    terminal: Uuid,
    sink: Arc<dyn TermSink>,
    epoch: u64,
    bytes: u64,
    needs_reset: bool,
    attaching: bool,
    owner_epoch: Option<Uuid>,
}
struct ClientState {
    epoch: Option<Uuid>,
    client: Option<Uuid>,
    hosts: Vec<AvailableHost>,
    snapshots: Vec<HostSnapshot>,
    sequence: u64,
    pending: HashMap<u64, Pending>,
    desired: HashMap<Uuid, Desired>,
}
pub(crate) struct BrokerPublication {
    pub epoch: Option<Uuid>,
    pub hosts: Vec<AvailableHost>,
    pub snapshots: Vec<HostSnapshot>,
}
pub(crate) struct BrokerClient {
    profile: String,
    state: Mutex<ClientState>,
    out: Mutex<Option<Arc<Outbox>>>,
    next: AtomicU64,
    generation: AtomicU64,
    manager: Weak<Manager>,
    stopped: AtomicBool,
    output_bytes: Arc<AtomicUsize>,
}
impl BrokerClient {
    pub fn new(profile: String, manager: &Arc<Manager>) -> Arc<Self> {
        Arc::new(Self {
            profile,
            state: Mutex::new(ClientState {
                epoch: None,
                client: None,
                hosts: Vec::new(),
                snapshots: Vec::new(),
                sequence: 0,
                pending: HashMap::new(),
                desired: HashMap::new(),
            }),
            out: Mutex::new(None),
            next: AtomicU64::new(0),
            generation: AtomicU64::new(1),
            manager: Arc::downgrade(manager),
            stopped: AtomicBool::new(false),
            output_bytes: Arc::new(AtomicUsize::new(0)),
        })
    }
    pub fn has_transport(&self) -> bool {
        self.out.lock().unwrap().is_some()
    }
    pub fn hosts(&self) -> Vec<AvailableHost> {
        self.state.lock().unwrap().hosts.clone()
    }
    pub fn publication(&self) -> BrokerPublication {
        let st = self.state.lock().unwrap();
        BrokerPublication {
            epoch: st.epoch,
            hosts: st.hosts.clone(),
            snapshots: st.snapshots.clone(),
        }
    }
    pub fn snapshots(&self) -> Vec<HostSnapshot> {
        let publication = self.publication();
        let Some(manager) = self.manager.upgrade() else {
            return Vec::new();
        };
        publication
            .snapshots
            .into_iter()
            .filter_map(|mut snapshot| {
                let key = publication
                    .hosts
                    .iter()
                    .find(|h| h.id == snapshot.status.host)?
                    .daemon_key?;
                snapshot.status = manager.action_targets().broker_status(
                    &snapshot.status,
                    publication.epoch,
                    key,
                )?;
                Some(snapshot)
            })
            .collect()
    }
    pub fn proxy(self: &Arc<Self>, id: &str) -> Option<BrokerHostProxy> {
        let st = self.state.lock().unwrap();
        st.epoch?;
        let row = st
            .hosts
            .iter()
            .find(|row| row.daemon_key.is_some() && row.aliases.iter().any(|a| a == id))?;
        Some(BrokerHostProxy {
            client: self.clone(),
            id: id.into(),
            canonical: row.daemon_key?,
            connection_generation: self.generation.load(Ordering::Acquire),
        })
    }
    fn emit(&self, message: &ClientMessage) -> Result<(), HostError> {
        self.out
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| HostError::offline("Host broker reconnecting"))?
            .send_key(
                message,
                false,
                match message {
                    ClientMessage::Request(r) => Some(r.request_id),
                    _ => None,
                },
            )
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    HostError::busy()
                } else {
                    HostError::offline(e.to_string())
                }
            })
    }
    fn request(
        &self,
        proxy: &BrokerHostProxy,
        operation: BrokerOperation,
        target: Option<InputTarget>,
        waiter: Waiter,
    ) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let mut waiter = Some(waiter);
        let request = (|| {
            let mut st = self.state.lock().unwrap();
            let epoch = st
                .epoch
                .ok_or_else(|| HostError::offline("Host broker reconnecting"))?;
            if proxy.connection_generation != self.generation.load(Ordering::Acquire) {
                return Err(HostError::offline(
                    "Host broker owner changed; obtain current access",
                ));
            }
            let expected = if matches!(operation, BrokerOperation::Detach { .. }) {
                InputTarget {
                    config_generation: 0,
                    link_generation: 0,
                }
            } else {
                let row = st
                    .hosts
                    .iter()
                    .find(|row| {
                        row.daemon_key == Some(proxy.canonical) && row.aliases.contains(&proxy.id)
                    })
                    .ok_or_else(|| HostError::offline("Host identity no longer available"))?;
                let snapshot = st
                    .snapshots
                    .iter()
                    .find(|s| s.status.host == row.id)
                    .ok_or_else(|| HostError::offline("Host snapshot not yet available"))?;
                if !snapshot.status.usable() && !matches!(operation, BrokerOperation::Kick) {
                    return Err(HostError::offline("Host broker connection is not usable"));
                }
                InputTarget {
                    config_generation: snapshot.status.config_generation,
                    link_generation: snapshot.status.link_generation,
                }
            };
            if let Some(target) = target {
                self.manager
                    .upgrade()
                    .ok_or_else(|| HostError::offline("Manager retired"))?
                    .action_targets()
                    .remote(&proxy.id, proxy.canonical, target, epoch, expected)?;
            }
            if st.pending.len() >= MAX_PENDING {
                return Err(HostError::busy());
            }
            st.pending.insert(
                id,
                Pending {
                    waiter: waiter.take().unwrap(),
                    mutation: operation.mutation(),
                    until: Instant::now() + operation.timeout(),
                },
            );
            Ok(BrokerRequest {
                owner_epoch: epoch,
                request_id: id,
                host: proxy.id.clone(),
                canonical: proxy.canonical,
                expected,
                operation,
            })
        })();
        match request {
            Err(error) => waiter.unwrap()(Err(error)),
            Ok(request) => {
                if let Err(error) = self.emit(&ClientMessage::Request(Box::new(request))) {
                    let pending = self.state.lock().unwrap().pending.remove(&id);
                    if let Some(pending) = pending {
                        (pending.waiter)(Err(error));
                    }
                }
            }
        }
        id
    }
    pub fn cancel(&self, id: u64) {
        let (epoch, pending) = {
            let mut st = self.state.lock().unwrap();
            (st.epoch, st.pending.remove(&id))
        };
        let unsent = self
            .out
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|out| out.cancel(id));
        if let Some(pending) = pending {
            (pending.waiter)(Err(if pending.mutation && !unsent {
                HostError::timeout("broker request cancelled").mutation_outcome()
            } else {
                HostError::timeout("broker request cancelled")
            }));
        }
        if !unsent {
            if let Some(owner_epoch) = epoch {
                let _ = self.emit(&ClientMessage::Cancel {
                    owner_epoch,
                    request_id: id,
                });
            }
        }
    }
    pub fn tick(self: &Arc<Self>) {
        let expired: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .pending
            .iter()
            .filter(|(_, p)| p.until <= Instant::now())
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.cancel(id);
        }
        self.restore_subscriptions();
    }
    fn restore_subscriptions(self: &Arc<Self>) {
        let generation = self.generation.load(Ordering::Acquire);
        let restore = {
            let mut st = self.state.lock().unwrap();
            if st.epoch.is_none() {
                return;
            }
            let restore = st
                .desired
                .iter()
                .filter(|(_, d)| {
                    !d.attaching
                        && d.epoch == 0
                        && st
                            .hosts
                            .iter()
                            .find(|h| h.daemon_key == Some(d.canonical))
                            .is_some_and(|h| {
                                st.snapshots.iter().any(|s| {
                                    s.status.host == h.id
                                        && s.status.usable()
                                        && s.terminals.as_ref().is_some_and(|list| {
                                            list.iter().any(|t| t.terminal == d.terminal)
                                        })
                                })
                            })
                })
                .map(|(sub, d)| {
                    (
                        *sub,
                        super::hosts::relay_host_id(&d.canonical),
                        d.terminal,
                        d.canonical,
                    )
                })
                .collect::<Vec<_>>();
            let epoch = st.epoch;
            for (subscription, _, _, _) in &restore {
                let desired = st
                    .desired
                    .get_mut(subscription)
                    .expect("selected under client lock");
                desired.attaching = true;
                desired.owner_epoch = epoch;
            }
            restore
        };
        // Current re-list is the barrier. Only same-UUID Attach can be retried; Open and
        // every mutation are deliberately absent from the retained intent.
        for (subscription, host, terminal, canonical) in restore {
            if let Some(proxy) = self
                .proxy(&host)
                .filter(|proxy| proxy.canonical == canonical)
            {
                let client = self.clone();
                self.request(
                    &proxy,
                    BrokerOperation::Attach {
                        terminal,
                        subscription,
                    },
                    None,
                    Box::new(move |reply| {
                        if reply.is_err() && client.generation.load(Ordering::Acquire) == generation
                        {
                            if let Some(desired) =
                                client.state.lock().unwrap().desired.get_mut(&subscription)
                            {
                                desired.epoch = 0;
                                desired.attaching = false;
                            }
                        }
                    }),
                );
            } else if let Some(desired) = self.state.lock().unwrap().desired.get_mut(&subscription)
            {
                desired.attaching = false;
            }
        }
    }
    fn disconnected(&self) {
        self.out.lock().unwrap().take().inspect(|out| out.stop());
        let pending = {
            let mut st = self.state.lock().unwrap();
            if st.epoch.take().is_some() {
                self.generation.fetch_add(1, Ordering::AcqRel);
            }
            st.client = None;
            for desired in st.desired.values_mut() {
                desired.epoch = 0;
                desired.needs_reset = true;
                desired.attaching = false;
                desired.owner_epoch = None;
            }
            for snapshot in &mut st.snapshots {
                snapshot.status.set_kind(crate::StatusKind::Reconnecting);
            }
            std::mem::take(&mut st.pending)
        };
        for (_, pending) in pending {
            let e = HostError::offline("Host broker owner lost");
            (pending.waiter)(Err(if pending.mutation {
                e.mutation_outcome()
            } else {
                e
            }));
        }
        self.publish();
    }
    fn publish(&self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.broker_changed();
        }
    }
    fn receive(self: &Arc<Self>, event: Event) -> Result<(), HostError> {
        match event {
            Event::Welcome {
                version,
                profile_id,
                owner_epoch,
                client_id,
            } => {
                if version != VERSION || profile_id != self.profile {
                    return Err(HostError::invalid("broker profile/version mismatch"));
                }
                let mut st = self.state.lock().unwrap();
                if st.epoch.is_some() {
                    return Err(HostError::invalid("duplicate broker welcome"));
                }
                st.epoch = Some(owner_epoch);
                st.client = Some(client_id);
                st.sequence = 0;
            }
            Event::Snapshot {
                sequence,
                hosts,
                snapshots,
            } => {
                {
                    let mut st = self.state.lock().unwrap();
                    if st.epoch.is_none() || sequence <= st.sequence {
                        return Err(HostError::invalid("broker snapshot barrier"));
                    }
                    st.sequence = sequence;
                    st.hosts = hosts;
                    st.snapshots = snapshots;
                }
                self.publish();
                self.restore_subscriptions();
            }
            Event::Reply { request_id, result } => {
                let pending = self.state.lock().unwrap().pending.remove(&request_id);
                if let Some(pending) = pending {
                    (pending.waiter)(result);
                }
            }
            Event::AttachReady {
                subscription,
                stream_epoch,
            } => {
                let sink = {
                    let mut st = self.state.lock().unwrap();
                    let epoch = st.epoch;
                    st.desired
                        .get_mut(&subscription)
                        .filter(|d| epoch.is_some() && d.owner_epoch == epoch)
                        .and_then(|d| {
                            d.epoch = stream_epoch;
                            d.attaching = false;
                            let reset = d.needs_reset;
                            d.needs_reset = false;
                            reset.then(|| d.sink.clone())
                        })
                };
                if let Some(sink) = sink {
                    sink.generation_reset();
                }
            }
            Event::Reset {
                subscription,
                stream_epoch,
            } => {
                let sink = {
                    let mut st = self.state.lock().unwrap();
                    let epoch = st.epoch;
                    st.desired
                        .get_mut(&subscription)
                        .filter(|d| epoch.is_some() && d.owner_epoch == epoch)
                        .and_then(|d| {
                            if stream_epoch > d.epoch {
                                d.epoch = stream_epoch;
                                Some(d.sink.clone())
                            } else {
                                None
                            }
                        })
                };
                if let Some(sink) = sink {
                    sink.generation_reset();
                }
            }
            Event::Data {
                subscription,
                stream_epoch,
                bytes,
            } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .map_err(|_| HostError::invalid("broker output encoding"))?;
                if bytes.len() > broker_ipc::CHUNK {
                    return Err(HostError::invalid("broker output chunk bounds"));
                }
                let sink = {
                    let mut st = self.state.lock().unwrap();
                    let epoch = st.epoch;
                    st.desired
                        .get_mut(&subscription)
                        .filter(|d| epoch.is_some() && d.owner_epoch == epoch)
                        .and_then(|d| {
                            if stream_epoch == d.epoch {
                                d.bytes += bytes.len() as u64;
                                Some(d.sink.clone())
                            } else {
                                None
                            }
                        })
                };
                if let Some(sink) = sink {
                    if !sink.data(&bytes) {
                        self.remove_subscription(subscription);
                    }
                }
            }
            Event::Exit {
                subscription,
                stream_epoch,
                code,
                bytes: _,
            } => {
                let sink = {
                    let st = self.state.lock().unwrap();
                    st.desired
                        .get(&subscription)
                        .filter(|d| {
                            d.epoch == stream_epoch
                                && st.epoch.is_some()
                                && d.owner_epoch == st.epoch
                        })
                        .map(|d| (d.sink.clone(), d.bytes))
                };
                if let Some((sink, bytes)) = sink {
                    sink.exit(code, bytes);
                }
            }
            Event::Resync { subscription } => {
                let sink = self
                    .state
                    .lock()
                    .unwrap()
                    .desired
                    .get(&subscription)
                    .map(|d| d.sink.clone());
                self.remove_subscription(subscription);
                if let Some(sink) = sink {
                    sink.resync_required();
                }
            }
        }
        Ok(())
    }
    fn remove_subscription(self: &Arc<Self>, subscription: Uuid) {
        let desired = self.state.lock().unwrap().desired.remove(&subscription);
        if let Some(d) = desired {
            let proxy = BrokerHostProxy {
                client: self.clone(),
                id: d.host,
                canonical: d.canonical,
                connection_generation: self.generation.load(Ordering::Acquire),
            };
            self.request(
                &proxy,
                BrokerOperation::Detach {
                    terminal: d.terminal,
                    subscription,
                },
                None,
                Box::new(|_| {}),
            );
        }
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.disconnected();
    }
}
#[derive(Clone)]
pub struct BrokerHostProxy {
    client: Arc<BrokerClient>,
    id: String,
    canonical: SignKey,
    connection_generation: u64,
}
impl BrokerHostProxy {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn canonical(&self) -> SignKey {
        self.canonical
    }
    pub fn snapshot(&self) -> HostSnapshot {
        let st = self.client.state.lock().unwrap();
        let row = st.hosts.iter().find(|row| row.aliases.contains(&self.id));
        let mut snapshot = row
            .and_then(|row| st.snapshots.iter().find(|s| s.status.host == row.id))
            .cloned()
            .unwrap_or_else(|| HostSnapshot {
                status: crate::HostStatus::initial(&self.id, "", None, 0),
                terminals: None,
            });
        let epoch = st.epoch;
        drop(st);
        if let Some(status) = self.client.manager.upgrade().and_then(|manager| {
            manager
                .action_targets()
                .broker_status(&snapshot.status, epoch, self.canonical)
        }) {
            snapshot.status = status;
        } else {
            snapshot.status.set_kind(crate::StatusKind::Reconnecting);
            snapshot.status.config_generation = 0;
            snapshot.status.link_generation = 0;
            snapshot.terminals = None;
        }
        if self.connection_generation != self.client.generation.load(Ordering::Acquire) {
            snapshot.status.set_kind(crate::StatusKind::Reconnecting);
        }
        snapshot
    }
    pub fn status(&self) -> crate::HostStatus {
        self.snapshot().status
    }
    pub fn terminals(&self) -> Option<Vec<xshell_protocol::msg::TerminalInfo>> {
        self.snapshot().terminals
    }
    pub fn link_generation(&self) -> u64 {
        self.status().link_generation
    }
    pub fn companion_pending(
        &self,
        request: xshell_protocol::ring::companion::CompanionAdminRequest,
        w: Waiter,
    ) {
        self.client
            .request(self, BrokerOperation::CompanionPending { request }, None, w);
    }
    pub fn call(&self, method: String, params: Value, w: Waiter) {
        self.call_cancellable(method, params, w);
    }
    pub fn call_cancellable(&self, method: String, params: Value, w: Waiter) -> u64 {
        self.client
            .request(self, BrokerOperation::Call { method, params }, None, w)
    }
    pub fn cancel(&self, id: u64) {
        self.client.cancel(id);
    }
    fn subscription(&self, terminal: Uuid, sink: Arc<dyn TermSink>) -> Result<Uuid, HostError> {
        let old: Vec<_> = self
            .client
            .state
            .lock()
            .unwrap()
            .desired
            .iter()
            .filter(|(_, d)| d.host == self.id && d.terminal == terminal)
            .map(|(id, _)| *id)
            .collect();
        for subscription in old {
            self.client.remove_subscription(subscription);
        }
        let subscription = Uuid::new_v4();
        let sink = ProxySink::new(sink, self.client.output_bytes.clone());
        let mut st = self.client.state.lock().unwrap();
        if st.desired.len() >= MAX_ATTACHMENTS {
            return Err(HostError::busy());
        }
        let owner_epoch = st.epoch;
        st.desired.insert(
            subscription,
            Desired {
                host: self.id.clone(),
                canonical: self.canonical,
                terminal,
                sink,
                epoch: 0,
                bytes: 0,
                needs_reset: false,
                attaching: true,
                owner_epoch,
            },
        );
        Ok(subscription)
    }
    pub fn term_open(
        &self,
        spec: OpenSpec,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<OpenReply, HostError>) + Send>,
    ) {
        let subscription = match self.subscription(spec.terminal, sink) {
            Ok(s) => s,
            Err(e) => {
                w(Err(e));
                return;
            }
        };
        let terminal = spec.terminal;
        let client = self.client.clone();
        self.client.request(
            self,
            BrokerOperation::Open { spec, subscription },
            None,
            Box::new(move |r| {
                let result: Result<OpenReply, HostError> = r.and_then(|v| {
                    serde_json::from_value(v).map_err(|e| HostError::invalid(e.to_string()))
                });
                if result
                    .as_ref()
                    .is_err_and(|e| e.code != HostErrorCode::Indeterminate)
                    || result
                        .as_ref()
                        .is_ok_and(|r| r.existed && r.terminal.is_some_and(|id| id != terminal))
                {
                    client.remove_subscription(subscription);
                }
                w(result);
            }),
        );
    }
    pub fn term_attach(
        &self,
        terminal: Uuid,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<Option<i32>, HostError>) + Send>,
    ) {
        let subscription = match self.subscription(terminal, sink) {
            Ok(s) => s,
            Err(e) => {
                w(Err(e));
                return;
            }
        };
        let client = self.client.clone();
        self.client.request(
            self,
            BrokerOperation::Attach {
                terminal,
                subscription,
            },
            None,
            Box::new(move |r| {
                if let Err(error) = &r {
                    if matches!(
                        error.code,
                        HostErrorCode::Offline
                            | HostErrorCode::Timeout
                            | HostErrorCode::Busy
                            | HostErrorCode::Indeterminate
                    ) {
                        if let Some(desired) =
                            client.state.lock().unwrap().desired.get_mut(&subscription)
                        {
                            desired.epoch = 0;
                            desired.attaching = false;
                        }
                    } else {
                        client.remove_subscription(subscription);
                    }
                }
                w(r.map(|v| v.get("exitCode").and_then(Value::as_i64).map(|c| c as i32)));
            }),
        );
    }
    pub fn term_detach(&self, terminal: Uuid) -> Result<(), HostError> {
        let subscriptions: Vec<_> = self
            .client
            .state
            .lock()
            .unwrap()
            .desired
            .iter()
            .filter(|(_, d)| d.host == self.id && d.terminal == terminal)
            .map(|(s, _)| *s)
            .collect();
        for subscription in subscriptions {
            self.client.remove_subscription(subscription);
        }
        Ok(())
    }
    pub fn term_input(
        &self,
        terminal: Uuid,
        data: String,
        observed: Option<Uuid>,
        target: Option<InputTarget>,
    ) -> Result<(), HostError> {
        self.enqueue(
            BrokerOperation::Input {
                terminal,
                data,
                handled_completion_id: observed,
            },
            target,
        )
    }
    pub fn term_resize(&self, terminal: Uuid, cols: u16, rows: u16) -> Result<(), HostError> {
        self.enqueue(
            BrokerOperation::Resize {
                terminal,
                cols,
                rows,
            },
            None,
        )
    }
    fn enqueue(
        &self,
        operation: BrokerOperation,
        target: Option<InputTarget>,
    ) -> Result<(), HostError> {
        // The facade's sync API reports admission, not a later Daemon acknowledgement.
        let result = Arc::new(Mutex::new(Ok(())));
        let r = result.clone();
        self.client.request(
            self,
            operation,
            target,
            Box::new(move |reply| {
                *r.lock().unwrap() = reply.map(|_| ());
            }),
        );
        let reply = result.lock().unwrap().clone();
        reply
    }
    pub fn term_close(&self, terminal: Uuid, w: Waiter) {
        self.client
            .request(self, BrokerOperation::Close { terminal }, None, w);
    }
    pub fn term_update(
        &self,
        terminal: Uuid,
        session_id: Option<String>,
        meta: Option<Map<String, Value>>,
        w: Waiter,
    ) {
        self.client.request(
            self,
            BrokerOperation::Update {
                terminal,
                session_id,
                meta,
            },
            None,
            w,
        );
    }
    pub fn term_relaunch(
        &self,
        terminal: Uuid,
        skip: bool,
        w: Box<dyn FnOnce(Result<Option<u32>, HostError>) + Send>,
    ) {
        self.client.request(
            self,
            BrokerOperation::Relaunch { terminal, skip },
            None,
            Box::new(move |r| {
                w(r.map(|v| {
                    v.get("pid")
                        .and_then(Value::as_u64)
                        .and_then(|p| p.try_into().ok())
                }))
            }),
        );
    }
    pub fn upgrade(&self, w: Waiter) {
        w(Err(HostError::invalid(
            "broker access cannot upgrade the Daemon; use its independently configured route",
        )));
    }
    pub fn upgrade_target(&self, _: Option<UpdateTarget>, w: Waiter) {
        self.upgrade(w);
    }
    pub fn kick(&self) {
        self.kick_with_reply(Box::new(|_| {}));
    }
    /// Acknowledges owner admission of the kick, not successful reconnection.
    pub fn kick_with_reply(&self, waiter: Waiter) {
        self.client
            .request(self, BrokerOperation::Kick, None, waiter);
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use broker_ipc::native::{self, Stream};
    use std::io;
    use std::path::{Path, PathBuf};
    // Idle subscriptions have no request deadline. Once a frame starts, its bounded
    // assembly has a separate budget; Hello uses one absolute five-second budget.
    struct FrameReader<'a> {
        stream: &'a mut Stream,
        header: usize,
        until: Option<Instant>,
    }
    impl<'a> FrameReader<'a> {
        fn hello(stream: &'a mut Stream) -> Self {
            Self {
                stream,
                header: 0,
                until: Some(Instant::now() + Duration::from_secs(5)),
            }
        }
        fn frame(stream: &'a mut Stream) -> Self {
            Self {
                stream,
                header: 12,
                until: None,
            }
        }
    }
    impl std::io::Read for FrameReader<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let timeout = self
                .until
                .map(|until| {
                    until
                        .checked_duration_since(Instant::now())
                        .filter(|left| !left.is_zero())
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::TimedOut, "broker frame deadline")
                        })
                })
                .transpose()?;
            self.stream.set_read_timeout(timeout)?;
            let length = if self.until.is_none() {
                bytes.len().min(1)
            } else if self.header > 0 {
                self.header.min(bytes.len())
            } else {
                bytes.len()
            };
            let n = std::io::Read::read(self.stream, &mut bytes[..length])?;
            if n > 0 && self.until.is_none() {
                self.until = Some(Instant::now() + Duration::from_secs(30));
            }
            if self.header > 0 {
                self.header -= n;
            }
            Ok(n)
        }
    }
    pub(crate) struct Server {
        pub broker: Arc<Broker>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    fn serve_stream(
        broker: &Arc<Broker>,
        mut stream: Stream,
    ) -> io::Result<(Stream, Arc<AtomicBool>)> {
        let shutdown = stream.try_clone()?;
        let mut writer = stream.try_clone()?;
        writer.set_write_timeout(Some(Duration::from_secs(2)))?;
        let out = Outbox::new(broker.assembly.clone());
        let session = broker.session(out.clone());
        let write_session = session.clone();
        std::thread::spawn(move || {
            let _ = out.write_to(&mut writer);
            let _ = writer.shutdown(std::net::Shutdown::Both);
            write_session.disconnect();
        });
        let assembly = broker.assembly.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        std::thread::spawn(move || {
            let greeted = match broker_ipc::read_limited(
                &mut FrameReader::hello(&mut stream),
                &assembly,
                4096,
            ) {
                Ok(hello @ ClientMessage::Hello { .. }) => session.receive(hello).is_ok(),
                _ => false,
            };
            if !greeted {
                session.disconnect();
                let _ = stream.shutdown(std::net::Shutdown::Both);
                done.store(true, Ordering::Release);
                return;
            }
            while !session.stopped.load(Ordering::Acquire) {
                match broker_ipc::read(&mut FrameReader::frame(&mut stream), &assembly) {
                    Ok(message) => {
                        if session.receive(message).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        if let Some(busy) = error
                            .get_ref()
                            .and_then(|e| e.downcast_ref::<broker_ipc::AssemblyBusy>())
                        {
                            if busy.request_id != 0 && session.greeted.load(Ordering::Acquire) {
                                session.emit(
                                    &Event::Reply {
                                        request_id: busy.request_id,
                                        result: Err(HostError::busy()),
                                    },
                                    false,
                                );
                                continue;
                            }
                        }
                        break;
                    }
                }
            }
            session.disconnect();
            let _ = stream.shutdown(std::net::Shutdown::Both);
            done.store(true, Ordering::Release);
        });
        Ok((shutdown, finished))
    }
    impl Server {
        /// Called only while DesktopRing holds ring-connector.lock.
        pub fn bind(path: PathBuf, broker: Arc<Broker>) -> io::Result<Self> {
            let listener = native::Listener::bind(path)?;
            let stop = Arc::new(AtomicBool::new(false));
            let done = stop.clone();
            let owner = broker.clone();
            let thread = std::thread::spawn(move || {
                let mut streams = Vec::new();
                while !done.load(Ordering::Acquire) {
                    streams.retain(|(_, done): &(Stream, Arc<AtomicBool>)| {
                        !done.load(Ordering::Acquire)
                    });
                    if let Ok(stream) = listener.accept() {
                        if streams.len() < 32 {
                            if let Ok(stream) = serve_stream(&owner, stream) {
                                streams.push(stream);
                            }
                        }
                    }
                    owner.tick();
                    std::thread::sleep(Duration::from_millis(100));
                }
                owner.stop();
                for (stream, _) in streams {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            });
            Ok(Self {
                broker,
                stop,
                thread: Some(thread),
            })
        }
        pub fn local(&self, client: &Arc<BrokerClient>) -> io::Result<()> {
            let (server, stream) = Stream::pair()?;
            serve_stream(&self.broker, server)?;
            client.connect_stream(stream)
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    impl BrokerClient {
        pub fn connect(self: &Arc<Self>, path: &Path) -> io::Result<()> {
            self.connect_stream(native::connect(path, Duration::from_secs(2))?)
        }
        fn connect_stream(self: &Arc<Self>, mut stream: Stream) -> io::Result<()> {
            let mut writer = stream.try_clone()?;
            writer.set_write_timeout(Some(Duration::from_secs(2)))?;
            let assembly = Arc::new(Assembly::default());
            let out = Outbox::new(assembly.clone());
            out.send(
                &ClientMessage::Hello {
                    version: VERSION,
                    profile_id: self.profile.clone(),
                },
                false,
            )?;
            *self.out.lock().unwrap() = Some(out.clone());
            let client = self.clone();
            std::thread::spawn(move || {
                let _ = out.write_to(&mut writer);
                let _ = writer.shutdown(std::net::Shutdown::Both);
            });
            std::thread::spawn(move || {
                let greeted = match broker_ipc::read_limited(
                    &mut FrameReader::hello(&mut stream),
                    &assembly,
                    4096,
                ) {
                    Ok(welcome @ Event::Welcome { .. }) => client.receive(welcome).is_ok(),
                    _ => false,
                };
                if !greeted {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    client.disconnected();
                    return;
                }
                while !client.stopped.load(Ordering::Acquire) {
                    let Ok(event) =
                        broker_ipc::read(&mut FrameReader::frame(&mut stream), &assembly)
                    else {
                        break;
                    };
                    if client.receive(event).is_err() {
                        break;
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
                client.disconnected();
            });
            Ok(())
        }
    }
    #[cfg(test)]
    mod frame_tests {
        use super::*;
        use std::io::{Read, Write};
        #[test]
        fn partial_post_hello_header_starts_one_absolute_budget_on_its_first_byte() {
            let (mut reader, mut writer) = Stream::pair().unwrap();
            writer.write_all(&[0]).unwrap();
            let mut framed = FrameReader::frame(&mut reader);
            let mut byte = [0];
            assert_eq!(framed.read(&mut byte).unwrap(), 1);
            let until = framed
                .until
                .expect("first frame byte must start assembly budget");
            writer.write_all(&[1]).unwrap();
            assert_eq!(framed.read(&mut byte).unwrap(), 1);
            assert_eq!(
                framed.until,
                Some(until),
                "header progress must not renew budget"
            );
        }
    }
}
#[cfg(unix)]
pub(crate) use unix::Server;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::test_host;
    use crate::manager::tests::{test_config, NullObserver};
    #[test]
    fn shared_sink_reset_precedes_replay_and_preserves_independent_exit_watermarks() {
        let manager = Arc::new(Manager::new(test_config(Arc::new(NullObserver))));
        let broker = Broker::new("test-profile".into(), &manager);
        let out = Outbox::new(broker.assembly.clone());
        let session = broker.session(out.clone());
        let sink = BrokerTerminalSink::new(&crate::attachments::ReplayBudget::default()).unwrap();
        let first = Uuid::new_v4();
        sink.subscribe(&session, first).unwrap();
        sink.data(b"old");
        sink.generation_reset();
        let a = b"\x1bc\x1b[?2004h\xe2\x82";
        let b = b"\xac\x1b[";
        sink.data(a);
        sink.data(b);
        sink.exit(7, 0);
        let second = Uuid::new_v4();
        sink.subscribe(&session, second).unwrap();
        let (mut reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let queue = out.clone();
        let thread = std::thread::spawn(move || queue.write_to(&mut writer));
        let assembly = Arc::new(Assembly::default());
        let events: Vec<Event> = (0..10)
            .map(|_| broker_ipc::read(&mut reader, &assembly).unwrap())
            .collect();
        out.stop();
        thread.join().unwrap().unwrap();
        assert!(matches!(
            events[0],
            Event::AttachReady {
                stream_epoch: 1,
                ..
            }
        ));
        assert!(matches!(
            events[3],
            Event::Reset {
                stream_epoch: 2,
                ..
            }
        ));
        assert!(
            matches!(events[6],Event::Exit{bytes,code:7,..}if bytes==2+3+a.len()as u64+b.len()as u64)
        );
        let Event::Data { bytes: replay, .. } = &events[8] else {
            panic!("missing replay")
        };
        let replay = base64::engine::general_purpose::STANDARD
            .decode(replay)
            .unwrap();
        assert!(replay.starts_with(b"\x1bc\x1b[?2004h"));
        assert!(!replay.windows(3).any(|w| w == b"old"));
        assert!(replay.ends_with(b"\xe2\x82\xac\x1b["));
        assert!(matches!(events[9],Event::Exit{bytes,code:7,..}if bytes==replay.len()as u64));
    }
    #[test]
    fn replay_profile_admission_is_bounded_and_detach_frees_only_its_viewer() {
        let manager = Arc::new(Manager::new(test_config(Arc::new(NullObserver))));
        let broker = Broker::new("quota".into(), &manager);
        let session = broker.session(Outbox::new(broker.assembly.clone()));
        let other = broker.session(Outbox::new(broker.assembly.clone()));
        let handle = Arc::new(HostHandle::new(
            test_host("h_abcdefgh", "test"),
            Arc::new(test_config(Arc::new(NullObserver))),
            1,
        ));
        let mut terminals = Vec::new();
        for _ in 0..MAX_ATTACHMENTS {
            let id = Uuid::new_v4();
            let sub = Uuid::new_v4();
            broker
                .sink(
                    test_slot("canonical", &handle, id),
                    &handle,
                    &session,
                    sub,
                    false,
                )
                .unwrap();
            terminals.push((id, sub));
        }
        assert!(broker
            .sink(
                test_slot("canonical", &handle, Uuid::new_v4()),
                &handle,
                &session,
                Uuid::new_v4(),
                false
            )
            .is_err());
        let (terminal, sub) = terminals[0];
        let other_sub = Uuid::new_v4();
        broker
            .sink(
                test_slot("canonical", &handle, terminal),
                &handle,
                &other,
                other_sub,
                false,
            )
            .unwrap();
        broker.detach(session.id, Some(("canonical", terminal, sub)));
        assert_eq!(broker.terminals.lock().unwrap().len(), MAX_ATTACHMENTS);
        broker.detach(other.id, None);
        assert_eq!(broker.terminals.lock().unwrap().len(), MAX_ATTACHMENTS - 1);
        broker
            .sink(
                test_slot("canonical", &handle, Uuid::new_v4()),
                &handle,
                &session,
                Uuid::new_v4(),
                false,
            )
            .unwrap();
        broker.stop();
    }
}

#[cfg(all(test, unix))]
mod subscription_tests {
    use super::*;
    use crate::manager::tests::{test_config, NullObserver};
    #[test]
    fn unchanged_snapshots_do_not_tick_observers_and_detach_is_owned_not_representative_scoped() {
        let manager = Arc::new(Manager::new(test_config(Arc::new(NullObserver))));
        let broker = Broker::new("profile".into(), &manager);
        let session = broker.session(Outbox::new(broker.assembly.clone()));
        broker.snapshot(&session);
        broker.snapshot(&session);
        assert_eq!(broker.sequence.load(Ordering::Acquire), 1);
        assert_eq!(session.last_snapshot.load(Ordering::Acquire), 1);
        let other = broker.session(Outbox::new(broker.assembly.clone()));
        let handle = Arc::new(HostHandle::new(
            crate::config::test_host("h_abcdefgh", "test"),
            Arc::new(test_config(Arc::new(NullObserver))),
            1,
        ));
        let terminal = Uuid::new_v4();
        let subscription = Uuid::new_v4();
        broker
            .sink(
                test_slot("old-representative", &handle, terminal),
                &handle,
                &session,
                subscription,
                false,
            )
            .unwrap();
        broker.detach_subscription(other.id, terminal, subscription);
        assert_eq!(broker.terminals.lock().unwrap().len(), 1);
        broker.detach_subscription(session.id, terminal, subscription);
        assert!(broker.terminals.lock().unwrap().is_empty());
        broker.stop();
    }
}

#[cfg(all(test, unix))]
mod review_controls_tests {
    use super::*;
    #[derive(Default)]
    struct Blocked {
        state: Mutex<(bool, bool, bool)>,
        cv: Condvar,
    }
    impl TermSink for Blocked {
        fn data(&self, _: &[u8]) -> bool {
            let mut s = self.state.lock().unwrap();
            s.0 = true;
            self.cv.notify_all();
            while !s.1 {
                s = self.cv.wait(s).unwrap();
            }
            true
        }
        fn exit(&self, _: i32, _: u64) {
            let mut s = self.state.lock().unwrap();
            s.2 = true;
            self.cv.notify_all();
        }
        fn generation_reset(&self) {
            let mut s = self.state.lock().unwrap();
            s.2 = true;
            self.cv.notify_all();
        }
        fn resync_required(&self) {
            let mut s = self.state.lock().unwrap();
            s.2 = true;
            self.cv.notify_all();
        }
    }
    fn full_control(reset: bool) {
        let sink = Arc::new(Blocked::default());
        let queued = ProxySink::new(sink.clone(), Arc::new(AtomicUsize::new(0)));
        assert!(queued.data(b"x"));
        {
            let s = sink.state.lock().unwrap();
            let (s, _) = sink
                .cv
                .wait_timeout_while(s, Duration::from_secs(2), |s| !s.0)
                .unwrap();
            assert!(s.0);
        }
        for _ in 0..4096 {
            assert!(queued.data(b"x"));
        }
        if reset {
            queued.generation_reset();
        } else {
            queued.exit(7, 4097);
        }
        {
            let mut s = sink.state.lock().unwrap();
            s.1 = true;
            sink.cv.notify_all();
        }
        let s = sink.state.lock().unwrap();
        let (s, _) = sink
            .cv
            .wait_timeout_while(s, Duration::from_secs(2), |s| !s.2)
            .unwrap();
        assert!(
            s.2,
            "full callback queue silently lost its control instead of delivery or resync"
        );
    }
    #[test]
    fn repeated_resync_preserves_the_already_queued_notification() {
        let sink = Arc::new(Blocked::default());
        let queued = ProxySink::new(sink.clone(), Arc::new(AtomicUsize::new(0)));
        queued.data(b"x");
        {
            let s = sink.state.lock().unwrap();
            let (s, _) = sink
                .cv
                .wait_timeout_while(s, Duration::from_secs(2), |s| !s.0)
                .unwrap();
            assert!(s.0);
        }
        queued.resync_required();
        queued.resync_required();
        {
            let mut s = sink.state.lock().unwrap();
            s.1 = true;
            sink.cv.notify_all();
        }
        let s = sink.state.lock().unwrap();
        let (s, _) = sink
            .cv
            .wait_timeout_while(s, Duration::from_secs(2), |s| !s.2)
            .unwrap();
        assert!(s.2, "repeated Resync erased the pending notification");
    }
    #[test]
    fn full_callback_queue_exit_is_delivered_or_resynced() {
        full_control(false)
    }
    #[test]
    fn full_callback_queue_reset_is_delivered_or_resynced() {
        full_control(true)
    }
}

#[cfg(all(test, unix))]
mod recovery_identity_tests {
    use super::*;
    struct Quiet;
    impl TermSink for Quiet {
        fn data(&self, _: &[u8]) -> bool {
            true
        }
        fn exit(&self, _: i32, _: u64) {}
    }

    use crate::manager::tests::{test_config, NullObserver};
    #[test]
    fn owner_loss_does_not_retarget_retained_mutable_alias_intent_to_another_key() {
        let manager = Arc::new(Manager::new(test_config(Arc::new(NullObserver))));
        let client = BrokerClient::new("profile".into(), &manager);
        let k1 = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let k2 = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let terminal = Uuid::new_v4();
        let subscription = Uuid::new_v4();
        {
            let mut st = client.state.lock().unwrap();
            st.epoch = Some(Uuid::new_v4());
            st.hosts = vec![AvailableHost {
                id: "h_abcdefgh".into(),
                aliases: vec!["h_abcdefgh".into()],
                name: "replacement".into(),
                color: None,
                daemon_key: Some(k2),
                routes: vec![super::super::hosts::RouteKind::Ssh],
                active_route: super::super::hosts::RouteKind::Ssh,
                reachability: super::super::hosts::Reachability::Online,
            }];
            let mut status = crate::HostStatus::initial("h_abcdefgh", "test", None, 1);
            status.set_kind(crate::StatusKind::Connected);
            status.link_generation = 1;
            st.snapshots = vec![HostSnapshot {
                status,
                terminals: Some(vec![crate::handle::tests::info(terminal)]),
            }];
            st.desired.insert(
                subscription,
                Desired {
                    host: "h_abcdefgh".into(),
                    canonical: k1,
                    terminal,
                    sink: Arc::new(Quiet),
                    epoch: 0,
                    bytes: 0,
                    needs_reset: true,
                    attaching: false,
                    owner_epoch: None,
                },
            );
        }
        *client.out.lock().unwrap() = Some(Outbox::new(Arc::new(Assembly::default())));
        client.restore_subscriptions();
        assert!(
            client.state.lock().unwrap().pending.is_empty(),
            "K1 intent was attached using a K2 proxy"
        );
        client.stop();
    }
}

#[cfg(all(test, unix))]
mod kick_admission_tests {
    use super::*;
    use crate::manager::tests::{test_config, NullObserver};
    #[test]
    fn verified_offline_proxy_enqueues_kick_instead_of_refusing_it_for_no_link() {
        let manager = Arc::new(Manager::new(test_config(Arc::new(NullObserver))));
        let client = BrokerClient::new("profile".into(), &manager);
        let key = xshell_protocol::ring::DeviceKeys::generate()
            .unwrap()
            .sign_key();
        let host = super::super::hosts::relay_host_id(&key);
        {
            let mut st = client.state.lock().unwrap();
            st.epoch = Some(Uuid::new_v4());
            st.hosts = vec![AvailableHost {
                id: host.clone(),
                aliases: vec![host.clone()],
                name: "offline".into(),
                color: None,
                daemon_key: Some(key),
                routes: vec![super::super::hosts::RouteKind::Relay],
                active_route: super::super::hosts::RouteKind::Relay,
                reachability: super::super::hosts::Reachability::Unknown,
            }];
            st.snapshots = vec![HostSnapshot {
                status: crate::HostStatus::initial(&host, "test", None, 1),
                terminals: None,
            }];
        }
        *client.out.lock().unwrap() = Some(Outbox::new(Arc::new(Assembly::default())));
        client.proxy(&host).unwrap().kick();
        assert_eq!(
            client.state.lock().unwrap().pending.len(),
            1,
            "authorized Kick never reached the owner channel while offline"
        );
        client.stop();
    }
}

#[cfg(all(test, unix))]
mod slot_configuration_tests {
    use super::*;
    use crate::manager::tests::{test_config, NullObserver};
    #[test]
    fn representative_uuid_slot_cannot_reuse_another_configuration_admission() {
        let manager = Arc::new(Manager::new(test_config(Arc::new(NullObserver))));
        let broker = Broker::new("profile".into(), &manager);
        let session = broker.session(Outbox::new(broker.assembly.clone()));
        let cfg = crate::config::test_host("h_abcdefgh", "target");
        let first = Arc::new(HostHandle::new(
            cfg.clone(),
            Arc::new(test_config(Arc::new(NullObserver))),
            1,
        ));
        let next = Arc::new(HostHandle::new(
            cfg,
            Arc::new(test_config(Arc::new(NullObserver))),
            2,
        ));
        let terminal = Uuid::new_v4();
        let (old, _) = broker
            .sink(
                test_slot("h_abcdefgh", &first, terminal),
                &first,
                &session,
                Uuid::new_v4(),
                false,
            )
            .unwrap();
        old.complete(Ok(None));
        let (_, fresh) = broker
            .sink(
                test_slot("h_abcdefgh", &next, terminal),
                &next,
                &session,
                Uuid::new_v4(),
                false,
            )
            .unwrap();
        assert!(
            fresh,
            "K2/config2 Attach returned K1/config1 cached admission"
        );
        broker.stop();
    }
}

#[cfg(all(test, unix))]
mod server_resync_tests {
    use super::*;
    use crate::manager::tests::{test_config, NullObserver};
    #[test]
    fn server_outbox_resync_releases_all_inactive_profile_slots_without_disconnect() {
        let manager = Arc::new(Manager::new(test_config(Arc::new(NullObserver))));
        let broker = Broker::new("profile".into(), &manager);
        let session = broker.session(Outbox::new(broker.assembly.clone()));
        let handle = Arc::new(HostHandle::new(
            crate::config::test_host("h_abcdefgh", "test"),
            Arc::new(test_config(Arc::new(NullObserver))),
            1,
        ));
        let mut sinks = Vec::new();
        for _ in 0..MAX_ATTACHMENTS {
            sinks.push(
                broker
                    .sink(
                        test_slot("h_abcdefgh", &handle, Uuid::new_v4()),
                        &handle,
                        &session,
                        Uuid::new_v4(),
                        false,
                    )
                    .unwrap()
                    .0,
            );
        }
        let chunk = vec![b'x'; broker_ipc::CHUNK];
        for sink in &sinks {
            while sink.data(&chunk) {}
        }
        assert!(
            !session.stopped.load(Ordering::Acquire),
            "output pressure must not consume control admission"
        );
        drop(sinks);
        let admitted = broker.sink(
            test_slot("h_abcdefgh", &handle, Uuid::new_v4()),
            &handle,
            &session,
            Uuid::new_v4(),
            false,
        );
        assert!(
            admitted.is_ok(),
            "last-subscriber Resync retained every replay permit/table slot"
        );
        broker.stop();
    }
}

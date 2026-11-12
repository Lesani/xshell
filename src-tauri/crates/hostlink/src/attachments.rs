//! Operational facade attachments. The low-level HostHandle API remains single-sink.
use crate::{HostError, TermSink};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use xshell_core::terminal::replay::ReplayBuffer;

static INTERACTION: AtomicU64 = AtomicU64::new(0);
pub(crate) fn interaction() -> u64 {
    INTERACTION.fetch_add(1, Ordering::Relaxed) + 1
}
pub(crate) type ScopedResize = Box<dyn Fn(u16, u16) -> Result<(), HostError> + Send + Sync>;
const REPLAY: usize = 2 * 1024 * 1024;
#[derive(Clone, Default)]
pub(crate) struct ReplayBudget(Arc<AtomicUsize>);
pub(crate) struct ReplayPermit(ReplayBudget);
impl ReplayBudget {
    pub fn reserve(&self) -> Result<ReplayPermit, HostError> {
        self.0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 64).then_some(n + 1)
            })
            .map(|_| ReplayPermit(self.clone()))
            .map_err(|_| HostError::busy())
    }
}
impl Drop for ReplayPermit {
    fn drop(&mut self) {
        self.0 .0.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(crate) type Fence = Box<dyn Fn() -> bool + Send + Sync>;
type Attached = Box<dyn FnOnce(Result<Option<i32>, HostError>) + Send>;
pub(crate) struct ViewerAccess {
    pub fence: Fence,
    pub config: u64,
    pub resize: ScopedResize,
    pub direct: bool,
}
struct Viewer {
    sink: Arc<dyn TermSink>,
    fence: Fence,
    config: u64,
    bytes: u64,
    direct: bool,
    size: Option<(u16, u16)>,
    interaction: u64,
    active: bool,
    resize: ScopedResize,
}
#[derive(Clone, Copy)]
pub(crate) struct Registration {
    pub physical: bool,
    pub epoch: u64,
}
struct State {
    admission_epoch: u64,
    viewers: HashMap<Uuid, Viewer>,
    // A broker-only hub already has its bounded replay in BrokerTerminalSink. A Direct
    // viewer needs a cache too; both caches debit the same 64-buffer profile budget.
    replay: Option<(ReplayBuffer, ReplayPermit)>,
    admission: Option<Result<Option<i32>, HostError>>,
    waiters: Vec<(u64, Attached)>,
    exit: Option<i32>,
    reset_generation: Option<u64>,
}
pub(crate) struct Hub {
    state: Mutex<State>,
    budget: ReplayBudget,
}
impl Hub {
    pub fn new(budget: ReplayBudget) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                admission_epoch: 0,
                viewers: HashMap::new(),
                replay: None,
                admission: None,
                waiters: Vec::new(),
                exit: None,
                reset_generation: None,
            }),
            budget,
        })
    }
    /// Atomic replay cut and registration, before the one physical Attach/Open.
    pub fn register(
        &self,
        key: Uuid,
        sink: Arc<dyn TermSink>,
        access: ViewerAccess,
        opening: bool,
    ) -> Result<Registration, HostError> {
        let ViewerAccess {
            fence,
            config,
            resize,
            direct,
        } = access;
        let mut st = self.state.lock().unwrap();
        let first = !st.viewers.values().any(|viewer| (viewer.fence)());
        if opening && !first {
            return Err(HostError::invalid(
                "Terminal already has viewers; reconcile before Open",
            ));
        }
        if !fence() {
            return Err(HostError::offline("attachment connection changed"));
        }
        let replay = if first {
            None
        } else {
            st.replay.as_ref().map(|(r, _)| r.snapshot()).or_else(|| {
                st.viewers
                    .values()
                    .filter(|v| (v.fence)())
                    .find_map(|v| v.sink.replay_snapshot())
            })
        };
        if direct && st.replay.is_none() {
            let permit = self.budget.reserve()?;
            let mut buffer = ReplayBuffer::new(REPLAY);
            if let Some(replay) = &replay {
                buffer.push(replay);
            }
            st.replay = Some((buffer, permit));
        }
        let mut viewer = Viewer {
            sink,
            fence,
            config,
            bytes: 0,
            direct,
            size: None,
            interaction: 0,
            active: true,
            resize,
        };
        if let Some(replay) = replay {
            if !viewer.sink.data(&replay) {
                return Err(HostError::busy());
            }
            viewer.bytes += replay.len() as u64;
        }
        if !first {
            if let Some(code) = st.exit {
                viewer.sink.exit(code, viewer.bytes);
            }
        }
        st.viewers.insert(key, viewer);
        if first {
            st.admission_epoch += 1;
            st.admission = None;
            st.exit = None;
            if let Some((replay, _)) = &mut st.replay {
                *replay = ReplayBuffer::new(REPLAY);
            }
        }
        Ok(Registration {
            physical: first,
            epoch: st.admission_epoch,
        })
    }
    pub fn wait(&self, epoch: u64, waiter: Attached) {
        let ready = {
            let mut st = self.state.lock().unwrap();
            if st.admission_epoch != epoch {
                Some(Err(HostError::offline("attachment admission changed")))
            } else if let Some(result) = &st.admission {
                Some(result.clone())
            } else {
                st.waiters.push((epoch, waiter));
                return;
            }
        };
        waiter(ready.unwrap());
    }
    pub fn complete(&self, epoch: u64, result: Result<Option<i32>, HostError>) {
        let (current, waiters) = {
            let mut st = self.state.lock().unwrap();
            let current = st.admission_epoch == epoch;
            if current {
                st.admission = Some(result.clone());
            }
            let (done, pending) = std::mem::take(&mut st.waiters)
                .into_iter()
                .partition::<Vec<_>, _>(|(waiting, _)| current || *waiting == epoch);
            st.waiters = pending;
            (current, done)
        };
        for (waiting, waiter) in waiters {
            waiter(if current && waiting == epoch {
                result.clone()
            } else {
                Err(HostError::offline("attachment admission changed"))
            });
        }
    }
    pub fn remove(&self, key: Uuid) -> (bool, Option<(u16, u16)>) {
        let mut st = self.state.lock().unwrap();
        Self::remove_locked(&mut st, key)
    }
    pub fn remove_current(&self, epoch: u64, key: Uuid) {
        let mut st = self.state.lock().unwrap();
        if st.admission_epoch == epoch {
            Self::remove_locked(&mut st, key);
        }
    }
    fn remove_locked(st: &mut State, key: Uuid) -> (bool, Option<(u16, u16)>) {
        st.viewers.remove(&key);
        if !st.viewers.values().any(|v| v.direct) {
            st.replay = None;
        }
        let selected = st
            .viewers
            .values()
            .filter(|v| (v.fence)())
            .max_by_key(|v| v.interaction);
        let size = selected.and_then(|v| v.size);
        if let (Some(viewer), Some((cols, rows))) = (selected, size) {
            let _ = (viewer.resize)(cols, rows);
        }
        (st.viewers.is_empty(), size)
    }
    pub fn perform(
        &self,
        key: Uuid,
        size: Option<(u16, u16)>,
        dispatch: impl FnOnce(Option<(u16, u16)>, Option<u64>) -> Result<(), HostError>,
    ) -> Result<u64, HostError> {
        let mut st = self.state.lock().unwrap();
        let Some(viewer) = st.viewers.get(&key) else {
            dispatch(size, None)?;
            return Ok(interaction());
        };
        if !(viewer.fence)() {
            return Err(HostError::offline("attachment connection changed"));
        }
        let selected = size.or(viewer.size);
        let config = viewer.config;
        dispatch(selected, Some(config))?;
        let sequence = interaction();
        let viewer = st.viewers.get_mut(&key).expect("registered under hub lock");
        viewer.interaction = sequence;
        if size.is_some() {
            viewer.size = size;
        }
        Ok(sequence)
    }
    pub fn refresh_proof(&self) {
        let mut st = self.state.lock().unwrap();
        let replay = st.replay.as_ref().map(|(replay, _)| replay.snapshot());
        for viewer in st.viewers.values_mut() {
            let eligible = (viewer.fence)();
            if eligible && !viewer.active {
                if let Some(replay) = &replay {
                    if viewer.sink.data(replay) {
                        viewer.bytes += replay.len() as u64;
                    }
                }
            }
            viewer.active = eligible;
        }
    }
    pub fn restore_viewer_size(&self, key: Uuid, size: (u16, u16), sequence: u64) {
        let mut st = self.state.lock().unwrap();
        if let Some(viewer) = st.viewers.get_mut(&key) {
            viewer.size = Some(size);
            viewer.interaction = sequence;
        }
        if let Some(viewer) = st
            .viewers
            .values()
            .filter(|v| (v.fence)())
            .max_by_key(|v| v.interaction)
        {
            if let Some((cols, rows)) = viewer.size {
                let _ = (viewer.resize)(cols, rows);
            }
        }
    }
    pub fn empty(&self) -> bool {
        self.state.lock().unwrap().viewers.is_empty()
    }
    pub fn interaction(
        &self,
        key: Uuid,
        size: Option<(u16, u16)>,
    ) -> Result<Option<(u16, u16)>, HostError> {
        let mut st = self.state.lock().unwrap();
        let sequence = interaction();
        let Some(viewer) = st.viewers.get_mut(&key) else {
            return Ok(None);
        };
        if !(viewer.fence)() {
            return Err(HostError::offline("attachment connection changed"));
        }
        viewer.interaction = sequence;
        if size.is_some() {
            viewer.size = size;
        }
        Ok(viewer.size)
    }
}
impl TermSink for Hub {
    fn data(&self, bytes: &[u8]) -> bool {
        let mut st = self.state.lock().unwrap();
        if let Some((replay, _)) = &mut st.replay {
            replay.push(bytes);
        }
        st.viewers.retain(|_, viewer| {
            if !(viewer.fence)() {
                viewer.active = false;
                return true;
            }
            if !viewer.sink.data(bytes) {
                return false;
            }
            viewer.bytes += bytes.len() as u64;
            true
        });
        if !st.viewers.values().any(|v| v.direct) {
            st.replay = None;
        }
        !st.viewers.is_empty()
    }
    fn exit(&self, code: i32, _: u64) {
        let mut st = self.state.lock().unwrap();
        st.exit = Some(code);
        for viewer in st.viewers.values() {
            if (viewer.fence)() {
                viewer.sink.exit(code, viewer.bytes);
            }
        }
    }
    fn generation_reset(&self) {
        let mut st = self.state.lock().unwrap();
        if let Some((replay, _)) = &mut st.replay {
            *replay = ReplayBuffer::new(REPLAY);
        }
        st.exit = None;
        for viewer in st.viewers.values_mut() {
            viewer.active = (viewer.fence)();
            viewer.sink.generation_reset();
        }
    }
    fn reset_for_generation(&self, generation: u64) {
        {
            let mut st = self.state.lock().unwrap();
            if st.reset_generation == Some(generation) {
                return;
            }
            st.reset_generation = Some(generation);
        }
        self.generation_reset();
    }
    fn reattach_size(&self) -> Option<Option<(u16, u16)>> {
        Some(
            self.state
                .lock()
                .unwrap()
                .viewers
                .values()
                .filter(|v| (v.fence)())
                .max_by_key(|v| v.interaction)
                .and_then(|v| v.size),
        )
    }
    fn can_reattach(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .viewers
            .values()
            .any(|v| (v.fence)())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    #[derive(Default)]
    struct Sink {
        data: Mutex<Vec<u8>>,
        exits: Mutex<Vec<u64>>,
    }
    impl TermSink for Sink {
        fn data(&self, b: &[u8]) -> bool {
            self.data.lock().unwrap().extend_from_slice(b);
            true
        }
        fn exit(&self, _: i32, bytes: u64) {
            self.exits.lock().unwrap().push(bytes);
        }
    }
    fn fence(current: &Arc<AtomicBool>) -> Fence {
        let current = current.clone();
        Box::new(move || current.load(Ordering::Acquire))
    }
    #[test]
    fn alias_fences_k2_replay_exit_and_size_while_independent_direct_viewer_continues() {
        let hub = Hub::new(ReplayBudget::default());
        let direct = Arc::new(Sink::default());
        let alias = Arc::new(Sink::default());
        let direct_current = Arc::new(AtomicBool::new(true));
        let alias_current = Arc::new(AtomicBool::new(true));
        let (d, a) = (Uuid::new_v4(), Uuid::new_v4());
        let first = hub
            .register(
                d,
                direct.clone(),
                ViewerAccess {
                    fence: fence(&direct_current),
                    config: 1,
                    resize: Box::new(|_, _| Ok(())),
                    direct: true,
                },
                false,
            )
            .unwrap();
        hub.complete(first.epoch, Ok(None));
        hub.data(b"K1");
        hub.register(
            a,
            alias.clone(),
            ViewerAccess {
                fence: fence(&alias_current),
                config: 1,
                resize: Box::new(|_, _| Ok(())),
                direct: false,
            },
            false,
        )
        .unwrap();
        hub.interaction(d, Some((80, 24))).unwrap();
        hub.interaction(a, Some((90, 30))).unwrap();
        let before = alias.data.lock().unwrap().len();
        alias_current.store(false, Ordering::Release);
        hub.reset_for_generation(2);
        assert!(hub.can_reattach());
        hub.data(b"K2-ONLY");
        hub.exit(7, 0);
        assert_eq!(alias.data.lock().unwrap().len(), before);
        assert!(alias.exits.lock().unwrap().is_empty());
        assert!(hub
            .perform(a, None, |_, _| panic!("alias must not dispatch on K2"))
            .is_err());
        assert_eq!(hub.reattach_size(), Some(Some((80, 24))));
        assert!(String::from_utf8_lossy(&direct.data.lock().unwrap()).contains("K2-ONLY"));
        assert_eq!(direct.exits.lock().unwrap().len(), 1);
        // On another reconnect, unverified replay is retained only for Direct. Current
        // proof activates the alias from an atomic cut of this generation's cache.
        hub.reset_for_generation(3);
        hub.data(b"K1-RESTORED");
        alias_current.store(true, Ordering::Release);
        hub.refresh_proof();
        let bytes = alias.data.lock().unwrap().clone();
        assert!(String::from_utf8_lossy(&bytes).contains("K1-RESTORED"));
        assert!(!String::from_utf8_lossy(&bytes).contains("K2-ONLY"));
        hub.remove(a);
        hub.data(b"DIRECT-AFTER-WITHDRAWAL");
        assert!(String::from_utf8_lossy(&direct.data.lock().unwrap())
            .contains("DIRECT-AFTER-WITHDRAWAL"));
        direct_current.store(false, Ordering::Release);
        assert!(!hub.can_reattach());
        let count = direct.data.lock().unwrap().len();
        hub.data(b"STALE-CONFIG");
        assert_eq!(direct.data.lock().unwrap().len(), count);
    }
    #[test]
    fn direct_and_broker_replay_caches_share_one_profile_reservation_budget() {
        let budget = ReplayBudget::default();
        let mut permits: Vec<_> = (0..64).map(|_| budget.reserve().unwrap()).collect();
        assert!(budget.reserve().is_err());
        permits.pop();
        assert!(budget.reserve().is_ok());
    }
}

#[cfg(test)]
mod arbitration_tests {
    use super::*;
    struct Sink;
    impl TermSink for Sink {
        fn data(&self, _: &[u8]) -> bool {
            true
        }
        fn exit(&self, _: i32, _: u64) {}
    }
    #[test]
    fn departing_broker_viewer_restores_older_group_order_without_overriding_newer_direct_interaction(
    ) {
        let hub = Hub::new(ReplayBudget::default());
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let (d, a) = (Uuid::new_v4(), Uuid::new_v4());
        let access = |direct| {
            let sizes = sizes.clone();
            ViewerAccess {
                fence: Box::new(|| true),
                config: 1,
                resize: Box::new(move |cols, rows| {
                    sizes.lock().unwrap().push((cols, rows));
                    Ok(())
                }),
                direct,
            }
        };
        hub.register(d, Arc::new(Sink), access(true), false)
            .unwrap();
        hub.register(a, Arc::new(Sink), access(false), false)
            .unwrap();
        let older_broker = hub.perform(a, Some((90, 30)), |_, _| Ok(())).unwrap();
        hub.perform(d, Some((80, 24)), |_, _| Ok(())).unwrap();
        hub.perform(a, Some((100, 40)), |_, _| Ok(())).unwrap();
        // The newest broker viewer leaves; its group's older viewer is older than Direct.
        hub.restore_viewer_size(a, (90, 30), older_broker);
        assert_eq!(sizes.lock().unwrap().last(), Some(&(80, 24)));
        assert_eq!(hub.reattach_size(), Some(Some((80, 24))));
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use std::sync::{atomic::AtomicBool, mpsc};
    struct Sink;
    impl TermSink for Sink {
        fn data(&self, _: &[u8]) -> bool {
            true
        }
        fn exit(&self, _: i32, _: u64) {}
    }
    #[test]
    fn obsolete_configuration_completion_cannot_complete_or_remove_replacement_admission() {
        let hub = Hub::new(ReplayBudget::default());
        let valid = Arc::new(AtomicBool::new(true));
        let old_valid = valid.clone();
        let access = |config, fence| ViewerAccess {
            config,
            fence,
            resize: Box::new(|_, _| Ok(())),
            direct: false,
        };
        let old = hub
            .register(
                Uuid::new_v4(),
                Arc::new(Sink),
                access(1, Box::new(move || old_valid.load(Ordering::Acquire))),
                false,
            )
            .unwrap();
        let (tx, rx) = mpsc::channel();
        hub.wait(old.epoch, Box::new(move |r| tx.send(r).unwrap()));
        valid.store(false, Ordering::Release);
        let key = Uuid::new_v4();
        let current = hub
            .register(key, Arc::new(Sink), access(2, Box::new(|| true)), false)
            .unwrap();
        assert!(current.physical);
        let (tx2, rx2) = mpsc::channel();
        hub.wait(current.epoch, Box::new(move |r| tx2.send(r).unwrap()));
        hub.complete(old.epoch, Ok(None));
        assert!(rx.recv().unwrap().is_err());
        assert!(rx2.try_recv().is_err());
        hub.remove_current(old.epoch, key);
        assert!(!hub.empty());
        hub.complete(current.epoch, Ok(None));
        assert!(rx2.recv().unwrap().is_ok());
    }
}

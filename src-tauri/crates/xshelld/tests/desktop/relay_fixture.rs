//! Public pairing fixture shared by owner routing and separate-process broker tests.
// Shared by several test crates, each using only part of it (some parts are feature-gated).
#![allow(unused_imports, dead_code)]
use super::common;
use super::common::TestHome;
use super::workerd::Relay;
use serde_json::Value;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::hosts::{relay_host_id, AvailableHost};
use xshell_hostlink::ring::{
    DesktopRing, DesktopRingConfig, PairingEvent, PairingFlow, PairingObserver, RingObserver,
    RingView,
};
use xshell_hostlink::{
    BinarySource, HostConfig, HostError, HostHandle, HostStatus, Manager, ManagerConfig, Observer,
    TermSink, Transport, TransportFactory,
};
use xshell_protocol::msg::TerminalInfo;
use xshell_protocol::ring::{RosterChain, SignKey};
use xshelld::server::{Config, Server, ServerHandle};
pub(super) const WAIT: Duration = Duration::from_secs(15);
#[derive(Default)]
pub(super) struct Events {
    pub(super) available: Mutex<Vec<Vec<AvailableHost>>>,
    pub(super) statuses: Mutex<std::collections::HashMap<String, HostStatus>>,
    pub(super) sync: Mutex<Option<&'static xshell_hostlink::ring::HostSync>>,
}
impl Observer for Events {
    fn status(&self, status: &HostStatus) {
        self.statuses
            .lock()
            .unwrap()
            .insert(status.host.clone(), status.clone());
        if let Some(sync) = *self.sync.lock().unwrap() {
            sync.observe(status);
        }
    }
    fn terminals(&self, _: &str, _: &[TerminalInfo]) {}
    fn available(&self, hosts: &[AvailableHost]) {
        self.available.lock().unwrap().push(hosts.to_vec());
    }
}
pub(super) struct Quiet;
impl RingObserver for Quiet {
    fn changed(&self, _: &RingView) {}
}
impl PairingObserver for Quiet {
    fn pairing(&self, _: PairingFlow, _: &PairingEvent) {}
}
pub(super) struct NoSsh;
impl TransportFactory for NoSsh {
    fn for_host(&self, _: &HostConfig) -> Box<dyn Transport> {
        panic!("Relay must not construct an SSH transport")
    }
    fn direct(&self, _: &HostConfig) -> Option<Box<dyn xshell_hostlink::Dialer>> {
        panic!("Relay must not probe a configured direct route")
    }
}
impl BinarySource for NoSsh {
    fn fetch(
        &self,
        _: &xshell_hostlink::Wanted,
        _: &xshell_hostlink::CancelToken,
    ) -> Result<xshell_hostlink::Fetched, String> {
        panic!("Relay must not install a Daemon")
    }
}
pub(super) struct Computer {
    pub(super) daemon: Option<ServerHandle>,
    pub(super) ring: Arc<DesktopRing>,
    pub(super) manager: Arc<Manager>,
    pub(super) events: Arc<Events>,
    pub(super) home: TestHome,
}
impl Computer {
    pub(super) fn new(relay: &Relay) -> Self {
        Self::with_config(relay, |_| {})
    }
    pub(super) fn with_config(relay: &Relay, tweak: impl FnOnce(&mut Config)) -> Self {
        Self::with_factory(relay, tweak, |_| Arc::new(NoSsh))
    }
    pub(super) fn with_ring_config(
        relay: &Relay,
        tweak: impl FnOnce(&mut DesktopRingConfig),
    ) -> Self {
        Self::with_options(relay, |_| {}, |_| Arc::new(NoSsh), tweak)
    }
    pub(super) fn with_factory(
        relay: &Relay,
        tweak: impl FnOnce(&mut Config),
        factory: impl FnOnce(&TestHome) -> Arc<dyn TransportFactory>,
    ) -> Self {
        Self::with_options(relay, tweak, factory, |_| {})
    }
    fn with_options(
        relay: &Relay,
        tweak: impl FnOnce(&mut Config),
        factory: impl FnOnce(&TestHome) -> Arc<dyn TransportFactory>,
        ring_config: impl FnOnce(&mut DesktopRingConfig),
    ) -> Self {
        let home = TestHome::new();
        let mut cfg = Config::new(home.home(), home.paths());
        cfg.ring_backoff_unit = Duration::from_millis(10);
        tweak(&mut cfg);
        let daemon = Server::start(cfg).unwrap();
        let mut cfg = DesktopRingConfig::new(home.root().join("desktop"), "same name".into());
        cfg.default_relay_url = relay.url();
        cfg.owner_retry = Duration::from_millis(20);
        cfg.backoff_unit = Duration::from_millis(10);
        ring_config(&mut cfg);
        let ring = DesktopRing::open(cfg, Arc::new(Quiet));
        let events = Arc::new(Events::default());
        let mut cfg = ManagerConfig::new("1.5.0", factory(&home), Arc::new(NoSsh), events.clone());
        cfg.backoff_unit = Duration::from_millis(20);
        let manager = Arc::new(Manager::new(cfg));
        ring.attach_manager(&manager).unwrap();
        Self {
            daemon: Some(daemon),
            ring,
            manager,
            events,
            home,
        }
    }
    pub(super) fn companion(&self) -> CompanionClient {
        CompanionClient::for_store(
            self.daemon
                .as_ref()
                .unwrap()
                .companion_endpoint()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .unwrap()
    }
    pub(super) fn create(&self) -> RosterChain {
        let companion = self.companion();
        self.ring.register_companion(&companion).unwrap();
        match self
            .ring
            .create_computer(&companion, |p, lease| {
                Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
            })
            .unwrap()
        {
            AdoptionOutcome::Installed { chain, .. } => chain,
            _ => panic!("Create not installed"),
        }
    }
    pub(super) fn join(&self, approver: &Self) -> RosterChain {
        let companion = self.companion();
        self.ring.register_companion(&companion).unwrap();
        let offer = self.ring.begin_join(&companion).unwrap();
        let code = offer.code.clone();
        std::thread::scope(|scope| {
            let approval = scope.spawn(|| approver.ring.approve_device_code(&code));
            let (p, lease) = offer.await_consent(WAIT).unwrap();
            offer
                .confirm_join(lease.confirm(&p.proposal_hash).unwrap())
                .unwrap();
            let result = offer.wait().unwrap();
            approval.join().unwrap().unwrap();
            match result {
                AdoptionOutcome::Installed { chain, .. } => chain,
                _ => panic!("Join not installed"),
            }
        })
    }
    #[cfg(feature = "legacy-pair-test-control")]
    pub(super) fn legacy_pair(&self, approver: &Self, relay: &Relay) -> SignKey {
        let (tx, rx) = mpsc::channel();
        struct Lines(Vec<u8>, mpsc::Sender<String>);
        impl Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.extend_from_slice(bytes);
                while let Some(at) = self.0.iter().position(|b| *b == b'\n') {
                    let line: Vec<_> = self.0.drain(..=at).collect();
                    if let Some(code) = String::from_utf8_lossy(&line)
                        .trim()
                        .strip_prefix("Pairing code: ")
                    {
                        self.1.send(code.to_string()).unwrap();
                    }
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let paths = self.home.paths();
        let opts = xshelld::cli::Opts {
            relay: Some(relay.url()),
            name: Some("same name".into()),
            ..Default::default()
        };
        let pair = std::thread::spawn(move || {
            xshelld::pair::run_legacy_pair_test_control(
                &opts,
                &paths,
                &mut Lines(Vec::new(), tx),
                Duration::from_secs(30),
                &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
        });
        let code = rx.recv_timeout(WAIT).unwrap();
        approver.ring.pair_computer(&code, Arc::new(Quiet)).unwrap();
        assert!(pair.join().unwrap().is_ok());
        common::ring::identity(self.daemon.as_ref().unwrap()).0
    }
    pub(super) fn relay_host(&self, key: SignKey) -> Arc<HostHandle> {
        let id = relay_host_id(&key);
        wait("Relay Host available and usable", || {
            self.manager.host(&id).is_some_and(|h| h.status().usable())
        });
        self.manager.host(&id).unwrap()
    }
}
impl Drop for Computer {
    fn drop(&mut self) {
        self.ring.quit();
        self.manager.shutdown();
    }
}
pub(super) fn wait(what: &str, f: impl Fn() -> bool) {
    let until = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < until, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
pub(super) fn reply<T: Send + 'static>(
    send: impl FnOnce(Box<dyn FnOnce(Result<T, HostError>) + Send>),
) -> Result<T, HostError> {
    let (tx, rx) = mpsc::channel();
    send(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    rx.recv_timeout(WAIT).unwrap()
}
pub(super) fn call(host: &HostHandle, method: &str, params: Value) -> Value {
    reply(|w| host.call(method.into(), params, w)).unwrap()
}
#[derive(Default)]
pub(super) struct Sink(pub(super) Mutex<Vec<u8>>);
impl TermSink for Sink {
    fn data(&self, b: &[u8]) -> bool {
        self.0.lock().unwrap().extend_from_slice(b);
        true
    }
    fn exit(&self, _: i32, _: u64) {}
}

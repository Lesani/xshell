//! Real CLI admission contention; backend readiness/cancellation, not native consent.
use super::*;
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use AdoptionTestPoint as P;

#[derive(Default)]
struct GateState {
    armed: bool,
    release: bool,
    events: Vec<P>,
}
#[derive(Default)]
struct RegistrationGate {
    state: Mutex<GateState>,
    changed: Condvar,
}
impl RegistrationGate {
    fn hook(self: &Arc<Self>) -> AdoptionTestHook {
        let gate = self.clone();
        AdoptionTestHook(Arc::new(move |point| {
            let mut state = gate.state.lock().unwrap();
            if !state.armed {
                return;
            }
            // Record the first rejected CAS, not every poll of the same admission.
            if point != P::OriginJoinAdmissionBusy || !state.events.contains(&point) {
                state.events.push(point);
            }
            gate.changed.notify_all();
            if point == P::RegistrationAdmitted {
                let (state, timeout) = gate
                    .changed
                    .wait_timeout_while(state, Duration::from_secs(8), |s| !s.release)
                    .unwrap();
                assert!(
                    state.release && !timeout.timed_out(),
                    "registration gate deadline"
                );
            } else if point == P::OriginJoinBeforeAdmission {
                let (state, _) = gate
                    .changed
                    .wait_timeout_while(state, Duration::from_secs(3), |s| {
                        !s.events.contains(&P::RegistrationAdmitted)
                    })
                    .unwrap();
                assert!(state.events.contains(&P::RegistrationAdmitted));
            }
        }))
    }
    fn wait_for(&self, point: P) {
        let (state, _) = self
            .changed
            .wait_timeout_while(self.state.lock().unwrap(), Duration::from_secs(6), |s| {
                !s.events.contains(&point)
            })
            .unwrap();
        assert!(state.events.contains(&point), "missing event {point:?}");
    }
    fn release(&self) {
        self.state.lock().unwrap().release = true;
        self.changed.notify_all();
    }
    fn count(&self, point: P) -> usize {
        self.state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|p| **p == point)
            .count()
    }
    fn before(&self, first: P, second: P) {
        let state = self.state.lock().unwrap();
        let a = state.events.iter().position(|p| *p == first).unwrap();
        let b = state.events.iter().position(|p| *p == second).unwrap();
        assert!(a < b, "causal event order {first:?} -> {second:?}");
    }
}
// Release on every unwind, then join only test-owned registration work and stop the ring.
struct OwnedRegistration {
    gate: Arc<RegistrationGate>,
    worker: Option<JoinHandle<Result<xshell_protocol::ring::companion::CompanionProfile, String>>>,
    ring: Arc<DesktopRing>,
}
impl OwnedRegistration {
    fn release_and_join(
        &mut self,
    ) -> Result<xshell_protocol::ring::companion::CompanionProfile, String> {
        self.gate.release();
        self.worker.take().unwrap().join().unwrap()
    }
}
impl Drop for OwnedRegistration {
    fn drop(&mut self) {
        self.gate.release();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.ring.quit();
    }
}
#[derive(Default)]
struct Output {
    pending: bool,
    words: Vec<DeviceCode>,
    expired: bool,
    ended_without_installation: bool,
    cancelled: bool,
    owner_disconnected: bool,
    stdout_eof: bool,
    stderr_eof: bool,
    unknown: usize,
    io_errors: usize,
    stderr_lines: Vec<String>,
    started: Option<Instant>,
    events: Vec<(Duration, &'static str)>,
}
struct ObservedChild {
    child: Child,
    output: Arc<Mutex<Output>>,
    readers: Vec<JoinHandle<()>>,
}
impl ObservedChild {
    fn start(f: &Fixture, relay: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_xshelld"))
            .arg("pair")
            .args(["--relay", relay])
            .arg("--home")
            .arg(f.home.path())
            .arg("--socket")
            .arg(&f.paths.socket)
            .env("XSHELLD_PAIR_TTL_MS", "4000")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(Output {
            started: Some(Instant::now()),
            ..Output::default()
        }));
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let stdout_state = output.clone();
        let stdout = std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let mut state = stdout_state.lock().unwrap();
                let Ok(line) = line else {
                    state.io_errors += 1;
                    break;
                };
                let at = state.started.unwrap().elapsed();
                if line.starts_with("Computer pairing pending.") {
                    state.events.push((at, "pending"));
                    state.pending = true;
                } else if let Some(words) = line.strip_prefix("Device code: ") {
                    state.events.push((at, "words"));
                    if let Ok(code) = DeviceCode::parse(words) {
                        state.words.push(code);
                    } else {
                        state.unknown += 1;
                    }
                } else if line.starts_with("Pairing request expired.") {
                    state.events.push((at, "expired"));
                    state.expired = true;
                } else if line
                    != "In xshell, open Settings -> Hosts -> Ring and enter these eight words."
                    && !line.starts_with("One use; ")
                {
                    state.unknown += 1;
                }
            }
            stdout_state.lock().unwrap().stdout_eof = true;
        });
        let stderr_state = output.clone();
        let stderr = std::thread::spawn(move || {
            for line in BufReader::new(err).lines() {
                let mut state = stderr_state.lock().unwrap();
                let Ok(line) = line else {
                    state.io_errors += 1;
                    break;
                };
                state.stderr_lines.push(line.clone());
                match line.as_str() {
                    "xshelld: Computer pairing ended without installation" => state.ended_without_installation = true,
                    "xshelld: Pairing cancelled; possible transmitted consent requires reconciliation" => state.cancelled = true,
                    "xshelld: Computer pairing owner/epoch disconnected; the offer will not be reopened. If consent may have left, reconcile in xshell." => state.owner_disconnected = true,
                    _ => state.unknown += 1,
                }
            }
            stderr_state.lock().unwrap().stderr_eof = true;
        });
        Self {
            child,
            output,
            readers: vec![stdout, stderr],
        }
    }
    fn finish(&mut self) -> i32 {
        wait(|| self.child.try_wait().unwrap().is_some());
        let code = self.child.wait().unwrap().code().unwrap();
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
        let state = self.output.lock().unwrap();
        assert!(state.stdout_eof && state.stderr_eof);
        assert_eq!(state.io_errors, 0);
        assert_eq!(
            state.unknown, 0,
            "unclassified CLI output; stderr: {:?}",
            state.stderr_lines
        );
        code
    }
    /// How the CLI run went, for failure messages.
    fn report(&self) -> String {
        let s = self.output.lock().unwrap();
        format!(
            "elapsed {:?}; stdout events {:?}; stderr {:?}",
            s.started.unwrap().elapsed(),
            s.events,
            s.stderr_lines
        )
    }
    fn cancel(&self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) },
            0
        );
    }
}
impl Drop for ObservedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}
fn no_join_effects(f: &Fixture, ring: &DesktopRing, gate: &RegistrationGate) {
    let stored = Store::new(f.desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert!(
        stored.pending_join.is_none() && stored.journal.is_none() && stored.membership.is_none()
    );
    assert!(!f.paths.ring_dir.join("roster.json").exists());
    assert!(!f.paths.ring_dir.join("standalone-adoption.json").exists());
    assert!(ring.administration_joins().is_empty());
    for point in [
        P::OriginJoinAdmitted,
        P::JoinBeforeCounterpartStart,
        P::JoinWorkerStarted,
        P::JoinSlotReady,
        P::OriginControllerAttached,
    ] {
        assert_eq!(gate.count(point), 0, "unexpected event {point:?}");
    }
}
#[derive(Clone, Copy)]
enum Ending {
    ReadyThenCancel,
    CancelBusy,
    ExpireBusy,
    OwnerLossBusy,
}
fn forced_registration_contention(ending: Ending) {
    let relay = workerd::Relay::start();
    let f = Fixture::new();
    let gate = Arc::new(RegistrationGate::default());
    let ring = f.ring_with_hook(&relay.url(), Some(gate.hook()));
    let local = xshell_hostlink::LocalDaemon::new(xshell_hostlink::LocalDaemonConfig {
        bin: env!("CARGO_BIN_EXE_xshelld").into(),
        home: f.home.path().into(),
        socket: f.paths.socket.clone(),
        env: vec![],
        version: "1.5.0".into(),
        build: xshelld::build_id::BUILD.clone(),
    })
    .unwrap();
    let profile = local
        .with_registration_endpoint(&f.desktop, |client| {
            let profile = ring.register_companion(client)?;
            assert!(client.request(CompanionRequest::PairDriverReady).is_err());
            Ok(profile)
        })
        .unwrap();
    let _manager = f.manager(&ring);
    let mut child = ObservedChild::start(&f, &relay.url());
    wait(|| child.output.lock().unwrap().pending);
    let original_start = unix_now();
    assert!(!f.paths.socket.exists());
    gate.state.lock().unwrap().armed = true;
    let _daemon = f.server();
    let registered_ring = ring.clone();
    let client = f.companion();
    let mut owned = OwnedRegistration {
        gate: gate.clone(),
        ring: ring.clone(),
        worker: Some(std::thread::spawn(move || {
            registered_ring.register_companion(&client)
        })),
    };
    gate.wait_for(P::RegistrationAdmitted);
    let companion_before = std::fs::read(f.paths.ring_dir.join("companion.json")).unwrap();
    gate.wait_for(P::OriginJoinAdmissionBusy);
    gate.before(P::RegistrationAdmitted, P::OriginJoinAdmissionBusy);
    no_join_effects(&f, &ring, &gate);
    assert!(std::fs::read(f.paths.ring_dir.join("companion.json")).unwrap() == companion_before);
    assert!(child.output.lock().unwrap().words.is_empty());
    match ending {
        Ending::ReadyThenCancel => {
            // Release only AFTER a real failed CAS, never wait for acquisition while held.
            assert!(owned.release_and_join().is_ok());
            wait(|| {
                !child.output.lock().unwrap().words.is_empty()
                    || child.child.try_wait().unwrap().is_some()
            });
            if child.output.lock().unwrap().words.is_empty() {
                // RED must prove the actual terminal chain before its positive words assertion.
                let exit = child.finish();
                assert_eq!(exit, 1, "CLI exit; {}", child.report());
                gate.wait_for(P::OriginDispatchEnded);
                let output = child.output.lock().unwrap();
                assert!(
                    output.ended_without_installation && output.stdout_eof && output.stderr_eof
                );
                assert!(!output.expired && !output.cancelled && !output.owner_disconnected);
                assert!(output.words.is_empty());
                gate.before(P::OriginJoinAdmissionBusy, P::OriginDispatchEnded);
                no_join_effects(&f, &ring, &gate);
                panic!("positive readiness requirement: actual CLI exited1 before words after real CAS contention");
            }
            let join = controller(&ring);
            gate.wait_for(P::OriginControllerAttached);
            gate.wait_for(P::JoinSlotReady);
            assert!(join.is_ready());
            assert!(join.code.format() == child.output.lock().unwrap().words[0].format());
            let pending = Store::new(f.desktop.clone())
                .load_profile()
                .unwrap()
                .0
                .unwrap()
                .pending_join
                .unwrap();
            assert_eq!(pending.intent.operation, join.operation);
            assert!(pending.intent.target == profile.target());
            assert_eq!(pending.intent.expires_at, join.expires_at);
            assert_eq!(pending.relay_url, relay.url());
            assert!(
                join.expires_at <= original_start + 4,
                "original expiry was renewed"
            );
            assert!(join.expires_at <= unix_now() + 4);
            gate.before(P::OriginJoinAdmissionBusy, P::OriginJoinAdmitted);
            gate.before(P::OriginJoinAdmitted, P::JoinBeforeCounterpartStart);
            gate.before(P::JoinBeforeCounterpartStart, P::JoinWorkerStarted);
            gate.before(P::JoinWorkerStarted, P::JoinSlotReady);
            for point in [
                P::OriginJoinBeforeAdmission,
                P::OriginJoinAdmitted,
                P::JoinBeforeCounterpartStart,
                P::JoinWorkerStarted,
                P::JoinSlotReady,
                P::OriginControllerAttached,
            ] {
                assert_eq!(gate.count(point), 1, "one original driver: {point:?}");
            }
            child.cancel();
            assert_eq!(child.finish(), 1);
            assert!(child.output.lock().unwrap().cancelled);
            assert_eq!(child.output.lock().unwrap().words.len(), 1);
            gate.wait_for(P::OriginDispatchEnded);
            assert_no_words_on_disk(&f, &join.code);
        }
        Ending::CancelBusy | Ending::ExpireBusy | Ending::OwnerLossBusy => {
            match ending {
                Ending::CancelBusy => child.cancel(),
                Ending::OwnerLossBusy => ring.quit(),
                Ending::ExpireBusy => {}
                Ending::ReadyThenCancel => unreachable!(),
            }
            assert_eq!(
                child.finish(),
                if matches!(ending, Ending::ExpireBusy) {
                    5
                } else {
                    1
                }
            );
            gate.wait_for(P::OriginDispatchEnded);
            let output = child.output.lock().unwrap();
            assert!(output.words.is_empty() && !output.ended_without_installation);
            assert!(match ending {
                Ending::CancelBusy => output.cancelled,
                Ending::ExpireBusy => output.expired,
                Ending::OwnerLossBusy => output.owner_disconnected,
                Ending::ReadyThenCancel => unreachable!(),
            });
            drop(output);
            // Dispatch is terminal before release: draining registration cannot rearm it.
            let registration = owned.release_and_join();
            if matches!(ending, Ending::OwnerLossBusy) {
                assert!(
                    matches!(registration, Err(error) if error == "another process owns the Desktop counterpart")
                );
            } else {
                assert!(registration.is_ok());
            }
            no_join_effects(&f, &ring, &gate);
            assert_eq!(gate.count(P::OriginJoinBeforeAdmission), 1);
            assert_eq!(gate.count(P::OriginDispatchEnded), 1);
        }
    }
    let output = child.output.lock().unwrap();
    eprintln!(
        "admission contention events={:?} words={} stdout_eof={} stderr_eof={}",
        gate.state.lock().unwrap().events,
        output.words.len(),
        output.stdout_eof,
        output.stderr_eof
    );
}
#[test]
fn registered_origin_real_cas_contention_keeps_original_offer_and_reaches_ack_once() {
    forced_registration_contention(Ending::ReadyThenCancel);
}
#[test]
fn registered_origin_cancel_while_admission_busy_cannot_start_late() {
    forced_registration_contention(Ending::CancelBusy);
}
#[test]
fn registered_origin_original_expiry_while_admission_busy_cannot_start_late() {
    forced_registration_contention(Ending::ExpireBusy);
}
#[test]
fn registered_origin_owner_epoch_loss_while_admission_busy_cannot_start_late() {
    forced_registration_contention(Ending::OwnerLossBusy);
}
#[test]
fn public_injected_join_keeps_immediate_busy_during_registration() {
    let f = Fixture::new();
    let _daemon = f.server();
    let gate = Arc::new(RegistrationGate::default());
    let ring = f.ring_with_hook("ws://127.0.0.1:9", Some(gate.hook()));
    ring.register_companion(&f.companion()).unwrap();
    gate.state.lock().unwrap().armed = true;
    let registered_ring = ring.clone();
    let client = f.companion();
    let mut owned = OwnedRegistration {
        gate: gate.clone(),
        ring: ring.clone(),
        worker: Some(std::thread::spawn(move || {
            registered_ring.register_companion(&client)
        })),
    };
    gate.wait_for(P::RegistrationAdmitted);
    let (tx, rx) = mpsc::channel();
    let public_ring = ring.clone();
    let client = f.companion();
    let public = std::thread::spawn(move || {
        let result = public_ring.begin_join_with_offer(
            &client,
            xshell_hostlink::ring::adoption::JoinOfferInput {
                operation: xshell_protocol::ring::relay::wire::new_nonce().unwrap(),
                code: DeviceCode::generate().unwrap(),
                relay_url: "ws://127.0.0.1:9".into(),
                expires_at: unix_now() + 4,
            },
            Instant::now() + Duration::from_secs(4),
        );
        let immediate = public_ring.begin_join(&client);
        tx.send((result.err(), immediate.err())).unwrap();
    });
    let (injected, desktop) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        injected.as_deref(),
        Some("another adoption operation is active")
    );
    assert_eq!(
        desktop.as_deref(),
        Some("another adoption operation is active")
    );
    public.join().unwrap();
    no_join_effects(&f, &ring, &gate);
    assert!(owned.release_and_join().is_ok());
}

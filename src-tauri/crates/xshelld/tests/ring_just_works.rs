#![cfg(unix)]
//! Ring just works (S2), end to end on production workerd with real Desktops and Daemons:
//! Join without confirmation, Remove of any unit (this computer included), removal
//! detected by the removed machine itself, Join again with a new identity, format-1 state
//! set aside, a standalone Daemon that joins as a Computer in one approval, and the
//! Mobile's approve/remove library (`relay::approve`, `relay::remove`) with its resume.
mod common;
#[path = "desktop/fault_proxy.rs"]
mod fault_proxy;
#[path = "desktop/relay_fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "desktop/workerd.rs"]
mod workerd;
use fault_proxy::{Fault, FaultProxy};
use fixture::*;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use workerd::Relay;
use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision, MembershipState};
use xshell_hostlink::ring::store::Store;
use xshell_hostlink::ring::{PairingEvent, PairingFlow, PairingObserver};
use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
use xshell_protocol::ring::membership::EnrollmentKind;
use xshell_protocol::ring::pairing::adoption::JoinTarget;
use xshell_protocol::ring::pairing::PairingOffer;
use xshell_protocol::ring::relay::approve::{
    approve_device_code, prepare_approval, publish_approval, resume_approval, ApproveError, Resume,
};
use xshell_protocol::ring::relay::pair::{
    adopt_mobile_as_guest_with_ready, MobileAdoptionRequest, PairOptions,
};
use xshell_protocol::ring::relay::remove::{
    prepare_removal, publish_removal, remove_enrollment, resume_removal, RemoveError,
};
use xshell_protocol::ring::relay::{Connector, ConnectorConfig, ConnectorEvents, LinkState};
use xshell_protocol::ring::relay::{RingClientConfig, RingTimeouts};
use xshell_protocol::ring::{DeviceKeys, RosterChain, SignKey};

struct Quiet;
impl PairingObserver for Quiet {
    fn pairing(&self, _: PairingFlow, _: &PairingEvent) {}
}
struct NoEvents;
impl ConnectorEvents for NoEvents {}

fn connected(c: &Computer) {
    wait("Desktop connected to workerd", || {
        c.ring.view().connection == "connected"
    });
}

fn profile(c: &Computer) -> xshell_hostlink::ring::store::ProfileState {
    Store::new(c.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
}

fn files(dir: &std::path::Path, prefix: &str) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .count()
}

/// The Daemon's durable chain (`roster.json`), if it holds one.
fn daemon_chain(c: &Computer) -> Option<RosterChain> {
    let bytes = std::fs::read(c.home.paths().ring_dir.join("roster.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let tokens: Vec<String> = serde_json::from_value(v["rosters"].clone()).unwrap();
    Some(RosterChain::from_tokens(&tokens).unwrap())
}

/// A phone in `c`'s Ring (through its v2 QR offer), with its own running Connector.
fn phone(c: &Computer, n: u8) -> (Arc<DeviceKeys>, Arc<Connector>) {
    phone_dialing(c, n, None)
}

/// [`phone`] whose Connector dials `dial` (a fault proxy in front of the Relay) instead of
/// the Relay its head names.
fn phone_dialing(c: &Computer, n: u8, dial: Option<String>) -> (Arc<DeviceKeys>, Arc<Connector>) {
    connected(c);
    let keys = Arc::new(DeviceKeys::from_seeds(&[n; 32], &[n.wrapping_add(100); 32]));
    let shown = c.ring.pair_phone(Arc::new(Quiet)).unwrap();
    let offer = PairingOffer::parse(&shown.payload).unwrap();
    let enrolled = adopt_mobile_as_guest_with_ready(
        &MobileAdoptionRequest {
            offer: &offer,
            keys: keys.clone(),
            name: "my phone",
            until: Instant::now() + Duration::from_secs(60),
        },
        &PairOptions::default(),
        None,
        || Ok(()),
        |_| Ok(true),
    )
    .unwrap();
    let mut client = RingClientConfig::new(enrolled.chain, keys.clone());
    client.dial = dial;
    client.timeouts = RingTimeouts {
        connect: Duration::from_secs(5),
        ..RingTimeouts::default()
    };
    let mut config = ConnectorConfig::new(client);
    config.backoff_unit = Duration::from_millis(50);
    let connector = Arc::new(Connector::start(config, Arc::new(NoEvents)).unwrap());
    wait("phone connected", || {
        matches!(connector.state(), LinkState::Connected { .. })
    });
    (keys, connector)
}

fn wait_removed(c: &Computer) {
    let until = Instant::now() + Duration::from_secs(20);
    while c.ring.membership_state().unwrap() != MembershipState::Removed || c.ring.chain().is_some()
    {
        assert!(
            Instant::now() < until,
            "the Desktop never detected its removal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_daemon_set_aside(c: &Computer) {
    let dir = c.home.paths().ring_dir;
    wait("the Daemon set its membership aside", || {
        !dir.join("roster.json").exists() && files(&dir, "roster.json.removed-") > 0
    });
}

/// The Desktop registers its current identity with the local Daemon the way the app's
/// registration loop does (after a set-aside it is a new identity).
fn reregister(c: &Computer) {
    wait("registration needed after the set-aside", || {
        c.ring.companion_registration_needed()
    });
    c.ring.register_companion(&c.companion()).unwrap();
    assert!(!c.ring.companion_registration_needed());
}

/// Join `guest` with words approved by `approve` (no confirmation on the guest).
fn join_with(
    guest: &Computer,
    approve: impl FnOnce(&xshell_protocol::ring::device_code::DeviceCode) + Send,
) -> RosterChain {
    let offer = guest.ring.begin_join(&guest.companion()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let approval = scope.spawn(move || approve(&code));
        let outcome = offer.wait().unwrap();
        approval.join().unwrap();
        match outcome {
            AdoptionOutcome::Installed { chain, .. } => chain,
            _ => panic!("Join not installed"),
        }
    })
}

#[test]
fn gui_join_installs_without_confirmation_and_two_approvers_get_one_winner() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let b = Computer::new(&relay);
    b.join(&a);
    let c = Computer::new(&relay);
    let offer = c.ring.begin_join(&c.companion()).unwrap();
    let code = offer.code.clone();
    // Two devices type the same words: one enrolls, the other is refused cleanly.
    let (first, second) = std::thread::scope(|scope| {
        let x = scope.spawn(|| a.ring.approve_device_code(&code));
        let y = scope.spawn(|| b.ring.approve_device_code(&code));
        let outcome = offer.wait().unwrap();
        assert!(matches!(outcome, AdoptionOutcome::Installed { .. }));
        (x.join().unwrap(), y.join().unwrap())
    });
    assert!(first.is_ok() != second.is_ok(), "{first:?} / {second:?}");
    let refused = first.err().or(second.err()).unwrap();
    assert!(
        ["already_used:", "not_found:", "closed:"]
            .iter()
            .any(|p| refused.starts_with(p)),
        "{refused}"
    );
    // Both of the joined computer's stores are installed, with no confirmation step.
    let chain = c.ring.chain().unwrap();
    assert_eq!(c.ring.membership_state().unwrap(), MembershipState::Member);
    assert_eq!(daemon_chain(&c).unwrap().head(), chain.head());
    assert_eq!(chain.head().version(), 3);
    // B5: the Ring never expires members.
    let policy = chain.head().roster().membership().unwrap().policy;
    assert_eq!(policy.inactivity_seconds, None);
    // The spent words never enroll again.
    assert!(a.ring.approve_device_code(&code).is_err());
}

#[test]
fn remove_self_cleans_both_stores_terminals_survive() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let b = Computer::new(&relay);
    b.join(&a);
    connected(&b);
    let before = profile(&b).keys.sign_key();
    let daemon_keys_before = std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap();
    // A Terminal on b's Daemon keeps running through the removal.
    let project = b.home.project("rjw-terminal");
    let mut local = common::Client::connect(&b.daemon.as_ref().unwrap().socket);
    local.hello(common::range(1, 1));
    let terminal = uuid::Uuid::new_v4();
    let opened = local.open(terminal, common::sh_spec(&project));
    let pid = opened["pid"].as_u64().unwrap() as i32;
    let chain = b.ring.remove_member(&before).unwrap();
    assert!(chain.authorized_member(&before).is_none());
    // The Desktop: in no Ring, new identity, no notice (it removed itself).
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::None);
    assert!(b.ring.chain().is_none());
    let after = profile(&b);
    assert_ne!(after.keys.sign_key(), before);
    assert!(after.removed.is_none() && after.companion.is_none());
    // The Daemon: membership and identity set aside too (told by the Desktop).
    wait_daemon_set_aside(&b);
    assert_ne!(
        std::fs::read(b.home.paths().ring_dir.join("keys.json")).unwrap_or_default(),
        daemon_keys_before
    );
    assert!(common::alive(pid), "the Terminal keeps running");
    // The rest of the Ring sees b gone.
    wait("a saw the removal", || {
        a.ring
            .chain()
            .is_some_and(|c| c.head().member(&before).is_none())
    });
}

#[test]
fn removed_desktop_and_daemon_detect_removal_and_join_again() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let b = Computer::new(&relay);
    b.join(&a);
    connected(&b);
    let old = profile(&b).keys.sign_key();
    connected(&a);
    a.ring.remove_member(&old).unwrap();
    // Both of b's sides learn it by themselves from the signed history.
    wait_removed(&b);
    let notice = b.ring.notices().unwrap().0.expect("removal notice");
    assert_eq!(&notice.ring_id, a.ring.chain().unwrap().ring_id());
    wait_daemon_set_aside(&b);
    // Join again in the same Desktop process: the new identity registers with the Daemon
    // (which rotated its own) and joins; the notice clears.
    reregister(&b);
    let chain = join_with(&b, |code| {
        a.ring.approve_device_code(code).unwrap();
    });
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::Member);
    assert!(b.ring.notices().unwrap().0.is_none(), "the notice clears");
    assert!(chain
        .authorized_member(&profile(&b).keys.sign_key())
        .is_some());
    assert!(chain.authorized_member(&old).is_none());
}

#[test]
fn removal_then_create_in_the_same_process_gets_a_new_ring() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    let first = a.create();
    connected(&a);
    let me = profile(&a).keys.sign_key();
    a.ring.remove_member(&me).unwrap();
    wait_daemon_set_aside(&a);
    reregister(&a);
    let second = a.create();
    assert_ne!(
        second.ring_id(),
        first.ring_id(),
        "a new creator key, a new Ring"
    );
}

#[test]
fn offline_removed_daemon_detects_on_restart() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let mut b = Computer::new(&relay);
    b.join(&a);
    let daemon_key = daemon_chain(&b)
        .unwrap()
        .head()
        .roster()
        .members
        .iter()
        .find(|m| m.role == xshell_protocol::ring::Role::Daemon && m.name != "same name")
        .map(|m| m.sign_key);
    let _ = daemon_key;
    // b's Desktop goes away for good (as if closed); its Daemon stops.
    b.ring.quit();
    b.daemon.take().unwrap().shutdown();
    connected(&a);
    let unit_key = profile(&b).keys.sign_key();
    a.ring.remove_member(&unit_key).unwrap();
    // The Daemon starts again: its stored chain is stale (it still lists it), the Relay
    // refuses it, and it verifies the removal by itself.
    let mut cfg = xshelld::server::Config::new(b.home.home(), b.home.paths());
    cfg.ring_backoff_unit = Duration::from_millis(10);
    b.daemon = Some(xshelld::server::Server::start(cfg).unwrap());
    wait_daemon_set_aside(&b);
    assert!(daemon_chain(&b).is_none());
}

#[test]
fn v1_stores_set_aside_and_create_gets_new_ring_id() {
    let relay = Relay::start();
    let dir = tempfile::tempdir().unwrap();
    let desktop = dir.path().join("desktop");
    // A format-1 Ring stored by an earlier build.
    let keys = {
        let store = Store::new(desktop.clone());
        let keys = store.retained_keys().unwrap();
        let g = xshell_protocol::ring::SignedRoster::genesis(
            &*keys,
            keys.noise_key(),
            "old",
            &relay.url(),
            1,
        )
        .unwrap();
        store
            .transact(|_, _| {
                Ok((
                    Some(xshell_hostlink::ring::store::RingState {
                        keys: keys.clone(),
                        chain: RosterChain::from_chain(vec![g]).unwrap(),
                        local_member: None,
                        farewells: vec![],
                        held_relays: Default::default(),
                        host_members: Default::default(),
                    }),
                    (),
                ))
            })
            .unwrap();
        keys
    };
    let old_ring = xshell_protocol::ring::RingId::derive(&keys.sign_key());
    let mut cfg = xshell_hostlink::ring::DesktopRingConfig::new(desktop.clone(), "d".into());
    cfg.default_relay_url = relay.url();
    struct Obs;
    impl xshell_hostlink::ring::RingObserver for Obs {
        fn changed(&self, _: &xshell_hostlink::ring::RingView) {}
    }
    let ring = xshell_hostlink::ring::DesktopRing::open(cfg, Arc::new(Obs));
    assert!(ring.chain().is_none(), "set aside at open");
    assert_eq!(ring.membership_state().unwrap(), MembershipState::None);
    let (removed, old) = ring.notices().unwrap();
    assert!(removed.is_none() && old.is_some(), "the old-Ring notice");
    assert_eq!(files(&desktop, "ring.json.v1-"), 1);
    let stored = Store::new(desktop.clone())
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert_ne!(stored.keys.sign_key(), keys.sign_key());
    assert_ne!(
        xshell_protocol::ring::RingId::derive(&stored.keys.sign_key()),
        old_ring,
        "Create derives a new Ring id"
    );
    ring.quit();
}

#[test]
fn format1_entry_points_refused() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let client = a.companion();
    let intent = xshell_protocol::ring::pairing::adoption::AdoptionIntent {
        operation: xshell_protocol::ring::b64::encode(&[5; 32]),
        expires_at: 4_000_000_000,
        target: profile(&a).companion.unwrap().target(),
    };
    for request in [
        CompanionRequest::CompletionStart {
            intent: intent.clone(),
        },
        CompanionRequest::RetainedCompletionStart {
            intent,
            predecessor: vec![],
        },
    ] {
        let error = client.request(request).unwrap_err();
        assert!(error.contains("unsupported: format 1"), "{error}");
    }
}

#[test]
fn mobile_identity_approves_computer_code_and_removes_it() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let (phone_keys, connector) = phone(&a, 31);
    let b = Computer::new(&relay);
    let chain = join_with(&b, |code| {
        let head = connector.chain().head().version();
        let prepared = prepare_approval(
            connector.clone(),
            phone_keys.clone(),
            code,
            &PairOptions::default(),
            None,
        )
        .unwrap();
        // The record the app stores before anything is published.
        let op = prepared.op().clone();
        assert_eq!(op.predecessor_version, head);
        assert!(op.replaced.is_none());
        assert_eq!(
            connector.chain().head().version(),
            head,
            "prepare publishes nothing"
        );
        let device = publish_approval(prepared, &mut |_| Ok(())).unwrap();
        assert!(matches!(resume_approval(&connector, &op), Resume::Done(_)));
        assert_eq!(device.kind, EnrollmentKind::Computer);
        assert_eq!(device.names.len(), 2);
        assert!(device.replaced.is_none());
    });
    let me = profile(&b).keys.sign_key();
    let unit = chain
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit_for(&me)
        .unwrap()
        .clone();
    assert_eq!(
        chain
            .get(chain.head().version())
            .unwrap()
            .roster()
            .signed_by,
        phone_keys.sign_key(),
        "the phone signed the Enroll"
    );
    // A Daemon is never an approver.
    let words = xshell_protocol::ring::device_code::DeviceCode::generate().unwrap();
    wait("phone has the new head", || {
        connector.chain().head().version() == chain.head().version()
    });
    // The phone removes the computer: prepared (nothing sent), published, resumed.
    let op = prepare_removal(&connector, &*phone_keys, &unit.id).unwrap();
    assert_eq!(
        connector.chain().head().version(),
        chain.head().version(),
        "prepare publishes nothing"
    );
    // The app died after storing the record, before sending anything: the head is still
    // the record's predecessor, so resume sends the identical stored candidate (P1-4).
    let removed = match resume_removal(&connector, &op) {
        Resume::Done(chain) => chain,
        other => panic!("{other:?}"),
    };
    assert!(removed.authorized_member(&me).is_none());
    assert_eq!(
        removed.get(op.candidate().unwrap().version()),
        Some(&op.candidate().unwrap()),
        "exactly the stored candidate"
    );
    // A lost answer after it landed: resume reads the chain and finds it done; a second
    // publication of the same bytes is harmless.
    assert!(matches!(resume_removal(&connector, &op), Resume::Done(_)));
    publish_removal(
        &connector,
        &*phone_keys,
        &op,
        Instant::now() + WAIT,
        &mut |_| Ok(()),
    )
    .unwrap();
    // Already gone: done, not an error.
    assert_eq!(
        prepare_removal(&connector, &*phone_keys, &unit.id).unwrap_err(),
        RemoveError::NotInRing
    );
    remove_enrollment(&connector, &*phone_keys, &unit.id, Instant::now() + WAIT).unwrap();
    wait_removed(&b);
    // Not found: nobody shows these words.
    assert_eq!(
        approve_device_code(
            &connector,
            phone_keys.clone(),
            &words,
            &PairOptions::default(),
            None
        )
        .unwrap_err(),
        ApproveError::NotFound
    );
    connector.stop(xshell_protocol::ring::relay::ByeReason::quit());
}

/// `xshelld pair` on a machine with no Desktop: a standalone Daemon member.
fn standalone(d: &xshelld::server::ServerHandle, approver: &Computer, url: &str) -> SignKey {
    let (tx, rx) = mpsc::channel();
    let chain = std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            d.adopt_standalone(
                url,
                Duration::from_secs(30),
                &Arc::new(AtomicBool::new(false)),
                |code| tx.send(code.clone()).unwrap(),
                |p, lease| Ok(Some(lease.confirm(&p.proposal_hash)?)),
            )
        });
        let code = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        approver.ring.approve_device_code(&code).unwrap();
        guest.join().unwrap().unwrap()
    });
    chain
        .head()
        .roster()
        .membership()
        .unwrap()
        .enrollments
        .last()
        .unwrap()
        .members[0]
}

#[test]
fn standalone_daemon_becomes_computer_in_one_join_terminals_survive() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    connected(&a);
    // b's Daemon pairs headless first; then b's Desktop joins.
    let b = Computer::new(&relay);
    let daemon_key = standalone(b.daemon.as_ref().unwrap(), &a, &relay.url());
    let project = b.home.project("rjw-standalone-terminal");
    let mut local = common::Client::connect(&b.daemon.as_ref().unwrap().socket);
    local.hello(common::range(1, 1));
    let opened = local.open(uuid::Uuid::new_v4(), common::sh_spec(&project));
    let pid = opened["pid"].as_u64().unwrap() as i32;
    b.ring.register_companion(&b.companion()).unwrap();
    let chain = join_with(&b, |code| {
        a.ring.approve_device_code(code).unwrap();
    });
    let m = chain.head().roster().membership().unwrap();
    let unit = m.unit_for(&daemon_key).unwrap();
    assert_eq!(unit.kind, EnrollmentKind::Computer, "one Computer unit now");
    assert!(unit.members.contains(&profile(&b).keys.sign_key()));
    assert_eq!(
        m.enrollments
            .iter()
            .filter(|u| u.members.contains(&daemon_key))
            .count(),
        1
    );
    assert!(common::alive(pid), "the headless Terminal survived");
    assert_eq!(daemon_chain(&b).unwrap().head(), chain.head());
}

#[test]
fn mobile_approval_dies_after_standalone_removal_resume_says_not_done() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let (phone_keys, connector) = phone(&a, 32);
    let b = Computer::with_ring_config(&relay, |c| c.pair_ttl = Duration::from_secs(6));
    standalone(b.daemon.as_ref().unwrap(), &a, &relay.url());
    wait("phone has the standalone head", || {
        connector.chain().head().version() == a.ring.chain().unwrap().head().version()
    });
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b.ring.begin_join(&b.companion()).unwrap();
    let code = offer.code.clone();
    let op = std::thread::scope(|scope| {
        let joining = scope.spawn(|| offer.wait());
        let prepared = prepare_approval(
            connector.clone(),
            phone_keys.clone(),
            &code,
            &PairOptions::default(),
            None,
        )
        .unwrap();
        let op = prepared.op().clone();
        let stored = serde_json::to_string(&op).unwrap();
        assert!(
            !stored.contains(code.format().as_str()),
            "no words in the record"
        );
        assert!(op.replaced.is_some(), "a standalone unit to replace");
        assert!(matches!(op.target, JoinTarget::Computer { .. }));
        // Stage 1 only: the standalone removal is published, then the app dies before
        // the Enroll (the prepared approval is dropped).
        publish_removal(
            &connector,
            &*phone_keys,
            op.replaced.as_ref().unwrap(),
            Instant::now() + WAIT,
            &mut |_| Ok(()),
        )
        .unwrap();
        drop(prepared);
        assert!(
            joining.join().unwrap().is_err(),
            "the joining machine was never enrolled"
        );
        op
    });
    // Absent while the code is unexpired: a publication could still land (P1-4).
    if (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs())
        < op.expires_at
    {
        assert!(matches!(resume_approval(&connector, &op), Resume::Retry));
    }
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        < op.expires_at
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    match resume_approval(&connector, &op) {
        Resume::NotDone { standalone_removed } => assert!(standalone_removed),
        other => panic!("{other:?}"),
    }
    // The machine's Daemon returns to "not in a Ring" by itself; a new approval of fresh
    // words then enrolls the Computer.
    wait_daemon_set_aside(&b);
    reregister(&b);
    let chain = join_with(&b, |code| {
        let device = approve_device_code(
            &connector,
            phone_keys.clone(),
            code,
            &PairOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            device.replaced.is_none(),
            "nothing standalone left to replace"
        );
    });
    assert!(chain
        .authorized_member(&profile(&b).keys.sign_key())
        .is_some());
    connector.stop(xshell_protocol::ring::relay::ByeReason::quit());
}

#[test]
fn removed_approver_cannot_publish() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let b = Computer::new(&relay);
    b.join(&a);
    connected(&a);
    let b_key = profile(&b).keys.sign_key();
    a.ring.remove_member(&b_key).unwrap();
    wait_removed(&b);
    let c = Computer::new(&relay);
    let offer = c.ring.begin_join(&c.companion()).unwrap();
    let error = b.ring.approve_device_code(&offer.code).unwrap_err();
    assert!(error.starts_with("not_approver:"), "{error}");
    offer.cancel_join();
    assert!(offer.wait().is_err());
    let _ = CompanionResponse::Left;
    let _ = CreateDecision::Decline;
}

#[test]
fn transitional_stores_on_both_sides_are_set_aside_and_create_works() {
    use xshell_protocol::ring::membership::{Enrollment, Membership, RingPolicy, RosterChange};
    use xshell_protocol::ring::{Member, Role, SignedRoster};
    let relay = Relay::start();
    let mut c = Computer::new(&relay);
    let desk = Store::new(c.home.root().join("desktop"))
        .retained_keys()
        .unwrap();
    let daemon = match c.companion().request(CompanionRequest::Descriptor).unwrap() {
        CompanionResponse::Identity { identity, .. } => identity,
        _ => panic!("no identity"),
    };
    let old_daemon_key = daemon.sign_key;
    // A format-2 Ring that still carries a format-1 shape: an Upgrade without the local
    // completion (a legacy Desktop unit next to a standalone Daemon unit).
    let g = SignedRoster::genesis(&*desk, desk.noise_key(), "old", &relay.url(), 100).unwrap();
    let v1 = g
        .next(&*desk, 101, |r| {
            r.add(Member::new(
                "host",
                Role::Daemon,
                daemon.sign_key,
                daemon.noise_key,
                101,
            ));
        })
        .unwrap();
    let mut r = v1.roster().clone();
    r.v = 2;
    r.version = 3;
    r.prev = Some(v1.hash());
    r.issued_at = 102;
    let unit = |n: u8, kind, key| Enrollment {
        id: xshell_protocol::ring::b64::encode(&[n; 32]),
        kind,
        members: vec![key],
        enrolled_at: 102,
    };
    r.set_membership(&Membership {
        enrollments: vec![
            unit(1, EnrollmentKind::LegacyDesktop, desk.sign_key()),
            unit(2, EnrollmentKind::Daemon, daemon.sign_key),
        ],
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: None,
            activity_key: desk.sign_key(),
        },
        change: RosterChange::Upgrade { proofs: vec![] },
    });
    let up = r.sign(&*desk).unwrap();
    let chain = RosterChain::from_chain(vec![g, v1, up]).unwrap();
    assert!(xshell_protocol::ring::membership::unsupported_stored_membership(chain.head()));
    // Both sides store it; then both start again.
    c.ring.quit();
    c.daemon.take().unwrap().shutdown();
    Store::new(c.home.root().join("desktop"))
        .transact(|_, _| {
            Ok((
                Some(xshell_hostlink::ring::store::RingState {
                    keys: desk.clone(),
                    chain: chain.clone(),
                    local_member: Some(daemon.sign_key),
                    farewells: vec![],
                    held_relays: Default::default(),
                    host_members: Default::default(),
                }),
                (),
            ))
        })
        .unwrap();
    let tokens: Vec<String> = chain
        .versions()
        .iter()
        .map(|r| r.token().to_string())
        .collect();
    let ring_dir = c.home.paths().ring_dir;
    std::fs::write(
        ring_dir.join("roster.json"),
        serde_json::to_vec(&serde_json::json!({"v": 1, "rosters": tokens})).unwrap(),
    )
    .unwrap();
    let mut cfg = xshelld::server::Config::new(c.home.home(), c.home.paths());
    cfg.ring_backoff_unit = Duration::from_millis(10);
    c.daemon = Some(xshelld::server::Server::start(cfg).unwrap());
    assert!(
        !ring_dir.join("roster.json").exists(),
        "the Daemon set it aside"
    );
    assert!(files(&ring_dir, "roster.json.v1-") == 1);
    let mut rc = xshell_hostlink::ring::DesktopRingConfig::new(
        c.home.root().join("desktop"),
        "same name".into(),
    );
    rc.default_relay_url = relay.url();
    c.ring = xshell_hostlink::ring::DesktopRing::open(rc, Arc::new(fixture::Quiet));
    assert!(c.ring.chain().is_none(), "the Desktop set it aside");
    assert!(c.ring.notices().unwrap().1.is_some(), "old-Ring notice");
    // A new Desktop identity registers with the Daemon's new identity, and Create works.
    c.ring.register_companion(&c.companion()).unwrap();
    let created = c.create();
    let new_daemon = profile(&c).companion.unwrap().daemon.sign_key;
    assert_ne!(new_daemon, old_daemon_key, "the Daemon rotated too");
    assert!(created.authorized_member(&new_daemon).is_some());
}

#[test]
fn approver_removed_between_consent_and_publication_cannot_publish() {
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    // b approves, paused after the joining keys' proofs arrived, before publication.
    let gate = Arc::new((
        std::sync::Mutex::new((false, false)),
        std::sync::Condvar::new(),
    ));
    let hook_gate = gate.clone();
    let b = Computer::with_ring_config(&relay, move |c| {
        c.adoption_test_hook = Some(AdoptionTestHook(Arc::new(move |p| {
            if p == AdoptionTestPoint::ApprovalProofsReceived {
                let (lock, cv) = &*hook_gate;
                let mut st = lock.lock().unwrap();
                st.0 = true;
                cv.notify_all();
                while !st.1 {
                    st = cv.wait(st).unwrap();
                }
            }
        })));
    });
    b.join(&a);
    connected(&b);
    connected(&a);
    let b_key = profile(&b).keys.sign_key();
    let c = Computer::new(&relay);
    let offer = c.ring.begin_join(&c.companion()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let approval = scope.spawn(|| b.ring.approve_device_code(&code));
        {
            let (lock, cv) = &*gate;
            let mut st = lock.lock().unwrap();
            while !st.0 {
                st = cv.wait_timeout(st, Duration::from_millis(50)).unwrap().0;
            }
        }
        // Meanwhile another device removes the approver.
        a.ring.remove_member(&b_key).unwrap();
        {
            let (lock, cv) = &*gate;
            lock.lock().unwrap().1 = true;
            cv.notify_all();
        }
        assert!(
            approval.join().unwrap().is_err(),
            "a removed approver cannot publish"
        );
        assert!(offer.wait().is_err(), "the joining machine is not enrolled");
    });
    let head = a.ring.chain().unwrap();
    assert!(head
        .authorized_member(&profile(&c).keys.sign_key())
        .is_none());
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `c` holds a head at `version` or later (the Relay stored what led there).
fn wait_head(c: &Computer, version: u64) {
    wait("the Relay stored the new head", || {
        c.ring
            .chain()
            .is_some_and(|chain| chain.head().version() >= version)
    });
}

fn phone_connected(connector: &Connector) {
    wait("phone connected again", || {
        matches!(connector.state(), LinkState::Connected { .. })
    });
}

/// B6: the Relay stores the phone's Enroll, then its answer is lost (and the connection
/// with it): the approval is `Uncertain`, and once the phone is back `resume_approval`
/// finds it `Done`. The same for a removal. Exactly one unit, one publication each.
#[test]
fn lost_acknowledgement_of_approval_and_removal_resumes_done_without_duplicates() {
    let relay = Relay::start();
    let proxy = FaultProxy::start(&relay.url());
    let a = Computer::new(&relay);
    a.create();
    let (phone_keys, connector) = phone_dialing(&a, 33, Some(proxy.url()));
    let b = Computer::new(&relay);
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b.ring.begin_join(&b.companion()).unwrap();
    let code = offer.code.clone();
    let start = connector.chain().head().version();
    let op = std::thread::scope(|scope| {
        let joining = scope.spawn(|| offer.wait());
        let prepared = prepare_approval(
            connector.clone(),
            phone_keys.clone(),
            &code,
            &PairOptions::default(),
            None,
        )
        .unwrap();
        let op = prepared.op().clone();
        // The Enroll reaches the Relay; nothing the Relay says after it reaches the phone.
        proxy.set_fault(Some(Fault::DeafAfter("roster.put")));
        let publishing = scope.spawn(move || publish_approval(prepared, &mut |_| Ok(())));
        wait_head(&a, start + 1);
        // Stored. The phone's connection then drops and stays down for a while.
        proxy.set_down(true);
        proxy.cut();
        assert_eq!(
            publishing.join().unwrap().unwrap_err(),
            ApproveError::Uncertain
        );
        // The joining machine never got the approver's answer either.
        let joined = joining.join().unwrap();
        assert!(
            joined.as_ref().is_err_and(|e| e.starts_with("finishing:")),
            "{:?}",
            joined.map(|_| ())
        );
        op
    });
    proxy.set_fault(None);
    proxy.set_down(false);
    phone_connected(&connector);
    let device = match resume_approval(&connector, &op) {
        Resume::Done(device) => device,
        other => panic!("{other:?}"),
    };
    let chain = device.chain;
    let m = chain.head().roster().membership().unwrap();
    assert_eq!(
        m.enrollments
            .iter()
            .filter(|u| u.id == op.operation)
            .count(),
        1,
        "one unit"
    );
    assert_eq!(
        chain.head().version(),
        start + 1,
        "one publication, never a second"
    );
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), chain.ring_id()),
        start + 1
    );
    assert_eq!(
        a.ring.chain().unwrap().get(start + 1),
        chain.get(start + 1),
        "the same bytes everywhere"
    );
    // A second resume is still Done and changes nothing.
    assert!(matches!(resume_approval(&connector, &op), Resume::Done(_)));
    // The joining machine finishes by itself.
    b.ring.start_reconciler();
    wait("the joining machine installed the Join", || {
        b.ring.membership_state() == Ok(MembershipState::Member)
    });

    // The removal of the joined machine: stored by the Relay, answer lost.
    let b_key = profile(&b).keys.sign_key();
    let unit = m.unit_for(&b_key).unwrap().id.clone();
    wait("phone has the new head", || {
        connector.chain().head().version() == start + 1
    });
    let removal = prepare_removal(&connector, &*phone_keys, &unit).unwrap();
    let candidate = removal.candidate().unwrap();
    proxy.set_fault(Some(Fault::DeafAfter("roster.put")));
    std::thread::scope(|scope| {
        let publishing = scope.spawn(|| {
            publish_removal(
                &connector,
                &*phone_keys,
                &removal,
                Instant::now() + Duration::from_secs(3),
                &mut |_| Ok(()),
            )
        });
        wait_head(&a, candidate.version());
        proxy.set_down(true);
        proxy.cut();
        assert_eq!(
            publishing.join().unwrap().unwrap_err(),
            RemoveError::Uncertain
        );
    });
    proxy.set_fault(None);
    proxy.set_down(false);
    phone_connected(&connector);
    let removed = match resume_removal(&connector, &removal) {
        Resume::Done(chain) => chain,
        other => panic!("{other:?}"),
    };
    assert_eq!(removed.get(candidate.version()), Some(&candidate));
    assert_eq!(
        removed.head().version(),
        candidate.version(),
        "never a second publication"
    );
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), removed.ring_id()),
        candidate.version()
    );
    assert!(matches!(
        resume_removal(&connector, &removal),
        Resume::Done(_)
    ));
    wait_removed(&b);
    connector.stop(xshell_protocol::ring::relay::ByeReason::quit());
}

/// A resume before the code's expiry while the Enroll has not landed is `Retry`, never
/// `NotDone` (the publication may still land); once it lands, `Done`.
#[test]
fn delayed_publication_resumes_retry_then_done() {
    let relay = Relay::start();
    let proxy = FaultProxy::start(&relay.url());
    let a = Computer::new(&relay);
    a.create();
    let (phone_keys, connector) = phone_dialing(&a, 34, Some(proxy.url()));
    // Another device of the same Ring reads the chain meanwhile.
    let (_other_keys, other) = phone(&a, 35);
    let b = Computer::new(&relay);
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b.ring.begin_join(&b.companion()).unwrap();
    let code = offer.code.clone();
    let start = connector.chain().head().version();
    std::thread::scope(|scope| {
        let joining = scope.spawn(|| offer.wait());
        let prepared = prepare_approval(
            connector.clone(),
            phone_keys.clone(),
            &code,
            &PairOptions::default(),
            None,
        )
        .unwrap();
        let op = prepared.op().clone();
        proxy.set_fault(Some(Fault::HoldFrom("roster.put")));
        let publishing = scope.spawn(move || publish_approval(prepared, &mut |_| Ok(())));
        wait("the Enroll is on its way", || proxy.holding());
        // The chain is readable and lacks the unit; the code is still shown.
        let pin = xshell_protocol::ring::relay::ChainPin {
            version: op.predecessor_version,
            hash: op.predecessor_hash.clone(),
        };
        let read = other.read_accepted_chain(&pin).unwrap();
        assert!(read
            .head()
            .roster()
            .membership()
            .unwrap()
            .unit(&op.operation)
            .is_none());
        assert!(unix_now() < op.expires_at);
        match resume_approval(&other, &op) {
            Resume::Retry => {}
            r => panic!("not landed yet, before expiry: {r:?}"),
        }
        assert_eq!(
            xshell_protocol::ring::relay::contract::relay_version(&relay.target(), read.ring_id()),
            start,
            "nothing landed while the resume ran"
        );
        proxy.release();
        wait_head(&a, start + 1);
        match resume_approval(&other, &op) {
            Resume::Done(device) => {
                assert_eq!(device.chain.head().version(), start + 1);
                assert!(device
                    .chain
                    .head()
                    .roster()
                    .membership()
                    .unwrap()
                    .unit(&op.operation)
                    .is_some());
            }
            r => panic!("{r:?}"),
        }
        let published = publishing.join().unwrap().unwrap();
        assert_eq!(published.chain.head().version(), start + 1);
        assert!(matches!(
            joining.join().unwrap().unwrap(),
            AdoptionOutcome::Installed { .. }
        ));
    });
    connector.stop(xshell_protocol::ring::relay::ByeReason::quit());
    other.stop(xshell_protocol::ring::relay::ByeReason::quit());
}

/// The key as it appears on the wire (`signKey`).
fn wire_key(key: &SignKey) -> String {
    serde_json::to_value(key)
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

fn wait_long(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let until = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < until, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `c`'s Daemon has its Relay connection up.
fn daemon_online(c: &Computer) -> bool {
    let mut local = common::Client::connect(&c.daemon.as_ref().unwrap().socket);
    local.hello(common::range(1, 1));
    local
        .request(&xshell_protocol::msg::ClientMsg::RingIdentity)
        .is_ok_and(|r| r["ring"]["state"] == "connected")
}

/// Two Computers in one Ring whose Relay URL is `proxy`; `span` is the terminal-expiry
/// span of the second one's Desktop and Daemon. Returns b's Desktop and Daemon wire keys.
fn ring_behind(proxy: &FaultProxy, span: Duration) -> (Computer, Computer, String, String) {
    let a = Computer::at_url(&proxy.url(), |_| {}, |_| Arc::new(NoSsh), |_| {});
    a.create();
    let b = Computer::at_url(
        &proxy.url(),
        |c| c.ring_expiry_span = span,
        |_| Arc::new(NoSsh),
        |c| c.expiry_span = span,
    );
    b.join(&a);
    connected(&b);
    wait("b's Daemon online", || daemon_online(&b));
    let p = profile(&b);
    let desk = wire_key(&p.keys.sign_key());
    let daemon = wire_key(&p.companion.unwrap().daemon.sign_key);
    (a, b, desk, daemon)
}

fn refusals(proxy: &FaultProxy, key: &str) -> Vec<Instant> {
    proxy
        .auths(key)
        .into_iter()
        .filter(|s| s.refused)
        .map(|s| s.at)
        .collect()
}

/// Both of b's sides are still members with their stores in place.
fn still_member(b: &Computer, head: u64) {
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::Member);
    assert!(b.ring.notices().unwrap().0.is_none());
    assert_eq!(b.ring.chain().unwrap().head().version(), head);
    assert_eq!(daemon_chain(b).unwrap().head().version(), head);
    assert_eq!(
        files(&b.home.root().join("desktop"), "ring.json.removed-"),
        0
    );
    assert_eq!(files(&b.home.paths().ring_dir, "roster.json.removed-"), 0);
}

/// B4 / P2-6: the Relay refuses a valid member (no signed removal exists). The Desktop
/// and the Daemon each verify their signed history, stay members, and dial again with
/// the normal backoff (never a tight loop); once the Relay lets them in, they are back.
#[test]
fn false_refusal_keeps_membership_and_reconnects_with_backoff() {
    let relay = Relay::start();
    let proxy = FaultProxy::start(&relay.url());
    let (_a, b, desk, daemon) = ring_behind(&proxy, Duration::from_secs(600));
    let head = b.ring.chain().unwrap().head().version();
    proxy.set_fault(Some(Fault::RefuseAuth {
        keys: vec![desk.clone(), daemon.clone()],
        code: "not_member",
        detail: None,
    }));
    let before = [proxy.auths(&desk).len(), proxy.auths(&daemon).len()];
    proxy.cut();
    wait_long(
        "a second refusal of each side",
        Duration::from_secs(30),
        || refusals(&proxy, &desk).len() >= 2 && refusals(&proxy, &daemon).len() >= 2,
    );
    proxy.set_fault(None);
    for key in [&desk, &daemon] {
        let r = refusals(&proxy, key);
        assert!(
            r[1].duration_since(r[0]) >= Duration::from_secs(4),
            "dialed again after {:?}: a tight loop",
            r[1].duration_since(r[0])
        );
    }
    wait_long("b back on the Relay", Duration::from_secs(40), || {
        b.ring.view().connection == "connected" && daemon_online(&b)
    });
    still_member(&b, head);
    // From the cut to the reconnection: two refusals and one admission per side, at most
    // one more.
    for (key, before) in [(&desk, before[0]), (&daemon, before[1])] {
        let n = proxy.auths(key).len() - before;
        assert!(n <= 4, "{n} attempts");
    }
}

/// P2-6 / B5: three refusals naming terminal expiry over at least the configured span set
/// the membership aside with the removed state, on both sides.
#[test]
fn expiry_refusals_over_the_span_set_the_membership_aside() {
    let relay = Relay::start();
    let proxy = FaultProxy::start(&relay.url());
    let span = Duration::from_secs(8);
    let (_a, b, desk, daemon) = ring_behind(&proxy, span);
    proxy.set_fault(Some(Fault::RefuseAuth {
        keys: vec![desk.clone(), daemon.clone()],
        code: "not_member",
        detail: Some("terminal_expired"),
    }));
    proxy.cut();
    wait_long("b shows the removed state", Duration::from_secs(60), || {
        b.ring.membership_state() == Ok(MembershipState::Removed) && b.ring.chain().is_none()
    });
    assert!(b.ring.notices().unwrap().0.is_some(), "the removal notice");
    wait_long(
        "b's Daemon set its membership aside",
        Duration::from_secs(60),
        || {
            !b.home.paths().ring_dir.join("roster.json").exists()
                && files(&b.home.paths().ring_dir, "roster.json.removed-") > 0
        },
    );
    for key in [&desk, &daemon] {
        let r = refusals(&proxy, key);
        assert!(r.len() >= 3, "{} refusals", r.len());
        assert!(r[2].duration_since(r[0]) >= span);
    }
}

/// The opposite cases: one expiry refusal, three plain refusals, or three expiry refusals
/// within less than the span keep the membership; the machine reconnects by itself.
fn refusals_keep_membership(n: usize, detail: Option<&'static str>, span: Duration) {
    let relay = Relay::start();
    let proxy = FaultProxy::start(&relay.url());
    let (_a, b, desk, daemon) = ring_behind(&proxy, span);
    let head = b.ring.chain().unwrap().head().version();
    proxy.set_fault(Some(Fault::RefuseAuth {
        keys: vec![desk.clone(), daemon.clone()],
        code: "not_member",
        detail,
    }));
    proxy.cut();
    wait_long("the refusals", Duration::from_secs(40), || {
        refusals(&proxy, &desk).len() >= n && refusals(&proxy, &daemon).len() >= n
    });
    proxy.set_fault(None);
    // The next dial is admitted; any set-aside would have happened before it.
    wait_long("b back on the Relay", Duration::from_secs(60), || {
        b.ring.view().connection == "connected" && daemon_online(&b)
    });
    still_member(&b, head);
    assert_eq!(refusals(&proxy, &desk).len(), n);
    assert_eq!(refusals(&proxy, &daemon).len(), n);
}

#[test]
fn a_single_expiry_refusal_keeps_membership() {
    refusals_keep_membership(1, Some("terminal_expired"), Duration::ZERO);
}

#[test]
fn plain_refusals_keep_membership() {
    refusals_keep_membership(3, None, Duration::from_secs(1));
}

#[test]
fn expiry_refusals_within_less_than_the_span_keep_membership() {
    refusals_keep_membership(3, Some("terminal_expired"), Duration::from_secs(120));
}

// ── Ported from the format-1 suites (upgrade_completion.rs, deleted in S2): the cases
// that do not depend on the format. ───────────────────────────────────────────────────

fn create_confirm(
    p: &xshell_hostlink::ring::adoption::ConsentPreview,
    lease: &xshell_hostlink::ring::adoption::CreateConfirmationLease,
) -> Result<CreateDecision, String> {
    Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
}

#[test]
fn local_companion_client_errors_until_registered_and_membership_follows_create() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    let error = a
        .ring
        .local_companion_client()
        .err()
        .expect("not registered");
    assert!(error.starts_with("not_registered"), "{error}");
    assert_eq!(a.ring.membership_state().unwrap(), MembershipState::None);
    a.ring.register_companion(&a.companion()).unwrap();
    let client = a.ring.local_companion_client().unwrap();
    assert_eq!(client.endpoint(), a.companion().endpoint());
    assert_eq!(a.ring.membership_state().unwrap(), MembershipState::None);
    // Create through the client the UI uses: the first member can approve at once.
    assert!(matches!(
        a.ring.create_computer(&client, create_confirm).unwrap(),
        AdoptionOutcome::Installed { .. }
    ));
    assert_eq!(a.ring.membership_state().unwrap(), MembershipState::Member);
}

#[test]
fn cancelled_join_start_aborts_the_ready_wait_and_never_shows_an_offer() {
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    let relay = Relay::start();
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(Some((reached_tx, release_rx))));
    let hook = AdoptionTestHook(Arc::new(move |point| {
        if point == AdoptionTestPoint::JoinSlotReady {
            // Only the first start is held at the slot acknowledgment.
            if let Some((reached, release)) = gate.lock().unwrap().take() {
                reached.send(()).unwrap();
                release.recv().unwrap();
            }
        }
    }));
    let b = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = Some(hook));
    b.ring.register_companion(&b.companion()).unwrap();
    let client = b.ring.local_companion_client().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (ring, start_client, start_cancel) = (b.ring.clone(), client.clone(), cancel.clone());
    let start =
        std::thread::spawn(move || ring.begin_join_cancellable(&start_client, start_cancel, None));
    reached_rx.recv_timeout(WAIT).expect("readiness reached");
    // The UI's cancel command during the start.
    cancel.store(true, Ordering::Release);
    release_tx.send(()).unwrap();
    let error = start
        .join()
        .unwrap()
        .err()
        .expect("a cancelled start shows no code");
    assert!(error.starts_with("cancelled"), "{error}");
    // The cancelled driver ended: its reservation is gone and nothing is unfinished.
    let stored = profile(&b);
    assert!(stored.pending_join.is_none() && stored.membership.is_none());
    assert!(stored.journal.as_ref().is_none_or(|j| matches!(
        j.phase,
        xshell_hostlink::ring::store::AdoptionPhase::Cancelled
            | xshell_hostlink::ring::store::AdoptionPhase::ExpiredBeforeProof
    )));
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::None);
    // The admission is free again: a new Join starts and waits for its approver.
    let offer = b
        .ring
        .begin_join_cancellable(&client, Arc::new(AtomicBool::new(false)), None)
        .unwrap();
    assert!(offer.is_ready());
    assert!(matches!(
        offer.join_status(),
        xshell_hostlink::ring::adoption::JoinStatus::Waiting
    ));
    offer.cancel_join();
    drop(offer);
}

#[test]
fn create_and_join_bind_the_chosen_relay_not_the_default() {
    let relay = Relay::start();
    // The default points nowhere: success proves the chosen Relay is bound.
    let unreachable = "ws://127.0.0.1:9/";
    let a = Computer::with_ring_config(&relay, |cfg| cfg.default_relay_url = unreachable.into());
    a.ring.register_companion(&a.companion()).unwrap();
    let chain = match a
        .ring
        .create_computer_on(&a.companion(), &relay.url(), create_confirm)
        .unwrap()
    {
        AdoptionOutcome::Installed { chain, .. } => chain,
        _ => panic!("Create not installed"),
    };
    assert_eq!(chain.head().roster().relay_url, relay.url());
    assert!(a
        .ring
        .create_computer_on(&a.companion(), "not a url", create_confirm)
        .is_err());
    // A Join binds to the Relay its code was created for, and completes there.
    let b = Computer::with_ring_config(&relay, |cfg| cfg.default_relay_url = unreachable.into());
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b
        .ring
        .begin_join_cancellable(
            &b.ring.local_companion_client().unwrap(),
            Arc::new(AtomicBool::new(false)),
            Some(&relay.url()),
        )
        .unwrap();
    assert!(offer.is_ready());
    connected(&a);
    let code = offer.code.clone();
    let joined = std::thread::scope(|scope| {
        let approval = scope.spawn(|| a.ring.approve_device_code(&code));
        let outcome = offer.wait().unwrap();
        approval.join().unwrap().unwrap();
        outcome
    });
    let AdoptionOutcome::Installed { chain, .. } = joined else {
        panic!("Join not installed");
    };
    assert_eq!(chain.head().roster().relay_url, relay.url());
    assert!(chain
        .authorized_member(&profile(&b).keys.sign_key())
        .is_some());
    // A code made for an unparseable Relay never starts.
    assert!(b
        .ring
        .begin_join_cancellable(
            &b.ring.local_companion_client().unwrap(),
            Arc::new(AtomicBool::new(false)),
            Some("not a url"),
        )
        .is_err());
}

#[test]
fn an_unfinished_join_stays_finishing_while_registration_holds_the_admission() {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Mutex;
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    let relay = Relay::start();
    // 1: abort a Join after its durable reservation (an earlier, now unfinished Join).
    // 2: hold the next registration's admission until released.
    let mode = Arc::new(AtomicU8::new(0));
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(Some((reached_tx, release_rx))));
    let hook_mode = mode.clone();
    let hook = AdoptionTestHook(Arc::new(move |point| {
        match hook_mode.load(Ordering::SeqCst) {
            1 if point == AdoptionTestPoint::JoinBeforeCounterpartStart => {
                hook_mode.store(0, Ordering::SeqCst);
                panic!("simulated crash after the Join reservation");
            }
            2 if point == AdoptionTestPoint::RegistrationAdmitted => {
                if let Some((reached, release)) = gate.lock().unwrap().take() {
                    reached.send(()).unwrap();
                    release.recv().unwrap();
                }
            }
            _ => {}
        }
    }));
    let b = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = Some(hook));
    b.ring.register_companion(&b.companion()).unwrap();
    mode.store(1, Ordering::SeqCst);
    let (ring, client) = (b.ring.clone(), b.ring.local_companion_client().unwrap());
    let crashed = std::thread::spawn(move || {
        ring.begin_join_cancellable(&client, Arc::new(AtomicBool::new(false)), None)
            .map(|_| ())
    })
    .join();
    assert!(crashed.is_err(), "the simulated crash ended the start");
    assert_eq!(
        b.ring.membership_state().unwrap(),
        MembershipState::Finishing
    );
    assert!(!b.ring.adoption_busy());
    // The registration loop holds the admission: still finishing, and busy.
    mode.store(2, Ordering::SeqCst);
    let (ring, companion) = (b.ring.clone(), b.companion());
    let registering = std::thread::spawn(move || ring.register_companion(&companion));
    reached_rx
        .recv_timeout(WAIT)
        .expect("registration admitted");
    assert!(b.ring.adoption_busy());
    assert_eq!(
        b.ring.membership_state().unwrap(),
        MembershipState::Finishing
    );
    release_tx.send(()).unwrap();
    registering.join().unwrap().unwrap();
    assert!(!b.ring.adoption_busy());
    // The reconciler finishes it by itself (the reservation is discarded).
    b.ring.start_reconciler();
    wait("the unfinished Join was discarded", || {
        b.ring.membership_state() == Ok(MembershipState::None)
    });
    assert!(profile(&b).pending_join.is_none());
}

#[test]
fn a_live_join_is_busy_not_finishing() {
    let relay = Relay::start();
    let b = Computer::new(&relay);
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b
        .ring
        .begin_join_cancellable(
            &b.ring.local_companion_client().unwrap(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap();
    assert!(b.ring.adoption_busy());
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::None);
    offer.cancel_join();
    drop(offer);
    assert!(!b.ring.adoption_busy());
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::None);
}

/// Ported from ring.rs `membership_survives_restart` (format 1): a Daemon of a format-2
/// Computer keeps every version it gets while connected, catches up with the versions
/// published while it was away, and keeps the whole chain across restarts.
#[test]
fn daemon_membership_survives_restart_and_catches_up() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let mut b = Computer::new(&relay);
    b.join(&a);
    connected(&a);
    // A version published while b's Daemon is connected is stored.
    let c = Computer::new(&relay);
    let v3 = c.join(&a).head().version();
    wait("b's Daemon stored the new version", || {
        daemon_chain(&b).is_some_and(|ch| ch.head().version() == v3)
    });
    let restart = |b: &mut Computer| {
        b.daemon.take().unwrap().shutdown();
        let mut cfg = xshelld::server::Config::new(b.home.home(), b.home.paths());
        cfg.ring_backoff_unit = Duration::from_millis(10);
        b.daemon = Some(xshelld::server::Server::start(cfg).unwrap());
    };
    // Versions published while it is away: it catches up when it connects again.
    b.daemon.take().unwrap().shutdown();
    let d = Computer::new(&relay);
    d.join(&a);
    let e = Computer::new(&relay);
    let head = e.join(&a).head().clone();
    let mut cfg = xshelld::server::Config::new(b.home.home(), b.home.paths());
    cfg.ring_backoff_unit = Duration::from_millis(10);
    b.daemon = Some(xshelld::server::Server::start(cfg).unwrap());
    wait("b's Daemon caught up", || {
        daemon_chain(&b).is_some_and(|ch| ch.head() == &head)
    });
    assert_eq!(
        daemon_chain(&b).unwrap().versions().len() as u64,
        head.version()
    );
    wait("b's Daemon online", || daemon_online(&b));
    // And from disk again.
    restart(&mut b);
    assert_eq!(daemon_chain(&b).unwrap().head(), &head);
    wait("b's Daemon online after the restart", || daemon_online(&b));
    assert_eq!(b.ring.membership_state().unwrap(), MembershipState::Member);
}

/// A removal whose first candidate loses its slot is retried with a fresh signature; the
/// caller journals that replacement before it is sent. Its answer is lost (held, then the
/// connection drops): a resume with the journaled replacement is never `NotDone` while it
/// can still land, and ends `Done` with exactly one removal.
#[test]
fn removal_conflict_retry_is_journaled_and_resumes_done_after_a_lost_answer() {
    let relay = Relay::start();
    let proxy = FaultProxy::start(&relay.url());
    let a = Computer::new(&relay);
    a.create();
    let (phone_keys, connector) = phone_dialing(&a, 36, Some(proxy.url()));
    let (_other_keys, other) = phone(&a, 37);
    let b = Computer::new(&relay);
    b.join(&a);
    let c = Computer::new(&relay);
    let joined = c.join(&a);
    let v = joined.head().version();
    wait("phone at the head", || {
        connector.chain().head().version() == v
    });
    let b_unit = joined
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit_for(&profile(&b).keys.sign_key())
        .unwrap()
        .id
        .clone();
    // The phone stores its removal of b on head v; meanwhile another device takes v+1.
    let original = prepare_removal(&connector, &*phone_keys, &b_unit).unwrap();
    connected(&a);
    a.ring.remove_member(&profile(&c).keys.sign_key()).unwrap();
    wait("phone saw the winning head", || {
        connector.chain().head().version() == v + 1
    });
    // The first candidate is refused locally (its slot is taken); the retry is the first
    // upload: it stays on its way, and the phone hears nothing more.
    proxy.set_fault(Some(Fault::LostFrom("roster.put", 1)));
    let journal = std::sync::Mutex::new(original.clone());
    let publishing = std::thread::scope(|scope| {
        let publishing = scope.spawn(|| {
            publish_removal(
                &connector,
                &*phone_keys,
                &original,
                Instant::now() + Duration::from_secs(4),
                &mut |fresh| {
                    *journal.lock().unwrap() = fresh.clone();
                    Ok(())
                },
            )
        });
        wait("the retry is on its way", || proxy.holding());
        let stored = journal.lock().unwrap().clone();
        assert_eq!(a.ring.chain().unwrap().head().version(), v + 1);
        // Resumed from the journal on another device while the retry has not landed: never
        // NotDone (it re-sends the identical replacement, which lands).
        match resume_removal(&other, &stored) {
            Resume::Done(chain) => assert_eq!(chain.get(v + 2), stored.candidate().ok().as_ref()),
            r => panic!("the replacement can still land: {r:?}"),
        }
        // The phone's answer is lost: its connection drops with the retry still held.
        proxy.set_down(true);
        proxy.cut();
        publishing.join().unwrap()
    });
    assert_eq!(publishing.unwrap_err(), RemoveError::Uncertain);
    proxy.set_fault(None);
    proxy.set_down(false);
    phone_connected(&connector);
    let stored = journal.lock().unwrap().clone();
    let removed = match resume_removal(&connector, &stored) {
        Resume::Done(chain) => chain,
        r => panic!("{r:?}"),
    };
    let replacement = stored.candidate().unwrap();
    assert_eq!(removed.get(v + 2), Some(&replacement));
    assert_eq!(removed.head().version(), v + 2, "exactly one removal");
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), removed.ring_id()),
        v + 2
    );
    wait_removed(&b);
    connector.stop(xshell_protocol::ring::relay::ByeReason::quit());
    other.stop(xshell_protocol::ring::relay::ByeReason::quit());
}

/// The same for an approval's standalone-removal stage: its conflict retry is handed to
/// the caller as an updated operation before it is sent, and the approval completes.
#[test]
fn approval_standalone_removal_retry_is_journaled_before_it_is_sent() {
    let relay = Relay::start();
    let a = Computer::new(&relay);
    a.create();
    let (phone_keys, connector) = phone(&a, 38);
    let c = Computer::new(&relay);
    c.join(&a);
    let b = Computer::new(&relay);
    standalone(b.daemon.as_ref().unwrap(), &a, &relay.url());
    let head = a.ring.chain().unwrap().head().version();
    wait("phone has the standalone head", || {
        connector.chain().head().version() == head
    });
    b.ring.register_companion(&b.companion()).unwrap();
    let offer = b.ring.begin_join(&b.companion()).unwrap();
    let code = offer.code.clone();
    std::thread::scope(|scope| {
        let joining = scope.spawn(|| offer.wait());
        let prepared = prepare_approval(
            connector.clone(),
            phone_keys.clone(),
            &code,
            &PairOptions::default(),
            None,
        )
        .unwrap();
        let original = prepared.op().clone();
        assert!(original.replaced.is_some());
        // Another device takes the slot the stored standalone removal was signed for.
        connected(&a);
        a.ring.remove_member(&profile(&c).keys.sign_key()).unwrap();
        wait("phone saw the winning head", || {
            connector.chain().head().version() == head + 1
        });
        let mut records = Vec::new();
        let device = publish_approval(prepared, &mut |next| {
            // Nothing of the retry has reached the Relay yet.
            assert_eq!(
                xshell_protocol::ring::relay::contract::relay_version(
                    &relay.target(),
                    &next.ring_id
                ),
                head + 1
            );
            records.push(next.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(records.len(), 1, "one replacement record");
        let record = &records[0];
        assert_ne!(record.replaced, original.replaced);
        assert_eq!(record.operation, original.operation);
        let fresh = record.replaced.as_ref().unwrap().candidate().unwrap();
        assert_eq!(device.chain.get(head + 2), Some(&fresh));
        match resume_approval(&connector, record) {
            Resume::Done(d) => assert_eq!(d.chain.head().version(), head + 3),
            r => panic!("{r:?}"),
        }
        assert!(matches!(
            joining.join().unwrap().unwrap(),
            AdoptionOutcome::Installed { .. }
        ));
    });
    connector.stop(xshell_protocol::ring::relay::ByeReason::quit());
}

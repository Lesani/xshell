#![cfg(unix)]
//! Ring just works (S2), end to end on production workerd with real Desktops and Daemons:
//! Join without confirmation, Remove of any unit (this computer included), removal
//! detected by the removed machine itself, Join again with a new identity, format-1 state
//! set aside, a standalone Daemon that joins as a Computer in one approval, and the
//! Mobile's approve/remove library (`relay::approve`, `relay::remove`) with its resume.
mod common;
#[path = "desktop/relay_fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "desktop/workerd.rs"]
mod workerd;
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
    client.timeouts = RingTimeouts {
        connect: Duration::from_secs(5),
        ..RingTimeouts::default()
    };
    let connector =
        Arc::new(Connector::start(ConnectorConfig::new(client), Arc::new(NoEvents)).unwrap());
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
        let device = publish_approval(prepared).unwrap();
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
    publish_removal(&connector, &*phone_keys, &op, Instant::now() + WAIT).unwrap();
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

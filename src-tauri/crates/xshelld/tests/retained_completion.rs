#![cfg(unix)]
//! Runtime3 public standalone -> retained v2 completion. No roster/authorization seeding.
mod common;
#[path = "desktop/relay_fixture.rs"]
#[allow(dead_code)]
mod fixture;
#[allow(dead_code)]
#[path = "desktop/workerd.rs"]
mod workerd;
use fixture::*;
use std::sync::{atomic::AtomicBool, mpsc, Arc};
use std::time::Duration;
use workerd::Relay;
use xshell_hostlink::ring::adoption::{CompletionOutcome, CreateDecision};
use xshell_hostlink::ring::companion_ipc::CompanionClient;
use xshell_hostlink::ring::store::{AdoptionPhase, Store};
use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig};
use xshell_hostlink::{Manager, ManagerConfig};
use xshell_protocol::ring::membership::{ConsentFlow, EnrollmentKind, RosterChange};
use xshelld::server::{Config, Server, ServerHandle};

fn standalone(a: &Computer, d: &ServerHandle, relay: &Relay) -> xshell_protocol::ring::RosterChain {
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let url = relay.url();
        let guest = scope.spawn(move || {
            d.adopt_standalone(
                &url,
                WAIT,
                &Arc::new(AtomicBool::new(false)),
                |code| tx.send(code.clone()).unwrap(),
                |p, lease| Ok(Some(lease.confirm(&p.proposal_hash)?)),
            )
        });
        let code = rx.recv_timeout(WAIT).unwrap();
        a.ring.approve_device_code(&code).unwrap();
        guest.join().unwrap().unwrap()
    })
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + WAIT;
    while !condition() {
        assert!(std::time::Instant::now() < until, "runtime3 wait timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn deliver_current_result(
    approval: &xshell_hostlink::ring::adoption::CompletionApproval<'_>,
    manager: &Manager,
    id: &str,
) {
    // Completing the singleton replaces its enrollment ID. The old Host generation
    // can still look usable just before its final invalidation. Only replay this typed,
    // exact accepted result on the replacement; never retry proposal/proofs/publication.
    wait_for(|| {
        let Some(host) = manager.access(id).filter(|h| h.status().usable()) else {
            return false;
        };
        match approval.deliver_result(&host) {
            Ok(()) => true,
            Err(error)
                if error.contains("broker connection changed")
                    || error.contains("broker connection is not usable")
                    || error.contains("broker connection ended")
                    || error.contains("is offline") =>
            {
                eprintln!("result generation replacement: {error}");
                false
            }
            Err(error) => panic!("accepted result delivery failed: {error}"),
        }
    });
}
fn restart_a(a: &mut Computer, relay: &Relay) {
    let mut cfg = Config::new(a.home.home(), a.home.paths());
    cfg.ring_backoff_unit = Duration::from_millis(10);
    a.daemon = Some(Server::start(cfg).unwrap());
    let mut cfg = DesktopRingConfig::new(a.home.root().join("desktop"), "same name".into());
    cfg.default_relay_url = relay.url();
    cfg.backoff_unit = Duration::from_millis(10);
    a.ring = DesktopRing::open(cfg, Arc::new(Quiet));
    a.manager = Arc::new(Manager::new(ManagerConfig::new(
        "1.5.0",
        Arc::new(NoSsh),
        Arc::new(NoSsh),
        Arc::new(Events::default()),
    )));
    a.ring.attach_manager(&a.manager).unwrap();
}

#[test]
fn retained_daemon_only_pending_while_a_offline_then_completes_without_code() {
    let relay = Relay::start();
    let mut a = Computer::new(&relay);
    a.create();
    let home = common::TestHome::new();
    let d = Server::start(Config::new(home.home(), home.paths())).unwrap();
    let daemon_dir = home.paths().ring_dir;
    let terminal = uuid::Uuid::new_v4();
    let project = home.project("synthetic-retained-shell");
    let mut local = common::Client::connect(&d.socket);
    local.hello(common::range(1, 1));
    let opened = local.open(terminal, common::sh_spec(&project));
    local.attach(terminal);
    let pid = opened["pid"].as_u64().unwrap() as i32;
    assert!(common::alive(pid));
    local.input(terminal, "printf 'RUNTIME3-BEFORE\\n'\n");
    local.output_until(terminal, "RUNTIME3-BEFORE");
    let initial = standalone(&a, &d, &relay);
    let old = initial
        .head()
        .roster()
        .membership()
        .unwrap()
        .enrollments
        .iter()
        .find(|e| e.kind == EnrollmentKind::Daemon)
        .unwrap()
        .clone();
    let key_bytes = std::fs::read(daemon_dir.join("keys.json")).unwrap();
    assert!(!daemon_dir.join("companion.json").exists());
    // The singleton Daemon has no Desktop signer and cannot be an approver.
    assert!(initial
        .authorized_member(&old.members[0])
        .is_some_and(|m| m.role == xshell_protocol::ring::Role::Daemon));
    a.ring.quit();
    a.manager.shutdown();
    a.daemon.take().unwrap().shutdown();
    let mut cfg = DesktopRingConfig::new(home.root().join("desktop"), "fresh B".into());
    cfg.default_relay_url = relay.url();
    let b = DesktopRing::open(cfg, Arc::new(Quiet));
    let client = CompanionClient::new(d.companion_endpoint().unwrap());
    let operation = b.begin_companion_completion(&client).unwrap();
    assert!(matches!(
        b.confirm_companion_completion(&client, |_, _| panic!("offline A cannot have proposed"))
            .unwrap(),
        CompletionOutcome::Pending { .. }
    ));
    let pending = std::fs::read(daemon_dir.join("completion.json")).unwrap();
    assert!(std::str::from_utf8(&pending).unwrap().contains(&operation));
    assert!(!std::str::from_utf8(&pending).unwrap().contains("secret"));
    // Durable offline request survives the Desktop controller's actual owner restart.
    b.quit();
    drop(b);
    let mut cfg = DesktopRingConfig::new(home.root().join("desktop"), "fresh B".into());
    cfg.default_relay_url = relay.url();
    let b = DesktopRing::open(cfg, Arc::new(Quiet));
    assert!(matches!(
        b.confirm_companion_completion(&client, |_, _| panic!("still offline"))
            .unwrap(),
        CompletionOutcome::Pending { .. }
    ));
    restart_a(&mut a, &relay);
    let id = xshell_hostlink::ring::hosts::relay_host_id(&old.members[0]);
    wait_for(|| a.manager.access(&id).is_some_and(|h| h.status().usable()));
    let host = a.manager.access(&id).unwrap();
    assert!(
        host.saved_config().is_none(),
        "completion must use discovered RelayHost without SSH config"
    );
    let mut approval = a.ring.prepare_companion_completion(host).unwrap();
    assert!(
        approval.publish().unwrap().is_none(),
        "typed proposal RPC cannot grant local consent"
    );
    let outcome = b
        .confirm_companion_completion(&client, |preview, lease| {
            assert_eq!(preview.context.kind, ConsentFlow::LegacyCompletion);
            assert!(b.chain().is_none());
            assert_eq!(preview.operation, operation);
            Ok(CreateDecision::Confirm(
                lease.confirm(&preview.proposal_hash)?,
            ))
        })
        .unwrap();
    assert!(matches!(outcome, CompletionOutcome::Consented { .. }));
    assert!(b.chain().is_none(), "consent is not installation");
    let accepted = approval.publish().unwrap().unwrap().chain;
    assert!(approval.publish().is_err(), "never replay publication");
    wait_for(|| a.manager.access(&id).is_some_and(|h| h.status().usable()));
    deliver_current_result(&approval, &a.manager, &id);
    drop(approval);
    let result = b.install_companion_completion(&client).unwrap();
    assert!(matches!(result, CompletionOutcome::Installed { .. }));
    let m = accepted.head().roster().membership().unwrap();
    assert_eq!(accepted.head().version(), 3);
    assert_eq!(m.enrollments.len(), 2);
    assert_eq!(m.retired, vec![old.id.clone()]);
    let fresh = m.unit_for(&old.members[0]).unwrap();
    assert_eq!(fresh.kind, EnrollmentKind::Computer);
    assert_ne!(fresh.id, old.id);
    assert_eq!(fresh.id, operation);
    let RosterChange::CompleteComputer {
        proofs, replaces, ..
    } = &m.change
    else {
        panic!("not completion");
    };
    assert_eq!(replaces, &vec![old.id]);
    assert_eq!(proofs.len(), 2);
    assert!(proofs
        .iter()
        .all(|p| p.context.kind == ConsentFlow::LegacyCompletion));
    assert_eq!(
        std::fs::read(daemon_dir.join("keys.json")).unwrap(),
        key_bytes
    );
    let stored = Store::new(home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    let journal = stored.journal.unwrap();
    assert_eq!(journal.phase, AdoptionPhase::Installed);
    assert!(journal.desktop_installed && journal.daemon_installed);
    let approver = Store::new(a.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .outgoing
        .unwrap();
    assert!(
        approver.final_noise_hash.is_none(),
        "local completion has no fake Noise transcript"
    );
    let serialized = serde_json::to_string(&approver).unwrap();
    assert!(!serialized.contains("desktopInstalled") && !serialized.contains("daemonInstalled"));
    assert!(
        common::alive(pid),
        "adoption/completion must not restart the retained Terminal"
    );
    let mut verify = common::Client::connect(&d.socket);
    let (_, listed) = verify.hello(common::range(1, 1));
    let retained_terminal = listed.iter().find(|t| t.terminal == terminal).unwrap();
    assert_eq!(retained_terminal.pid, Some(pid as u32));
    verify.attach(terminal);
    verify.output_until(terminal, "RUNTIME3-BEFORE");
    verify.input(terminal, "printf 'RUNTIME3-AFTER\\n'\n");
    verify.output_until(terminal, "RUNTIME3-AFTER");
    b.quit();
}

struct Retained {
    b: Arc<DesktopRing>,
    d: ServerHandle,
    a: Computer,
    home: common::TestHome,
    relay: Relay,
    id: String,
}
impl Retained {
    fn new(ttl: Duration) -> Self {
        Self::with_hook(ttl, None)
    }
    fn with_hook(ttl: Duration, hook: Option<xshelld::server::TestHook>) -> Self {
        Self::with_hooks(ttl, hook, None)
    }
    fn with_hooks(
        ttl: Duration,
        hook: Option<xshelld::server::TestHook>,
        desktop_hook: Option<xshell_hostlink::ring::AdoptionTestHook>,
    ) -> Self {
        let relay = Relay::start();
        let a = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = desktop_hook);
        a.create();
        let home = common::TestHome::new();
        let mut config = Config::new(home.home(), home.paths());
        config.test_hook = hook;
        let d = Server::start(config).unwrap();
        let chain = standalone(&a, &d, &relay);
        let singleton = chain
            .head()
            .roster()
            .membership()
            .unwrap()
            .enrollments
            .into_iter()
            .find(|e| e.kind == EnrollmentKind::Daemon)
            .unwrap();
        let id = xshell_hostlink::ring::hosts::relay_host_id(&singleton.members[0]);
        wait_for(|| a.manager.access(&id).is_some_and(|h| h.status().usable()));
        let mut cfg = DesktopRingConfig::new(home.root().join("desktop"), "B".into());
        cfg.default_relay_url = relay.url();
        cfg.pair_ttl = ttl;
        let b = DesktopRing::open(cfg, Arc::new(Quiet));
        Self {
            b,
            d,
            a,
            home,
            relay,
            id,
        }
    }
    fn client(&self) -> CompanionClient {
        CompanionClient::new(self.d.companion_endpoint().unwrap())
    }
    fn begin(&self) {
        self.b.begin_companion_completion(&self.client()).unwrap();
    }
    fn host(&self) -> xshell_hostlink::HostAccess {
        wait_for(|| {
            self.a
                .manager
                .access(&self.id)
                .is_some_and(|h| h.status().usable())
        });
        self.a.manager.access(&self.id).unwrap()
    }
    fn no_local_proofs(&self) {
        let policy: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.home.paths().ring_dir.join("companion.json")).unwrap(),
        )
        .unwrap();
        assert!(policy["obligation"]["proofs"].is_null());
        assert!(self.b.chain().is_none());
    }
    fn confirm(&self) {
        assert!(matches!(
            self.b
                .confirm_companion_completion(&self.client(), |p, lease| Ok(
                    CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?)
                ))
                .unwrap(),
            CompletionOutcome::Consented { .. }
        ));
    }
}
impl Drop for Retained {
    fn drop(&mut self) {
        self.b.quit();
    }
}
#[test]
fn completion_decline_and_wrong_lease_never_release_local_proofs_or_reuse_confirmation() {
    let r = Retained::new(WAIT);
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    assert!(matches!(
        r.b.confirm_companion_completion(&r.client(), |_, _| Ok(CreateDecision::Decline))
            .unwrap(),
        CompletionOutcome::Declined { .. }
    ));
    r.no_local_proofs();
    drop(approval);
    let first = Store::new(r.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .journal
        .unwrap()
        .intent
        .operation;
    let fresh = r.b.begin_companion_completion(&r.client()).unwrap();
    assert_ne!(first, fresh);
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let other = Computer::new(&r.relay);
    let mut wrong = None;
    other
        .ring
        .create_computer(&other.companion(), |p, lease| {
            wrong = Some(lease.confirm(&p.proposal_hash)?);
            Ok(CreateDecision::Decline)
        })
        .unwrap();
    assert!(r
        .b
        .confirm_companion_completion(&r.client(), |_, _| Ok(CreateDecision::Confirm(
            wrong.take().unwrap()
        )))
        .is_err());
    r.no_local_proofs();
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &r.relay.target(),
            r.a.ring.chain().unwrap().ring_id()
        ),
        2
    );
}
#[test]
fn expired_completion_requires_fresh_local_intent_without_another_code() {
    let r = Retained::new(Duration::from_secs(2));
    r.begin();
    let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(r
        .b
        .confirm_companion_completion(&r.client(), |_, _| panic!(
            "expired proposal cannot show live confirmation"
        ))
        .is_err());
    assert!(approval.publish().is_err());
    r.no_local_proofs();
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
    r.b.begin_companion_completion(&r.client()).unwrap();
}
#[test]
fn independent_changed_head_refuses_obsolete_completion_without_local_proofs() {
    let r = Retained::new(WAIT);
    let c = Computer::new(&r.relay);
    c.join(&r.a);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 3)
    });
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let e = Computer::new(&r.relay);
    e.join(&c);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 4)
    });
    let error =
        r.b.confirm_companion_completion(&r.client(), |_, _| {
            panic!("obsolete head cannot release a confirmation")
        })
        .err()
        .unwrap();
    assert!(
        error.contains("membership") || error.contains("retained"),
        "{error}"
    );
    r.no_local_proofs();
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
    // Return/reproposal is a fresh operation, never a rebased old consent.
    r.begin();
    let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    assert!(approval.publish().unwrap().is_none());
    r.confirm();
    approval.publish().unwrap().unwrap();
    deliver_current_result(&approval, &r.a.manager, &r.id);
    assert!(matches!(
        r.b.install_companion_completion(&r.client()).unwrap(),
        CompletionOutcome::Installed { .. }
    ));
}

#[test]
fn registered_or_known_stopped_desktop_never_downgrades_to_standalone() {
    let relay = Relay::start();
    let computer = Computer::new(&relay);
    let profile = computer
        .ring
        .register_companion(&computer.companion())
        .unwrap();
    let before = std::fs::read(computer.home.paths().ring_dir.join("keys.json")).unwrap();
    computer.ring.quit();
    assert!(computer
        .daemon
        .as_ref()
        .unwrap()
        .adopt_standalone(
            &relay.url(),
            WAIT,
            &Arc::new(AtomicBool::new(false)),
            |_| panic!("registered GUI cannot receive standalone words"),
            |_, _| panic!("not headless")
        )
        .is_err());
    assert_eq!(
        std::fs::read(computer.home.paths().ring_dir.join("keys.json")).unwrap(),
        before
    );
    let policy: serde_json::Value = serde_json::from_slice(
        &std::fs::read(computer.home.paths().ring_dir.join("companion.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(policy["profile"]["profile"], profile.profile);
}
#[test]
fn host_local_or_ssh_ingress_cannot_administer_register_or_confirm_completion() {
    use xshell_protocol::msg::ClientMsg;
    use xshell_protocol::ring::companion::CompanionAdminRequest;
    let r = Retained::new(WAIT);
    r.begin();
    let mut local = common::Client::connect(&r.d.socket);
    local.hello(common::range(1, 1));
    let error = local
        .request(&ClientMsg::RingCompanion {
            request: CompanionAdminRequest::Pending,
        })
        .unwrap_err();
    assert!(error.contains("authorized Ring link"), "{error}");
    for json in [
        r#"{"t":"ring.companion-pending","request":{"action":"confirm","confirm":true}}"#,
        r#"{"t":"ring.companion-pending","request":{"action":"sign","bytes":"eA"}}"#,
        r#"{"t":"ring.companion-pending","request":{"action":"register","profile":{}}}"#,
    ] {
        assert!(xshell_protocol::msg::decode_inbound(json.as_bytes()).is_err());
    }
    r.no_local_proofs();
}
#[test]
fn either_store_failure_reports_reconciliation_and_retries_installation_not_enrollment() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    use xshelld::server::{TestHook, TestPoint};
    for fail_desktop in [false, true] {
        let armed = Arc::new(AtomicBool::new(false));
        let admitted = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(AtomicBool::new(false));
        let directory = Arc::new(Mutex::new(None::<std::path::PathBuf>));
        let hook = TestHook(Arc::new({
            let armed = armed.clone();
            let admitted = admitted.clone();
            let directory = directory.clone();
            let hit = hit.clone();
            move |_, point| {
                if !armed.load(Ordering::SeqCst) {
                    return false;
                }
                if point == TestPoint::CompanionRequestAdmitted
                    && admitted.fetch_add(1, Ordering::SeqCst) == 1
                {
                    let dir = directory.lock().unwrap().clone().unwrap();
                    let file = dir.join(if fail_desktop {
                        "ring.json"
                    } else {
                        "roster.json"
                    });
                    std::fs::rename(&file, file.with_extension("fault-backup")).unwrap();
                    if fail_desktop {
                        std::fs::create_dir(&file).unwrap();
                    }
                }
                if !fail_desktop && point == TestPoint::BeforeRingRosterWrite {
                    hit.store(true, Ordering::SeqCst);
                    return true;
                }
                false
            }
        }));
        let r = Retained::with_hook(WAIT, Some(hook));
        r.begin();
        let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
        r.confirm();
        let accepted = approval.publish().unwrap().unwrap().chain;
        deliver_current_result(&approval, &r.a.manager, &r.id);
        *directory.lock().unwrap() = Some(if fail_desktop {
            r.home.root().join("desktop")
        } else {
            r.home.paths().ring_dir
        });
        armed.store(true, Ordering::SeqCst);
        let error =
            r.b.install_companion_completion(&r.client())
                .err()
                .expect("failed private store cannot report Installed");
        assert!(error.starts_with("reconciliation_required:"), "{error}");
        armed.store(false, Ordering::SeqCst);
        if !fail_desktop {
            assert!(
                hit.load(Ordering::SeqCst),
                "actual Saved.write_error path must run"
            );
        }
        let dir = directory.lock().unwrap().clone().unwrap();
        let file = dir.join(if fail_desktop {
            "ring.json"
        } else {
            "roster.json"
        });
        if fail_desktop {
            std::fs::remove_dir(&file).unwrap();
        }
        std::fs::rename(file.with_extension("fault-backup"), &file).unwrap();
        let saved = Store::new(r.home.root().join("desktop"))
            .load_profile()
            .unwrap()
            .0
            .unwrap();
        assert!(saved.membership.is_none());
        assert_eq!(saved.journal.unwrap().phase, AdoptionPhase::Accepted);
        assert!(matches!(
            r.b.install_companion_completion(&r.client()).unwrap(),
            CompletionOutcome::Installed { .. }
        ));
        assert_eq!(
            xshell_protocol::ring::relay::contract::relay_version(
                &r.relay.target(),
                accepted.ring_id()
            ),
            3,
            "installation retry must not enroll or publish again"
        );
    }
}

struct ProtocolEvents;
impl xshell_protocol::ring::relay::RingEvents for ProtocolEvents {
    fn envelope(&self, _: xshell_protocol::ring::SignKey, _: Vec<u8>) {}
    fn presence(
        &self,
        _: xshell_protocol::ring::SignKey,
        _: xshell_protocol::ring::relay::MemberPresence,
    ) {
    }
    fn roster(&self, _: &xshell_protocol::ring::SignedRoster) {}
    fn roster_rejected(&self, e: xshell_protocol::ring::RosterError) {
        panic!("negative-control publication was invalid: {e}");
    }
    fn closed(&self, _: xshell_protocol::ring::relay::CloseReason) {}
}
#[test]
fn revoked_after_accepted_result_never_installs_or_resurrects_the_completed_computer() {
    use xshell_protocol::ring::relay::{RingClient, RingClientConfig};
    let r = Retained::new(WAIT);
    r.begin();
    let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    r.confirm();
    let accepted = approval.publish().unwrap().unwrap().chain;
    deliver_current_result(&approval, &r.a.manager, &r.id);
    drop(approval);
    let saved = Store::new(r.a.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    let b_keys = Store::new(r.home.root().join("desktop"))
        .retained_keys()
        .unwrap();
    let unit = accepted
        .head()
        .roster()
        .membership()
        .unwrap()
        .unit_for(&b_keys.sign_key())
        .unwrap()
        .clone();
    // Public v2 Remove controller is a later slice. This negative control uses the actual
    // retained approver signer and a real authenticated protocol publication, after ending
    // its Connector (never a seeded row, roster injection or competing same-key client).
    r.a.ring.quit();
    r.a.manager.shutdown();
    let client = RingClient::connect(
        RingClientConfig::new(accepted.clone(), saved.keys.clone()),
        Arc::new(ProtocolEvents),
    )
    .unwrap();
    let mut removed = accepted.head().roster().clone();
    removed.version += 1;
    removed.prev = Some(accepted.head().hash());
    removed.issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut membership = removed.membership().unwrap();
    membership.enrollments.retain(|e| e.id != unit.id);
    membership.retired.push(unit.id.clone());
    membership.change = RosterChange::Remove {
        enrollment_id: unit.id,
    };
    removed
        .members
        .retain(|m| !unit.members.contains(&m.sign_key));
    removed.set_membership(&membership);
    let signed = removed.sign(saved.keys.as_ref()).unwrap();
    xshell_protocol::ring::verify_successor(accepted.head(), &signed).unwrap();
    client.publish_roster(&signed).unwrap();
    let mut local = common::Client::connect(&r.d.socket);
    local.hello(common::range(1, 1));
    wait_for(|| {
        local
            .request(&xshell_protocol::msg::ClientMsg::RingIdentity)
            .unwrap()["ring"]["version"]
            .as_u64()
            == Some(4)
    });
    assert!(r.b.install_companion_completion(&r.client()).is_err());
    assert!(r.b.chain().is_none());
    assert!(r.b.begin_companion_completion(&r.client()).is_err());
    let desktop = Store::new(r.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap();
    assert!(desktop.membership.is_none());
    assert_ne!(desktop.journal.unwrap().phase, AdoptionPhase::Installed);
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &r.relay.target(),
            accepted.ring_id()
        ),
        4
    );
    drop(client);
}

#[test]
fn creator_connection_loss_keeps_possible_publication_truthful_and_never_replays() {
    let mut r = Retained::new(WAIT);
    r.begin();
    let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    r.confirm();
    r.relay.restart();
    wait_for(|| {
        r.a.ring.view().connection == "connected"
            && r.a
                .manager
                .access(&r.id)
                .is_some_and(|h| h.status().usable())
    });
    wait_for(|| {
        let Some(host) = r.a.manager.access(&r.id) else {
            return false;
        };
        let (tx, rx) = mpsc::channel();
        host.companion_pending(
            xshell_protocol::ring::companion::CompanionAdminRequest::Pending,
            Box::new(move |value| {
                let _ = tx.send(value);
            }),
        );
        rx.recv_timeout(Duration::from_secs(1))
            .is_ok_and(|value| value.is_ok())
    });
    let error = approval
        .publish()
        .expect_err("captured creator cannot be replaced by reconnection");
    assert!(error.starts_with("reconciliation_required:"), "{error}");
    let outgoing = Store::new(r.a.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .outgoing
        .unwrap();
    assert_eq!(
        outgoing.phase,
        xshell_hostlink::ring::store::OutgoingPhase::PublicationUncertain
    );
    assert!(outgoing.signed_candidate.is_some());
    assert!(outgoing.accepted.is_empty());
    assert!(approval.publish().is_err());
    drop(approval);
    assert!(r.a.ring.prepare_companion_completion(r.host()).is_err());
    assert!(r
        .b
        .cancel_companion_completion(&r.client())
        .unwrap_err()
        .starts_with("reconciliation_required:"));
    assert!(r.b.begin_companion_completion(&r.client()).is_err());
    assert!(r.b.chain().is_none());
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(
            &r.relay.target(),
            r.a.ring.chain().unwrap().ring_id()
        ),
        2
    );
}

#[test]
fn callback_successor_prevents_desktop_completion_proof_release() {
    let r = Retained::new(WAIT);
    let c = Computer::new(&r.relay);
    c.join(&r.a);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 3)
    });
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let e = Computer::new(&r.relay);
    let result = r.b.confirm_companion_completion(&r.client(), |p, lease| {
        e.join(&c);
        wait_for(|| {
            let disk: serde_json::Value = serde_json::from_slice(
                &std::fs::read(r.home.paths().ring_dir.join("roster.json")).unwrap(),
            )
            .unwrap();
            disk["rosters"].as_array().unwrap().len() == 4
        });
        Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
    });
    assert!(
        result.is_err(),
        "changed head inside callback must not return Consented"
    );
    r.no_local_proofs();
    let journal = Store::new(r.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .journal
        .unwrap();
    assert_eq!(
        journal.phase,
        AdoptionPhase::Prepared,
        "no MayHaveLeft before the fresh-head gate"
    );
    r.b.cancel_companion_completion(&r.client()).unwrap();
    drop(approval);
}

#[test]
fn daemon_consent_independently_rejects_prepared_predecessor_after_callback_successor() {
    use xshell_protocol::ring::companion::CompanionRequest;
    use xshell_protocol::ring::membership::{consent_message, ConsentProof};
    use xshell_protocol::ring::{Signature, Signer};
    let r = Retained::new(WAIT);
    let c = Computer::new(&r.relay);
    c.join(&r.a);
    wait_for(|| {
        r.a.ring
            .chain()
            .is_some_and(|chain| chain.head().version() == 3)
    });
    r.begin();
    let approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
    let e = Computer::new(&r.relay);
    r.b.confirm_companion_completion(&r.client(), |p, lease| {
        let _confirmation = lease.confirm(&p.proposal_hash)?;
        e.join(&c);
        wait_for(|| {
            let disk: serde_json::Value = serde_json::from_slice(
                &std::fs::read(r.home.paths().ring_dir.join("roster.json")).unwrap(),
            )
            .unwrap();
            disk["rosters"].as_array().unwrap().len() == 4
        });
        // Explicitly test D's independent proof boundary with B's actual local signer,
        // not forged signatures or a caller-supplied boolean confirmation.
        let keys = Store::new(r.home.root().join("desktop"))
            .retained_keys()
            .unwrap();
        let message = consent_message(&p.proposal, p.expires_at, &p.context).unwrap();
        let proof = ConsentProof {
            key: keys.sign_key(),
            expires_at: p.expires_at,
            context: p.context.clone(),
            signature: Signature::from_bytes(keys.sign(&message).unwrap()),
        };
        let reply = r.client().request(CompanionRequest::Consent {
            operation: p.operation.clone(),
            desktop_proof: proof,
        });
        assert!(
            reply.is_err(),
            "D must refuse its proof even when live and durable agree at a newer head"
        );
        Ok(CreateDecision::Decline)
    })
    .unwrap();
    r.no_local_proofs();
    drop(approval);
}

#[test]
fn completion_lock_wait_rechecks_deadline_and_owner_before_any_publication_attempt() {
    use std::sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Mutex,
    };
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    for lose_owner in [false, true] {
        let path = Arc::new(Mutex::new(None::<std::path::PathBuf>));
        let holder = Arc::new(Mutex::new(None::<std::fs::File>));
        let attempts = Arc::new(AtomicUsize::new(0));
        let expiry = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let hook = AdoptionTestHook(Arc::new({
            let path = path.clone();
            let holder = holder.clone();
            let attempts = attempts.clone();
            move |point| {
                if point == AdoptionTestPoint::CompletionBeforeUncertainWrite {
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(path.lock().unwrap().as_ref().unwrap())
                        .unwrap();
                    f.lock().unwrap();
                    *holder.lock().unwrap() = Some(f);
                    tx.send(()).unwrap();
                }
                if point == AdoptionTestPoint::CompletionBeforePublish {
                    attempts.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
        let r = Retained::with_hooks(
            if lose_owner {
                WAIT
            } else {
                Duration::from_secs(4)
            },
            None,
            Some(hook),
        );
        r.begin();
        let mut approval = r.a.ring.prepare_companion_completion(r.host()).unwrap();
        r.b.confirm_companion_completion(&r.client(), |p, lease| {
            expiry.store(p.expires_at, Ordering::SeqCst);
            Ok(CreateDecision::Confirm(lease.confirm(&p.proposal_hash)?))
        })
        .unwrap();
        *path.lock().unwrap() = Some(r.a.home.root().join("desktop/ring.lock"));
        let result = std::thread::scope(|scope| {
            let publisher = scope.spawn(move || approval.publish());
            rx.recv_timeout(WAIT).unwrap();
            let quit = if lose_owner {
                let q = scope.spawn(|| r.a.ring.quit());
                wait_for(|| !r.a.ring.pairing_ipc_owner());
                Some(q)
            } else {
                wait_for(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        > expiry.load(Ordering::SeqCst)
                });
                None
            };
            holder.lock().unwrap().take();
            let result = publisher.join().unwrap();
            if let Some(q) = quit {
                q.join().unwrap();
            }
            result
        });
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            0,
            "must not call captured publication after expiry/owner loss in the journal lock wait"
        );
        let outgoing = Store::new(r.a.home.root().join("desktop"))
            .load_profile()
            .unwrap()
            .0
            .unwrap()
            .outgoing
            .unwrap();
        assert_eq!(
            outgoing.phase,
            xshell_hostlink::ring::store::OutgoingPhase::PublicationUncertain
        );
        assert!(outgoing.signed_candidate.is_some());
    }
}

#[test]
fn standalone_lost_ack_obligation_fences_alternative_starts_after_expiry_and_restart() {
    use std::sync::{atomic::Ordering, Mutex};
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
    use xshell_protocol::ring::{DeviceKeys, Member, Role, SignedRoster};
    struct NoLegacyCode(Vec<u8>);
    impl std::io::Write for NoLegacyCode {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(bytes);
            assert!(
                !self
                    .0
                    .windows(b"Pairing code:".len())
                    .any(|s| s == b"Pairing code:"),
                "legacy alternative emitted a second credential"
            );
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut failures = vec![];
    for mode in ["create", "join", "legacy", "host"] {
        let relay = Relay::start();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (release, go) = mpsc::channel();
        let go = Mutex::new(go);
        let hook = AdoptionTestHook(Arc::new({
            let cancel = cancel.clone();
            move |point| {
                if point == AdoptionTestPoint::ApprovalProofsReceived {
                    cancel.store(true, Ordering::SeqCst);
                    tx.send(()).unwrap();
                    go.lock().unwrap().recv().unwrap();
                }
            }
        }));
        let a = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = Some(hook));
        a.create();
        let home = common::TestHome::new();
        let paths = home.paths();
        let d = Server::start(Config::new(home.home(), paths.clone())).unwrap();
        let client = CompanionClient::new(d.companion_endpoint().unwrap());
        let CompanionResponse::Identity { identity, .. } =
            client.request(CompanionRequest::Descriptor).unwrap()
        else {
            panic!("identity");
        };
        let (words_tx, words_rx) = mpsc::channel();
        let url = relay.url();
        let guest_cancel = cancel.clone();
        std::thread::scope(|scope| {
            let guest = scope.spawn(|| {
                d.adopt_standalone(
                    &url,
                    Duration::from_secs(3),
                    &guest_cancel,
                    |code| words_tx.send(code.clone()).unwrap(),
                    |p, lease| Ok(Some(lease.confirm(&p.proposal_hash)?)),
                )
            });
            let code = words_rx.recv_timeout(WAIT).unwrap();
            let approver = &a.ring;
            let approval = scope.spawn(move || approver.approve_device_code(&code));
            rx.recv_timeout(WAIT).unwrap();
            let outcome = guest.join().unwrap();
            release.send(()).unwrap();
            assert!(
                outcome
                    .err()
                    .is_some_and(|e| e.starts_with("reconciliation_required:")),
                "real proofs were received but D must lose ACK/readback"
            );
            approval.join().unwrap().unwrap();
        });
        assert!(!paths.ring_dir.join("roster.json").exists());
        let journal: serde_json::Value = serde_json::from_slice(
            &std::fs::read(paths.ring_dir.join("standalone-adoption.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(journal["consentMayHaveLeft"], true);
        assert_eq!(journal["installed"], false);
        let expires = journal["intent"]["expiresAt"].as_u64().unwrap();
        d.shutdown();
        wait_for(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                > expires
        });
        let d = Server::start(Config::new(home.home(), paths.clone())).unwrap();
        let client = CompanionClient::new(d.companion_endpoint().unwrap());
        let mut cfg = DesktopRingConfig::new(home.root().join("desktop"), "B".into());
        cfg.default_relay_url = relay.url();
        let b = DesktopRing::open(cfg, Arc::new(Quiet));
        let blocked = match mode {
            "create" => b
                .create_computer(&client, |_, _| {
                    panic!("uncertainty may not reach fresh Computer confirmation")
                })
                .is_err(),
            "join" => match b.begin_join(&client) {
                Err(_) => true,
                Ok(offer) => {
                    drop(offer);
                    false
                }
            },
            "legacy" => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                xshelld::pair::run_pair_to(
                    &xshelld::cli::Opts {
                        relay: Some(relay.url()),
                        ..Default::default()
                    },
                    &paths,
                    &mut NoLegacyCode(vec![]),
                )
            }))
            .is_ok(),
            "host" => {
                // Historical v1 Host-establishment negative, never injected membership.
                let signer = DeviceKeys::generate().unwrap();
                let genesis =
                    SignedRoster::genesis(&signer, signer.noise_key(), "other", &relay.url(), 1)
                        .unwrap();
                let next = genesis
                    .next(&signer, 2, |draft| {
                        draft.add(Member::new(
                            "daemon",
                            Role::Daemon,
                            identity.sign_key,
                            identity.noise_key,
                            2,
                        ))
                    })
                    .unwrap();
                let mut local = common::Client::connect(&d.socket);
                local.hello(common::range(1, 1));
                local
                    .request(&xshell_protocol::msg::ClientMsg::RingJoin {
                        rosters: vec![genesis.token().into(), next.token().into()],
                        expect: None,
                    })
                    .is_err()
            }
            _ => unreachable!(),
        };
        if !blocked {
            failures.push(mode);
        }
        b.quit();
    }
    assert!(
        failures.is_empty(),
        "unresolved standalone consent admitted alternative starts: {failures:?}"
    );
}

#[test]
fn standalone_initial_admission_wait_does_not_extend_wire_expiry_or_publication_authority() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    use std::time::Instant;
    use xshell_hostlink::ring::{AdoptionTestHook, AdoptionTestPoint};
    use xshell_protocol::ring::companion::CompanionRequest;
    fn wall_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
    let relay = Relay::start();
    let (proof_tx, proof_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let attempts = Arc::new(AtomicUsize::new(0));
    let hook = AdoptionTestHook(Arc::new({
        let attempts = attempts.clone();
        move |point| {
            if point == AdoptionTestPoint::ApprovalProofsReceived {
                proof_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            if point == AdoptionTestPoint::ApprovalBeforePublish {
                attempts.fetch_add(1, Ordering::SeqCst);
            }
        }
    }));
    let a = Computer::with_ring_config(&relay, |cfg| cfg.adoption_test_hook = Some(hook));
    let created = a.create();
    let home = common::TestHome::new();
    let paths = home.paths();
    let d = Server::start(Config::new(home.home(), paths.clone())).unwrap();
    let client = CompanionClient::new(d.companion_endpoint().unwrap());
    client.request(CompanionRequest::Descriptor).unwrap();
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.ring_dir.join("ring.lock"))
        .unwrap();
    holder.lock().unwrap();
    let ttl = Duration::from_secs(8);
    let url = relay.url();
    let cancel = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = mpsc::channel();
    let (words_tx, words_rx) = mpsc::channel();
    let (preview_tx, preview_rx) = mpsc::channel();
    let (
        earliest_expiry,
        latest_expiry,
        preview_expiry,
        guest_result,
        approval_result,
        proofs_received,
    ) = std::thread::scope(|scope| {
        let guest = scope.spawn(|| {
            started_tx.send(wall_now()).unwrap();
            d.adopt_standalone(
                &url,
                ttl,
                &cancel,
                |code| words_tx.send(code.clone()).unwrap(),
                |p, lease| {
                    preview_tx.send(p.intent.expires_at).unwrap();
                    Ok(Some(lease.confirm(&p.proposal_hash)?))
                },
            )
        });
        let earliest_expiry = started_rx.recv_timeout(WAIT).unwrap() + ttl.as_secs();
        // Observe the actual offer reservation before charging admission time. The file
        // lock is already held, so the command cannot yet construct or expose its intent.
        wait_for(|| {
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(paths.ring_dir.join("ring-legacy-offer.lock"))
            else {
                return false;
            };
            matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
        });
        let admitted_by = Instant::now();
        let latest_expiry = wall_now() + ttl.as_secs();
        std::thread::sleep(Duration::from_secs(2));
        drop(holder);
        let code = words_rx.recv_timeout(WAIT).unwrap();
        let approver = &a.ring;
        let approval = scope.spawn(move || approver.approve_device_code(&code));
        let preview_expiry = preview_rx.recv_timeout(WAIT);
        let proofs_received = proof_rx.recv_timeout(WAIT).is_ok();
        if proofs_received {
            // Even the latest possible original command deadline has passed. With the
            // incoming bug, the serialized proof still has the admission delay left.
            std::thread::sleep(
                (admitted_by + ttl + Duration::from_millis(100))
                    .saturating_duration_since(Instant::now()),
            );
        }
        let _ = release_tx.send(());
        let approval_result = approval.join().unwrap();
        let guest_result = guest.join().unwrap();
        (
            earliest_expiry,
            latest_expiry,
            preview_expiry.unwrap(),
            guest_result,
            approval_result,
            proofs_received,
        )
    });
    eprintln!("admission lifetime: original wire bounds={earliest_expiry}..={latest_expiry}, preview={preview_expiry}, publication_attempts={}, approval_succeeded={}", attempts.load(Ordering::SeqCst), approval_result.is_ok());
    assert!(
        proofs_received,
        "the real verified guest proof set must reach the publication pause"
    );
    assert!((earliest_expiry..=latest_expiry).contains(&preview_expiry),
        "initial admission renewed wire expiry: preview={preview_expiry}, original bounds={earliest_expiry}..={latest_expiry}");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "no captured publication attempt after the original command deadline"
    );
    assert!(approval_result.is_err());
    assert!(guest_result
        .err()
        .is_some_and(|e| e.starts_with("reconciliation_required:")));
    let journal: serde_json::Value = serde_json::from_slice(
        &std::fs::read(paths.ring_dir.join("standalone-adoption.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(journal["intent"]["expiresAt"], preview_expiry);
    assert_eq!(journal["consentMayHaveLeft"], true);
    assert_eq!(journal["installed"], false);
    assert!(!paths.ring_dir.join("roster.json").exists());
    let outgoing = Store::new(a.home.root().join("desktop"))
        .load_profile()
        .unwrap()
        .0
        .unwrap()
        .outgoing
        .unwrap();
    assert_eq!(outgoing.intent.expires_at, preview_expiry);
    assert_eq!(
        outgoing.phase,
        xshell_hostlink::ring::store::OutgoingPhase::Cancelled
    );
    let candidate =
        xshell_protocol::ring::SignedRoster::parse(outgoing.signed_candidate.as_ref().unwrap())
            .unwrap();
    let RosterChange::Enroll { proofs, .. } = candidate.roster().membership().unwrap().change
    else {
        panic!("not Enroll");
    };
    assert_eq!(proofs.len(), 1);
    assert_eq!(proofs[0].expires_at, preview_expiry);
    assert_eq!(
        xshell_protocol::ring::relay::contract::relay_version(&relay.target(), created.ring_id()),
        1
    );
}

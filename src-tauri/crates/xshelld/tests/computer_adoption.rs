//! Runtime1: real Daemon, protected local IPC and public Create against production workerd.
//! Unix runnable first. No seeded v2 membership and no TestRelay success substitution.
#[cfg(unix)]
#[path = "desktop/workerd.rs"]
mod workerd;
#[cfg(unix)]
mod unix {
    use super::workerd::Relay;
    use std::sync::Arc;
    use xshell_hostlink::ring::adoption::{AdoptionOutcome, CreateDecision};
    use xshell_hostlink::ring::companion_ipc::CompanionClient;
    use xshell_hostlink::ring::store::{AdoptionPhase, Store};
    use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig, RingObserver, RingView};
    use xshell_protocol::ring::membership::{ConsentFlow, RosterChange};
    use xshell_protocol::ring::relay::contract::relay_version;
    use xshelld::server::{Config, Server, ServerHandle};
    struct Observer;
    impl RingObserver for Observer {
        fn changed(&self, _: &RingView) {}
    }
    struct Computer {
        daemon: ServerHandle,
        ring: Arc<DesktopRing>,
        desktop: std::path::PathBuf,
        daemon_dir: std::path::PathBuf,
        // Drop the Daemon/IPC before deleting its private home.
        _home: tempfile::TempDir,
    }
    impl Computer {
        fn new(relay: &Relay) -> Self {
            Self::with_hook(relay, None)
        }
        fn with_hook(relay: &Relay, hook: Option<xshelld::server::TestHook>) -> Self {
            let home = tempfile::Builder::new().prefix("r1-").tempdir().unwrap();
            let paths = xshelld::paths::resolve(home.path(), None, None);
            let daemon_dir = paths.ring_dir.clone();
            let mut daemon_config = Config::new(home.path().into(), paths);
            daemon_config.test_hook = hook;
            let daemon = Server::start(daemon_config).unwrap();
            let desktop = home.path().join("desktop");
            let mut config = DesktopRingConfig::new(desktop.clone(), "computer".into());
            config.default_relay_url = relay.url();
            let ring = DesktopRing::open(config, Arc::new(Observer));
            Self {
                _home: home,
                daemon,
                ring,
                desktop,
                daemon_dir,
            }
        }
        fn companion(&self) -> CompanionClient {
            CompanionClient::new(self.daemon.companion_endpoint().unwrap())
        }
    }
    impl Drop for Computer {
        fn drop(&mut self) {
            self.ring.quit();
        }
    }

    #[test]
    fn public_create_uses_two_private_stores_only_after_backend_confirmation() {
        let mut relay = Relay::start();
        let computer = Computer::new(&relay);
        let companion = computer.companion();
        let profile = computer.ring.register_companion(&companion).unwrap();
        assert!(computer.ring.chain().is_none());
        assert!(!computer.daemon_dir.join("roster.json").exists());
        let keys_before = std::fs::read(computer.daemon_dir.join("keys.json")).unwrap();
        let outcome = computer
            .ring
            .create_computer(&companion, |preview, lease| {
                assert!(computer.ring.chain().is_none());
                assert!(!computer.daemon_dir.join("roster.json").exists());
                let journal = Store::new(computer.desktop.clone())
                    .load_profile()
                    .unwrap()
                    .0
                    .unwrap()
                    .journal
                    .unwrap();
                assert_eq!(journal.phase, AdoptionPhase::Prepared);
                assert!(journal.signed_candidate.is_none());
                assert!(
                    computer.ring.register_companion(&companion).is_err(),
                    "one admitted owner operation"
                );
                assert_eq!(preview.context.kind, ConsentFlow::LocalGenesis);
                assert_eq!(relay_version(&relay.target(), &preview.ring_id), 0);
                Ok(CreateDecision::Confirm(
                    lease.confirm(&preview.proposal_hash)?,
                ))
            })
            .unwrap();
        let AdoptionOutcome::Installed { chain, .. } = outcome else {
            panic!("Create not installed");
        };
        let head = chain.head();
        let membership = head.roster().membership().unwrap();
        assert_eq!(membership.enrollments.len(), 1);
        assert_eq!(head.roster().members.len(), 2);
        let RosterChange::Genesis { proofs } = &membership.change else {
            panic!("not genesis");
        };
        assert_eq!(proofs.len(), 2);
        assert!(proofs
            .iter()
            .all(|p| p.context.kind == ConsentFlow::LocalGenesis));
        assert!(proofs.iter().any(|p| p.key == profile.desktop.sign_key));
        assert!(proofs.iter().any(|p| p.key == profile.daemon.sign_key));
        assert!(
            std::fs::read(computer.daemon_dir.join("keys.json")).unwrap() == keys_before,
            "Daemon keys changed"
        );
        let saved = Store::new(computer.desktop.clone())
            .load_profile()
            .unwrap()
            .0
            .unwrap();
        assert_eq!(saved.keys.sign_key(), profile.desktop.sign_key);
        let journal = saved.journal.unwrap();
        assert_eq!(journal.phase, AdoptionPhase::Installed);
        assert!(journal.desktop_installed && journal.daemon_installed);
        assert_eq!(journal.signed_candidate.as_deref(), Some(head.token()));
        assert_eq!(journal.accepted[0], head.token());
        let daemon: serde_json::Value = serde_json::from_slice(
            &std::fs::read(computer.daemon_dir.join("roster.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(daemon["rosters"][0], head.token());
        assert_eq!(membership.enrollments[0].id, journal.intent.operation);
        let desktop_bytes = std::fs::read(computer.desktop.join("ring.json")).unwrap();
        let desktop_json: serde_json::Value = serde_json::from_slice(&desktop_bytes).unwrap();
        let daemon_json: serde_json::Value = serde_json::from_slice(&keys_before).unwrap();
        assert!(
            desktop_json["signSeed"] != daemon_json["signSeed"],
            "private signing seeds must remain separate"
        );
        assert!(
            desktop_json["noiseSeed"] != daemon_json["noiseSeed"],
            "private Noise seeds must remain separate"
        );
        assert!(!String::from_utf8_lossy(&desktop_bytes)
            .contains(daemon_json["signSeed"].as_str().unwrap()));
        let daemon_journal = std::fs::read(computer.daemon_dir.join("companion.json")).unwrap();
        assert!(!String::from_utf8_lossy(&daemon_journal)
            .contains(desktop_json["signSeed"].as_str().unwrap()));
        use std::os::unix::fs::PermissionsExt;
        for file in [
            computer.desktop.join("ring.json"),
            computer.daemon_dir.join("keys.json"),
            computer.daemon_dir.join("companion.json"),
            computer.daemon_dir.join("roster.json"),
        ] {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        let wait_online = || {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let view = computer.ring.view();
                if view.connection == "connected"
                    && view.members.len() == 2
                    && view
                        .members
                        .iter()
                        .all(|member| member.presence.kind == "online")
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < until,
                    "normal Connector did not become usable: {} {:?}",
                    view.connection,
                    view.connection_error
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        wait_online();
        assert!(computer
            .ring
            .create_computer(&companion, |_, _| panic!(
                "installed membership cannot silently create again"
            ))
            .is_err());
        assert_eq!(relay_version(&relay.target(), chain.ring_id()), 1);
        relay.restart();
        assert_eq!(relay_version(&relay.target(), chain.ring_id()), 1);
        assert_eq!(computer.ring.chain().unwrap().head().hash(), head.hash());
        wait_online();
    }

    #[test]
    fn decline_creates_no_enrollment_proofs_candidate_or_relay_membership() {
        let relay = Relay::start();
        let computer = Computer::new(&relay);
        let companion = computer.companion();
        let profile = computer.ring.register_companion(&companion).unwrap();
        let result = computer
            .ring
            .create_computer(&companion, |_, _| Ok(CreateDecision::Decline))
            .unwrap();
        assert!(matches!(result, AdoptionOutcome::Declined { .. }));
        let journal = Store::new(computer.desktop.clone())
            .load_profile()
            .unwrap()
            .0
            .unwrap()
            .journal
            .unwrap();
        assert_eq!(journal.phase, AdoptionPhase::Cancelled);
        assert!(journal.signed_candidate.is_none() && journal.accepted.is_empty());
        assert!(computer.ring.chain().is_none());
        assert!(!computer.daemon_dir.join("roster.json").exists());
        let policy: serde_json::Value = serde_json::from_slice(
            &std::fs::read(computer.daemon_dir.join("companion.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(policy["obligation"]["phase"], "cancelled");
        assert!(policy["obligation"]["proofs"].is_null());
        assert_eq!(
            relay_version(
                &relay.target(),
                &xshell_protocol::ring::RingId::derive(&profile.desktop.sign_key)
            ),
            0
        );
    }
    fn host_request(
        daemon: &ServerHandle,
        message: xshell_protocol::msg::ClientMsg,
    ) -> Result<serde_json::Value, String> {
        use std::io::Write;
        use xshell_protocol::frame::{read_frame, Frame, MAX_FRAME_LEN};
        use xshell_protocol::msg::{encode_msg, ClientMsg, Hello};
        let mut stream = daemon
            .connect_in_process(xshelld::server::Role::Desktop)
            .unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        read_frame(&mut stream, MAX_FRAME_LEN).unwrap();
        stream
            .write_all(
                &encode_msg(
                    &ClientMsg::Hello(Hello {
                        protocol: xshell_protocol::PROTOCOL,
                        version: "test".into(),
                        capabilities: vec![],
                        build: None,
                    }),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        stream
            .write_all(&encode_msg(&message, Some(1)).unwrap())
            .unwrap();
        loop {
            if let Some(Frame::Json(bytes)) = read_frame(&mut stream, MAX_FRAME_LEN).unwrap() {
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                if value["t"] == "res" && value["id"] == 1 {
                    return if let Some(error) = value["err"].as_str() {
                        Err(error.into())
                    } else {
                        Ok(value["ok"].clone())
                    };
                }
            }
        }
    }
    #[test]
    fn registered_counterpart_cannot_be_downgraded_through_host_ring_join() {
        let relay = Relay::start();
        let computer = Computer::new(&relay);
        let profile = computer
            .ring
            .register_companion(&computer.companion())
            .unwrap();
        // Synthetic legacy negative fixture, not v2 bootstrap: an unrelated Desktop tries
        // to install a Daemon singleton over ordinary Host ingress after GUI registration.
        use xshell_protocol::ring::{DeviceKeys, Member, Role, SignedRoster};
        let unrelated = DeviceKeys::generate().unwrap();
        let genesis = SignedRoster::genesis(
            &unrelated,
            unrelated.noise_key(),
            "unrelated",
            &relay.url(),
            1,
        )
        .unwrap();
        let next = genesis
            .next(&unrelated, 2, |draft| {
                draft.add(Member::new(
                    "daemon",
                    Role::Daemon,
                    profile.daemon.sign_key,
                    profile.daemon.noise_key,
                    2,
                ))
            })
            .unwrap();
        let error = host_request(
            &computer.daemon,
            xshell_protocol::msg::ClientMsg::RingJoin {
                rosters: vec![genesis.token().into(), next.token().into()],
                expect: None,
            },
        )
        .unwrap_err();
        assert!(error.contains("protected_counterpart_required"), "{error}");
        assert!(!computer.daemon_dir.join("roster.json").exists());
    }

    #[test]
    fn cancelled_wrong_hash_and_stale_leases_release_no_enrollment_proofs() {
        let relay = Relay::start();
        let computer = Computer::new(&relay);
        let client = computer.companion();
        computer.ring.register_companion(&client).unwrap();
        assert!(computer
            .ring
            .create_computer(&client, |preview, lease| {
                let proof = lease.confirm(&preview.proposal_hash)?;
                lease.cancel();
                Ok(CreateDecision::Confirm(proof))
            })
            .is_err());
        assert!(computer
            .ring
            .create_computer(&client, |_, lease| Ok(CreateDecision::Confirm(
                lease.confirm("wrong-hash")?
            )))
            .is_err());
        let mut stale = None;
        computer
            .ring
            .create_computer(&client, |preview, lease| {
                stale = Some(lease.confirm(&preview.proposal_hash)?);
                Ok(CreateDecision::Decline)
            })
            .unwrap();
        assert!(computer
            .ring
            .create_computer(&client, |_, _| Ok(CreateDecision::Confirm(
                stale.take().unwrap()
            )))
            .is_err());
        let policy: serde_json::Value = serde_json::from_slice(
            &std::fs::read(computer.daemon_dir.join("companion.json")).unwrap(),
        )
        .unwrap();
        assert!(policy["obligation"]["proofs"].is_null());
        assert!(!computer.daemon_dir.join("roster.json").exists());
        assert!(computer.ring.chain().is_none());
        let stored = Store::new(computer.desktop.clone())
            .load_profile()
            .unwrap()
            .0
            .unwrap();
        assert_eq!(stored.journal.unwrap().phase, AdoptionPhase::Cancelled);
        assert_eq!(
            relay_version(
                &relay.target(),
                &xshell_protocol::ring::RingId::derive(&stored.keys.sign_key())
            ),
            0
        );
    }

    #[test]
    fn registered_gui_survives_daemon_stop_restart_without_fallback_or_new_keys() {
        let relay = Relay::start();
        let home = tempfile::Builder::new().prefix("r1-").tempdir().unwrap();
        let paths = xshelld::paths::resolve(home.path(), None, None);
        let daemon_dir = paths.ring_dir.clone();
        let server = Server::start(Config::new(home.path().into(), paths.clone())).unwrap();
        let client = CompanionClient::new(server.companion_endpoint().unwrap());
        let desktop = home.path().join("desktop");
        let mut config = DesktopRingConfig::new(desktop.clone(), "computer".into());
        config.default_relay_url = relay.url();
        let ring = DesktopRing::open(config, Arc::new(Observer));
        let profile = ring.register_companion(&client).unwrap();
        let before = std::fs::read(daemon_dir.join("keys.json")).unwrap();
        server.shutdown();
        assert!(!client.endpoint().exists());
        assert!(daemon_dir.join("companion.json").is_file());
        assert!(ring
            .create_computer(&client, |_, _| panic!(
                "stopped counterpart cannot reach confirmation"
            ))
            .is_err());
        assert!(ring.chain().is_none());
        assert_eq!(
            Store::new(desktop)
                .load_profile()
                .unwrap()
                .0
                .unwrap()
                .companion,
            Some(profile.clone())
        );
        let server = Server::start(Config::new(home.path().into(), paths)).unwrap();
        assert_eq!(ring.register_companion(&client).unwrap(), profile);
        assert!(
            std::fs::read(daemon_dir.join("keys.json")).unwrap() == before,
            "Daemon keys changed on restart"
        );
        assert!(matches!(
            ring.create_computer(&client, |preview, lease| Ok(CreateDecision::Confirm(
                lease.confirm(&preview.proposal_hash)?
            )))
            .unwrap(),
            AdoptionOutcome::Installed { .. }
        ));
        ring.quit();
        server.shutdown();
    }

    #[test]
    fn conflicting_profile_and_wire_confirmation_cannot_authorize_the_daemon() {
        use xshell_protocol::ring::companion::{
            registration_message, CompanionProfile, CompanionRequest, CompanionResponse,
        };
        use xshell_protocol::ring::{DeviceKeys, Signature, Signer};
        let relay = Relay::start();
        let computer = Computer::new(&relay);
        let client = computer.companion();
        let profile = computer.ring.register_companion(&client).unwrap();
        let other = DeviceKeys::generate().unwrap();
        let mut identity = profile.desktop.clone();
        identity.sign_key = other.sign_key();
        identity.noise_key = other.noise_key();
        let conflict = CompanionProfile::for_stores(
            profile.desktop_store.clone(),
            profile.daemon_store.clone(),
            identity,
            profile.daemon.clone(),
            profile.endpoint.clone(),
        );
        let proof = Signature::from_bytes(other.sign(&registration_message(&conflict)).unwrap());
        assert!(client
            .request(CompanionRequest::Register {
                profile: conflict,
                proof
            })
            .is_err());
        for request in [
            serde_json::json!({"type":"confirm","confirm":true}),
            serde_json::json!({"type":"sign","bytes":"eA"}),
        ] {
            assert!(xshell_core::local_ipc::exchange::<_, CompanionResponse>(
                client.endpoint(),
                &request
            )
            .is_err());
        }
        let CompanionResponse::Identity {
            profile: retained, ..
        } = client.request(CompanionRequest::Descriptor).unwrap()
        else {
            panic!("identity missing");
        };
        assert_eq!(retained, Some(profile));
        assert!(!computer.daemon_dir.join("roster.json").exists());
    }

    #[test]
    fn second_desktop_owner_cannot_start_or_confirm_create() {
        let relay = Relay::start();
        let computer = Computer::new(&relay);
        let client = computer.companion();
        computer.ring.register_companion(&client).unwrap();
        let mut config = DesktopRingConfig::new(computer.desktop.clone(), "computer".into());
        config.default_relay_url = relay.url();
        let other = DesktopRing::open(config, Arc::new(Observer));
        assert!(other
            .create_computer(&client, |_, _| panic!("non-owner callback must not run"))
            .is_err());
        assert!(!computer.daemon_dir.join("roster.json").exists());
        other.quit();
    }

    #[test]
    fn accepted_create_with_invalid_daemon_store_path_stays_reconciliation_not_success_or_genesis()
    {
        let relay = Relay::start();
        let computer = Computer::new(&relay);
        let client = computer.companion();
        let profile = computer.ring.register_companion(&client).unwrap();
        let error = computer
            .ring
            .create_computer(&client, |preview, lease| {
                std::fs::create_dir(computer.daemon_dir.join("roster.json")).unwrap();
                Ok(CreateDecision::Confirm(
                    lease.confirm(&preview.proposal_hash)?,
                ))
            })
            .err()
            .expect("failed Daemon installation cannot succeed");
        assert!(error.starts_with("reconciliation_required:"));
        std::fs::remove_dir(computer.daemon_dir.join("roster.json")).unwrap();
        let saved = Store::new(computer.desktop.clone())
            .load_profile()
            .unwrap()
            .0
            .unwrap();
        assert!(saved.membership.is_none());
        let journal = saved.journal.unwrap();
        assert_eq!(journal.phase, AdoptionPhase::Accepted);
        assert_eq!(journal.signed_candidate.as_ref(), journal.accepted.first());
        assert!(!journal.desktop_installed && !journal.daemon_installed);
        assert_eq!(
            relay_version(
                &relay.target(),
                &xshell_protocol::ring::RingId::derive(&profile.desktop.sign_key)
            ),
            1
        );
        assert!(computer
            .ring
            .create_computer(&client, |_, _| panic!(
                "uncertain Create is not a new attempt"
            ))
            .err()
            .unwrap()
            .starts_with("reconciliation_required:"));
    }
    #[test]
    fn long_home_short_xdg_keeps_legacy_server_usable_and_companion_explicitly_unsupported() {
        let temp = tempfile::Builder::new()
            .prefix("r1-path-")
            .tempdir()
            .unwrap();
        let home = temp.path().join("h".repeat(110));
        std::fs::create_dir(&home).unwrap();
        let runtime = temp.path().join("run");
        let paths = xshelld::paths::resolve(&home, Some(&runtime), None);
        let server = Server::start(Config::new(home, paths.clone()));
        assert!(
            server.is_ok(),
            "optional companion path length must not break ordinary serving"
        );
        let server = server.unwrap();
        assert!(host_request(&server, xshell_protocol::msg::ClientMsg::RingIdentity).is_ok());
        assert_eq!(
            server.companion_endpoint().unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        server.shutdown();
    }

    #[test]
    fn exited_handle_cannot_keep_companion_endpoint_or_reject_successor() {
        let home = tempfile::Builder::new()
            .prefix("r1-life-")
            .tempdir()
            .unwrap();
        let paths = xshelld::paths::resolve(home.path(), None, None);
        let old = Server::start(Config::new(home.path().into(), paths.clone())).unwrap();
        let endpoint = old.companion_endpoint().unwrap();
        old.stopper().shutdown();
        assert!(old
            .wait_timeout(std::time::Duration::from_secs(2))
            .is_some());
        let removed = !endpoint.exists();
        let next = Server::start(Config::new(home.path().into(), paths));
        let usable = next.is_ok();
        // Destroy the old wrapper while the successor is live; its cleanup must not
        // remove the successor's endpoint either.
        drop(old);
        if let Ok(next) = next {
            assert!(host_request(&next, xshell_protocol::msg::ClientMsg::RingIdentity).is_ok());
            assert!(CompanionClient::new(next.companion_endpoint().unwrap())
                .request(xshell_protocol::ring::companion::CompanionRequest::Descriptor)
                .is_ok());
            next.shutdown();
        }
        assert!(removed && usable, "shutdown completion must drain/remove companion before releasing owner, even while old handle lives");
    }

    // Harness-only child: holds a real cross-process File lock until killed by its owner.
    #[test]
    fn external_ring_lock_holder() {
        use std::io::{Read, Write};
        let Some(path) = std::env::var_os("XSHELL_TEST_COMPANION_LOCK") else {
            return;
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        file.lock().unwrap();
        println!("RING_LOCK_READY");
        std::io::stdout().flush().unwrap();
        let _ = std::io::stdin().read(&mut [0]);
    }
    struct ExternalLock(std::process::Child);
    impl ExternalLock {
        fn start(path: &std::path::Path) -> Self {
            use std::io::BufRead;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "unix::external_ring_lock_holder", "--nocapture"])
                .env("XSHELL_TEST_COMPANION_LOCK", path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let output = child.stdout.take().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in std::io::BufReader::new(output)
                    .lines()
                    .map_while(Result::ok)
                {
                    if line.contains("RING_LOCK_READY") {
                        let _ = tx.send(());
                        break;
                    }
                }
            });
            let held = Self(child);
            assert!(
                rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
                "owned lock helper did not become ready"
            );
            reader.join().unwrap();
            held
        }
    }
    impl Drop for ExternalLock {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn external_ring_lock_bounds_request_and_endpoint_shutdown_without_late_workers() {
        use xshell_protocol::ring::companion::CompanionRequest;
        let home = tempfile::Builder::new()
            .prefix("r1-lock-")
            .tempdir()
            .unwrap();
        let paths = xshelld::paths::resolve(home.path(), None, None);
        let (admit, admitted) = std::sync::mpsc::channel();
        let mut config = Config::new(home.path().into(), paths.clone());
        config.test_hook = Some(xshelld::server::TestHook(Arc::new(move |_, point| {
            if point == xshelld::server::TestPoint::CompanionRequestAdmitted {
                let _ = admit.send(());
            }
            false
        })));
        let server = Server::start(config).unwrap();
        let endpoint = server.companion_endpoint().unwrap();
        let holder = ExternalLock::start(&paths.ring_dir.join("ring.lock"));
        let client = CompanionClient::new(endpoint.clone());
        let started = std::time::Instant::now();
        assert!(client.request(CompanionRequest::Descriptor).is_err());
        let client_bounded = started.elapsed() < std::time::Duration::from_millis(6500);
        admitted
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let pending = CompanionClient::new(endpoint.clone());
        let pending = std::thread::spawn(move || pending.request(CompanionRequest::Descriptor));
        // Stop a second admitted request while it still has almost its entire budget,
        // rather than proving shutdown only after every request has already timed out.
        admitted
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let stopper = server.stopper();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            stopper.shutdown();
            let _ = tx.send(());
        });
        let drained =
            rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok() && !endpoint.exists();
        let no_early_write = !paths.ring_dir.join("keys.json").exists();
        // Cleanup even on the original failure; never leave a blocked test thread/process.
        drop(holder);
        stop.join().unwrap();
        let pending_refused = pending.join().unwrap().is_err();
        drop(server);
        assert!(client_bounded && pending_refused && drained && no_early_write && !paths.ring_dir.join("keys.json").exists(),
            "request and endpoint shutdown must terminate before external lock release, without detached or late-writing workers");
    }

    #[test]
    fn host_join_paused_before_commit_cannot_cross_successful_registration() {
        use xshell_protocol::ring::companion::{CompanionRequest, CompanionResponse};
        use xshell_protocol::ring::{DeviceKeys, Member, Role, SignedRoster};
        use xshelld::server::{TestHook, TestPoint};
        let relay = Relay::start();
        let (entered, reached) = std::sync::mpsc::channel();
        let (release, go) = std::sync::mpsc::channel();
        let go = std::sync::Mutex::new(go);
        let hook = TestHook(Arc::new(move |_, point| {
            if point == TestPoint::BeforeHostRingJoinCommit {
                entered.send(()).unwrap();
                go.lock().unwrap().recv().unwrap();
            }
            false
        }));
        let computer = Computer::with_hook(&relay, Some(hook));
        let client = computer.companion();
        let CompanionResponse::Identity { identity, .. } =
            client.request(CompanionRequest::Descriptor).unwrap()
        else {
            panic!("no identity");
        };
        // Historical v1 negative, never the public v2 bootstrap fixture.
        let other = DeviceKeys::generate().unwrap();
        let genesis =
            SignedRoster::genesis(&other, other.noise_key(), "other", &relay.url(), 1).unwrap();
        let next = genesis
            .next(&other, 2, |draft| {
                draft.add(Member::new(
                    "daemon",
                    Role::Daemon,
                    identity.sign_key,
                    identity.noise_key,
                    2,
                ))
            })
            .unwrap();
        let result = std::thread::scope(|scope| {
            let request = scope.spawn(|| {
                host_request(
                    &computer.daemon,
                    xshell_protocol::msg::ClientMsg::RingJoin {
                        rosters: vec![genesis.token().into(), next.token().into()],
                        expect: None,
                    },
                )
            });
            reached
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let registration = computer.ring.register_companion(&client);
            release.send(()).unwrap();
            registration.unwrap();
            request.join().unwrap()
        });
        assert!(
            result
                .err()
                .is_some_and(|message| message.contains("protected_counterpart_required")),
            "Host commit must recheck binding under ring.lock"
        );
        assert!(!computer.daemon_dir.join("roster.json").exists());
        assert!(host_request(
            &computer.daemon,
            xshell_protocol::msg::ClientMsg::RingIdentity
        )
        .unwrap()["ring"]
            .is_null());
        assert_eq!(
            relay_version(&relay.target(), genesis.ring_id()),
            0,
            "a refused Host commit must not stage a standalone Ring through a Connector"
        );
    }

    #[test]
    fn actual_roster_write_error_never_produces_an_installed_receipt() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use xshelld::server::{TestHook, TestPoint};
        let relay = Relay::start();
        let hit = Arc::new(AtomicBool::new(false));
        let recorded = hit.clone();
        let hook = TestHook(Arc::new(move |_, point| {
            if point == TestPoint::BeforeRingRosterWrite {
                recorded.store(true, Ordering::SeqCst);
                true
            } else {
                false
            }
        }));
        let computer = Computer::with_hook(&relay, Some(hook));
        let client = computer.companion();
        computer.ring.register_companion(&client).unwrap();
        let result = computer.ring.create_computer(&client, |preview, lease| {
            Ok(CreateDecision::Confirm(
                lease.confirm(&preview.proposal_hash)?,
            ))
        });
        let error = result
            .err()
            .expect("actual roster write fault must report reconciliation, not success");
        assert!(
            error.starts_with("reconciliation_required:") && error.contains("could not be saved"),
            "{error}"
        );
        assert!(
            hit.load(Ordering::SeqCst),
            "actual post-read roster write hook was not exercised"
        );
        let in_memory = host_request(
            &computer.daemon,
            xshell_protocol::msg::ClientMsg::RingIdentity,
        )
        .unwrap();
        assert_eq!(
            in_memory["ring"]["version"], 1,
            "Saved.write_error keeps verified accepted membership in memory, not on disk"
        );
        assert!(!computer.daemon_dir.join("roster.json").exists());
        let daemon: serde_json::Value = serde_json::from_slice(
            &std::fs::read(computer.daemon_dir.join("companion.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(daemon["obligation"]["phase"], "accepted");
        let journal = Store::new(computer.desktop.clone())
            .load_profile()
            .unwrap()
            .0
            .unwrap()
            .journal
            .unwrap();
        assert_eq!(journal.phase, AdoptionPhase::Accepted);
        assert!(!journal.desktop_installed && !journal.daemon_installed);
        assert!(computer
            .ring
            .create_computer(&client, |_, _| panic!("a failed write is not a new Create"))
            .is_err());
    }

    #[test]
    fn outstanding_legacy_offer_refuses_conflicting_registration_until_it_closes() {
        use std::io::Write;
        use xshell_protocol::ring::pairing::{JoinRequest, Joined, PairCode, PairRefusal};
        use xshell_protocol::ring::relay::pair::{
            host_pairing, HostRequest, PairOptions, PairPipe, PairingHost,
        };
        use xshell_protocol::ring::{DeviceKeys, RingId, Role};
        struct Output {
            line: Vec<u8>,
            code: std::sync::mpsc::Sender<String>,
        }
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                for byte in bytes {
                    if *byte == b'\n' {
                        if let Ok(line) = std::str::from_utf8(&self.line) {
                            if let Some(code) = line.strip_prefix("Pairing code: ") {
                                let _ = self.code.send(code.into());
                            }
                        }
                        self.line.clear();
                    } else {
                        self.line.push(*byte);
                    }
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct Refuse;
        impl PairingHost for Refuse {
            fn consume(&self) -> Result<(), PairRefusal> {
                Err(PairRefusal::PublishFailed)
            }
            fn add(&self, _: &JoinRequest) -> Result<Joined, PairRefusal> {
                unreachable!("negative legacy offer is never enrolled")
            }
        }
        let relay = Relay::start();
        let computer = Computer::new(&relay);
        let client = computer.companion();
        let paths = xshelld::paths::resolve(computer._home.path(), None, None);
        let options = xshelld::cli::Opts {
            relay: Some(relay.url()),
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let pairing = std::thread::spawn(move || {
            xshelld::pair::run_pair_to(
                &options,
                &paths,
                &mut Output {
                    line: Vec::new(),
                    code: tx,
                },
            )
        });
        let text = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let code = PairCode::parse(&text).unwrap();
        let registration = computer.ring.register_companion(&client);
        let bound_while_offer_open = computer.daemon_dir.join("companion.json").exists();
        // Terminate the already-issued offer through the real legacy Noise refusal path;
        // no global TTL changes, secrets in logs, sleep-based ordering or detached thread.
        let secret = code.secret();
        let mut pipe =
            PairPipe::open(&relay.url(), &secret.slot(), &PairOptions::default()).unwrap();
        pipe.wait_peer(
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            None,
        )
        .unwrap();
        let host = DeviceKeys::generate().unwrap();
        let ring = RingId::derive(&host.sign_key());
        assert!(host_pairing(
            pipe,
            &HostRequest {
                keys: &host,
                secret: &secret,
                ring_id: &ring,
                name: "legacy negative",
                role: Role::Daemon
            },
            &Refuse,
            None
        )
        .is_err());
        assert_ne!(pairing.join().unwrap(), 0);
        assert!(
            registration
                .err()
                .is_some_and(|message| message.contains("legacy_offer_active")),
            "registration must not become effective while a legacy offer can commit"
        );
        assert!(
            !bound_while_offer_open,
            "conflicting registration must not become effective before the offer lease closes"
        );
        assert!(!computer.daemon_dir.join("roster.json").exists());
        computer.ring.register_companion(&client).unwrap();
    }
    #[test]
    fn unsupported_long_path_does_not_ignore_occupied_endpoint() {
        let temp = tempfile::Builder::new()
            .prefix("r1-path-")
            .tempdir()
            .unwrap();
        let home = temp.path().join("h".repeat(110));
        std::fs::create_dir(&home).unwrap();
        let paths = xshelld::paths::resolve(&home, Some(&temp.path().join("run")), None);
        std::fs::create_dir_all(&paths.ring_dir).unwrap();
        let endpoint = paths.ring_dir.join("companion.sock");
        std::fs::write(&endpoint, b"not a socket").unwrap();
        assert!(Server::start(Config::new(home, paths)).is_err());
        assert_eq!(std::fs::read(endpoint).unwrap(), b"not a socket");
    }

    #[test]
    fn unsupported_long_path_keeps_retained_registration_downgrade_fences() {
        let temp = tempfile::Builder::new()
            .prefix("r1-path-")
            .tempdir()
            .unwrap();
        let home = temp.path().join("h".repeat(110));
        std::fs::create_dir(&home).unwrap();
        let paths = xshelld::paths::resolve(&home, Some(&temp.path().join("run")), None);
        std::fs::create_dir_all(&paths.ring_dir).unwrap();
        std::fs::write(
            paths.ring_dir.join("companion.json"),
            b"retained unreadable registration",
        )
        .unwrap();
        let server = Server::start(Config::new(home, paths.clone())).unwrap();
        assert_eq!(
            server.companion_endpoint().unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert!(host_request(&server, xshell_protocol::msg::ClientMsg::RingIdentity).is_ok());
        let error = host_request(
            &server,
            xshell_protocol::msg::ClientMsg::RingJoin {
                rosters: vec![],
                expect: None,
            },
        )
        .unwrap_err();
        assert!(error.starts_with("protected_counterpart_required:"));
        let mut output = Vec::new();
        assert_ne!(
            xshelld::pair::run_pair_to(&xshelld::cli::Opts::default(), &paths, &mut output),
            0
        );
        assert!(output.is_empty() && !paths.ring_dir.join("roster.json").exists());
        server.shutdown();
    }
}

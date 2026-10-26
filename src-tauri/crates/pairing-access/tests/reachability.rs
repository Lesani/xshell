use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use xshell_pairing_access::{now, tls, Controller, HelperInput, HelperOutput};
use xshell_protocol::pairing_access::*;
use xshell_protocol::ring::{DeviceKeys, RingId};
fn controller() -> Controller {
    let keys = DeviceKeys::generate().unwrap();
    Controller::open(
        Scope {
            ring: RingId::derive(&keys.sign_key()),
            approver: keys.sign_key(),
            role: MobileRole::Mobile,
            purpose: ReachabilityPurpose::Reachability,
            enrollment: None,
        },
        "test-controller".into(),
        xshell_pairing_access::now() + 60,
    )
    .unwrap()
}
fn server(c: &Controller) -> ServerDescriptor {
    ServerDescriptor {
        version: VERSION,
        task: c.proposal().task,
        certificate: tls::Identity::generate(true, c.proposal().expires_at)
            .unwrap()
            .certificate,
        controller_fingerprint: c.controller_fingerprint(),
        scope: c.proposal().scope.clone(),
        endpoint: "127.0.0.1:47839".parse().unwrap(),
        expires_at: c.proposal().expires_at,
        recipient: None,
    }
}
#[test]
fn pairing_access_import_is_not_verification_and_all_scope_metadata_is_bound() {
    let mut c = controller();
    let descriptor = server(&c);
    c.import_server(descriptor.clone()).unwrap();
    assert!(matches!(c.connect(), Err(AccessError::BootstrapUnverified)));
    let mut decision = VerificationDecision {
        controller_fingerprint: c.controller_fingerprint(),
        server_fingerprint: tls::fingerprint(&descriptor.certificate),
        endpoint: descriptor.endpoint,
        descriptor: None,
    };
    decision.endpoint.set_port(12345);
    assert_eq!(c.verify(decision), Err(AccessError::BootstrapUnverified));
    for field in 0..9 {
        let mut changed = descriptor.clone();
        match field {
            0 => changed.version += 1,
            1 => changed.task = uuid::Uuid::new_v4(),
            2 => changed.controller_fingerprint[0] ^= 1,
            3 => changed.scope.ring = server(&controller()).scope.ring,
            4 => changed.scope.approver = DeviceKeys::generate().unwrap().sign_key(),
            5 => changed.expires_at += 1,
            6 => changed.endpoint = "0.0.0.0:1234".parse().unwrap(),
            7 => changed.endpoint = "224.0.0.1:1234".parse().unwrap(),
            _ => changed.endpoint.set_port(0),
        }
        assert!(c.import_server(changed).is_err());
        assert!(matches!(c.connect(), Err(AccessError::BootstrapUnverified)));
    }
    // The enums themselves reject broadened role/purpose instead of silently ignoring them.
    let mut value = serde_json::to_value(&descriptor).unwrap();
    value["scope"]["role"] = "desktop".into();
    assert!(serde_json::from_value::<ServerDescriptor>(value).is_err());
    let mut value = serde_json::to_value(&descriptor).unwrap();
    value["scope"]["purpose"] = "enrollment".into();
    assert!(serde_json::from_value::<ServerDescriptor>(value).is_err());
}
#[test]
fn pairing_access_helper_protected_stream_dispatch_and_redacted_output() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xshell-pairing-access"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let c = controller();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    write_frame(
        &mut input,
        &HelperInput::Open {
            scope: c.proposal().scope.clone(),
            label: "test-controller".into(),
            expires_at: c.proposal().expires_at,
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame::<HelperOutput>(&mut output).unwrap(),
        HelperOutput::Proposal { .. }
    ));
    drop(input);
    assert!(child.wait().unwrap().success());
    let help = Command::new(env!("CARGO_BIN_EXE_xshell-pairing-access"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert_eq!(
        String::from_utf8(help.stdout).unwrap(),
        xshell_pairing_access::HELP
    );
    let rejected = Command::new(env!("CARGO_BIN_EXE_xshell-pairing-access"))
        .arg("--descriptor=not-accepted")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert_eq!(rejected.stderr, b"invalid-request\n");
    let file_input = Command::new(env!("CARGO_BIN_EXE_xshell-pairing-access"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!file_input.status.success());
    assert_eq!(file_input.stderr, b"invalid-request\n");
}
#[test]
fn pairing_access_helper_rejects_oversized_duplicate_and_unknown_commands() {
    for data in [
        vec![0xff; 4],
        {
            let body = br#"{"method":"verify","decision":{},"decision":{}}"#;
            let mut data = (body.len() as u32).to_be_bytes().to_vec();
            data.extend_from_slice(body);
            data
        },
        {
            let body = br#"{"method":"start"}"#;
            let mut data = (body.len() as u32).to_be_bytes().to_vec();
            data.extend_from_slice(body);
            data
        },
    ] {
        let mut output = Vec::new();
        assert!(xshell_pairing_access::run(&mut &data[..], &mut output).is_err());
        assert!(output.is_empty());
    }
}

#[test]
fn pairing_access_helper_actual_tls_connect_deadline_rejects_dripfeed() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = listener.local_addr().unwrap();
    let server = tls::Identity::generate(true, xshell_pairing_access::now() + 60).unwrap();
    let client = tls::Identity::generate(false, xshell_pairing_access::now() + 60).unwrap();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut hello = [0; 4096];
        assert!(socket.read(&mut hello).unwrap() > 0);
        for byte in [0x16, 0x03, 0x03, 0, 0x20, 0x02, 0, 0, 0x1c]
            .into_iter()
            .cycle()
            .take(20)
        {
            if socket.write_all(&[byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    let start = Instant::now();
    assert!(tls::connect(
        endpoint,
        tls::client_config(&client, &server.certificate).unwrap(),
        &server.certificate
    )
    .is_err());
    assert!(start.elapsed() < Duration::from_millis(3600));
    worker.join().unwrap();
}

#[cfg(unix)]
#[test]
fn pairing_access_helper_rejects_inherited_tcp_stdio() {
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::OwnedFd;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let fd: OwnedFd = socket.into();
    let rejected = Command::new(env!("CARGO_BIN_EXE_xshell-pairing-access"))
        .stdin(Stdio::from(fd))
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert_eq!(rejected.stderr, b"invalid-request\n");
}

#[test]
fn pairing_access_controller_rejects_wrong_same_key_and_excess_chain_servers() {
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Instant;
    use zeroize::Zeroizing;

    for scenario in [
        "exact-server",
        "wrong-server",
        "same-key-different-certificate",
        "excessive-server-chain",
    ] {
        let mut controller = controller();
        let key = rcgen::KeyPair::generate().unwrap();
        let generated_at = time::OffsetDateTime::now_utc();
        let make_certificate = |key: &rcgen::KeyPair, offset| {
            let mut params = rcgen::CertificateParams::new(vec![SERVER_NAME.into()]).unwrap();
            params.not_before = generated_at - time::Duration::seconds(offset);
            params.not_after =
                time::OffsetDateTime::from_unix_timestamp(controller.proposal().expires_at as i64)
                    .unwrap();
            params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
            Certificate(params.self_signed(key).unwrap().der().to_vec())
        };
        let actual = make_certificate(&key, 30);
        let pinned = match scenario {
            "wrong-server" => make_certificate(&rcgen::KeyPair::generate().unwrap(), 30),
            "same-key-different-certificate" => make_certificate(&key, 29),
            _ => actual.clone(),
        };
        if matches!(scenario, "wrong-server" | "same-key-different-certificate") {
            assert_ne!(actual, pinned);
        }
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(
                controller.proposal().certificate.0.clone(),
            ))
            .unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .unwrap();
        let der = Zeroizing::new(key.serialize_der());
        let private = PrivatePkcs8KeyDer::from(der.as_slice()).into();
        let signer = rustls::crypto::ring::sign::any_supported_type(&private).unwrap();
        let mut chain = vec![CertificateDer::from(actual.0.clone())];
        if scenario == "excessive-server-chain" {
            chain.push(CertificateDer::from(actual.0.clone()));
        }
        let certified = rustls::sign::CertifiedKey::new(chain, signer);
        certified.keys_match().unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified)));
        config.send_tls13_tickets = 0;
        config.max_early_data_size = 0;
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap();
        let descriptor = ServerDescriptor {
            version: VERSION,
            task: controller.proposal().task,
            certificate: pinned,
            controller_fingerprint: controller.controller_fingerprint(),
            scope: controller.proposal().scope.clone(),
            endpoint,
            expires_at: controller.proposal().expires_at,
            recipient: None,
        };
        controller.import_server(descriptor.clone()).unwrap();
        controller
            .verify(VerificationDecision {
                controller_fingerprint: controller.controller_fingerprint(),
                server_fingerprint: tls::fingerprint(&descriptor.certificate),
                endpoint,
                descriptor: None,
            })
            .unwrap();
        let worker = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket.set_nodelay(true).unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, tls::DeadlineSocket::new(socket));
            while stream.conn.is_handshaking() {
                if stream.conn.complete_io(&mut stream.sock).is_err() {
                    return false;
                }
            }
            stream.sock.deadline(Instant::now() + tls::READ);
            let mut application_byte = [0];
            let received = matches!(stream.read(&mut application_byte), Ok(n) if n > 0);
            if received {
                let _ = write_frame(
                    &mut stream,
                    &Response {
                        phase: TaskPhase::Authorized,
                        accepted_sequence: 1,
                        expires_at: descriptor.expires_at,
                        outcome: Outcome::Status {},
                    },
                );
            }
            received
        });
        let request = Request {
            task: controller.proposal().task,
            request: uuid::Uuid::new_v4(),
            sequence: 1,
            scope: controller.proposal().scope.clone(),
            command: xshell_protocol::pairing_access::Command::Status {},
        };
        let result = controller.exchange(&request);
        let received = worker.join().unwrap();
        if scenario == "exact-server" {
            assert_eq!(result.unwrap().outcome, Outcome::Status {});
            assert!(
                received,
                "positive TLS fixture control must receive a command"
            );
        } else {
            assert_eq!(result, Err(AccessError::Unauthorized), "{scenario}");
            assert!(!received, "helper sent application data to {scenario}");
        }
    }
}

#[test]
fn enrollment_verification_binds_full_descriptor_and_recipient_not_only_fingerprints() {
    use xshell_protocol::pairing_access::enrollment::*;
    use xshell_protocol::ring::DeviceKeys;
    let desktop = DeviceKeys::from_seeds(&[41; 32], &[42; 32]);
    let mobile = DeviceKeys::from_seeds(&[43; 32], &[44; 32]);
    let scope = Scope {
        ring: xshell_protocol::ring::RingId::derive(&desktop.sign_key()),
        approver: desktop.sign_key(),
        role: MobileRole::Mobile,
        purpose: ReachabilityPurpose::MobileEnrollment,
        enrollment: Some(EnrollmentTarget {
            approver_noise: desktop.noise_key(),
            relay: "wss://relay.invalid".into(),
            device_name: "phone".into(),
            initial_head: ChainHead {
                version: 1,
                hash: xshell_protocol::ring::b64::encode(&[7; 32]),
            },
        }),
    };
    let mut controller =
        Controller::open(scope.clone(), "fixture controller".into(), now() + 60).unwrap();
    let identity = tls::Identity::generate(true, controller.proposal().expires_at).unwrap();
    let descriptor = ServerDescriptor {
        version: VERSION,
        task: controller.proposal().task,
        certificate: identity.certificate,
        controller_fingerprint: controller.controller_fingerprint(),
        scope,
        endpoint: "127.0.0.1:47839".parse().unwrap(),
        expires_at: controller.proposal().expires_at,
        recipient: Some(RecipientIdentity {
            signing: mobile.sign_key(),
            noise: mobile.noise_key(),
        }),
    };
    controller.import_server(descriptor.clone()).unwrap();
    let mut decision = VerificationDecision {
        controller_fingerprint: descriptor.controller_fingerprint,
        server_fingerprint: tls::fingerprint(&descriptor.certificate),
        endpoint: descriptor.endpoint,
        descriptor: Some(descriptor.clone()),
    };
    let other = DeviceKeys::from_seeds(&[45; 32], &[46; 32]);
    decision
        .descriptor
        .as_mut()
        .unwrap()
        .recipient
        .as_mut()
        .unwrap()
        .noise = other.noise_key();
    assert_eq!(
        controller.verify(decision),
        Err(AccessError::BootstrapUnverified)
    );
    controller
        .verify(VerificationDecision {
            controller_fingerprint: descriptor.controller_fingerprint,
            server_fingerprint: tls::fingerprint(&descriptor.certificate),
            endpoint: descriptor.endpoint,
            descriptor: Some(descriptor),
        })
        .unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn desktop_private_ipc_recovers_stale_socket_rejects_permissions_and_symlinks_without_creating_ring(
) {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::Arc;
    use xshell_hostlink::ring::{DesktopRing, DesktopRingConfig, RingObserver, RingView};
    use xshell_protocol::pairing_access::enrollment::*;
    struct Quiet;
    impl RingObserver for Quiet {
        fn changed(&self, _: &RingView) {}
    }
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join(".xshell");
    let dir = parent.join("pairing");
    xshell_hostlink::ring::store::ensure_private_dir(&dir).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("desktop.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let desktop = DesktopRing::open(
        DesktopRingConfig::new(root.path().join("ring"), "fixture".into()),
        Arc::new(Quiet),
    );
    let server = xshell_pairing_access::local::serve(desktop.clone(), path.clone()).unwrap();
    let keys = xshell_protocol::ring::DeviceKeys::from_seeds(&[51; 32], &[52; 32]);
    let request = DesktopRequest::Scope {
        ring: xshell_protocol::ring::RingId::derive(&keys.sign_key()),
        device_name: "phone".into(),
    };
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(DesktopResponse::Error {
            error: AccessError::MissingRing,
        }) = xshell_pairing_access::local::exchange(&path, &request)
        {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(desktop.chain().is_none());
    assert!(!root.path().join("ring/ring.json").exists());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(matches!(
        xshell_pairing_access::local::exchange(&path, &request),
        Err(AccessError::PermissionRequired)
    ));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let moved = dir.join("saved.sock");
    std::fs::rename(&path, &moved).unwrap();
    symlink(&moved, &path).unwrap();
    assert!(matches!(
        xshell_pairing_access::local::exchange(&path, &request),
        Err(AccessError::PermissionRequired)
    ));
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&moved, &path).unwrap();
    drop(server);
    desktop.quit();
}

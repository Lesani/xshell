#![cfg(feature = "ring")]
use xshell_protocol::ring::pairing::{Guest, Host, PairSecret};
use xshell_protocol::ring::{DeviceKeys, RingId, Role};
#[test]
fn final_noise_context_and_monotonic_nonce_replay() {
    let a = DeviceKeys::generate().unwrap();
    let b = DeviceKeys::generate().unwrap();
    let secret = PairSecret::generate().unwrap();
    let (mut guest, m1) = Guest::start(&b, &secret, None).unwrap();
    let mut host = Host::start(&a, &secret).unwrap();
    host.read_start(&m1).unwrap();
    let m2 = host
        .hello(&RingId::derive(&a.sign_key()), "approver")
        .unwrap();
    guest.read_hello(&m2).unwrap();
    assert!(guest.final_handshake_hash().is_err());
    let m3 = guest.join(&b, Role::Daemon, "guest").unwrap();
    host.read_join(&m3).unwrap();
    assert_eq!(
        guest.final_handshake_hash().unwrap(),
        host.final_handshake_hash().unwrap()
    );
    let handle = host.encrypt(b"proposal handle").unwrap();
    assert_eq!(&*guest.decrypt(&handle).unwrap(), b"proposal handle");
    assert!(guest.decrypt(&handle).is_err());
    let consent = guest.encrypt(b"explicit consent").unwrap();
    assert_eq!(&*host.decrypt(&consent).unwrap(), b"explicit consent");
    let ack = host.encrypt(b"commit acknowledgment").unwrap();
    assert_ne!(ack, handle);
    assert_eq!(&*guest.decrypt(&ack).unwrap(), b"commit acknowledgment");
}

#[test]
fn computer_requires_independent_counterpart_possession_and_cross_flow_replays_fail() {
    use xshell_protocol::ring::b64;
    use xshell_protocol::ring::pairing::adoption::{
        AdoptionIntent, AdoptionSigners, JoinTarget, PublicIdentity,
    };
    let a = DeviceKeys::generate().unwrap();
    let desktop = DeviceKeys::generate().unwrap();
    let daemon = DeviceKeys::generate().unwrap();
    let public = |k: &DeviceKeys| PublicIdentity {
        sign_key: k.sign_key(),
        noise_key: k.noise_key(),
        name: "new computer".into(),
    };
    let intent = AdoptionIntent {
        operation: b64::encode(&[9; 32]),
        expires_at: 300,
        target: JoinTarget::Computer {
            desktop: public(&desktop),
            daemon: public(&daemon),
        },
    };
    let secret = PairSecret::generate().unwrap();
    let start = || {
        let (mut g, m1) = Guest::start(&desktop, &secret, None).unwrap();
        let mut h = Host::start(&a, &secret).unwrap();
        h.read_start(&m1).unwrap();
        let m2 = h.hello(&RingId::derive(&a.sign_key()), "approver").unwrap();
        g.read_hello(&m2).unwrap();
        (g, h)
    };
    let (mut no_companion, _) = start();
    assert!(no_companion
        .join_adoption(
            &desktop,
            &intent,
            &AdoptionSigners {
                principal: &desktop,
                companion: None
            }
        )
        .is_err());
    let (mut guest, mut host) = start();
    let m3 = guest
        .join_adoption(
            &desktop,
            &intent,
            &AdoptionSigners {
                principal: &desktop,
                companion: Some(&daemon),
            },
        )
        .unwrap();
    assert_eq!(host.read_adoption(&m3).unwrap(), intent);
    let (_, mut another) = start();
    assert!(another.read_adoption(&m3).is_err());
    let hash = host.final_handshake_hash().unwrap();
    let (mut second, mut second_host) = start();
    let m3 = second
        .join_adoption(
            &desktop,
            &intent,
            &AdoptionSigners {
                principal: &desktop,
                companion: Some(&daemon),
            },
        )
        .unwrap();
    second_host.read_adoption(&m3).unwrap();
    assert_ne!(hash, second_host.final_handshake_hash().unwrap());
    let encrypted = host.encrypt(b"first operation descriptor").unwrap();
    assert!(second.decrypt(&encrypted).is_err());
}

//! The Roster and its signature chain: what a device accepts and what it refuses.
#![allow(clippy::cloned_ref_to_slice_refs)] // `&[x.clone()]` reads better in tests

use serde_json::{json, Value};
use xshell_protocol::ring::b64;
use xshell_protocol::ring::roster::MAX_ROSTER_TOKEN;
use xshell_protocol::ring::{
    verify_genesis, verify_successor, DeviceKeys, Member, RingError, RingId, Role, Roster,
    RosterChain, RosterError, SignedRoster, Signer, ROSTER_CONTEXT,
};

const RELAY: &str = "wss://relay.example.com";
const T0: u64 = 1_760_000_000;

fn keys(n: u8) -> DeviceKeys {
    DeviceKeys::from_seeds(&[n; 32], &[n.wrapping_add(100); 32])
}

fn member(k: &DeviceKeys, name: &str, role: Role) -> Member {
    Member::new(name, role, k.sign_key(), k.noise_key(), T0)
}

/// v1 by `a` (desktop); v2 adds `b` (desktop), `d` (daemon) and `m` (mobile).
struct Fixture {
    a: DeviceKeys,
    b: DeviceKeys,
    d: DeviceKeys,
    m: DeviceKeys,
    v1: SignedRoster,
    v2: SignedRoster,
}

fn fixture() -> Fixture {
    let (a, b, d, m) = (keys(1), keys(2), keys(3), keys(4));
    let v1 = SignedRoster::genesis(&a, a.noise_key(), "laptop", RELAY, T0).unwrap();
    let v2 = v1
        .next(&a, T0 + 1, |r| {
            r.add(member(&b, "desktop-2", Role::Desktop));
            r.add(member(&d, "server", Role::Daemon));
            r.add(member(&m, "phone", Role::Mobile));
        })
        .unwrap();
    Fixture { a, b, d, m, v1, v2 }
}

/// A successor of `prev` signed by `signer` with no client-side checks, as an attacker
/// (or a confused device) would build it.
fn raw_next(
    prev: &SignedRoster,
    signer: &dyn Signer,
    edit: impl FnOnce(&mut Roster),
) -> SignedRoster {
    let mut r = prev.roster().clone();
    r.version += 1;
    r.prev = Some(prev.hash());
    r.signed_by = signer.sign_key();
    r.issued_at += 1;
    edit(&mut r);
    r.sign(signer).unwrap()
}

/// Re-signs an arbitrary payload JSON as `signer`.
fn sign_json(payload: &Value, signer: &dyn Signer) -> String {
    let part = b64::encode(payload.to_string().as_bytes());
    let mut msg = ROSTER_CONTEXT.as_bytes().to_vec();
    msg.extend_from_slice(part.as_bytes());
    let sig = signer.sign(&msg).unwrap();
    format!("xro1.{part}.{}", b64::encode(&sig))
}

fn payload_of(token: &str) -> Value {
    let part = token
        .strip_prefix("xro1.")
        .unwrap()
        .split('.')
        .next()
        .unwrap();
    serde_json::from_slice(&b64::decode(part).unwrap()).unwrap()
}

#[test]
fn ring_id_is_derived_from_creator_key_and_matches_gateway_regex() {
    let f = fixture();
    let id = RingId::derive(&f.a.sign_key());
    assert_eq!(f.v1.ring_id(), &id);
    assert_eq!(id.as_str().len(), 43);
    assert!(id
        .as_str()
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    assert_eq!(RingId::parse(id.as_str()).unwrap(), id);
    assert_ne!(RingId::derive(&f.b.sign_key()), id);
}

#[test]
fn genesis_verifies_self_signed_by_ring_key() {
    let f = fixture();
    verify_genesis(&f.v1).unwrap();
    let chain = RosterChain::from_chain(vec![f.v1.clone()]).unwrap();
    assert_eq!(chain.head().version(), 1);
    assert_eq!(f.v1.roster().prev, None);
    assert_eq!(f.v1.member(&f.a.sign_key()).unwrap().role, Role::Desktop);
}

#[test]
fn genesis_signed_by_other_key_is_refused() {
    let f = fixture();
    // `b` signs a v1 claiming `a`'s Ring id.
    let mut r = f.v1.roster().clone();
    r.signed_by = f.b.sign_key();
    r.members.push(member(&f.b, "b", Role::Desktop));
    let forged = r.sign(&f.b).unwrap();
    assert_eq!(verify_genesis(&forged), Err(RosterError::RingMismatch));
    // `b`'s own Ring id, but `b` is not in it.
    let mut r = f.v1.roster().clone();
    r.signed_by = f.b.sign_key();
    r.ring_id = RingId::derive(&f.b.sign_key());
    let absent = r.sign(&f.b).unwrap();
    assert_eq!(verify_genesis(&absent), Err(RosterError::SignerNotMember));
    // A Mobile cannot create a Ring.
    let mut r = absent.roster().clone();
    r.members = vec![
        member(&f.b, "phone", Role::Mobile),
        member(&f.a, "a", Role::Desktop),
    ];
    let mobile = r.sign(&f.b).unwrap();
    assert_eq!(verify_genesis(&mobile), Err(RosterError::SignerNotDesktop));
    // And a chain cannot start anywhere but v1.
    assert_eq!(
        RosterChain::from_chain(vec![f.v2.clone()]),
        Err(RosterError::NotGenesis)
    );
}

#[test]
fn newer_roster_signed_by_desktop_is_accepted() {
    let f = fixture();
    let mut chain = RosterChain::from_chain(vec![f.v1.clone()]).unwrap();
    let got = chain.accept(&[f.v2.clone()]).unwrap();
    assert_eq!(got.added, 1);
    assert_eq!(chain.head(), &f.v2);
    // The second Desktop, added in v2, may sign v3.
    let v3 =
        f.v2.next(&f.b, T0 + 2, |r| {
            r.remove(&f.d.sign_key());
        })
        .unwrap();
    let got = chain.accept(&[v3.clone()]).unwrap();
    assert_eq!(got.removed, vec![f.d.sign_key()]);
    assert_eq!(chain.head(), &v3);
}

#[test]
fn older_roster_is_refused() {
    let f = fixture();
    let mut chain = RosterChain::from_chain(vec![f.v1.clone(), f.v2.clone()]).unwrap();
    // A different v1 (a rollback attempt) is refused …
    let mut r = f.v1.roster().clone();
    r.issued_at += 5;
    let other_v1 = r.sign(&f.a).unwrap();
    assert_eq!(chain.accept(&[other_v1]), Err(RosterError::Stale));
    // … the identical v1 changes nothing and is refused as stale too.
    assert_eq!(chain.accept(&[f.v1.clone()]), Err(RosterError::Stale));
    assert_eq!(chain.head(), &f.v2);
    assert_eq!(verify_successor(&f.v2, &f.v1), Err(RosterError::Stale));
}

#[test]
fn same_version_different_bytes_is_refused() {
    let f = fixture();
    let mut chain = RosterChain::from_chain(vec![f.v1.clone(), f.v2.clone()]).unwrap();
    let fork = raw_next(&f.v1, &f.a, |r| r.issued_at += 99);
    assert_eq!(fork.version(), 2);
    assert_eq!(chain.accept(&[fork]), Err(RosterError::PrevMismatch));
    assert_eq!(chain.head(), &f.v2);
}

#[test]
fn forged_signature_is_refused() {
    let f = fixture();
    let (head, _) = f.v2.token().rsplit_once('.').unwrap();
    let bad_sig = b64::encode(&[7u8; 64]);
    assert_eq!(
        SignedRoster::parse(&format!("{head}.{bad_sig}")),
        Err(RosterError::BadSignature)
    );
    // A real signature by a key other than `signedBy`.
    let mut p = payload_of(f.v2.token());
    p["issuedAt"] = json!(T0 + 50);
    let by_b = sign_json(&p, &f.b);
    assert_eq!(SignedRoster::parse(&by_b), Err(RosterError::BadSignature));
}

#[test]
fn tampered_payload_is_refused() {
    let f = fixture();
    let mut p = payload_of(f.v2.token());
    p["members"][3]["role"] = json!("desktop");
    let part = b64::encode(p.to_string().as_bytes());
    let sig = f.v2.token().rsplit_once('.').unwrap().1;
    assert_eq!(
        SignedRoster::parse(&format!("xro1.{part}.{sig}")),
        Err(RosterError::BadSignature)
    );
}

#[test]
fn mobile_signed_roster_is_refused() {
    let f = fixture();
    let v3 = raw_next(&f.v2, &f.m, |_| {});
    assert_eq!(
        verify_successor(&f.v2, &v3),
        Err(RosterError::SignerNotDesktop)
    );
    let mut chain = RosterChain::from_chain(vec![f.v1.clone(), f.v2.clone()]).unwrap();
    assert_eq!(chain.accept(&[v3]), Err(RosterError::SignerNotDesktop));
    // The builder refuses it before it reaches any Relay.
    assert_eq!(
        f.v2.next(&f.m, T0 + 3, |_| {}),
        Err(RingError::Roster(RosterError::SignerNotDesktop))
    );
}

#[test]
fn daemon_signed_roster_is_refused() {
    let f = fixture();
    let v3 = raw_next(&f.v2, &f.d, |_| {});
    assert_eq!(
        verify_successor(&f.v2, &v3),
        Err(RosterError::SignerNotDesktop)
    );
}

#[test]
fn non_member_signer_is_refused() {
    let f = fixture();
    let stranger = keys(50);
    let v3 = raw_next(&f.v2, &stranger, |r| {
        r.members.push(member(&stranger, "intruder", Role::Desktop));
    });
    assert_eq!(
        verify_successor(&f.v2, &v3),
        Err(RosterError::SignerNotMember)
    );
}

#[test]
fn removed_desktop_cannot_sign_next_version() {
    let f = fixture();
    let v3 =
        f.v2.next(&f.a, T0 + 2, |r| {
            r.remove(&f.b.sign_key());
        })
        .unwrap();
    let v4 = raw_next(&v3, &f.b, |_| {});
    assert_eq!(
        verify_successor(&v3, &v4),
        Err(RosterError::SignerNotMember)
    );
    // A signer may remove itself; it then signs nothing more.
    let v4 = v3
        .next(&f.a, T0 + 3, |r| {
            r.add(member(&f.b, "back", Role::Desktop));
            r.remove(&f.a.sign_key());
        })
        .unwrap();
    assert!(v4.member(&f.a.sign_key()).is_none());
    assert_eq!(
        v4.next(&f.a, T0 + 4, |_| {}),
        Err(RingError::Roster(RosterError::SignerNotMember))
    );
}

#[test]
fn gap_in_chain_is_refused() {
    let f = fixture();
    let v3 = raw_next(&f.v2, &f.a, |r| r.version += 1);
    assert_eq!(v3.version(), 4);
    assert_eq!(verify_successor(&f.v2, &v3), Err(RosterError::Gap));
    assert_eq!(
        RosterChain::from_chain(vec![f.v1.clone(), v3]),
        Err(RosterError::Gap)
    );
}

#[test]
fn full_chain_is_accepted_in_order() {
    let f = fixture();
    let v3 = f.v2.next(&f.b, T0 + 2, |_| {}).unwrap();
    let tokens = [f.v1.token(), f.v2.token(), v3.token()];
    let chain = RosterChain::from_tokens(&tokens).unwrap();
    assert_eq!(chain.head(), &v3);
    assert_eq!(chain.since(1).len(), 2);
    assert_eq!(chain.get(2), Some(&f.v2));
    // Out of order is not a chain.
    assert!(RosterChain::from_tokens(&[f.v1.token(), v3.token(), f.v2.token()]).is_err());
    // A device at v1 catches up on several versions at once, skipping what it has.
    let mut c = RosterChain::from_chain(vec![f.v1.clone()]).unwrap();
    assert_eq!(
        c.accept(&[f.v1.clone(), f.v2.clone(), v3.clone()])
            .unwrap()
            .added,
        2
    );
    assert_eq!(c, chain);
}

#[test]
fn prev_mismatch_is_refused() {
    let f = fixture();
    let v3 = raw_next(&f.v2, &f.a, |r| r.prev = Some(f.v1.hash()));
    assert_eq!(verify_successor(&f.v2, &v3), Err(RosterError::PrevMismatch));
}

#[test]
fn relay_url_change_is_a_new_version() {
    let f = fixture();
    let v3 =
        f.v2.next(&f.a, T0 + 2, |r| {
            r.relay_url = "wss://self.hosted.example:8443/relay".into()
        })
        .unwrap();
    assert_eq!(v3.version(), 3);
    assert_eq!(
        v3.roster().relay_url,
        "wss://self.hosted.example:8443/relay"
    );
    let mut c = RosterChain::from_chain(vec![f.v1.clone(), f.v2.clone()]).unwrap();
    c.accept(&[v3]).unwrap();
    // A plain-text Relay is refused unless it is loopback.
    assert!(matches!(
        c.head().next(&f.a, T0 + 3, |r| r.relay_url =
            "ws://relay.example.com".into()),
        Err(RingError::Roster(RosterError::Invalid(_)))
    ));
}

#[test]
fn duplicate_keys_or_no_desktop_is_invalid() {
    let f = fixture();
    let invalid = |e: Result<SignedRoster, RingError>| {
        matches!(e, Err(RingError::Roster(RosterError::Invalid(_))))
    };
    assert!(invalid(f.v2.next(&f.a, T0 + 2, |r| r.add(member(
        &f.b,
        "again",
        Role::Mobile
    )))));
    let other = keys(60);
    assert!(invalid(f.v2.next(&f.a, T0 + 2, |r| {
        r.add(Member::new(
            "same noise",
            Role::Mobile,
            other.sign_key(),
            f.m.noise_key(),
            T0,
        ))
    })));
    assert!(invalid(f.v2.next(&f.a, T0 + 2, |r| {
        r.remove(&f.a.sign_key());
        r.remove(&f.b.sign_key());
    })));
    assert!(invalid(f.v2.next(&f.a, T0 + 2, |r| r.members.clear())));
    for name in [
        "",
        "tab\there",
        "bidi\u{202E}",
        "zero\u{200B}width",
        &"x".repeat(65),
    ] {
        assert!(
            invalid(f.v2.next(&f.a, T0 + 2, |r| r.members[3].name = name.to_string())),
            "{name:?}"
        );
    }
    assert!(invalid(f.v2.next(&f.a, T0 + 2, |r| {
        for i in 0..70u8 {
            let k = keys(150u8.wrapping_add(i));
            r.add(member(&k, "many", Role::Mobile));
        }
    })));
    // Integers past 2^53-1, and a duplicate JSON key, are refused.
    let mut p = payload_of(f.v1.token());
    p["issuedAt"] = json!(1u64 << 53);
    assert!(matches!(
        SignedRoster::parse(&sign_json(&p, &f.a)),
        Err(RosterError::Invalid(_))
    ));
    let dup = payload_of(f.v1.token())
        .to_string()
        .replacen("{", "{\"version\":1,", 1);
    let part = b64::encode(dup.as_bytes());
    let mut msg = ROSTER_CONTEXT.as_bytes().to_vec();
    msg.extend_from_slice(part.as_bytes());
    let token = format!("xro1.{part}.{}", b64::encode(&f.a.sign(&msg).unwrap()));
    assert!(matches!(
        SignedRoster::parse(&token),
        Err(RosterError::Malformed(_))
    ));
}

#[test]
fn unknown_fields_survive_and_are_signed() {
    let f = fixture();
    let mut p = payload_of(f.v1.token());
    p["future"] = json!({"x": 1});
    p["members"][0]["avatar"] = json!("cat");
    let token = sign_json(&p, &f.a);
    let r = SignedRoster::parse(&token).unwrap();
    verify_genesis(&r).unwrap();
    assert_eq!(r.token(), token);
    assert_eq!(r.roster().extra["future"], json!({"x": 1}));
    // Re-signing carries them forward.
    let v2 = r.next(&f.a, T0 + 1, |_| {}).unwrap();
    assert_eq!(payload_of(v2.token())["future"], json!({"x": 1}));
    assert_eq!(payload_of(v2.token())["members"][0]["avatar"], json!("cat"));
    // Changing one breaks the signature.
    let mut p2 = payload_of(&token);
    p2["future"] = json!({"x": 2});
    let sig = token.rsplit_once('.').unwrap().1;
    let part = b64::encode(p2.to_string().as_bytes());
    assert_eq!(
        SignedRoster::parse(&format!("xro1.{part}.{sig}")),
        Err(RosterError::BadSignature)
    );
}

#[test]
fn oversize_token_is_refused() {
    let f = fixture();
    let mut p = payload_of(f.v1.token());
    p["pad"] = json!("x".repeat(MAX_ROSTER_TOKEN));
    assert_eq!(
        SignedRoster::parse(&sign_json(&p, &f.a)),
        Err(RosterError::TooLarge)
    );
}

#[test]
fn malformed_tokens_never_panic() {
    let f = fixture();
    let t = f.v1.token();
    let (payload, sig) = t.strip_prefix("xro1.").unwrap().split_once('.').unwrap();
    let not_json = b64::encode(b"not json");
    let array = b64::encode(b"[1,2]");
    let cases: Vec<String> = vec![
        String::new(),
        "xro1.".into(),
        "xro1..".into(),
        format!("xro2.{payload}.{sig}"),
        format!("xro1.{payload}"),
        format!("xro1.{payload}.{sig}.x"),
        format!("xro1.{payload}=.{sig}"),
        format!("xro1.{payload}.{sig}="),
        format!("xro1.{payload}.{}", &sig[..sig.len() - 1]),
        format!("xro1.{not_json}.{sig}"),
        format!("xro1.{array}.{sig}"),
        format!("XRO1.{payload}.{sig}"),
        format!(" xro1.{payload}.{sig}"),
        "xro1.\u{0}.\u{0}".into(),
    ];
    for c in &cases {
        assert!(
            matches!(SignedRoster::parse(c), Err(RosterError::Malformed(_))),
            "{c:?}"
        );
    }
    // Every single-character corruption is refused, never accepted or panicking.
    let bytes = t.as_bytes();
    for i in (0..bytes.len()).step_by(7) {
        let mut b = bytes.to_vec();
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        let s = String::from_utf8(b).unwrap();
        if s != t {
            assert!(
                SignedRoster::parse(&s).is_err(),
                "corruption at {i} accepted"
            );
        }
    }
    // Missing or mistyped fields.
    for (k, v) in [
        ("version", json!("1")),
        ("version", json!(1.0)),
        ("version", json!(-1)),
        ("version", json!(0)),
        ("v", json!(2)),
        ("members", json!({})),
        ("relayUrl", json!(5)),
        ("prev", json!("abc")),
        ("signedBy", json!("AAAA")),
        ("ringId", json!("short")),
    ] {
        let mut p = payload_of(t);
        p[k] = v.clone();
        assert!(
            SignedRoster::parse(&sign_json(&p, &f.a)).is_err(),
            "{k}={v}"
        );
    }
    for k in ["prev", "members", "ringId"] {
        let mut p = payload_of(t);
        p.as_object_mut().unwrap().remove(k);
        assert!(
            matches!(
                SignedRoster::parse(&sign_json(&p, &f.a)),
                Err(RosterError::Malformed(_))
            ),
            "missing {k}"
        );
    }
    let mut p = payload_of(t);
    p["members"][0]["role"] = json!("admin");
    assert!(SignedRoster::parse(&sign_json(&p, &f.a)).is_err());
    let mut p = payload_of(t);
    p["members"][0].as_object_mut().unwrap().remove("noiseKey");
    assert!(SignedRoster::parse(&sign_json(&p, &f.a)).is_err());
}

#[test]
fn failing_signer_propagates_as_sign_error() {
    struct Broken(xshell_protocol::ring::SignKey);
    impl Signer for Broken {
        fn sign_key(&self) -> xshell_protocol::ring::SignKey {
            self.0
        }
        fn sign(&self, _: &[u8]) -> Result<[u8; 64], xshell_protocol::ring::SignError> {
            Err(xshell_protocol::ring::SignError("keystore locked".into()))
        }
    }
    let f = fixture();
    let broken = Broken(f.a.sign_key());
    assert!(matches!(
        SignedRoster::genesis(&broken, f.a.noise_key(), "x", RELAY, T0),
        Err(RingError::Sign(_))
    ));
    assert!(matches!(
        f.v2.next(&broken, T0, |_| {}),
        Err(RingError::Sign(_))
    ));
    let _ = (&f.b, &f.d, &f.m);
}

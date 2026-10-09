//! Test vectors for other implementations (the Relay Worker): fixed seeds and timestamps, so
//! every file under `testdata/ring/` is reproducible (Ed25519 signatures are deterministic).
//! Regenerate with
//! `cargo test -p xshell-protocol --all-features --test ring_vectors -- --ignored bless_ring_vectors`.

use curve25519_dalek::edwards::CompressedEdwardsY;
use curve25519_dalek::scalar::{clamp_integer, Scalar};
use ed25519_dalek::Verifier as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha512};
use std::collections::BTreeMap;
use std::path::PathBuf;
use xshell_protocol::ring::entitlement::{
    entitlement_kid, sign_entitlement, verify_entitlement, GatewayKeys, Tier,
};
use xshell_protocol::ring::relay::wire::{
    auth_message, decode_client, decode_relay, ByeReason, ClientFrame, ErrorCode, Presence,
    RelayFrame, WireError,
};
use xshell_protocol::ring::url::RelayUrl;
use xshell_protocol::ring::{
    b64, verify, DeviceKeys, Member, NoiseKey, RingId, Role, Roster, RosterChain, SignKey,
    Signature, SignedRoster, Signer, ROSTER_CONTEXT,
};

const T0: u64 = 1_767_225_600; // 2026-01-01T00:00:00Z
const RELAY: &str = "wss://relay.example.com";

struct Dev {
    name: &'static str,
    role: Role,
    sign_seed: [u8; 32],
    noise_seed: [u8; 32],
}

const DEVS: &[Dev] = &[
    Dev {
        name: "desktop",
        role: Role::Desktop,
        sign_seed: [0x11; 32],
        noise_seed: [0x12; 32],
    },
    Dev {
        name: "desktop-2",
        role: Role::Desktop,
        sign_seed: [0x21; 32],
        noise_seed: [0x22; 32],
    },
    Dev {
        name: "daemon",
        role: Role::Daemon,
        sign_seed: [0x31; 32],
        noise_seed: [0x32; 32],
    },
    Dev {
        name: "mobile",
        role: Role::Mobile,
        sign_seed: [0x41; 32],
        noise_seed: [0x42; 32],
    },
    Dev {
        name: "stranger",
        role: Role::Desktop,
        sign_seed: [0x51; 32],
        noise_seed: [0x52; 32],
    },
];

fn dev(i: usize) -> DeviceKeys {
    DeviceKeys::from_seeds(&DEVS[i].sign_seed, &DEVS[i].noise_seed)
}

fn member(i: usize, at: u64) -> Member {
    let k = dev(i);
    Member::new(DEVS[i].name, DEVS[i].role, k.sign_key(), k.noise_key(), at)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// v1 by the desktop; v2 adds desktop-2, the daemon and the mobile; v3, signed by
/// desktop-2, removes the mobile and moves the Ring to another Relay.
fn chain() -> Vec<SignedRoster> {
    let a = dev(0);
    let v1 = SignedRoster::genesis(&a, a.noise_key(), "desktop", RELAY, T0).unwrap();
    let v2 = v1
        .next(&a, T0 + 60, |r| {
            r.add(member(1, T0 + 60));
            r.add(member(2, T0 + 60));
            r.add(member(3, T0 + 60));
        })
        .unwrap();
    let v3 = v2
        .next(&dev(1), T0 + 120, |r| {
            r.remove(&dev(3).sign_key());
            r.relay_url = "wss://relay.self-hosted.example:8443/xshell".into();
        })
        .unwrap();
    vec![v1, v2, v3]
}

/// The token of a version after `prev`, signed by `by` with no checks at all.
fn raw_next(prev: &SignedRoster, by: &dyn Signer, edit: impl FnOnce(&mut Roster)) -> String {
    let mut r = prev.roster().clone();
    r.version += 1;
    r.prev = Some(prev.hash());
    r.signed_by = by.sign_key();
    r.issued_at += 60;
    edit(&mut r);
    sign_raw(&serde_json::to_string(&r).unwrap(), by)
}

fn payload_of(token: &str) -> String {
    let part = token
        .strip_prefix("xro1.")
        .unwrap()
        .split('.')
        .next()
        .unwrap();
    String::from_utf8(b64::decode(part).unwrap()).unwrap()
}

/// A token over `payload` (any bytes) signed by `by`.
fn sign_raw(payload: &str, by: &dyn Signer) -> String {
    let part = b64::encode(payload.as_bytes());
    let mut msg = ROSTER_CONTEXT.as_bytes().to_vec();
    msg.extend_from_slice(part.as_bytes());
    format!("xro1.{part}.{}", b64::encode(&by.sign(&msg).unwrap()))
}

/// The eight Ed25519 points of small order, in their canonical encodings.
const SMALL_ORDER_SIGN_KEYS: [&str; 8] = [
    "0100000000000000000000000000000000000000000000000000000000000000",
    "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    "0000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000080",
    "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
    "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac03fa",
    "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
    "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc85",
];

/// p = 2^255 − 19, little-endian.
const P_LE: [u8; 32] = [
    0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
];

/// The group order L, little-endian.
const L_LE: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

fn unhex32(h: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// The encoding of y = p + `k` (a non-canonical y), with the sign bit `sign`.
fn y_plus_p(k: u8, sign: bool) -> [u8; 32] {
    let mut b = P_LE;
    b[0] += k;
    if sign {
        b[31] |= 0x80;
    }
    b
}

/// The same signature with S + L: the classic malleated copy.
fn malleate(sig: &[u8; 64]) -> [u8; 64] {
    let mut out = *sig;
    let mut carry = 0u16;
    for i in 0..32 {
        let v = out[32 + i] as u16 + L_LE[i] as u16 + carry;
        out[32 + i] = v as u8;
        carry = v >> 8;
    }
    out
}

/// A signature over `msg` by the key of `seed` whose R is the identity point (small order):
/// s = k·a, so the cofactorless equation sB = R + kA holds and a non-strict verifier accepts.
fn small_order_r(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    let a_key = DeviceKeys::from_seeds(seed, &[0; 32]).sign_key();
    let r = unhex32(SMALL_ORDER_SIGN_KEYS[0]);
    let mut h = Sha512::new();
    h.update(r);
    h.update(a_key.as_bytes());
    h.update(msg);
    let k = Scalar::from_bytes_mod_order_wide(&h.finalize().into());
    let expanded: [u8; 64] = Sha512::digest(seed).into();
    let mut lower = [0u8; 32];
    lower.copy_from_slice(&expanded[..32]);
    let a = Scalar::from_bytes_mod_order(clamp_integer(lower));
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&r);
    sig[32..].copy_from_slice((k * a).as_bytes());
    // Meaningful only if a non-strict verifier accepts it.
    let vk = ed25519_dalek::VerifyingKey::from_bytes(a_key.as_bytes()).unwrap();
    assert!(vk
        .verify(msg, &ed25519_dalek::Signature::from_bytes(&sig))
        .is_ok());
    sig
}

fn ed25519_json() -> Value {
    let mut keys = Vec::new();
    let mut key = |bytes: [u8; 32], ok: bool, why: &str| {
        assert_eq!(
            SignKey::from_bytes(bytes).is_ok(),
            ok,
            "{why}: {bytes:02x?}"
        );
        keys.push(json!({"key": b64::encode(&bytes), "ok": ok, "why": why}));
    };
    key(*dev(0).sign_key().as_bytes(), true, "an ordinary key");
    for h in SMALL_ORDER_SIGN_KEYS {
        let b = unhex32(h);
        let pt = CompressedEdwardsY(b).decompress().unwrap();
        assert!(pt.is_small_order() && pt.compress().0 == b);
        key(b, false, "small order");
    }
    // Every y >= p (y = p + k, k < 19), with either sign bit.
    for k in 0..19u8 {
        for sign in [false, true] {
            let b = y_plus_p(k, sign);
            match CompressedEdwardsY(b).decompress() {
                None => key(b, false, "non-canonical y, not a point"),
                Some(pt) if pt.is_small_order() => key(b, false, "non-canonical y, small order"),
                Some(_) => key(b, true, "non-canonical y, read mod p: accepted"),
            }
        }
    }
    let not_point = (2u8..)
        .map(|y| {
            let mut b = [0u8; 32];
            b[0] = y;
            b
        })
        .find(|b| CompressedEdwardsY(*b).decompress().is_none())
        .unwrap();
    key(not_point, false, "not a point");

    let seed = [0x71u8; 32];
    let k = DeviceKeys::from_seeds(&seed, &[0x72; 32]);
    let msg = b"xshell ed25519 vector";
    let good = k.sign(msg).unwrap();
    let identity = unhex32(SMALL_ORDER_SIGN_KEYS[0]);
    let mut base_r = [0u8; 64];
    base_r[..32]
        .copy_from_slice(curve25519_dalek::constants::ED25519_BASEPOINT_COMPRESSED.as_bytes());
    base_r[32] = 1;
    let sigs: Vec<(&str, [u8; 32], [u8; 64], bool)> = vec![
        ("valid", *k.sign_key().as_bytes(), good, true),
        ("s_plus_l", *k.sign_key().as_bytes(), malleate(&good), false),
        (
            "small_order_r",
            *k.sign_key().as_bytes(),
            small_order_r(&seed, msg),
            false,
        ),
        ("small_order_key", identity, base_r, false),
    ];
    // The identity key with R = B, s = 1 passes the cofactorless equation for any message.
    assert!(ed25519_dalek::VerifyingKey::from_bytes(&identity)
        .unwrap()
        .verify(msg, &ed25519_dalek::Signature::from_bytes(&base_r))
        .is_ok());
    let signatures: Vec<Value> = sigs
        .into_iter()
        .map(|(name, key, sig, ok)| {
            let strict = ed25519_dalek::VerifyingKey::from_bytes(&key)
                .map(|vk| {
                    vk.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig))
                        .is_ok()
                })
                .unwrap_or(false);
            assert_eq!(strict, ok, "{name}");
            json!({
                "name": name,
                "key": b64::encode(&key),
                "message": String::from_utf8(msg.to_vec()).unwrap(),
                "sig": b64::encode(&sig),
                "ok": ok,
            })
        })
        .collect();
    json!({
        "description": "Ed25519 as ed25519-dalek 2.2 verify_strict reads it. signKeys: a key decodes as a point with y taken mod p (a y >= p is accepted, as curve25519-dalek decompresses it) and is refused if it is not a point or is of small order. signatures: S must be canonical (< L), R must decode and not be of small order, the key must not be of small order, the encoded R must equal the canonical encoding of sB - kA, and k = SHA-512(R bytes || key bytes as given || message) mod L.",
        "signKeys": keys,
        "signatures": signatures,
    })
}

fn url_cases() -> (Vec<(&'static str, &'static str)>, Vec<String>) {
    let accept = vec![
        ("wss://relay.example.com", "wss://relay.example.com"),
        ("WSS://Relay.Example.COM/", "wss://relay.example.com"),
        (
            "wss://relay.example.com:443/x/y/",
            "wss://relay.example.com",
        ),
        (
            "wss://relay.example.com:8443",
            "wss://relay.example.com:8443",
        ),
        (
            "wss://relay.self-hosted.example:8443/xshell",
            "wss://relay.self-hosted.example:8443",
        ),
        (
            "wss://r.example.com/a-b/c_d~e!$&'()*+,;=:",
            "wss://r.example.com",
        ),
        ("ws://127.0.0.1:80", "ws://127.0.0.1"),
        ("ws://127.0.0.1:8787", "ws://127.0.0.1:8787"),
        ("ws://127.255.0.9", "ws://127.255.0.9"),
        ("ws://localhost:1", "ws://localhost:1"),
        ("ws://LOCALHOST", "ws://localhost"),
        ("ws://[::1]:9000", "ws://[::1]:9000"),
        ("ws://[0:0:0:0:0:0:0:1]", "ws://[::1]"),
        ("ws://[0:0:0:0:0:0:0:1]:80", "ws://[::1]"),
        ("wss://[2001:DB8::1]", "wss://[2001:db8::1]"),
        (
            "wss://[2001:db8:0:0:1:0:0:1]:443",
            "wss://[2001:db8::1:0:0:1]",
        ),
        ("wss://10.0.0.1", "wss://10.0.0.1"),
        ("wss://xn--bcher-kva.example", "wss://xn--bcher-kva.example"),
    ];
    let mut reject: Vec<String> = [
        "https://relay.example.com",
        "relay.example.com",
        "ws://relay.example.com",
        "ws://10.0.0.1",
        "wss://relay.example.com?x=1",
        "wss://relay.example.com/#f",
        "wss://user@relay.example.com",
        "wss://relay.example.com/%2e%2e",
        "wss://relay.example.com/a/../b",
        "wss://relay.example.com/./b",
        "wss://relay.example.com/a//b",
        "wss://relay.example.com/a b",
        "wss://relay.example.com/a\"b",
        "wss://relay.example.com:0",
        "wss://relay.example.com:0443",
        "wss://relay.example.com:65536",
        "wss://relay.example.com:",
        "wss://relay.example.com:+443",
        "wss://127.1",
        "wss://0x7f.0.0.1",
        "wss://1.2.3.04",
        "wss://1.2.3.256",
        "wss://-bad.example.com",
        "wss://bad-.example.com",
        "wss://bad..example.com",
        "wss://relay.example.com.",
        "wss://bad_host.example.com",
        "wss://réla y.example",
        "wss://[::1",
        "wss://[::ffff:1.2.3.4]",
        "wss://[::1]x",
        "wss://[fe80::1%25eth0]",
        "wss://",
        "wss://a\\b",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    reject.push(format!("wss://{}.example", "a".repeat(64)));
    reject.push(format!("wss://a.example/{}", "x".repeat(600)));
    (accept, reject)
}

fn urls_json() -> Value {
    let ring = RingId::derive(&dev(0).sign_key());
    let (accept, reject) = url_cases();
    json!({
        "description": "Relay URLs (RELAY.md section 5). Each `accept` URL parses, normalizes to `origin`, and its Ring endpoint for `ringId` is `endpoint`; each `reject` URL is refused (`why` is this implementation's reason, for humans).",
        "ringId": ring,
        "accept": accept.iter().map(|(u, o)| {
            let url = RelayUrl::parse(u).unwrap();
            assert_eq!(url.origin(), *o, "{u}");
            json!({"url": u, "origin": o, "endpoint": url.ring_endpoint(&ring)})
        }).collect::<Vec<_>>(),
        "reject": reject.iter().map(|u| {
            let why = RelayUrl::parse(u).expect_err(u).to_string();
            json!({"url": u, "why": why})
        }).collect::<Vec<_>>(),
    })
}

fn keys_json() -> Value {
    let devices: Vec<Value> = DEVS
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let k = dev(i);
            json!({
                "name": d.name,
                "role": d.role,
                "signSeedHex": hex(&d.sign_seed),
                "noiseSeedHex": hex(&d.noise_seed),
                "signKey": k.sign_key(),
                "noiseKey": k.noise_key(),
            })
        })
        .collect();
    json!({
        "ringIdRule": "b64u(SHA-256(\"xshell-ring-v1\\n\" || signKey))",
        "ringId": RingId::derive(&dev(0).sign_key()),
        "devices": devices,
        "noiseKeyRule": "canonical (u < 2^255-19, high bit clear) and not of small order",
        "noiseKeyRejects": NoiseKey::refused_examples().iter().map(|b| b64::encode(b)).collect::<Vec<_>>(),
    })
}

fn chain_json() -> Value {
    let c = chain();
    json!({
        "description": "v1 by desktop; v2 adds desktop-2, daemon, mobile; v3 by desktop-2 removes mobile and changes relayUrl",
        "versions": c.iter().map(|r| json!({
            "version": r.version(),
            "token": r.token(),
            "payload": payload_of(r.token()),
            "hash": r.hash(),
        })).collect::<Vec<_>>(),
    })
}

fn reject_cases() -> Vec<(String, Vec<String>, String, String)> {
    let c = chain();
    let (v1, v2) = (&c[0], &c[1]);
    let (a, a2, d, m, s) = (dev(0), dev(1), dev(2), dev(3), dev(4));
    let t = |v: &[&SignedRoster]| v.iter().map(|r| r.token().to_string()).collect::<Vec<_>>();
    let mut cases = Vec::new();
    let mut add = |name: &str, trusted: Vec<String>, candidate: String, error: &str| {
        cases.push((name.to_string(), trusted, candidate, error.to_string()));
    };

    let mut older = v1.roster().clone();
    older.issued_at += 1;
    add(
        "stale",
        t(&[v1, v2]),
        older.sign(&a).unwrap().token().into(),
        "stale",
    );
    let (head, _) = v2.token().rsplit_once('.').unwrap();
    let mut msg = ROSTER_CONTEXT.as_bytes().to_vec();
    msg.extend_from_slice(head.strip_prefix("xro1.").unwrap().as_bytes());
    let foreign_sig = b64::encode(&s.sign(&msg).unwrap());
    add(
        "forged_signature",
        t(&[v1]),
        format!("{head}.{foreign_sig}"),
        "bad_signature",
    );
    let tampered = payload_of(v2.token()).replace("\"role\":\"mobile\"", "\"role\":\"desktop\"");
    let sig = v2.token().rsplit_once('.').unwrap().1;
    add(
        "tampered_payload",
        t(&[v1]),
        format!("xro1.{}.{sig}", b64::encode(tampered.as_bytes())),
        "bad_signature",
    );
    add(
        "mobile_signed",
        t(&[v1, v2]),
        raw_next(v2, &m, |_| {}),
        "signer_not_desktop",
    );
    add(
        "daemon_signed",
        t(&[v1, v2]),
        raw_next(v2, &d, |_| {}),
        "signer_not_desktop",
    );
    add(
        "non_member_signer",
        t(&[v1, v2]),
        raw_next(v2, &s, |r| r.members.push(member(4, T0))),
        "signer_not_member",
    );
    add("gap", t(&[v1]), raw_next(v2, &a, |_| {}), "gap");
    add(
        "prev_mismatch",
        t(&[v1, v2]),
        raw_next(v2, &a, |r| r.prev = Some(v1.hash())),
        "prev_mismatch",
    );
    add(
        "same_version_fork",
        t(&[v1, v2]),
        raw_next(v1, &a, |r| r.members.push(member(3, T0))),
        "prev_mismatch",
    );
    add(
        "wrong_ring_id",
        t(&[v1, v2]),
        raw_next(v2, &a, |r| r.ring_id = RingId::derive(&s.sign_key())),
        "ring_mismatch",
    );
    let mut g = v1.roster().clone();
    g.ring_id = RingId::derive(&a2.sign_key());
    g.members.push(member(1, T0));
    add(
        "genesis_not_self_derived",
        vec![],
        g.sign(&a).unwrap().token().into(),
        "ring_mismatch",
    );
    add(
        "chain_not_from_genesis",
        vec![],
        v2.token().into(),
        "not_genesis",
    );
    let mut g = v1.roster().clone();
    g.members = vec![member(3, T0), member(1, T0)];
    g.signed_by = m.sign_key();
    g.ring_id = RingId::derive(&m.sign_key());
    add(
        "genesis_by_mobile",
        vec![],
        g.sign(&m).unwrap().token().into(),
        "signer_not_desktop",
    );
    add(
        "no_desktop",
        t(&[v1, v2]),
        raw_next(v2, &a, |r| r.members.retain(|x| x.role != Role::Desktop)),
        "invalid",
    );
    add(
        "duplicate_member_key",
        t(&[v1, v2]),
        raw_next(v2, &a, |r| {
            let mut dup = member(3, T0);
            dup.name = "again".into();
            r.members.push(dup);
        }),
        "invalid",
    );
    let p = payload_of(v1.token());
    add(
        "duplicate_json_key",
        vec![],
        sign_raw(&p.replacen('{', "{\"version\":1,", 1), &a),
        "malformed",
    );
    add(
        "unsafe_integer",
        vec![],
        sign_raw(
            &p.replace(
                &format!("\"issuedAt\":{T0}"),
                "\"issuedAt\":9007199254740992",
            ),
            &a,
        ),
        "invalid",
    );
    add(
        "plain_ws_relay",
        vec![],
        sign_raw(&p.replace(RELAY, "ws://relay.example.com"), &a),
        "invalid",
    );
    add(
        "padded_base64",
        vec![],
        format!("{}=", v1.token()),
        "malformed",
    );
    add(
        "wrong_prefix",
        vec![],
        v1.token().replacen("xro1.", "xro2.", 1),
        "malformed",
    );

    // Strict signatures (section 2): a malleated copy of a genuine token, and a signature
    // with a small-order R that a cofactorless, non-strict verifier accepts.
    let (v1_head, v1_sig) = v1.token().rsplit_once('.').unwrap();
    let v1_sig: [u8; 64] = b64::decode_array(v1_sig).unwrap();
    add(
        "malleated_signature",
        vec![],
        format!("{v1_head}.{}", b64::encode(&malleate(&v1_sig))),
        "bad_signature",
    );
    let mut msg = ROSTER_CONTEXT.as_bytes().to_vec();
    msg.extend_from_slice(v1_head.strip_prefix("xro1.").unwrap().as_bytes());
    add(
        "small_order_r_signature",
        vec![],
        format!(
            "{v1_head}.{}",
            b64::encode(&small_order_r(&DEVS[0].sign_seed, &msg))
        ),
        "bad_signature",
    );

    // Keys a Roster carries are typed fields: an unusable key is `malformed` (section 4.4).
    let with_member = |sign: &str, noise: &str| {
        let extra = format!(
            r#"{{"name":"extra","role":"daemon","signKey":"{sign}","noiseKey":"{noise}","addedAt":{T0}}}"#
        );
        sign_raw(
            &p.replacen("\"members\":[", &format!("\"members\":[{extra},"), 1),
            &a,
        )
    };
    let (dk, dn) = (d.sign_key().to_b64(), d.noise_key().to_b64());
    let identity = b64::encode(&unhex32(SMALL_ORDER_SIGN_KEYS[0]));
    let order8 = b64::encode(&unhex32(SMALL_ORDER_SIGN_KEYS[4]));
    let mut not_point = [0u8; 32];
    not_point[0] = 2;
    assert!(CompressedEdwardsY(not_point).decompress().is_none());
    add(
        "small_order_member_sign_key",
        vec![],
        with_member(&order8, &dn),
        "malformed",
    );
    add(
        "member_sign_key_not_a_point",
        vec![],
        with_member(&b64::encode(&not_point), &dn),
        "malformed",
    );
    add(
        "small_order_noise_key",
        vec![],
        with_member(&dk, &b64::encode(&[0u8; 32])),
        "malformed",
    );
    add(
        "non_canonical_noise_key",
        vec![],
        with_member(&dk, &b64::encode(&P_LE)),
        "malformed",
    );
    add(
        "small_order_signed_by",
        vec![],
        sign_raw(
            &p.replace(
                &format!("\"signedBy\":\"{}\"", a.sign_key()),
                &format!("\"signedBy\":\"{identity}\""),
            ),
            &a,
        ),
        "malformed",
    );

    // JSON lexemes (section 2): only plain non-negative integer literals are integers.
    let issued = format!("\"issuedAt\":{T0}");
    let lexeme = |from: &str, to: &str| sign_raw(&p.replace(from, to), &a);
    for (name, to, error) in [
        ("negative_zero", "\"issuedAt\":-0", "malformed"),
        ("fraction", "\"issuedAt\":1767225600.0", "malformed"),
        ("exponent", "\"issuedAt\":1.7672256e9", "malformed"),
        ("negative", "\"issuedAt\":-1", "malformed"),
        ("u64_max", "\"issuedAt\":18446744073709551615", "invalid"),
        (
            "above_u64",
            "\"issuedAt\":18446744073709551616",
            "malformed",
        ),
    ] {
        add(name, vec![], lexeme(&issued, to), error);
    }
    add(
        "unsupported_format",
        vec![],
        lexeme("\"v\":1,", "\"v\":2,"),
        "invalid",
    );
    add(
        "format_above_u32",
        vec![],
        lexeme("\"v\":1,", "\"v\":4294967296,"),
        "malformed",
    );
    let unknown = |field: &str| sign_raw(&p.replacen('{', &format!("{{\"x\":{field},"), 1), &a);
    add(
        "non_finite_in_unknown_field",
        vec![],
        unknown("1e999"),
        "malformed",
    );
    add(
        "negative_non_finite_in_unknown_field",
        vec![],
        unknown("-1e999"),
        "malformed",
    );
    add(
        "lone_surrogate_in_unknown_field",
        vec![],
        unknown("\"\\ud800\""),
        "malformed",
    );
    add(
        "lone_surrogate_in_name",
        vec![],
        lexeme("\"name\":\"desktop\"", "\"name\":\"\\udc00\""),
        "malformed",
    );
    add(
        "nested_128_deep",
        vec![],
        unknown(&format!("{}{}", "[".repeat(127), "]".repeat(127))),
        "malformed",
    );
    let mut bad_utf8 = p.clone().into_bytes();
    let at = p.find("\"desktop\"").unwrap() + 1;
    bad_utf8[at] = 0xff;
    let part = b64::encode(&bad_utf8);
    let mut msg = ROSTER_CONTEXT.as_bytes().to_vec();
    msg.extend_from_slice(part.as_bytes());
    add(
        "invalid_utf8",
        vec![],
        format!("xro1.{part}.{}", b64::encode(&a.sign(&msg).unwrap())),
        "malformed",
    );
    cases
}

/// Genesis tokens that are valid although they look unusual: each must be accepted as a
/// chain of one.
fn accept_cases() -> Vec<(&'static str, String)> {
    let c = chain();
    let a = dev(0);
    let p = payload_of(c[0].token());
    let unknown = |field: &str| sign_raw(&p.replacen('{', &format!("{{\"x\":{field},"), 1), &a);
    let non_canonical = (0..19u8)
        .map(|k| y_plus_p(k, false))
        .find(|b| {
            CompressedEdwardsY(*b)
                .decompress()
                .is_some_and(|pt| !pt.is_small_order())
        })
        .unwrap();
    let extra = format!(
        r#"{{"name":"extra","role":"daemon","signKey":"{}","noiseKey":"{}","addedAt":{T0}}}"#,
        b64::encode(&non_canonical),
        dev(2).noise_key()
    );
    vec![
        ("fraction_in_unknown_field", unknown("1.5")),
        ("large_float_in_unknown_field", unknown("1.5e308")),
        (
            "big_integer_in_unknown_field",
            unknown("18446744073709551616"),
        ),
        (
            "nested_127_deep",
            unknown(&format!("{}{}", "[".repeat(126), "]".repeat(126))),
        ),
        (
            "escaped_surrogate_pair_in_name",
            sign_raw(
                &p.replace("\"name\":\"desktop\"", "\"name\":\"desk\\ud83d\\ude00\""),
                &a,
            ),
        ),
        (
            "non_canonical_member_sign_key",
            sign_raw(
                &p.replacen("\"members\":[", &format!("\"members\":[{extra},"), 1),
                &a,
            ),
        ),
    ]
}

fn reject_json() -> Value {
    json!({
        "description": "Each candidate must be refused with `error` (the Relay's roster_invalid detail). An empty `trusted` means the candidate is checked as a chain of one, from genesis. Each `accepted` candidate must be accepted as a chain of one.",
        "cases": reject_cases().into_iter().map(|(name, trusted, candidate, error)| json!({
            "name": name, "trusted": trusted, "candidate": candidate, "error": error,
        })).collect::<Vec<_>>(),
        "accepted": accept_cases().into_iter().map(|(name, candidate)| json!({
            "name": name, "candidate": candidate,
        })).collect::<Vec<_>>(),
    })
}

fn auth_json() -> Value {
    let k = dev(2);
    let ring = RingId::derive(&dev(0).sign_key());
    let nonce = b64::encode(&[0x42; 32]);
    let origin = "wss://relay.example.com";
    let msg = auth_message(origin, &ring, &nonce, &k.sign_key());
    let sig = b64::encode(&k.sign(&msg).unwrap());
    json!({
        "origin": origin,
        "ringId": ring,
        "nonce": nonce,
        "signKey": k.sign_key(),
        "message": String::from_utf8(msg).unwrap(),
        "sig": sig,
        "mustNotVerifyFor": [
            {"origin": "wss://other-relay.example", "why": "another origin"},
            {"origin": "wss://relay.example.com:443", "why": "origins are normalized before signing; the default port is dropped"},
        ],
        "origins": [
            ["WSS://Relay.Example.COM:443/some/path/", "wss://relay.example.com"],
            ["wss://relay.example.com:8443", "wss://relay.example.com:8443"],
            ["ws://127.0.0.1:8787", "ws://127.0.0.1:8787"],
            ["ws://[0:0:0:0:0:0:0:1]:80", "ws://[::1]"],
        ],
    })
}

fn frames() -> Vec<(&'static str, String)> {
    let c = chain();
    let (a, d) = (dev(0), dev(2));
    let ring = RingId::derive(&a.sign_key());
    let nonce = b64::encode(&[0x42; 32]);
    let sig = Signature::from_bytes(
        d.sign(&auth_message(RELAY, &ring, &nonce, &d.sign_key()))
            .unwrap(),
    );
    let payload = b64::encode(b"noise message");
    let client = [
        ClientFrame::AuthChain {
            rosters: vec![c[0].token().into(), c[1].token().into()],
        },
        ClientFrame::Auth {
            sign_key: d.sign_key(),
            sig,
            caps: vec![],
        },
        ClientFrame::Env {
            to: a.sign_key(),
            payload: payload.clone(),
        },
        ClientFrame::Bye {
            reason: ByeReason::upgrade(),
        },
        ClientFrame::RosterPut {
            id: 1,
            roster: c[2].token().into(),
        },
        ClientFrame::RosterGet { id: 2, since: 1 },
        ClientFrame::EntitlementPut {
            id: 3,
            token: "xet1.e30.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        },
        ClientFrame::Ping,
    ];
    let presence = Presence {
        sign_key: d.sign_key(),
        online: false,
        last_seen: Some(T0 + 300),
        last_reason: Some("quit".into()),
    };
    let relay = vec![
        RelayFrame::Challenge {
            v: 1,
            nonce,
            roster_version: 2,
            caps: vec![],
        },
        RelayFrame::Welcome {
            you: d.sign_key(),
            roster_version: 2,
            presence: vec![
                Presence {
                    sign_key: a.sign_key(),
                    online: true,
                    last_seen: Some(T0 + 200),
                    last_reason: None,
                },
                presence.clone(),
            ],
            entitlement: None,
            limited: false,
            caps: vec![],
        },
        RelayFrame::Welcome {
            you: d.sign_key(),
            roster_version: 2,
            presence: vec![],
            entitlement: None,
            limited: true,
            caps: vec![],
        },
        RelayFrame::Presence(presence),
        RelayFrame::Env {
            from: d.sign_key(),
            payload,
        },
        RelayFrame::Ok { id: 1 },
        RelayFrame::Error {
            code: ErrorCode::RosterInvalid,
            id: Some(1),
            to: None,
            detail: Some("signer_not_desktop".into()),
        },
        RelayFrame::Error {
            code: ErrorCode::Offline,
            id: None,
            to: Some(a.sign_key()),
            detail: None,
        },
        RelayFrame::Roster {
            roster: c[2].token().into(),
        },
        RelayFrame::RosterChain {
            id: 2,
            rosters: vec![c[1].token().into(), c[2].token().into()],
            more: false,
        },
        RelayFrame::Entitlement {
            token: None,
            limited: false,
        },
        RelayFrame::Entitlement {
            token: Some(entitlement_token(T0 + 3600)),
            limited: false,
        },
        RelayFrame::Error {
            code: ErrorCode::EntitlementRequired,
            id: None,
            to: Some(a.sign_key()),
            detail: None,
        },
        RelayFrame::Pong,
    ];
    client
        .iter()
        .map(|f| ("client", f.encode()))
        .chain(relay.iter().map(|f| ("relay", f.encode())))
        .collect()
}

/// Frames with an integer one past 2^53−1, each next to the same frame at the limit.
fn int_bounds() -> Vec<(&'static str, String, String)> {
    let max = 9_007_199_254_740_991u64;
    let key = dev(2).sign_key().to_b64();
    type Frame = Box<dyn Fn(u64) -> Value>;
    let cases: Vec<(&str, Frame)> = vec![
        (
            "client",
            Box::new(|n| json!({"t":"roster.get","id":n,"since":1})),
        ),
        (
            "client",
            Box::new(|n| json!({"t":"roster.get","id":1,"since":n})),
        ),
        (
            "client",
            Box::new(|n| json!({"t":"roster.put","id":n,"roster":"xro1.a.b"})),
        ),
        (
            "client",
            Box::new(|n| json!({"t":"entitlement.put","id":n,"token":"xet1.a.b"})),
        ),
        ("relay", Box::new(|n| json!({"t":"ok","id":n}))),
        (
            "relay",
            Box::new(|n| json!({"t":"error","code":"internal","id":n})),
        ),
        (
            "relay",
            Box::new(|n| json!({"t":"roster.chain","id":n,"rosters":[],"more":false})),
        ),
        (
            "relay",
            Box::new(|n| json!({"t":"challenge","v":1,"nonce":"","rosterVersion":n})),
        ),
        (
            "relay",
            Box::new(move |n| json!({"t":"presence","signKey":key,"online":false,"lastSeen":n})),
        ),
    ];
    cases
        .into_iter()
        .map(|(dir, f)| (dir, f(max).to_string(), f(max + 1).to_string()))
        .collect()
}

/// Client frames and how a Relay must read them: `ok` (decodes, possibly as an unknown type),
/// `malformed` (not one strict JSON object with a string `t`: `bad_request`, close 4000) or
/// `invalid` (a known type with bad fields: `bad_request`, close 4000 only before auth).
fn lexemes() -> Vec<(String, &'static str)> {
    let get = |id: &str| format!(r#"{{"t":"roster.get","id":{id},"since":0}}"#);
    let deep = |n: usize| format!(r#"{{"t":"ping","x":{}{}}}"#, "[".repeat(n), "]".repeat(n));
    let k = dev(2).sign_key();
    vec![
        (get("0"), "ok"),
        (get("-0"), "invalid"),
        (get("1.0"), "invalid"),
        (get("1e2"), "invalid"),
        (get("-1"), "invalid"),
        (get("18446744073709551615"), "invalid"),
        (get("18446744073709551616"), "invalid"),
        (
            r#"{"t":"roster.get","id":1,"since":0,"x":1.5}"#.into(),
            "ok",
        ),
        (
            r#"{"t":"roster.get","id":1,"since":0,"x":18446744073709551616}"#.into(),
            "ok",
        ),
        (
            r#"{"t":"roster.get","id":1,"since":0,"x":1e999}"#.into(),
            "malformed",
        ),
        (
            r#"{"t":"roster.get","id":1,"since":0,"x":-1e999}"#.into(),
            "malformed",
        ),
        (r#"{"t":"future.type","x":1.5e308}"#.into(), "ok"),
        (r#"{"t":"future.type","x":1e309}"#.into(), "malformed"),
        (r#"{"t":"ping","x":"\ud800"}"#.into(), "malformed"),
        (r#"{"t":"ping","x":"\udc00\ud800"}"#.into(), "malformed"),
        (r#"{"t":"ping","x":"\ud83d\ude00"}"#.into(), "ok"),
        (r#"{"t":"ping","x":{"a":1,"a":1}}"#.into(), "malformed"),
        (r#"{"t":"ping","t":"ping"}"#.into(), "malformed"),
        (deep(126), "ok"),
        (deep(127), "malformed"),
        (format!(r#"{{"t":"env","to":"{k}","payload":"aGk"}}"#), "ok"),
        (
            format!(r#"{{"t":"env","to":"{k}","payload":1}}"#),
            "invalid",
        ),
        (
            format!(
                r#"{{"t":"env","to":"{}","payload":"aGk"}}"#,
                b64::encode(&unhex32(SMALL_ORDER_SIGN_KEYS[0]))
            ),
            "invalid",
        ),
    ]
}

fn frames_json() -> Value {
    json!({
        "description": "One of every v1 frame, as this implementation encodes it. Field order is not significant. `intBounds`: each `max` frame decodes, each `over` frame (one past 2^53-1) is refused. `lexemes`: client frames and their reading: `ok` decodes (an unknown `t` included), `malformed` is not one strict JSON object with a string `t` (bad_request, close 4000), `invalid` is a known type with bad fields (bad_request; close 4000 before auth).",
        "frames": frames().into_iter().map(|(dir, j)| json!({"dir": dir, "json": j})).collect::<Vec<_>>(),
        "intBounds": int_bounds().into_iter().map(|(dir, max, over)| json!({"dir": dir, "max": max, "over": over})).collect::<Vec<_>>(),
        "lexemes": lexemes().into_iter().map(|(j, result)| json!({"dir": "client", "json": j, "result": result})).collect::<Vec<_>>(),
    })
}

fn gateway() -> DeviceKeys {
    DeviceKeys::from_seeds(&[0x61; 32], &[0x62; 32])
}

fn entitlement_token(expires_at: u64) -> String {
    sign_entitlement(
        &gateway(),
        &RingId::derive(&dev(0).sign_key()),
        Tier::Hosted,
        "purchase-1",
        T0,
        expires_at,
    )
    .unwrap()
}

fn entitlement_json() -> Value {
    let gw = gateway();
    let ring = RingId::derive(&dev(0).sign_key());
    let push = sign_entitlement(&gw, &ring, Tier::Push, "purchase-1", T0, T0 + 3600).unwrap();
    let hosted = entitlement_token(T0 + 3600);
    let (head, _) = hosted.rsplit_once('.').unwrap();
    json!({
        "description": "Push Gateway entitlement tokens in the format of push/core/entitlement.ts, verified at `now` for `ringId` with the gateway key `publicKey` (kid = b64u(SHA-256(key)[0..8])).",
        "gatewaySeedHex": hex(&[0x61; 32]),
        "publicKey": gw.sign_key(),
        "kid": entitlement_kid(&gw.sign_key()),
        "ringId": ring,
        "now": T0 + 60,
        "cases": [
            {"name": "hosted", "token": hosted, "hosted": true, "result": "ok"},
            {"name": "push_tier_for_hosted", "token": push, "hosted": true, "result": "wrong_tier"},
            {"name": "push_tier", "token": push, "hosted": false, "result": "ok"},
            {"name": "expired", "token": entitlement_token(T0 + 60), "hosted": false, "result": "expired"},
            {"name": "other_ring", "token": sign_entitlement(&gw, &RingId::derive(&dev(1).sign_key()), Tier::Hosted, "p", T0, T0 + 3600).unwrap(), "hosted": false, "result": "ring_mismatch"},
            {"name": "unknown_kid", "token": sign_entitlement(&dev(4), &ring, Tier::Hosted, "p", T0, T0 + 3600).unwrap(), "hosted": false, "result": "unknown_kid"},
            {"name": "bad_signature", "token": format!("{head}.{}", b64::encode(&[0u8; 64])), "hosted": false, "result": "bad_signature"},
            {"name": "padded", "token": format!("{hosted}="), "hosted": false, "result": "malformed"},
        ],
    })
}

fn build() -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    for (name, v) in [
        ("keys.json", keys_json()),
        ("roster-chain.json", chain_json()),
        ("roster-reject.json", reject_json()),
        ("auth.json", auth_json()),
        ("frames.json", frames_json()),
        ("entitlement.json", entitlement_json()),
        ("ed25519.json", ed25519_json()),
        ("urls.json", urls_json()),
    ] {
        m.insert(name, serde_json::to_string_pretty(&v).unwrap() + "\n");
    }
    m
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/ring")
}

fn read(name: &str) -> Value {
    let s = std::fs::read_to_string(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&s).unwrap()
}

#[test]
#[ignore = "rewrites testdata/ring"]
fn bless_ring_vectors() {
    std::fs::create_dir_all(dir()).unwrap();
    for (name, body) in build() {
        std::fs::write(dir().join(name), body).unwrap();
    }
}

#[test]
fn vectors_are_current() {
    for (name, body) in build() {
        let on_disk = std::fs::read_to_string(dir().join(name)).unwrap_or_default();
        assert!(
            on_disk == body,
            "testdata/ring/{name} is stale: run bless_ring_vectors"
        );
    }
}

#[test]
fn vector_keys() {
    let v = read("keys.json");
    for d in v["devices"].as_array().unwrap() {
        let seed = |k: &str| -> [u8; 32] {
            let h = d[k].as_str().unwrap();
            let mut out = [0u8; 32];
            for (i, b) in out.iter_mut().enumerate() {
                *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
            }
            out
        };
        let k = DeviceKeys::from_seeds(&seed("signSeedHex"), &seed("noiseSeedHex"));
        assert_eq!(json!(k.sign_key()), d["signKey"]);
        assert_eq!(json!(k.noise_key()), d["noiseKey"]);
    }
    let creator =
        xshell_protocol::ring::SignKey::parse(v["devices"][0]["signKey"].as_str().unwrap())
            .unwrap();
    assert_eq!(json!(RingId::derive(&creator)), v["ringId"]);
}

#[test]
fn vector_chain_verifies() {
    let v = read("roster-chain.json");
    let versions = v["versions"].as_array().unwrap();
    let tokens: Vec<&str> = versions
        .iter()
        .map(|x| x["token"].as_str().unwrap())
        .collect();
    let chain = RosterChain::from_tokens(&tokens).unwrap();
    assert_eq!(chain.head().version(), 3);
    for (i, x) in versions.iter().enumerate() {
        let r = &chain.versions()[i];
        assert_eq!(x["hash"].as_str().unwrap(), r.hash());
        assert_eq!(x["payload"].as_str().unwrap(), payload_of(r.token()));
        if i > 0 {
            assert_eq!(r.roster().prev.as_deref(), versions[i - 1]["hash"].as_str());
        }
    }
}

#[test]
fn vector_rejections() {
    let v = read("roster-reject.json");
    for c in v["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let trusted: Vec<&str> = c["trusted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        let candidate = c["candidate"].as_str().unwrap();
        let got = if trusted.is_empty() {
            RosterChain::from_tokens(&[candidate]).map(|_| ())
        } else {
            let mut chain = RosterChain::from_tokens(&trusted).unwrap();
            SignedRoster::parse(candidate).and_then(|r| chain.accept(&[r]).map(|_| ()))
        };
        let code = got.err().map(|e| e.as_code());
        assert_eq!(code, c["error"].as_str(), "case {name}");
    }
    for c in v["accepted"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let got = RosterChain::from_tokens(&[c["candidate"].as_str().unwrap()]);
        assert!(got.is_ok(), "case {name}: {got:?}");
    }
}

#[test]
fn vector_auth_message() {
    let v = read("auth.json");
    let s = |k: &str| v[k].as_str().unwrap().to_string();
    let key = xshell_protocol::ring::SignKey::parse(&s("signKey")).unwrap();
    let ring = RingId::parse(&s("ringId")).unwrap();
    let msg = auth_message(&s("origin"), &ring, &s("nonce"), &key);
    assert_eq!(String::from_utf8(msg.clone()).unwrap(), s("message"));
    let sig = Signature::parse(&s("sig")).unwrap();
    assert!(verify(&key, &msg, &sig));
    for case in v["mustNotVerifyFor"].as_array().unwrap() {
        let other = auth_message(case["origin"].as_str().unwrap(), &ring, &s("nonce"), &key);
        assert!(!verify(&key, &other, &sig));
    }
    for pair in v["origins"].as_array().unwrap() {
        let got = xshell_protocol::ring::url::normalize_origin(pair[0].as_str().unwrap()).unwrap();
        assert_eq!(got, pair[1].as_str().unwrap());
    }
}

#[test]
fn vector_noise_key_rejects() {
    let v = read("keys.json");
    let rejects = v["noiseKeyRejects"].as_array().unwrap();
    assert!(rejects.len() >= 10);
    for r in rejects {
        assert!(NoiseKey::parse(r.as_str().unwrap()).is_err(), "{r}");
    }
}

#[test]
fn vector_entitlements() {
    let v = read("entitlement.json");
    let key = xshell_protocol::ring::SignKey::parse(v["publicKey"].as_str().unwrap()).unwrap();
    assert_eq!(entitlement_kid(&key), v["kid"].as_str().unwrap());
    let keys = GatewayKeys::new(&[key]);
    let ring = RingId::parse(v["ringId"].as_str().unwrap()).unwrap();
    let now = v["now"].as_u64().unwrap();
    for c in v["cases"].as_array().unwrap() {
        let got = verify_entitlement(
            c["token"].as_str().unwrap(),
            &ring,
            now,
            &keys,
            c["hosted"].as_bool().unwrap(),
        );
        let code = match got {
            Ok(_) => "ok",
            Err(e) => e.as_code(),
        };
        assert_eq!(code, c["result"].as_str().unwrap(), "{}", c["name"]);
    }
}

#[test]
fn vector_int_bounds() {
    let v = read("frames.json");
    for c in v["intBounds"].as_array().unwrap() {
        let (max, over) = (c["max"].as_str().unwrap(), c["over"].as_str().unwrap());
        let (ok, bad) = match c["dir"].as_str().unwrap() {
            "client" => (decode_client(max).is_ok(), decode_client(over).is_err()),
            _ => (decode_relay(max).is_ok(), decode_relay(over).is_err()),
        };
        assert!(ok, "{max} refused");
        assert!(bad, "{over} decoded");
    }
}

#[test]
fn vector_frames_round_trip() {
    let v = read("frames.json");
    for f in v["frames"].as_array().unwrap() {
        let j = f["json"].as_str().unwrap();
        let again = match f["dir"].as_str().unwrap() {
            "client" => decode_client(j).unwrap().encode(),
            _ => decode_relay(j).unwrap().encode(),
        };
        assert_eq!(again, j);
    }
}

#[test]
fn vector_lexemes() {
    let v = read("frames.json");
    for c in v["lexemes"].as_array().unwrap() {
        let j = c["json"].as_str().unwrap();
        let got = match decode_client(j) {
            Ok(_) => "ok",
            Err(WireError::Malformed(_)) => "malformed",
            Err(WireError::Invalid { .. }) => "invalid",
            Err(WireError::TooLarge) => "too_large",
        };
        assert_eq!(got, c["result"].as_str().unwrap(), "{j}");
    }
}

#[test]
fn vector_ed25519() {
    let v = read("ed25519.json");
    for c in v["signKeys"].as_array().unwrap() {
        let key = c["key"].as_str().unwrap();
        assert_eq!(
            SignKey::parse(key).is_ok(),
            c["ok"].as_bool().unwrap(),
            "{key}: {}",
            c["why"]
        );
    }
    for c in v["signatures"].as_array().unwrap() {
        let ok = c["ok"].as_bool().unwrap();
        let msg = c["message"].as_str().unwrap().as_bytes();
        let sig = Signature::parse(c["sig"].as_str().unwrap()).unwrap();
        let got = SignKey::parse(c["key"].as_str().unwrap())
            .map(|k| verify(&k, msg, &sig))
            .unwrap_or(false);
        assert_eq!(got, ok, "{}", c["name"]);
    }
}

#[test]
fn vector_urls() {
    let v = read("urls.json");
    let ring = RingId::parse(v["ringId"].as_str().unwrap()).unwrap();
    for c in v["accept"].as_array().unwrap() {
        let url = RelayUrl::parse(c["url"].as_str().unwrap()).unwrap();
        assert_eq!(url.origin(), c["origin"].as_str().unwrap());
        assert_eq!(url.ring_endpoint(&ring), c["endpoint"].as_str().unwrap());
    }
    for c in v["reject"].as_array().unwrap() {
        assert!(
            RelayUrl::parse(c["url"].as_str().unwrap()).is_err(),
            "{}",
            c["url"]
        );
    }
}

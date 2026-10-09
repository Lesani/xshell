//! Test vectors for other implementations (the Relay Worker): fixed seeds and timestamps, so
//! every file under `testdata/ring/` is reproducible (Ed25519 signatures are deterministic).
//! Regenerate with
//! `cargo test -p xshell-protocol --all-features --test ring_vectors -- --ignored bless_ring_vectors`.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use xshell_protocol::ring::entitlement::{
    entitlement_kid, sign_entitlement, verify_entitlement, GatewayKeys, Tier,
};
use xshell_protocol::ring::relay::wire::{
    auth_message, decode_client, decode_relay, ByeReason, ClientFrame, ErrorCode, Presence,
    RelayFrame,
};
use xshell_protocol::ring::{
    b64, verify, DeviceKeys, Member, NoiseKey, RingId, Role, Roster, RosterChain, Signature,
    SignedRoster, Signer, ROSTER_CONTEXT,
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
    cases
}

fn reject_json() -> Value {
    json!({
        "description": "Each candidate must be refused with `error` (the Relay's roster_invalid detail). An empty `trusted` means the candidate is checked as a chain of one, from genesis.",
        "cases": reject_cases().into_iter().map(|(name, trusted, candidate, error)| json!({
            "name": name, "trusted": trusted, "candidate": candidate, "error": error,
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

fn frames_json() -> Value {
    json!({
        "description": "One of every v1 frame, as this implementation encodes it. Field order is not significant. `intBounds`: each `max` frame decodes, each `over` frame (one past 2^53-1) is refused.",
        "frames": frames().into_iter().map(|(dir, j)| json!({"dir": dir, "json": j})).collect::<Vec<_>>(),
        "intBounds": int_bounds().into_iter().map(|(dir, max, over)| json!({"dir": dir, "max": max, "over": over})).collect::<Vec<_>>(),
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

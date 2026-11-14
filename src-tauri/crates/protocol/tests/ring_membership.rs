//! Public signed-token seams for the versioned Computer membership contract.
use serde_json::{json, Value};
use xshell_protocol::ring::{
    b64, DeviceKeys, Member, RingId, Role, RosterChain, SignedRoster, Signer,
};

fn keys(n: u8) -> DeviceKeys {
    DeviceKeys::from_seeds(&[n; 32], &[n + 100; 32])
}
fn token(payload: &Value, signer: &dyn Signer) -> String {
    let part = b64::encode(payload.to_string().as_bytes());
    let sig = signer
        .sign(format!("xshell-roster-v2\n{part}").as_bytes())
        .unwrap();
    format!("xro2.{part}.{}", b64::encode(&sig))
}
fn singleton_upgrade() -> (DeviceKeys, SignedRoster, Value) {
    let a = keys(1);
    let g =
        SignedRoster::genesis(&a, a.noise_key(), "retained", "ws://127.0.0.1:8787", 100).unwrap();
    let r = json!({"v":2,"ringId":g.ring_id(),"version":2,"prev":g.hash(),"relayUrl":g.roster().relay_url,"signedBy":a.sign_key(),"issuedAt":101,"members":g.roster().members,
      "membership":{"enrollments":[{"id":b64::encode(&[1;32]),"kind":"legacyDesktop","members":[a.sign_key()],"enrolledAt":101}],"retired":[],"policy":{"inactivitySeconds":2592000,"activityKey":keys(9).sign_key()},"change":{"type":"upgrade","proofs":[]}}});
    (a, g, r)
}
#[test]
fn desktop_only_v1_upgrade_preserves_ring_key_and_authority() {
    let (a, g, r) = singleton_upgrade();
    let u = SignedRoster::parse(&token(&r, &a))
        .expect("a strict v2 retained Desktop singleton must parse");
    let c = RosterChain::from_chain(vec![g.clone(), u.clone()]).unwrap();
    assert_eq!(c.ring_id(), g.ring_id());
    assert_eq!(c.head().member(&a.sign_key()), g.member(&a.sign_key()));
}
#[test]
fn v2_signature_cannot_be_downgraded_or_reinterpreted_as_v1() {
    let (a, _, r) = singleton_upgrade();
    let t = token(&r, &a);
    assert!(SignedRoster::parse(&t.replacen("xro2.", "xro1.", 1)).is_err());
}
#[test]
fn upgrade_must_not_invent_or_drop_a_counterpart() {
    let (a, g, mut r) = singleton_upgrade();
    let d = keys(2);
    r["members"].as_array_mut().unwrap().push(
        serde_json::to_value(Member::new(
            "invented",
            Role::Daemon,
            d.sign_key(),
            d.noise_key(),
            101,
        ))
        .unwrap(),
    );
    r["membership"]["enrollments"][0]["kind"] = json!("computer");
    r["membership"]["enrollments"][0]["members"] = json!([a.sign_key(), d.sign_key()]);
    if let Ok(u) = SignedRoster::parse(&token(&r, &a)) {
        assert!(RosterChain::from_chain(vec![g, u]).is_err());
    }
}
#[test]
fn old_binary_genesis_domain_stays_v1() {
    let k = keys(1);
    assert_eq!(
        RingId::derive(&k.sign_key()),
        singleton_upgrade().1.roster().ring_id
    );
}

use xshell_protocol::ring::activity::{policy_hash, ActivityEvidence, UnitActivity};
use xshell_protocol::ring::membership::{
    consent_message, local_consent_context, pairing_consent_context, ConsentProof, Enrollment,
    EnrollmentKind as Kind, Membership, RingPolicy, RosterChange as Change,
};
use xshell_protocol::ring::{Roster, Signature};
fn eid(n: u8) -> String {
    b64::encode(&[n; 32])
}
fn unit(n: u8, kind: Kind, ks: &[&DeviceKeys], at: u64) -> Enrollment {
    Enrollment {
        id: eid(n),
        kind,
        members: ks.iter().map(|k| k.sign_key()).collect(),
        enrolled_at: at,
    }
}
fn raw(r: &Roster, k: &dyn Signer) -> String {
    token(&serde_json::to_value(r).unwrap(), k)
}
fn consent(r: &mut Roster, ks: &[&DeviceKeys]) {
    let mut m = r.membership().unwrap();
    let context = if matches!(m.change, Change::Enroll { .. }) {
        pairing_consent_context(r, &[42; 32]).unwrap()
    } else {
        local_consent_context(r).unwrap()
    };
    let ps = ks
        .iter()
        .map(|k| ConsentProof {
            key: k.sign_key(),
            expires_at: r.issued_at + 600,
            context: context.clone(),
            signature: Signature::from_bytes(
                k.sign(&consent_message(r, r.issued_at + 600, &context).unwrap())
                    .unwrap(),
            ),
        })
        .collect();
    match &mut m.change {
        Change::Genesis { proofs }
        | Change::Upgrade { proofs }
        | Change::Enroll { proofs, .. }
        | Change::CompleteComputer { proofs, .. } => *proofs = ps,
        _ => panic!("not consent"),
    };
    r.set_membership(&m);
}
fn draft(p: &SignedRoster, k: &DeviceKeys, change: Change) -> Roster {
    let mut r = p.roster().clone();
    r.v = 2;
    r.version += 1;
    r.prev = Some(p.hash());
    r.issued_at += 1;
    r.signed_by = k.sign_key();
    let mut m = r.membership().unwrap();
    m.change = change;
    r.set_membership(&m);
    r
}
fn signed(r: &Roster, k: &DeviceKeys) -> SignedRoster {
    SignedRoster::parse(&raw(r, k)).unwrap()
}
fn upgrade_multi() -> (Vec<DeviceKeys>, Vec<SignedRoster>) {
    let ks = (1..=9).map(keys).collect::<Vec<_>>();
    let a = &ks[0];
    let g = SignedRoster::genesis(a, a.noise_key(), "a", "ws://127.0.0.1:8787", 100).unwrap();
    let v1 = g
        .next(a, 101, |r| {
            r.add(Member::new(
                "other desktop",
                Role::Desktop,
                ks[1].sign_key(),
                ks[1].noise_key(),
                101,
            ));
            r.add(Member::new(
                "headless",
                Role::Daemon,
                ks[2].sign_key(),
                ks[2].noise_key(),
                101,
            ));
            r.add(Member::new(
                "mobile",
                Role::Mobile,
                ks[3].sign_key(),
                ks[3].noise_key(),
                101,
            ));
        })
        .unwrap();
    let mut r = v1.roster().clone();
    r.v = 2;
    r.version = 3;
    r.prev = Some(v1.hash());
    r.issued_at = 102;
    r.set_membership(&Membership {
        enrollments: vec![
            unit(1, Kind::LegacyDesktop, &[a], 102),
            unit(2, Kind::LegacyDesktop, &[&ks[1]], 102),
            unit(3, Kind::Daemon, &[&ks[2]], 102),
            unit(4, Kind::Mobile, &[&ks[3]], 102),
        ],
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: Some(30),
            activity_key: ks[8].sign_key(),
        },
        change: Change::Upgrade { proofs: vec![] },
    });
    let u = signed(&r, a);
    (ks, vec![g, v1, u])
}
fn evidence(h: &SignedRoster, ks: &[DeviceKeys], at: u64, terminal: bool) -> String {
    let m = h.roster().membership().unwrap();
    ActivityEvidence {
        ring_id: h.ring_id().clone(),
        relay_url: h.roster().relay_url.clone(),
        policy_head_hash: h.hash(),
        policy_hash: policy_hash(&m.policy),
        observed_at: at,
        units: m
            .enrollments
            .iter()
            .map(|u| UnitActivity {
                enrollment_id: u.id.clone(),
                member_keys: u.members.clone(),
                online: false,
                last_active: u.enrolled_at,
                terminal_expired: terminal,
            })
            .collect(),
    }
    .sign(&ks[8])
    .unwrap()
}
fn complete(p: &SignedRoster, ks: &[DeviceKeys]) -> Roster {
    let mut r = draft(
        p,
        &ks[0],
        Change::CompleteComputer {
            replaces: vec![eid(1), eid(3)],
            enrollment_id: eid(5),
            proofs: vec![],
        },
    );
    let mut m = r.membership().unwrap();
    m.enrollments.retain(|e| e.id != eid(1) && e.id != eid(3));
    m.enrollments
        .push(unit(5, Kind::Computer, &[&ks[0], &ks[2]], r.issued_at));
    m.retired = vec![eid(1), eid(3)];
    r.set_membership(&m);
    consent(&mut r, &[&ks[0], &ks[2]]);
    r
}
#[test]
fn completion_retains_both_keys_and_old_leave_cannot_remove_replacement() {
    let (ks, mut h) = upgrade_multi();
    let r = complete(h.last().unwrap(), &ks);
    let c = signed(&r, &ks[0]);
    h.push(c.clone());
    RosterChain::from_chain(h.clone()).unwrap();
    assert_eq!(c.member(&ks[0].sign_key()), h[2].member(&ks[0].sign_key()));
    assert_eq!(c.member(&ks[2].sign_key()), h[2].member(&ks[2].sign_key()));
    let mut old = draft(
        &c,
        &ks[2],
        Change::Remove {
            enrollment_id: eid(3),
        },
    );
    let mut m = old.membership().unwrap();
    m.change = Change::Remove {
        enrollment_id: eid(3),
    };
    old.set_membership(&m);
    h.push(signed(&old, &ks[2]));
    assert!(RosterChain::from_chain(h).is_err());
}
#[test]
fn supplied_terminal_evidence_blocks_stale_authority_but_not_self_cleanup() {
    let (ks, h) = upgrade_multi();
    let p = h.last().unwrap();
    let mut c = RosterChain::from_chain(h.clone()).unwrap();
    assert!(c.authorized_member(&ks[0].sign_key()).is_some());
    c.observe_activity(&evidence(p, &ks, 140, true)).unwrap();
    assert!(c.authorized_member(&ks[0].sign_key()).is_none());
    assert!(
        c.head().member(&ks[0].sign_key()).is_some(),
        "signed head is not rewritten"
    );
    let r = complete(p, &ks);
    assert!(c.accept(&[signed(&r, &ks[0])]).is_err());
    let mut remove = draft(
        p,
        &ks[2],
        Change::Remove {
            enrollment_id: eid(3),
        },
    );
    let mut m = remove.membership().unwrap();
    m.enrollments.retain(|e| e.id != eid(3));
    m.retired.push(eid(3));
    remove.members.retain(|a| a.sign_key != ks[2].sign_key());
    remove.set_membership(&m);
    c.accept(&[signed(&remove, &ks[2])]).unwrap();
}
#[test]
fn shared_membership_vectors() {
    let (ks, h) = upgrade_multi();
    let p = h.last().unwrap();
    let mut cases: Vec<Value> = vec![];
    let mut add = |name: &str, chain: Vec<String>, accepted: bool| {
        let result = RosterChain::from_tokens(&chain).is_ok();
        assert_eq!(result, accepted, "{name}");
        cases.push(json!({"name":name,"tokens":chain,"accepted":accepted}));
    };
    let tokens = |r: &Roster, k: &DeviceKeys| {
        let mut t = h.iter().map(|r| r.token().to_string()).collect::<Vec<_>>();
        t.push(raw(r, k));
        t
    };
    add(
        "multiple_unproven_retained_desktops",
        h.iter().map(|r| r.token().to_string()).collect(),
        true,
    );
    let (legacy_signer, legacy_genesis, mut stripped_upgrade) = singleton_upgrade();
    let mut legacy_roster = legacy_genesis.roster().clone();
    legacy_roster.members[0]
        .extra
        .insert("futureMetadata".into(), json!("retained"));
    let legacy_head = legacy_roster.sign(&legacy_signer).unwrap();
    stripped_upgrade["prev"] = json!(legacy_head.hash());
    add(
        "upgrade_cannot_strip_legacy_member_extension",
        vec![
            legacy_head.token().to_string(),
            token(&stripped_upgrade, &legacy_signer),
        ],
        false,
    );
    let good = complete(p, &ks);
    add(
        "complete_both_legacy_singletons_preserves_keys",
        tokens(&good, &ks[0]),
        true,
    );
    let mut bad = good.clone();
    let mut m = bad.membership().unwrap();
    if let Change::CompleteComputer { proofs, .. } = &mut m.change {
        proofs.pop();
    }
    bad.set_membership(&m);
    add(
        "completion_missing_daemon_possession",
        tokens(&bad, &ks[0]),
        false,
    );
    let mut bad = good.clone();
    let mut m = bad.membership().unwrap();
    m.enrollments.last_mut().unwrap().members = vec![ks[0].sign_key(), ks[4].sign_key()];
    bad.set_membership(&m);
    add(
        "completion_cannot_substitute_daemon",
        tokens(&bad, &ks[0]),
        false,
    );
    let mut remove = draft(
        p,
        &ks[2],
        Change::Remove {
            enrollment_id: eid(3),
        },
    );
    let mut m = remove.membership().unwrap();
    m.enrollments.retain(|e| e.id != eid(3));
    m.retired.push(eid(3));
    remove.members.retain(|x| x.sign_key != ks[2].sign_key());
    remove.set_membership(&m);
    add("daemon_whole_self_departure", tokens(&remove, &ks[2]), true);
    let mut bad = remove.clone();
    bad.signed_by = ks[1].sign_key();
    add("desktop_can_remove_daemon", tokens(&bad, &ks[1]), true);
    let mut bad = remove.clone();
    bad.signed_by = ks[3].sign_key();
    add("mobile_can_remove_daemon", tokens(&bad, &ks[3]), true);
    let mut bad = good.clone();
    bad.signed_by = ks[2].sign_key();
    add(
        "daemon_cannot_complete_or_enroll",
        tokens(&bad, &ks[2]),
        false,
    );
    let mut downgrade = p.roster().clone();
    downgrade.v = 1;
    downgrade.version += 1;
    downgrade.prev = Some(p.hash());
    downgrade.extra.clear();
    let part = b64::encode(&serde_json::to_vec(&downgrade).unwrap());
    let sig = ks[0]
        .sign(format!("xshell-roster-v1\n{part}").as_bytes())
        .unwrap();
    let mut t = h.iter().map(|r| r.token().to_string()).collect::<Vec<_>>();
    t.push(format!("xro1.{part}.{}", b64::encode(&sig)));
    add("no_v2_to_v1_downgrade", t, false);
    let mut expiry = draft(
        p,
        &ks[2],
        Change::Expire {
            enrollment_ids: vec![eid(1), eid(2), eid(4)],
            evidence: evidence(p, &ks, 140, true),
        },
    );
    expiry.issued_at = 140;
    let mut m = expiry.membership().unwrap();
    m.enrollments.retain(|e| e.id == eid(3));
    m.retired = vec![eid(1), eid(2), eid(4)];
    expiry.members.retain(|x| x.sign_key == ks[2].sign_key());
    expiry.set_membership(&m);
    add(
        "daemon_independently_proven_expiry",
        tokens(&expiry, &ks[2]),
        true,
    );
    let mut bad = expiry.clone();
    let mut m = bad.membership().unwrap();
    if let Change::Expire { evidence: ev, .. } = &mut m.change {
        *ev = evidence(p, &ks, 140, false);
    }
    bad.set_membership(&m);
    add(
        "offline_snapshot_not_terminal_evidence",
        tokens(&bad, &ks[2]),
        false,
    );
    let mut empty = draft(
        p,
        &ks[0],
        Change::Expire {
            enrollment_ids: vec![eid(1), eid(2), eid(3), eid(4)],
            evidence: evidence(p, &ks, 140, true),
        },
    );
    empty.issued_at = 140;
    empty.members.clear();
    let mut m = empty.membership().unwrap();
    m.enrollments.clear();
    m.retired = vec![eid(1), eid(2), eid(3), eid(4)];
    empty.set_membership(&m);
    add("terminal_empty_ring_allowed", tokens(&empty, &ks[0]), true);
    let c = signed(&good, &ks[0]);
    let mut split = draft(
        &c,
        &ks[2],
        Change::Remove {
            enrollment_id: eid(5),
        },
    );
    let mut m = split.membership().unwrap();
    m.enrollments
        .iter_mut()
        .find(|e| e.id == eid(5))
        .unwrap()
        .members
        .pop();
    split.members.retain(|x| x.sign_key != ks[2].sign_key());
    split.set_membership(&m);
    let mut t = tokens(&good, &ks[0]);
    t.push(raw(&split, &ks[2]));
    add("computer_cannot_be_split", t, false);
    let mut reused = draft(
        &c,
        &ks[1],
        Change::Enroll {
            enrollment_id: eid(1),
            proofs: vec![],
        },
    );
    let mut m = reused.membership().unwrap();
    m.enrollments
        .push(unit(1, Kind::Daemon, &[&ks[4]], reused.issued_at));
    reused.members.push(Member::new(
        "new",
        Role::Daemon,
        ks[4].sign_key(),
        ks[4].noise_key(),
        reused.issued_at,
    ));
    reused.set_membership(&m);
    consent(&mut reused, &[&ks[4]]);
    let mut t = tokens(&good, &ks[0]);
    t.push(raw(&reused, &ks[1]));
    add("retired_enrollment_id_never_reused", t, false);
    let mut bad = good.clone();
    bad.signed_by = ks[2].sign_key();
    consent(&mut bad, &[&ks[0], &ks[2]]);
    add(
        "daemon_completion_denied_even_with_valid_consent",
        tokens(&bad, &ks[2]),
        false,
    );
    let mut tampered = good.clone();
    tampered.signed_by = ks[1].sign_key();
    add(
        "consent_binds_approving_desktop",
        tokens(&tampered, &ks[1]),
        false,
    );
    let mut tampered = good.clone();
    let mut m = tampered.membership().unwrap();
    if let Change::CompleteComputer { proofs, .. } = &mut m.change {
        proofs[0].context.hash = eid(99);
    }
    tampered.set_membership(&m);
    add("tampered_consent_context", tokens(&tampered, &ks[0]), false);
    let mut cross = good.clone();
    let mut m = cross.membership().unwrap();
    if let Change::CompleteComputer { proofs, .. } = &mut m.change {
        for proof in proofs {
            proof.context.kind = xshell_protocol::ring::membership::ConsentFlow::Pairing;
            proof.signature = Signature::from_bytes(
                ks.iter()
                    .find(|k| k.sign_key() == proof.key)
                    .unwrap()
                    .sign(&consent_message(&good, proof.expires_at, &proof.context).unwrap())
                    .unwrap(),
            );
        }
    }
    cross.set_membership(&m);
    add(
        "cross_flow_context_replay_denied_even_with_valid_signatures",
        tokens(&cross, &ks[0]),
        false,
    );
    let mut forged_local = good.clone();
    let mut m = forged_local.membership().unwrap();
    if let Change::CompleteComputer { proofs, .. } = &mut m.change {
        for proof in proofs {
            proof.context.hash = eid(88);
            proof.signature = Signature::from_bytes(
                ks.iter()
                    .find(|k| k.sign_key() == proof.key)
                    .unwrap()
                    .sign(&consent_message(&good, proof.expires_at, &proof.context).unwrap())
                    .unwrap(),
            );
        }
    }
    forged_local.set_membership(&m);
    add(
        "local_context_must_match_exact_proposal",
        tokens(&forged_local, &ks[0]),
        false,
    );
    let mut remove_other = draft(
        p,
        &ks[2],
        Change::Remove {
            enrollment_id: eid(2),
        },
    );
    let mut m = remove_other.membership().unwrap();
    m.enrollments.retain(|u| u.id != eid(2));
    m.retired.push(eid(2));
    remove_other
        .members
        .retain(|a| a.sign_key != ks[1].sign_key());
    remove_other.set_membership(&m);
    add(
        "daemon_cannot_remove_unrelated_active_peer",
        tokens(&remove_other, &ks[2]),
        false,
    );
    let mut enroll = draft(
        p,
        &ks[3],
        Change::Enroll {
            enrollment_id: eid(6),
            proofs: vec![],
        },
    );
    let mut m = enroll.membership().unwrap();
    m.enrollments
        .push(unit(6, Kind::Daemon, &[&ks[4]], enroll.issued_at));
    enroll.members.push(Member::new(
        "new daemon",
        Role::Daemon,
        ks[4].sign_key(),
        ks[4].noise_key(),
        enroll.issued_at,
    ));
    enroll.set_membership(&m);
    consent(&mut enroll, &[&ks[4]]);
    add(
        "mobile_scoped_enrollment_with_pairing_context",
        tokens(&enroll, &ks[3]),
        true,
    );
    let mut bad = enroll.clone();
    bad.signed_by = ks[2].sign_key();
    consent(&mut bad, &[&ks[4]]);
    add(
        "daemon_cannot_enroll_even_with_valid_pairing_proof",
        tokens(&bad, &ks[2]),
        false,
    );
    let mut legacy = enroll.clone();
    let mut m = legacy.membership().unwrap();
    m.enrollments.last_mut().unwrap().kind = Kind::LegacyDesktop;
    legacy.members.last_mut().unwrap().role = Role::Desktop;
    legacy.set_membership(&m);
    consent(&mut legacy, &[&ks[4]]);
    add(
        "new_enrollment_cannot_create_legacy_desktop",
        tokens(&legacy, &ks[3]),
        false,
    );
    let mut genesis = good.clone();
    genesis.version = 1;
    genesis.prev = None;
    genesis
        .members
        .retain(|a| [ks[0].sign_key(), ks[2].sign_key()].contains(&a.sign_key));
    let mut m = genesis.membership().unwrap();
    m.enrollments.retain(|u| u.id == eid(5));
    m.retired.clear();
    m.change = Change::Genesis { proofs: vec![] };
    genesis.set_membership(&m);
    consent(&mut genesis, &[&ks[0], &ks[2]]);
    add("new_computer_genesis", vec![raw(&genesis, &ks[0])], true);
    let mut legacy = p.roster().clone();
    legacy.version = 1;
    legacy.prev = None;
    let mut m = legacy.membership().unwrap();
    m.change = Change::Genesis { proofs: vec![] };
    legacy.set_membership(&m);
    consent(&mut legacy, &[&ks[0], &ks[1], &ks[2], &ks[3]]);
    add(
        "normal_genesis_cannot_create_migration_singletons",
        vec![raw(&legacy, &ks[0])],
        false,
    );
    let grouped = signed(&good, &ks[0]);
    let mut expire_group = draft(
        &grouped,
        &ks[1],
        Change::Expire {
            enrollment_ids: vec![eid(5)],
            evidence: evidence(&grouped, &ks, 140, true),
        },
    );
    expire_group.issued_at = 140;
    let mut m = expire_group.membership().unwrap();
    m.enrollments.retain(|u| u.id != eid(5));
    m.retired.push(eid(5));
    expire_group
        .members
        .retain(|a| ![ks[0].sign_key(), ks[2].sign_key()].contains(&a.sign_key));
    expire_group.set_membership(&m);
    let grouped_tokens = |r: &Roster| {
        let mut t = tokens(&good, &ks[0]);
        t.push(raw(r, &ks[1]));
        t
    };
    add(
        "full_group_terminal_evidence",
        grouped_tokens(&expire_group),
        true,
    );
    for defect in [
        "half_group",
        "online_half",
        "not_elapsed",
        "wrong_ring",
        "wrong_policy",
        "unknown_policy_head",
        "forged_activity_signature",
    ] {
        let mut r = expire_group.clone();
        let mut m = r.membership().unwrap();
        let mut ev =
            ActivityEvidence::parse(&evidence(&grouped, &ks, 140, true), &ks[8].sign_key())
                .unwrap();
        match defect {
            "half_group" => {
                ev.units
                    .iter_mut()
                    .find(|u| u.enrollment_id == eid(5))
                    .unwrap()
                    .member_keys
                    .pop();
            }
            "online_half" => {
                ev.units
                    .iter_mut()
                    .find(|u| u.enrollment_id == eid(5))
                    .unwrap()
                    .online = true
            }
            "not_elapsed" => {
                ev.units
                    .iter_mut()
                    .find(|u| u.enrollment_id == eid(5))
                    .unwrap()
                    .last_active = 139
            }
            "wrong_ring" => ev.ring_id = RingId::derive(&ks[7].sign_key()),
            "wrong_policy" => ev.policy_hash = eid(99),
            "unknown_policy_head" => ev.policy_head_hash = eid(99),
            _ => {}
        }
        if let Change::Expire { evidence, .. } = &mut m.change {
            *evidence = ev
                .sign(if defect == "forged_activity_signature" {
                    &ks[7]
                } else {
                    &ks[8]
                })
                .unwrap();
        }
        r.set_membership(&m);
        add(defect, grouped_tokens(&r), false);
    }
    let mut ancestor = draft(
        &grouped,
        &ks[2],
        Change::Expire {
            enrollment_ids: vec![eid(2)],
            evidence: evidence(p, &ks, 140, true),
        },
    );
    ancestor.issued_at = 140;
    let mut m = ancestor.membership().unwrap();
    m.enrollments.retain(|u| u.id != eid(2));
    m.retired.push(eid(2));
    ancestor.members.retain(|a| a.sign_key != ks[1].sign_key());
    ancestor.set_membership(&m);
    let mut t = tokens(&good, &ks[0]);
    t.push(raw(&ancestor, &ks[2]));
    add(
        "ancestor_policy_terminal_evidence_with_exact_current_head_signature",
        t,
        true,
    );
    let mut mismatched = remove.clone();
    mismatched.prev = Some(grouped.hash());
    add(
        "changed_predecessor_cannot_rebase_signed_departure",
        tokens(&mismatched, &ks[2]),
        false,
    );

    let original = raw(&remove, &ks[2]);
    let pieces = original.split('.').collect::<Vec<_>>();
    let mut changed: Value = serde_json::from_slice(&b64::decode(pieces[1]).unwrap()).unwrap();
    changed["prev"] = json!(eid(77));
    let changed = format!(
        "xro2.{}.{}",
        b64::encode(changed.to_string().as_bytes()),
        pieces[2]
    );
    assert_eq!(
        SignedRoster::parse(&changed).unwrap_err(),
        xshell_protocol::ring::RosterError::BadSignature
    );
    let mut t = h.iter().map(|r| r.token().to_string()).collect::<Vec<_>>();
    t.push(changed);
    add(
        "changed_predecessor_invalidates_original_signature",
        t,
        false,
    );
    // External terminal observations are not fabricated signed Roster versions.
    let mut observation =
        ActivityEvidence::parse(&evidence(p, &ks, 140, true), &ks[8].sign_key()).unwrap();
    observation.units.retain(|u| u.enrollment_id == eid(1));
    let observation = observation.sign(&ks[8]).unwrap();
    let mut grant = enroll.clone();
    grant.signed_by = ks[0].sign_key();
    grant.issued_at = 140;
    grant.members.last_mut().unwrap().added_at = 140;
    let mut m = grant.membership().unwrap();
    m.enrollments.last_mut().unwrap().enrolled_at = 140;
    grant.set_membership(&m);
    consent(&mut grant, &[&ks[4]]);
    let mut self_remove = draft(
        p,
        &ks[0],
        Change::Remove {
            enrollment_id: eid(1),
        },
    );
    self_remove.issued_at = 140;
    self_remove
        .members
        .retain(|a| a.sign_key != ks[0].sign_key());
    let mut m = self_remove.membership().unwrap();
    m.enrollments.retain(|u| u.id != eid(1));
    m.retired.push(eid(1));
    self_remove.set_membership(&m);
    for (name, candidate, accepted) in [
        ("learned_terminal_signer_cannot_enroll", grant, false),
        (
            "learned_terminal_signer_can_only_self_cleanup",
            self_remove,
            true,
        ),
    ] {
        let mut chain = RosterChain::from_chain(h.clone()).unwrap();
        chain.observe_activity(&observation).unwrap();
        let candidate = raw(&candidate, &ks[0]);
        let result = SignedRoster::parse(&candidate)
            .and_then(|r| chain.accept(&[r]))
            .is_ok();
        assert_eq!(result, accepted);
        cases.push(json!({"name":name,"tokens":h.iter().map(|r|r.token()).collect::<Vec<_>>(),"observations":[observation],"candidate":candidate,"accepted":accepted}));
    }
    let output = json!({"contract":"xshell-membership-v2","cases":cases});
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/ring/membership-v2.json");
    if std::env::var_os("X87_WRITE_VECTORS").is_some() {
        std::fs::write(&path, serde_json::to_string_pretty(&output).unwrap() + "\n").unwrap();
    }
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap()).unwrap(),
        output,
        "regenerate canonical Rust vectors explicitly"
    );
}

#[test]
fn maximum_legacy_membership_and_token_budget_are_enforced() {
    let ks = (1..=64).map(keys).collect::<Vec<_>>();
    let a = &ks[0];
    let g = SignedRoster::genesis(a, a.noise_key(), "creator", "ws://127.0.0.1:8787", 100).unwrap();
    let all = g
        .next(a, 101, |r| {
            for k in &ks[1..] {
                r.add(Member::new(
                    &"x".repeat(64),
                    Role::Desktop,
                    k.sign_key(),
                    k.noise_key(),
                    101,
                ));
            }
        })
        .unwrap();
    let mut r = all.roster().clone();
    r.v = 2;
    r.version += 1;
    r.prev = Some(all.hash());
    r.issued_at = 102;
    r.set_membership(&Membership {
        enrollments: ks
            .iter()
            .enumerate()
            .map(|(i, k)| unit(i as u8 + 1, Kind::LegacyDesktop, &[k], 102))
            .collect(),
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: Some(30),
            activity_key: keys(9).sign_key(),
        },
        change: Change::Upgrade { proofs: vec![] },
    });
    let u = r.sign(a).unwrap();
    assert!(u.token().len() <= xshell_protocol::ring::roster::MAX_ROSTER_TOKEN);
    RosterChain::from_chain(vec![g, all, u]).unwrap();
    let mut m = r.membership().unwrap();
    for n in 1000u32..5000 {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        m.retired.push(b64::encode(&bytes));
    }
    r.set_membership(&m);
    assert!(raw(&r, a).len() > xshell_protocol::ring::roster::MAX_ROSTER_TOKEN);
    assert!(SignedRoster::parse(&raw(&r, a)).is_err());
}

#[cfg(feature = "test-relay")]
#[test]
fn departure_does_not_trust_bare_ack_forged_or_withheld_absence() {
    use std::{net::TcpListener, sync::Arc, time::Duration};
    use tungstenite::Message;
    use xshell_protocol::ring::relay::{DepartureClient, DepartureStatus};
    for mode in ["bare_ack", "forged_chain", "opaque_absence", "withheld"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (a, _, mut r) = singleton_upgrade();
        let g = SignedRoster::genesis(&a, a.noise_key(), "retained", &url, 100).unwrap();
        r["relayUrl"] = json!(url);
        r["prev"] = json!(g.hash());
        let u = SignedRoster::parse(&token(&r, &a)).unwrap();
        let chain = RosterChain::from_chain(vec![g, u]).unwrap();
        let target = chain.head().roster().membership().unwrap().enrollments[0].clone();
        let foreign = SignedRoster::genesis(&keys(2), keys(2).noise_key(), "foreign", &url, 100)
            .unwrap()
            .token()
            .to_string();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut ws = tungstenite::accept(socket).unwrap();
            ws.send(Message::text(
                json!({"t":"challenge","v":1,"nonce":b64::encode(&[7;32]),"rosterVersion":2})
                    .to_string(),
            ))
            .unwrap();
            let request: Value =
                serde_json::from_str(ws.read().unwrap().into_text().unwrap().as_str()).unwrap();
            let request_id = request["id"].clone();
            if mode == "withheld" {
                std::thread::sleep(Duration::from_millis(400));
                return;
            }
            if mode == "opaque_absence" {
                ws.send(Message::text(
                    json!({"t":"absent","id":request_id,"removed":true}).to_string(),
                ))
                .unwrap();
                let _ = ws.close(None);
                return;
            }
            if mode == "forged_chain" {
                ws.send(Message::text(
                    json!({"t":"roster.chain","id":request_id,"rosters":[foreign],"more":false})
                        .to_string(),
                ))
                .unwrap();
                return;
            }
            ws.send(Message::text(
                json!({"t":"roster.chain","id":request_id,"rosters":[],"more":false}).to_string(),
            ))
            .unwrap();
            let put: Value =
                serde_json::from_str(ws.read().unwrap().into_text().unwrap().as_str()).unwrap();
            assert_eq!(put["t"], "leave.put");
            ws.send(Message::text(json!({"t":"ok","id":put["id"]}).to_string()))
                .unwrap();
            let sync: Value =
                serde_json::from_str(ws.read().unwrap().into_text().unwrap().as_str()).unwrap();
            ws.send(Message::text(
                json!({"t":"roster.chain","id":sync["id"],"rosters":[],"more":false}).to_string(),
            ))
            .unwrap();
        });
        let mut client =
            DepartureClient::connect(chain, Arc::new(a), target, None, Duration::from_millis(250))
                .unwrap();
        let result = client.complete();
        assert!(
            !matches!(result, Ok(DepartureStatus::Absent(_))),
            "{mode} cannot prove absence"
        );
        assert_eq!(client.chain().head().version(), 2);
        server.join().unwrap();
    }
}

// #87: a phone enrolls into a v2 Ring as its own Mobile unit and signs its own consent.
mod mobile {
    use super::*;
    use xshell_protocol::ring::membership::{consent_proposal_hash, verify_enroll_proposal};
    use xshell_protocol::ring::pairing::adoption::{
        apply_consent, enroll_proposal, AdoptionConsent, AdoptionIntent, JoinTarget, PublicIdentity,
    };
    use xshell_protocol::ring::pairing::PairError;

    fn identity(k: &DeviceKeys, name: &str) -> PublicIdentity {
        PublicIdentity {
            sign_key: k.sign_key(),
            noise_key: k.noise_key(),
            name: name.into(),
        }
    }
    fn intent(phone: &DeviceKeys) -> AdoptionIntent {
        AdoptionIntent {
            operation: eid(77),
            expires_at: 103 + 300,
            target: JoinTarget::Mobile {
                phone: identity(phone, "my phone"),
            },
        }
    }
    fn chain() -> (Vec<DeviceKeys>, RosterChain) {
        let (ks, versions) = upgrade_multi();
        (ks, RosterChain::from_chain(versions).unwrap())
    }
    fn consent_for(
        r: &Roster,
        i: &AdoptionIntent,
        k: &DeviceKeys,
        hash: &[u8; 32],
    ) -> AdoptionConsent {
        let context = pairing_consent_context(r, hash).unwrap();
        AdoptionConsent {
            operation: i.operation.clone(),
            proposal_hash: consent_proposal_hash(r).unwrap(),
            proofs: vec![ConsentProof {
                key: k.sign_key(),
                expires_at: i.expires_at,
                context: context.clone(),
                signature: Signature::from_bytes(
                    k.sign(&consent_message(r, i.expires_at, &context).unwrap())
                        .unwrap(),
                ),
            }],
        }
    }

    #[test]
    fn mobile_target_shape_kind_and_name() {
        let phone = keys(30);
        let i = intent(&phone);
        let v = serde_json::to_value(&i.target).unwrap();
        assert_eq!(v["kind"], json!("mobile"));
        assert_eq!(v["phone"]["name"], json!("my phone"));
        let back: JoinTarget = serde_json::from_value(v).unwrap();
        assert_eq!(back, i.target);
        assert_eq!(i.target.enrollment_kind(), Kind::Mobile);
        assert_eq!(i.target.identities().len(), 1);
        assert_eq!(i.target.identities()[0].1, Role::Mobile);
        assert_eq!(i.target.principal().sign_key, phone.sign_key());
        i.validate().unwrap();
        for bad in ["", " padded ", "line\nbreak"] {
            let t = JoinTarget::Mobile {
                phone: identity(&phone, bad),
            };
            assert!(t.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn mobile_enroll_is_a_valid_unsigned_proposal_completed_only_by_the_phones_proof() {
        let (ks, mut c) = chain();
        let phone = keys(30);
        let i = intent(&phone);
        let p = enroll_proposal(c.head(), ks[0].sign_key(), &i, 103).unwrap();
        verify_enroll_proposal(c.head(), &p).unwrap();
        i.target.matches_proposal(&p).unwrap();
        let m = p.membership().unwrap();
        let unit = m.unit(&i.operation).unwrap();
        assert_eq!(unit.kind, Kind::Mobile);
        assert_eq!(unit.members, vec![phone.sign_key()]);
        assert_eq!(p.member(&phone.sign_key()).unwrap().role, Role::Mobile);
        // Unsigned consent: the signed successor does not verify.
        assert!(c.clone().accept(&[signed(&p, &ks[0])]).is_err());
        let hash = [9u8; 32];
        // Another key's (even a member Desktop's) proof is not the phone's consent.
        assert_eq!(
            apply_consent(&p, &i, &hash, consent_for(&p, &i, &ks[0], &hash)).unwrap_err(),
            PairError::Crypto
        );
        // A proof bound to another transcript is not consent for this session.
        assert_eq!(
            apply_consent(&p, &i, &hash, consent_for(&p, &i, &phone, &[8; 32])).unwrap_err(),
            PairError::Crypto
        );
        let done = apply_consent(&p, &i, &hash, consent_for(&p, &i, &phone, &hash)).unwrap();
        c.accept(&[signed(&done, &ks[0])]).unwrap();
        assert_eq!(
            c.head().member(&phone.sign_key()).unwrap().role,
            Role::Mobile
        );
    }

    #[test]
    fn mobile_target_refuses_a_proposal_that_changed_kind_role_or_keys() {
        let (ks, c) = chain();
        let phone = keys(30);
        let i = intent(&phone);
        let daemon = AdoptionIntent {
            target: JoinTarget::StandaloneDaemon {
                daemon: identity(&phone, "my phone"),
            },
            ..i.clone()
        };
        let as_daemon = enroll_proposal(c.head(), ks[0].sign_key(), &daemon, 103).unwrap();
        assert!(i.target.matches_proposal(&as_daemon).is_err());
        let other = intent(&keys(31));
        let p = enroll_proposal(c.head(), ks[0].sign_key(), &other, 103).unwrap();
        assert!(i.target.matches_proposal(&p).is_err());
        // An existing member (the upgraded phone) cannot be enrolled a second time.
        assert!(enroll_proposal(c.head(), ks[0].sign_key(), &intent(&ks[3]), 103).is_err());
        // A Daemon is not an approver.
        assert!(enroll_proposal(c.head(), ks[2].sign_key(), &i, 103).is_err());
    }
}

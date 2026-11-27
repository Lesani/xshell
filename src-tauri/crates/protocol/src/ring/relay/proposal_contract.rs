//! Production-workerd proposal contracts; the in-process TestRelay cannot run v2.
use super::*;
pub const SCENARIOS: &[MembershipScenario] = &[
    (
        "proposal_expired_pipe_write_stops",
        proposal_expired_pipe_write_stops,
    ),
    (
        "proposal_original_deadline_fences_late_consent",
        proposal_original_deadline_fences_late_consent,
    ),
    (
        "proposal_explicit_decline_creates_no_membership",
        proposal_explicit_decline_creates_no_membership,
    ),
    (
        "proposal_legal_64k_tokens_multipage_near_16m",
        proposal_legal_64k_tokens_multipage_near_16m,
    ),
    (
        "proposal_final_chain_version4096_bound",
        proposal_final_chain_version4096_bound,
    ),
    (
        "proposal_challenge_and_descriptor_binding",
        proposal_challenge_and_descriptor_binding,
    ),
    (
        "proposal_reader_is_not_member_authority",
        proposal_reader_is_not_member_authority,
    ),
    (
        "proposal_cursor_replay_cancel_and_creator_loss",
        proposal_cursor_replay_cancel_and_creator_loss,
    ),
    (
        "proposal_continuation_real_noise",
        proposal_continuation_real_noise,
    ),
    (
        "proposal_human_wait_over_15_seconds",
        proposal_human_wait_over_15_seconds,
    ),
    (
        "proposal_mobile_guest_signs_own_enrollment",
        proposal_mobile_guest_signs_own_enrollment,
    ),
    (
        "proposal_mobile_guest_decline_creates_no_membership",
        proposal_mobile_guest_decline_creates_no_membership,
    ),
];

fn adoption_draft(
    chain: &RosterChain,
    signer: &DeviceKeys,
    intent: &crate::ring::pairing::adoption::AdoptionIntent,
) -> Roster {
    let mut r = draft(
        chain,
        signer,
        RosterChange::Enroll {
            enrollment_id: intent.operation.clone(),
            proofs: vec![],
        },
    );
    let mut m = r.membership().unwrap();
    let identities = intent.target.identities();
    for (p, role) in &identities {
        r.members.push(Member::new(
            &p.name,
            *role,
            p.sign_key,
            p.noise_key,
            r.issued_at,
        ));
    }
    m.enrollments.push(Enrollment {
        id: intent.operation.clone(),
        kind: intent.target.enrollment_kind(),
        members: identities.iter().map(|(p, _)| p.sign_key).collect(),
        enrolled_at: r.issued_at,
    });
    r.set_membership(&m);
    r
}
fn adoption_intent(
    computer: bool,
    guest: &DeviceKeys,
    daemon: &DeviceKeys,
) -> crate::ring::pairing::adoption::AdoptionIntent {
    use crate::ring::pairing::adoption::{AdoptionIntent, JoinTarget, PublicIdentity};
    let public = |k: &DeviceKeys| PublicIdentity {
        sign_key: k.sign_key(),
        noise_key: k.noise_key(),
        name: "joining computer".into(),
    };
    AdoptionIntent {
        operation: b64::encode(&keys().sign_key().as_bytes()[..]),
        expires_at: now() + 120,
        target: if computer {
            JoinTarget::Computer {
                desktop: public(guest),
                daemon: public(daemon),
            }
        } else {
            JoinTarget::StandaloneDaemon {
                daemon: public(guest),
            }
        },
    }
}
struct ConsentAudit<'a> {
    keys: &'a DeviceKeys,
    consents: std::sync::atomic::AtomicUsize,
}
impl Signer for ConsentAudit<'_> {
    fn sign_key(&self) -> crate::ring::SignKey {
        self.keys.sign_key()
    }
    fn sign(&self, msg: &[u8]) -> Result<[u8; 64], crate::ring::SignError> {
        if msg.starts_with(crate::ring::membership::CONSENT_CONTEXT.as_bytes()) {
            self.consents
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.keys.sign(msg)
    }
}
#[derive(Clone, Copy)]
enum Expected {
    Approve,
    Decline,
    Expire,
}
fn adoption_roundtrip(
    t: &RelayTarget,
    computer: bool,
    human_wait: std::time::Duration,
    expected: Expected,
) {
    let accept = matches!(expected, Expected::Approve);
    use super::super::pair::{
        adopt_as_guest, approve_adoption, AdoptionGuestRequest, AdoptionHostRequest, PairOptions,
        PairPipe,
    };
    use super::super::VerifiedProposal;
    use crate::ring::pairing::adoption::AdoptionSigners;
    use crate::ring::pairing::PairError;
    let (ks, chain) = upgraded(t, false);
    let creator = Arc::new(dial(t, chain.clone(), ks[0].clone()));
    let guest = keys();
    let daemon = keys();
    let intent = adoption_intent(computer, &guest, &daemon);
    let secret = crate::ring::device_code::DeviceCode::generate()
        .unwrap()
        .secret();
    let opts = PairOptions {
        tls: t.tls.clone(),
        ..Default::default()
    };
    let pipe = PairPipe::open(&t.url, &secret.slot(), &opts).unwrap();
    let until = std::time::Instant::now()
        + std::time::Duration::from_secs(if matches!(expected, Expected::Expire) {
            2
        } else {
            110
        });
    let (host_keys, host_secret, host_chain, host_client) = (
        ks[0].clone(),
        secret.clone(),
        chain.clone(),
        creator.clone(),
    );
    let ring = chain.head().ring_id().clone();
    let host = std::thread::spawn(move || {
        approve_adoption(
            pipe,
            &AdoptionHostRequest {
                keys: &host_keys,
                secret: &host_secret,
                ring_id: &ring,
                until,
            },
            None,
            |intent, hash| {
                let proposal = adoption_draft(&host_chain, &host_keys, intent);
                let handle = host_client
                    .create_proposal(
                        intent.operation.clone(),
                        intent.target.principal().sign_key,
                        hash,
                        intent.expires_at,
                        &proposal,
                    )
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                Ok(VerifiedProposal {
                    chain: host_chain.clone(),
                    proposal,
                    handle,
                })
            },
            |signed| {
                host_client
                    .publish_roster(signed)
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                Ok(crate::ring::pairing::Joined {
                    ring_id: signed.ring_id().clone(),
                    relay_url: signed.roster().relay_url.clone(),
                    version: signed.version(),
                    hash: signed.hash(),
                    signed_by: host_keys.sign_key(),
                })
            },
            |handle| {
                let _ = host_client.cancel_proposal(handle);
            },
        )
    });
    let mut confirmations = 0;
    let principal = ConsentAudit {
        keys: &guest,
        consents: Default::default(),
    };
    let counterpart = ConsentAudit {
        keys: &daemon,
        consents: Default::default(),
    };
    let result = adopt_as_guest(
        &AdoptionGuestRequest {
            relay_url: &t.url,
            secret: &secret,
            keys: guest.clone(),
            signers: AdoptionSigners {
                principal: &principal,
                companion: computer.then_some(&counterpart as &dyn Signer),
            },
            intent: &intent,
            until,
            approver: Some(ks[0].sign_key()),
        },
        &opts,
        None,
        |request| {
            confirmations += 1;
            assert_eq!(
                principal.consents.load(std::sync::atomic::Ordering::SeqCst),
                0
            );
            assert_eq!(
                counterpart
                    .consents
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
            assert_eq!(request.verified.chain.head().hash(), chain.head().hash());
            assert!(
                request.verified.proposal.membership().unwrap().change
                    == RosterChange::Enroll {
                        enrollment_id: intent.operation.clone(),
                        proofs: vec![]
                    }
            );
            // No membership has been published while the actual local callback is pending.
            assert_eq!(creator.roster().version(), chain.head().version());
            std::thread::sleep(human_wait);
            Ok(!matches!(expected, Expected::Decline))
        },
    );
    let host_result = host.join().unwrap();
    assert_eq!(
        principal.consents.load(std::sync::atomic::Ordering::SeqCst),
        usize::from(accept)
    );
    assert_eq!(
        counterpart
            .consents
            .load(std::sync::atomic::Ordering::SeqCst),
        usize::from(accept && computer)
    );
    if !accept {
        let error = if matches!(expected, Expected::Expire) {
            PairError::Expired
        } else {
            PairError::Cancelled
        };
        assert_eq!(confirmations, 1);
        assert_eq!(result.unwrap_err(), error);
        assert_eq!(host_result.unwrap_err(), error);
        assert_eq!(creator.roster().version(), chain.head().version());
        return;
    }
    let result = result.unwrap();
    host_result.unwrap();
    assert_eq!(confirmations, 1);
    assert_eq!(result.0.head().version(), chain.head().version() + 1);
    assert!(result.0.head().member(&guest.sign_key()).is_some());
    assert_eq!(
        result.0.head().member(&daemon.sign_key()).is_some(),
        computer
    );
}
/// #87: a phone scans a v2 offer (head anchor), reads its frozen `Enroll` through the proposal
/// reader as the guest, signs its own Pairing consent and gets the published version.
fn mobile_roundtrip(t: &RelayTarget, accept: bool) {
    use super::super::pair::{
        adopt_mobile_as_guest_with_ready, approve_mobile_adoption, MobileAdoptionRequest,
        MobileHostRequest, PairOptions, PairPipe, Prepared,
    };
    use super::super::VerifiedProposal;
    use crate::ring::pairing::{OfferHead, PairError, PairSecret, PairingOffer};
    let (ks, chain) = upgraded(t, false);
    let creator = Arc::new(dial(t, chain.clone(), ks[0].clone()));
    let phone = keys();
    let secret = PairSecret::generate().unwrap();
    let offer = PairingOffer {
        ring_id: chain.ring_id().clone(),
        relay_url: chain.head().roster().relay_url.clone(),
        sign_key: ks[0].sign_key(),
        noise_key: ks[0].noise_key(),
        secret: secret.clone(),
        expires_at: now() + 120,
        head: Some(OfferHead {
            version: chain.head().version(),
            hash: chain.head().hash(),
        }),
    };
    let offer = PairingOffer::parse(&offer.encode()).unwrap();
    let opts = PairOptions {
        tls: t.tls.clone(),
        ..Default::default()
    };
    let pipe = PairPipe::open(&t.url, &secret.slot(), &opts).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(110);
    let (host_keys, host_chain, host_client, ring) = (
        ks[0].clone(),
        chain.clone(),
        creator.clone(),
        chain.ring_id().clone(),
    );
    let expires_at = offer.expires_at;
    let host = std::thread::spawn(move || {
        approve_mobile_adoption(
            pipe,
            &MobileHostRequest {
                keys: &host_keys,
                secret: &secret,
                ring_id: &ring,
                name: "approver",
                until,
                offer_expires_at: expires_at,
                abort: None,
            },
            None,
            |intent, hash| {
                let proposal = crate::ring::pairing::adoption::enroll_proposal(
                    host_chain.head(),
                    host_keys.sign_key(),
                    intent,
                    now(),
                )?;
                let handle = host_client
                    .create_proposal(
                        intent.operation.clone(),
                        intent.target.principal().sign_key,
                        hash,
                        intent.expires_at,
                        &proposal,
                    )
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                Ok(Prepared::Proposal(Box::new(VerifiedProposal {
                    chain: host_chain.clone(),
                    proposal,
                    handle,
                })))
            },
            |signed| {
                host_client
                    .publish_roster(signed)
                    .map_err(|e| PairError::Relay(e.to_string()))?;
                Ok(crate::ring::pairing::Joined {
                    ring_id: signed.ring_id().clone(),
                    relay_url: signed.roster().relay_url.clone(),
                    version: signed.version(),
                    hash: signed.hash(),
                    signed_by: host_keys.sign_key(),
                })
            },
            |handle| {
                let _ = host_client.cancel_proposal(handle);
            },
        )
    });
    let mut confirmations = 0;
    let result = adopt_mobile_as_guest_with_ready(
        &MobileAdoptionRequest {
            offer: &offer,
            keys: phone.clone(),
            name: "my phone",
            until,
        },
        &opts,
        None,
        || Ok(()),
        |request| {
            confirmations += 1;
            assert_eq!(request.verified.chain.head().hash(), chain.head().hash());
            let m = request.verified.proposal.membership().unwrap();
            let RosterChange::Enroll { enrollment_id, .. } = &m.change else {
                panic!("not Enroll");
            };
            let unit = m.unit(enrollment_id).unwrap();
            assert_eq!(unit.kind, EnrollmentKind::Mobile);
            assert_eq!(unit.members, vec![phone.sign_key()]);
            assert_eq!(creator.roster().version(), chain.head().version());
            Ok(accept)
        },
    );
    let host_result = host.join().unwrap();
    assert_eq!(confirmations, 1);
    if !accept {
        assert_eq!(result.unwrap_err(), PairError::Cancelled);
        assert_eq!(host_result.unwrap_err(), PairError::Cancelled);
        assert_eq!(creator.roster().version(), chain.head().version());
        return;
    }
    host_result.unwrap();
    let enrolled = result.unwrap();
    assert!(!enrolled.already_enrolled);
    let head = enrolled.chain.head();
    assert_eq!(head.version(), chain.head().version() + 1);
    let member = head.member(&phone.sign_key()).unwrap();
    assert_eq!(member.role, Role::Mobile);
    assert_eq!(member.name, "my phone");
    let RosterChange::Enroll { proofs, .. } = head.roster().membership().unwrap().change else {
        panic!("not Enroll");
    };
    assert_eq!(proofs.len(), 1);
    assert_eq!(proofs[0].key, phone.sign_key());
    assert_eq!(
        proofs[0].context.kind,
        crate::ring::membership::ConsentFlow::Pairing
    );
    assert_eq!(creator.roster().version(), head.version());
}
pub fn proposal_mobile_guest_signs_own_enrollment(t: &RelayTarget) {
    mobile_roundtrip(t, true);
}
pub fn proposal_mobile_guest_decline_creates_no_membership(t: &RelayTarget) {
    mobile_roundtrip(t, false);
}
pub fn proposal_continuation_real_noise(t: &RelayTarget) {
    adoption_roundtrip(t, true, std::time::Duration::ZERO, Expected::Approve);
}
pub fn proposal_human_wait_over_15_seconds(t: &RelayTarget) {
    adoption_roundtrip(
        t,
        false,
        std::time::Duration::from_secs(16),
        Expected::Approve,
    );
}

use super::super::contract::RawConn;
use super::super::proposal::ProposalHandle;
use super::super::ProposalReader;
use crate::ring::pairing::adoption::AdoptionSigners;
use crate::ring::pairing::{Guest, Host};

pub struct Prepared {
    pub client: RingClient,
    pub chain: RosterChain,
    pub guest: Arc<DeviceKeys>,
    pub hash: [u8; 32],
    pub proposal: Roster,
    pub handle: ProposalHandle,
}
/// Full cryptographic handshake, including both independent possession signatures. Only
/// the metadata transport tests below omit the pair socket; the exchange tests use both.
pub fn prepare(t: &RelayTarget) -> Prepared {
    let (ks, chain) = upgraded(t, false);
    prepare_with_chain(t, ks[0].clone(), chain)
}
fn prepare_with_chain(t: &RelayTarget, approver: Arc<DeviceKeys>, chain: RosterChain) -> Prepared {
    let client = dial(t, chain.clone(), approver.clone());
    let guest = keys();
    let daemon = keys();
    let intent = adoption_intent(true, &guest, &daemon);
    let secret = crate::ring::device_code::DeviceCode::generate()
        .unwrap()
        .secret();
    let (mut g, m1) = Guest::start(&guest, &secret, None).unwrap();
    let mut h = Host::start(&approver, &secret).unwrap();
    h.read_start(&m1).unwrap();
    let m2 = h.hello(chain.head().ring_id(), "approver").unwrap();
    g.read_hello(&m2).unwrap();
    let m3 = g
        .join_adoption(
            &guest,
            &intent,
            &AdoptionSigners {
                principal: &*guest,
                companion: Some(&*daemon),
            },
        )
        .unwrap();
    assert_eq!(h.read_adoption(&m3).unwrap(), intent);
    let hash = h.final_handshake_hash().unwrap();
    assert_eq!(g.final_handshake_hash().unwrap(), hash);
    let proposal = adoption_draft(&chain, &approver, &intent);
    let handle = client
        .create_proposal(
            intent.operation.clone(),
            guest.sign_key(),
            hash,
            intent.expires_at,
            &proposal,
        )
        .unwrap();
    // Authenticate the final Noise peer and round-trip the exact descriptor encrypted.
    let message = h.encrypt(&serde_json::to_vec(&handle).unwrap()).unwrap();
    let checked: ProposalHandle = serde_json::from_slice(&g.decrypt(&message).unwrap()).unwrap();
    assert_eq!(checked, handle);
    Prepared {
        client,
        chain,
        guest,
        hash,
        proposal,
        handle,
    }
}
fn admit(t: &RelayTarget, p: &Prepared) -> RawConn {
    let mut raw = RawConn::open(t, p.chain.head().ring_id()).unwrap();
    let (nonce, _) = raw.challenge();
    raw.send(&serde_json::json!({"t":"proposal.read","handle":p.handle,"sig":Signature::from_bytes(p.guest.sign(&p.handle.read_message(&t.origin(),&nonce)).unwrap())}).to_string()).unwrap();
    assert!(matches!(
        raw.frame(WAIT),
        Some(super::super::wire::RelayFrame::ProposalReady { .. })
    ));
    raw
}
pub fn proposal_challenge_and_descriptor_binding(t: &RelayTarget) {
    let p = prepare(t);
    for field in [
        "operation",
        "head",
        "resource",
        "grant",
        "context",
        "expiresAt",
        "guest",
        "relayUrl",
        "ringId",
        "version",
    ] {
        let mut raw = RawConn::open(t, p.chain.head().ring_id()).unwrap();
        let (nonce, _) = raw.challenge();
        let mut handle = serde_json::to_value(&p.handle).unwrap();
        handle[field] = match field {
            "expiresAt" | "version" => serde_json::json!(handle[field].as_u64().unwrap() + 1),
            "relayUrl" => serde_json::json!("ws://127.0.0.1:9"),
            "guest" => serde_json::json!(keys().sign_key()),
            _ => serde_json::json!(id(224)),
        };
        let changed: ProposalHandle = serde_json::from_value(handle.clone()).unwrap();
        raw.send(&serde_json::json!({"t":"proposal.read","handle":handle,"sig":Signature::from_bytes(p.guest.sign(&changed.read_message(&t.origin(),&nonce)).unwrap())}).to_string()).unwrap();
        assert_eq!(
            raw.error(WAIT).map(|r| r.0),
            Some(super::super::wire::ErrorCode::BadSignature),
            "{field}"
        );
        assert_eq!(raw.close_code(WAIT), Some(4003));
    }
    let mut first = RawConn::open(t, p.chain.head().ring_id()).unwrap();
    let (nonce, _) = first.challenge();
    let replay=serde_json::json!({"t":"proposal.read","handle":p.handle,"sig":Signature::from_bytes(p.guest.sign(&p.handle.read_message(&t.origin(),&nonce)).unwrap())}).to_string();
    let mut other = RawConn::open(t, p.chain.head().ring_id()).unwrap();
    other.challenge();
    other.send(&replay).unwrap();
    assert_eq!(
        other.error(WAIT).unwrap().0,
        super::super::wire::ErrorCode::BadSignature
    );
    let mut leaked = RawConn::open(t, p.chain.head().ring_id()).unwrap();
    let (nonce, _) = leaked.challenge();
    leaked.send(&serde_json::json!({"t":"proposal.read","handle":p.handle,"sig":Signature::from_bytes(keys().sign(&p.handle.read_message(&t.origin(),&nonce)).unwrap())}).to_string()).unwrap();
    assert_eq!(
        leaked.error(WAIT).unwrap().0,
        super::super::wire::ErrorCode::BadSignature
    );
    let verified = ProposalReader::connect(&p.handle, &*p.guest, t.tls.clone(), WAIT)
        .unwrap()
        .fetch()
        .unwrap();
    assert_eq!(verified.proposal, p.proposal);
    assert_eq!(verified.chain.head().hash(), p.chain.head().hash());
    assert_eq!(verified.handle.context, b64::encode(&p.hash));
}
pub fn proposal_reader_is_not_member_authority(t: &RelayTarget) {
    let p = prepare(t);
    let frames = vec![
        serde_json::json!({"t":"auth","signKey":p.guest.sign_key(),"sig":Signature::from_bytes(p.guest.sign(b"not ordinary auth").unwrap())}),
        serde_json::json!({"t":"auth.chain","rosters":[]}),
        serde_json::json!({"t":"roster.get","id":1,"since":0}),
        serde_json::json!({"t":"roster.put","id":1,"roster":p.chain.head().token()}),
        serde_json::json!({"t":"env","to":p.chain.head().roster().signed_by,"payload":"AA"}),
        serde_json::json!({"t":"activity.key","id":1}),
        serde_json::json!({"t":"activity.get","id":1}),
        serde_json::json!({"t":"activity.preview","id":1,"policy":p.proposal.membership().unwrap().policy}),
        serde_json::json!({"t":"state","foreground":true}),
        serde_json::json!({"t":"push","id":1,"blob":"xpb1.abc","sealedPayload":"AA","collapseId":"x"}),
        serde_json::json!({"t":"entitlement.put","id":1,"token":"x"}),
        serde_json::json!({"t":"proposal.cancel","id":1,"grant":p.handle.grant}),
        serde_json::json!({"t":"leave.put","id":1,"roster":p.chain.head().token()}),
        serde_json::json!({"t":"unknown"}),
    ];
    for f in frames {
        let mut reader = admit(t, &p);
        reader.send(&f.to_string()).unwrap();
        assert_eq!(
            reader.error(WAIT).unwrap().0,
            super::super::wire::ErrorCode::BadRequest,
            "{f}"
        );
        assert_eq!(reader.close_code(WAIT), Some(4000));
    }
    assert_eq!(p.client.roster().version(), p.chain.head().version());
    assert!(p
        .client
        .members()
        .iter()
        .all(|m| m.member.sign_key != p.guest.sign_key()));
    assert!(p.client.probe_start().unwrap().wait(WAIT).is_ok());
}
pub fn proposal_cursor_replay_cancel_and_creator_loss(t: &RelayTarget) {
    let p = prepare(t);
    let mut first = admit(t, &p);
    // One active reader, even with a new valid challenge and signature.
    let mut second = RawConn::open(t, p.chain.head().ring_id()).unwrap();
    let (nonce, _) = second.challenge();
    second.send(&serde_json::json!({"t":"proposal.read","handle":p.handle,"sig":Signature::from_bytes(p.guest.sign(&p.handle.read_message(&t.origin(),&nonce)).unwrap())}).to_string()).unwrap();
    assert_eq!(
        second.error(WAIT).unwrap().0,
        super::super::wire::ErrorCode::TooMany
    );
    first.send(r#"{"t":"proposal.get","cursor":0}"#).unwrap();
    assert!(matches!(
        first.frame(WAIT),
        Some(super::super::wire::RelayFrame::ProposalPage { .. })
    ));
    drop(first);
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut replay = admit(t, &p);
    replay.send(r#"{"t":"proposal.get","cursor":0}"#).unwrap();
    assert!(matches!(
        replay.frame(WAIT),
        Some(super::super::wire::RelayFrame::ProposalPage { .. })
    ));
    replay.send(r#"{"t":"proposal.get","cursor":0}"#).unwrap();
    assert_eq!(
        replay.error(WAIT).unwrap().0,
        super::super::wire::ErrorCode::BadRequest
    );
    p.client.cancel_proposal(&p.handle).unwrap();
    assert!(ProposalReader::connect(&p.handle, &*p.guest, t.tls.clone(), WAIT).is_err());
    let q = prepare(t);
    let mut reader = admit(t, &q);
    drop(q.client);
    assert_eq!(reader.close_code(WAIT), Some(4003));
    assert!(ProposalReader::connect(&q.handle, &*q.guest, t.tls.clone(), WAIT).is_err());
}

fn history(
    t: &RelayTarget,
    versions: usize,
    token_bytes: Option<usize>,
) -> (Arc<DeviceKeys>, RosterChain) {
    let a = keys();
    let initial = SignedRoster::genesis(&*a, a.noise_key(), "legacy", &t.url, now()).unwrap();
    let mut r = initial.roster().clone();
    let mut tokens = Vec::new();
    for i in 0..versions {
        r.version = i as u64 + 1;
        r.prev = tokens.last().map(SignedRoster::hash);
        if let Some(size) = token_bytes {
            r.extra.insert("padding".into(), serde_json::json!(""));
            let base = r.sign(&*a).unwrap().token().len();
            // Base64url has gaps;65536 is representable at every payload alignment.
            let start = (size - base) * 3 / 4;
            let mut found = false;
            for n in start.saturating_sub(3)..=start + 3 {
                r.extra
                    .insert("padding".into(), serde_json::json!("p".repeat(n)));
                let payload = b64::encode(&serde_json::to_vec(&r).unwrap());
                if 5 + payload.len() + 1 + 86 == size {
                    found = true;
                    break;
                }
            }
            assert!(found);
        }
        let signed = r.sign(&*a).unwrap();
        if let Some(n) = token_bytes {
            assert_eq!(signed.token().len(), n);
        }
        tokens.push(signed);
    }
    let last = tokens.last().unwrap();
    let key = RingClient::activity_key(&t.url, last.ring_id(), t.tls.clone(), WAIT).unwrap();
    let mut upgrade = last.roster().clone();
    upgrade.extra.clear();
    upgrade.v = 2;
    upgrade.version += 1;
    upgrade.prev = Some(last.hash());
    upgrade.set_membership(&Membership {
        enrollments: vec![unit(
            1,
            EnrollmentKind::LegacyDesktop,
            &[&a],
            upgrade.issued_at,
        )],
        retired: vec![],
        policy: RingPolicy {
            inactivity_seconds: None,
            activity_key: key,
        },
        change: RosterChange::Upgrade { proofs: vec![] },
    });
    tokens.push(upgrade.sign(&*a).unwrap());
    (a, RosterChain::from_chain(tokens).unwrap())
}
pub fn proposal_legal_64k_tokens_multipage_near_16m(t: &RelayTarget) {
    let (a, chain) = history(t, 255, Some(65536));
    let p = prepare_with_chain(t, a, chain);
    let verified = ProposalReader::connect(&p.handle, &*p.guest, t.tls.clone(), WAIT)
        .unwrap()
        .fetch()
        .unwrap();
    assert_eq!(verified.chain.versions().len(), 256);
    assert_eq!(verified.chain.versions()[0].token().len(), 65536);
    assert_eq!(verified.chain.head().hash(), p.chain.head().hash());
    assert_eq!(verified.proposal, p.proposal);
}
pub fn proposal_final_chain_version4096_bound(t: &RelayTarget) {
    let (a, chain) = history(t, 4094, None);
    let p = prepare_with_chain(t, a, chain);
    let verified = ProposalReader::connect(&p.handle, &*p.guest, t.tls.clone(), WAIT)
        .unwrap()
        .fetch()
        .unwrap();
    assert_eq!(verified.proposal.version, 4096);
    let (a, full) = history(t, 4095, None);
    let client = dial(t, full.clone(), a.clone());
    let guest = keys();
    let daemon = keys();
    let intent = adoption_intent(true, &guest, &daemon);
    let proposal = adoption_draft(&full, &a, &intent);
    // Reject admission at version4097 before any temporary read authority is created.
    assert!(client
        .create_proposal(
            intent.operation,
            guest.sign_key(),
            p.hash,
            intent.expires_at,
            &proposal
        )
        .is_err());
}

pub fn proposal_explicit_decline_creates_no_membership(t: &RelayTarget) {
    adoption_roundtrip(t, true, std::time::Duration::ZERO, Expected::Decline);
}

pub fn proposal_original_deadline_fences_late_consent(t: &RelayTarget) {
    adoption_roundtrip(
        t,
        true,
        std::time::Duration::from_millis(2200),
        Expected::Expire,
    );
}

pub fn proposal_expired_pipe_write_stops(t: &RelayTarget) {
    use super::super::pair::{PairOptions, PairPipe};
    let secret = crate::ring::pairing::PairSecret::generate().unwrap();
    let opts = PairOptions {
        tls: t.tls.clone(),
        ..Default::default()
    };
    let mut pipe = PairPipe::open(&t.url, &secret.slot(), &opts).unwrap();
    pipe.limit_until(std::time::Instant::now() - std::time::Duration::from_millis(1));
    let start = std::time::Instant::now();
    assert_eq!(
        pipe.send(b"expired").unwrap_err(),
        crate::ring::pairing::PairError::Expired
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
}

pub const QUOTA_SCENARIOS: &[MembershipScenario] = &[
    (
        "proposal_create_creator_quota_returns_typed_refusal_promptly",
        proposal_create_creator_quota_returns_typed_refusal_promptly,
    ),
    (
        "proposal_cancel_creator_quota_returns_typed_refusal_promptly",
        proposal_cancel_creator_quota_returns_typed_refusal_promptly,
    ),
];
fn exhaust_creator_quota(t: &RelayTarget, client: &RingClient) {
    let limit = t
        .quota_frames_per_day
        .expect("quota-configured workerd required");
    for _ in 0..=limit {
        match client.probe_start().unwrap().wait(WAIT) {
            Ok(()) => {}
            Err(crate::ring::RingError::Relay {
                code: super::super::wire::ErrorCode::Quota,
                ..
            }) => return,
            other => panic!("quota exhaustion probe: {other:?}"),
        }
    }
    panic!("creator did not exhaust configured Ring quota");
}
fn assert_prompt_quota<T: std::fmt::Debug>(
    result: Result<T, crate::ring::RingError>,
    started: std::time::Instant,
) {
    let elapsed = started.elapsed();
    assert!(
        matches!(
            result,
            Err(crate::ring::RingError::Relay {
                code: super::super::wire::ErrorCode::Quota,
                ..
            })
        ),
        "expected typed Relay quota refusal, got {result:?} after {elapsed:?}"
    );
    // Keep the actual API's15-second default timeout. A correlated refusal must arrive
    // within the ordinary public-contract wait, not be reclassified from a timeout.
    assert!(elapsed < WAIT, "quota refusal took {elapsed:?}");
}
fn proposal_create_creator_quota_returns_typed_refusal_promptly(t: &RelayTarget) {
    let p = prepare(t);
    p.client.cancel_proposal(&p.handle).unwrap();
    exhaust_creator_quota(t, &p.client);
    let started = std::time::Instant::now();
    let result = p.client.create_proposal(
        p.handle.operation.clone(),
        p.guest.sign_key(),
        p.hash,
        p.handle.expires_at,
        &p.proposal,
    );
    assert_prompt_quota(result, started);
}
fn proposal_cancel_creator_quota_returns_typed_refusal_promptly(t: &RelayTarget) {
    let p = prepare(t);
    exhaust_creator_quota(t, &p.client);
    let started = std::time::Instant::now();
    let result = p.client.cancel_proposal(&p.handle);
    assert_prompt_quota(result, started);
}

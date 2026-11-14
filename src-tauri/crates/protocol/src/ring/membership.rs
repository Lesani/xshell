//! Format-v2 membership semantics. The signed `membership` field is mandatory and strictly
//! decoded for xro2; v1's ignored extensions never authorize a v2 operation.
use super::{
    b64, verify, Member, RingError, Role, Roster, RosterError, SignKey, Signature, SignedRoster,
    Signer, MAX_SAFE_INT,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const DEFAULT_INACTIVITY_SECONDS: u64 = 30 * 24 * 60 * 60;
pub const CONSENT_CONTEXT: &str = "xshell-computer-consent-v2\n";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EnrollmentKind {
    Computer,
    Daemon,
    Mobile,
    LegacyDesktop,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Enrollment {
    pub id: String,
    pub kind: EnrollmentKind,
    pub members: Vec<SignKey>,
    pub enrolled_at: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RingPolicy {
    /// None is Never; finite policies are strictly positive seconds.
    #[serde(deserialize_with = "required_option")]
    pub inactivity_seconds: Option<u64>,
    pub activity_key: SignKey,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentProof {
    pub key: SignKey,
    pub expires_at: u64,
    pub context: ConsentContext,
    pub signature: Signature,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConsentFlow {
    Pairing,
    LocalGenesis,
    LocalUpgrade,
    LegacyCompletion,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentContext {
    pub kind: ConsentFlow,
    pub hash: String,
}
impl ConsentFlow {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pairing => "pairing",
            Self::LocalGenesis => "localGenesis",
            Self::LocalUpgrade => "localUpgrade",
            Self::LegacyCompletion => "legacyCompletion",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RosterChange {
    Genesis {
        proofs: Vec<ConsentProof>,
    },
    Upgrade {
        proofs: Vec<ConsentProof>,
    },
    Enroll {
        enrollment_id: String,
        proofs: Vec<ConsentProof>,
    },
    CompleteComputer {
        replaces: Vec<String>,
        enrollment_id: String,
        proofs: Vec<ConsentProof>,
    },
    Remove {
        enrollment_id: String,
    },
    Expire {
        enrollment_ids: Vec<String>,
        evidence: String,
    },
    SetPolicy {
        affected: Vec<String>,
        preview: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Membership {
    pub enrollments: Vec<Enrollment>,
    /// Cumulative tombstones make identity reuse fail even for a pinned-head successor check.
    pub retired: Vec<String>,
    pub policy: RingPolicy,
    pub change: RosterChange,
}
pub(crate) fn invalid(s: &str) -> RosterError {
    RosterError::Invalid(s.into())
}
fn id(s: &str) -> bool {
    b64::decode_array::<32>(s).is_ok()
}
impl Roster {
    pub fn membership(&self) -> Result<Membership, RosterError> {
        if self.v != 2 {
            return Err(invalid("not format v2"));
        }
        serde_json::from_value(
            self.extra
                .get("membership")
                .cloned()
                .ok_or_else(|| invalid("missing membership"))?,
        )
        .map_err(|e| invalid(&format!("membership: {e}")))
    }
    pub fn set_membership(&mut self, membership: &Membership) {
        self.extra.insert(
            "membership".into(),
            serde_json::to_value(membership).expect("membership serializes"),
        );
    }
}
impl Membership {
    pub fn unit(&self, id: &str) -> Option<&Enrollment> {
        self.enrollments.iter().find(|e| e.id == id)
    }
    pub fn unit_for(&self, key: &SignKey) -> Option<&Enrollment> {
        self.enrollments.iter().find(|e| e.members.contains(key))
    }
    pub fn validate(&self, r: &Roster) -> Result<(), RosterError> {
        if self.enrollments.len() > 64
            || self.retired.len() > 4096
            || self
                .policy
                .inactivity_seconds
                .is_some_and(|s| s == 0 || s > MAX_SAFE_INT)
        {
            return Err(invalid("membership bounds"));
        }
        let ps = match &self.change {
            RosterChange::Genesis { proofs }
            | RosterChange::Upgrade { proofs }
            | RosterChange::Enroll { proofs, .. }
            | RosterChange::CompleteComputer { proofs, .. } => Some(proofs),
            _ => None,
        };
        if ps.is_some_and(|ps| {
            ps.len() > 64
                || ps.iter().any(|p| {
                    p.expires_at > MAX_SAFE_INT || b64::decode_array::<32>(&p.context.hash).is_err()
                })
        }) {
            return Err(invalid("proof bounds"));
        }
        let mut ids = HashSet::new();
        for s in &self.retired {
            if !id(s) || !ids.insert(s) {
                return Err(invalid("retired ID"));
            }
        }
        let mut keys = HashSet::new();
        for e in &self.enrollments {
            if !id(&e.id) || !ids.insert(&e.id) || e.enrolled_at > r.issued_at {
                return Err(invalid("enrollment ID or time"));
            }
            let roles = e
                .members
                .iter()
                .map(|k| r.member(k).map(|m| m.role))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| invalid("unknown enrolled member"))?;
            let valid = match e.kind {
                EnrollmentKind::Computer => {
                    roles.len() == 2
                        && roles.contains(&Role::Desktop)
                        && roles.contains(&Role::Daemon)
                }
                EnrollmentKind::Daemon => roles == [Role::Daemon],
                EnrollmentKind::Mobile => roles == [Role::Mobile],
                EnrollmentKind::LegacyDesktop => roles == [Role::Desktop],
            };
            if !valid || e.members.iter().any(|k| !keys.insert(*k)) {
                return Err(invalid("unit roles or duplicate member"));
            }
        }
        if keys.len() != r.members.len() {
            return Err(invalid("unenrolled member"));
        }
        Ok(())
    }
}
fn same_members(a: &[Member], b: &[Member]) -> bool {
    a.len() == b.len() && a.iter().all(|m| b.iter().any(|n| n == m))
}
fn same_ids(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}
fn retained(prev: &Roster, next: &Roster, removed: &[SignKey]) -> bool {
    prev.members
        .iter()
        .filter(|m| !removed.contains(&m.sign_key))
        .all(|m| next.member(&m.sign_key) == Some(m))
}
/// Deterministic, domain-separated possession/consent statement; no JSON canonicalization.
/// Both identities sign the complete proposed unit, predecessor, policy and operation.
fn proposal_statement(r: &Roster) -> Result<Vec<u8>, RosterError> {
    let m = r.membership()?;
    let mut lines = vec![
        CONSENT_CONTEXT.trim_end().to_string(),
        r.ring_id.to_string(),
        r.relay_url.clone(),
        r.prev.clone().unwrap_or_default(),
        r.version.to_string(),
        r.issued_at.to_string(),
        r.signed_by.to_string(),
        m.policy.activity_key.to_string(),
        m.policy
            .inactivity_seconds
            .map(|n| n.to_string())
            .unwrap_or("never".into()),
    ];
    let (tag, mut targets) = match &m.change {
        RosterChange::Genesis { .. } => ("genesis", vec![]),
        RosterChange::Upgrade { .. } => ("upgrade", vec![]),
        RosterChange::Enroll { enrollment_id, .. } => ("enroll", vec![enrollment_id.clone()]),
        RosterChange::CompleteComputer {
            replaces,
            enrollment_id,
            ..
        } => {
            let mut t = replaces.clone();
            t.sort();
            t.insert(0, enrollment_id.clone());
            ("completeComputer", t)
        }
        _ => return Err(invalid("no consent operation")),
    };
    lines.push(tag.into());
    lines.append(&mut targets);
    let mut units = m.enrollments.clone();
    units.sort_by(|a, b| a.id.cmp(&b.id));
    for e in units {
        lines.push(format!(
            "{}:{}:{}",
            e.id,
            match e.kind {
                EnrollmentKind::Computer => "computer",
                EnrollmentKind::Daemon => "daemon",
                EnrollmentKind::Mobile => "mobile",
                EnrollmentKind::LegacyDesktop => "legacyDesktop",
            },
            e.enrolled_at
        ));
        let mut keys = e.members;
        keys.sort_by_key(|k| k.to_string());
        for key in keys {
            let member = r.member(&key).ok_or_else(|| invalid("proof key"))?;
            lines.push(format!(
                "{}:{}:{}:{}:{}",
                key,
                member.noise_key,
                match member.role {
                    Role::Desktop => "desktop",
                    Role::Daemon => "daemon",
                    Role::Mobile => "mobile",
                },
                member.added_at,
                b64::encode(member.name.as_bytes())
            ));
        }
    }
    let mut retired = m.retired;
    retired.sort();
    lines.extend(retired);
    Ok(lines.join("\n").into_bytes())
}
/// Digest of the exact proposal excluding proofs, signatures and context self-reference.
pub fn consent_proposal_hash(r: &Roster) -> Result<String, RosterError> {
    Ok(b64::encode(&Sha256::digest(proposal_statement(r)?)))
}
/// Local flows have no Noise session. Their domain-separated transcript commits the exact
/// proposal, operation nonce, approver and trusted predecessor/unit descriptors.
pub fn local_consent_context(r: &Roster) -> Result<ConsentContext, RosterError> {
    let kind = match r.membership()?.change {
        RosterChange::Genesis { .. } => ConsentFlow::LocalGenesis,
        RosterChange::Upgrade { .. } => ConsentFlow::LocalUpgrade,
        RosterChange::CompleteComputer { .. } => ConsentFlow::LegacyCompletion,
        _ => return Err(invalid("not a local consent flow")),
    };
    let hash = b64::encode(&Sha256::digest(
        format!(
            "xshell-local-consent-transcript-v2\n{}\n{}",
            kind.as_str(),
            consent_proposal_hash(r)?
        )
        .as_bytes(),
    ));
    Ok(ConsentContext { kind, hash })
}
/// Pairing callers must supply the actual authenticated Noise transcript hash and compare
/// this expected context before consent/signing. The roster verifier cannot infer that
/// unseen session. Slice2 performs that runtime comparison, not a second handshake here.
pub fn pairing_consent_context(
    r: &Roster,
    transcript_hash: &[u8; 32],
) -> Result<ConsentContext, RosterError> {
    if !matches!(r.membership()?.change, RosterChange::Enroll { .. }) {
        return Err(invalid("not a Pairing flow"));
    }
    Ok(ConsentContext {
        kind: ConsentFlow::Pairing,
        hash: b64::encode(&Sha256::digest(
            format!(
                "xshell-pairing-consent-transcript-v2\n{}\n{}",
                b64::encode(transcript_hash),
                consent_proposal_hash(r)?
            )
            .as_bytes(),
        )),
    })
}
pub fn consent_message(
    r: &Roster,
    expires_at: u64,
    context: &ConsentContext,
) -> Result<Vec<u8>, RosterError> {
    Ok(format!(
        "{CONSENT_CONTEXT}{}\n{}\n{}\n{}",
        consent_proposal_hash(r)?,
        expires_at,
        context.kind.as_str(),
        context.hash
    )
    .into_bytes())
}
fn valid_context(r: &Roster, context: &ConsentContext) -> bool {
    if b64::decode_array::<32>(&context.hash).is_err() {
        return false;
    }
    if matches!(
        r.membership().map(|m| m.change),
        Ok(RosterChange::Enroll { .. })
    ) {
        context.kind == ConsentFlow::Pairing
    } else {
        local_consent_context(r).as_ref() == Ok(context)
    }
}
fn proofs(r: &Roster, required: &[SignKey], proofs: &[ConsentProof]) -> Result<(), RosterError> {
    if proofs.len() != required.len() {
        return Err(invalid("missing consent/possession"));
    }
    let mut seen = HashSet::new();
    for p in proofs {
        if !required.contains(&p.key)
            || !seen.insert(p.key)
            || p.expires_at < r.issued_at
            || p.expires_at.saturating_sub(r.issued_at) > 600
            || !valid_context(r, &p.context)
            || proofs
                .first()
                .is_some_and(|first| first.context != p.context)
            || !verify(
                &p.key,
                &consent_message(r, p.expires_at, &p.context)?,
                &p.signature,
            )
        {
            return Err(invalid("invalid consent/possession"));
        }
    }
    Ok(())
}
pub fn verify_genesis(r: &Roster) -> Result<(), RosterError> {
    verify_genesis_roster(r, false)
}
fn verify_genesis_roster(r: &Roster, unsigned: bool) -> Result<(), RosterError> {
    let m = r.membership()?;
    let RosterChange::Genesis { proofs: ps } = &m.change else {
        return Err(invalid("v2 genesis change"));
    };
    if !m.retired.is_empty()
        || m.enrollments
            .iter()
            .any(|e| e.kind == EnrollmentKind::LegacyDesktop || e.enrolled_at != r.issued_at)
    {
        return Err(invalid("invalid new unit"));
    }
    let creator = m
        .unit_for(&r.signed_by)
        .ok_or(RosterError::SignerNotMember)?;
    if creator.kind != EnrollmentKind::Computer {
        return Err(invalid("genesis requires Computer"));
    }
    if unsigned {
        return Ok(());
    }
    proofs(
        r,
        &r.members.iter().map(|m| m.sign_key).collect::<Vec<_>>(),
        ps,
    )
}
/// Checks the full allowed delta and scoped previous-head authority. Expiry evidence is
/// verified separately with chain history, not trusted merely because a signer supplied it.
pub fn verify_transition(prev: &SignedRoster, next: &SignedRoster) -> Result<(), RosterError> {
    verify_transition_roster(prev, next.roster(), false)
}
fn verify_transition_roster(
    prev: &SignedRoster,
    n: &Roster,
    unsigned_proposal: bool,
) -> Result<(), RosterError> {
    let p = prev.roster();
    if n.v != 2 || p.v > n.v || p.relay_url != n.relay_url || n.issued_at < p.issued_at {
        return Err(invalid("format downgrade, Relay move or clock rollback"));
    }
    let m = n.membership()?;
    let signer = p.member(&n.signed_by).ok_or(RosterError::SignerNotMember)?;
    if p.v == 1 {
        if signer.role != Role::Desktop {
            return Err(RosterError::SignerNotDesktop);
        }
        let RosterChange::Upgrade { proofs: ps } = &m.change else {
            return Err(invalid("upgrade required"));
        };
        if !m.retired.is_empty()
            || !same_members(&p.members, &n.members)
            || m.enrollments.iter().any(|e| e.enrolled_at != n.issued_at)
        {
            return Err(invalid("upgrade must retain keys and set baseline once"));
        }
        let required = m
            .enrollments
            .iter()
            .filter(|e| e.kind == EnrollmentKind::Computer)
            .flat_map(|e| e.members.clone())
            .collect::<Vec<_>>();
        return if unsigned_proposal {
            Ok(())
        } else {
            proofs(n, &required, ps)
        };
    }
    let old = p.membership()?;
    if m.policy.activity_key != old.policy.activity_key {
        return Err(invalid("activity pin changed"));
    }
    let own = old
        .unit_for(&n.signed_by)
        .ok_or(RosterError::SignerNotMember)?;
    let admin = signer.role == Role::Desktop || signer.role == Role::Mobile;
    let (removed, added, ps): (Vec<String>, Option<&Enrollment>, Option<&Vec<ConsentProof>>) =
        match &m.change {
            RosterChange::Enroll {
                enrollment_id,
                proofs,
            } if admin => (
                vec![],
                Some(
                    m.unit(enrollment_id)
                        .ok_or_else(|| invalid("new unit missing"))?,
                ),
                Some(proofs),
            ),
            RosterChange::CompleteComputer {
                replaces,
                enrollment_id,
                proofs,
            } if admin => {
                if replaces.is_empty()
                    || replaces.len() > 2
                    || replaces.iter().collect::<HashSet<_>>().len() != replaces.len()
                {
                    return Err(invalid("completion targets"));
                }
                let new = m
                    .unit(enrollment_id)
                    .ok_or_else(|| invalid("Computer missing"))?;
                if new.kind != EnrollmentKind::Computer {
                    return Err(invalid("not a Computer"));
                }
                let mut roles = HashSet::new();
                for id in replaces {
                    let e = old
                        .unit(id)
                        .ok_or_else(|| invalid("completion target absent"))?;
                    if !matches!(
                        e.kind,
                        EnrollmentKind::Daemon | EnrollmentKind::LegacyDesktop
                    ) || !roles.insert(p.member(&e.members[0]).unwrap().role)
                        || !new.members.contains(&e.members[0])
                    {
                        return Err(invalid("cannot regroup or steal unit"));
                    }
                }
                (replaces.clone(), Some(new), Some(proofs))
            }
            RosterChange::Remove { enrollment_id } if admin || own.id == *enrollment_id => {
                (vec![enrollment_id.clone()], None, None)
            }
            RosterChange::Expire { enrollment_ids, .. } if !enrollment_ids.is_empty() => {
                (enrollment_ids.clone(), None, None)
            }
            RosterChange::SetPolicy { .. } if admin => (vec![], None, None),
            _ => return Err(invalid("change outside signer scope")),
        };
    if removed.iter().collect::<HashSet<_>>().len() != removed.len()
        || removed.iter().any(|id| old.unit(id).is_none())
    {
        return Err(invalid("removal target"));
    }
    let mut retired = old.retired.clone();
    retired.extend(removed.clone());
    if !same_ids(&retired, &m.retired) {
        return Err(invalid("retired IDs changed"));
    }
    let survivors = old
        .enrollments
        .iter()
        .filter(|e| !removed.contains(&e.id))
        .collect::<Vec<_>>();
    if m.enrollments.len() != survivors.len() + usize::from(added.is_some())
        || survivors.iter().any(|e| m.unit(&e.id) != Some(*e))
    {
        return Err(invalid("immutable unit changed"));
    }
    let removed_keys = removed
        .iter()
        .flat_map(|id| old.unit(id).unwrap().members.clone())
        .collect::<Vec<_>>();
    if !retained(p, n, &removed_keys) {
        return Err(invalid("retained member changed"));
    }
    if let Some(e) = added {
        if old.unit(&e.id).is_some()
            || old.retired.contains(&e.id)
            || e.enrolled_at != n.issued_at
            || e.kind == EnrollmentKind::LegacyDesktop
        {
            return Err(invalid("unsafe new unit"));
        }
        if e.members
            .iter()
            .any(|k| p.member(k).is_some() && !removed_keys.contains(k))
        {
            return Err(invalid("member already enrolled"));
        }
        // Completion retains full public identities, not just signing keys.
        if e.members
            .iter()
            .any(|k| p.member(k).is_some() && p.member(k) != n.member(k))
        {
            return Err(invalid("retained counterpart changed"));
        }
        if !unsigned_proposal {
            proofs(n, &e.members, ps.unwrap())?;
        }
    }
    let expected_keys = survivors
        .iter()
        .flat_map(|e| e.members.iter())
        .chain(added.into_iter().flat_map(|e| e.members.iter()))
        .collect::<HashSet<_>>();
    if expected_keys.len() != n.members.len()
        || n.members
            .iter()
            .any(|m| !expected_keys.contains(&m.sign_key))
    {
        return Err(invalid("unexpected member delta"));
    }
    if !matches!(m.change, RosterChange::SetPolicy { .. }) && m.policy != old.policy {
        return Err(invalid("unexpected policy delta"));
    }
    Ok(())
}
/// The typed successor of `prev` that removes the whole enrollment unit `enrollment_id`
/// (a Computer's Desktop and Daemon together, never half of it): every member of the unit
/// goes, its id joins `retired`, and the result must be a valid successor signed by `signer`
/// (so a Daemon signer removing another unit fails here, not at the Relay).
pub fn remove_unit(
    prev: &SignedRoster,
    signer: &dyn Signer,
    now: u64,
    enrollment_id: &str,
) -> Result<SignedRoster, RingError> {
    let p = prev.roster();
    let mut m = p.membership()?;
    let unit = m
        .unit(enrollment_id)
        .cloned()
        .ok_or_else(|| invalid("removal target"))?;
    let mut r = p.clone();
    r.v = 2;
    r.version = p.version + 1;
    r.prev = Some(prev.hash());
    r.signed_by = signer.sign_key();
    r.issued_at = now.max(p.issued_at);
    r.members.retain(|x| !unit.members.contains(&x.sign_key));
    m.enrollments.retain(|e| e.id != unit.id);
    m.retired.push(unit.id.clone());
    m.change = RosterChange::Remove {
        enrollment_id: unit.id,
    };
    r.set_membership(&m);
    let signed = r.sign(signer)?;
    super::chain::verify_successor(prev, &signed)?;
    Ok(signed)
}

/// Worker and consumers that have learned terminal evidence apply this additional gate to
/// *each* candidate signer, independent of the socket transporting the candidate.
pub fn verify_terminal_authority(
    prev: &SignedRoster,
    next: &SignedRoster,
    terminal: &HashSet<String>,
) -> Result<(), RosterError> {
    if prev.roster().v != 2 {
        return Ok(());
    }
    let old = prev.roster().membership()?;
    let unit = old
        .unit_for(&next.roster().signed_by)
        .ok_or(RosterError::SignerNotMember)?;
    if !terminal.contains(&unit.id) {
        return Ok(());
    }
    match next.roster().membership()?.change {
        RosterChange::Remove { enrollment_id } if enrollment_id == unit.id => Ok(()),
        RosterChange::Expire { .. } => Ok(()),
        _ => Err(invalid("terminal-expired signer")),
    }
}

pub(crate) fn verify_embedded_signatures(r: &Roster) -> Result<(), RosterError> {
    let m = r.membership()?;
    let ps = match &m.change {
        RosterChange::Genesis { proofs }
        | RosterChange::Upgrade { proofs }
        | RosterChange::Enroll { proofs, .. }
        | RosterChange::CompleteComputer { proofs, .. } => Some(proofs),
        _ => None,
    };
    if let Some(ps) = ps {
        for p in ps {
            if !verify(
                &p.key,
                &consent_message(r, p.expires_at, &p.context)?,
                &p.signature,
            ) {
                return Err(RosterError::BadSignature);
            }
        }
    }
    match &m.change {
        RosterChange::Expire { evidence, .. } => {
            super::activity::ActivityEvidence::parse(evidence, &m.policy.activity_key)?;
        }
        RosterChange::SetPolicy { preview, .. } => {
            super::activity::ActivityEvidence::parse(preview, &m.policy.activity_key)?;
        }
        _ => {}
    }
    Ok(())
}

fn required_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(d)
}

/// Structural verification of an honestly unsigned Enroll; this never verifies or creates
/// consent. Callers must verify final Noise context and obtain real proofs before signing.
pub fn verify_enroll_proposal(prev: &SignedRoster, n: &Roster) -> Result<(), RosterError> {
    n.validate()?;
    let m = n.membership()?;
    if !matches!(&m.change, RosterChange::Enroll { proofs, .. } if proofs.is_empty())
        || n.ring_id != *prev.ring_id()
        || n.version != prev.version() + 1
        || n.prev.as_deref() != Some(&prev.hash())
        || prev
            .member(&n.signed_by)
            .is_none_or(|a| a.role != Role::Desktop)
    {
        return Err(invalid("not an unsigned Desktop Enroll"));
    }
    verify_transition_roster(prev, n, true)
}

/// Checks an honestly unsigned local Genesis/Upgrade/CompleteComputer with the same
/// structural rules as the final verifier. This creates no proof or membership authority;
/// the protected local controller must obtain real consent and verify the signed chain.
pub fn verify_local_proposal(prev: Option<&SignedRoster>, n: &Roster) -> Result<(), RosterError> {
    n.validate()?;
    let m = n.membership()?;
    if !matches!(&m.change,
        RosterChange::Genesis { proofs } | RosterChange::Upgrade { proofs }
        | RosterChange::CompleteComputer { proofs, .. } if proofs.is_empty())
    {
        return Err(invalid("not an unsigned local consent proposal"));
    }
    match prev {
        None => {
            if n.version != 1
                || n.prev.is_some()
                || n.ring_id != super::RingId::derive(&n.signed_by)
            {
                return Err(invalid("local genesis identity or predecessor"));
            }
            if !n
                .member(&n.signed_by)
                .is_some_and(|m| m.role == Role::Desktop)
            {
                return Err(RosterError::SignerNotDesktop);
            }
            verify_genesis_roster(n, true)
        }
        Some(prev) => {
            if !prev
                .member(&n.signed_by)
                .is_some_and(|member| member.role == Role::Desktop)
            {
                return Err(RosterError::SignerNotDesktop);
            }
            if n.ring_id != *prev.ring_id()
                || n.version != prev.version() + 1
                || n.prev != Some(prev.hash())
            {
                return Err(invalid("local proposal predecessor"));
            }
            verify_transition_roster(prev, n, true)
        }
    }
}

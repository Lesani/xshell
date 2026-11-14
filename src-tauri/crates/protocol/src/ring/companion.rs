//! Typed local counterpart obligations. This is not a Host RPC or a generic signing
//! service: possession cannot authorize consent, and consent requires the exact recorded
//! Desktop proof. The endpoint must authenticate local ownership before dispatching here.
use super::membership::{
    consent_message, local_consent_context, pairing_consent_context, verify_enroll_proposal,
    verify_local_proposal, ConsentContext, ConsentProof, EnrollmentKind,
};
use super::pairing::adoption::{possession_message, AdoptionIntent, JoinTarget, PublicIdentity};
use super::{b64, verify, DeviceKeys, Roster, RosterChain, Signature, Signer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_COMPANION_FRAME: usize = 17 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionProfile {
    pub profile: String,
    pub desktop_store: String,
    pub daemon_store: String,
    pub desktop: PublicIdentity,
    pub daemon: PublicIdentity,
    pub endpoint: String,
}
impl CompanionProfile {
    pub fn for_stores(
        desktop_store: String,
        daemon_store: String,
        desktop: PublicIdentity,
        daemon: PublicIdentity,
        endpoint: String,
    ) -> Self {
        let mut profile = Self {
            profile: String::new(),
            desktop_store,
            daemon_store,
            desktop,
            daemon,
            endpoint,
        };
        profile.profile = profile.identifier();
        profile
    }
    fn identifier(&self) -> String {
        use sha2::{Digest, Sha256};
        b64::encode(&Sha256::digest(format!(
            "xshell-companion-profile-v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.desktop_store,
            self.daemon_store,
            self.desktop.sign_key,
            self.desktop.noise_key,
            self.daemon.sign_key,
            self.daemon.noise_key,
            self.endpoint
        )))
    }

    pub fn validate(&self) -> Result<(), String> {
        if b64::decode_array::<32>(&self.profile).is_err()
            || self.profile != self.identifier()
            || [&self.desktop_store, &self.daemon_store]
                .iter()
                .any(|path| {
                    path.len() > 4096
                        || path.chars().any(char::is_control)
                        || !std::path::Path::new(path).is_absolute()
                })
            || self.desktop_store == self.daemon_store
            || !std::path::Path::new(&self.endpoint).is_absolute()
            || self.endpoint.is_empty()
            || self.endpoint.len() > 1024
            || self.endpoint.chars().any(char::is_control)
        {
            return Err("invalid companion profile".into());
        }
        self.target().validate().map_err(|e| e.to_string())
    }
    pub fn target(&self) -> JoinTarget {
        JoinTarget::Computer {
            desktop: self.desktop.clone(),
            daemon: self.daemon.clone(),
        }
    }
}

/// Registration proves possession only, not authorization of any enrollment proposal.
pub fn registration_message(profile: &CompanionProfile) -> Vec<u8> {
    let mut message = b"xshell-companion-registration-v1\n".to_vec();
    message.extend(serde_json::to_vec(profile).expect("profile serializes"));
    message
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ConsentTranscript {
    Pairing { final_noise_hash: String },
    Local,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionProposal {
    pub operation: String,
    pub predecessor: Vec<String>,
    pub proposal: Roster,
    pub transcript: ConsentTranscript,
}
impl CompanionProposal {
    pub fn context(&self) -> Result<ConsentContext, String> {
        match &self.transcript {
            ConsentTranscript::Pairing { final_noise_hash } => pairing_consent_context(
                &self.proposal,
                &b64::decode_array::<32>(final_noise_hash).map_err(|e| e.to_string())?,
            ),
            ConsentTranscript::Local => local_consent_context(&self.proposal),
        }
        .map_err(|e| e.to_string())
    }
}

/// None of these messages can confirm a GUI operation. The Desktop proof is only made
/// by the principal's protected callback; a boolean from Host/CLI ingress cannot replace it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CompanionRequest {
    /// Deny-only reservation; no identity, membership or proof authority.
    ReserveRegistration {
        desktop_store: String,
    },
    Register {
        profile: CompanionProfile,
        proof: Signature,
    },
    Descriptor,
    /// Read-only actual Daemon readiness; registration-only children refuse it.
    PairDriverReady,
    /// Read-only running-owner admission; neither request can sign or confirm.
    StandaloneCommandReady,
    PrepareStandaloneCommand {
        intent: AdoptionIntent,
        proposal: Box<CompanionProposal>,
    },
    CompletionStart {
        intent: AdoptionIntent,
    },
    /// Protected local preparation only. Never installs supplied membership.
    RetainedCompletionStart {
        intent: AdoptionIntent,
        predecessor: Vec<String>,
    },
    CompletionStatus,
    /// Protected read-only current membership; conveys no installation receipt.
    MembershipSnapshot,
    CheckPrepared {
        operation: String,
    },
    Start {
        intent: AdoptionIntent,
    },
    Status {
        operation: String,
    },
    Cancel {
        operation: String,
    },
    Possession {
        operation: String,
        handshake_hash: String,
    },
    Prepare {
        proposal: Box<CompanionProposal>,
    },
    Consent {
        operation: String,
        desktop_proof: ConsentProof,
    },
    InstallAccepted {
        operation: String,
        rosters: Vec<String>,
    },
    /// Local accepted-only command handoff. Exact durable journal selects authority;
    /// this cannot sign, confirm, admit a new offer or override a Host origin.
    InstallStandaloneAccepted {
        operation: String,
        rosters: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompanionPhase {
    Waiting,
    Prepared,
    ConsentMayHaveLeft,
    Accepted,
    Installed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CompanionResponse {
    StandaloneCommandReady,
    StandalonePrepared {
        operation: String,
    },
    Identity {
        identity: PublicIdentity,
        profile: Option<CompanionProfile>,
    },
    Descriptor {
        profile: Option<CompanionProfile>,
    },
    Status {
        operation: String,
        phase: CompanionPhase,
    },
    Possession {
        signature: Signature,
    },
    Consent {
        proof: ConsentProof,
    },
    Installed {
        hash: String,
    },
    Completion {
        pending: Option<Box<CompletionPending>>,
    },
    MembershipSnapshot {
        rosters: Vec<String>,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Obligation {
    intent: AdoptionIntent,
    phase: CompanionPhase,
    possession: Option<String>,
    proposal: Option<CompanionProposal>,
    #[serde(default)]
    proofs: Option<Vec<ConsentProof>>,
    #[serde(default)]
    accepted_token: Option<String>,
}

/// Nonsecret private-journal state. Callers must persist a change before returning a proof
/// and hold their own store lock only around this compare-and-save, not a network wait.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionPolicy {
    profile: CompanionProfile,
    obligation: Option<Obligation>,
    retired: BTreeMap<String, u64>,
}
impl CompanionPolicy {
    pub fn new(profile: CompanionProfile) -> Result<Self, String> {
        profile.validate()?;
        Ok(Self {
            profile,
            obligation: None,
            retired: BTreeMap::new(),
        })
    }
    pub fn profile(&self) -> &CompanionProfile {
        &self.profile
    }
    pub fn retained_local(&self, operation: &str) -> bool {
        self.operation(operation)
            .ok()
            .and_then(|o| o.proposal.as_ref())
            .is_some_and(|p| {
                matches!(p.transcript, ConsentTranscript::Local) && !p.predecessor.is_empty()
            })
    }
    pub fn completion_proofs(&self, proposal: &CompanionProposal) -> Option<Vec<ConsentProof>> {
        let obligation = self.obligation.as_ref()?;
        (obligation.proposal.as_ref() == Some(proposal)
            && matches!(proposal.transcript, ConsentTranscript::Local))
        .then(|| obligation.proofs.clone())
        .flatten()
    }
    pub fn phase(&self, operation: &str) -> Result<CompanionPhase, String> {
        Ok(self.operation(operation)?.phase.clone())
    }
    fn operation(&self, operation: &str) -> Result<&Obligation, String> {
        self.obligation
            .as_ref()
            .filter(|o| o.intent.operation == operation)
            .ok_or_else(|| "unknown companion operation".into())
    }
    fn live(&self, operation: &str, now: u64) -> Result<&Obligation, String> {
        let o = self.operation(operation)?;
        if now >= o.intent.expires_at
            || o.phase == CompanionPhase::Cancelled
            || o.phase == CompanionPhase::Installed
            || o.phase == CompanionPhase::Accepted
        {
            return Err("inactive companion operation".into());
        }
        Ok(o)
    }
    pub fn start(&mut self, intent: AdoptionIntent, now: u64) -> Result<(), String> {
        self.profile.validate()?;
        intent.validate().map_err(|e| e.to_string())?;
        if intent.target != self.profile.target()
            || intent.expires_at <= now
            || intent.expires_at - now > 600
        {
            return Err("companion target or deadline mismatch".into());
        }
        self.retired.retain(|_, expires| *expires > now);
        if self.retired.contains_key(&intent.operation)
            || self.obligation.as_ref().is_some_and(|o| {
                o.intent.operation == intent.operation
                    || !matches!(
                        o.phase,
                        CompanionPhase::Cancelled | CompanionPhase::Installed
                    )
            })
        {
            return Err("companion operation already reserved".into());
        }
        if let Some(old) = &self.obligation {
            if self.retired.len() >= 64 {
                return Err("companion operation capacity".into());
            }
            self.retired
                .insert(old.intent.operation.clone(), old.intent.expires_at);
        }
        self.obligation = Some(Obligation {
            intent,
            phase: CompanionPhase::Waiting,
            possession: None,
            proposal: None,
            proofs: None,
            accepted_token: None,
        });
        Ok(())
    }
    pub fn cancel(&mut self, operation: &str) -> Result<(), String> {
        self.operation(operation)?;
        let o = self.obligation.as_mut().expect("checked operation");
        // After proof release cancellation must not erase the reconciliation obligation.
        if matches!(o.phase, CompanionPhase::Waiting | CompanionPhase::Prepared) {
            o.phase = CompanionPhase::Cancelled;
        }
        Ok(())
    }
    pub fn possession(
        &mut self,
        keys: &DeviceKeys,
        operation: &str,
        h2: [u8; 32],
        now: u64,
    ) -> Result<Signature, String> {
        let o = self.live(operation, now)?;
        self.check_keys(keys)?;
        if o.phase != CompanionPhase::Waiting || o.possession.is_some() {
            return Err("possession already used or proposal prepared".into());
        }
        let signature = keys
            .sign(&possession_message(&o.intent, &h2))
            .map_err(|e| e.to_string())?;
        self.obligation
            .as_mut()
            .expect("checked operation")
            .possession = Some(b64::encode(&h2));
        Ok(Signature::from_bytes(signature))
    }
    fn check_keys(&self, keys: &DeviceKeys) -> Result<(), String> {
        if keys.sign_key() != self.profile.daemon.sign_key
            || keys.noise_key() != self.profile.daemon.noise_key
        {
            return Err("registered Daemon identity changed".into());
        }
        Ok(())
    }
    /// An unpaired Daemon may prepare against its registered Desktop's retained
    /// chain. This confers no membership or signing authority without the exact
    /// Desktop consent proof. Existing Daemon trust is never replaced.
    pub fn completion_trust<'a>(
        &self,
        recorded: Option<&'a RosterChain>,
        trusted: Option<&'a RosterChain>,
    ) -> Result<Option<&'a RosterChain>, String> {
        // Matching heads do not restore a removed/terminal retained participant.
        // Only a genuinely unpaired Daemon may use the never-seen bootstrap below.
        if trusted.is_some_and(|chain| {
            chain
                .authorized_member(&self.profile.daemon.sign_key)
                .is_none_or(|member| {
                    member.role != super::Role::Daemon
                        || member.noise_key != self.profile.daemon.noise_key
                })
        }) {
            return Err("retained Daemon is removed, terminal or mismatched".into());
        }
        if recorded.map(|c| (c.ring_id(), c.head().hash()))
            == trusted.map(|c| (c.ring_id(), c.head().hash()))
        {
            return Ok(trusted);
        }
        if trusted.is_some() {
            return Err("local proposal does not extend retained membership".into());
        }
        let chain = recorded.ok_or("retained Desktop predecessor missing")?;
        let member = chain
            .authorized_member(&self.profile.desktop.sign_key)
            .ok_or("registered Desktop is not authorized by preparation trust")?;
        let membership = chain
            .head()
            .roster()
            .membership()
            .map_err(|e| e.to_string())?;
        if chain.head().roster().v != 2
            || member.role != super::Role::Desktop
            || member.noise_key != self.profile.desktop.noise_key
            || membership.unit_for(&member.sign_key).is_none_or(|unit| {
                !matches!(
                    unit.kind,
                    EnrollmentKind::LegacyDesktop | EnrollmentKind::Computer
                )
            })
            || chain
                .versions()
                .iter()
                .any(|roster| roster.member(&self.profile.daemon.sign_key).is_some())
        {
            return Err(
                "unpaired Daemon preparation requires its exact registered retained Desktop".into(),
            );
        }
        Ok(Some(chain))
    }
    pub fn prepare(
        &mut self,
        proposal: CompanionProposal,
        trusted: Option<&RosterChain>,
        now: u64,
    ) -> Result<(), String> {
        let o = self.live(&proposal.operation, now)?;
        if o.phase != CompanionPhase::Waiting {
            return Err("proposal already frozen".into());
        }
        if proposal.predecessor.len() > 4096
            || proposal.predecessor.iter().map(String::len).sum::<usize>() > 16 * 1024 * 1024
        {
            return Err("companion predecessor bounds".into());
        }
        let r = &proposal.proposal;
        r.validate().map_err(|e| e.to_string())?;
        if r.issued_at > now || o.intent.expires_at.saturating_sub(r.issued_at) > 600 {
            return Err("proposal outside the original operation deadline".into());
        }
        let m = r.membership().map_err(|e| e.to_string())?;
        match &proposal.transcript {
            ConsentTranscript::Pairing { .. } => {
                if trusted.is_some() {
                    return Err("leave required before pairing an enrolled counterpart".into());
                }
                if o.possession.is_none() {
                    return Err("pairing possession missing".into());
                }
                let chain =
                    RosterChain::from_tokens(&proposal.predecessor).map_err(|e| e.to_string())?;
                verify_enroll_proposal(chain.head(), r).map_err(|e| e.to_string())?;
                o.intent
                    .target
                    .matches_proposal(r)
                    .map_err(|e| e.to_string())?;
            }
            ConsentTranscript::Local => {
                let recorded = if proposal.predecessor.is_empty() {
                    None
                } else {
                    Some(
                        RosterChain::from_tokens(&proposal.predecessor)
                            .map_err(|e| e.to_string())?,
                    )
                };
                let preparation_trust = self.completion_trust(recorded.as_ref(), trusted)?;
                if preparation_trust
                    .is_some_and(|chain| chain.authorized_member(&r.signed_by).is_none())
                {
                    return Err("local proposal approver is terminal or removed".into());
                }
                verify_local_proposal(preparation_trust.map(RosterChain::head), r)
                    .map_err(|e| e.to_string())?;
                let unit = m
                    .unit_for(&self.profile.desktop.sign_key)
                    .ok_or("local Computer missing")?;
                // The local context commits enrollment IDs. Use the operation nonce as
                // the fresh Computer ID so a cancelled proof cannot authorize a new op.
                if unit.id != o.intent.operation
                    || unit.kind != EnrollmentKind::Computer
                    || unit.members.len() != 2
                    || !unit.members.contains(&self.profile.daemon.sign_key)
                    || (preparation_trust.is_none()
                        && (m.enrollments.len() != 1 || r.members.len() != 2))
                {
                    return Err("local proposal counterpart mismatch".into());
                }
                for (identity, role) in o.intent.target.identities() {
                    if r.member(&identity.sign_key).is_none_or(|member| {
                        member.role != role
                            || member.noise_key != identity.noise_key
                            || member.name != identity.name
                    }) {
                        return Err("local proposal identity mismatch".into());
                    }
                }
            }
        }
        proposal.context()?;
        let o = self.obligation.as_mut().expect("checked operation");
        o.proposal = Some(proposal);
        o.phase = CompanionPhase::Prepared;
        Ok(())
    }
    /// Recheck a retained local proposal at proof release, not just when prepared.
    /// Callers serialize current runtime trust and the obligation through this check/sign/save.
    pub fn check_prepared(
        &self,
        operation: &str,
        trusted: Option<&RosterChain>,
        now: u64,
    ) -> Result<(), String> {
        let obligation = self.live(operation, now)?;
        if obligation.phase != CompanionPhase::Prepared {
            return Err("proposal not awaiting consent".into());
        }
        let proposal = obligation
            .proposal
            .as_ref()
            .ok_or("prepared proposal missing")?;
        if !matches!(proposal.transcript, ConsentTranscript::Local)
            || proposal.predecessor.is_empty()
        {
            return Err("retained local proposal required".into());
        }
        let recorded =
            RosterChain::from_tokens(&proposal.predecessor).map_err(|e| e.to_string())?;
        let current = self
            .completion_trust(Some(&recorded), trusted)?
            .ok_or("retained membership missing at consent")?;
        if current.head() != recorded.head()
            || current.ring_id() != recorded.ring_id()
            || current
                .authorized_member(&proposal.proposal.signed_by)
                .is_none()
            || (current
                .head()
                .member(&self.profile.daemon.sign_key)
                .is_some()
                && current
                    .authorized_member(&self.profile.daemon.sign_key)
                    .is_none())
        {
            return Err(
                "prepared predecessor or participant changed; fresh proposal/confirmation required"
                    .into(),
            );
        }
        verify_local_proposal(Some(current.head()), &proposal.proposal).map_err(|e| e.to_string())
    }
    pub fn consent(
        &mut self,
        keys: &DeviceKeys,
        operation: &str,
        desktop: &ConsentProof,
        now: u64,
    ) -> Result<ConsentProof, String> {
        let o = self.live(operation, now)?;
        self.check_keys(keys)?;
        if o.phase != CompanionPhase::Prepared {
            return Err("proposal not awaiting consent".into());
        }
        let proposal = o.proposal.as_ref().ok_or("missing companion proposal")?;
        let context = proposal.context()?;
        let message = consent_message(&proposal.proposal, o.intent.expires_at, &context)
            .map_err(|e| e.to_string())?;
        if desktop.key != self.profile.desktop.sign_key
            || desktop.context != context
            || desktop.expires_at != o.intent.expires_at
            || !verify(&desktop.key, &message, &desktop.signature)
        {
            return Err("exact protected Desktop consent proof required".into());
        }
        let proof = ConsentProof {
            key: keys.sign_key(),
            expires_at: o.intent.expires_at,
            context,
            signature: Signature::from_bytes(keys.sign(&message).map_err(|e| e.to_string())?),
        };
        let obligation = self.obligation.as_mut().expect("checked operation");
        obligation.proofs = Some(vec![desktop.clone(), proof.clone()]);
        obligation.phase = CompanionPhase::ConsentMayHaveLeft;
        Ok(proof)
    }
    /// Verify the exact locally consented transition and an authorized current descendant.
    /// Expired offers still have reconciliation authority, never fresh signing authority.
    pub fn accept_chain(
        &mut self,
        operation: &str,
        chain: &RosterChain,
        trusted: Option<&RosterChain>,
    ) -> Result<RosterChain, String> {
        let o = self.operation(operation)?;
        if !matches!(
            o.phase,
            CompanionPhase::ConsentMayHaveLeft
                | CompanionPhase::Accepted
                | CompanionPhase::Installed
        ) {
            return Err("no consented local installation obligation".into());
        }
        let proposal = o.proposal.as_ref().ok_or("missing proposal")?;
        let signed = chain
            .get(proposal.proposal.version)
            .ok_or("accepted transition missing")?;
        if o.accepted_token
            .as_ref()
            .is_some_and(|token| token != signed.token())
        {
            return Err("accepted counterpart transition changed".into());
        }
        if proposal
            .predecessor
            .iter()
            .enumerate()
            .any(|(index, token)| {
                chain.versions().get(index).map(|r| r.token()) != Some(token.as_str())
            })
        {
            return Err("accepted counterpart predecessor changed".into());
        }
        let mut unsigned = signed.roster().clone();
        let mut membership = unsigned.membership().map_err(|e| e.to_string())?;
        use super::membership::RosterChange;
        let proofs = match &mut membership.change {
            RosterChange::Genesis { proofs }
            | RosterChange::Upgrade { proofs }
            | RosterChange::Enroll { proofs, .. }
            | RosterChange::CompleteComputer { proofs, .. } => proofs,
            _ => return Err("accepted transition is not adoption".into()),
        };
        let expected = o.proofs.as_ref().ok_or("local proof receipt missing")?;
        if proofs.len() != expected.len() || expected.iter().any(|proof| !proofs.contains(proof)) {
            return Err("accepted transition differs from local proofs".into());
        }
        proofs.clear();
        unsigned.set_membership(&membership);
        if unsigned != proposal.proposal {
            return Err("accepted transition differs from local proposal".into());
        }
        let current = match trusted {
            Some(current) => {
                current
                    .extended(chain.versions())
                    .map_err(|e| e.to_string())?
                    .0
            }
            None => chain.clone(),
        };
        if current.get(signed.version()) != Some(signed)
            || [self.profile.desktop.sign_key, self.profile.daemon.sign_key]
                .iter()
                .any(|key| current.authorized_member(key).is_none())
        {
            return Err("local Computer is terminal, revoked or forked".into());
        }
        let obligation = self.obligation.as_mut().expect("checked operation");
        obligation.accepted_token = Some(signed.token().into());
        if obligation.phase != CompanionPhase::Installed {
            obligation.phase = CompanionPhase::Accepted;
        }
        Ok(current)
    }
    /// Called only after the Daemon's own durable membership write has been verified.
    pub fn installed(&mut self, operation: &str, durable: &RosterChain) -> Result<(), String> {
        self.accept_chain(operation, durable, Some(durable))?;
        self.obligation.as_mut().expect("checked operation").phase = CompanionPhase::Installed;
        Ok(())
    }
}

/// Public completion administration carries proposals and proofs, never a local
/// confirmation capability or either guest store's installation receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionPending {
    pub intent: AdoptionIntent,
    pub predecessor: Vec<String>,
    pub proposal: Option<CompanionProposal>,
    pub proofs: Option<Vec<ConsentProof>>,
    pub accepted: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CompanionAdminRequest {
    Pending,
    Propose {
        proposal: Box<CompanionProposal>,
    },
    Proofs {
        operation: String,
    },
    Accepted {
        operation: String,
        rosters: Vec<String>,
    },
}

//! The Desktop's Ring state on disk: one file, `ring.json`, in a private directory, so the
//! keys, the chain and the local Daemon's membership never disagree after a crash.
//!
//! ```json
//! {"v":1,"signSeed":"b64u","noiseSeed":"b64u","rosters":["xro1…",…],
//!  "localMember":"<signKey>"|null,
//!  "farewells":[{"source":"wss://old","from":N,"target":M},…],
//!  "heldRelays":["wss://…",…],
//!  "hostMembers":{"<hostId>":{"signKey":"<signKey>","target":"…"},…}}
//! ```
//!
//! `hostMembers` (the Remote Hosts' Daemons as last added to the Roster) and `farewells`
//! (ADR-0012) are written only when not empty. Earlier builds wrote `pendingMove` (one move
//! owed, or `null`) and no `heldRelays`: such a file is read once more and rewritten in this
//! shape. An earlier build refuses this shape (it moves the file aside).
//!
//! Every change is a transaction ([`Store::transact`]): an in-process mutex and an exclusive
//! lock on `ring.lock` (so a second app instance waits), the state reloaded from disk, the
//! change, and an atomic write (an exclusive temp file with a unique name, synced, renamed
//! over `ring.json`). A file that does not parse or verify is moved aside and reported,
//! never silently replaced: until a new Ring is made explicitly, every transaction reports
//! the moved-aside file ([`Loaded::moved_aside`]).
//!
//! Secrets (the seeds, the file's bytes) live in buffers wiped when dropped, on every path.
//!
//! On Unix the directory is 0700 and the files 0600, owned by this user; a directory or file
//! that is a symlink or another user's is refused, and broader modes are tightened.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use xshell_protocol::ring::relay::Farewell;
use xshell_protocol::ring::url::normalize_origin;
use xshell_protocol::ring::{DeviceKeys, RosterChain, SecretSeed, SignKey};
use zeroize::Zeroizing;

/// The Ring state this Desktop holds.
#[derive(Clone)]
pub struct RingState {
    pub keys: Arc<DeviceKeys>,
    pub chain: RosterChain,
    /// The sign key of the Daemon on this computer, as last added to the Roster.
    pub local_member: Option<SignKey>,
    /// The farewells owed to old Relays (ADR-0012), at most one per Relay, by Relay.
    pub farewells: Vec<Farewell>,
    /// The Relays known to have held this Ring: no farewell is owed to any other.
    pub held_relays: BTreeSet<String>,
    /// Per configured Remote Host (by id): the sign key of its Daemon, as last added to the
    /// Roster.
    pub host_members: BTreeMap<String, HostMember>,
}

/// Retained identity and reconciliation state, including before the first membership.
/// Credentials and live authorization leases must never be stored here.
#[derive(Clone)]
pub struct ProfileState {
    pub keys: Arc<DeviceKeys>,
    pub membership: Option<RingState>,
    pub journal: Option<AdoptionJournal>,
    pub companion: Option<xshell_protocol::ring::companion::CompanionProfile>,
    pub pending_join: Option<PendingJoin>,
    pub outgoing: Option<OutgoingAdoption>,
}

/// A conservative durable obligation, not permission to replay a signed mutation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdoptionJournal {
    pub intent: xshell_protocol::ring::pairing::adoption::AdoptionIntent,
    pub predecessor: Vec<String>,
    pub proposal: xshell_protocol::ring::Roster,
    pub context: xshell_protocol::ring::membership::ConsentContext,
    pub transcript: xshell_protocol::ring::companion::ConsentTranscript,
    pub phase: AdoptionPhase,
    pub signed_candidate: Option<String>,
    pub accepted: Vec<String>,
    pub desktop_installed: bool,
    pub daemon_installed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AdoptionPhase {
    Prepared,
    ConsentMayHaveLeft,
    PublicationUncertain,
    Accepted,
    Installed,
    /// Legacy cancellation: the precise pre-proof reason was not recorded.
    Cancelled,
    DeclinedBeforeProof,
    ExpiredBeforeProof,
}
impl AdoptionPhase {
    pub(crate) fn before_proof_terminal(&self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::DeclinedBeforeProof | Self::ExpiredBeforeProof
        )
    }
    pub(crate) fn permits_new_operation(&self) -> bool {
        *self == Self::Installed || self.before_proof_terminal()
    }
}

impl AdoptionJournal {
    fn validate(&self) -> Result<(), String> {
        self.intent.validate().map_err(|e| e.to_string())?;
        if self.predecessor.len() > 4096
            || self.accepted.len() > 4096
            || self
                .predecessor
                .iter()
                .chain(&self.accepted)
                .map(String::len)
                .sum::<usize>()
                > 32 * 1024 * 1024
        {
            return Err("adoption journal bounds".into());
        }
        let chain = if self.predecessor.is_empty() {
            None
        } else {
            Some(RosterChain::from_tokens(&self.predecessor).map_err(|e| e.to_string())?)
        };
        if self.proposal.v != 2
            || self.proposal.version != chain.as_ref().map_or(1, |c| c.head().version() + 1)
            || self.proposal.prev != chain.as_ref().map(|c| c.head().hash())
        {
            return Err("adoption journal predecessor".into());
        }
        let proposal = xshell_protocol::ring::companion::CompanionProposal {
            operation: self.intent.operation.clone(),
            predecessor: self.predecessor.clone(),
            proposal: self.proposal.clone(),
            transcript: self.transcript.clone(),
        };
        if proposal.context()? != self.context {
            return Err("adoption journal context mismatch".into());
        }
        match self.transcript {
            xshell_protocol::ring::companion::ConsentTranscript::Pairing { .. } => {
                let predecessor = chain
                    .as_ref()
                    .ok_or("pairing journal missing predecessor")?;
                xshell_protocol::ring::membership::verify_enroll_proposal(
                    predecessor.head(),
                    &self.proposal,
                )
                .map_err(|e| e.to_string())?;
                self.intent
                    .target
                    .matches_proposal(&self.proposal)
                    .map_err(|e| e.to_string())?;
            }
            xshell_protocol::ring::companion::ConsentTranscript::Local => {
                xshell_protocol::ring::membership::verify_local_proposal(
                    chain.as_ref().map(RosterChain::head),
                    &self.proposal,
                )
                .map_err(|e| e.to_string())?;
                let xshell_protocol::ring::pairing::adoption::JoinTarget::Computer {
                    desktop,
                    daemon,
                } = &self.intent.target
                else {
                    return Err("local adoption journal requires a Computer".into());
                };
                let membership = self.proposal.membership().map_err(|e| e.to_string())?;
                // A singleton-only Upgrade deliberately does not group the registered
                // counterpart. Its explicit local capability authorizes only format migration.
                if matches!(
                    membership.change,
                    xshell_protocol::ring::membership::RosterChange::Upgrade { .. }
                ) {
                    if membership.enrollments.iter().any(|unit| {
                        unit.kind == xshell_protocol::ring::membership::EnrollmentKind::Computer
                    }) {
                        return Err("Upgrade journal cannot infer a Computer".into());
                    }
                } else {
                    let unit = membership
                        .unit_for(&desktop.sign_key)
                        .ok_or("local journal Computer missing")?;
                    if unit.id != self.intent.operation
                        || unit.kind != xshell_protocol::ring::membership::EnrollmentKind::Computer
                        || unit.members.len() != 2
                        || !unit.members.contains(&daemon.sign_key)
                        || self
                            .intent
                            .target
                            .identities()
                            .iter()
                            .any(|(identity, role)| {
                                self.proposal
                                    .member(&identity.sign_key)
                                    .is_none_or(|member| {
                                        member.role != *role
                                            || member.noise_key != identity.noise_key
                                            || member.name != identity.name
                                    })
                            })
                    {
                        return Err("local journal counterpart mismatch".into());
                    }
                }
            }
        }
        if let Some(token) = &self.signed_candidate {
            let signed =
                xshell_protocol::ring::SignedRoster::parse(token).map_err(|e| e.to_string())?;
            self.matches_signed(&signed)?;
            match &chain {
                Some(chain) => {
                    chain
                        .extended(std::slice::from_ref(&signed))
                        .map_err(|e| e.to_string())?;
                }
                None => {
                    RosterChain::from_chain(vec![signed]).map_err(|e| e.to_string())?;
                }
            }
        }
        if !self.accepted.is_empty() {
            let accepted = RosterChain::from_tokens(&self.accepted).map_err(|e| e.to_string())?;
            if self.predecessor.iter().enumerate().any(|(i, token)| {
                accepted.versions().get(i).map(|r| r.token()) != Some(token.as_str())
            }) {
                return Err("adoption journal accepted predecessor mismatch".into());
            }
            self.matches_signed(
                accepted
                    .get(self.proposal.version)
                    .ok_or("accepted transition missing")?,
            )?;
            if self.signed_candidate.as_ref().is_some_and(|candidate| {
                accepted
                    .get(self.proposal.version)
                    .map(|signed| signed.token())
                    != Some(candidate.as_str())
            }) {
                return Err("accepted transition differs from exact signed candidate".into());
            }
        }
        if (matches!(
            self.phase,
            AdoptionPhase::Accepted | AdoptionPhase::Installed
        ) || self.desktop_installed
            || self.daemon_installed)
            && self.accepted.is_empty()
        {
            return Err("installation receipt without verified accepted bytes".into());
        }
        if matches!(
            self.phase,
            AdoptionPhase::DeclinedBeforeProof | AdoptionPhase::ExpiredBeforeProof
        ) && (self.signed_candidate.is_some()
            || !self.accepted.is_empty()
            || self.desktop_installed
            || self.daemon_installed)
        {
            return Err(
                "pre-proof terminal state carries publication/installation evidence".into(),
            );
        }
        if self.phase == AdoptionPhase::Installed
            && !(self.desktop_installed && self.daemon_installed)
        {
            return Err("installed journal missing store receipt".into());
        }
        Ok(())
    }
    fn may_replace_with(&self, new: Option<&Self>) -> Result<(), String> {
        let terminal = self.phase.permits_new_operation();
        let Some(new) = new else {
            return if terminal {
                Ok(())
            } else {
                Err("adoption reconciliation journal cannot be discarded".into())
            };
        };
        if new.intent.operation != self.intent.operation {
            return if terminal {
                Ok(())
            } else {
                Err("adoption operation still requires reconciliation".into())
            };
        }
        if self.intent != new.intent
            || self.predecessor != new.predecessor
            || self.proposal != new.proposal
            || self.context != new.context
            || self.transcript != new.transcript
            || (self.signed_candidate.is_some() && self.signed_candidate != new.signed_candidate)
            || (self.desktop_installed && !new.desktop_installed)
            || (self.daemon_installed && !new.daemon_installed)
            || !new.accepted.starts_with(&self.accepted)
        {
            return Err("frozen adoption obligation cannot be changed or rolled back".into());
        }
        let rank = |phase: &AdoptionPhase| match phase {
            AdoptionPhase::Prepared => 0,
            AdoptionPhase::ConsentMayHaveLeft => 1,
            AdoptionPhase::PublicationUncertain => 2,
            AdoptionPhase::Accepted => 3,
            AdoptionPhase::Installed => 4,
            AdoptionPhase::Cancelled
            | AdoptionPhase::DeclinedBeforeProof
            | AdoptionPhase::ExpiredBeforeProof => 5,
        };
        if rank(&new.phase) < rank(&self.phase)
            || (new.phase.before_proof_terminal()
                && self.phase != AdoptionPhase::Prepared
                && self.phase != new.phase)
        {
            return Err("possible consent/publication cannot be cancelled as nonmembership".into());
        }
        Ok(())
    }

    /// Verify the recorded transition without releasing proofs or publishing any bytes.
    pub(crate) fn verify_accepted(&self, chain: &RosterChain) -> Result<(), String> {
        if self
            .predecessor
            .iter()
            .enumerate()
            .any(|(i, token)| chain.versions().get(i).map(|r| r.token()) != Some(token.as_str()))
        {
            return Err("reconciliation predecessor mismatch".into());
        }
        let signed = chain
            .get(self.proposal.version)
            .ok_or("recorded transition not observed")?;
        self.matches_signed(signed)?;
        if self
            .signed_candidate
            .as_ref()
            .is_some_and(|token| token != signed.token())
        {
            return Err("reconciliation exact signed candidate mismatch".into());
        }
        Ok(())
    }

    fn matches_signed(&self, signed: &xshell_protocol::ring::SignedRoster) -> Result<(), String> {
        let mut unsigned = signed.roster().clone();
        let mut membership = unsigned.membership().map_err(|e| e.to_string())?;
        use xshell_protocol::ring::membership::RosterChange;
        let proofs = match &mut membership.change {
            RosterChange::Genesis { proofs }
            | RosterChange::Upgrade { proofs }
            | RosterChange::Enroll { proofs, .. }
            | RosterChange::CompleteComputer { proofs, .. } => proofs,
            _ => return Err("accepted transition is not consented adoption".into()),
        };
        if proofs
            .iter()
            .any(|p| p.context != self.context || p.expires_at != self.intent.expires_at)
        {
            return Err("accepted transition consent mismatch".into());
        }
        proofs.clear();
        unsigned.set_membership(&membership);
        if unsigned != self.proposal {
            return Err("accepted transition differs from exact proposal".into());
        }
        Ok(())
    }
}

/// Nonsecret pre-proposal reservation. Words, PairSecret and live leases never enter it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingJoin {
    /// Nonempty only for retained completion: the independently verified local trust
    /// captured before any remote proposal. Fresh pairing uses its actual Noise context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_predecessor: Vec<String>,
    pub intent: xshell_protocol::ring::pairing::adoption::AdoptionIntent,
    pub relay_url: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OutgoingPhase {
    Prepared,
    PublicationUncertain,
    Published,
    Cancelled,
}
/// The approver's publication obligation. Published is a Relay disposition, not a receipt
/// for either of the guest's private stores; those receipts are deliberately absent here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutgoingAdoption {
    pub intent: xshell_protocol::ring::pairing::adoption::AdoptionIntent,
    pub predecessor: Vec<String>,
    pub proposal: xshell_protocol::ring::Roster,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_noise_hash: Option<String>,
    pub phase: OutgoingPhase,
    pub signed_candidate: Option<String>,
    pub accepted: Vec<String>,
}
impl OutgoingAdoption {
    pub(crate) fn validation_record(&self) -> Result<AdoptionJournal, String> {
        let (transcript, context) = match &self.final_noise_hash {
            Some(hash) => {
                let bytes = xshell_protocol::ring::b64::decode_array::<32>(hash)
                    .map_err(|e| e.to_string())?;
                (
                    xshell_protocol::ring::companion::ConsentTranscript::Pairing {
                        final_noise_hash: hash.clone(),
                    },
                    xshell_protocol::ring::membership::pairing_consent_context(
                        &self.proposal,
                        &bytes,
                    )
                    .map_err(|e| e.to_string())?,
                )
            }
            None => {
                if !matches!(
                    self.proposal
                        .membership()
                        .map_err(|e| e.to_string())?
                        .change,
                    xshell_protocol::ring::membership::RosterChange::CompleteComputer { .. }
                        | xshell_protocol::ring::membership::RosterChange::Upgrade { .. }
                ) {
                    return Err(
                        "local outgoing proposal must be Upgrade or CompleteComputer".into(),
                    );
                }
                (
                    xshell_protocol::ring::companion::ConsentTranscript::Local,
                    xshell_protocol::ring::membership::local_consent_context(&self.proposal)
                        .map_err(|e| e.to_string())?,
                )
            }
        };
        Ok(AdoptionJournal {
            intent: self.intent.clone(),
            predecessor: self.predecessor.clone(),
            proposal: self.proposal.clone(),
            context,
            transcript,
            phase: match self.phase {
                OutgoingPhase::Prepared => AdoptionPhase::Prepared,
                OutgoingPhase::PublicationUncertain => AdoptionPhase::PublicationUncertain,
                OutgoingPhase::Published => AdoptionPhase::Accepted,
                OutgoingPhase::Cancelled => AdoptionPhase::Cancelled,
            },
            signed_candidate: self.signed_candidate.clone(),
            accepted: self.accepted.clone(),
            desktop_installed: false,
            daemon_installed: false,
        })
    }
    fn validate(&self) -> Result<(), String> {
        if matches!(
            self.phase,
            OutgoingPhase::PublicationUncertain | OutgoingPhase::Published
        ) && self.signed_candidate.is_none()
        {
            return Err("outgoing publication requires its exact signed candidate".into());
        }
        self.validation_record()?.validate()
    }
    fn may_replace_with(&self, new: Option<&Self>) -> Result<(), String> {
        if matches!(
            self.phase,
            OutgoingPhase::Published | OutgoingPhase::Cancelled
        ) && new.is_none_or(|new| new.intent.operation != self.intent.operation)
        {
            return Ok(());
        }
        let new = new.ok_or("outgoing publication still requires reconciliation")?;
        self.validation_record()?
            .may_replace_with(Some(&new.validation_record()?))
    }
}

/// A Remote Host's Daemon in the Roster.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostMember {
    pub sign_key: SignKey,
    /// Where the Host was reached when its key was recorded (its SSH target and Daemon
    /// command): a new key at the same place is a reinstall, at another place another
    /// machine.
    pub target: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct FarewellFile {
    source: String,
    from: u64,
    target: u64,
}

impl From<&Farewell> for FarewellFile {
    fn from(f: &Farewell) -> Self {
        FarewellFile {
            source: f.source.clone(),
            from: f.from,
            target: f.target,
        }
    }
}

impl From<&FarewellFile> for Farewell {
    fn from(f: &FarewellFile) -> Self {
        Farewell {
            source: f.source.clone(),
            from: f.from,
            target: f.target,
        }
    }
}

/// The file as stored. The seeds decode straight into wiping storage, so every path, a
/// failed parse included, wipes them.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct File {
    v: u32,
    sign_seed: SecretSeed,
    noise_seed: SecretSeed,
    rosters: Vec<String>,
    local_member: Option<SignKey>,
    /// Earlier builds' one move owed: read, never written.
    #[serde(default, skip_serializing)]
    pending_move: Option<FarewellFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    farewells: Vec<FarewellFile>,
    /// `None` in an earlier build's file.
    #[serde(default)]
    held_relays: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    host_members: BTreeMap<String, HostMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adoption: Option<AdoptionJournal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    companion: Option<xshell_protocol::ring::companion::CompanionProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_join: Option<PendingJoin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outgoing: Option<OutgoingAdoption>,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct Store {
    dir: PathBuf,
    /// Serializes this process's transactions (the file lock covers other processes).
    tx: Mutex<()>,
}

/// What loading found besides the state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Loaded {
    /// An unreadable `ring.json` was moved here.
    pub moved_aside: Option<PathBuf>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store {
            dir,
            tx: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self) -> PathBuf {
        self.dir.join("ring.json")
    }

    /// Runs `f` on the state as on disk now, holding both locks; when `f` returns
    /// `Ok((Some(new), _))`, `new` is written before the locks are released. What loading
    /// found comes back with the result, whatever `f` decided.
    pub fn transact<R>(
        &self,
        f: impl FnOnce(Option<RingState>, &Loaded) -> Result<(Option<RingState>, R), String>,
    ) -> Result<(R, Loaded), String> {
        match self.transact_full(f) {
            (Ok(r), Some(l)) => Ok((r, l)),
            (Ok(r), None) => Ok((r, Loaded::default())),
            (Err(e), _) => Err(e),
        }
    }

    /// [`Store::transact`], with what loading found returned also when the transaction
    /// fails: `None` only when the state could not be loaded at all (the directory, the lock
    /// or the file itself was refused), and the error then says why.
    pub fn transact_full<R>(
        &self,
        f: impl FnOnce(Option<RingState>, &Loaded) -> Result<(Option<RingState>, R), String>,
    ) -> (Result<R, String>, Option<Loaded>) {
        self.transact_profile_full(|profile, loaded| {
            let state = profile.as_ref().and_then(|p| p.membership.clone());
            let (new, result) = f(state, loaded)?;
            let new = match new {
                None => None,
                Some(state) => {
                    if profile
                        .as_ref()
                        .and_then(|p| p.journal.as_ref())
                        .is_some_and(|j| !j.phase.permits_new_operation())
                    {
                        return Err(
                            "adoption reconciliation required before legacy Ring mutation".into(),
                        );
                    }
                    if profile.as_ref().is_some_and(|p| {
                        p.keys.sign_key() != state.keys.sign_key()
                            || p.keys.noise_key() != state.keys.noise_key()
                    }) {
                        return Err("retained Desktop identity cannot be replaced".into());
                    }
                    Some(ProfileState {
                        keys: state.keys.clone(),
                        membership: Some(state),
                        journal: profile.as_ref().and_then(|p| p.journal.clone()),
                        companion: profile.as_ref().and_then(|p| p.companion.clone()),
                        pending_join: profile.as_ref().and_then(|p| p.pending_join.clone()),
                        outgoing: profile.and_then(|p| p.outgoing),
                    })
                }
            };
            Ok((new, result))
        })
    }

    pub fn transact_profile<R>(
        &self,
        f: impl FnOnce(Option<ProfileState>, &Loaded) -> Result<(Option<ProfileState>, R), String>,
    ) -> Result<(R, Loaded), String> {
        match self.transact_profile_full(f) {
            (Ok(result), loaded) => Ok((result, loaded.unwrap_or_default())),
            (Err(e), _) => Err(e),
        }
    }

    fn transact_profile_full<R>(
        &self,
        f: impl FnOnce(Option<ProfileState>, &Loaded) -> Result<(Option<ProfileState>, R), String>,
    ) -> (Result<R, String>, Option<Loaded>) {
        let _g = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = ensure_private_dir(&self.dir) {
            return (Err(e.to_string()), None);
        }
        let lock = match open_lock(&self.dir.join("ring.lock")) {
            Ok(l) => l,
            Err(e) => return (Err(e.to_string()), None),
        };
        if let Err(e) = lock.lock() {
            return (Err(e.to_string()), None);
        }
        let out = match self.load_locked() {
            Err(e) => (Err(e), None),
            Ok((state, loaded)) => {
                let retained = state
                    .as_ref()
                    .map(|s| (s.keys.sign_key(), s.keys.noise_key()));
                let journal = state.as_ref().and_then(|s| s.journal.clone());
                let outgoing = state.as_ref().and_then(|s| s.outgoing.clone());
                let pending = state.as_ref().and_then(|s| s.pending_join.clone());
                let companion = state.as_ref().and_then(|s| s.companion.clone());
                let membership = state
                    .as_ref()
                    .and_then(|s| s.membership.as_ref())
                    .map(|s| s.chain.clone());
                let r = f(state, &loaded).and_then(|(new, out)| {
                    if let Some(n) = new {
                        if retained.is_some_and(|(sign, noise)| {
                            sign != n.keys.sign_key() || noise != n.keys.noise_key()
                        }) {
                            return Err("retained profile identity cannot be replaced".into());
                        }
                        if companion.as_ref().is_some_and(|registered| n.companion.as_ref() != Some(registered)) {
                            return Err("retained companion registration cannot be replaced".into());
                        }
                        if let Some(journal) = &journal {
                            journal.may_replace_with(n.journal.as_ref())?;
                        }
                        if let Some(outgoing) = &outgoing { outgoing.may_replace_with(n.outgoing.as_ref())?; }
                        if pending.as_ref().is_some_and(|pending| n.pending_join.as_ref().is_some_and(|new| new != pending)) {
                            return Err("pending Join must be explicitly terminated before replacement".into());
                        }
                        if let Some(old) = &membership {
                            let next = n.membership.as_ref().ok_or("leave_required: retained membership cannot be discarded")?;
                            if old.ring_id() != next.chain.ring_id()
                                || old.extended(next.chain.versions()).map_err(|e| e.to_string())?.0 != next.chain {
                                return Err("leave_required: retained membership cannot be replaced or rolled back".into());
                            }
                        }
                        self.save_locked(&n)?;
                        self.notify();
                    }
                    Ok(out)
                });
                (r, Some(loaded))
            }
        };
        let _ = lock.unlock();
        out
    }

    /// Local-only Leave of a membership that no longer includes this Desktop. Under both
    /// locks: `removed` must accept the stored profile (the caller's readiness check), the
    /// current file is first copied to `ring.json.left-<ms>` (kept as evidence), then the
    /// profile is rewritten with its identity and counterpart registration only. Atomic:
    /// an interruption leaves either the old profile (Leave can run again) or the new one.
    /// No profile or no membership: nothing to leave, a no-op.
    pub fn leave_removed(
        &self,
        removed: impl FnOnce(&ProfileState) -> Result<(), String>,
    ) -> Result<(), String> {
        let _g = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        ensure_private_dir(&self.dir).map_err(|e| e.to_string())?;
        let lock = open_lock(&self.dir.join("ring.lock")).map_err(|e| e.to_string())?;
        lock.lock().map_err(|e| e.to_string())?;
        let out = (|| {
            let (profile, loaded) = self.load_locked()?;
            if loaded.moved_aside.is_some() {
                return Err("unreadable retained Ring profile; explicit recovery required".into());
            }
            let Some(profile) = profile else {
                return Ok(());
            };
            if profile.membership.is_none() {
                return Ok(());
            }
            removed(&profile)?;
            let bytes = read_private(&self.file())
                .map_err(|e| e.to_string())?
                .ok_or("retained Ring profile disappeared")?;
            let aside = self.dir.join(format!(
                "ring.json.left-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            ));
            write_private(&aside, &bytes).map_err(|e| e.to_string())?;
            self.save_locked(&ProfileState {
                keys: profile.keys,
                membership: None,
                journal: None,
                companion: profile.companion,
                pending_join: None,
                outgoing: None,
            })?;
            self.notify();
            Ok(())
        })();
        let _ = lock.unlock();
        out
    }

    /// Tells other instances a commit happened (they watch [`Store::signature`]).
    fn notify(&self) {
        let stamp = format!(
            "{}.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let _ = write_private(&self.dir.join("ring.notify"), stamp.as_bytes());
    }

    /// A cheap fingerprint of the state on disk: `ring.json`'s mtime and size and the last
    /// commit notification. It changes with every commit, from any instance.
    pub fn signature(&self) -> (Option<std::time::SystemTime>, u64, Vec<u8>) {
        let m = fs::symlink_metadata(self.file()).ok();
        let note = fs::read(self.dir.join("ring.notify")).unwrap_or_default();
        (
            m.as_ref().and_then(|m| m.modified().ok()),
            m.map(|m| m.len()).unwrap_or(0),
            note,
        )
    }

    /// The state on disk, read under the locks.
    pub fn load(&self) -> Result<(Option<RingState>, Loaded), String> {
        self.transact(|s, _| Ok((None, s)))
    }

    fn load_locked(&self) -> Result<(Option<ProfileState>, Loaded), String> {
        let p = self.file();
        let bytes = match read_private(&p) {
            Ok(Some(b)) => b,
            // No Ring, unless one was set aside and not yet started over.
            Ok(None) => {
                return Ok((
                    None,
                    Loaded {
                        moved_aside: self.newest_bad(),
                    },
                ))
            }
            // A symlink or another user's file: refuse, leave it where it is.
            Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
        };
        match parse(&bytes).and_then(|profile| {
            self.validate_association(&profile)?;
            Ok(profile)
        }) {
            Ok(s) => Ok((Some(s), Loaded::default())),
            Err(why) => {
                let aside = self.dir.join(format!(
                    "ring.json.bad-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                ));
                fs::rename(&p, &aside).map_err(|e| {
                    format!(
                        "{} is unreadable ({why}) and cannot be moved: {e}",
                        p.display()
                    )
                })?;
                eprintln!(
                    "xshell: {} was unreadable ({why}); moved to {}",
                    p.display(),
                    aside.display()
                );
                Ok((
                    None,
                    Loaded {
                        moved_aside: Some(aside),
                    },
                ))
            }
        }
    }

    /// The newest `ring.json.bad-*` in the directory.
    fn newest_bad(&self) -> Option<PathBuf> {
        let mut bad: Vec<PathBuf> = fs::read_dir(&self.dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("ring.json.bad-"))
            })
            .collect();
        bad.sort();
        bad.pop()
    }

    /// Creates only a missing identity, never a genesis or a replacement after quarantine.
    pub fn retained_keys(&self) -> Result<Arc<DeviceKeys>, String> {
        self.transact_profile(|profile, loaded| {
            if let Some(profile) = profile {
                return Ok((None, profile.keys));
            }
            if loaded.moved_aside.is_some() {
                return Err("unreadable retained Ring profile; explicit recovery required".into());
            }
            let keys = Arc::new(DeviceKeys::generate().map_err(|e| e.to_string())?);
            Ok((
                Some(ProfileState {
                    keys: keys.clone(),
                    membership: None,
                    journal: None,
                    companion: None,
                    pending_join: None,
                    outgoing: None,
                }),
                keys,
            ))
        })
        .map(|(keys, _)| keys)
    }

    pub fn load_profile(&self) -> Result<(Option<ProfileState>, Loaded), String> {
        self.transact_profile(|profile, _| Ok((None, profile)))
    }

    fn validate_association(&self, profile: &ProfileState) -> Result<(), String> {
        if let Some(companion) = &profile.companion {
            companion.validate()?;
            if Path::new(&companion.desktop_store)
                != self.dir.canonicalize().map_err(|e| e.to_string())?
                || companion.desktop.sign_key != profile.keys.sign_key()
                || companion.desktop.noise_key != profile.keys.noise_key()
            {
                return Err("retained Desktop companion profile association mismatch".into());
            }
        }
        if let Some(pending) = &profile.pending_join {
            pending.intent.validate().map_err(|e| e.to_string())?;
            xshell_protocol::ring::url::RelayUrl::parse(&pending.relay_url)
                .map_err(|e| e.to_string())?;
            if !pending.retained_predecessor.is_empty() {
                if pending.retained_predecessor.len() > 4096
                    || pending
                        .retained_predecessor
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                        > 16 * 1024 * 1024
                {
                    return Err("retained completion predecessor bounds".into());
                }
                let chain = RosterChain::from_tokens(&pending.retained_predecessor)
                    .map_err(|e| e.to_string())?;
                let companion = profile
                    .companion
                    .as_ref()
                    .ok_or("completion counterpart missing")?;
                if chain.head().roster().v != 2
                    || chain.head().roster().relay_url != pending.relay_url
                    || chain
                        .authorized_member(&companion.daemon.sign_key)
                        .is_none_or(|m| m.noise_key != companion.daemon.noise_key)
                    || chain
                        .head()
                        .roster()
                        .membership()
                        .map_err(|e| e.to_string())?
                        .unit_for(&companion.daemon.sign_key)
                        .is_none_or(|unit| {
                            unit.kind != xshell_protocol::ring::membership::EnrollmentKind::Daemon
                        })
                {
                    return Err("retained completion trust mismatch".into());
                }
            }
            if profile.membership.is_some()
                || profile
                    .companion
                    .as_ref()
                    .is_none_or(|companion| pending.intent.target != companion.target())
            {
                return Err("pending Join profile/membership mismatch".into());
            }
        }
        if let Some(outgoing) = &profile.outgoing {
            if profile.membership.is_none() {
                return Err("outgoing adoption requires retained approver membership".into());
            }
            outgoing.validate()?;
            let member = profile
                .membership
                .as_ref()
                .ok_or("approver membership missing")?;
            if outgoing.proposal.signed_by != profile.keys.sign_key()
                || outgoing.proposal.ring_id != *member.chain.ring_id()
                || outgoing
                    .predecessor
                    .iter()
                    .enumerate()
                    .any(|(index, token)| {
                        member.chain.versions().get(index).map(|r| r.token())
                            != Some(token.as_str())
                    })
            {
                return Err("outgoing proposal does not match the retained creator/trust".into());
            }
        }
        Ok(())
    }
    fn save_locked(&self, profile: &ProfileState) -> Result<(), String> {
        self.validate_association(profile)?;
        if let Some(journal) = &profile.journal {
            journal.validate()?;
        }
        let state = profile.membership.as_ref();
        if state.is_some_and(|s| {
            s.keys.sign_key() != profile.keys.sign_key()
                || s.keys.noise_key() != profile.keys.noise_key()
        }) {
            return Err("profile membership identity mismatch".into());
        }
        let (sign, noise) = profile.keys.seeds();
        let f = File {
            v: if state.is_some()
                && profile.journal.is_none()
                && profile.companion.is_none()
                && profile.pending_join.is_none()
                && profile.outgoing.is_none()
            {
                1
            } else {
                2
            },
            sign_seed: SecretSeed::new(&sign),
            noise_seed: SecretSeed::new(&noise),
            rosters: state
                .map(|s| {
                    s.chain
                        .versions()
                        .iter()
                        .map(|r| r.token().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            local_member: state.and_then(|s| s.local_member),
            pending_move: None,
            farewells: state
                .map(|s| s.farewells.iter().map(FarewellFile::from).collect())
                .unwrap_or_default(),
            held_relays: Some(state.map(|s| s.held_relays.clone()).unwrap_or_default()),
            host_members: state.map(|s| s.host_members.clone()).unwrap_or_default(),
            adoption: profile.journal.clone(),
            companion: profile.companion.clone(),
            pending_join: profile.pending_join.clone(),
            outgoing: profile.outgoing.clone(),
        };
        let json = encode(&f)?;
        write_private(&self.file(), &json).map_err(|e| e.to_string())
    }
}

/// An upper bound of `f`'s JSON: every free-form string counted as if each byte were
/// escaped (`\u00XX`, 6 bytes); the tokens are base64url and dots, never escaped.
fn capacity(f: &File) -> usize {
    let esc = |s: &str| 6 * s.len() + 2;
    // The keys, the seeds, `localMember`, the numbers and the punctuation.
    1024 + f
        .pending_join
        .as_ref()
        .map(|v| {
            serde_json::to_vec(v)
                .expect("pending Join serializes")
                .len()
        })
        .unwrap_or(0)
        + f.outgoing
            .as_ref()
            .map(|v| {
                serde_json::to_vec(v)
                    .expect("outgoing adoption serializes")
                    .len()
            })
            .unwrap_or(0)
        + f.companion
            .as_ref()
            .map(|profile| {
                serde_json::to_vec(profile)
                    .expect("profile serializes")
                    .len()
            })
            .unwrap_or(0)
        + f.adoption
            .as_ref()
            .map(|a| serde_json::to_vec(a).expect("journal serializes").len())
            .unwrap_or(0)
        + f.rosters.iter().map(|t| t.len() + 3).sum::<usize>()
        + f.farewells
            .iter()
            .map(|p| esc(&p.source) + 64)
            .sum::<usize>()
        + f.held_relays
            .iter()
            .flatten()
            .map(|r| esc(r) + 1)
            .sum::<usize>()
        + f.host_members
            .iter()
            .map(|(id, m)| esc(id) + esc(&m.target) + 96)
            .sum::<usize>()
}

/// Serialized into a buffer sized up front (no reallocation leaves a copy of the seeds
/// behind) and wiped on drop.
fn encode(f: &File) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut json = Zeroizing::new(Vec::with_capacity(capacity(f)));
    serde_json::to_writer(&mut *json, f).map_err(|e| e.to_string())?;
    Ok(json)
}

fn parse(bytes: &[u8]) -> Result<ProfileState, String> {
    let f: File = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if (f.v != 1 && f.v != 2)
        || (f.v == 1
            && (f.adoption.is_some()
                || f.companion.is_some()
                || f.pending_join.is_some()
                || f.outgoing.is_some()))
    {
        return Err(format!("unsupported profile format {}", f.v));
    }
    let keys = DeviceKeys::from_seeds(&f.sign_seed.0, &f.noise_seed.0);
    let keys = Arc::new(keys);
    if let Some(journal) = &f.adoption {
        journal.validate()?;
    }
    if f.rosters.is_empty() {
        if f.v != 2
            || f.local_member.is_some()
            || !f.farewells.is_empty()
            || f.pending_move.is_some()
            || !f.host_members.is_empty()
            || f.held_relays.as_ref().is_some_and(|r| !r.is_empty())
        {
            return Err("pre-membership profile has membership metadata".into());
        }
        return Ok(ProfileState {
            keys,
            membership: None,
            journal: f.adoption,
            companion: f.companion,
            pending_join: f.pending_join,
            outgoing: f.outgoing,
        });
    }
    let chain = RosterChain::from_tokens(&f.rosters).map_err(|e| e.to_string())?;
    let placeholder = normalize_origin(super::LEGACY_PLACEHOLDER_RELAY_URL).ok();
    let is_placeholder =
        |url: &str| placeholder.is_some() && normalize_origin(url).ok() == placeholder;
    // An earlier build's move owed becomes a farewell, one per Relay.
    let mut by_source: BTreeMap<String, Farewell> = BTreeMap::new();
    for p in f.pending_move.iter().chain(&f.farewells) {
        by_source.insert(p.source.clone(), Farewell::from(p));
    }
    // Without the record, every Relay the chain names held the Ring, but the placeholder,
    // which never existed.
    let held_relays = f.held_relays.clone().unwrap_or_else(|| {
        chain
            .versions()
            .iter()
            .map(|r| r.roster().relay_url.clone())
            .filter(|u| !is_placeholder(u))
            .collect()
    });
    let farewells = by_source
        .into_values()
        .filter(|w| !is_placeholder(&w.source) && w.still_owed(&chain))
        .collect();
    let membership = RingState {
        keys: keys.clone(),
        chain,
        local_member: f.local_member,
        farewells,
        held_relays,
        host_members: f.host_members.clone(),
    };
    Ok(ProfileState {
        keys,
        membership: Some(membership),
        journal: f.adoption,
        companion: f.companion,
        pending_join: f.pending_join,
        outgoing: f.outgoing,
    })
}

impl RingState {
    /// After a new version: every farewell now delivers the head, and one the chain no longer
    /// owes (to the Relay the head names, say) is dropped.
    /// A format-v2 Ring never moves Relays: nothing changes.
    pub fn retarget_farewells(&mut self) {
        if self.chain.head().roster().v == 2 {
            return;
        }
        let head = self.chain.head().version();
        for f in &mut self.farewells {
            f.target = head;
        }
        let chain = &self.chain;
        self.farewells.retain(|f| f.still_owed(chain));
    }

    /// The farewell owed to `source` for leaving it after version `from`, unless one is owed
    /// already or the chain owes none.
    pub fn owe_farewell(&mut self, from: u64) {
        let Some(f) = Farewell::new(&self.chain, from) else {
            return;
        };
        if !f.still_owed(&self.chain) || self.farewells.iter().any(|x| x.source == f.source) {
            return;
        }
        self.farewells.push(f);
        self.farewells.sort_by(|a, b| a.source.cmp(&b.source));
    }
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// A shared directory above private Stores/IPC (`~/.xshell`): see
/// [`xshell_core::server::prepare_shared_dir`]. A group/world-writable one is refused
/// unchanged, never repaired.
#[cfg(unix)]
pub fn prepare_shared_dir(dir: &Path) -> io::Result<()> {
    xshell_core::server::prepare_shared_dir(dir)
}

/// Creates `dir` (and parents) private: on Unix 0700, owned by this user, not a symlink; on
/// Windows owned by this user with a protected user-only DACL, not a reparse point (see
/// `xshell_core::private_fs`).
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory (a symlink?); refusing it",
                dir.display()
            )));
        }
        if meta.uid() != uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is owned by uid {}, not by us",
                    dir.display(),
                    meta.uid()
                ),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        // Validate the existing ancestor before recursive private creation can traverse it.
        for ancestor in (dir).ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(_) => {
                    xshell_core::server::no_reparse(ancestor)?;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        // Validate an existing Store without repair before normalizing it (#87 review P1).
        xshell_core::private_fs::ensure_dir_validated(dir)
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::create_dir_all(dir)
    }
}

pub(crate) fn open_lock(p: &Path) -> io::Result<fs::File> {
    #[cfg(windows)]
    {
        xshell_core::private_fs::open_lock_no_repair(p)
    }
    #[cfg(not(windows))]
    {
        let mut o = fs::OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        o.open(p)
    }
}

/// `path` read if it is a regular file of ours (not a symlink), a broader mode tightened.
/// `None`: it does not exist.
pub fn read_private(path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    #[cfg(windows)]
    {
        let Some(f) = xshell_core::private_fs::open_read_no_repair(path)? else {
            return Ok(None);
        };
        let len = f.metadata()?.len() as usize;
        let mut buf = Zeroizing::new(Vec::with_capacity(len + 1));
        (&f).read_to_end(&mut buf)?;
        Ok(Some(buf))
    }
    #[cfg(not(windows))]
    read_private_unix(path)
}

#[cfg(not(windows))]
fn read_private_unix(path: &Path) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        Ok(m) if !m.is_file() => {
            return Err(io::Error::other(format!(
                "{} is not a regular file (a symlink?); refusing it",
                path.display()
            )))
        }
        Ok(_) => {}
    }
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = f.metadata()?;
        if meta.uid() != uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is owned by uid {}, not by us",
                    path.display(),
                    meta.uid()
                ),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    // Sized up front, so no reallocation leaves a copy of a secret behind.
    let len = f.metadata()?.len() as usize;
    let mut buf = Zeroizing::new(Vec::with_capacity(len + 1));
    (&f).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

/// Writes `bytes` to `path` atomically through an exclusive (`O_CREAT|O_EXCL`, no symlink
/// following) 0600 temp file with a unique name.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}.{}.{nanos}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(name);
    let r = (|| {
        #[cfg(windows)]
        let mut f = xshell_core::private_fs::create_new(&tmp)?;
        #[cfg(not(windows))]
        let mut f = {
            let mut o = fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            o.open(&tmp)?
        };
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use xshell_protocol::ring::SignedRoster;

    fn state() -> RingState {
        let keys = Arc::new(DeviceKeys::generate().unwrap());
        let g =
            SignedRoster::genesis(&*keys, keys.noise_key(), "desk", "wss://r.example", 1).unwrap();
        RingState {
            chain: RosterChain::from_chain(vec![g]).unwrap(),
            keys,
            local_member: None,
            farewells: Vec::new(),
            held_relays: ["wss://r.example".to_string()].into(),
            host_members: BTreeMap::new(),
        }
    }

    /// A chain from `urls`: version n names `urls[n - 1]`.
    fn moved(urls: &[&str]) -> RingState {
        let mut st = state();
        let keys = st.keys.clone();
        let g = SignedRoster::genesis(&*keys, keys.noise_key(), "desk", urls[0], 1).unwrap();
        st.chain = RosterChain::from_chain(vec![g]).unwrap();
        for u in &urls[1..] {
            let n = st
                .chain
                .head()
                .next(&*keys, 1, |d| d.relay_url = u.to_string())
                .unwrap();
            st.chain.accept(std::slice::from_ref(&n)).unwrap();
        }
        st
    }

    /// Writes `st` as an earlier build did: `pendingMove` (here `pending`), no `heldRelays`,
    /// no `farewells`.
    fn write_legacy(dir: &Path, st: &RingState, pending: Option<(&str, u64, u64)>) {
        let (sign, noise) = st.keys.seeds();
        let f = File {
            v: 1,
            sign_seed: SecretSeed::new(&sign),
            noise_seed: SecretSeed::new(&noise),
            rosters: st
                .chain
                .versions()
                .iter()
                .map(|r| r.token().to_string())
                .collect(),
            local_member: None,
            pending_move: None,
            farewells: Vec::new(),
            held_relays: None,
            host_members: BTreeMap::new(),
            adoption: None,
            companion: None,
            pending_join: None,
            outgoing: None,
        };
        let mut v = serde_json::to_value(&f).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("heldRelays");
        o.insert(
            "pendingMove".into(),
            match pending {
                Some((source, from, target)) => {
                    serde_json::json!({"source": source, "from": from, "target": target})
                }
                None => serde_json::Value::Null,
            },
        );
        ensure_private_dir(dir).unwrap();
        // Private, as the Store writes it: on Windows it refuses a file it doesn't own alone.
        write_private(&dir.join("ring.json"), &serde_json::to_vec(&v).unwrap()).unwrap();
    }

    const OLD: &str = "wss://old.example";
    const NEW: &str = "wss://new.example";

    #[test]
    fn legacy_pending_move_becomes_a_farewell() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        write_legacy(&dir, &moved(&[OLD, NEW]), Some((OLD, 1, 2)));
        let got = Store::new(dir).load().unwrap().0.unwrap();
        assert_eq!(
            got.farewells,
            [Farewell {
                source: OLD.into(),
                from: 1,
                target: 2
            }]
        );
        assert_eq!(got.held_relays, [OLD.to_string(), NEW.to_string()].into());
    }

    /// Mario's case: a Ring made on the placeholder, then moved to the Hosted Relay by a build
    /// that waited for the placeholder.
    #[test]
    fn a_farewell_owed_to_the_placeholder_is_dropped_on_load() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        // The placeholder in another spelling of its origin, the same in the chain and the
        // move, so the farewell is otherwise owed: only the origin check drops it.
        let placeholder = "wss://RELAY.xshell.app:443/";
        assert_eq!(
            normalize_origin(placeholder).unwrap(),
            normalize_origin(super::super::LEGACY_PLACEHOLDER_RELAY_URL).unwrap()
        );
        let hosted = xshell_protocol::ring::url::HOSTED_RELAY_URL;
        let st = moved(&[placeholder, hosted]);
        let owed = Farewell::new(&st.chain, 1).unwrap();
        assert!(owed.still_owed(&st.chain));
        write_legacy(&dir, &st, Some((placeholder, 1, 2)));
        let got = Store::new(dir).load().unwrap().0.unwrap();
        assert!(got.farewells.is_empty(), "{:?}", got.farewells);
        assert_eq!(got.held_relays, [hosted.to_string()].into());
    }

    #[test]
    fn legacy_state_counts_every_named_relay_but_the_placeholder_as_held() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let placeholder = super::super::LEGACY_PLACEHOLDER_RELAY_URL;
        write_legacy(&dir, &moved(&[placeholder, OLD, NEW, OLD]), None);
        let got = Store::new(dir).load().unwrap().0.unwrap();
        assert_eq!(got.held_relays, [OLD.to_string(), NEW.to_string()].into());
        assert!(got.farewells.is_empty());
    }

    #[test]
    fn farewells_and_held_relays_round_trip() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::new(t.path().join("ring"));
        let mut st = moved(&[OLD, "wss://mid.example", NEW]);
        st.held_relays = [OLD.to_string(), NEW.to_string()].into();
        st.owe_farewell(2);
        st.owe_farewell(1);
        assert_eq!(
            st.farewells
                .iter()
                .map(|f| f.source.as_str())
                .collect::<Vec<_>>(),
            ["wss://mid.example", OLD],
            "one per Relay, by Relay"
        );
        s.transact(|_, _| Ok((Some(st.clone()), ()))).unwrap();
        let got = s.load().unwrap().0.unwrap();
        assert_eq!(got.farewells, st.farewells);
        assert_eq!(got.held_relays, st.held_relays);
        // A farewell the chain no longer owes is dropped on load.
        let mut back = st.clone();
        let n = back
            .chain
            .head()
            .next(&*back.keys, 1, |d| d.relay_url = OLD.into())
            .unwrap();
        back.chain.accept(std::slice::from_ref(&n)).unwrap();
        s.transact(|_, _| Ok((Some(back), ()))).unwrap();
        let got = s.load().unwrap().0.unwrap();
        assert_eq!(
            got.farewells
                .iter()
                .map(|f| f.source.as_str())
                .collect::<Vec<_>>(),
            ["wss://mid.example"]
        );
    }

    #[test]
    fn pending_move_is_never_written() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        write_legacy(&dir, &moved(&[OLD, NEW]), Some((OLD, 1, 2)));
        let s = Store::new(dir.clone());
        s.transact(|cur, _| Ok((cur, ()))).unwrap();
        let text = fs::read_to_string(dir.join("ring.json")).unwrap();
        assert!(!text.contains("pendingMove"), "{text}");
        assert!(
            text.contains("\"farewells\":[{\"source\":\"wss://old.example\""),
            "{text}"
        );
        assert!(text.contains("\"heldRelays\":["), "{text}");
        assert_eq!(s.load().unwrap().0.unwrap().farewells.len(), 1);
    }

    fn key() -> SignKey {
        DeviceKeys::generate().unwrap().sign_key()
    }

    #[test]
    fn old_file_without_host_members_parses() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        // #8's shape, but `heldRelays` for `pendingMove` (ADR-0012): no `hostMembers` key,
        // no `farewells` while none is owed.
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("ring.json")).unwrap()).unwrap();
        let keys: Vec<_> = raw.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "heldRelays",
                "localMember",
                "noiseSeed",
                "rosters",
                "signSeed",
                "v"
            ]
        );
        let got = s.load().unwrap().0.unwrap();
        assert!(got.host_members.is_empty());
    }

    #[test]
    fn empty_host_members_not_written() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        let text = fs::read_to_string(dir.join("ring.json")).unwrap();
        assert!(!text.contains("hostMembers"), "{text}");
    }

    #[test]
    fn host_members_round_trip() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        let mut st = state();
        let (a, b) = (key(), key());
        st.host_members.insert(
            "h_aaaaaaaa".into(),
            HostMember {
                sign_key: a,
                target: "dev|".into(),
            },
        );
        st.host_members.insert(
            "h_bbbbbbbb".into(),
            HostMember {
                sign_key: b,
                target: "box|~/xd".into(),
            },
        );
        s.transact(|_, _| Ok((Some(st.clone()), ()))).unwrap();
        let text = fs::read_to_string(dir.join("ring.json")).unwrap();
        assert!(
            text.contains("\"hostMembers\":{\"h_aaaaaaaa\":{\"signKey\""),
            "{text}"
        );
        let got = s.load().unwrap().0.unwrap();
        assert_eq!(got.host_members, st.host_members);
    }

    /// The buffer holding the seeds is never reallocated (no copy left behind), whatever
    /// the Hosts' ids and targets need escaped, at the Roster's limits.
    #[test]
    fn serialization_never_reallocates() {
        use xshell_protocol::ring::roster::MAX_MEMBERS;
        use xshell_protocol::ring::{Member, Role};
        let mut st = state();
        let keys = st.keys.clone();
        let mut next = st.chain.head().clone();
        for i in 0..MAX_MEMBERS - 1 {
            let k = DeviceKeys::generate().unwrap();
            let long = format!("{i}-{}", "\u{1}\"".repeat(20));
            let name = xshell_protocol::ring::member_name(&long, "x");
            next = next
                .next(&*keys, 1, |d| {
                    d.add(Member::new(
                        &name,
                        Role::Daemon,
                        k.sign_key(),
                        k.noise_key(),
                        1,
                    ))
                })
                .unwrap();
            st.chain.accept(std::slice::from_ref(&next)).unwrap();
            st.host_members.insert(
                format!("h_{i:08}\u{1}\"\\"),
                HostMember {
                    sign_key: k.sign_key(),
                    target: "\u{2}\"".repeat(40),
                },
            );
        }
        for i in 0..8 {
            st.farewells.push(Farewell {
                source: format!("{i}{}", "\u{3}".repeat(200)),
                from: u64::MAX,
                target: u64::MAX,
            });
            st.held_relays
                .insert(format!("{i}{}", "\u{4}\"".repeat(100)));
        }
        let (sign, noise) = st.keys.seeds();
        let f = File {
            v: 1,
            sign_seed: SecretSeed::new(&sign),
            noise_seed: SecretSeed::new(&noise),
            rosters: st
                .chain
                .versions()
                .iter()
                .map(|r| r.token().to_string())
                .collect(),
            local_member: Some(key()),
            pending_move: None,
            farewells: st.farewells.iter().map(FarewellFile::from).collect(),
            held_relays: Some(st.held_relays.clone()),
            host_members: st.host_members.clone(),
            adoption: None,
            companion: None,
            pending_join: None,
            outgoing: None,
        };
        let cap = capacity(&f);
        let json = encode(&f).unwrap();
        assert_eq!(json.capacity(), cap, "reallocated");
        assert!(json.len() <= cap);
    }

    #[test]
    fn round_trip() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::new(t.path().join("ring"));
        assert!(s.load().unwrap().0.is_none());
        let st = state();
        s.transact(|_, _| Ok((Some(st.clone()), ()))).unwrap();
        let (got, l) = s.load().unwrap();
        let got = got.unwrap();
        assert_eq!(got.farewells, st.farewells);
        assert_eq!(got.held_relays, st.held_relays);
        assert_eq!(got.keys.sign_key(), st.keys.sign_key());
        assert_eq!(got.keys.noise_key(), st.keys.noise_key());
        assert_eq!(got.chain, st.chain);
        assert_eq!(l, Loaded::default());
    }

    #[cfg(unix)]
    #[test]
    fn private_modes_and_refusals() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let mode = |p: &Path| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(p).unwrap().mode() & 0o777
        };
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("ring.json")), 0o600);
        // Broader modes found later are tightened.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dir.join("ring.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(s.load().unwrap().0.is_some());
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("ring.json")), 0o600);
        // No temp file left.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");

        // A ring.json that is a symlink is refused and left alone.
        let real = t.path().join("elsewhere.json");
        fs::rename(dir.join("ring.json"), &real).unwrap();
        symlink(&real, dir.join("ring.json")).unwrap();
        assert!(s.load().is_err());
        assert!(fs::symlink_metadata(dir.join("ring.json"))
            .unwrap()
            .file_type()
            .is_symlink());

        // A ring dir that is a symlink is refused.
        let other = t.path().join("other");
        fs::create_dir(&other).unwrap();
        let linked = t.path().join("linked");
        symlink(&other, &linked).unwrap();
        let s2 = Store::new(linked);
        assert!(s2.transact(|_, _| Ok((Some(state()), ()))).is_err());
        assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
    }

    #[test]
    fn interrupted_write_is_moved_aside_not_replaced() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let s = Store::new(dir.clone());
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        let full = fs::read(dir.join("ring.json")).unwrap();
        // A torn write: half the file, plus a stray temp file from the crash.
        fs::write(dir.join("ring.json"), &full[..full.len() / 2]).unwrap();
        fs::write(dir.join("ring.json.123.0.0.tmp"), b"junk").unwrap();
        let (got, l) = s.load().unwrap();
        assert!(got.is_none());
        let aside = l.moved_aside.expect("moved aside");
        assert_eq!(fs::read(&aside).unwrap(), &full[..full.len() / 2]);
        assert!(!dir.join("ring.json").exists());
        // Every later transaction keeps reporting it, until a new Ring exists.
        assert_eq!(s.load().unwrap().1.moved_aside, Some(aside));
        s.transact(|_, _| Ok((Some(state()), ()))).unwrap();
        assert_eq!(s.load().unwrap().1.moved_aside, None);
    }
}

#[cfg(test)]
mod adoption_profile_tests {
    use super::*;

    #[test]
    fn pre_membership_keys_survive_cancel_restart_without_genesis() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("desktop"));
        let first = store.retained_keys().unwrap();
        assert!(store.load().unwrap().0.is_none());
        let reopened = Store::new(store.dir().to_path_buf());
        let second = reopened.retained_keys().unwrap();
        assert_eq!(first.sign_key(), second.sign_key());
        assert_eq!(first.noise_key(), second.noise_key());
        assert!(reopened.load().unwrap().0.is_none());
    }

    #[test]
    fn unreadable_profile_never_generates_replacement_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        store.retained_keys().unwrap();
        fs::write(store.file(), b"{broken").unwrap();
        assert!(store.retained_keys().is_err());
        assert!(
            store.retained_keys().is_err(),
            "quarantine is not permission to regenerate"
        );
    }
    fn genesis_journal_with_daemon(keys: &DeviceKeys) -> (AdoptionJournal, DeviceKeys) {
        use xshell_protocol::ring::membership::{
            Enrollment, EnrollmentKind, Membership, RingPolicy, RosterChange,
        };
        use xshell_protocol::ring::pairing::adoption::{
            AdoptionIntent, JoinTarget, PublicIdentity,
        };
        use xshell_protocol::ring::{b64, Member, RingId, Role, Roster};
        let daemon = DeviceKeys::generate().unwrap();
        let identity = |keys: &DeviceKeys| PublicIdentity {
            name: "computer".into(),
            sign_key: keys.sign_key(),
            noise_key: keys.noise_key(),
        };
        let target = JoinTarget::Computer {
            desktop: identity(keys),
            daemon: identity(&daemon),
        };
        let mut proposal = Roster {
            v: 2,
            ring_id: RingId::derive(&keys.sign_key()),
            version: 1,
            prev: None,
            relay_url: "ws://127.0.0.1:7777".into(),
            signed_by: keys.sign_key(),
            issued_at: 1000,
            members: vec![
                Member::new(
                    "computer",
                    Role::Desktop,
                    keys.sign_key(),
                    keys.noise_key(),
                    1000,
                ),
                Member::new(
                    "computer",
                    Role::Daemon,
                    daemon.sign_key(),
                    daemon.noise_key(),
                    1000,
                ),
            ],
            extra: Default::default(),
        };
        proposal.set_membership(&Membership {
            enrollments: vec![Enrollment {
                id: b64::encode(&[52; 32]),
                kind: EnrollmentKind::Computer,
                members: vec![keys.sign_key(), daemon.sign_key()],
                enrolled_at: 1000,
            }],
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: Some(2592000),
                activity_key: keys.sign_key(),
            },
            change: RosterChange::Genesis { proofs: vec![] },
        });
        let journal = AdoptionJournal {
            intent: AdoptionIntent {
                operation: b64::encode(&[52; 32]),
                expires_at: 1600,
                target,
            },
            predecessor: vec![],
            context: xshell_protocol::ring::membership::local_consent_context(&proposal).unwrap(),
            transcript: xshell_protocol::ring::companion::ConsentTranscript::Local,
            proposal,
            phase: AdoptionPhase::Prepared,
            signed_candidate: None,
            accepted: vec![],
            desktop_installed: false,
            daemon_installed: false,
        };
        (journal, daemon)
    }

    fn genesis_journal(keys: &DeviceKeys) -> AdoptionJournal {
        genesis_journal_with_daemon(keys).0
    }

    #[test]
    fn pre_membership_journal_round_trip_never_manufactures_genesis() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let keys = store.retained_keys().unwrap();
        let journal = genesis_journal(&keys);
        store
            .transact_profile(|profile, _| {
                let mut profile = profile.unwrap();
                profile.journal = Some(journal.clone());
                Ok((Some(profile), ()))
            })
            .unwrap();
        let profile = Store::new(dir.path().to_path_buf())
            .load_profile()
            .unwrap()
            .0
            .unwrap();
        assert_eq!(profile.keys.sign_key(), keys.sign_key());
        assert_eq!(profile.keys.noise_key(), keys.noise_key());
        assert!(profile.membership.is_none());
        assert_eq!(profile.journal, Some(journal));
        assert!(store.load().unwrap().0.is_none());
    }

    #[test]
    fn invalid_journal_and_identity_replacement_do_not_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let keys = store.retained_keys().unwrap();
        let original = fs::read(store.file()).unwrap();
        assert!(store
            .transact_profile(|profile, _| {
                let mut profile = profile.unwrap();
                let mut journal = genesis_journal(&keys);
                journal.context.hash = xshell_protocol::ring::b64::encode(&[99; 32]);
                profile.journal = Some(journal);
                Ok((Some(profile), ()))
            })
            .is_err());
        assert!(store
            .transact_profile(|profile, _| {
                let mut profile = profile.unwrap();
                profile.keys = Arc::new(DeviceKeys::generate().unwrap());
                Ok((Some(profile), ()))
            })
            .is_err());
        assert_eq!(fs::read(store.file()).unwrap(), original);
    }

    #[test]
    fn journal_write_failure_is_reported_without_committing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let keys = store.retained_keys().unwrap();
        let backup = dir.path().join("backup.json");
        let result = store.transact_profile(|profile, _| {
            let mut profile = profile.unwrap();
            profile.journal = Some(genesis_journal(&keys));
            fs::rename(store.file(), &backup).unwrap();
            fs::create_dir(store.file()).unwrap();
            Ok((Some(profile), ()))
        });
        assert!(result.is_err());
        fs::remove_dir(store.file()).unwrap();
        fs::rename(backup, store.file()).unwrap();
        let profile = store.load_profile().unwrap().0.unwrap();
        assert!(profile.journal.is_none());
        assert_eq!(profile.keys.sign_key(), keys.sign_key());
    }

    #[test]
    fn possible_consent_cannot_be_erased_or_relabelled_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let keys = store.retained_keys().unwrap();
        let mut journal = genesis_journal(&keys);
        journal.phase = AdoptionPhase::ConsentMayHaveLeft;
        store
            .transact_profile(|profile, _| {
                let mut profile = profile.unwrap();
                profile.journal = Some(journal.clone());
                Ok((Some(profile), ()))
            })
            .unwrap();
        for cancel in [false, true] {
            assert!(store
                .transact_profile(|profile, _| {
                    let mut profile = profile.unwrap();
                    if cancel {
                        profile.journal.as_mut().unwrap().phase = AdoptionPhase::Cancelled;
                    } else {
                        profile.journal = None;
                    }
                    Ok((Some(profile), ()))
                })
                .is_err());
        }
        assert!(
            store
                .transact(|_, _| {
                    let genesis = xshell_protocol::ring::SignedRoster::genesis(
                        &*keys,
                        keys.noise_key(),
                        "computer",
                        "ws://127.0.0.1:7777",
                        1000,
                    )
                    .unwrap();
                    Ok((
                        Some(RingState {
                            keys: keys.clone(),
                            chain: RosterChain::from_chain(vec![genesis]).unwrap(),
                            local_member: None,
                            farewells: vec![],
                            held_relays: BTreeSet::new(),
                            host_members: BTreeMap::new(),
                        }),
                        (),
                    ))
                })
                .is_err(),
            "legacy mutation cannot bypass reconciliation"
        );
        assert_eq!(
            store.load_profile().unwrap().0.unwrap().journal,
            Some(journal)
        );
    }
    #[test]
    fn known_candidate_requires_exact_accepted_token_not_an_equivalent_proof_order_fork() {
        use xshell_protocol::ring::membership::{consent_message, ConsentProof, RosterChange};
        use xshell_protocol::ring::{Signature, Signer};
        let keys = DeviceKeys::generate().unwrap();
        let (mut journal, daemon) = genesis_journal_with_daemon(&keys);
        let message = consent_message(
            &journal.proposal,
            journal.intent.expires_at,
            &journal.context,
        )
        .unwrap();
        let proofs: Vec<_> = [&keys, &daemon]
            .into_iter()
            .map(|key| ConsentProof {
                key: key.sign_key(),
                expires_at: journal.intent.expires_at,
                context: journal.context.clone(),
                signature: Signature::from_bytes(key.sign(&message).unwrap()),
            })
            .collect();
        let sign = |mut proofs: Vec<ConsentProof>, reverse: bool| {
            if reverse {
                proofs.reverse();
            }
            let mut roster = journal.proposal.clone();
            let mut membership = roster.membership().unwrap();
            membership.change = RosterChange::Genesis { proofs };
            roster.set_membership(&membership);
            roster.sign(&keys).unwrap()
        };
        let first = sign(proofs.clone(), false);
        let fork = sign(proofs, true);
        RosterChain::from_chain(vec![first.clone()]).unwrap();
        RosterChain::from_chain(vec![fork.clone()]).unwrap();
        assert_ne!(first.hash(), fork.hash());
        journal
            .verify_accepted(&RosterChain::from_chain(vec![first.clone()]).unwrap())
            .unwrap();
        // A guest without the token can recognize the exact recorded proposal/proofs.
        journal
            .verify_accepted(&RosterChain::from_chain(vec![fork.clone()]).unwrap())
            .unwrap();
        journal.signed_candidate = Some(first.token().into());
        journal.accepted = vec![first.token().into()];
        journal.phase = AdoptionPhase::Accepted;
        journal.validate().unwrap();
        journal
            .verify_accepted(&RosterChain::from_chain(vec![first.clone()]).unwrap())
            .unwrap();
        assert!(journal
            .verify_accepted(&RosterChain::from_chain(vec![fork.clone()]).unwrap())
            .is_err());
        let mut changed_context = journal.clone();
        changed_context.context = xshell_protocol::ring::membership::ConsentContext {
            kind: xshell_protocol::ring::membership::ConsentFlow::LocalUpgrade,
            hash: journal.context.hash.clone(),
        };
        assert!(changed_context
            .verify_accepted(&RosterChain::from_chain(vec![first.clone()]).unwrap())
            .is_err());
        journal.accepted = vec![fork.token().into()];
        assert!(
            journal.validate().is_err(),
            "accepted proof-order fork must not match known candidate"
        );
    }
    #[test]
    fn legacy_cancelled_and_explicit_preproof_terminal_states_round_trip_without_reclassification()
    {
        let keys = DeviceKeys::generate().unwrap();
        let prepared = genesis_journal(&keys);
        for (wire, phase) in [
            ("cancelled", AdoptionPhase::Cancelled),
            ("declinedBeforeProof", AdoptionPhase::DeclinedBeforeProof),
            ("expiredBeforeProof", AdoptionPhase::ExpiredBeforeProof),
        ] {
            let mut json = serde_json::to_value(&prepared).unwrap();
            json["phase"] = serde_json::Value::String(wire.into());
            let decoded: AdoptionJournal = serde_json::from_value(json).unwrap();
            assert_eq!(decoded.phase, phase);
            decoded.validate().unwrap();
            prepared.may_replace_with(Some(&decoded)).unwrap();
            assert!(decoded.phase.permits_new_operation());
            assert!(decoded.may_replace_with(Some(&prepared)).is_err());
            let encoded = serde_json::to_value(&decoded).unwrap();
            assert_eq!(encoded["phase"], wire);
        }
        let mut legacy = prepared.clone();
        legacy.phase = AdoptionPhase::Cancelled;
        let mut relabelled = legacy.clone();
        relabelled.phase = AdoptionPhase::DeclinedBeforeProof;
        assert!(
            legacy.may_replace_with(Some(&relabelled)).is_err(),
            "old cancellation reason is unknown"
        );
    }

    #[test]
    fn proof_or_publication_uncertainty_never_becomes_declined_or_expired_before_proof() {
        let keys = DeviceKeys::generate().unwrap();
        let prepared = genesis_journal(&keys);
        for from in [
            AdoptionPhase::ConsentMayHaveLeft,
            AdoptionPhase::PublicationUncertain,
            AdoptionPhase::Accepted,
            AdoptionPhase::Installed,
        ] {
            let mut escaped = prepared.clone();
            escaped.phase = from;
            for to in [
                AdoptionPhase::Cancelled,
                AdoptionPhase::DeclinedBeforeProof,
                AdoptionPhase::ExpiredBeforeProof,
            ] {
                let mut terminal = escaped.clone();
                terminal.phase = to;
                assert!(escaped.may_replace_with(Some(&terminal)).is_err());
            }
        }
        let mut invalid = prepared;
        invalid.phase = AdoptionPhase::ExpiredBeforeProof;
        invalid.signed_candidate = Some("not-even-a-token".into());
        assert!(invalid.validate().is_err());
    }
}

//! [`DesktopRing`]: this Desktop as the Ring's creator and signer. It creates the Ring,
//! signs every new Roster version (a new Relay URL, the local Daemon added or replaced),
//! keeps a Relay connection through the [`Connector`], and builds the [`RingView`] that
//! Settings → Mobile shows.
//!
//! Every mutation is a [`Store::transact`] transaction: serialized in this process and
//! across app instances, applied to the state as on disk now, written atomically. The
//! Connector's callbacks persist newer chains through the same transactions and never hold a
//! lock of their own while they do.
//!
//! **Lock order.** The configured-Hosts predicate ([`DesktopRing::set_configured_hosts`])
//! may take the Hosts manager's lock or read the settings file. It is only ever called with
//! no `DesktopRing` lock held (neither `commit` nor `live`): [`DesktopRing::view`] calls it on
//! a copy of the state, and [`DesktopRing::remove_member`] takes its answers before its
//! transaction and checks against them. A view emission asked for under `commit` is deferred
//! until the commit lock is released. (The Connector never reports from `set_chain` or
//! `set_farewells` themselves, so an adoption reports nothing synchronously.)
//!
//! **Relay changes (ADR-0012).** A change of the Relay URL takes effect at once: the
//! Connector moves to the new Relay whatever the old one's state. Each old Relay that held
//! the Ring ([`RingState::held_relays`], from [`ConnectorEvents::held`]) is owed a farewell
//! ([`RingState::farewells`]), kept on disk across restarts, always carrying the newest head,
//! sent and retried by the Connector, and dropped once that Relay took it. A further change is
//! never refused for a farewell owed. Only the window that runs the connection sends
//! farewells and records held Relays.
//!
//! **The daily quota (xshell#42).** When the Ring has used up its Relay's daily message quota,
//! the Relay refuses its frames until the quota resets at the next UTC midnight. The traffic
//! that uses the quota up is the Daemons' (envelopes to phones); this Desktop's own connection
//! sends almost nothing counted. So the window that runs the connection probes it now and then
//! with a `roster.get` ([`Connector::probe_start`]) while a phone is in the Roster: every
//! [`DesktopRingConfig::quota_probe`], or every [`DesktopRingConfig::quota_probe_limited`]
//! while the notice is shown. A quota refusal of any frame of this connection (a probe's
//! included, through [`ConnectorEvents::error`]) shows the notice until the next UTC midnight;
//! an accepted probe clears it, and so does the reconcile tick once the reset time has passed.
//! A probe result applies only if the connection and the quota state are still the ones it was
//! sent under; no probe is sent while this connection has had [`PROBE_STREAK_LIMIT`] refusals
//! in a row, so the probes never make the Relay close the socket (4029); an accepted counted
//! frame ([`ConnectorEvents::accepted`]), a new connection or the lapse of the notice ends the
//! run. Reports of a retired Connector (an older [`Live::epoch`]) change nothing. The
//! Connector also reports a connect the Relay refused for quota and a close for quota
//! (xshell#43), so the notice shows while the link is waiting too. When the Ring's Relay
//! changes, the notice is the old Relay's and is cleared; the Connector reports nothing about
//! an old Relay's quota (a farewell's refusals are its own).

use super::hosts::{OwnerRouting, RingHosts};
use super::store::{HostMember, RingState, Store};
use crate::LOCAL_HOST_ID;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::time::Instant;
use xshell_protocol::pairing_access::enrollment::*;
use xshell_protocol::pairing_access::{AccessError, MobileRole, ReachabilityPurpose, Scope};
use xshell_protocol::ring::membership::{remove_unit, EnrollmentKind};
use xshell_protocol::ring::pairing::{
    JoinRequest, Joined, OfferHead, PairCode, PairError, PairRefusal, PairSecret, PairingOffer,
};
use xshell_protocol::ring::relay::pair::{
    host_pairing, Cancel, HostRequest, PairOptions, PairPipe, PairingHost,
};
use xshell_protocol::ring::relay::sessions::{SessionEvents, Sessions, SessionsConfig};
use xshell_protocol::ring::relay::wire::QUOTA_REFUSALS_BEFORE_CLOSE;
use xshell_protocol::ring::relay::{
    ByeReason, Connector, ConnectorConfig, ConnectorEvents, ErrorCode, Farewell, FarewellState,
    LinkState, MemberPresence, RingClientConfig, RingTimeouts,
};
use xshell_protocol::ring::roster::MAX_MEMBERS;
use xshell_protocol::ring::url::{normalize_origin, RelayUrl};
use xshell_protocol::ring::{
    member_name, verify_genesis, DeviceKeys, Member, NoiseKey, RingError, RingId, Role, Roster,
    RosterChain, RosterError, SignKey, SignedRoster,
};

/// The local Daemon's Ring identity, from its `ring.identity` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdentity {
    pub sign_key: SignKey,
    pub noise_key: NoiseKey,
    pub name: String,
}

impl LocalIdentity {
    pub fn from_json(v: &Value) -> Result<LocalIdentity, String> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("ring.identity without {k}"))
        };
        Ok(LocalIdentity {
            sign_key: SignKey::parse(s("signKey")?).map_err(|e| e.to_string())?,
            noise_key: NoiseKey::parse(s("noiseKey")?).map_err(|e| e.to_string())?,
            name: xshell_protocol::ring::member_name(s("name").unwrap_or(""), "this computer"),
        })
    }
}

/// A Host whose Daemon belongs in the Ring, as the caller sees it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRef {
    /// The Host's id; [`crate::LOCAL_HOST_ID`] for the Local Host.
    pub id: String,
    /// Its member name: the Settings → Hosts name (the hostname for the Local Host).
    pub name: String,
    /// Where it is reached ([`HostMember::target`]); ignored for the Local Host.
    pub target: String,
}

/// What [`DesktopRing::ensure_host_daemons`] did for one Host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostOutcome {
    /// Its Daemon is in the head as a `daemon`: it can join.
    Joined,
    /// Its key is in the Ring with another role: refused for this Host only.
    OtherRole,
    /// Previously enrolled, now removed or terminal: reconnect must not enroll it again.
    Revoked,
    /// The Roster has no room for it.
    Full,
    /// Its identity was for a connection that is no longer current: discarded.
    Stale,
    /// The batch was decided for a Ring that is no longer this Desktop's: discarded.
    RingChanged,
    /// A format-v2 Ring: a Daemon not enrolled yet joins only through an enrollment flow
    /// (with its own consent), never a Roster this Desktop signs for it.
    NeedsEnrollment,
}

/// Per Host id, what [`DesktopRing::ensure_host_daemons`] did.
pub type HostOutcomes = Vec<(String, HostOutcome)>;

/// The changes one batch makes ([`plan_hosts`]).
struct HostPlan {
    remove: Vec<SignKey>,
    add: Vec<Member>,
    rename: Vec<(SignKey, String)>,
    /// Every Host's key (and target) afterwards, the Local Host's included.
    after: BTreeMap<String, (SignKey, String)>,
    outcomes: Vec<(String, HostOutcome)>,
}

impl HostPlan {
    fn edits_roster(&self) -> bool {
        !self.remove.is_empty() || !self.add.is_empty() || !self.rename.is_empty()
    }
}

/// Every Host's key before a batch: the Remote Hosts' entries and the Local Host.
fn mapping(s: &RingState) -> BTreeMap<String, (SignKey, String)> {
    let mut m: BTreeMap<String, (SignKey, String)> = s
        .host_members
        .iter()
        .map(|(id, h)| (id.clone(), (h.sign_key, h.target.clone())))
        .collect();
    if let Some(k) = s.local_member {
        m.insert(LOCAL_HOST_ID.to_string(), (k, String::new()));
    }
    m
}

/// [`plan_hosts`] for a format-v2 Ring, where this Desktop never signs for a Host: the
/// mapping of Hosts whose key is enrolled still applies, a planned addition is
/// [`HostOutcome::NeedsEnrollment`] (its Host keeps its previous mapping), and removals and
/// renames are dropped.
fn enrolled_only(s: &RingState, p: &mut HostPlan) {
    let head = s.chain.head().roster();
    let before = mapping(s);
    p.remove.clear();
    p.rename.clear();
    p.add.clear();
    let unenrolled: Vec<String> = p
        .after
        .iter()
        .filter(|(_, (k, _))| head.member(k).is_none())
        .map(|(id, _)| id.clone())
        .collect();
    for id in &unenrolled {
        match before.get(id) {
            Some(old) => p.after.insert(id.clone(), old.clone()),
            None => p.after.remove(id),
        };
    }
    for (id, o) in &mut p.outcomes {
        if *o == HostOutcome::Full || (*o == HostOutcome::Joined && unenrolled.contains(id)) {
            *o = HostOutcome::NeedsEnrollment;
        }
    }
}

/// The v2 Remove of `key`'s whole enrollment unit on `effective`, signed by `keys`, and the
/// unit's keys. Checked against `effective` itself, so its terminal evidence fences a
/// terminal-expired signer. This app's own unit is [`REMOVE_SELF`] (leaving is its own
/// flow); a configured SSH Host does not block it. `None`: the head does not list the key.
fn removal_candidate(
    effective: &RosterChain,
    keys: &DeviceKeys,
    key: &SignKey,
    at: u64,
) -> Result<Option<(SignedRoster, Vec<SignKey>)>, String> {
    let membership = effective
        .head()
        .roster()
        .membership()
        .map_err(|e| e.to_string())?;
    let Some(unit) = membership.unit_for(key).cloned() else {
        return Ok(None);
    };
    if unit.members.contains(&keys.sign_key()) {
        return Err(REMOVE_SELF.to_string());
    }
    let next = remove_unit(effective.head(), keys, at, &unit.id).map_err(|e| e.to_string())?;
    effective
        .extended(std::slice::from_ref(&next))
        .map_err(|e| e.to_string())?;
    Ok(Some((next, unit.members)))
}

/// `s` with the Relay-acknowledged `candidate` (and the `effective` versions before it that
/// it does not hold yet; the Connector's own report may have stored them already) and
/// without the Ring mapping of the removed unit's keys.
fn commit_acknowledged(
    mut s: RingState,
    effective: &RosterChain,
    candidate: &SignedRoster,
    unit_keys: &[SignKey],
) -> Result<RingState, String> {
    let mut newer = effective.since(s.chain.head().version()).to_vec();
    newer.push(candidate.clone());
    s.chain = s.chain.extended(&newer).map_err(|e| e.to_string())?.0;
    if s.chain.get(candidate.version()) != Some(candidate) {
        return Err(REMOVE_CONFLICT.to_string());
    }
    s.host_members
        .retain(|_, h| !unit_keys.contains(&h.sign_key));
    if s.local_member.is_some_and(|k| unit_keys.contains(&k)) {
        s.local_member = None;
    }
    Ok(s)
}

/// `candidate` (its predecessor in `current`) is still within its signer's authority under
/// `current`'s terminal evidence: a terminal-expired signer may only clean up itself.
fn still_authorized(current: &RosterChain, candidate: &SignedRoster) -> Result<(), String> {
    let prev = candidate
        .version()
        .checked_sub(1)
        .and_then(|v| current.get(v))
        .filter(|p| candidate.roster().prev.as_deref() == Some(p.hash().as_str()))
        .ok_or(REMOVE_CONFLICT)?;
    xshell_protocol::ring::membership::verify_terminal_authority(
        prev,
        candidate,
        current.terminal_enrollments(),
    )
    .map_err(|e| e.to_string())
}

/// A refusal because the Relay holds another head than the candidate's predecessor.
fn removal_conflict(e: &RingError) -> bool {
    matches!(
        e,
        RingError::Relay {
            code: ErrorCode::RosterStale | ErrorCode::RosterConflict,
            ..
        } | RingError::Roster(RosterError::Stale | RosterError::PrevMismatch | RosterError::Gap)
    )
}

/// What the batch changes, decided on the whole batch at once:
/// - a key already in the head as a `daemon` only updates the mapping (Host entries that
///   reach the same Daemon share its member);
/// - a key in the head with another role is refused for that Host;
/// - a new key is added; the Host's previous key is removed in the same version only when
///   it is a reinstall (the same place, a new key) and no Host, the Local Host included,
///   maps to it afterwards. A Host retargeted elsewhere keeps its old member, which the user
///   removes in Settings → Mobile ([`DesktopRing::remove_member`]);
/// - the member's name is that of the Host with the smallest id that maps to it, when that
///   Host is in the batch;
/// - new keys that do not fit under [`MAX_MEMBERS`] are refused as `full`, the last (by
///   their owner's id) first.
fn plan_hosts(s: &RingState, batch: &[&(HostRef, LocalIdentity)], now: u64) -> HostPlan {
    let head = s.chain.head().roster();
    let before = mapping(s);
    // One entry per Host (the last given), in id order.
    let mut want: BTreeMap<&str, &(HostRef, LocalIdentity)> = BTreeMap::new();
    for e in batch {
        want.insert(e.0.id.as_str(), e);
    }
    let mut outcomes = Vec::new();
    let mut candidates = Vec::new();
    for (id, e) in &want {
        if s.chain.authorized_member(&e.1.sign_key).is_none()
            && s.chain
                .versions()
                .iter()
                .any(|r| r.member(&e.1.sign_key).is_some())
        {
            outcomes.push((id.to_string(), HostOutcome::Revoked));
            continue;
        }
        match head.member(&e.1.sign_key) {
            Some(m) if m.role != Role::Daemon => {
                outcomes.push((id.to_string(), HostOutcome::OtherRole))
            }
            _ => candidates.push(*e),
        }
    }
    let target = |h: &HostRef| {
        if h.id == LOCAL_HOST_ID {
            String::new()
        } else {
            h.target.clone()
        }
    };
    let reinstall = |h: &HostRef, old_target: &str| h.id == LOCAL_HOST_ID || h.target == old_target;
    let mut rejected: Vec<SignKey> = Vec::new();
    let (after, remove, new_keys) = loop {
        let accepted: Vec<_> = candidates
            .iter()
            .filter(|e| !rejected.contains(&e.1.sign_key))
            .collect();
        let mut after = before.clone();
        for e in &accepted {
            after.insert(e.0.id.clone(), (e.1.sign_key, target(&e.0)));
        }
        let mut new_keys: Vec<SignKey> = Vec::new();
        for e in &accepted {
            if head.member(&e.1.sign_key).is_none() && !new_keys.contains(&e.1.sign_key) {
                new_keys.push(e.1.sign_key);
            }
        }
        let mut remove: Vec<SignKey> = Vec::new();
        // New keys that replace a removed one (net zero members).
        let mut replacing: Vec<SignKey> = Vec::new();
        for e in &accepted {
            let Some((old, _)) = before.get(&e.0.id) else {
                continue;
            };
            if *old == e.1.sign_key || remove.contains(old) {
                continue;
            }
            let still_mapped = after.values().any(|(k, _)| k == old);
            // Every Host moving away from `old` must be a reinstall, not a retarget.
            let all_reinstalls = accepted.iter().all(|x| match before.get(&x.0.id) {
                Some((k, t)) if k == old && x.1.sign_key != *old => reinstall(&x.0, t),
                _ => true,
            });
            let daemon = head.member(old).is_some_and(|m| m.role == Role::Daemon);
            if !still_mapped && all_reinstalls && daemon {
                remove.push(*old);
                for x in &accepted {
                    let moved = before.get(&x.0.id).is_some_and(|(k, _)| k == old);
                    if moved && new_keys.contains(&x.1.sign_key) {
                        replacing.push(x.1.sign_key);
                    }
                }
            }
        }
        let count = head.members.len() - remove.len() + new_keys.len();
        if count <= MAX_MEMBERS {
            break (after, remove, new_keys);
        }
        // `count` exceeds the (valid) head's size only through new keys. Refuse additions
        // first (the last by owner id), keeping the one-for-one replacements that fit.
        let pick = new_keys
            .iter()
            .rev()
            .find(|k| !replacing.contains(k))
            .or(new_keys.last())
            .expect("a new key");
        rejected.push(*pick);
    };
    for e in &candidates {
        let o = if rejected.contains(&e.1.sign_key) {
            HostOutcome::Full
        } else {
            HostOutcome::Joined
        };
        outcomes.push((e.0.id.clone(), o));
    }
    // The name each mapped key should carry: its smallest-id Host's, when that one is in the
    // batch (`after` iterates in id order, so the first owner is the smallest).
    let name_of = |key: &SignKey| -> Option<String> {
        let (owner, _) = after.iter().find(|(_, (k, _))| k == key)?;
        let e = want.get(owner.as_str())?;
        Some(member_name(&e.0.name, &e.1.name))
    };
    let mut add = Vec::new();
    for k in &new_keys {
        let e = candidates
            .iter()
            .find(|e| e.1.sign_key == *k)
            .expect("a new key has a candidate");
        let name = name_of(k).unwrap_or_else(|| member_name(&e.0.name, &e.1.name));
        add.push(Member::new(&name, Role::Daemon, *k, e.1.noise_key, now));
    }
    let mut rename = Vec::new();
    for m in &head.members {
        if m.role != Role::Daemon || remove.contains(&m.sign_key) {
            continue;
        }
        if let Some(name) = name_of(&m.sign_key) {
            if name != m.name {
                rename.push((m.sign_key, name));
            }
        }
    }
    HostPlan {
        remove,
        add,
        rename,
        after,
        outcomes,
    }
}

/// Told every time the view may have changed. Called without any Ring lock held.
pub trait RingObserver: Send + Sync {
    fn changed(&self, view: &RingView);
}

/// Deterministic transaction test seams; none supplies authority or alters a decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptionTestPoint {
    RegistrationAdmitted,
    OriginJoinBeforeAdmission,
    OriginJoinAdmissionBusy,
    OriginJoinAdmitted,
    JoinBeforeCounterpartStart,
    JoinWorkerStarted,
    JoinSlotReady,
    OriginControllerAttached,
    OriginDispatchEnded,
    ApprovalProofsReceived,
    ApprovalBeforePublish,
    CompletionBeforeUncertainWrite,
    CompletionBeforePublish,
    UpgradeBeforeUncertainWrite,
    UpgradeBeforePublish,
    /// v2 phone pairing: consent received, before the membership-mutation reservation.
    PhoneBeforeReservation,
    /// v2 phone pairing: predecessor checked and publication marked, before the send.
    PhoneBeforePublish,
    /// v2 phone pairing: candidate sent, before the accepted read-back.
    PhonePublishedBeforeReadBack,
    RetainedPublicationAttempt,
    AcceptedBeforeCompanionInstall,
    /// Accepted readback/guest Joined has returned; controller journal/return has not.
    AcceptedBeforeJournal,
    /// Matching durable Daemon receipt received, before the Desktop store transaction.
    CompanionInstalledBeforeDesktop,
    /// A v2 Remove candidate is built (no Ring lock held), before each publication.
    RemoveBeforePublish,
    /// Leave of a removed Computer: the Daemon has left, before the Desktop store.
    LeaveDaemonLeft,
}
#[derive(Clone)]
pub struct AdoptionTestHook(pub Arc<dyn Fn(AdoptionTestPoint) + Send + Sync>);

#[derive(Clone)]
pub struct DesktopRingConfig {
    pub adoption_test_hook: Option<AdoptionTestHook>,
    /// The private directory holding `ring.json`.
    pub dir: PathBuf,
    /// This Desktop's member name (the hostname).
    pub name: String,
    /// What the UI offers as the Hosted Relay.
    pub hosted_relay_url: String,
    /// The Relay a new Ring starts on (the Hosted one unless overridden).
    pub default_relay_url: String,
    pub timeouts: RingTimeouts,
    pub backoff_unit: Duration,
    /// The first wait after a failed farewell; it doubles up to `farewell_backoff_max`.
    pub farewell_backoff_unit: Duration,
    pub farewell_backoff_max: Duration,
    /// How often an instance that does not own the connection re-reads the state and tries
    /// to take the connection over.
    pub owner_retry: Duration,
    /// How long a pairing secret (a phone's QR, a computer's code) stays good.
    pub pair_ttl: Duration,
    /// How long a pairing waits for the Relay to hold the new Roster version (for the
    /// connection to come up, say) before it refuses the device. Below the guest's step
    /// ([`PairOptions::step`], 15 s by default), so the device hears the refusal rather than
    /// timing out; never past the pairing secret's time.
    pub pair_publish_wait: Duration,
    /// How often the window running the connection probes the Relay's daily quota while a
    /// phone is in the Roster and no quota notice is shown.
    pub quota_probe: Duration,
    /// The same while the quota notice is shown: rarely enough that a day of refusals stays
    /// well under the Relay's close after 32 in a row.
    pub quota_probe_limited: Duration,
}

impl DesktopRingConfig {
    pub fn new(dir: PathBuf, name: String) -> Self {
        let hosted = super::HOSTED_RELAY_URL.to_string();
        DesktopRingConfig {
            adoption_test_hook: None,
            dir,
            name,
            default_relay_url: hosted.clone(),
            hosted_relay_url: hosted,
            timeouts: RingTimeouts::default(),
            backoff_unit: Duration::from_secs(1),
            farewell_backoff_unit: Duration::from_secs(5),
            farewell_backoff_max: Duration::from_secs(15 * 60),
            owner_retry: Duration::from_secs(2),
            pair_ttl: Duration::from_secs(xshell_protocol::ring::pairing::PAIR_TTL_SECS),
            pair_publish_wait: Duration::from_secs(10),
            quota_probe: Duration::from_secs(10 * 60),
            quota_probe_limited: Duration::from_secs(60 * 60),
        }
    }
}

/// Which pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PairingFlow {
    /// A phone scans this Desktop's QR code.
    Phone,
    /// Another computer runs `xshelld pair`, and its code is typed here.
    Computer,
}

/// How a pairing goes, as the UI shows it (`ring:pairing`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PairingEvent {
    /// The QR is up, or the computer is being looked for.
    Waiting,
    Paired {
        name: String,
        role: Role,
    },
    /// Nobody came in time.
    Expired,
    /// `code`: a [`PairError::as_code`] (`used`, `role`, `full`, `not_found`, `cancelled`, …).
    Failed {
        code: String,
    },
}

/// Told how a pairing goes. Called on the pairing's own thread, without any Ring lock held.
pub trait PairingObserver: Send + Sync {
    fn pairing(&self, flow: PairingFlow, event: &PairingEvent);
}

/// A phone offer as the UI shows it.
#[derive(Debug, Clone)]
pub struct PhoneOffer {
    /// The QR payload (`xsp1.…`), also offered as text to copy.
    pub payload: String,
    /// Unix seconds; shown only, the expiry is enforced with a monotonic clock.
    pub expires_at: u64,
}

/// Refusals of the pairing commands start with these codes.
pub const PAIR_NOT_ENABLED: &str = "not_enabled: mobile access is not enabled";
pub const PAIR_OTHER_WINDOW: &str = "other_window: another xshell window manages your devices";

/// Refusals of [`DesktopRing::remove_member`] start with these codes (or with
/// [`PAIR_NOT_ENABLED`] / [`PAIR_OTHER_WINDOW`]).
pub const REMOVE_SELF: &str = "self: this app can't remove itself";
pub const REMOVE_IN_USE: &str = "in_use: this device belongs to a host in Settings → Hosts";

/// [`DesktopRing::remove_member`] on a format-v2 Ring, refused or not confirmed: nothing
/// is committed.
pub const REMOVE_OFFLINE: &str =
    "remove_offline: removing a device needs a connection to the Relay";
pub const REMOVE_CONFLICT: &str =
    "remove_conflict: another device changed your devices at the same time; try again";
pub const REMOVE_UNCERTAIN: &str =
    "remove_uncertain: the Relay did not confirm the removal; it applies only if the Relay accepted it";

/// A v2 membership change (removal, phone enrollment) waited for another one in progress
/// (an adoption, another phone, a removal) and gave up: nothing was sent.
pub const MEMBERSHIP_BUSY: &str = "busy: another change to your devices is in progress; try again";

/// [`DesktopRing::set_relay_url`] on an enrolled format-v2 Ring: the Relay is part of the
/// Ring's identity there.
pub const RELAY_FIXED: &str =
    "relay_fixed: an enrolled ring keeps its Relay; leave and register in a new ring to change Relays";

/// Whether a Host id is still configured in Settings → Hosts. See
/// [`DesktopRing::set_configured_hosts`].
pub type ConfiguredHosts = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// One member's presence as Settings → Mobile shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceView {
    /// `online`, `closed` (xshell closed), `unreachable`, `never` (never connected) or
    /// `unknown` (this Desktop is not connected to the Relay).
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

impl PresenceView {
    fn of(p: &MemberPresence) -> Self {
        let (kind, reason, at) = match p {
            MemberPresence::Online { since, .. } => ("online", None, *since),
            MemberPresence::NeverConnected => ("never", None, None),
            MemberPresence::Closed { reason, at } => ("closed", Some(reason.clone()), *at),
            MemberPresence::Unreachable { at } => ("unreachable", None, *at),
        };
        PresenceView { kind, reason, at }
    }

    fn unknown() -> Self {
        PresenceView {
            kind: "unknown",
            reason: None,
            at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberView {
    pub name: String,
    /// `desktop`, `daemon` or `mobile`.
    pub role: Role,
    pub sign_key: SignKey,
    pub this_app: bool,
    pub this_computer: bool,
    /// The configured Remote Host (the smallest id still in Settings → Hosts) whose Daemon
    /// this member is.
    pub host_id: Option<String>,
    /// Settings → Mobile offers to remove it: not this app, not this computer's Daemon, and
    /// no configured Host's Daemon. On a format-v2 Ring: not this app's own unit (a
    /// configured Host does not block it).
    pub removable: bool,
    pub presence: PresenceView,
    /// Format v2: the member's enrollment unit. A Computer is one row (its Desktop's name
    /// and key, its Daemon in `daemon_key`); removing it removes both.
    pub enrollment_id: Option<String>,
    pub kind: Option<EnrollmentKind>,
    pub daemon_key: Option<SignKey>,
    /// A grouped Computer: its Daemon's own presence as the Relay reports it (`presence` is
    /// online when either key is).
    pub daemon_presence: Option<PresenceView>,
}

/// A farewell still owed to an old Relay (ADR-0012).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FarewellView {
    /// The old Relay.
    pub relay_url: String,
    /// `pending` (no report yet: another window runs the connection, or it just started),
    /// `sending`, or `waiting` (the last attempt failed; retried after `retry_in`).
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// While `waiting`: seconds until the next attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_in: Option<u64>,
    /// Failed attempts in a row.
    pub failures: u32,
}

/// What Settings → Mobile shows (the `ring:status` event and the commands' answer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RingView {
    pub enabled: bool,
    pub ring_id: Option<RingId>,
    pub version: Option<u64>,
    pub relay_url: Option<String>,
    pub hosted_relay_url: String,
    /// `off`, `connecting`, `connected`, `waiting`, `stopped` or `other-window` (another
    /// instance runs the connection).
    pub connection: &'static str,
    /// While `waiting`: seconds until the next attempt.
    pub retry_in: Option<u64>,
    pub connection_error: Option<String>,
    pub limited: bool,
    /// The farewells owed to old Relays, by Relay.
    pub farewells: Vec<FarewellView>,
    /// The stored state could not be used: `recovered` (an unreadable file was moved aside;
    /// Mobile access starts over) or `unreadable` (refused, left in place).
    pub problem: Option<String>,
    pub problem_detail: Option<String>,
    pub members: Vec<MemberView>,
    /// While the Ring is over its Relay's daily message quota: when the quota resets (Unix
    /// seconds, the next UTC midnight after the last refusal). Only in the window that runs the
    /// connection.
    pub quota_reset_at: Option<u64>,
    /// The Ring's Relay is the Hosted one (same origin as `hosted_relay_url`, or a limited
    /// session, which only the Hosted Relay gives), not the user's own.
    pub relay_hosted: bool,
}

/// What the user must do before a moved-aside Ring is replaced by a new one.
pub const START_OVER_REQUIRED: &str =
    "the mobile access settings were unreadable; start over to make a new ring";

/// Quota refusals in a row on one connection past which no probe is sent, below the Relay's
/// close after [`QUOTA_REFUSALS_BEFORE_CLOSE`] (other frames of the connection may be refused
/// meanwhile).
pub const PROBE_STREAK_LIMIT: u32 = 24;
const _: () = assert!((PROBE_STREAK_LIMIT as usize) < QUOTA_REFUSALS_BEFORE_CLOSE);

/// The end of the Relay's quota day that `now` (Unix seconds) is in: the next UTC midnight.
fn next_utc_midnight(now: u64) -> u64 {
    (now / 86_400 + 1) * 86_400
}

/// The reset time to show: `until` while it is still ahead.
fn quota_notice(until: Option<u64>, now: u64) -> Option<u64> {
    until.filter(|&u| now < u)
}

/// Whether the next probe is due at `at`, given the last one and whether the notice is shown.
fn probe_due(last: Option<Instant>, limited: bool, at: Instant, cfg: &DesktopRingConfig) -> bool {
    let every = if limited {
        cfg.quota_probe_limited
    } else {
        cfg.quota_probe
    };
    last.is_none_or(|l| at.saturating_duration_since(l) >= every)
}

/// The Ring's daily quota as the window running the connection knows it.
#[derive(Default)]
struct Quota {
    /// The reset time after the last refusal (Unix seconds); the notice shows until then.
    until: Option<u64>,
    /// Bumped by every refusal and every change of `until` by other means (an accepted probe,
    /// the lapse, the Connector's retirement): a probe result applies only if it is unchanged
    /// since the probe was sent.
    rev: u64,
    /// The probe in flight, by sequence number.
    in_flight: Option<u64>,
    seq: u64,
    last_probe: Option<Instant>,
    /// Quota refusals in a row on one connection (its [`Live::conn_epoch`]). An accepted
    /// counted frame of that connection, a new connection or the lapse of the notice (the
    /// Relay's day ended, so its next frame is accepted) ends the run.
    streak: (u64, u32),
}

impl Quota {
    /// The Connector is retired, or the Ring's Relay changed: nothing reported so far, or a
    /// probe sent so far answers, counts any more. The next probe decides afresh.
    fn retire(&mut self) {
        *self = Quota {
            rev: self.rev + 1,
            seq: self.seq,
            ..Quota::default()
        };
    }
}

/// Test hook: sees a probe's answer before it is applied.
#[cfg(test)]
type ProbeHook = Box<dyn Fn(&Result<(), RingError>) + Send>;

/// Test hook: sees a `held` report's Relay before it is applied.
#[cfg(test)]
type HeldHook = Box<dyn Fn(&str) + Send>;

/// What a probe was sent under; its result applies only if all of it still holds.
struct ProbeTag {
    seq: u64,
    rev: u64,
    /// [`Live::epoch`] and [`Live::conn_epoch`] when it was sent.
    epoch: u64,
    conn_epoch: u64,
    /// Unix seconds.
    sent_at: u64,
}

struct EnrollmentOperation {
    task: uuid::Uuid,
    parameters: EnrollmentParameters,
    response: EnrollmentResponse,
    released: bool,
    reserved: bool,
    /// A v2 phone candidate may have reached the Relay: from here a cancellation reports an
    /// unknown outcome, never `NoDesktopCommit`. Set under the commit and Live locks, the
    /// same synchronization `enrollment_status` cancels under.
    publishing: bool,
}
#[derive(Default)]
struct Live {
    state: Option<RingState>,
    /// Changes on every accepted adoption/departure, including same-head metadata changes.
    state_revision: u64,
    connector: Option<Arc<Connector>>,
    routing: Option<Arc<OwnerRouting>>,
    /// `ring-connector.lock`, held while this instance runs the Connector.
    owner: Option<std::fs::File>,
    /// Another instance holds the owner lock: this one shows the state from disk.
    other_window: bool,
    link: Option<LinkState>,
    /// The Connector's last report per farewell, by old Relay.
    farewells: BTreeMap<String, FarewellState>,
    problem: Option<(String, String)>,
    quitting: bool,
    /// The pairing in progress per flow (its cancel flag). Secrets live only in memory, on
    /// the pairing's thread: `ring.json` never holds one.
    pairing: Vec<(PairingFlow, uuid::Uuid, Cancel)>,
    // Nonsecret operation tombstones include cancellation before delayed start reservation.
    pairing_operations: Vec<(uuid::Uuid, Instant)>,
    enrollment: Option<EnrollmentOperation>,
    quota: Quota,
    /// Bumped for every Connector started: its [`Events`] carry it, and a report of a retired
    /// one (an older epoch, or none running) changes nothing.
    epoch: u64,
    /// Bumped whenever the connection goes down or a new one comes up (every link report
    /// but `Connected` after `Connected`).
    conn_epoch: u64,
}

#[cfg(any(unix, windows))]
struct HostBrokerBinding {
    generation: Option<u64>,
    /// The single accepted successor retained only to deliver this Join's receipt.
    join_receipt_generation: Option<u64>,
    server: Option<super::broker::Server>,
    client: Arc<super::broker::BrokerClient>,
    path: PathBuf,
    profile: String,
}

pub struct DesktopRing {
    pub(super) cfg: DesktopRingConfig,
    pub(super) store: Store,
    /// Nonblocking admission for one local adoption; never a filesystem lock.
    pub(super) adoption: Arc<AtomicBool>,
    pub(super) adoption_cancel: Mutex<Option<Weak<AtomicBool>>>,
    /// While an admission is held: the operation ids of the durable adoption records that
    /// existed when it was taken. Records not in it belong to the live operation.
    pub(super) adoption_baseline: super::adoption::AdoptionBaseline,
    observer: Arc<dyn RingObserver>,
    live: Mutex<Live>,
    routing_manager: Mutex<Option<Weak<crate::Manager>>>,
    routing_tx: std::sync::mpsc::SyncSender<()>,
    #[cfg(any(unix, windows))]
    host_broker: Mutex<Option<HostBrokerBinding>>,
    /// Serializes every commit with the adoption of its result (into `live` and the
    /// Connector), so results are adopted in commit order.
    commit: Mutex<()>,
    me: Weak<DesktopRing>,
    /// Which Host ids are still configured; every one until set. Called without any other
    /// `DesktopRing` lock held (see the module docs).
    configured: Mutex<ConfiguredHosts>,
    /// A view emission asked for while this thread held `commit`: made once the commit scope
    /// ends.
    deferred_emit: AtomicBool,
    /// Test hook: pauses a fresh election reload after the store read, outside Live.
    #[cfg(test)]
    after_owner_load: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Test hook: runs between a commit and the adoption of its result.
    #[cfg(test)]
    after_commit: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Test hook: runs before a `held` report is applied.
    #[cfg(test)]
    before_held: Mutex<Option<HeldHook>>,
    /// Test hook: runs when a pairing starts waiting for the Relay to hold its version.
    #[cfg(test)]
    before_publish_wait: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// The thread holding `commit`, for the lock-order test.
    #[cfg(test)]
    commit_owner: Mutex<Option<std::thread::ThreadId>>,
    /// Test hook: runs on the probe's thread between its answer and applying it.
    #[cfg(test)]
    before_probe_apply: Mutex<Option<ProbeHook>>,
    /// Every probe result so far: `true` applied, `false` fenced off.
    #[cfg(test)]
    probe_results: Mutex<Vec<bool>>,
}

thread_local! {
    /// How many `DesktopRing` commit locks this thread holds.
    static COMMITS_HELD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// `commit`, held. Dropping it releases the lock first, then makes the emission deferred
/// while it was held, so no view (and no configured-Hosts predicate) is built under it.
struct CommitGuard<'a> {
    ring: &'a DesktopRing,
    guard: Option<std::sync::MutexGuard<'a, ()>>,
}

impl Drop for CommitGuard<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        {
            *self
                .ring
                .commit_owner
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.guard.take();
        let held = COMMITS_HELD.with(|c| {
            c.set(c.get().saturating_sub(1));
            c.get()
        });
        if held == 0 && self.ring.deferred_emit.swap(false, Ordering::AcqRel) {
            self.ring.emit();
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The Connector's reports, for the Connector started under `epoch`.
struct Events {
    ring: Weak<DesktopRing>,
    epoch: u64,
}

impl ConnectorEvents for Events {
    fn state(&self, s: &LinkState) {
        if let Some(r) = self.ring.upgrade() {
            {
                let mut l = r.lock();
                if l.epoch != self.epoch {
                    // A retired Connector's late report.
                    return;
                }
                let up = |s: Option<&LinkState>| matches!(s, Some(LinkState::Connected { .. }));
                if !(up(l.link.as_ref()) && up(Some(s))) {
                    l.conn_epoch += 1;
                }
                l.link = Some(s.clone());
            }
            r.queue_routing();
            r.emit();
        }
    }

    fn roster(&self, chain: &RosterChain) {
        if let Some(r) = self.ring.upgrade() {
            {
                let l = r.lock();
                if l.epoch != self.epoch {
                    return;
                }
                if let Some(owner) = &l.routing {
                    owner.adopt(chain);
                }
            }
            // Fence effective authorization before disk I/O; a failed save cannot keep a
            // removed/terminal member's live Link authorized.
            r.queue_routing();
            r.persist_newer(chain);
            r.emit();
        }
    }

    fn presence(&self, _key: SignKey, _p: &MemberPresence) {
        if let Some(r) = self.ring.upgrade() {
            if r.lock().epoch != self.epoch {
                return;
            }
            r.queue_routing();
            r.emit();
        }
    }

    fn farewell(&self, f: &FarewellState) {
        let Some(r) = self.ring.upgrade() else {
            return;
        };
        match f {
            FarewellState::Done { farewell } => r.clear_farewell(self.epoch, farewell),
            FarewellState::Sending { farewell } | FarewellState::Waiting { farewell, .. } => {
                let mut l = r.lock();
                if l.epoch != self.epoch {
                    return;
                }
                l.farewells.insert(farewell.source.clone(), f.clone());
            }
        }
        r.emit();
    }

    fn held(&self, relay: &str, version: u64) {
        if let Some(r) = self.ring.upgrade() {
            if r.note_held(self.epoch, relay, version) {
                r.emit();
            }
        }
    }

    fn quota(&self, relay: &str, _to: Option<SignKey>) {
        if let Some(r) = self.ring.upgrade() {
            if r.quota_refused(self.epoch, relay) {
                r.emit();
            }
        }
    }

    fn accepted(&self) {
        if let Some(r) = self.ring.upgrade() {
            r.quota_accepted(self.epoch);
        }
    }
}

impl DesktopRing {
    /// Opens the stored Ring, if any, and resumes its Relay connection (when no other
    /// instance runs it). Every `owner_retry` it checks the state on disk ([`Store::signature`])
    /// and reconciles with commits of other instances; one that does not own the connection
    /// also retries taking it over.
    pub fn open(cfg: DesktopRingConfig, observer: Arc<dyn RingObserver>) -> Arc<DesktopRing> {
        let store = Store::new(cfg.dir.clone());
        let (routing_tx, routing_rx) = std::sync::mpsc::sync_channel(1);
        let ring = Arc::new_cyclic(|me| DesktopRing {
            cfg,
            store,
            adoption: Arc::new(AtomicBool::new(false)),
            adoption_cancel: Mutex::new(None),
            adoption_baseline: Default::default(),
            observer,
            live: Mutex::new(Live::default()),
            routing_manager: Mutex::new(None),
            routing_tx,
            #[cfg(any(unix, windows))]
            host_broker: Mutex::new(None),
            commit: Mutex::new(()),
            me: me.clone(),
            configured: Mutex::new(Arc::new(|_: &str| true)),
            deferred_emit: AtomicBool::new(false),
            #[cfg(test)]
            after_owner_load: Mutex::new(None),
            #[cfg(test)]
            after_commit: Mutex::new(None),
            #[cfg(test)]
            before_held: Mutex::new(None),
            #[cfg(test)]
            before_publish_wait: Mutex::new(None),
            #[cfg(test)]
            commit_owner: Mutex::new(None),
            #[cfg(test)]
            before_probe_apply: Mutex::new(None),
            #[cfg(test)]
            probe_results: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&ring);
        std::thread::Builder::new()
            .name("ring-hosts".into())
            .spawn(move || loop {
                let received = routing_rx.recv_timeout(Duration::from_millis(100));
                if matches!(
                    received,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                ) {
                    return;
                }
                let Some(r) = weak.upgrade() else {
                    return;
                };
                if r.lock().quitting {
                    return;
                }
                if received.is_ok() {
                    r.project_hosts();
                } else {
                    #[cfg(any(unix, windows))]
                    let manager = r
                        .routing_manager
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(Weak::upgrade);
                    #[cfg(any(unix, windows))]
                    if let Some(manager) = manager {
                        let generation = r.lock().routing.as_ref().map(|owner| owner.generation);
                        r.maintain_host_broker(&manager, generation);
                    }
                }
            })
            .expect("spawn Ring Host projection");
        let mut seen = ring.store.signature();
        ring.reload();
        let weak = Arc::downgrade(&ring);
        let every = ring.cfg.owner_retry;
        let _ = std::thread::Builder::new()
            .name("ring-reconcile".into())
            .spawn(move || loop {
                std::thread::sleep(every);
                let Some(r) = weak.upgrade() else { return };
                let (quitting, other) = {
                    let l = r.lock();
                    (l.quitting, l.other_window)
                };
                if quitting {
                    return;
                }
                let now = r.store.signature();
                if now != seen || other {
                    seen = now;
                    r.reload();
                    r.emit();
                }
                r.quota_tick();
            });
        ring
    }

    /// Bind the backend Manager to this profile. Only the elected Connector owner can
    /// create Relay handles; followers receive availability, not a second Connector.
    pub fn attach_manager(&self, manager: &Arc<crate::Manager>) -> Result<(), String> {
        if !cfg!(any(unix, windows)) {
            return Err(
                "persistent protected Host broker adapter is not implemented on this platform"
                    .into(),
            );
        }
        #[cfg(any(unix, windows))]
        {
            super::store::ensure_private_dir(&self.cfg.dir).map_err(|e| e.to_string())?;
            let dir = xshell_core::local_ipc::canonical_store(&self.cfg.dir)
                .map_err(|e| e.to_string())?;
            xshell_core::local_ipc::private_dir(&dir).map_err(|e| e.to_string())?;
            xshell_core::local_ipc::host_broker_endpoint(&dir)
                .and_then(|p| xshell_core::local_ipc::check_path(&p))
                .map_err(|e| e.to_string())?;
        }
        let mut bound = self.routing_manager.lock().unwrap();
        if bound
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|old| !Arc::ptr_eq(&old, manager))
        {
            return Err("this Ring already has an owner Host Manager".into());
        }
        *bound = Some(Arc::downgrade(manager));
        drop(bound);
        self.queue_routing();
        Ok(())
    }

    pub(super) fn host_identity_observed(&self) {
        self.queue_routing();
    }

    fn queue_routing(&self) {
        let _ = self.routing_tx.try_send(());
    }

    fn project_hosts(&self) {
        let Some(manager) = self
            .routing_manager
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
        else {
            return;
        };
        let projection = {
            let l = self.lock();
            if l.quitting {
                None
            } else {
                l.state.clone().map(|mut state| {
                    if let Some(owner) = &l.routing {
                        state.chain = owner.chain();
                    }
                    RingHosts {
                        state,
                        owner: l.routing.clone(),
                    }
                })
            }
        };
        #[cfg(any(unix, windows))]
        let owner_generation = projection
            .as_ref()
            .and_then(|p| p.owner.as_ref().map(|o| o.generation));
        #[cfg(any(unix, windows))]
        let elected = self.pairing_ipc_owner();
        #[cfg(any(unix, windows))]
        {
            // This routing worker is the only broker binder/rebinder. Retire old Host
            // target resolution before publishing a changed authority generation, then
            // release all guards before Manager callbacks or server retirement/rebind.
            let binding = self.host_broker.lock().unwrap();
            if let Some(binding) = binding.as_ref() {
                if binding.generation != owner_generation || !elected {
                    if let Some(server) = &binding.server {
                        server.broker.retire_host_generation();
                    }
                }
            }
        }
        manager.set_ring_hosts(projection);
        #[cfg(any(unix, windows))]
        self.maintain_host_broker(&manager, owner_generation);
    }

    #[cfg(any(unix, windows))]
    fn maintain_host_broker(&self, manager: &Arc<crate::Manager>, generation: Option<u64>) {
        let elected = self.pairing_ipc_owner();
        let mut binding = self.host_broker.lock().unwrap();
        if self.lock().quitting {
            return;
        }
        if binding.is_none() {
            let Ok(dir) = xshell_core::local_ipc::canonical_store(&self.cfg.dir) else {
                return;
            };
            if xshell_core::local_ipc::private_dir(&dir).is_err() {
                return;
            }
            let Ok(profile) = xshell_core::local_ipc::host_broker_profile(&dir) else {
                return;
            };
            let Ok(path) = xshell_core::local_ipc::host_broker_endpoint(&dir) else {
                return;
            };
            let client = super::broker::BrokerClient::new(profile.clone(), manager);
            *binding = Some(HostBrokerBinding {
                generation: None,
                join_receipt_generation: None,
                server: None,
                client,
                path,
                profile,
            });
        }
        let binding = binding.as_mut().expect("initialized");
        if binding.generation != generation || !elected {
            // Preserve only this active guest's own unpaired -> accepted transition,
            // never an owner loss, revocation or a later membership generation.
            let defer = elected
                && binding.generation.is_none()
                && generation.is_some()
                && (binding.join_receipt_generation.is_none()
                    || binding.join_receipt_generation == generation)
                && binding
                    .server
                    .as_ref()
                    .is_some_and(|server| !server.broker.administration.lock().unwrap().is_empty())
                && {
                    let effective = self
                        .adoption_owner_connector()
                        .and_then(|owner| owner.try_chain());
                    let installed_operation = self
                        .store
                        .load_profile()
                        .ok()
                        .and_then(|(p, _)| p)
                        .and_then(|p| {
                            let j = p.journal.as_ref()?;
                            let m = p.membership.as_ref()?;
                            let companion = p.companion.as_ref()?;
                            (j.desktop_installed
                                && j.daemon_installed
                                && j.phase == super::store::AdoptionPhase::Installed
                                && j.accepted.last().map(String::as_str)
                                    == Some(m.chain.head().token())
                                && m.chain.authorized_member(&p.keys.sign_key()).is_some()
                                && m.chain
                                    .authorized_member(&companion.daemon.sign_key)
                                    .is_some()
                                && effective.as_ref().is_some_and(|current| {
                                    current.head() == m.chain.head()
                                        && current.authorized_member(&p.keys.sign_key()).is_some()
                                        && current
                                            .authorized_member(&companion.daemon.sign_key)
                                            .is_some()
                                }))
                            .then(|| j.intent.operation.clone())
                        });

                    installed_operation.is_some_and(|operation| {
                        binding.server.as_ref().is_some_and(|server| {
                            server
                                .broker
                                .administration
                                .lock()
                                .unwrap()
                                .installing_transition(&operation)
                        })
                    })
                };
            if defer {
                binding.join_receipt_generation = generation;
                if let Some(server) = &binding.server {
                    server.broker.retire_host_generation();
                }
            } else {
                binding.server.take();
                binding.generation = generation;
                binding.join_receipt_generation = None;
            }
        }
        manager.set_broker(binding.client.clone(), !elected);
        if elected && binding.server.is_none() {
            let broker = super::broker::Broker::new(binding.profile.clone(), manager);
            if let Ok(server) = super::broker::Server::bind(binding.path.clone(), broker) {
                binding.server = Some(server);
            }
        }
        if !binding.client.has_transport() {
            if let Some(server) = &binding.server {
                let _ = server.local(&binding.client);
            } else if !elected {
                let _ = binding.client.connect(&binding.path);
            }
        }
        binding.client.tick();
        if elected {
            if let (Some(server), Some(ring)) = (&binding.server, self.me.upgrade()) {
                super::administration::dispatch_pending(&ring, &server.broker.administration);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_commit(&self) -> CommitGuard<'_> {
        let guard = self.commit.lock().unwrap_or_else(|e| e.into_inner());
        COMMITS_HELD.with(|c| c.set(c.get() + 1));
        #[cfg(test)]
        {
            *self.commit_owner.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::thread::current().id());
        }
        CommitGuard {
            ring: self,
            guard: Some(guard),
        }
    }

    /// Tells the observer about the current view. Under a commit lock (a Connector callback
    /// made synchronously from `adopt`), only noted: the guard emits when it is released.
    fn emit(&self) {
        if COMMITS_HELD.with(|c| c.get()) > 0 {
            self.deferred_emit.store(true, Ordering::Release);
            return;
        }
        self.observer.changed(&self.view());
    }

    /// Tells which Host ids are still configured in Settings → Hosts (by default every one
    /// is). A Daemon member mapped only by Hosts that are not is shown without a Host and can
    /// be removed. `f` may take the Hosts manager's lock or read the settings file: it is
    /// never called under a `DesktopRing` lock.
    pub fn set_configured_hosts(&self, f: ConfiguredHosts) {
        *self.configured.lock().unwrap_or_else(|e| e.into_inner()) = f;
        self.emit();
    }

    fn configured(&self) -> ConfiguredHosts {
        self.configured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Re-reads the state from disk and adopts it (under the commit lock, fenced), then
    /// tries to run the Connector.
    pub(super) fn reload(&self) {
        {
            let _c = self.lock_commit();
            if let Ok(Some(s)) = self.tx(|cur, _| Ok((None, cur))) {
                self.adopt(s);
            }
        }
        self.ensure_connector();
    }

    /// After a local Leave: the Ring held in memory is dropped and its Connector retired,
    /// as for moved-aside settings but with no recovery problem.
    pub(in crate::ring) fn forget_membership(&self) {
        let retire = {
            let _c = self.lock_commit();
            let mut l = self.lock();
            l.state = None;
            l.state_revision += 1;
            l.farewells.clear();
            l.link = None;
            l.quota.retire();
            if let Some(owner) = l.routing.take() {
                owner.stop();
            }
            self.queue_routing();
            l.connector.take()
        };
        if let Some(old) = retire {
            let _ = std::thread::Builder::new()
                .name("ring-retire".into())
                .spawn(move || old.stop(ByeReason::quit()));
        }
        self.emit();
    }

    /// A transaction whose recovery metadata reaches the view, whether it succeeded or not.
    /// Moved-aside settings put the view in the recovery-required state: the Ring held in
    /// memory is dropped (its Connector retired) until an explicit start-over.
    fn tx<R>(
        &self,
        f: impl FnOnce(
            Option<RingState>,
            &super::store::Loaded,
        ) -> Result<(Option<RingState>, R), String>,
    ) -> Result<R, String> {
        let (r, loaded) = self.store.transact_full(f);
        let retire = {
            let mut l = self.lock();
            match (&loaded, &r) {
                (Some(loaded), _) => match &loaded.moved_aside {
                    Some(p) => {
                        l.problem = Some(("recovered".to_string(), p.display().to_string()));
                        l.state = None;
                        l.state_revision += 1;
                        l.farewells.clear();
                        l.link = None;
                        l.quota.retire();
                        if let Some(owner) = l.routing.take() {
                            owner.stop();
                        }
                        self.queue_routing();
                        l.connector.take()
                    }
                    None => {
                        l.problem = None;
                        None
                    }
                },
                // Not even loaded: the file or its directory is refused.
                (None, Err(e)) => {
                    l.problem = Some(("unreadable".to_string(), e.clone()));
                    None
                }
                (None, Ok(_)) => None,
            }
        };
        if let Some(old) = retire {
            let _ = std::thread::Builder::new()
                .name("ring-retire".into())
                .spawn(move || old.stop(ByeReason::quit()));
        }
        r
    }

    #[cfg(test)]
    fn hook(&self) {
        if let Some(h) = self.after_commit.lock().unwrap().as_ref() {
            h();
        }
    }

    #[cfg(not(test))]
    fn hook(&self) {}

    #[cfg(test)]
    fn publish_wait_hook(&self) {
        if let Some(h) = self.before_publish_wait.lock().unwrap().as_ref() {
            h();
        }
    }

    #[cfg(not(test))]
    fn publish_wait_hook(&self) {}

    /// Adopts a committed state (this instance's or another's) into `live` and the
    /// Connector. Under the commit lock; a state older than the one held (same Ring, lower
    /// version) is refused as stale, and an unchanged one (the held Relays aside) is not
    /// handed to the Connector.
    fn adopt(&self, s: RingState) -> bool {
        let (retire, connector) = {
            let mut l = self.lock();
            let mut unchanged = false;
            if let Some(cur) = &l.state {
                if cur.chain.ring_id() == s.chain.ring_id()
                    && s.chain.head().version() < cur.chain.head().version()
                {
                    return false;
                }
                // The held Relays are not the Connector's business.
                unchanged = cur.chain == s.chain
                    && cur.farewells == s.farewells
                    && cur.local_member == s.local_member
                    && cur.host_members == s.host_members;
            }
            let other_ring = l
                .state
                .as_ref()
                .is_some_and(|c| c.chain.ring_id() != s.chain.ring_id());
            let other_relay = l.state.as_ref().is_some_and(|c| {
                c.chain.head().roster().relay_url != s.chain.head().roster().relay_url
            });
            if other_ring {
                if let Some(owner) = l.routing.take() {
                    owner.stop();
                }
            } else if let Some(owner) = &l.routing {
                owner.adopt(&s.chain);
            }
            self.queue_routing();
            l.state = Some(s.clone());
            l.state_revision += 1;
            // Reports about farewells no longer owed are of no use.
            l.farewells
                .retain(|source, _| s.farewells.iter().any(|f| &f.source == source));
            if other_relay && !other_ring {
                // The quota notice was the old Relay's.
                l.quota.retire();
            }
            if other_ring {
                l.farewells.clear();
                l.link = None;
                l.quota.retire();
                (l.connector.take(), None)
            } else if unchanged && l.connector.is_some() {
                return true;
            } else {
                (None, l.connector.clone())
            }
        };
        if let Some(old) = retire {
            // A Ring started over: the old Connector says goodbye on its own thread (its
            // callbacks may wait for the commit lock held here).
            let _ = std::thread::Builder::new()
                .name("ring-retire".into())
                .spawn(move || old.stop(ByeReason::quit()));
        }
        match connector {
            Some(c) => {
                // The chain first: the Connector keeps only the farewells it still owes.
                c.set_chain(s.chain.clone());
                c.set_farewells(s.farewells.clone());
            }
            None => self.ensure_connector(),
        }
        true
    }

    /// Starts the Connector for the state held, once, if this instance can own it. Having
    /// just won the owner lock, it re-reads the state from disk first: the previous owner
    /// may have committed since this instance last looked.
    fn ensure_connector(&self) {
        let (fresh, revision) = {
            let mut l = self.lock();
            if l.connector.is_some() || l.quitting {
                return;
            }
            let revision = l.state_revision;
            if l.owner.is_some() {
                (false, revision)
            } else {
                let lock = super::store::open_lock(&self.cfg.dir.join("ring-connector.lock"));
                match lock.map(|f| f.try_lock().map(|_| f)) {
                    Ok(Ok(f)) => {
                        l.owner = Some(f);
                        l.other_window = false;
                        (true, revision)
                    }
                    Ok(Err(std::fs::TryLockError::WouldBlock)) => {
                        l.other_window = true;
                        return;
                    }
                    Ok(Err(std::fs::TryLockError::Error(e))) | Err(e) => {
                        l.link = Some(LinkState::Stopped {
                            error: Some(format!("cannot take the connection lock: {e}")),
                        });
                        return;
                    }
                }
            }
        };
        if fresh {
            let loaded = self.store.load();
            #[cfg(test)]
            if let Some(hook) = self.after_owner_load.lock().unwrap().as_ref() {
                hook();
            }
            let mut l = self.lock();
            // A public owner admission can read outside `commit`. Install only while
            // the exact adoption/departure state we elected for is still current. Even
            // a missing/error result must not erase an intervening accepted membership.
            if l.quitting || l.connector.is_some() || l.state_revision != revision {
                return;
            }
            l.state_revision += 1;
            match loaded {
                Ok((state, _)) => l.state = state,
                Err(error) => {
                    l.problem = Some(("unreadable".into(), error));
                    l.state = None;
                    self.queue_routing();
                    return;
                }
            }
            self.queue_routing();
        }
        let mut l = self.lock();
        if l.connector.is_some() || l.quitting {
            return;
        }
        let Some(s) = l.state.clone() else {
            return;
        };
        let mut client = RingClientConfig::new(s.chain.clone(), s.keys.clone());
        client.timeouts = self.cfg.timeouts;
        let mut cfg = ConnectorConfig::new(client);
        cfg.backoff_unit = self.cfg.backoff_unit;
        cfg.farewells = s.farewells.clone();
        cfg.farewell_backoff_unit = self.cfg.farewell_backoff_unit;
        cfg.farewell_backoff_max = self.cfg.farewell_backoff_max;
        l.epoch += 1;
        let events = Events {
            ring: self.me.clone(),
            epoch: l.epoch,
        };
        let sessions = Sessions::new(SessionsConfig::new(s.keys.clone()), None);
        let routing = OwnerRouting::new(sessions.clone(), l.epoch, s.chain.clone());
        let session_events = SessionEvents {
            sessions,
            next: Some(Arc::new(events)),
        };
        match Connector::start(cfg, Arc::new(session_events)) {
            Ok(c) => {
                let connector = Arc::new(c);
                routing.attach(&connector);
                l.connector = Some(connector);
                l.routing = Some(routing);
                self.queue_routing();
            }
            Err(e) => {
                l.link = Some(LinkState::Stopped {
                    error: Some(format!("cannot start the relay connection: {e}")),
                })
            }
        }
    }

    /// A newer chain from the Relay: stored if it extends what is on disk.
    fn persist_newer(&self, chain: &RosterChain) {
        let _c = self.lock_commit();
        let r = self.tx(|cur, _| {
            let Some(mut s) = cur else {
                return Ok((None, None));
            };
            if s.chain.ring_id() != chain.ring_id()
                || chain.head().version() <= s.chain.head().version()
            {
                return Ok((None, None));
            }
            match s.chain.extended(chain.since(s.chain.head().version())) {
                Ok((c, _)) => {
                    s.chain = c;
                    s.retarget_farewells();
                    Ok((Some(s.clone()), Some(s)))
                }
                Err(_) => Ok((None, None)),
            }
        });
        match r {
            Ok(Some(s)) => {
                self.hook();
                self.adopt(s);
            }
            Ok(None) => {}
            Err(e) => eprintln!("xshell: cannot save the Roster: {e}"),
        }
    }

    /// The old Relay `done.source` took the chain up to `done.target`: the farewell owed to it
    /// is dropped unless it asks for a newer head (then the Connector gets it again), and the
    /// Relay is recorded as held. Only for the Connector of `epoch`.
    fn clear_farewell(&self, epoch: u64, done: &Farewell) {
        let _c = self.lock_commit();
        {
            let mut l = self.lock();
            if l.epoch != epoch || l.connector.is_none() {
                return;
            }
            l.farewells.remove(&done.source);
        }
        let r = self.tx(|cur, _| {
            let Some(mut s) = cur else {
                return Ok((None, None));
            };
            let before = (s.farewells.clone(), s.held_relays.clone());
            s.farewells
                .retain(|f| !(f.source == done.source && f.target <= done.target));
            s.held_relays.insert(done.source.clone());
            if (s.farewells.clone(), s.held_relays.clone()) == before {
                return Ok((None, None));
            }
            Ok((Some(s.clone()), Some(s)))
        });
        match r {
            Ok(Some(s)) => {
                self.hook();
                self.adopt(s);
            }
            Ok(None) => {}
            Err(e) => eprintln!("xshell: cannot save the Ring state: {e}"),
        }
    }

    /// `relay` holds the Ring, at least up to `version`: recorded (only for the Connector of
    /// `epoch`). A Relay recorded only now that the head no longer names is owed a farewell
    /// from the newest version up to `version` that names it: the report came late, after the
    /// Relay change that would have owed it. `true`: changed.
    fn note_held(&self, epoch: u64, relay: &str, version: u64) -> bool {
        #[cfg(test)]
        if let Some(h) = self.before_held.lock().unwrap().as_ref() {
            h(relay);
        }
        let _c = self.lock_commit();
        {
            let l = self.lock();
            let known = l
                .state
                .as_ref()
                .is_some_and(|s| s.held_relays.contains(relay));
            if l.epoch != epoch || l.connector.is_none() || known {
                return false;
            }
        }
        let r = self.tx(|cur, _| {
            let Some(mut s) = cur else {
                return Ok((None, None));
            };
            if !s.held_relays.insert(relay.to_string()) {
                return Ok((None, None));
            }
            let head = s.chain.head();
            if head.roster().relay_url != relay && version < head.version() {
                let from = (1..=version).rev().find(|v| {
                    s.chain
                        .get(*v)
                        .is_some_and(|r| r.roster().relay_url == relay)
                });
                if let Some(from) = from {
                    s.owe_farewell(from);
                }
            }
            Ok((Some(s.clone()), Some(s)))
        });
        match r {
            Ok(Some(s)) => {
                self.hook();
                self.adopt(s)
            }
            Ok(None) => false,
            Err(e) => {
                eprintln!("xshell: cannot save the Ring state: {e}");
                false
            }
        }
    }

    /// Enables Mobile access: creates the Ring on the default Relay, with the local Daemon
    /// in version 1 when its identity is known. A Ring that exists already (made by this or
    /// another instance) is returned as it is. After an unreadable Ring was moved aside, a new
    /// one (a new identity) is made only with `start_over`.
    pub fn enable(
        &self,
        local: Option<LocalIdentity>,
        start_over: bool,
    ) -> Result<RingView, String> {
        let retained = self.store.load_profile()?.0.map(|profile| profile.keys);
        {
            let _c = self.lock_commit();
            let (s, created) = self.tx(|cur, loaded| {
                if let Some(s) = cur {
                    return Ok((None, (s, false)));
                }
                if loaded.moved_aside.is_some() && !start_over {
                    return Err(START_OVER_REQUIRED.to_string());
                }
                let s = self.genesis(local.as_ref(), retained.clone())?;
                Ok((Some(s.clone()), (s, true)))
            })?;
            if created {
                self.lock().problem = None;
            }
            self.hook();
            self.adopt(s);
        }
        self.emit();
        Ok(self.view())
    }

    fn genesis(
        &self,
        local: Option<&LocalIdentity>,
        retained: Option<Arc<DeviceKeys>>,
    ) -> Result<RingState, String> {
        let keys = match retained {
            Some(keys) => keys,
            None => Arc::new(DeviceKeys::generate().map_err(|e| e.to_string())?),
        };
        let t = now();
        let url = &self.cfg.default_relay_url;
        let genesis = match local {
            Some(d) => {
                let me = keys.sign_key();
                let roster = Roster {
                    v: 1,
                    ring_id: RingId::derive(&me),
                    version: 1,
                    prev: None,
                    relay_url: url.clone(),
                    signed_by: me,
                    issued_at: t,
                    members: vec![
                        Member::new(&self.cfg.name, Role::Desktop, me, keys.noise_key(), t),
                        Member::new(&d.name, Role::Daemon, d.sign_key, d.noise_key, t),
                    ],
                    extra: Default::default(),
                };
                let g = roster.sign(&*keys).map_err(|e| e.to_string())?;
                verify_genesis(&g).map_err(|e| e.to_string())?;
                g
            }
            None => SignedRoster::genesis(&*keys, keys.noise_key(), &self.cfg.name, url, t)
                .map_err(|e| e.to_string())?,
        };
        Ok(RingState {
            keys,
            chain: RosterChain::from_chain(vec![genesis]).map_err(|e| e.to_string())?,
            local_member: local.map(|d| d.sign_key),
            farewells: Vec::new(),
            held_relays: BTreeSet::new(),
            host_members: BTreeMap::new(),
        })
    }

    /// Moves the Ring to `url`: a new Roster version, which the Connector follows at once. The
    /// old Relay is owed a farewell if it held the Ring, and every farewell owed now carries
    /// the new head; a farewell owed never holds a change up. Returns the new chain, for the
    /// local Daemon. The same URL is a no-op.
    pub fn set_relay_url(&self, url: &str) -> Result<RosterChain, String> {
        let url = url.trim();
        RelayUrl::parse(url).map_err(|e| e.to_string())?;
        let chain = {
            let _c = self.lock_commit();
            let (s, changed) = self.tx(|cur, _| {
                let mut s = cur.ok_or("mobile access is not enabled")?;
                if s.chain.head().roster().relay_url == url {
                    return Ok((None, (s, false)));
                }
                if s.chain.head().roster().v == 2 {
                    return Err(RELAY_FIXED.to_string());
                }
                let old = s.chain.head().version();
                let next = s
                    .chain
                    .head()
                    .next(&*s.keys, now(), |d| d.relay_url = url.to_string())
                    .map_err(|e| e.to_string())?;
                s.chain
                    .accept(std::slice::from_ref(&next))
                    .map_err(|e| e.to_string())?;
                s.retarget_farewells();
                let left = &s.chain.get(old).expect("the old head").roster().relay_url;
                if s.held_relays.contains(left) {
                    s.owe_farewell(old);
                }
                Ok((Some(s.clone()), (s, true)))
            })?;
            self.hook();
            self.adopt(s.clone());
            if !changed {
                return Ok(s.chain);
            }
            s.chain
        };
        self.emit();
        Ok(chain)
    }

    /// The local Daemon is in the head under `local`'s key, as a `daemon`: added in a new
    /// version, or replacing an older key of this computer that no other Host maps to.
    /// Idempotent. `None`: no Ring.
    pub fn ensure_local_daemon(
        &self,
        local: &LocalIdentity,
    ) -> Result<Option<RosterChain>, String> {
        let h = HostRef {
            id: LOCAL_HOST_ID.to_string(),
            name: local.name.clone(),
            target: String::new(),
        };
        match self.ensure_host_daemons(&[(h, local.clone())], &|_| true, None)? {
            None => Ok(None),
            Some((c, out)) => match out.first().map(|o| o.1) {
                Some(HostOutcome::OtherRole) => {
                    Err("this computer's key is in the ring with another role".into())
                }
                Some(HostOutcome::Full) => Err(format!(
                    "the ring already has the maximum of {MAX_MEMBERS} devices"
                )),
                _ => Ok(Some(c)),
            },
        }
    }

    /// Every Daemon of `batch` in the head as a `daemon` ([`plan_hosts`] has the rules), in
    /// one transaction: at most one new Roster version for the whole batch. An entry for
    /// which `valid` (called once each, inside the transaction) is false is discarded as
    /// [`HostOutcome::Stale`]. With `ring`, the batch was decided for that Ring: when the
    /// state on disk is another Ring's, nothing changes and every entry is
    /// [`HostOutcome::RingChanged`]. Idempotent. `None`: no Ring.
    pub fn ensure_host_daemons(
        &self,
        batch: &[(HostRef, LocalIdentity)],
        valid: &dyn Fn(&str) -> bool,
        ring: Option<&RingId>,
    ) -> Result<Option<(RosterChain, HostOutcomes)>, String> {
        let (chain, outcomes, changed) = {
            let _c = self.lock_commit();
            let r = self.tx(|cur, _| {
                let Some(mut s) = cur else {
                    return Ok((None, None));
                };
                if ring.is_some_and(|r| r != s.chain.ring_id()) {
                    let out = batch
                        .iter()
                        .map(|e| (e.0.id.clone(), HostOutcome::RingChanged))
                        .collect();
                    return Ok((None, Some((s, out, false))));
                }
                let mut outcomes = Vec::new();
                let mut live = Vec::new();
                for e in batch {
                    if self
                        .lock()
                        .routing
                        .as_ref()
                        .is_some_and(|owner| owner.revoked_key(&e.1.sign_key))
                    {
                        outcomes.push((e.0.id.clone(), HostOutcome::Revoked));
                    } else if valid(&e.0.id) {
                        live.push(e);
                    } else {
                        outcomes.push((e.0.id.clone(), HostOutcome::Stale));
                    }
                }
                let t = now();
                let mut p = plan_hosts(&s, &live, t);
                if s.chain.head().roster().v == 2 {
                    enrolled_only(&s, &mut p);
                }
                outcomes.extend(p.outcomes.iter().cloned());
                let mut dirty = p.after != mapping(&s);
                if p.edits_roster() {
                    let next = s
                        .chain
                        .head()
                        .next(&*s.keys, t, |d| {
                            for k in &p.remove {
                                d.remove(k);
                            }
                            for (k, name) in &p.rename {
                                if let Some(m) = d.members.iter_mut().find(|m| m.sign_key == *k) {
                                    m.name = name.clone();
                                }
                            }
                            for m in &p.add {
                                d.add(m.clone());
                            }
                        })
                        .map_err(|e| e.to_string())?;
                    s.chain
                        .accept(std::slice::from_ref(&next))
                        .map_err(|e| e.to_string())?;
                    s.retarget_farewells();
                    dirty = true;
                }
                s.local_member = p.after.get(LOCAL_HOST_ID).map(|(k, _)| *k);
                s.host_members = p
                    .after
                    .iter()
                    .filter(|(id, _)| id.as_str() != LOCAL_HOST_ID)
                    .map(|(id, (k, t))| {
                        (
                            id.clone(),
                            HostMember {
                                sign_key: *k,
                                target: t.clone(),
                            },
                        )
                    })
                    .collect();
                let out = (s.clone(), outcomes, dirty);
                Ok((dirty.then_some(s), Some(out)))
            })?;
            let Some((s, outcomes, changed)) = r else {
                return Ok(None);
            };
            if changed {
                self.hook();
                self.adopt(s.clone());
            }
            (s.chain, outcomes, changed)
        };
        if changed {
            self.emit();
        }
        Ok(Some((chain, outcomes)))
    }

    /// The same connection election owns the private enrollment endpoint, even before
    /// a Ring exists. Missing membership never creates a Ring or a second signer.
    pub(super) fn proposal_snapshot(
        &self,
    ) -> Result<
        (
            RosterChain,
            xshell_protocol::ring::relay::connector::ProposalConnection,
        ),
        String,
    > {
        let connector = self
            .lock()
            .connector
            .clone()
            .ok_or("creator Connector is not available")?;
        let lane = connector.proposal_connection().map_err(|e| e.to_string())?;
        let chain = connector.chain();
        if chain.head() != lane.predecessor() {
            return Err("creator snapshot changed before proposal reservation".into());
        }
        Ok((chain, lane))
    }

    pub(super) fn adoption_owner_connector(&self) -> Option<Arc<Connector>> {
        self.lock().connector.clone()
    }

    /// The current owner's effective evidence, not Live.state's version-only cache.
    /// Hold the owner slot stable during bounded Connector snapshot admission; never
    /// take a file lock or perform network work while holding this lifecycle lock.
    pub(super) fn adoption_effective_chain(&self) -> Result<Option<RosterChain>, String> {
        let budget = xshell_core::local_ipc::RequestBudget::for_operation(
            Instant::now() + Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
        );
        let live = budget.lock(&self.live).map_err(|e| e.to_string())?;
        let mut effective = live.state.as_ref().map(|s| s.chain.clone());
        if let Some(connector) = &live.connector {
            let owner = loop {
                budget.check().map_err(|e| e.to_string())?;
                if let Some(chain) = connector.try_chain() {
                    break chain;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            effective = Some(match effective {
                Some(mut chain) => {
                    chain.merge_terminal_from(&owner);
                    chain
                        .extended(owner.versions())
                        .map_err(|e| e.to_string())?
                        .0
                }
                None => owner,
            });
        }
        Ok(effective)
    }

    /// Native startup workers stop with the backend, before releasing election ownership.
    pub fn is_quitting(&self) -> bool {
        self.lock().quitting
    }

    /// Focus can request presentation, never confirmation. Consumed only by native setup.
    #[cfg(unix)]
    pub fn take_administration_focus_request(&self) -> bool {
        self.host_broker
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.server.as_ref())
            .is_some_and(|s| s.broker.administration.lock().unwrap().take_focus_request())
    }

    /// Owner-local backend controller access. This is not native-window attestation;
    /// C1 must bind its private lease to the actual injected Tauri origin.
    #[cfg(unix)]
    pub fn administration_join(
        &self,
        operation: uuid::Uuid,
    ) -> Option<Arc<super::adoption::JoinOffer>> {
        if !self.pairing_ipc_owner() {
            return None;
        }
        self.host_broker
            .lock()
            .unwrap()
            .as_ref()?
            .server
            .as_ref()?
            .broker
            .administration
            .lock()
            .unwrap()
            .controller(operation)
    }

    /// Private-controller projection for owner-local dispatch/presentation, never IPC DTOs.
    #[cfg(unix)]
    pub fn administration_joins(&self) -> Vec<(uuid::Uuid, Arc<super::adoption::JoinOffer>)> {
        if !self.pairing_ipc_owner() {
            return vec![];
        }
        self.host_broker
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|b| b.server.as_ref())
            .map(|s| s.broker.administration.lock().unwrap().controllers())
            .unwrap_or_default()
    }

    pub fn pairing_ipc_owner(&self) -> bool {
        self.ensure_connector();
        let state = self.lock();
        state.owner.is_some() && !state.quitting
    }

    pub fn enrollment_scope(
        &self,
        ring: &RingId,
        device_name: String,
    ) -> Result<Scope, AccessError> {
        if !xshell_protocol::pairing_access::printable(
            &device_name,
            xshell_protocol::pairing_access::LABEL_MAX,
        ) {
            return Err(AccessError::InvalidRequest);
        }
        let s = self.pairing_state().map_err(|e| {
            if e == PAIR_NOT_ENABLED {
                AccessError::MissingRing
            } else {
                AccessError::Busy
            }
        })?;
        if s.chain.ring_id() != ring {
            return Err(AccessError::ScopeMismatch);
        }
        let head = s.chain.head();
        Ok(Scope {
            ring: ring.clone(),
            approver: s.keys.sign_key(),
            role: MobileRole::Mobile,
            purpose: ReachabilityPurpose::MobileEnrollment,
            enrollment: Some(EnrollmentTarget {
                approver_noise: s.keys.noise_key(),
                relay: head.roster().relay_url.clone(),
                device_name,
                initial_head: ChainHead {
                    version: head.version(),
                    hash: head.hash(),
                },
            }),
        })
    }

    pub fn enrollment_offer(
        &self,
        task: uuid::Uuid,
        operation: PairOperationId,
        parameters: EnrollmentParameters,
    ) -> Result<EnrollmentStart, AccessError> {
        operation.validate()?;
        parameters.validate(now())?;
        if task.is_nil() {
            return Err(AccessError::InvalidRequest);
        }
        {
            let mut live = self.lock();
            if live
                .enrollment
                .as_ref()
                .is_some_and(|e| !e.released && now() < e.parameters.expires_at)
            {
                return Err(AccessError::Busy);
            }
            live.enrollment = Some(EnrollmentOperation {
                task,
                parameters: parameters.clone(),
                response: EnrollmentResponse {
                    progress: Some(EnrollmentProgress {
                        operation,
                        stage: EnrollmentStage::HelperReserved,
                    }),
                    receipt: Some(EnrollmentReceipt {
                        operation,
                        evidence: EnrollmentEvidence::OutcomeUnknown,
                    }),
                    error: None,
                },
                released: false,
                reserved: false,
                publishing: false,
            });
        }
        struct Observer;
        impl PairingObserver for Observer {
            fn pairing(&self, _: PairingFlow, _: &PairingEvent) {}
        }
        match self.pair_phone_scoped(
            operation.uuid(),
            Arc::new(Observer),
            Some(parameters.clone()),
        ) {
            Ok(offer) => {
                self.note_enrollment(
                    operation.uuid(),
                    EnrollmentStage::DesktopOfferReady,
                    EnrollmentEvidence::OutcomeUnknown,
                );
                Ok(EnrollmentStart {
                    operation,
                    parameters,
                    offer: SecretOffer::new(offer.payload)?,
                })
            }
            Err(_) => {
                self.note_enrollment(
                    operation.uuid(),
                    EnrollmentStage::HelperReserved,
                    EnrollmentEvidence::NoDesktopCommit,
                );
                Err(AccessError::Busy)
            }
        }
    }
    fn note_enrollment(
        &self,
        operation: uuid::Uuid,
        stage: EnrollmentStage,
        evidence: EnrollmentEvidence,
    ) {
        let mut live = self.lock();
        if let Some(enrollment) = &mut live.enrollment {
            let response = &mut enrollment.response;
            if response
                .progress
                .as_ref()
                .is_some_and(|p| p.operation.uuid() == operation)
            {
                let operation = PairOperationId::new(operation).expect("reserved operation");
                response.progress = Some(EnrollmentProgress { operation, stage });
                response.receipt = Some(EnrollmentReceipt {
                    operation,
                    evidence,
                });
            }
        }
    }
    pub fn enrollment_status(
        &self,
        task: uuid::Uuid,
        operation: PairOperationId,
        cancel: bool,
    ) -> EnrollmentResponse {
        let _commit = self.lock_commit();
        let mut live = self.lock();
        let Some(enrollment) = &mut live.enrollment else {
            return EnrollmentResponse::error(AccessError::Unauthorized);
        };
        if enrollment.task != task
            || !enrollment
                .response
                .progress
                .as_ref()
                .is_some_and(|v| v.operation == operation)
        {
            return EnrollmentResponse::error(AccessError::Unauthorized);
        }
        if now() >= enrollment.parameters.expires_at {
            return EnrollmentResponse::error(AccessError::Expired);
        }
        if cancel {
            enrollment.released = true;
        }
        if cancel
            && !enrollment.publishing
            && matches!(
                enrollment.response.receipt.as_ref().map(|r| &r.evidence),
                Some(EnrollmentEvidence::OutcomeUnknown)
            )
        {
            enrollment.response.receipt = Some(EnrollmentReceipt {
                operation,
                evidence: EnrollmentEvidence::NoDesktopCommit,
            });
        }
        let reserved = enrollment.reserved;
        let result = enrollment.response.clone();
        if cancel {
            // A pre-reservation cancellation belongs to this exact recorded task, not an
            // arbitrary public operation ID. Failed/colliding starts own no GUI operation.
            if !live
                .pairing_operations
                .iter()
                .any(|(op, _)| *op == operation.uuid())
            {
                live.pairing_operations
                    .retain(|(_, until)| Instant::now() < *until);
                if live.pairing_operations.len() < xshell_protocol::pairing_access::CACHE_MAX {
                    live.pairing_operations
                        .push((operation.uuid(), Instant::now() + Duration::from_secs(600)));
                }
            }
            if reserved {
                for (flow, op, flag) in &live.pairing {
                    if *flow == PairingFlow::Phone && *op == operation.uuid() {
                        flag.store(true, Ordering::Release);
                    }
                }
            }
        }
        result
    }

    /// Starts a phone offer, replacing the one shown. The slot on the Relay is held before
    /// this returns; a thread waits for the phone and runs the Desktop's side.
    pub fn pair_phone(&self, observer: Arc<dyn PairingObserver>) -> Result<PhoneOffer, String> {
        self.pairing_state()?;
        self.cancel_pairing(PairingFlow::Phone);
        self.pair_phone_owned(uuid::Uuid::new_v4(), observer)
    }

    /// GUI-owned operation; a competing reservation is busy, never stolen.
    pub fn pair_phone_owned(
        &self,
        operation: uuid::Uuid,
        observer: Arc<dyn PairingObserver>,
    ) -> Result<PhoneOffer, String> {
        self.pair_phone_scoped(operation, observer, None)
    }

    fn pair_phone_scoped(
        &self,
        operation: uuid::Uuid,
        observer: Arc<dyn PairingObserver>,
        enrollment: Option<EnrollmentParameters>,
    ) -> Result<PhoneOffer, String> {
        let s = self.pairing_state()?;
        self.hurry_connection();
        let secret = PairSecret::generate().map_err(|e| e.to_string())?;
        let head = s.chain.head();
        let ttl = enrollment
            .as_ref()
            .map(|p| Duration::from_secs(p.expires_at.saturating_sub(now())).min(self.cfg.pair_ttl))
            .unwrap_or(self.cfg.pair_ttl);
        // A v2 (membership) head pairs through the v2 adoption transport: the offer then
        // carries the head anchor and only a v2-capable phone accepts it.
        let v2 = head.roster().v == 2;
        let offer = PairingOffer {
            ring_id: head.ring_id().clone(),
            relay_url: head.roster().relay_url.clone(),
            sign_key: s.keys.sign_key(),
            noise_key: s.keys.noise_key(),
            secret: secret.clone(),
            expires_at: now() + ttl.as_secs(),
            head: v2.then(|| OfferHead {
                version: head.version(),
                hash: head.hash(),
            }),
        };
        let mut until = Instant::now() + ttl;
        if enrollment.is_some() {
            let remaining = (std::time::UNIX_EPOCH + Duration::from_secs(offer.expires_at))
                .duration_since(std::time::SystemTime::now())
                .unwrap_or_default();
            until = until.min(Instant::now() + remaining);
        }
        let cancel =
            self.start_pairing_scoped(PairingFlow::Phone, operation, enrollment.is_some())?;
        if let Some(parameters) = &enrollment {
            if parameters.ring != *head.ring_id()
                || parameters.approver.signing != s.keys.sign_key()
                || parameters.approver.noise != s.keys.noise_key()
                || parameters.relay != head.roster().relay_url
                || parameters.initial_head.hash != head.hash()
                || parameters.initial_head.version != head.version()
            {
                self.end_pairing(PairingFlow::Phone, &cancel);
                return Err("scope-mismatch".into());
            }
        }
        let opts = self.pair_options();
        let pipe = PairPipe::open(&offer.relay_url, &secret.slot(), &opts).map_err(|e| {
            self.end_pairing(PairingFlow::Phone, &cancel);
            format!("{}: {e}", e.as_code())
        })?;
        // Acquisition consumes the original lifetime; never restart the clock after dial.
        if Instant::now() >= until || cancel.load(Ordering::Acquire) {
            pipe.close();
            self.end_pairing(PairingFlow::Phone, &cancel);
            return Err(if Instant::now() >= until {
                "expired"
            } else {
                "cancelled"
            }
            .into());
        }
        let me = self.me.clone();
        let c = cancel.clone();
        let ring_id = offer.ring_id.clone();
        let offer_expires_at = offer.expires_at;
        let spawned = std::thread::Builder::new()
            .name("ring-pair-phone".into())
            .spawn(move || {
                let mut pipe = pipe;
                observer.pairing(PairingFlow::Phone, &PairingEvent::Waiting);
                let r = pipe.wait_peer(until, Some(&c)).and_then(|()| {
                    let ring = me.upgrade().ok_or(PairError::Cancelled)?;
                    if v2 {
                        return ring.host_mobile_v2(
                            pipe,
                            &s.keys,
                            &secret,
                            &ring_id,
                            until,
                            offer_expires_at,
                            &c,
                            &s.chain.head().hash(),
                            enrollment.as_ref().map(|p| (operation, p)),
                        );
                    }
                    ring.host(
                        pipe,
                        &s.keys,
                        &secret,
                        &ring_id,
                        Role::Mobile,
                        until,
                        &c,
                        &s.chain.head().hash(),
                        enrollment.as_ref().map(|p| (operation, p)),
                    )
                });
                if let Some(ring) = me.upgrade() {
                    ring.end_pairing(PairingFlow::Phone, &c);
                }
                observer.pairing(PairingFlow::Phone, &event_of(r));
            });
        if let Err(e) = spawned {
            self.end_pairing(PairingFlow::Phone, &cancel);
            return Err(e.to_string());
        }
        Ok(PhoneOffer {
            payload: offer.encode(),
            expires_at: offer.expires_at,
        })
    }

    /// Adds the computer whose `xshelld pair` shows `code`. Returns once the code parses;
    /// the rest happens on a thread of its own, reported to `observer`.
    pub fn pair_computer(
        &self,
        code: &str,
        observer: Arc<dyn PairingObserver>,
    ) -> Result<(), String> {
        PairCode::parse(code).map_err(|e| format!("{}: {e}", e.as_code()))?;
        self.pairing_state()?;
        self.cancel_pairing(PairingFlow::Computer);
        self.pair_computer_owned(uuid::Uuid::new_v4(), code, observer)
    }

    pub fn pair_computer_owned(
        &self,
        operation: uuid::Uuid,
        code: &str,
        observer: Arc<dyn PairingObserver>,
    ) -> Result<(), String> {
        let code = PairCode::parse(code).map_err(|e| format!("{}: {e}", e.as_code()))?;
        let s = self.pairing_state()?;
        self.hurry_connection();
        let secret = code.secret();
        let cancel = self.start_pairing(PairingFlow::Computer, operation)?;
        let opts = self.pair_options();
        let until = Instant::now() + self.cfg.pair_ttl;
        let me = self.me.clone();
        let c = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("ring-pair-computer".into())
            .spawn(move || {
                observer.pairing(PairingFlow::Computer, &PairingEvent::Waiting);
                let head = s.chain.head();
                let r = PairPipe::open(&head.roster().relay_url, &secret.slot(), &opts).and_then(
                    |pipe| {
                        // `xshelld pair` waits on its slot: a code nobody waits on is wrong
                        // (or that computer gave up).
                        if !pipe.has_peer() {
                            pipe.close();
                            return Err(PairError::NotFound);
                        }
                        let ring = me.upgrade().ok_or(PairError::Cancelled)?;
                        ring.host(
                            pipe,
                            &s.keys,
                            &secret,
                            head.ring_id(),
                            Role::Daemon,
                            until,
                            &c,
                            &head.hash(),
                            None,
                        )
                    },
                );
                if let Some(ring) = me.upgrade() {
                    ring.end_pairing(PairingFlow::Computer, &c);
                }
                observer.pairing(PairingFlow::Computer, &event_of(r));
            });
        if let Err(e) = spawned {
            self.end_pairing(PairingFlow::Computer, &cancel);
            return Err(e.to_string());
        }
        Ok(())
    }

    /// Stops the pairing of `flow` in progress, if any (its observer hears `cancelled`).
    pub fn cancel_pairing(&self, flow: PairingFlow) {
        let _commit = self.lock_commit();
        let mut l = self.lock();
        let external = l
            .enrollment
            .as_ref()
            .filter(|e| e.reserved)
            .and_then(|e| e.response.progress.as_ref().map(|p| p.operation.uuid()));
        for (f, operation, c) in &l.pairing {
            if *f == flow && Some(*operation) != external {
                c.store(true, Ordering::Release);
            }
        }
        l.pairing
            .retain(|(f, operation, _)| *f != flow || Some(*operation) == external);
    }

    /// The state a pairing needs: a Ring, and this instance running its connection (it
    /// publishes the new version).
    fn pairing_state(&self) -> Result<RingState, String> {
        let l = self.lock();
        let s = match (&l.state, &l.problem) {
            (Some(s), None) => s.clone(),
            _ => return Err(PAIR_NOT_ENABLED.into()),
        };
        if l.connector.is_none() {
            return Err(PAIR_OTHER_WINDOW.into());
        }
        Ok(s)
    }

    /// A pairing starts: a Connector waiting out its backoff dials now, as the pairing
    /// needs the connection to publish the new version.
    fn hurry_connection(&self) {
        let (connector, link) = {
            let l = self.lock();
            (l.connector.clone(), l.link.clone())
        };
        if !matches!(link, Some(LinkState::Connected { .. })) {
            if let Some(c) = connector {
                c.kick();
            }
        }
    }

    /// Reserves one operation; only legacy UI convenience methods explicitly replace.
    fn start_pairing(&self, flow: PairingFlow, operation: uuid::Uuid) -> Result<Cancel, String> {
        self.start_pairing_scoped(flow, operation, false)
    }
    fn start_pairing_scoped(
        &self,
        flow: PairingFlow,
        operation: uuid::Uuid,
        external: bool,
    ) -> Result<Cancel, String> {
        let mut state = self.lock();
        state
            .pairing_operations
            .retain(|(_, until)| Instant::now() < *until);
        if state.quitting
            || operation.is_nil()
            || state
                .pairing_operations
                .iter()
                .any(|(op, _)| *op == operation)
            || state.pairing_operations.len() >= xshell_protocol::pairing_access::CACHE_MAX
            || state.pairing.iter().any(|(f, _, _)| *f == flow)
        {
            return Err("busy".into());
        }
        let c: Cancel = Arc::new(AtomicBool::new(false));
        state
            .pairing_operations
            .push((operation, Instant::now() + Duration::from_secs(600)));
        state.pairing.push((flow, operation, c.clone()));
        if external {
            if let Some(enrollment) = &mut state.enrollment {
                if enrollment
                    .response
                    .progress
                    .as_ref()
                    .is_some_and(|p| p.operation.uuid() == operation)
                {
                    enrollment.reserved = true;
                }
            }
        }
        Ok(c)
    }

    pub fn cancel_pairing_owned(&self, flow: PairingFlow, operation: uuid::Uuid) {
        let _commit = self.lock_commit();
        let mut state = self.lock();
        if state.enrollment.as_ref().is_some_and(|e| {
            e.reserved
                && e.response
                    .progress
                    .as_ref()
                    .is_some_and(|p| p.operation.uuid() == operation)
        }) {
            return;
        }
        state
            .pairing_operations
            .retain(|(_, until)| Instant::now() < *until);
        if state.pairing_operations.len() < xshell_protocol::pairing_access::CACHE_MAX
            && !state
                .pairing_operations
                .iter()
                .any(|(op, _)| *op == operation)
        {
            state
                .pairing_operations
                .push((operation, Instant::now() + Duration::from_secs(600)));
        }
        for (f, op, cancel) in &state.pairing {
            if *f == flow && *op == operation {
                cancel.store(true, Ordering::Release);
            }
        }
        state
            .pairing
            .retain(|(f, op, _)| !(*f == flow && *op == operation));
    }

    fn end_pairing(&self, flow: PairingFlow, c: &Cancel) {
        self.lock()
            .pairing
            .retain(|(f, _, x)| !(*f == flow && Arc::ptr_eq(x, c)));
    }

    fn pair_options(&self) -> PairOptions {
        PairOptions {
            step: self.cfg.timeouts.connect,
            ring: self.cfg.timeouts,
            ..PairOptions::default()
        }
    }

    /// The Desktop's side of one pairing over `pipe` (which has the peer).
    #[allow(clippy::too_many_arguments)]
    fn host(
        &self,
        pipe: PairPipe,
        keys: &DeviceKeys,
        secret: &PairSecret,
        ring_id: &RingId,
        role: Role,
        until: Instant,
        cancel: &Cancel,
        initial_head: &str,
        enrollment: Option<(uuid::Uuid, &EnrollmentParameters)>,
    ) -> Result<JoinRequest, PairError> {
        let adder = Adder {
            ring: self,
            used: AtomicBool::new(false),
            until,
            cancel: cancel.clone(),
            initial_head,
            enrollment,
        };
        host_pairing(
            pipe,
            &HostRequest {
                keys,
                secret,
                ring_id,
                name: &self.cfg.name,
                role,
            },
            &adder,
            Some(cancel),
        )
    }

    /// The Desktop's side of a v2 phone pairing (#87): the phone's possession-proven
    /// `Mobile` intent becomes an unsigned `Enroll` proposal frozen at the offer's head on the
    /// Connector's creator lane, the phone signs its own Pairing consent, and this Desktop
    /// signs, publishes once and installs the Relay-accepted chain. The checks of
    /// [`Self::add_paired_member_scoped`] apply ([`v2_phone_admission`]). An exact member
    /// retrying with retained keys gets an authenticated "already enrolled" completion (no
    /// new version); every refusal reaches the phone as a typed answer.
    #[allow(clippy::too_many_arguments)]
    fn host_mobile_v2(
        &self,
        pipe: PairPipe,
        keys: &Arc<DeviceKeys>,
        secret: &PairSecret,
        ring_id: &RingId,
        until: Instant,
        offer_expires_at: u64,
        cancel: &Cancel,
        initial_head: &str,
        enrollment: Option<(uuid::Uuid, &EnrollmentParameters)>,
    ) -> Result<JoinRequest, PairError> {
        use xshell_protocol::ring::pairing::adoption::{enroll_proposal, JoinTarget};
        use xshell_protocol::ring::relay::connector::{ProposalConnection, PublishFailure};
        use xshell_protocol::ring::relay::pair::{
            approve_mobile_adoption, MobileHostRequest, Prepared,
        };
        use xshell_protocol::ring::relay::VerifiedProposal;
        let refused = |r: PairRefusal| PairError::Refused(r);
        let lane = Mutex::new(None::<Arc<ProposalConnection>>);
        let joining = Mutex::new(None::<JoinRequest>);
        let result = approve_mobile_adoption(
            pipe,
            &MobileHostRequest {
                keys,
                secret,
                ring_id,
                name: &self.cfg.name,
                until,
                offer_expires_at,
            },
            Some(cancel),
            |intent, hash| {
                let JoinTarget::Mobile { phone } = &intent.target else {
                    return Err(refused(PairRefusal::Role));
                };
                let req = JoinRequest {
                    role: Role::Mobile,
                    sign_key: phone.sign_key,
                    noise_key: phone.noise_key,
                    name: phone.name.clone(),
                };
                *joining.lock().unwrap_or_else(|e| e.into_inner()) = Some(req.clone());
                let live = |stage: &dyn Fn() -> bool| -> Result<(), PairError> {
                    if cancel.load(Ordering::Acquire) || self.lock().quitting {
                        return Err(refused(PairRefusal::Cancelled));
                    }
                    if Instant::now() >= until || now() >= offer_expires_at || stage() {
                        return Err(refused(PairRefusal::Expired));
                    }
                    Ok(())
                };
                let scope_expired = || enrollment.is_some_and(|(_, p)| now() >= p.expires_at);
                live(&scope_expired)?;
                let state = self
                    .tx(|cur, _| Ok((None, cur)))
                    .ok()
                    .flatten()
                    .ok_or(refused(PairRefusal::NotCommitted))?;
                match v2_phone_admission(
                    state.chain.head(),
                    keys,
                    &req,
                    initial_head,
                    enrollment.map(|(_, p)| p),
                )
                .map_err(refused)?
                {
                    PhoneAdmission::Enrolled(joined) => {
                        if let Some((operation, _)) = enrollment {
                            let head = ChainHead {
                                version: joined.version,
                                hash: joined.hash.clone(),
                            };
                            self.note_enrollment(
                                operation,
                                EnrollmentStage::DesktopCommitted,
                                EnrollmentEvidence::DesktopCommitted { head: head.clone() },
                            );
                            self.note_enrollment(
                                operation,
                                EnrollmentStage::PublicationAcknowledged,
                                EnrollmentEvidence::PublicationAcknowledged { head },
                            );
                        }
                        return Ok(Prepared::Enrolled(joined));
                    }
                    PhoneAdmission::Enroll => {}
                }
                // The creator lane must sit exactly at the offer's head: a head that moved
                // is stale; no lane in time means nothing was (or will be) committed.
                self.hurry_connection();
                let ready_until = until.min(Instant::now() + self.cfg.pair_publish_wait);
                let (chain, captured) = loop {
                    live(&scope_expired)?;
                    if let Ok((chain, captured)) = self.proposal_snapshot() {
                        if chain.head().hash() == initial_head {
                            break (chain, captured);
                        }
                        // A Connector still catching up to the accepted head is waited
                        // for; any other head is not the offer's.
                        if chain.head().version() >= state.chain.head().version() {
                            return Err(refused(PairRefusal::Stale));
                        }
                    }
                    if Instant::now() >= ready_until {
                        return Err(refused(PairRefusal::NotCommitted));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let proposal = enroll_proposal(chain.head(), keys.sign_key(), intent, now())?;
                let captured = Arc::new(captured);
                *lane.lock().unwrap_or_else(|e| e.into_inner()) = Some(captured.clone());
                let handle = captured
                    .create(intent, hash, &proposal)
                    .map_err(|_| refused(PairRefusal::NotCommitted))?;
                Ok(Prepared::Proposal(Box::new(VerifiedProposal {
                    chain,
                    proposal,
                    handle,
                })))
            },
            |signed| {
                let captured = lane
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or(refused(PairRefusal::NotCommitted))?;
                // The shared v2 membership-mutation reservation spans the final predecessor
                // check, the send and the accepted installation. A removal or adoption in
                // flight first: wait (within the offer) or give up with nothing sent.
                self.adoption_test_point(AdoptionTestPoint::PhoneBeforeReservation);
                let _reservation = self
                    .reserve_membership_mutation(
                        until.min(Instant::now() + self.cfg.pair_publish_wait),
                    )
                    .ok_or(refused(PairRefusal::NotCommitted))?;
                if cancel.load(Ordering::Acquire) || self.lock().quitting {
                    return Err(refused(PairRefusal::Cancelled));
                }
                if Instant::now() >= until
                    || now() >= offer_expires_at
                    || enrollment.is_some_and(|(_, p)| now() >= p.expires_at)
                {
                    return Err(refused(PairRefusal::Expired));
                }
                let pre_send = |_| {
                    let moved = self
                        .adoption_owner_connector()
                        .is_some_and(|c| c.chain().head().hash() != initial_head);
                    refused(if moved {
                        PairRefusal::Stale
                    } else {
                        PairRefusal::NotCommitted
                    })
                };
                captured.check_current().map_err(pre_send)?;
                let operation = enrollment.map(|(operation, _)| operation);
                self.begin_phone_publication(cancel, operation)
                    .map_err(refused)?;
                self.adoption_test_point(AdoptionTestPoint::PhoneBeforePublish);
                self.publish_wait_hook();
                match captured.publish_once_classified(signed) {
                    Ok(()) => {}
                    Err(PublishFailure::NotSent(e)) => {
                        // Proven pre-send termination: nothing can be at the Relay.
                        self.end_unsent_phone_publication(operation);
                        return Err(pre_send(e));
                    }
                    // From here the candidate may be at the Relay: failures are uncertain.
                    Err(PublishFailure::MaybeSent(_)) => {
                        return Err(refused(PairRefusal::PublicationUncertain))
                    }
                }
                self.adoption_test_point(AdoptionTestPoint::PhonePublishedBeforeReadBack);
                let accepted = captured
                    .read_accepted(signed)
                    .map_err(|_| refused(PairRefusal::PublicationUncertain))?;
                self.install_accepted(&accepted)
                    .map_err(|_| refused(PairRefusal::PublicationUncertain))?;
                if let Some((operation, _)) = enrollment {
                    let head = ChainHead {
                        version: signed.version(),
                        hash: signed.hash(),
                    };
                    self.note_enrollment(
                        operation,
                        EnrollmentStage::DesktopCommitted,
                        EnrollmentEvidence::DesktopCommitted { head: head.clone() },
                    );
                    self.note_enrollment(
                        operation,
                        EnrollmentStage::PublicationAcknowledged,
                        EnrollmentEvidence::PublicationAcknowledged { head },
                    );
                }
                Ok(Joined {
                    ring_id: signed.ring_id().clone(),
                    relay_url: signed.roster().relay_url.clone(),
                    version: signed.version(),
                    hash: signed.hash(),
                    signed_by: signed.roster().signed_by,
                })
            },
            |handle| {
                if let Some(lane) = lane.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    let _ = lane.cancel(handle);
                }
            },
        );
        result?;
        joining
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .ok_or(PairError::Cancelled)
    }

    /// The send boundary of a v2 phone candidate. Atomically with `enrollment_status`
    /// cancellation (commit lock, then Live lock): a cancellation that came first wins
    /// (`Cancelled`, nothing sent); otherwise the scoped receipt can no longer become
    /// `NoDesktopCommit`.
    fn begin_phone_publication(
        &self,
        cancel: &Cancel,
        operation: Option<uuid::Uuid>,
    ) -> Result<(), PairRefusal> {
        let _commit = self.lock_commit();
        let mut live = self.lock();
        if cancel.load(Ordering::Acquire) || live.quitting {
            return Err(PairRefusal::Cancelled);
        }
        if let Some(operation) = operation {
            let Some(enrollment) = live.enrollment.as_mut().filter(|e| {
                e.response
                    .progress
                    .as_ref()
                    .is_some_and(|p| p.operation.uuid() == operation)
            }) else {
                return Err(PairRefusal::Cancelled);
            };
            if enrollment.released {
                return Err(PairRefusal::Cancelled);
            }
            enrollment.publishing = true;
        }
        Ok(())
    }

    /// The candidate provably never left: cancellation may again prove no commit.
    fn end_unsent_phone_publication(&self, operation: Option<uuid::Uuid>) {
        let Some(operation) = operation else { return };
        let _commit = self.lock_commit();
        let mut live = self.lock();
        if let Some(enrollment) = live.enrollment.as_mut().filter(|e| {
            e.response
                .progress
                .as_ref()
                .is_some_and(|p| p.operation.uuid() == operation)
        }) {
            enrollment.publishing = false;
        }
    }

    /// Installs a Relay-accepted extension of the held chain (committed, adopted, shown).
    fn install_accepted(&self, accepted: &RosterChain) -> Result<(), String> {
        {
            let _c = self.lock_commit();
            let s = self.tx(|cur, _| {
                let mut s = cur.ok_or("no Ring")?;
                s.chain = s
                    .chain
                    .extended(accepted.versions())
                    .map_err(|e| e.to_string())?
                    .0;
                s.retarget_farewells();
                Ok((Some(s.clone()), s))
            })?;
            self.hook();
            self.adopt(s);
        }
        self.emit();
        Ok(())
    }

    /// Adds a paired device: a new Roster version, committed, adopted and published, and
    /// acknowledged by the Relay. The same keys with the same role again are a member
    /// already, not an error. Waits for the Relay to hold the version (for the connection
    /// to come up, say) up to `pair_publish_wait` and never past `until`; `cancel` (the
    /// pairing cancelled, the app quitting) ends the wait.
    #[cfg(test)]
    fn add_paired_member(
        &self,
        req: &JoinRequest,
        until: Instant,
        cancel: &Cancel,
        initial_head: &str,
    ) -> Result<Joined, PairRefusal> {
        self.add_paired_member_scoped(req, until, cancel, initial_head, None)
    }
    fn add_paired_member_scoped(
        &self,
        req: &JoinRequest,
        until: Instant,
        cancel: &Cancel,
        initial_head: &str,
        enrollment: Option<(uuid::Uuid, &EnrollmentParameters)>,
    ) -> Result<Joined, PairRefusal> {
        let (s, next) = {
            let _c = self.lock_commit();
            let r = self.tx(|cur, _| {
                let Some(mut s) = cur else {
                    return Ok((None, Err(PairRefusal::PublishFailed)));
                };
                if cancel.load(Ordering::Acquire) {
                    return Ok((None, Err(PairRefusal::Used)));
                }
                if Instant::now() >= until {
                    return Ok((None, Err(PairRefusal::Expired)));
                }
                if let Some((_, p)) = enrollment {
                    if now() >= p.expires_at {
                        return Ok((None, Err(PairRefusal::Expired)));
                    }
                    if req.sign_key != p.recipient.signing
                        || req.noise_key != p.recipient.noise
                        || req.role != Role::Mobile
                        || req.name != p.device_name
                        || s.keys.sign_key() != p.approver.signing
                        || s.keys.noise_key() != p.approver.noise
                        || s.chain.ring_id() != &p.ring
                        || s.chain.head().roster().relay_url != p.relay
                    {
                        return Ok((None, Err(PairRefusal::Duplicate)));
                    }
                }
                let head = s.chain.head();
                if head.hash() != initial_head {
                    return Ok((None, Err(PairRefusal::PublishFailed)));
                }
                // `next()` is v1 only: a v2 head is never extended here (not even by the
                // exact-key retry below); phones pair through `host_mobile_v2`.
                if head.roster().v != 1 {
                    return Ok((None, Err(PairRefusal::PublishFailed)));
                }
                if let Some(m) = head.member(&req.sign_key) {
                    if m.noise_key != req.noise_key || m.role != req.role {
                        return Ok((None, Err(PairRefusal::Duplicate)));
                    }
                    if head.roster().signed_by == s.keys.sign_key() {
                        return Ok((None, Ok((s, None))));
                    }
                    // A fresh offer pins this approving Desktop, not the signer of an
                    // intervening update. Preserve every member on an exact-key retry,
                    // but return an authenticated successor signed by the approver.
                    let next = head
                        .next(&*s.keys, now(), |_| {})
                        .map_err(|e| e.to_string())?;
                    s.chain
                        .accept(std::slice::from_ref(&next))
                        .map_err(|e| e.to_string())?;
                    s.retarget_farewells();
                    return Ok((Some(s.clone()), Ok((s, Some(next)))));
                }
                if head
                    .roster()
                    .members
                    .iter()
                    .any(|m| m.noise_key == req.noise_key)
                {
                    return Ok((None, Err(PairRefusal::Duplicate)));
                }
                if head.roster().members.len() >= MAX_MEMBERS {
                    return Ok((None, Err(PairRefusal::Full)));
                }
                let t = now();
                let next = head
                    .next(&*s.keys, t, |d| {
                        d.add(Member::new(
                            &req.name,
                            req.role,
                            req.sign_key,
                            req.noise_key,
                            t,
                        ))
                    })
                    .map_err(|e| e.to_string())?;
                s.chain
                    .accept(std::slice::from_ref(&next))
                    .map_err(|e| e.to_string())?;
                s.retarget_farewells();
                Ok((Some(s.clone()), Ok((s, Some(next)))))
            });
            let (s, next) = match r {
                Ok(Ok(x)) => x,
                Ok(Err(refusal)) => return Err(refusal),
                Err(e) => {
                    eprintln!("xshell: cannot add the paired device: {e}");
                    return Err(PairRefusal::PublishFailed);
                }
            };
            if let Some((operation, _)) = enrollment {
                self.note_enrollment(
                    operation,
                    EnrollmentStage::DesktopCommitted,
                    EnrollmentEvidence::DesktopCommitted {
                        head: ChainHead {
                            version: s.chain.head().version(),
                            hash: s.chain.head().hash(),
                        },
                    },
                );
            }
            if next.is_some() {
                self.hook();
                self.adopt(s.clone());
            }
            (s, next)
        };
        self.emit();
        let head = next.unwrap_or_else(|| s.chain.head().clone());
        let connector = self.lock().connector.clone();
        let deadline = until.min(Instant::now() + self.cfg.pair_publish_wait);
        self.publish_wait_hook();
        // No Ring lock is held while waiting.
        match connector.map(|c| c.publish_until(&head, deadline, Some(cancel))) {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                eprintln!("xshell: cannot publish the new roster: {e}");
                return Err(PairRefusal::PublishFailed);
            }
            None => return Err(PairRefusal::PublishFailed),
        }
        if let Some((operation, _)) = enrollment {
            self.note_enrollment(
                operation,
                EnrollmentStage::PublicationAcknowledged,
                EnrollmentEvidence::PublicationAcknowledged {
                    head: ChainHead {
                        version: head.version(),
                        hash: head.hash(),
                    },
                },
            );
        }
        Ok(Joined {
            ring_id: head.ring_id().clone(),
            relay_url: head.roster().relay_url.clone(),
            version: head.version(),
            hash: head.hash(),
            signed_by: head.roster().signed_by,
        })
    }

    /// Removes `key` from the Ring: a new Roster version without it, committed, adopted and
    /// published through the Connector (the Relay then cuts the member and refuses it). Like
    /// pairing, it needs this instance to run the connection. Refused for this app
    /// ([`REMOVE_SELF`]), this computer's Daemon and the Daemon of a Host still configured
    /// ([`REMOVE_IN_USE`]: the Host worker would add it back). Every Host entry mapped to the
    /// key goes with it. A key the head does not list is a no-op. Every farewell owed grows to
    /// the new version, so the old Relays learn of the removal too. Returns the chain, for the
    /// Daemons.
    pub fn remove_member(&self, key: &SignKey) -> Result<RosterChain, String> {
        let v2 = self
            .lock()
            .state
            .as_ref()
            .is_some_and(|s| s.chain.head().roster().v == 2);
        if v2 {
            return self.remove_member_v2(key);
        }
        // The configured-Hosts answers, taken before any Ring lock (see the module docs). A
        // Host id mapped to the key that was not asked about counts as configured.
        let ids: Vec<String> = self
            .lock()
            .state
            .as_ref()
            .map(|s| {
                s.host_members
                    .iter()
                    .filter(|(_, h)| h.sign_key == *key)
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let configured = self.configured();
        let unconfigured: BTreeSet<String> = ids.into_iter().filter(|id| !configured(id)).collect();
        let chain = {
            let _c = self.lock_commit();
            self.pairing_state()?;
            let (s, changed) = self.tx(|cur, _| {
                let Some(mut s) = cur else {
                    return Err(PAIR_NOT_ENABLED.to_string());
                };
                if s.chain.head().roster().v == 2 {
                    // Upgraded meanwhile: v2 removal is never committed before the Relay.
                    return Err(REMOVE_CONFLICT.to_string());
                }
                if *key == s.keys.sign_key() {
                    return Err(REMOVE_SELF.to_string());
                }
                if s.chain.head().member(key).is_none() {
                    return Ok((None, (s, false)));
                }
                let in_use = s.local_member == Some(*key)
                    || s.host_members
                        .iter()
                        .any(|(id, h)| h.sign_key == *key && !unconfigured.contains(id));
                if in_use {
                    return Err(REMOVE_IN_USE.to_string());
                }
                let next = s
                    .chain
                    .head()
                    .next(&*s.keys, now(), |d| {
                        d.remove(key);
                    })
                    .map_err(|e| e.to_string())?;
                s.chain
                    .accept(std::slice::from_ref(&next))
                    .map_err(|e| e.to_string())?;
                s.host_members.retain(|_, h| h.sign_key != *key);
                // The devices still on an old Relay must learn of the removal too. A farewell
                // acknowledged for an older head leaves the grown one standing.
                s.retarget_farewells();
                Ok((Some(s.clone()), (s, true)))
            })?;
            if !changed {
                return Ok(s.chain);
            }
            self.hook();
            self.adopt(s.clone());
            s.chain
        };
        self.emit();
        Ok(chain)
    }

    /// [`DesktopRing::remove_member`] on a format-v2 Ring. The key's whole enrollment unit
    /// goes in one typed Remove, and nothing is accepted here before the Relay has it:
    /// - the candidate is built under the commit lock on the effective head (the Connector's
    ///   newer head and same-ancestry terminal evidence included), so a terminal-expired
    ///   signer or this app's own unit ([`REMOVE_SELF`]) is refused before anything is sent;
    /// - it needs a connected Relay ([`REMOVE_OFFLINE`]); it is published with no Ring lock
    ///   held (the Connector's Roster report takes the commit lock), fenced again at each
    ///   upload by the owner's current terminal evidence ([`Connector::publish_fenced_until`]);
    /// - on the Relay's acknowledgment, from the same Connector and still authorized under
    ///   the current evidence, it is committed and the unit's Host mappings are dropped
    ///   (Settings → Hosts entries stay);
    /// - a conflicting head is read again and a fresh candidate (a new signature on the new
    ///   head, never an old one moved) is tried once, then [`REMOVE_CONFLICT`]; no answer
    ///   is [`REMOVE_UNCERTAIN`]; another refusal is reported. None of them is persisted.
    ///
    /// A key the effective head does not list is a no-op.
    fn remove_member_v2(&self, key: &SignKey) -> Result<RosterChain, String> {
        // One v2 membership mutation at a time, from the candidate's predecessor through
        // the accepted commit (a phone enrollment or an adoption controller in flight first
        // finishes, or this is busy).
        let _reservation = self
            .reserve_membership_mutation(Instant::now() + self.cfg.pair_publish_wait)
            .ok_or(MEMBERSHIP_BUSY)?;
        let mut retried = false;
        loop {
            let (candidate, unit_keys, effective, connector, epoch) = {
                let _c = self.lock_commit();
                let s = self.pairing_state()?;
                let effective = self.adoption_effective_chain()?.ok_or(PAIR_NOT_ENABLED)?;
                let Some((candidate, unit_keys)) =
                    removal_candidate(&effective, &s.keys, key, now())?
                else {
                    return Ok(effective);
                };
                let (connector, link, epoch) = {
                    let l = self.lock();
                    (l.connector.clone(), l.link.clone(), l.epoch)
                };
                let connector = connector.ok_or(PAIR_OTHER_WINDOW)?;
                if !matches!(link, Some(LinkState::Connected { .. })) {
                    return Err(REMOVE_OFFLINE.to_string());
                }
                (candidate, unit_keys, effective, connector, epoch)
            };
            self.adoption_test_point(AdoptionTestPoint::RemoveBeforePublish);
            let deadline = Instant::now() + self.cfg.pair_publish_wait;
            match connector.publish_fenced_until(&candidate, deadline, None) {
                Ok(()) => {
                    return self
                        .commit_removal(&effective, &candidate, &unit_keys, &connector, epoch)
                }
                Err(e) if removal_conflict(&e) => {
                    if retried {
                        return Err(REMOVE_CONFLICT.to_string());
                    }
                    retried = true;
                    // Read again once the Connector holds the head that won.
                    let head = effective.head().hash();
                    while connector
                        .try_chain()
                        .is_none_or(|c| c.head().hash() == head)
                        && Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(e @ (RingError::Relay { .. } | RingError::Roster(_))) => {
                    return Err(e.to_string())
                }
                Err(_) => return Err(REMOVE_UNCERTAIN.to_string()),
            }
        }
    }

    /// The Relay acknowledged `candidate`: committed (with the effective versions before it),
    /// unless the owner changed meanwhile ([`REMOVE_UNCERTAIN`]: the new owner learns the
    /// Relay's head on its own).
    fn commit_removal(
        &self,
        effective: &RosterChain,
        candidate: &SignedRoster,
        unit_keys: &[SignKey],
        connector: &Arc<Connector>,
        epoch: u64,
    ) -> Result<RosterChain, String> {
        let chain = {
            let _c = self.lock_commit();
            {
                let l = self.lock();
                if l.epoch != epoch
                    || !l
                        .connector
                        .as_ref()
                        .is_some_and(|c| Arc::ptr_eq(c, connector))
                {
                    return Err(REMOVE_UNCERTAIN.to_string());
                }
            }
            // The owner's current evidence, not the one the candidate was built under.
            let current = self.adoption_effective_chain()?.ok_or(PAIR_NOT_ENABLED)?;
            still_authorized(&current, candidate)?;
            let s = self.tx(|cur, _| {
                let s = cur.ok_or(PAIR_NOT_ENABLED)?;
                let s = commit_acknowledged(s, effective, candidate, unit_keys)?;
                Ok((Some(s.clone()), s))
            })?;
            self.hook();
            self.adopt(s.clone());
            s.chain
        };
        self.emit();
        Ok(chain)
    }

    /// The chain held, if a Ring exists.
    pub fn chain(&self) -> Option<RosterChain> {
        self.lock().state.as_ref().map(|s| s.chain.clone())
    }

    pub fn view(&self) -> RingView {
        let (state, connector, link, farewells, problem, other, quota_until) = {
            let l = self.lock();
            (
                l.state.clone(),
                l.connector.clone(),
                l.link.clone(),
                l.farewells.clone(),
                l.problem.clone(),
                l.other_window && l.connector.is_none(),
                l.quota.until,
            )
        };
        let (problem, problem_detail) = match problem {
            Some((p, d)) => (Some(p), Some(d)),
            None => (None, None),
        };
        let mut v = RingView {
            enabled: state.is_some(),
            ring_id: None,
            version: None,
            relay_url: None,
            hosted_relay_url: self.cfg.hosted_relay_url.clone(),
            connection: "off",
            retry_in: None,
            connection_error: None,
            limited: false,
            farewells: Vec::new(),
            problem,
            problem_detail,
            members: Vec::new(),
            quota_reset_at: None,
            relay_hosted: false,
        };
        // Recovery required (or the settings refused): nothing else counts until then.
        if v.problem.is_some() {
            v.enabled = false;
            return v;
        }
        let Some(s) = state else {
            return v;
        };
        let head = s.chain.head();
        v.ring_id = Some(head.ring_id().clone());
        v.version = Some(head.version());
        v.relay_url = Some(head.roster().relay_url.clone());
        match link.unwrap_or(LinkState::Connecting { attempt: 0 }) {
            _ if other => v.connection = "other-window",
            LinkState::Connecting { .. } => v.connection = "connecting",
            LinkState::Connected { limited } => {
                v.connection = "connected";
                v.limited = limited;
            }
            LinkState::Waiting { retry_in, error } => {
                v.connection = "waiting";
                v.retry_in = Some(retry_in.as_secs_f64().ceil() as u64);
                v.connection_error = Some(error);
            }
            LinkState::Stopped { error } => {
                v.connection = "stopped";
                v.connection_error = error;
            }
        }
        let origin = |u: &str| normalize_origin(u).ok();
        v.relay_hosted = v.limited
            || origin(&head.roster().relay_url)
                .is_some_and(|o| Some(o) == origin(&self.cfg.hosted_relay_url));
        if !other {
            v.quota_reset_at = quota_notice(quota_until, now());
        }
        v.farewells = s
            .farewells
            .iter()
            .map(|f| {
                let mut fv = FarewellView {
                    relay_url: f.source.clone(),
                    state: "pending",
                    error: None,
                    retry_in: None,
                    failures: 0,
                };
                match farewells.get(&f.source) {
                    _ if other => {}
                    Some(FarewellState::Sending { .. }) => fv.state = "sending",
                    Some(FarewellState::Waiting {
                        failures,
                        error,
                        retry_in,
                        ..
                    }) => {
                        fv.state = "waiting";
                        fv.error = Some(error.clone());
                        fv.retry_in = Some(retry_in.as_secs_f64().ceil() as u64);
                        fv.failures = *failures;
                    }
                    Some(FarewellState::Done { .. }) | None => {}
                }
                fv
            })
            .collect();
        let me = s.keys.sign_key();
        // Asked here, on a copy of the state, with no Ring lock held.
        let configured = self.configured();
        let configured: BTreeSet<&str> = s
            .host_members
            .keys()
            .map(String::as_str)
            .filter(|id| configured(id))
            .collect();
        let live = if v.connection == "connected" {
            connector.and_then(|c| c.members())
        } else {
            None
        };
        let presence_of = |key: &SignKey| match &live {
            _ if *key == me && v.connection == "connected" => {
                PresenceView::of(&MemberPresence::Online {
                    since: None,
                    foreground_until: None,
                })
            }
            Some(list) => list
                .iter()
                .find(|x| x.member.sign_key == *key)
                .map(|x| PresenceView::of(&x.presence))
                .unwrap_or_else(PresenceView::unknown),
            None => PresenceView::unknown(),
        };
        // `host_members` iterates in id order: the first is the smallest.
        let host_of = |key: &SignKey| {
            s.host_members
                .iter()
                .find(|(id, h)| h.sign_key == *key && configured.contains(id.as_str()))
                .map(|(id, _)| id.clone())
        };
        let membership = head.roster().membership().ok();
        v.members = head
            .roster()
            .members
            .iter()
            .filter_map(|m| {
                let this_app = m.sign_key == me;
                let this_computer = s.local_member == Some(m.sign_key);
                let Some(unit) = membership.as_ref().and_then(|ms| ms.unit_for(&m.sign_key)) else {
                    let host_id = host_of(&m.sign_key);
                    return Some(MemberView {
                        name: m.name.clone(),
                        role: m.role,
                        sign_key: m.sign_key,
                        this_app,
                        this_computer,
                        removable: !this_app && !this_computer && host_id.is_none(),
                        host_id,
                        presence: presence_of(&m.sign_key),
                        enrollment_id: None,
                        kind: None,
                        daemon_key: None,
                        daemon_presence: None,
                    });
                };
                let mine = |k: &SignKey| *k == me || s.local_member == Some(*k);
                let own = unit.members.iter().any(mine);
                if unit.kind != EnrollmentKind::Computer {
                    return Some(MemberView {
                        name: m.name.clone(),
                        role: m.role,
                        sign_key: m.sign_key,
                        this_app,
                        this_computer,
                        host_id: host_of(&m.sign_key),
                        removable: !own,
                        presence: presence_of(&m.sign_key),
                        enrollment_id: Some(unit.id.clone()),
                        kind: Some(unit.kind.clone()),
                        daemon_key: None,
                        daemon_presence: None,
                    });
                }
                // One row per Computer, at its Desktop.
                if m.role != Role::Desktop {
                    return None;
                }
                let daemon = unit.members.iter().copied().find(|k| *k != m.sign_key);
                let desktop = presence_of(&m.sign_key);
                let daemon_presence = daemon.map(|k| presence_of(&k));
                let presence = match &daemon_presence {
                    Some(d) if desktop.kind != "online" && d.kind == "online" => d.clone(),
                    _ => desktop,
                };
                Some(MemberView {
                    name: m.name.clone(),
                    role: m.role,
                    sign_key: m.sign_key,
                    this_app,
                    this_computer: own,
                    host_id: daemon.as_ref().and_then(host_of),
                    removable: !own,
                    presence,
                    enrollment_id: Some(unit.id.clone()),
                    kind: Some(EnrollmentKind::Computer),
                    daemon_key: daemon,
                    daemon_presence,
                })
            })
            .collect();
        v
    }

    /// A quota refusal on the current connection, to `relay`: the notice shows until the
    /// Relay's day ends, and the refusal counts toward the streak of that connection. `true`:
    /// noted. A retired Connector's late report (from an older `epoch`) is not, and neither is
    /// one about a Relay the Ring has left (the report crossed the change, which cleared that
    /// Relay's notice): checked here, under the same lock the change is adopted under.
    fn quota_refused(&self, epoch: u64, relay: &str) -> bool {
        let at = now();
        let mut l = self.lock();
        let ours = l
            .state
            .as_ref()
            .is_some_and(|s| s.chain.head().roster().relay_url == relay);
        if l.epoch != epoch || l.connector.is_none() || !ours {
            return false;
        }
        let conn = l.conn_epoch;
        let q = &mut l.quota;
        q.rev += 1;
        q.until = Some(next_utc_midnight(at));
        if q.streak.0 != conn {
            q.streak = (conn, 0);
        }
        q.streak.1 += 1;
        true
    }

    /// The Relay accepted a counted frame of the current connection: the run of refusals
    /// there is over (the notice stays until a probe or the reset time clears it).
    fn quota_accepted(&self, epoch: u64) {
        let mut l = self.lock();
        if l.epoch == epoch && l.connector.is_some() {
            l.quota.streak = (l.conn_epoch, 0);
        }
    }

    /// The reconcile tick: clears a notice whose reset time has passed, then probes the
    /// quota if one is due.
    fn quota_tick(&self) {
        let lapsed = {
            let mut l = self.lock();
            let q = &mut l.quota;
            if q.until.is_some() && quota_notice(q.until, now()).is_none() {
                q.until = None;
                q.rev += 1;
                // The Relay's day ended: its next frame is accepted, which ends its run too.
                q.streak.1 = 0;
                true
            } else {
                false
            }
        };
        if lapsed {
            self.emit();
        }
        self.maybe_probe();
    }

    /// Sends a quota probe if this window runs the connection, it is up, a phone is in the
    /// Roster, none is in flight, one is due, and the connection is not near the Relay's close
    /// for refusals in a row. Its answer is waited for on a thread of its own.
    fn maybe_probe(&self) {
        let (c, tag) = {
            let mut l = self.lock();
            let phone = l.state.as_ref().is_some_and(|s| {
                s.chain
                    .head()
                    .roster()
                    .members
                    .iter()
                    .any(|m| m.role == Role::Mobile)
            });
            if l.quitting || !phone || !matches!(l.link, Some(LinkState::Connected { .. })) {
                return;
            }
            let Some(c) = l.connector.clone() else {
                return;
            };
            let at = Instant::now();
            let shown = quota_notice(l.quota.until, now()).is_some();
            let (epoch, conn) = (l.epoch, l.conn_epoch);
            let q = &mut l.quota;
            if q.in_flight.is_some()
                || !probe_due(q.last_probe, shown, at, &self.cfg)
                || (q.streak.0 == conn && q.streak.1 >= PROBE_STREAK_LIMIT)
            {
                return;
            }
            q.seq += 1;
            q.in_flight = Some(q.seq);
            q.last_probe = Some(at);
            let tag = ProbeTag {
                seq: q.seq,
                rev: q.rev,
                epoch,
                conn_epoch: conn,
                sent_at: now(),
            };
            (c, tag)
        };
        let seq = tag.seq;
        let clear = |ring: &DesktopRing| {
            let mut l = ring.lock();
            if l.quota.in_flight == Some(seq) {
                l.quota.in_flight = None;
            }
        };
        let ticket = match c.probe_start() {
            Ok(t) => t,
            Err(_) => return clear(self),
        };
        let me = self.me.clone();
        let wait = self.cfg.timeouts.request;
        let spawned = std::thread::Builder::new()
            .name("ring-quota-probe".into())
            .spawn(move || {
                let r = ticket.wait(wait);
                if let Some(ring) = me.upgrade() {
                    ring.probe_done(tag, r);
                }
            });
        if spawned.is_err() {
            clear(self);
        }
    }

    /// A probe's answer, applied only if the Connector, its connection and the quota state are
    /// the ones it was sent under, checked in the same critical section that applies it.
    /// Called with no Ring lock held; emits with none held.
    fn probe_done(&self, tag: ProbeTag, r: Result<(), RingError>) {
        #[cfg(test)]
        if let Some(h) = self.before_probe_apply.lock().unwrap().as_ref() {
            h(&r);
        }
        let (applied, changed) = {
            let mut l = self.lock();
            let current = l.epoch == tag.epoch && l.connector.is_some();
            let conn = l.conn_epoch;
            let q = &mut l.quota;
            if q.in_flight == Some(tag.seq) {
                q.in_flight = None;
            }
            if !current || conn != tag.conn_epoch || q.rev != tag.rev {
                (false, false)
            } else {
                match r {
                    // The Relay accepts this Ring's traffic again.
                    Ok(()) => {
                        q.streak = (conn, 0);
                        q.rev += 1;
                        (true, q.until.take().is_some())
                    }
                    // Counted toward the streak where every refusal is, in `quota_refused`.
                    Err(RingError::Relay {
                        code: ErrorCode::Quota,
                        ..
                    }) => {
                        let until = Some(next_utc_midnight(tag.sent_at));
                        q.rev += 1;
                        let changed = q.until != until;
                        q.until = until;
                        (true, changed)
                    }
                    Err(_) => (true, false),
                }
            }
        };
        #[cfg(test)]
        self.probe_results.lock().unwrap().push(applied);
        let _ = applied;
        if changed {
            self.emit();
        }
    }

    /// The app quits: say goodbye (`quit`), stop connecting, and hand the connection over
    /// (the owner lock is released after the goodbye).
    pub fn quit(&self) {
        if let Some(cancel) = self
            .adoption_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
        {
            cancel.store(true, Ordering::Release);
        }
        // Owner loss is visible before waiting for the commit lock: a commit holder can be
        // blocked on another process's `ring.lock` (the periodic reload, a user change),
        // and fenced work (approval publication) must see the loss meanwhile, not after.
        {
            let mut live = self.lock();
            live.quitting = true;
            for (_, _, cancel) in &live.pairing {
                cancel.store(true, Ordering::Release);
            }
        }
        // Then wait out any in-flight commit, so none starts unfenced after this point.
        {
            let _commit = self.lock_commit();
            let mut live = self.lock();
            live.quitting = true;
            for (_, _, cancel) in &live.pairing {
                cancel.store(true, Ordering::Release);
            }
        }
        self.cancel_pairing(PairingFlow::Phone);
        self.cancel_pairing(PairingFlow::Computer);
        let c = {
            let mut l = self.lock();
            l.quitting = true;
            l.quota.retire();
            if let Some(owner) = l.routing.take() {
                owner.stop();
            }
            l.connector.take()
        };
        #[cfg(any(unix, windows))]
        let binding = self.host_broker.lock().unwrap().take();
        #[cfg(any(unix, windows))]
        if let Some(binding) = binding {
            binding.client.stop();
            drop(binding.server);
        }
        // Drain owner handles outside callback/commit locks, before election is released.
        if let Some(manager) = self
            .routing_manager
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
        {
            manager.retire_ring_hosts();
        }
        self.queue_routing();
        if let Some(c) = c {
            c.stop(ByeReason::quit());
        }
        let owner = self.lock().owner.take();
        if let Some(f) = owner {
            let _ = f.unlock();
        }
    }
}

/// One pairing's bookkeeping: its secret is good once, until `until`.
struct Adder<'a> {
    ring: &'a DesktopRing,
    used: AtomicBool,
    until: Instant,
    cancel: Cancel,
    initial_head: &'a str,
    enrollment: Option<(uuid::Uuid, &'a EnrollmentParameters)>,
}

impl PairingHost for Adder<'_> {
    fn consume(&self) -> Result<(), PairRefusal> {
        if self.used.swap(true, Ordering::SeqCst) {
            return Err(PairRefusal::Used);
        }
        if Instant::now() >= self.until {
            return Err(PairRefusal::Expired);
        }
        Ok(())
    }

    fn add(&self, req: &JoinRequest) -> Result<Joined, PairRefusal> {
        self.ring.add_paired_member_scoped(
            req,
            self.until,
            &self.cancel,
            self.initial_head,
            self.enrollment,
        )
    }
}

/// What a v2 head allows for a possession-proven phone ([`DesktopRing::host_mobile_v2`]).
#[derive(Debug, PartialEq)]
enum PhoneAdmission {
    /// Not a member yet: build an `Enroll` proposal.
    Enroll,
    /// Already this exact Mobile unit in the accepted head: answer "already enrolled".
    Enrolled(Joined),
}

/// The checks of `add_paired_member_scoped` for a v2 head, as typed refusals: the scoped
/// enrollment parameters, the offer's initial head (`Stale`), a member with these keys
/// (exact own Mobile unit → recovery, anything else → `Duplicate`), a Noise key in use
/// (`Duplicate`) and `MAX_MEMBERS` (`Full`).
fn v2_phone_admission(
    head: &SignedRoster,
    approver: &DeviceKeys,
    req: &JoinRequest,
    initial_head: &str,
    enrollment: Option<&EnrollmentParameters>,
) -> Result<PhoneAdmission, PairRefusal> {
    if let Some(p) = enrollment {
        if req.sign_key != p.recipient.signing
            || req.noise_key != p.recipient.noise
            || req.role != Role::Mobile
            || req.name != p.device_name
            || approver.sign_key() != p.approver.signing
            || approver.noise_key() != p.approver.noise
            || head.ring_id() != &p.ring
            || head.roster().relay_url != p.relay
        {
            return Err(PairRefusal::Duplicate);
        }
    }
    if head.roster().v != 2 || head.hash() != initial_head {
        return Err(PairRefusal::Stale);
    }
    if let Some(m) = head.member(&req.sign_key) {
        let own_unit = head.roster().membership().is_ok_and(|m| {
            m.unit_for(&req.sign_key).is_some_and(|u| {
                u.kind == xshell_protocol::ring::membership::EnrollmentKind::Mobile
                    && u.members == [req.sign_key]
            })
        });
        if m.noise_key != req.noise_key || m.role != Role::Mobile || !own_unit {
            return Err(PairRefusal::Duplicate);
        }
        return Ok(PhoneAdmission::Enrolled(Joined {
            ring_id: head.ring_id().clone(),
            relay_url: head.roster().relay_url.clone(),
            version: head.version(),
            hash: head.hash(),
            signed_by: head.roster().signed_by,
        }));
    }
    if head
        .roster()
        .members
        .iter()
        .any(|m| m.noise_key == req.noise_key)
    {
        return Err(PairRefusal::Duplicate);
    }
    if head.roster().members.len() >= MAX_MEMBERS {
        return Err(PairRefusal::Full);
    }
    Ok(PhoneAdmission::Enroll)
}

fn event_of(r: Result<JoinRequest, PairError>) -> PairingEvent {
    match r {
        Ok(req) => PairingEvent::Paired {
            name: req.name,
            role: req.role,
        },
        Err(PairError::Expired) => PairingEvent::Expired,
        Err(e) => PairingEvent::Failed {
            code: e.as_code().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::time::Instant;
    use xshell_protocol::ring::relay::contract;
    use xshell_protocol::ring::relay::test_relay::{TestRelay, TestRelayOptions};

    const WAIT: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct Views {
        last: Mutex<Option<RingView>>,
        cv: Condvar,
    }

    impl RingObserver for Views {
        fn changed(&self, v: &RingView) {
            *self.last.lock().unwrap() = Some(v.clone());
            self.cv.notify_all();
        }
    }

    fn relay() -> TestRelay {
        TestRelay::start_with(TestRelayOptions {
            auth_timeout: Duration::from_millis(500),
            ..TestRelayOptions::default()
        })
    }

    fn cfg(dir: &std::path::Path, url: &str) -> DesktopRingConfig {
        let mut c = DesktopRingConfig::new(dir.join("ring"), "desk".into());
        c.default_relay_url = url.into();
        c.hosted_relay_url = url.into();
        c.backoff_unit = Duration::from_millis(10);
        c.farewell_backoff_unit = Duration::from_millis(10);
        c.farewell_backoff_max = Duration::from_millis(200);
        c.timeouts = RingTimeouts {
            connect: Duration::from_secs(3),
            ping_interval: Duration::from_millis(300),
            dead_after: Duration::from_secs(3),
            request: Duration::from_secs(1),
            bye: Duration::from_millis(500),
        };
        c.owner_retry = Duration::from_millis(50);
        c
    }

    fn open(c: DesktopRingConfig) -> (Arc<DesktopRing>, Arc<Views>) {
        let v = Arc::new(Views::default());
        (DesktopRing::open(c, v.clone()), v)
    }

    fn wait_view(r: &DesktopRing, what: &str, pred: impl Fn(&RingView) -> bool) -> RingView {
        let deadline = Instant::now() + WAIT;
        loop {
            let v = r.view();
            if pred(&v) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {v:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn identity() -> (Arc<DeviceKeys>, LocalIdentity) {
        let k = Arc::new(DeviceKeys::generate().unwrap());
        let id = LocalIdentity {
            sign_key: k.sign_key(),
            noise_key: k.noise_key(),
            name: "host".into(),
        };
        (k, id)
    }

    fn connected(v: &RingView) -> bool {
        v.connection == "connected"
    }

    fn fresh_owner_reload_race(intervene: impl FnOnce(&Arc<DesktopRing>), initial: bool) {
        fresh_owner_reload_race_with_setup(intervene, initial, |_| {});
    }

    fn fresh_owner_reload_race_with_setup(
        intervene: impl FnOnce(&Arc<DesktopRing>),
        initial: bool,
        before_load: impl FnOnce(&Arc<DesktopRing>),
    ) {
        let relay = relay();
        let t = tempfile::tempdir().unwrap();
        let mut config = cfg(t.path(), &relay.url());
        config.owner_retry = Duration::from_secs(60);
        let store = Store::new(config.dir.clone());
        store.retained_keys().unwrap();
        let election =
            super::super::store::open_lock(&config.dir.join("ring-connector.lock")).unwrap();
        election.lock().unwrap();
        let (ring, _) = open(config);
        if initial {
            ring.enable(None, false).unwrap();
        }
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        *ring.after_owner_load.lock().unwrap() = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        before_load(&ring);
        election.unlock().unwrap();
        std::thread::scope(|scope| {
            struct ReleaseLoad(Option<std::sync::mpsc::SyncSender<()>>);
            impl Drop for ReleaseLoad {
                fn drop(&mut self) {
                    if let Some(sender) = self.0.take() {
                        let _ = sender.send(());
                    }
                }
            }
            let acquiring = scope.spawn(|| ring.pairing_ipc_owner());
            let mut release = ReleaseLoad(Some(release_tx));
            entered_rx.recv_timeout(WAIT).unwrap();
            intervene(&ring);
            let expected = ring.chain();
            release.0.take().unwrap().send(()).unwrap();
            assert!(acquiring.join().unwrap());
            let live = ring.lock();
            assert_eq!(
                live.state.as_ref().map(|s| &s.chain),
                expected.as_ref(),
                "fresh reload overwrote intervening adoption/departure"
            );
            assert_eq!(
                live.connector.as_ref().map(|c| c.chain()),
                expected,
                "Connector disagrees with accepted Live membership"
            );
            assert_eq!(
                live.routing.as_ref().map(|r| r.chain()),
                expected,
                "routing disagrees with accepted Live membership"
            );
        });
        ring.quit();
    }

    #[test]
    fn fresh_owner_reload_does_not_overwrite_newer_adoption() {
        let successor = relay();
        fresh_owner_reload_race(
            |ring| {
                ring.set_relay_url(&successor.url()).unwrap();
            },
            true,
        );
    }

    #[test]
    fn fresh_owner_reload_does_not_restore_recovered_departure() {
        fresh_owner_reload_race(
            |ring| {
                std::fs::write(ring.cfg.dir.join("ring.json"), b"broken membership").unwrap();
                ring.reload();
                assert!(ring.chain().is_none());
            },
            true,
        );
    }

    #[test]
    fn fresh_owner_reload_does_not_overwrite_replacement_membership() {
        fresh_owner_reload_race(
            |ring| {
                let previous = ring.chain().unwrap().ring_id().clone();
                std::fs::write(ring.cfg.dir.join("ring.json"), b"broken membership").unwrap();
                ring.reload();
                ring.enable(None, true).unwrap();
                assert_ne!(ring.chain().unwrap().ring_id(), &previous);
            },
            true,
        );
    }

    #[test]
    fn fresh_owner_missing_snapshot_does_not_erase_new_membership() {
        fresh_owner_reload_race(
            |ring| {
                ring.enable(None, false).unwrap();
            },
            false,
        );
    }

    #[test]
    fn fresh_owner_error_snapshot_does_not_erase_intervening_membership() {
        fresh_owner_reload_race_with_setup(
            |ring| {
                std::fs::remove_dir(ring.cfg.dir.join("ring.json")).unwrap();
                ring.enable(None, false).unwrap();
            },
            false,
            |ring| {
                std::fs::remove_file(ring.cfg.dir.join("ring.json")).unwrap();
                std::fs::create_dir(ring.cfg.dir.join("ring.json")).unwrap();
            },
        );
    }

    #[test]
    fn enable_defaults_to_hosted_url() {
        let c = DesktopRingConfig::new("/x".into(), "desk".into());
        assert_eq!(c.default_relay_url, super::super::HOSTED_RELAY_URL);
        assert_eq!(c.hosted_relay_url, super::super::HOSTED_RELAY_URL);
        // With no input, the Ring is created on the default Relay.
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let before = ring.view();
        assert!(!before.enabled);
        assert_eq!(before.connection, "off");
        let v = ring.enable(None, false).unwrap();
        assert!(v.enabled);
        assert_eq!(v.version, Some(1));
        assert_eq!(v.relay_url.as_deref(), Some(r.url().as_str()));
        assert_eq!(v.relay_url.as_deref(), Some(v.hosted_relay_url.as_str()));
        assert_eq!(v.members.len(), 1);
        assert!(v.members[0].this_app);
        wait_view(&ring, "connected", connected);
        assert_eq!(r.head_version(v.ring_id.as_ref().unwrap()), Some(1));
        ring.quit();
    }

    #[test]
    fn enable_with_identity_has_two_members_in_v1() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let (_, id) = identity();
        let v = ring.enable(Some(id.clone()), false).unwrap();
        assert_eq!(v.version, Some(1));
        let roles: Vec<_> = v
            .members
            .iter()
            .map(|m| (m.role, m.this_app, m.this_computer))
            .collect();
        assert_eq!(
            roles,
            [(Role::Desktop, true, false), (Role::Daemon, false, true)]
        );
        assert_eq!(v.members[1].sign_key, id.sign_key);
        assert_eq!(v.members[1].name, "host");
        ring.quit();
    }

    #[test]
    fn enable_returns_the_existing_ring_in_process_and_across_instances() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (a, _) = open(cfg(t.path(), &r.url()));
        let (b, _) = open(cfg(t.path(), &r.url()));
        let ids: Vec<_> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|i| {
                    let ring = if i % 2 == 0 { a.clone() } else { b.clone() };
                    sc.spawn(move || ring.enable(None, false).unwrap().ring_id.unwrap())
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
        let (s, _) = Store::new(t.path().join("ring")).load().unwrap();
        assert_eq!(s.unwrap().chain.ring_id(), &ids[0]);
        a.quit();
        b.quit();
    }

    fn online(r: &TestRelay, rid: &RingId, key: &SignKey) -> bool {
        r.presence(rid, key).is_some_and(|p| p.online)
    }

    // ---- Relay changes and farewells (#51, ADR-0012) --------------------------------------

    use xshell_protocol::ring::relay::test_relay::GateProxy;

    /// A Relay behind a gate: the Ring names the gate, and the Relay knows itself by its origin.
    fn gated(r: &TestRelay) -> GateProxy {
        let g = GateProxy::start(r.addr());
        r.set_origin(&g.url());
        g
    }

    /// A Relay no device can reach: the name does not resolve.
    const DEAD: &str = "wss://relay.xshell.invalid";

    /// Writes a Ring whose versions name `urls` as an earlier build left it after moving off
    /// the first one: `pendingMove` from version 1, no `heldRelays`.
    fn write_legacy_moved(dir: &std::path::Path, urls: &[&str]) -> RingId {
        let keys = Arc::new(DeviceKeys::generate().unwrap());
        let g = SignedRoster::genesis(&*keys, keys.noise_key(), "desk", urls[0], now()).unwrap();
        let mut chain = RosterChain::from_chain(vec![g]).unwrap();
        for u in &urls[1..] {
            let n = chain
                .head()
                .next(&*keys, now(), |d| d.relay_url = u.to_string())
                .unwrap();
            chain.accept(std::slice::from_ref(&n)).unwrap();
        }
        let rid = chain.ring_id().clone();
        let st = RingState {
            keys,
            chain,
            local_member: None,
            farewells: Vec::new(),
            held_relays: BTreeSet::new(),
            host_members: BTreeMap::new(),
        };
        Store::new(dir.to_path_buf())
            .transact(|_, _| Ok((Some(st), ())))
            .unwrap();
        let p = dir.join("ring.json");
        let mut v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("heldRelays");
        o.insert(
            "pendingMove".into(),
            serde_json::json!({"source": urls[0], "from": 1, "target": urls.len()}),
        );
        std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        rid
    }

    fn farewell_relays(v: &RingView) -> Vec<&str> {
        v.farewells.iter().map(|f| f.relay_url.as_str()).collect()
    }

    fn stored_farewells(t: &tempfile::TempDir) -> Vec<(String, u64, u64)> {
        stored(t)
            .farewells
            .iter()
            .map(|f| (f.source.clone(), f.from, f.target))
            .collect()
    }

    #[test]
    fn set_relay_url_bumps_version() {
        let (one, two) = (relay(), relay());
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &one.url()));
        let v = ring.enable(None, false).unwrap();
        let rid = v.ring_id.unwrap();
        wait_view(&ring, "connected", connected);
        wait_until("the relay held", || {
            stored(&t).held_relays.contains(&one.url())
        });
        assert!(ring.set_relay_url("http://x").is_err());
        assert!(
            ring.set_relay_url("ws://example.com").is_err(),
            "plain ws only to loopback"
        );
        assert_eq!(ring.view().version, Some(1));
        // The same URL: no new version.
        assert_eq!(ring.set_relay_url(&one.url()).unwrap().head().version(), 1);
        let c = ring.set_relay_url(&two.url()).unwrap();
        assert_eq!(c.head().version(), 2);
        assert_eq!(c.head().roster().relay_url, two.url());
        // The new Relay has the chain, and the old one was told.
        wait_until("the new relay has version 2", || {
            two.head_version(&rid) == Some(2)
        });
        wait_until("the farewell went out", || {
            one.head_version(&rid) == Some(2) && stored(&t).farewells.is_empty()
        });
        wait_view(&ring, "connected, nothing owed", |v| {
            connected(v) && v.farewells.is_empty()
        });
        // `held` for the new Relay is recorded after its connect, in a transaction of its own.
        wait_until("both relays held", || {
            stored(&t).held_relays == [one.url(), two.url()].into_iter().collect()
        });
        ring.quit();
    }

    /// Mario's case (2026-10-16): an earlier build made the Ring on the placeholder that never
    /// existed and kept waiting for it after the move to the Hosted Relay. This build connects
    /// to the head's Relay at once and owes the placeholder nothing (it is never dialed).
    #[test]
    fn upgrade_from_a_move_owed_to_the_placeholder_connects_to_the_head_at_once() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let rid = write_legacy_moved(
            &t.path().join("ring"),
            &[super::super::LEGACY_PLACEHOLDER_RELAY_URL, &r.url()],
        );
        let start = Instant::now();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let v = wait_view(&ring, "connected", connected);
        assert!(start.elapsed() < Duration::from_secs(2), "at once");
        assert!(v.farewells.is_empty(), "{:?}", v.farewells);
        assert_eq!(r.head_version(&rid), Some(2));
        assert!(stored(&t).farewells.is_empty());
        ring.quit();
    }

    #[test]
    fn upgrade_with_a_move_owed_to_a_dead_relay_keeps_the_farewell() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        write_legacy_moved(&t.path().join("ring"), &[DEAD, &r.url()]);
        let (ring, _) = open(cfg(t.path(), &r.url()));
        wait_view(&ring, "connected", connected);
        let v = wait_view(&ring, "the farewell retried", |v| {
            v.farewells.len() == 1
                && v.farewells[0].state == "waiting"
                && v.farewells[0].failures > 0
        });
        assert_eq!(farewell_relays(&v), [DEAD]);
        assert!(v.farewells[0].error.is_some());
        assert_eq!(stored_farewells(&t), [(DEAD.to_string(), 1, 2)]);
        assert!(connected(&ring.view()), "the link never cared");
        ring.quit();
    }

    #[test]
    fn a_move_off_a_dead_relay_connects_to_the_new_one_at_once() {
        let (one, two) = (relay(), relay());
        let g1 = gated(&one);
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &g1.url()));
        let rid = ring.enable(None, false).unwrap().ring_id.unwrap();
        let me = ring.view().members[0].sign_key;
        wait_view(&ring, "connected", connected);
        wait_until("the relay held", || {
            stored(&t).held_relays.contains(&g1.url())
        });
        g1.set_open(false);
        wait_view(&ring, "the old relay gone", |v| v.connection == "waiting");
        ring.set_relay_url(&two.url()).unwrap();
        wait_view(&ring, "connected to the new relay", connected);
        wait_until("on the new relay", || online(&two, &rid, &me));
        let v = wait_view(&ring, "the farewell owed", |v| {
            v.farewells.first().is_some_and(|f| f.state == "waiting")
        });
        assert_eq!(farewell_relays(&v), [g1.url().as_str()]);
        ring.quit();
    }

    #[test]
    fn no_farewell_to_a_relay_that_never_held_the_ring() {
        let two = relay();
        let t = tempfile::tempdir().unwrap();
        // A Ring made on a Relay that was never reached.
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "waiting", |v| v.connection == "waiting");
        ring.set_relay_url(&two.url()).unwrap();
        wait_view(&ring, "connected to the new relay", connected);
        wait_until("the new relay held", || {
            stored(&t).held_relays.contains(&two.url())
        });
        assert!(ring.view().farewells.is_empty());
        assert!(stored(&t).farewells.is_empty());
        assert!(!stored(&t).held_relays.contains("ws://127.0.0.1:9"));
        ring.quit();
    }

    /// one → two → three while one is gone: each change takes effect at once, neither waits
    /// for one, and one gets the newest head straight away once it is back.
    #[test]
    fn a_relay_change_takes_effect_at_once_and_never_blocks_another() {
        let (one, two, three) = (relay(), relay(), relay());
        let g1 = gated(&one);
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &g1.url()));
        let rid = ring.enable(None, false).unwrap().ring_id.unwrap();
        let me = ring.view().members[0].sign_key;
        wait_view(&ring, "connected", connected);
        wait_until("one held", || stored(&t).held_relays.contains(&g1.url()));
        g1.set_open(false);
        ring.set_relay_url(&two.url()).unwrap();
        wait_until("on two", || online(&two, &rid, &me));
        wait_until("two held", || stored(&t).held_relays.contains(&two.url()));
        ring.set_relay_url(&three.url()).unwrap();
        wait_until("on three", || online(&three, &rid, &me));
        // two took its farewell; one's carries the newest head.
        wait_until("two told", || two.head_version(&rid) == Some(3));
        wait_until("only one owed", || {
            stored_farewells(&t) == [(g1.url(), 1, 3)]
        });
        assert_eq!(one.head_version(&rid), Some(1));
        g1.set_open(true);
        wait_until("one told", || {
            one.head_version(&rid) == Some(3) && stored(&t).farewells.is_empty()
        });
        wait_view(&ring, "nothing owed", |v| {
            connected(v) && v.farewells.is_empty()
        });
        ring.quit();
    }

    #[test]
    fn a_farewell_survives_a_restart() {
        let (one, two) = (relay(), relay());
        let g1 = gated(&one);
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &g1.url()));
        let rid = ring.enable(None, false).unwrap().ring_id.unwrap();
        wait_view(&ring, "connected", connected);
        wait_until("one held", || stored(&t).held_relays.contains(&g1.url()));
        g1.set_open(false);
        ring.set_relay_url(&two.url()).unwrap();
        wait_view(&ring, "connected to two", connected);
        ring.quit();
        drop(ring);
        assert_eq!(stored_farewells(&t), [(g1.url(), 1, 2)]);

        let (ring, _) = open(cfg(t.path(), &two.url()));
        let v = wait_view(&ring, "connected after the restart", connected);
        assert_eq!(farewell_relays(&v), [g1.url().as_str()]);
        g1.set_open(true);
        wait_until("one told", || {
            one.head_version(&rid) == Some(2) && stored(&t).farewells.is_empty()
        });
        ring.quit();
    }

    /// A phone that only knew the old Relay, offline while the Ring moved, follows the move
    /// once that Relay is reachable again.
    #[test]
    fn a_device_left_on_the_old_relay_follows_once_it_comes_back() {
        let (one, two) = (relay(), relay());
        let g1 = gated(&one);
        let t = tempfile::tempdir().unwrap();
        let (ring, _, phone, chain) = with_phone_cfg(cfg(t.path(), &g1.url()));
        let rid = chain.ring_id().clone();
        wait_until("one held", || stored(&t).held_relays.contains(&g1.url()));
        g1.set_open(false);
        ring.set_relay_url(&two.url()).unwrap();
        wait_view(&ring, "connected to two", connected);
        let mut pc = ConnectorConfig::new(RingClientConfig::new(chain, phone.clone()));
        pc.backoff_unit = Duration::from_millis(10);
        pc.client.timeouts = ring.cfg.timeouts;
        let p = Connector::start(pc, Arc::new(NoEvents)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!online(&two, &rid, &phone.sign_key()));
        g1.set_open(true);
        wait_until("the phone on two", || online(&two, &rid, &phone.sign_key()));
        assert_eq!(p.chain().head().version(), 3);
        p.stop(ByeReason::quit());
        ring.quit();
    }

    struct NoEvents;
    impl ConnectorEvents for NoEvents {}

    #[test]
    fn a_host_added_while_a_farewell_is_owed_reaches_the_new_relay() {
        let (one, two) = (relay(), relay());
        let g1 = gated(&one);
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &g1.url()));
        let rid = ring.enable(None, false).unwrap().ring_id.unwrap();
        wait_view(&ring, "connected", connected);
        wait_until("one held", || stored(&t).held_relays.contains(&g1.url()));
        g1.set_open(false);
        ring.set_relay_url(&two.url()).unwrap();
        let (_, a) = identity();
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), a)]);
        assert_eq!(c.head().version(), 3);
        assert_eq!(stored_farewells(&t), [(g1.url(), 1, 3)]);
        wait_until("v3 on the new relay", || two.head_version(&rid) == Some(3));
        g1.set_open(true);
        wait_until("v3 on the old relay", || one.head_version(&rid) == Some(3));
        ring.quit();
    }

    /// A removal while a farewell is owed reaches the old Relay too: the old Relay refuses the
    /// removed phone even when the new Relay is down. Here the farewell is in flight (held at
    /// its `auth`, which stages the move) when the removal comes: it follows as an upload, and
    /// the farewell is cleared only once the old Relay holds it.
    #[test]
    fn removal_while_a_farewell_is_owed_reaches_both_relays() {
        for dest_up in [true, false] {
            let (one, two) = (TestRelay::start(), relay());
            let g1 = gated(&one);
            let dest = if dest_up {
                two.url()
            } else {
                "ws://127.0.0.1:9".to_string()
            };
            let t = tempfile::tempdir().unwrap();
            let (ring, _, phone, chain) = with_phone_cfg(cfg(t.path(), &g1.url()));
            let rid = chain.ring_id().clone();
            wait_until("one held", || stored(&t).held_relays.contains(&g1.url()));
            let (_pc, prec) = contract::connect(&one.target(), &chain, phone.clone());
            let held_at = g1.connections();
            g1.hold_next();
            ring.set_relay_url(&dest).unwrap();
            assert_eq!(stored_farewells(&t), [(g1.url(), 2, 3)]);
            wait_until("the farewell held", || g1.connections() > held_at);
            let c = ring.remove_member(&phone.sign_key()).unwrap();
            assert_eq!(c.head().version(), 4);
            assert_eq!(
                stored_farewells(&t),
                [(g1.url(), 2, 4)],
                "dest_up {dest_up}"
            );
            g1.release();
            wait_until("v4 on the old relay", || one.head_version(&rid) == Some(4));
            assert!(
                prec.closed(WAIT).is_some(),
                "the phone was cut on the old relay"
            );
            let mut target = one.target();
            target.url = g1.url();
            let mut c = contract::RawConn::open(&target, &rid).unwrap();
            let (nonce, _) = c.challenge();
            c.auth(&*phone, &g1.url(), &rid, &nonce);
            assert_eq!(
                c.error(WAIT).map(|e| e.0),
                Some(ErrorCodeOf::NotMember),
                "dest_up {dest_up}"
            );
            wait_until("the farewell cleared", || stored(&t).farewells.is_empty());
            if dest_up {
                wait_until("v4 on the new relay", || two.head_version(&rid) == Some(4));
                assert_eq!(
                    login_error(&two, &rid, &phone),
                    Some(ErrorCodeOf::NotMember)
                );
            }
            ring.quit();
        }
    }

    /// A window that does not own the connection changes the Relay: the owner picks the
    /// commit up from disk, moves, and says farewell to the old Relay.
    #[test]
    fn the_owner_says_farewell_for_a_move_committed_by_another_window() {
        let (one, two) = (relay(), relay());
        let t = tempfile::tempdir().unwrap();
        // B is open before the Ring exists.
        let (b, _) = open(cfg(t.path(), &one.url()));
        let (a, _) = open(cfg(t.path(), &one.url()));
        let rid = a.enable(None, false).unwrap().ring_id.unwrap();
        let me = a.view().members[0].sign_key;
        // Either window may win the connection (B re-reads the state and may take the lock
        // first): the owner is the one connected, the other one defers.
        let deadline = Instant::now() + WAIT;
        let (owner, other) = loop {
            let (va, vb) = (a.view(), b.view());
            if connected(&va)
                && vb.ring_id.as_ref() == Some(&rid)
                && vb.connection == "other-window"
            {
                break (&a, &b);
            }
            if connected(&vb) && va.connection == "other-window" {
                break (&b, &a);
            }
            assert!(
                Instant::now() < deadline,
                "one owns, one defers: {va:?} {vb:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        wait_until("one held", || stored(&t).held_relays.contains(&one.url()));
        other.set_relay_url(&two.url()).unwrap();
        let v = other.view();
        assert_eq!(farewell_relays(&v), [one.url().as_str()]);
        assert_eq!(v.farewells[0].state, "pending", "another window sends it");
        wait_view(owner, "the owner adopted the move", |v| {
            v.version == Some(2)
        });
        wait_until("the owner told one and moved", || {
            one.head_version(&rid) == Some(2) && online(&two, &rid, &me)
        });
        wait_until("the farewell cleared", || stored(&t).farewells.is_empty());
        a.quit();
        b.quit();
    }

    /// Held Relays come only from the Connector's `held` reports, never from a `Connected`
    /// link report, which can be the old Relay's for a moment after a change.
    #[test]
    fn a_stale_connected_report_never_records_the_new_relay_as_held() {
        let (one, two) = (relay(), relay());
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &one.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        wait_until("one held", || stored(&t).held_relays.contains(&one.url()));
        ring.set_relay_url(DEAD).unwrap();
        let epoch = ring.lock().epoch;
        let events = Events {
            ring: Arc::downgrade(&ring),
            epoch,
        };
        events.state(&LinkState::Connected { limited: false });
        // A retired Connector's report counts for nothing either.
        Events {
            ring: Arc::downgrade(&ring),
            epoch: epoch - 1,
        }
        .held(DEAD, 2);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!stored(&t).held_relays.contains(DEAD));
        ring.set_relay_url(&two.url()).unwrap();
        let owed: Vec<String> = stored(&t).farewells.into_iter().map(|f| f.source).collect();
        assert!(!owed.contains(&DEAD.to_string()), "{owed:?}");
        ring.quit();
    }

    /// A2: a `held` report that comes after the change it should have owed a farewell for
    /// creates that farewell then, and the devices on that Relay follow.
    #[test]
    fn a_late_held_report_owes_the_farewell() {
        let (one, two, three) = (relay(), relay(), relay());
        let t = tempfile::tempdir().unwrap();
        let (ring, _, phone, _) = with_phone_cfg(cfg(t.path(), &one.url()));
        wait_until("one held", || stored(&t).held_relays.contains(&one.url()));
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (at_tx, at_rx) = std::sync::mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let two_url = two.url();
        *ring.before_held.lock().unwrap() = Some(Box::new(move |relay: &str| {
            if relay == two_url {
                let _ = at_tx.send(());
                let _ = go_rx.lock().unwrap().recv_timeout(WAIT);
            }
        }));
        let c2 = ring.set_relay_url(&two.url()).unwrap();
        let rid = c2.ring_id().clone();
        at_rx.recv_timeout(WAIT).expect("two's held report");
        // The phone follows to two.
        let mut pc = ConnectorConfig::new(RingClientConfig::new(c2, phone.clone()));
        pc.backoff_unit = Duration::from_millis(10);
        pc.client.timeouts = ring.cfg.timeouts;
        let p = Connector::start(pc, Arc::new(NoEvents)).unwrap();
        wait_until("the phone on two", || online(&two, &rid, &phone.sign_key()));
        ring.set_relay_url(&three.url()).unwrap();
        assert!(
            !stored_farewells(&t).iter().any(|f| f.0 == two.url()),
            "two not known to hold the Ring yet"
        );
        go_tx.send(()).unwrap();
        wait_until("two owed", || stored(&t).held_relays.contains(&two.url()));
        wait_until("the phone on three", || {
            online(&three, &rid, &phone.sign_key())
        });
        assert_eq!(two.head_version(&rid), Some(4));
        p.stop(ByeReason::quit());
        ring.quit();
    }

    /// The quota probe is the new Relay's business and goes on while a farewell is owed.
    #[test]
    fn quota_probe_runs_while_a_farewell_is_owed() {
        let (one, two) = (relay(), relay());
        let g1 = gated(&one);
        let t = tempfile::tempdir().unwrap();
        let mut c = probing(t.path(), &one, 10, 10);
        c.default_relay_url = g1.url();
        c.hosted_relay_url = g1.url();
        let (ring, _, _, _) = with_phone_cfg(c);
        wait_until("one held", || stored(&t).held_relays.contains(&g1.url()));
        g1.set_open(false);
        ring.set_relay_url(&two.url()).unwrap();
        wait_view(&ring, "connected to two, a farewell owed", |v| {
            connected(v) && v.farewells.len() == 1
        });
        let n = probe_results(&ring).len();
        wait_until("a probe applied", || {
            probe_results(&ring)[n..].iter().any(|applied| *applied)
        });
        assert_eq!(ring.view().farewells.len(), 1);
        ring.quit();
    }

    /// Results are adopted in commit order: a commit held between commit and adoption keeps
    /// a later one waiting, and a stale state is refused.
    #[test]
    fn adoption_follows_commit_order() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        *ring.after_commit.lock().unwrap() = Some(Box::new(move || {
            let _ = tx.send(());
            std::thread::sleep(Duration::from_millis(200));
        }));
        let first = {
            let ring = ring.clone();
            let url = relay().url();
            std::thread::spawn(move || ring.set_relay_url(&url).map(|c| c.head().version()))
        };
        rx.recv().unwrap();
        *ring.after_commit.lock().unwrap() = None;
        let (_, id) = identity();
        let second = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                ring.ensure_local_daemon(&id)
                    .map(|c| c.unwrap().head().version())
            })
        };
        assert_eq!(first.join().unwrap().unwrap(), 2);
        assert_eq!(second.join().unwrap().unwrap(), 3);
        let disk = Store::new(t.path().join("ring")).load().unwrap().0.unwrap();
        assert_eq!(
            ring.chain().unwrap(),
            disk.chain,
            "live state is the last commit"
        );
        // A stale state is refused.
        let mut old = disk.clone();
        old.chain = RosterChain::from_chain(disk.chain.versions()[..1].to_vec()).unwrap();
        assert!(!ring.adopt(old));
        assert_eq!(ring.chain().unwrap().head().version(), 3);
        ring.quit();
    }

    #[test]
    fn a_second_instance_waits_for_the_connection_and_takes_it_over() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (a, _) = open(cfg(t.path(), &r.url()));
        let rid = a.enable(None, false).unwrap().ring_id.unwrap();
        wait_view(&a, "a connected", connected);
        let (b, _) = open(cfg(t.path(), &r.url()));
        let v = wait_view(&b, "b defers", |v| v.connection == "other-window");
        assert_eq!(v.ring_id.as_ref(), Some(&rid));
        assert!(v.members.iter().all(|m| m.presence.kind == "unknown"));
        a.quit();
        wait_view(&b, "b took over", connected);
        b.quit();
    }

    #[test]
    fn ensure_local_daemon_replaces_stale_key() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let (_, old) = identity();
        assert_eq!(ring.ensure_local_daemon(&old).unwrap(), None, "no ring yet");
        ring.enable(Some(old.clone()), false).unwrap();
        // Same key: nothing new.
        assert_eq!(
            ring.ensure_local_daemon(&old)
                .unwrap()
                .unwrap()
                .head()
                .version(),
            1
        );
        let (_, new) = identity();
        let c = ring.ensure_local_daemon(&new).unwrap().unwrap();
        assert_eq!(c.head().version(), 2);
        assert!(c.head().member(&old.sign_key).is_none());
        assert_eq!(c.head().member(&new.sign_key).unwrap().role, Role::Daemon);
        assert_eq!(
            ring.ensure_local_daemon(&new)
                .unwrap()
                .unwrap()
                .head()
                .version(),
            2
        );
        let v = ring.view();
        assert!(v
            .members
            .iter()
            .any(|m| m.this_computer && m.sign_key == new.sign_key));
        // Published to the Relay too.
        wait_view(&ring, "published", |v| {
            connected(v) && r.head_version(v.ring_id.as_ref().unwrap()) == Some(2)
        });
        ring.quit();
    }

    #[test]
    fn view_maps_presence() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let (dk, id) = identity();
        ring.enable(Some(id.clone()), false).unwrap();
        let kind = |ring: &DesktopRing| {
            ring.view()
                .members
                .iter()
                .find(|m| m.sign_key == id.sign_key)
                .map(|m| m.presence.clone())
                .unwrap()
        };
        wait_view(&ring, "never connected", |v| {
            connected(v)
                && v.members
                    .iter()
                    .any(|m| m.this_app && m.presence.kind == "online")
        });
        assert_eq!(kind(&ring).kind, "never");
        let chain = ring.chain().unwrap();
        let (c, _) = contract::connect(&r.target(), &chain, dk.clone());
        wait_view(&ring, "online", |_| kind(&ring).kind == "online");
        c.bye(ByeReason::quit()).unwrap();
        wait_view(&ring, "closed", |_| kind(&ring).kind == "closed");
        assert_eq!(kind(&ring).reason.as_deref(), Some("quit"));
        let (c, _) = contract::connect(&r.target(), &chain, dk.clone());
        wait_view(&ring, "online again", |_| kind(&ring).kind == "online");
        drop(c);
        wait_view(&ring, "unreachable", |_| kind(&ring).kind == "unreachable");
        // Without a Relay connection of its own, the Desktop does not know.
        drop(r);
        wait_view(&ring, "unknown", |v| {
            !connected(v) && v.members.iter().all(|m| m.presence.kind == "unknown")
        });
        ring.quit();
    }

    /// First run under a loose umask (an installer made `~/.xshell/bin` 0775, the app data
    /// dir is group-readable): the startup denial reservation succeeds on the first call.
    #[cfg(unix)]
    #[test]
    fn first_run_reservation_tightens_loose_umask_directories() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mode = |p: &std::path::Path| {
            std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(p).unwrap()) & 0o777
        };
        for loose in [0o775, 0o755] {
            let t = tempfile::tempdir().unwrap();
            let home = t.path().canonicalize().unwrap();
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
            let app = home.join("app");
            for d in [home.join(".xshell"), home.join(".xshell/bin"), app.clone()] {
                std::fs::DirBuilder::new().mode(loose).create(&d).unwrap();
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(loose)).unwrap();
            }
            let (ring, _) = open(cfg(&app, "ws://127.0.0.1:9"));
            assert_eq!(ring.reserve_local_companion(&home), Ok(true), "{loose:o}");
            assert_eq!(mode(&app.join("ring")), 0o700);
            assert_eq!(mode(&home.join(".xshell")), 0o700);
            assert!(home
                .join(".xshell/daemon/ring")
                .join(xshell_core::companion_reservation::FILE)
                .exists());
            ring.quit();
        }
    }

    #[test]
    fn unreadable_state_is_moved_aside_and_reported() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        super::super::store::ensure_private_dir(&dir).unwrap();
        // Written as the store writes (on Windows owned by this user: an elevated token's
        // plain write is owned by Administrators, which the store refuses).
        super::super::store::write_private(&dir.join("ring.json"), b"{\"v\":1,\"signSe").unwrap();
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        let v = ring.view();
        assert!(!v.enabled);
        assert_eq!(v.problem.as_deref(), Some("recovered"));
        assert!(v.problem_detail.unwrap().contains("ring.json.bad-"));
        // No new identity without an explicit start-over.
        assert_eq!(ring.enable(None, false).unwrap_err(), START_OVER_REQUIRED);
        let v = ring.enable(None, true).unwrap();
        assert!(v.enabled && v.problem.is_none());
        ring.quit();
    }

    #[test]
    fn corruption_after_open_is_reported_and_never_replaced_implicitly() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let (ring, _) = open(cfg(t.path(), &r.url()));
        let first = ring.enable(None, false).unwrap().ring_id.unwrap();
        // The connection's own commit (the Relay recorded as held) is behind us.
        wait_until("the relay held", || {
            stored_in(&dir).held_relays.contains(&r.url())
        });
        std::fs::write(dir.join("ring.json"), b"torn").unwrap();
        let (_, id) = identity();
        assert_eq!(ring.ensure_local_daemon(&id).unwrap(), None);
        assert_eq!(ring.view().problem.as_deref(), Some("recovered"));
        assert!(ring.set_relay_url(&relay().url()).is_err());
        assert_eq!(ring.view().problem.as_deref(), Some("recovered"));
        assert_eq!(ring.enable(None, false).unwrap_err(), START_OVER_REQUIRED);
        let v = ring.enable(None, true).unwrap();
        assert_ne!(v.ring_id.unwrap(), first, "a new Ring, made explicitly");
        assert!(v.problem.is_none());
        ring.quit();
    }

    /// A failing mutation still reports what loading found, and the view is in the
    /// recovery-required state whatever was held in memory.
    #[test]
    fn a_failing_mutation_reports_recovery() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        // The connection's own commit (the Relay recorded as held) is behind us.
        wait_until("the relay held", || {
            stored_in(&dir).held_relays.contains(&r.url())
        });
        std::fs::write(dir.join("ring.json"), b"torn").unwrap();
        // The first thing after the corruption is a mutation that fails.
        assert!(ring.set_relay_url(&relay().url()).is_err());
        let v = ring.view();
        assert_eq!(v.problem.as_deref(), Some("recovered"));
        assert!(!v.enabled && v.members.is_empty());
        assert_eq!(ring.enable(None, false).unwrap_err(), START_OVER_REQUIRED);
        assert!(ring.enable(None, true).unwrap().enabled);
        ring.quit();
    }

    fn href(id: &str, name: &str, target: &str) -> HostRef {
        HostRef {
            id: id.into(),
            name: name.into(),
            target: target.into(),
        }
    }

    fn quiet(t: &tempfile::TempDir) -> Arc<DesktopRing> {
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        ring.enable(None, false).unwrap();
        ring
    }

    fn all(_: &str) -> bool {
        true
    }

    fn ensure(
        ring: &DesktopRing,
        batch: &[(HostRef, LocalIdentity)],
    ) -> (RosterChain, Vec<(String, HostOutcome)>) {
        ring.ensure_host_daemons(batch, &all, None)
            .unwrap()
            .unwrap()
    }

    fn has(ring: &DesktopRing, id: &LocalIdentity) -> bool {
        ring.chain().unwrap().head().member(&id.sign_key).is_some()
    }

    fn stored(t: &tempfile::TempDir) -> RingState {
        stored_in(&t.path().join("ring"))
    }

    fn stored_in(dir: &std::path::Path) -> RingState {
        Store::new(dir.to_path_buf()).load().unwrap().0.unwrap()
    }

    #[test]
    fn ensure_host_daemons_batches_into_one_version() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (_, b) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_aaaaaaaa", "Alpha", "a|"), a.clone()),
                (href("h_bbbbbbbb", "Beta", "b|"), b.clone()),
            ],
        );
        assert_eq!(c.head().version(), 2, "one version for the batch");
        assert_eq!(
            out,
            [
                ("h_aaaaaaaa".to_string(), HostOutcome::Joined),
                ("h_bbbbbbbb".to_string(), HostOutcome::Joined)
            ]
        );
        assert_eq!(c.head().member(&a.sign_key).unwrap().name, "Alpha");
        assert_eq!(c.head().member(&b.sign_key).unwrap().role, Role::Daemon);
        let s = stored(&t);
        assert_eq!(s.host_members["h_aaaaaaaa"].sign_key, a.sign_key);
        assert_eq!(s.host_members["h_bbbbbbbb"].target, "b|");
        assert_eq!(s.local_member, None);
        ring.quit();
    }

    #[test]
    fn revoked_saved_daemon_is_not_automatically_enrolled_on_reconnect() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, daemon) = identity();
        let batch = [(href("h_aaaaaaaa", "Host", "target|"), daemon.clone())];
        ensure(&ring, &batch);
        ring.set_configured_hosts(Arc::new(|_| false));
        ring.remove_member(&daemon.sign_key).unwrap();
        let before = ring.chain().unwrap().head().hash();
        let (chain, _) = ensure(&ring, &batch);
        assert!(
            chain.authorized_member(&daemon.sign_key).is_none(),
            "reconnect re-enrolled a removed Daemon"
        );
        assert_eq!(chain.head().hash(), before);
        ring.quit();
    }

    #[test]
    fn ensure_host_daemons_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let batch = [(href("h_aaaaaaaa", "Alpha", "a|"), a)];
        ensure(&ring, &batch);
        let sig = Store::new(t.path().join("ring")).signature();
        let (c, out) = ensure(&ring, &batch);
        assert_eq!(c.head().version(), 2);
        assert_eq!(out[0].1, HostOutcome::Joined);
        assert_eq!(
            Store::new(t.path().join("ring")).signature(),
            sig,
            "nothing written"
        );
        ring.quit();
    }

    #[test]
    fn no_ring_adds_nothing() {
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        let (_, a) = identity();
        let r = ring
            .ensure_host_daemons(&[(href("h_aaaaaaaa", "A", "a|"), a)], &all, None)
            .unwrap();
        assert!(r.is_none());
        assert!(!ring.view().enabled);
        ring.quit();
    }

    #[test]
    fn reinstalled_host_replaces_its_old_key() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, old) = identity();
        let (_, new) = identity();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), old.clone())]);
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), new.clone())]);
        assert_eq!(c.head().version(), 3);
        assert!(c.head().member(&old.sign_key).is_none(), "replaced");
        assert!(c.head().member(&new.sign_key).is_some());
        assert_eq!(c.head().roster().members.len(), 2);
        ring.quit();
    }

    /// A Host pointed at another machine keeps the old machine's member (#22 removes it).
    #[test]
    fn retargeted_host_keeps_its_old_member() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, old) = identity();
        let (_, new) = identity();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), old.clone())]);
        ensure(
            &ring,
            &[(href("h_aaaaaaaa", "A", "elsewhere|"), new.clone())],
        );
        assert!(has(&ring, &old) && has(&ring, &new));
        assert_eq!(stored(&t).host_members["h_aaaaaaaa"].sign_key, new.sign_key);
        ring.quit();
    }

    /// Two Host entries reaching one Daemon share its member, named after the entry with
    /// the smallest id; one of them reinstalling does not remove the key the other still
    /// maps to.
    #[test]
    fn two_host_entries_share_one_daemon_member() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, d) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_bbbbbbbb", "Bee", "b|"), d.clone()),
                (href("h_aaaaaaaa", "Ay", "a|"), d.clone()),
            ],
        );
        assert_eq!(c.head().version(), 2);
        assert_eq!(c.head().roster().members.len(), 2, "one member");
        assert_eq!(c.head().member(&d.sign_key).unwrap().name, "Ay");
        assert!(out.iter().all(|(_, o)| *o == HostOutcome::Joined));
        // h_bbbbbbbb now reports a new key at the same place: h_aaaaaaaa still maps to the
        // old one, so it stays.
        let (_, n) = identity();
        ensure(&ring, &[(href("h_bbbbbbbb", "Bee", "b|"), n.clone())]);
        assert!(has(&ring, &d) && has(&ring, &n));
        ring.quit();
    }

    /// The Local Host and a Remote Host entry for the same computer, in either order and in
    /// one batch or two: one member, and a reinstall seen by both replaces it.
    #[test]
    fn local_and_remote_alias_share_one_member_in_both_orders() {
        for order in 0..3 {
            let t = tempfile::tempdir().unwrap();
            let ring = quiet(&t);
            let (_, d) = identity();
            let local = (href(LOCAL_HOST_ID, "host", ""), d.clone());
            let remote = (href("h_aaaaaaaa", "Me again", "me|"), d.clone());
            match order {
                0 => drop(ensure(&ring, &[local.clone(), remote.clone()])),
                1 => drop(ensure(&ring, &[remote.clone(), local.clone()])),
                _ => {
                    ring.ensure_local_daemon(&d).unwrap();
                    ensure(&ring, std::slice::from_ref(&remote));
                }
            }
            let c = ring.chain().unwrap();
            assert_eq!(c.head().roster().members.len(), 2, "order {order}");
            let v = ring.view();
            let m = v.members.iter().find(|m| m.sign_key == d.sign_key).unwrap();
            assert!(m.this_computer);
            assert_eq!(m.host_id.as_deref(), Some("h_aaaaaaaa"));
            // The smallest id names it: "h_…" sorts before "local".
            assert_eq!(m.name, "Me again", "order {order}");
            // Only the Local Host reports the new key: the Remote entry still maps to the
            // old one, which stays.
            let (_, n) = identity();
            ring.ensure_local_daemon(&n).unwrap();
            assert!(has(&ring, &d) && has(&ring, &n), "order {order}");
            // Now the Remote entry sees it too: the old key is nobody's and goes.
            let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "Me again", "me|"), n.clone())]);
            assert!(c.head().member(&d.sign_key).is_none(), "order {order}");
            assert_eq!(c.head().roster().members.len(), 2);
            ring.quit();
        }
    }

    #[test]
    fn rename_updates_member_name_keeping_added_at() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "Old", "a|"), a.clone())]);
        let before = c.head().member(&a.sign_key).unwrap().clone();
        let (c, _) = ensure(&ring, &[(href("h_aaaaaaaa", "New", "a|"), a.clone())]);
        assert_eq!(c.head().version(), 3, "a name-only version");
        let after = c.head().member(&a.sign_key).unwrap();
        assert_eq!(after.name, "New");
        assert_eq!(after.added_at, before.added_at);
        assert_eq!(after.noise_key, before.noise_key);
        ring.quit();
    }

    fn fill(ring: &DesktopRing, n: usize) {
        let batch: Vec<_> = (0..n)
            .map(|i| (href(&format!("h_{i:08}"), "x", "x|"), identity().1))
            .collect();
        ensure(ring, &batch);
    }

    #[test]
    fn full_roster_marks_host_full() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        // The Desktop plus 62: room for one more.
        fill(&ring, MAX_MEMBERS - 2);
        let (_, a) = identity();
        let (_, b) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_zzzzzzza", "A", "a|"), a.clone()),
                (href("h_zzzzzzzb", "B", "b|"), b.clone()),
            ],
        );
        assert_eq!(c.head().roster().members.len(), MAX_MEMBERS);
        assert_eq!(
            out,
            [
                ("h_zzzzzzza".to_string(), HostOutcome::Joined),
                ("h_zzzzzzzb".to_string(), HostOutcome::Full)
            ]
        );
        assert!(has(&ring, &a) && !has(&ring, &b));
        assert!(!stored(&t).host_members.contains_key("h_zzzzzzzb"));
        ring.quit();
    }

    #[test]
    fn replacement_at_capacity_works() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        fill(&ring, MAX_MEMBERS - 2);
        let (_, old) = identity();
        ensure(&ring, &[(href("h_zzzzzzza", "A", "a|"), old.clone())]);
        assert_eq!(
            ring.chain().unwrap().head().roster().members.len(),
            MAX_MEMBERS
        );
        let (_, new) = identity();
        let (c, out) = ensure(&ring, &[(href("h_zzzzzzza", "A", "a|"), new.clone())]);
        assert_eq!(out[0].1, HostOutcome::Joined);
        assert_eq!(c.head().roster().members.len(), MAX_MEMBERS);
        assert!(has(&ring, &new) && !has(&ring, &old));
        ring.quit();
    }

    /// At capacity, a new Host and a reinstalled one in one batch, in either order: the
    /// reinstall replaces its key, the addition is refused.
    #[test]
    fn replacement_kept_before_an_addition_at_capacity() {
        for flip in [false, true] {
            let t = tempfile::tempdir().unwrap();
            let ring = quiet(&t);
            fill(&ring, MAX_MEMBERS - 2);
            let (_, old) = identity();
            ensure(&ring, &[(href("h_zzzzzzzb", "B", "b|"), old.clone())]);
            assert_eq!(
                ring.chain().unwrap().head().roster().members.len(),
                MAX_MEMBERS
            );
            let (_, a) = identity();
            let (_, new) = identity();
            let mut batch = vec![
                (href("h_zzzzzzza", "A", "a|"), a.clone()),
                (href("h_zzzzzzzb", "B", "b|"), new.clone()),
            ];
            if flip {
                batch.reverse();
            }
            let (c, mut out) = ensure(&ring, &batch);
            out.sort();
            assert_eq!(
                out,
                [
                    ("h_zzzzzzza".to_string(), HostOutcome::Full),
                    ("h_zzzzzzzb".to_string(), HostOutcome::Joined)
                ],
                "flip {flip}"
            );
            assert_eq!(c.head().roster().members.len(), MAX_MEMBERS);
            assert!(has(&ring, &new) && !has(&ring, &old) && !has(&ring, &a));
            ring.quit();
        }
    }

    #[test]
    fn a_batch_for_another_ring_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let other = RingId::derive(&identity().1.sign_key);
        let (_, a) = identity();
        let sig = Store::new(t.path().join("ring")).signature();
        let (c, out) = ring
            .ensure_host_daemons(
                &[(href("h_aaaaaaaa", "A", "a|"), a.clone())],
                &all,
                Some(&other),
            )
            .unwrap()
            .unwrap();
        assert_eq!(out, [("h_aaaaaaaa".to_string(), HostOutcome::RingChanged)]);
        assert_eq!(c.head().version(), 1);
        assert!(stored(&t).host_members.is_empty());
        assert_eq!(Store::new(t.path().join("ring")).signature(), sig);
        ring.quit();
    }

    #[test]
    fn other_role_refused_for_that_host_only() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        // This Desktop's own key, reported by a Host: refused; the other Host is added.
        let me = ring.view().members[0].sign_key;
        let mut mine = identity().1;
        mine.sign_key = me;
        let (_, b) = identity();
        let (c, out) = ensure(
            &ring,
            &[
                (href("h_aaaaaaaa", "A", "a|"), mine),
                (href("h_bbbbbbbb", "B", "b|"), b.clone()),
            ],
        );
        assert_eq!(
            out,
            [
                ("h_aaaaaaaa".to_string(), HostOutcome::OtherRole),
                ("h_bbbbbbbb".to_string(), HostOutcome::Joined)
            ]
        );
        assert_eq!(c.head().member(&me).unwrap().role, Role::Desktop);
        assert!(has(&ring, &b));
        assert!(!stored(&t).host_members.contains_key("h_aaaaaaaa"));
        ring.quit();
    }

    #[test]
    fn stale_identities_are_discarded_in_the_transaction() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (_, b) = identity();
        let (c, out) = ring
            .ensure_host_daemons(
                &[
                    (href("h_aaaaaaaa", "A", "a|"), a.clone()),
                    (href("h_bbbbbbbb", "B", "b|"), b.clone()),
                ],
                &|id| id != "h_aaaaaaaa",
                None,
            )
            .unwrap()
            .unwrap();
        assert!(out.contains(&("h_aaaaaaaa".to_string(), HostOutcome::Stale)));
        assert!(c.head().member(&a.sign_key).is_none());
        assert!(c.head().member(&b.sign_key).is_some());
        ring.quit();
    }

    #[test]
    fn member_view_carries_host_id() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (_, l) = identity();
        ring.ensure_local_daemon(&l).unwrap();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), a.clone())]);
        let v = ring.view();
        let of = |k: &SignKey| v.members.iter().find(|m| m.sign_key == *k).unwrap();
        assert_eq!(of(&a.sign_key).host_id.as_deref(), Some("h_aaaaaaaa"));
        assert_eq!(of(&l.sign_key).host_id, None);
        assert!(of(&l.sign_key).this_computer);
        let json = serde_json::to_value(of(&a.sign_key)).unwrap();
        assert_eq!(json["hostId"], "h_aaaaaaaa");
        ring.quit();
    }

    #[cfg(unix)]
    #[test]
    fn refused_settings_are_reported_as_unreadable() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("ring");
        let (ring, _) = open(cfg(t.path(), "ws://127.0.0.1:9"));
        ring.enable(None, false).unwrap();
        let real = t.path().join("elsewhere.json");
        std::fs::rename(dir.join("ring.json"), &real).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("ring.json")).unwrap();
        assert!(ring.set_relay_url("wss://other.example").is_err());
        let v = ring.view();
        assert_eq!(v.problem.as_deref(), Some("unreadable"));
        assert!(!v.enabled);
        ring.quit();
    }

    // ---- Pairing ------------------------------------------------------------------------

    use xshell_protocol::ring::pairing::{PairCode, PairingOffer};
    use xshell_protocol::ring::relay::pair::{pair_as_guest, GuestRequest, PairOptions};

    #[derive(Default)]
    struct Pairings {
        events: Mutex<Vec<(PairingFlow, PairingEvent)>>,
        cv: Condvar,
    }

    impl PairingObserver for Pairings {
        fn pairing(&self, flow: PairingFlow, e: &PairingEvent) {
            self.events.lock().unwrap().push((flow, e.clone()));
            self.cv.notify_all();
        }
    }

    impl Pairings {
        /// The first event after `Waiting` for `flow`.
        fn outcome(&self, flow: PairingFlow) -> PairingEvent {
            let deadline = Instant::now() + WAIT;
            let mut ev = self.events.lock().unwrap();
            loop {
                if let Some((_, e)) = ev
                    .iter()
                    .find(|(f, e)| *f == flow && *e != PairingEvent::Waiting)
                {
                    return e.clone();
                }
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "no outcome for {flow:?}: {ev:?}");
                ev = self.cv.wait_timeout(ev, left).unwrap().0;
            }
        }
    }

    fn guest_opts() -> PairOptions {
        PairOptions {
            step: Duration::from_secs(3),
            ring: RingTimeouts {
                connect: Duration::from_secs(3),
                ..RingTimeouts::default()
            },
            ..PairOptions::default()
        }
    }

    fn scan(payload: &str, keys: &Arc<DeviceKeys>) -> Result<RosterChain, PairError> {
        let o = PairingOffer::parse(payload).unwrap();
        pair_as_guest(
            &GuestRequest {
                relay_url: &o.relay_url,
                secret: &o.secret,
                pin: Some(o.noise_key),
                ring_id: Some(&o.ring_id),
                keys: keys.clone(),
                role: Role::Mobile,
                name: "my phone",
                wait: Duration::ZERO,
            },
            &guest_opts(),
            None,
        )
    }

    #[test]
    fn owned_pairing_stale_cancel_cannot_cancel_successor_or_replay_start() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        ring.pair_phone_owned(a, Arc::new(Pairings::default()))
            .unwrap();
        assert!(ring
            .pair_phone_owned(b, Arc::new(Pairings::default()))
            .is_err());
        ring.cancel_pairing_owned(PairingFlow::Phone, a);
        let offer = ring
            .pair_phone_owned(b, Arc::new(Pairings::default()))
            .unwrap();
        ring.cancel_pairing_owned(PairingFlow::Phone, a);
        assert!(scan(&offer.payload, &Arc::new(DeviceKeys::generate().unwrap())).is_ok());
        assert!(ring
            .pair_phone_owned(a, Arc::new(Pairings::default()))
            .is_err());
        let cancelled_before_reservation = uuid::Uuid::new_v4();
        ring.cancel_pairing_owned(PairingFlow::Phone, cancelled_before_reservation);
        assert!(ring
            .pair_phone_owned(cancelled_before_reservation, Arc::new(Pairings::default()))
            .is_err());
    }

    #[test]
    fn pairing_commit_keeps_old_reader_open_without_storage_migration() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (candidate, _) = open(cfg(t.path(), &r.url()));
        candidate.enable(None, false).unwrap();
        wait_view(&candidate, "connected", connected);
        let (reader, _) = open(cfg(t.path(), &r.url()));
        let offer = candidate.pair_phone(Arc::new(Pairings::default())).unwrap();
        scan(&offer.payload, &Arc::new(DeviceKeys::generate().unwrap())).unwrap();
        wait_view(
            &reader,
            "read-only reader loads committed membership",
            |v| v.members.len() == 2,
        );
        assert!(reader.view().problem.is_none());
        // Frozen old-reader top-level shape; deny_unknown_fields must never see a receipt.
        let bytes = std::fs::read(t.path().join("ring/ring.json")).unwrap();
        let file: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let fields = [
            "v",
            "signSeed",
            "noiseSeed",
            "rosters",
            "localMember",
            "pendingMove",
            "farewells",
            "heldRelays",
            "hostMembers",
        ];
        assert!(file
            .as_object()
            .unwrap()
            .keys()
            .all(|key| fields.contains(&key.as_str())));
        assert!(t.path().join("ring/ring.json").exists());
    }

    #[test]
    fn pairing_abort_after_local_commit_reopens_existing_chain_without_receipt_migration() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (candidate, _) = open(cfg(t.path(), &r.url()));
        candidate.enable(None, false).unwrap();
        wait_view(&candidate, "connected", connected);
        let initial = candidate.chain().unwrap().head().clone();
        let mobile = DeviceKeys::from_seeds(&[71; 32], &[72; 32]);
        let req = JoinRequest {
            role: Role::Mobile,
            sign_key: mobile.sign_key(),
            noise_key: mobile.noise_key(),
            name: "phone".into(),
        };
        *candidate.after_commit.lock().unwrap() =
            Some(Box::new(|| panic!("synthetic postcommit interruption")));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            candidate.add_paired_member(
                &req,
                Instant::now() + WAIT,
                &Arc::new(AtomicBool::new(false)),
                &initial.hash(),
            )
        }));
        assert!(result.is_err());
        candidate.quit();
        drop(candidate);
        let (reopened, _) = open(cfg(t.path(), &r.url()));
        let chain = reopened.chain().unwrap();
        assert_eq!(chain.head().version(), initial.version() + 1);
        assert!(chain
            .head()
            .member(&mobile.sign_key())
            .is_some_and(
                |member| member.noise_key == mobile.noise_key() && member.role == Role::Mobile
            ));
        assert!(reopened.lock().pairing_operations.is_empty());
        // Lost volatile attribution is not proof of publication or a successful task.
        assert!(reopened.view().problem.is_none());
    }

    #[test]
    fn pairing_transaction_rechecks_cancel_expiry_and_authorized_initial_head() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let initial = ring.chain().unwrap().head().clone();
        let phone = DeviceKeys::from_seeds(&[81; 32], &[82; 32]);
        let req = JoinRequest {
            role: Role::Mobile,
            sign_key: phone.sign_key(),
            noise_key: phone.noise_key(),
            name: "phone".into(),
        };
        for (cancel, until, head) in [
            (true, Instant::now() + WAIT, initial.hash()),
            (false, Instant::now(), initial.hash()),
            (
                false,
                Instant::now() + WAIT,
                xshell_protocol::ring::b64::encode(&[99; 32]),
            ),
        ] {
            assert!(ring
                .add_paired_member(&req, until, &Arc::new(AtomicBool::new(cancel)), &head)
                .is_err());
            assert_eq!(ring.chain().unwrap().head(), &initial);
        }
        ring.add_paired_member(
            &req,
            Instant::now() + WAIT,
            &Arc::new(AtomicBool::new(false)),
            &initial.hash(),
        )
        .unwrap();
        // A later operation cannot adopt the first operation's successor as its baseline.
        assert!(ring
            .add_paired_member(
                &req,
                Instant::now() + WAIT,
                &Arc::new(AtomicBool::new(false)),
                &initial.hash()
            )
            .is_err());
    }

    #[test]
    fn exact_member_retry_after_another_desktop_signed_head_pins_this_approver() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let phone = Arc::new(DeviceKeys::from_seeds(&[81; 32], &[82; 32]));
        let other = DeviceKeys::from_seeds(&[83; 32], &[84; 32]);
        let state = ring.pairing_state().unwrap();
        let added = state
            .chain
            .head()
            .next(&*state.keys, now(), |d| {
                d.add(Member::new(
                    "other Desktop",
                    Role::Desktop,
                    other.sign_key(),
                    other.noise_key(),
                    now(),
                ));
                d.add(Member::new(
                    "phone",
                    Role::Mobile,
                    phone.sign_key(),
                    phone.noise_key(),
                    now(),
                ));
            })
            .unwrap();
        let intervening = added.next(&other, now(), |_| {}).unwrap();
        let mut chain = state.chain.clone();
        chain.accept(&[added, intervening]).unwrap();
        ring.persist_newer(&chain);
        wait_view(&ring, "other head adopted", |_| {
            ring.chain().unwrap().head() == chain.head()
        });
        let members = chain.head().roster().members.clone();
        let offer = ring.pair_phone(Arc::new(Pairings::default())).unwrap();
        let retried = scan(&offer.payload, &phone).expect("exact-member retry remains pinned");
        assert_eq!(retried.head().roster().signed_by, state.keys.sign_key());
        assert_eq!(retried.head().version(), chain.head().version() + 1);
        assert_eq!(retried.head().roster().members, members);
    }

    #[test]
    fn enrollment_owned_cancellation_and_expected_recipient_are_enforced() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let phone = Arc::new(DeviceKeys::from_seeds(&[85; 32], &[86; 32]));
        let gui = uuid::Uuid::new_v4();
        let offer = ring
            .pair_phone_owned(gui, Arc::new(Pairings::default()))
            .unwrap();
        let scope = ring
            .enrollment_scope(&ring.chain().unwrap().ring_id().clone(), "my phone".into())
            .unwrap();
        let parameters = scope.enrollment.as_ref().unwrap().parameters(
            &scope,
            RecipientIdentity {
                signing: phone.sign_key(),
                noise: phone.noise_key(),
            },
            now() + 60,
        );
        let task = uuid::Uuid::new_v4();
        let collision = PairOperationId::new(gui).unwrap();
        assert!(ring.enrollment_offer(task, collision, parameters).is_err());
        ring.enrollment_status(task, collision, true);
        scan(&offer.payload, &phone).expect("failed external reservation cannot cancel GUI");

        let scope = ring
            .enrollment_scope(&ring.chain().unwrap().ring_id().clone(), "my phone".into())
            .unwrap();
        let expected = Arc::new(DeviceKeys::from_seeds(&[87; 32], &[88; 32]));
        let parameters = scope.enrollment.as_ref().unwrap().parameters(
            &scope,
            RecipientIdentity {
                signing: expected.sign_key(),
                noise: expected.noise_key(),
            },
            now() + 60,
        );
        let external = PairOperationId::new(uuid::Uuid::new_v4()).unwrap();
        let offer = ring
            .enrollment_offer(uuid::Uuid::new_v4(), external, parameters)
            .unwrap();
        ring.cancel_pairing_owned(PairingFlow::Phone, external.uuid());
        ring.cancel_pairing(PairingFlow::Phone);
        assert!(ring
            .pair_phone_owned(uuid::Uuid::new_v4(), Arc::new(Pairings::default()))
            .is_err());
        scan(offer.offer.expose(), &expected).expect("GUI cannot steal external reservation");

        let scope = ring
            .enrollment_scope(&ring.chain().unwrap().ring_id().clone(), "my phone".into())
            .unwrap();
        let parameters = scope.enrollment.as_ref().unwrap().parameters(
            &scope,
            RecipientIdentity {
                signing: expected.sign_key(),
                noise: expected.noise_key(),
            },
            now() + 60,
        );
        // Explicitly release the completed task; a new request is never an automatic retry.
        let task = ring.lock().enrollment.as_ref().unwrap().task;
        ring.enrollment_status(task, external, true);
        let head = ring.chain().unwrap().head().clone();
        let offer = ring
            .enrollment_offer(
                uuid::Uuid::new_v4(),
                PairOperationId::new(uuid::Uuid::new_v4()).unwrap(),
                parameters,
            )
            .unwrap();
        let wrong = Arc::new(DeviceKeys::from_seeds(&[89; 32], &[90; 32]));
        assert!(scan(offer.offer.expose(), &wrong).is_err());
        assert_eq!(ring.chain().unwrap().head(), &head);
    }

    #[test]
    fn pair_phone_adds_mobile() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let obs = Arc::new(Pairings::default());
        let offer = ring.pair_phone(obs.clone()).unwrap();
        assert!(offer.payload.starts_with("xsp1."));
        assert!(offer.expires_at > now());
        // A v1 Ring keeps the v1 QR (no head anchor) and the v1 flow.
        assert!(PairingOffer::parse(&offer.payload).unwrap().head.is_none());
        let phone = Arc::new(DeviceKeys::generate().unwrap());
        let chain = scan(&offer.payload, &phone).unwrap();
        assert_eq!(
            obs.outcome(PairingFlow::Phone),
            PairingEvent::Paired {
                name: "my phone".into(),
                role: Role::Mobile
            }
        );
        let m = chain.head().member(&phone.sign_key()).unwrap();
        assert_eq!(m.role, Role::Mobile);
        assert_eq!(ring.chain().unwrap().head(), chain.head());
        // The secret is not in the state file.
        let file = std::fs::read_to_string(t.path().join("ring").join("ring.json")).unwrap();
        let o = PairingOffer::parse(&offer.payload).unwrap();
        let secret = xshell_protocol::ring::b64::encode(o.secret.as_bytes());
        assert!(!file.contains(&secret));
        // Scanning the same code again finds nobody: it is used up.
        let again = scan(&offer.payload, &Arc::new(DeviceKeys::generate().unwrap()));
        assert!(again.is_err());
        ring.quit();
    }

    /// A v2 head: an approving Desktop `a`, `extra` Daemons, and a phone `p` as its own
    /// Mobile unit.
    fn v2_head(extra: usize) -> (DeviceKeys, DeviceKeys, SignedRoster) {
        use xshell_protocol::ring::membership::{
            Enrollment, EnrollmentKind, Membership, RingPolicy, RosterChange,
        };
        let a = DeviceKeys::from_seeds(&[1; 32], &[2; 32]);
        let p = DeviceKeys::from_seeds(&[3; 32], &[4; 32]);
        let daemons: Vec<DeviceKeys> = (0..extra)
            .map(|i| DeviceKeys::from_seeds(&[10 + i as u8; 32], &[200 - i as u8; 32]))
            .collect();
        let g = SignedRoster::genesis(&a, a.noise_key(), "desk", "ws://127.0.0.1:9", 100).unwrap();
        let mut r = g.roster().clone();
        r.members.push(Member::new(
            "phone",
            Role::Mobile,
            p.sign_key(),
            p.noise_key(),
            101,
        ));
        for (i, d) in daemons.iter().enumerate() {
            r.members.push(Member::new(
                &format!("d{i}"),
                Role::Daemon,
                d.sign_key(),
                d.noise_key(),
                101,
            ));
        }
        r.v = 2;
        r.version = 2;
        r.prev = Some(g.hash());
        r.issued_at = 101;
        let unit = |n: usize, kind, k: &DeviceKeys| Enrollment {
            id: xshell_protocol::ring::b64::encode(&[n as u8 + 1; 32]),
            kind,
            members: vec![k.sign_key()],
            enrolled_at: 101,
        };
        let mut enrollments = vec![
            unit(0, EnrollmentKind::LegacyDesktop, &a),
            unit(1, EnrollmentKind::Mobile, &p),
        ];
        for (i, d) in daemons.iter().enumerate() {
            enrollments.push(unit(i + 2, EnrollmentKind::Daemon, d));
        }
        r.set_membership(&Membership {
            enrollments,
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: Some(2_592_000),
                activity_key: DeviceKeys::from_seeds(&[99; 32], &[98; 32]).sign_key(),
            },
            change: RosterChange::Upgrade { proofs: vec![] },
        });
        let head = r.sign(&a).unwrap();
        (a, p, head)
    }

    fn phone_req(k: &DeviceKeys) -> JoinRequest {
        JoinRequest {
            role: Role::Mobile,
            sign_key: k.sign_key(),
            noise_key: k.noise_key(),
            name: "phone".into(),
        }
    }

    #[test]
    fn v2_phone_admission_keeps_the_v1_pairing_checks_as_typed_refusals() {
        let (a, p, head) = v2_head(1);
        let fresh = DeviceKeys::from_seeds(&[40; 32], &[41; 32]);
        let initial = head.hash();
        assert_eq!(
            v2_phone_admission(&head, &a, &phone_req(&fresh), &initial, None),
            Ok(PhoneAdmission::Enroll)
        );
        // The offer was issued at another head: stale, nothing committed.
        assert_eq!(
            v2_phone_admission(&head, &a, &phone_req(&fresh), "other", None),
            Err(PairRefusal::Stale)
        );
        // Already this exact Mobile unit: authenticated recovery with the accepted head.
        let Ok(PhoneAdmission::Enrolled(j)) =
            v2_phone_admission(&head, &a, &phone_req(&p), &initial, None)
        else {
            panic!("exact member must recover");
        };
        assert_eq!((j.version, j.hash.clone()), (head.version(), head.hash()));
        assert_eq!(j.signed_by, a.sign_key());
        // The same signing key under another Noise key, or as another role: duplicate.
        let mut changed = phone_req(&p);
        changed.noise_key = fresh.noise_key();
        assert_eq!(
            v2_phone_admission(&head, &a, &changed, &initial, None),
            Err(PairRefusal::Duplicate)
        );
        let daemon = head
            .roster()
            .members
            .iter()
            .find(|m| m.role == Role::Daemon)
            .unwrap()
            .clone();
        let as_daemon = JoinRequest {
            role: Role::Mobile,
            sign_key: daemon.sign_key,
            noise_key: daemon.noise_key,
            name: "phone".into(),
        };
        assert_eq!(
            v2_phone_admission(&head, &a, &as_daemon, &initial, None),
            Err(PairRefusal::Duplicate)
        );
        // A Noise key in use under another signing key: duplicate.
        let mut noise = phone_req(&fresh);
        noise.noise_key = p.noise_key();
        assert_eq!(
            v2_phone_admission(&head, &a, &noise, &initial, None),
            Err(PairRefusal::Duplicate)
        );
    }

    #[test]
    fn v2_phone_admission_refuses_a_full_ring_and_a_v1_head() {
        let (a, _, head) = v2_head(MAX_MEMBERS - 2);
        assert_eq!(head.roster().members.len(), MAX_MEMBERS);
        let fresh = DeviceKeys::from_seeds(&[250; 32], &[251; 32]);
        assert_eq!(
            v2_phone_admission(&head, &a, &phone_req(&fresh), &head.hash(), None),
            Err(PairRefusal::Full)
        );
        let v1 = SignedRoster::genesis(&a, a.noise_key(), "desk", "ws://127.0.0.1:9", 100).unwrap();
        assert_eq!(
            v2_phone_admission(&v1, &a, &phone_req(&fresh), &v1.hash(), None),
            Err(PairRefusal::Stale)
        );
    }

    #[test]
    fn v2_phone_admission_holds_a_scoped_enrollment_to_its_parameters() {
        let (a, _, head) = v2_head(0);
        let fresh = DeviceKeys::from_seeds(&[40; 32], &[41; 32]);
        let identity = |k: &DeviceKeys| RecipientIdentity {
            signing: k.sign_key(),
            noise: k.noise_key(),
        };
        let parameters = EnrollmentParameters {
            ring: head.ring_id().clone(),
            approver: identity(&a),
            recipient: identity(&fresh),
            relay: head.roster().relay_url.clone(),
            role: MobileRole::Mobile,
            device_name: "phone".into(),
            initial_head: ChainHead {
                version: head.version(),
                hash: head.hash(),
            },
            expires_at: now() + 60,
        };
        assert_eq!(
            v2_phone_admission(
                &head,
                &a,
                &phone_req(&fresh),
                &head.hash(),
                Some(&parameters)
            ),
            Ok(PhoneAdmission::Enroll)
        );
        let other = DeviceKeys::from_seeds(&[42; 32], &[43; 32]);
        let mut renamed = phone_req(&fresh);
        renamed.name = "another".into();
        for req in [phone_req(&other), renamed] {
            assert_eq!(
                v2_phone_admission(&head, &a, &req, &head.hash(), Some(&parameters)),
                Err(PairRefusal::Duplicate)
            );
        }
    }

    #[test]
    fn new_offer_invalidates_old() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let obs = Arc::new(Pairings::default());
        let old = ring.pair_phone(obs.clone()).unwrap();
        let new = ring.pair_phone(Arc::new(Pairings::default())).unwrap();
        assert_eq!(
            obs.outcome(PairingFlow::Phone),
            PairingEvent::Failed {
                code: "cancelled".into()
            }
        );
        let phone = Arc::new(DeviceKeys::generate().unwrap());
        assert!(
            scan(&old.payload, &phone).is_err(),
            "the old code still works"
        );
        scan(&new.payload, &phone).unwrap();
        ring.quit();
    }

    #[test]
    fn pair_computer_adds_daemon() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        // `xshelld pair` on the other computer: it waits on the slot of its code.
        let code = PairCode::generate().unwrap();
        let host = Arc::new(DeviceKeys::generate().unwrap());
        let (url, c2, h2) = (r.url(), code.clone(), host.clone());
        let guest = std::thread::spawn(move || {
            let secret = c2.secret();
            pair_as_guest(
                &GuestRequest {
                    relay_url: &url,
                    secret: &secret,
                    pin: None,
                    ring_id: None,
                    keys: h2,
                    role: Role::Daemon,
                    name: "server",
                    wait: Duration::from_secs(5),
                },
                &guest_opts(),
                None,
            )
        });
        std::thread::sleep(Duration::from_millis(300));
        // A wrong code finds nobody, and the waiting computer is not disturbed.
        let obs = Arc::new(Pairings::default());
        assert!(ring
            .pair_computer("not a code", obs.clone())
            .unwrap_err()
            .starts_with("invalid_code"));
        let mut wrong: Vec<char> = code.format().chars().collect();
        wrong[0] = if wrong[0] == '0' { '1' } else { '0' };
        ring.pair_computer(&wrong.iter().collect::<String>(), obs.clone())
            .unwrap();
        assert_eq!(
            obs.outcome(PairingFlow::Computer),
            PairingEvent::Failed {
                code: "not_found".into()
            }
        );
        let obs = Arc::new(Pairings::default());
        ring.pair_computer(&code.format().to_lowercase(), obs.clone())
            .unwrap();
        assert_eq!(
            obs.outcome(PairingFlow::Computer),
            PairingEvent::Paired {
                name: "server".into(),
                role: Role::Daemon
            }
        );
        let chain = guest.join().unwrap().unwrap();
        assert_eq!(
            chain.head().member(&host.sign_key()).unwrap().role,
            Role::Daemon
        );
        ring.quit();
    }

    #[test]
    fn pairing_needs_connection_owner() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (none, _) = open(cfg(t.path(), &r.url()));
        assert_eq!(
            none.pair_phone(Arc::new(Pairings::default())).unwrap_err(),
            PAIR_NOT_ENABLED
        );
        none.quit();
        let (a, _) = open(cfg(t.path(), &r.url()));
        a.enable(None, false).unwrap();
        wait_view(&a, "a connected", connected);
        let (b, _) = open(cfg(t.path(), &r.url()));
        wait_view(&b, "b defers", |v| v.connection == "other-window");
        assert_eq!(
            b.pair_phone(Arc::new(Pairings::default())).unwrap_err(),
            PAIR_OTHER_WINDOW
        );
        assert_eq!(
            b.pair_computer("0000-0000-0000-0000", Arc::new(Pairings::default()))
                .unwrap_err(),
            PAIR_OTHER_WINDOW
        );
        a.quit();
        b.quit();
    }

    // ---- Pairing while the connection is not up (xshell#38) -----------------------------

    /// A Ring on `r` whose Relay connection cannot authenticate (the Relay expects another
    /// origin) while the pairing pipe still works, with a hook that reports when a pairing
    /// starts waiting for the Relay to hold its version. Returns the real origin.
    fn unauthenticated(
        r: &TestRelay,
        c: DesktopRingConfig,
    ) -> (Arc<DesktopRing>, String, std::sync::mpsc::Receiver<Instant>) {
        let real = r.origin();
        r.set_origin("https://wrong.example");
        let (ring, _) = open(c);
        ring.enable(None, false).unwrap();
        wait_view(&ring, "a failed attempt", |v| v.connection == "waiting");
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        *ring.before_publish_wait.lock().unwrap() = Some(Box::new(move || {
            let _ = tx.lock().unwrap().send(Instant::now());
        }));
        (ring, real, rx)
    }

    fn scan_in_thread(
        payload: &str,
    ) -> (
        Arc<DeviceKeys>,
        std::thread::JoinHandle<Result<RosterChain, PairError>>,
    ) {
        let phone = Arc::new(DeviceKeys::generate().unwrap());
        let (p, k) = (payload.to_string(), phone.clone());
        (phone, std::thread::spawn(move || scan(&p, &k)))
    }

    #[test]
    fn pairing_while_connecting_waits_and_succeeds() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, real, waiting) = unauthenticated(&r, cfg(t.path(), &r.url()));
        let obs = Arc::new(Pairings::default());
        let offer = ring.pair_phone(obs.clone()).unwrap();
        let (phone, guest) = scan_in_thread(&offer.payload);
        waiting
            .recv_timeout(WAIT)
            .expect("the pairing waits for the relay");
        assert!(!connected(&ring.view()), "{:?}", ring.view());
        r.set_origin(&real);
        assert_eq!(
            obs.outcome(PairingFlow::Phone),
            PairingEvent::Paired {
                name: "my phone".into(),
                role: Role::Mobile
            }
        );
        let chain = guest.join().unwrap().expect("the phone is paired");
        assert_eq!(
            chain.head().member(&phone.sign_key()).unwrap().role,
            Role::Mobile
        );
        let rid = chain.ring_id().clone();
        assert_eq!(r.head_version(&rid), Some(chain.head().version()));
        assert_eq!(ring.chain().unwrap().head(), chain.head());
        ring.quit();
    }

    #[test]
    fn pairing_publish_wait_is_bounded() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let mut c = cfg(t.path(), &r.url());
        // Below the guest's 3 s step.
        c.pair_publish_wait = Duration::from_millis(1500);
        let (ring, _real, waiting) = unauthenticated(&r, c);
        let obs = Arc::new(Pairings::default());
        let offer = ring.pair_phone(obs.clone()).unwrap();
        let (_phone, guest) = scan_in_thread(&offer.payload);
        let started = waiting.recv_timeout(WAIT).expect("the pairing waits");
        assert_eq!(
            obs.outcome(PairingFlow::Phone),
            PairingEvent::Failed {
                code: "publish_failed".into()
            }
        );
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(1400) && took < Duration::from_millis(2500),
            "{took:?}"
        );
        // The phone hears the refusal, not a step timeout.
        assert_eq!(
            guest.join().unwrap().unwrap_err(),
            PairError::Refused(PairRefusal::PublishFailed)
        );
        ring.quit();
    }

    #[test]
    fn pairing_wait_ends_on_quit() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _real, waiting) = unauthenticated(&r, cfg(t.path(), &r.url()));
        let obs = Arc::new(Pairings::default());
        let offer = ring.pair_phone(obs.clone()).unwrap();
        let (_phone, guest) = scan_in_thread(&offer.payload);
        waiting.recv_timeout(WAIT).expect("the pairing waits");
        let q = Instant::now();
        ring.quit();
        assert!(q.elapsed() < Duration::from_secs(2), "{:?}", q.elapsed());
        match obs.outcome(PairingFlow::Phone) {
            PairingEvent::Failed { code } => {
                assert!(code == "publish_failed" || code == "cancelled", "{code}")
            }
            other => panic!("{other:?}"),
        }
        assert!(q.elapsed() < Duration::from_secs(2), "{:?}", q.elapsed());
        assert!(guest.join().unwrap().is_err());
    }

    // ---- Removal (#22) ------------------------------------------------------------------

    use xshell_protocol::ring::relay::wire::RelayFrame;

    /// A Ring on `r` with a paired phone; returns the ring, the phone's keys and its chain.
    fn with_phone(
        t: &tempfile::TempDir,
        r: &TestRelay,
    ) -> (Arc<DesktopRing>, Arc<DeviceKeys>, RosterChain) {
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let offer = ring.pair_phone(Arc::new(Pairings::default())).unwrap();
        let phone = Arc::new(DeviceKeys::generate().unwrap());
        let chain = scan(&offer.payload, &phone).unwrap();
        (ring, phone, chain)
    }

    fn wait_until(what: &str, pred: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !pred() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The Relay's answer to `keys` logging in now.
    fn login_error(r: &TestRelay, rid: &RingId, keys: &DeviceKeys) -> Option<ErrorCodeOf> {
        let t = r.target();
        let mut c = contract::RawConn::open(&t, rid).unwrap();
        let (nonce, _) = c.challenge();
        c.auth(keys, &t.origin(), rid, &nonce);
        c.error(WAIT).map(|e| e.0)
    }

    type ErrorCodeOf = xshell_protocol::ring::relay::wire::ErrorCode;

    #[test]
    fn remove_member_publishes_a_version_without_it() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, phone, chain) = with_phone(&t, &r);
        let rid = chain.ring_id().clone();
        assert_eq!(chain.head().version(), 2);
        let c = ring.remove_member(&phone.sign_key()).unwrap();
        assert_eq!(c.head().version(), 3);
        assert!(c.head().member(&phone.sign_key()).is_none());
        assert_eq!(stored(&t).chain, c, "committed");
        assert!(ring
            .view()
            .members
            .iter()
            .all(|m| m.sign_key != phone.sign_key()));
        // Published through the Connector: the Relay holds it and refuses the phone.
        wait_until("v3 on the relay", || r.head_version(&rid) == Some(3));
        assert_eq!(login_error(&r, &rid, &phone), Some(ErrorCodeOf::NotMember));
        ring.quit();
    }

    #[test]
    fn remove_member_refuses_this_app() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let me = ring.view().members[0].sign_key;
        assert_eq!(ring.remove_member(&me).unwrap_err(), REMOVE_SELF);
        assert_eq!(ring.view().version, Some(1));
        assert_eq!(stored(&t).chain.head().version(), 1);
        ring.quit();
    }

    #[test]
    fn remove_member_refuses_hosts_still_configured() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let (_, l) = identity();
        ring.ensure_local_daemon(&l).unwrap();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), a.clone())]);
        let version = ring.view().version;
        ring.set_configured_hosts(Arc::new(|_: &str| true));
        assert_eq!(ring.remove_member(&a.sign_key).unwrap_err(), REMOVE_IN_USE);
        assert_eq!(ring.remove_member(&l.sign_key).unwrap_err(), REMOVE_IN_USE);
        assert_eq!(ring.view().version, version, "nothing signed");
        // Once its Host is gone from Settings → Hosts, the member can go, and its mapping
        // with it.
        ring.set_configured_hosts(Arc::new(|id: &str| id != "h_aaaaaaaa"));
        let c = ring.remove_member(&a.sign_key).unwrap();
        assert!(c.head().member(&a.sign_key).is_none());
        assert!(!stored(&t).host_members.contains_key("h_aaaaaaaa"));
        // This computer's Daemon stays refused whatever is configured.
        ring.set_configured_hosts(Arc::new(|_: &str| false));
        assert_eq!(ring.remove_member(&l.sign_key).unwrap_err(), REMOVE_IN_USE);
        ring.quit();
    }

    /// Window B saved Host H in the shared settings file; window A, which does not have H in
    /// memory, still refuses to remove H's Daemon.
    #[test]
    fn remove_member_refuses_a_host_saved_by_another_window() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), a.clone())]);
        let settings = t.path().join("settings.json");
        ring.set_configured_hosts(super::super::configured_hosts(settings.clone(), |_| false));
        std::fs::write(&settings, r#"{"hosts":[{"id":"h_aaaaaaaa","name":"A"}]}"#).unwrap();
        assert_eq!(ring.remove_member(&a.sign_key).unwrap_err(), REMOVE_IN_USE);
        let m = |ring: &DesktopRing| {
            ring.view()
                .members
                .into_iter()
                .find(|m| m.sign_key == a.sign_key)
        };
        assert!(!m(&ring).unwrap().removable);
        std::fs::write(&settings, r#"{"hosts":[]}"#).unwrap();
        assert!(m(&ring).unwrap().removable);
        ring.remove_member(&a.sign_key).unwrap();
        assert!(m(&ring).is_none());
        ring.quit();
    }

    #[test]
    fn remove_member_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let (_, a) = identity();
        let sig = Store::new(t.path().join("ring")).signature();
        let c = ring.remove_member(&a.sign_key).unwrap();
        assert_eq!(c.head().version(), 1);
        assert_eq!(
            Store::new(t.path().join("ring")).signature(),
            sig,
            "nothing written"
        );
        ring.quit();
    }

    #[test]
    fn remove_member_needs_the_connection_owner() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (_, k) = identity();
        let (none, _) = open(cfg(t.path(), &r.url()));
        assert_eq!(
            none.remove_member(&k.sign_key).unwrap_err(),
            PAIR_NOT_ENABLED
        );
        none.quit();
        let (a, _) = open(cfg(t.path(), &r.url()));
        a.enable(None, false).unwrap();
        wait_view(&a, "a connected", connected);
        let (b, _) = open(cfg(t.path(), &r.url()));
        wait_view(&b, "b defers", |v| v.connection == "other-window");
        assert_eq!(b.remove_member(&k.sign_key).unwrap_err(), PAIR_OTHER_WINDOW);
        a.quit();
        b.quit();
    }

    #[test]
    fn view_marks_removable_and_only_configured_host_ids() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, phone, _) = with_phone(&t, &r);
        let (_, l) = identity();
        let (_, a) = identity();
        let (_, b) = identity();
        ring.ensure_local_daemon(&l).unwrap();
        ensure(
            &ring,
            &[
                (href("h_aaaaaaaa", "A", "a|"), a.clone()),
                (href("h_bbbbbbbb", "B", "b|"), b.clone()),
            ],
        );
        ring.set_configured_hosts(Arc::new(|id: &str| id == "h_aaaaaaaa"));
        let v = ring.view();
        let of = |k: &SignKey| v.members.iter().find(|m| m.sign_key == *k).unwrap();
        let me = v.members.iter().find(|m| m.this_app).unwrap();
        assert!(!me.removable);
        assert!(of(&l.sign_key).this_computer && !of(&l.sign_key).removable);
        assert_eq!(of(&a.sign_key).host_id.as_deref(), Some("h_aaaaaaaa"));
        assert!(!of(&a.sign_key).removable);
        assert_eq!(
            of(&b.sign_key).host_id,
            None,
            "its Host is no longer configured"
        );
        assert!(of(&b.sign_key).removable);
        assert!(of(&phone.sign_key()).removable);
        let json = serde_json::to_value(of(&phone.sign_key())).unwrap();
        assert_eq!(json["removable"], true);
        ring.quit();
    }

    /// A Roster the Relay sends that a phone signed is ignored: the Desktop keeps its head.
    #[test]
    fn desktop_ring_ignores_a_mobile_signed_roster() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, phone, chain) = with_phone(&t, &r);
        let rid = chain.ring_id().clone();
        let me = ring
            .view()
            .members
            .iter()
            .find(|m| m.this_app)
            .unwrap()
            .sign_key;
        let intruder = Arc::new(DeviceKeys::generate().unwrap());
        let forged = contract::raw_next(chain.head(), &*phone, |x| {
            x.members.retain(|m| m.sign_key != me);
            x.members
                .push(contract::member(&intruder, "intruder", Role::Desktop));
        });
        let frame = RelayFrame::Roster {
            roster: forged.token().to_string(),
        }
        .encode();
        assert!(r.inject(&rid, &me, &frame));
        // The phone coming online is announced on the same socket after the forged Roster:
        // once the view shows it, the forged one was handled.
        let (_pc, _) = contract::connect(&r.target(), &chain, phone.clone());
        wait_view(&ring, "the phone online", |v| {
            v.members
                .iter()
                .any(|m| m.sign_key == phone.sign_key() && m.presence.kind == "online")
        });
        let v = ring.view();
        assert_eq!(v.version, Some(2));
        assert!(v.members.iter().all(|m| m.sign_key != intruder.sign_key()));
        assert!(v.members.iter().any(|m| m.this_app));
        assert_eq!(stored(&t).chain.head().version(), 2);
        ring.quit();
    }

    /// The configured-Hosts predicate may take the Hosts manager's lock: it never runs while
    /// this thread holds the commit lock, also when a removal's adoption hands the Connector
    /// a farewell owed (`set_farewells` never reports synchronously).
    #[test]
    fn the_configured_predicate_never_runs_under_the_commit_lock() {
        use std::sync::atomic::AtomicUsize;
        let (one, two) = (relay(), relay());
        let g1 = gated(&one);
        let t = tempfile::tempdir().unwrap();
        let (ring, views) = open(cfg(t.path(), &g1.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let (_, a) = identity();
        ensure(&ring, &[(href("h_aaaaaaaa", "A", "a|"), a.clone())]);
        let (calls, under) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (c2, u2) = (calls.clone(), under.clone());
        let weak = Arc::downgrade(&ring);
        ring.set_configured_hosts(Arc::new(move |_: &str| {
            c2.fetch_add(1, Ordering::SeqCst);
            if let Some(r) = weak.upgrade() {
                let owner = *r.commit_owner.lock().unwrap();
                if owner == Some(std::thread::current().id()) {
                    u2.fetch_add(1, Ordering::SeqCst);
                }
            }
            false
        }));
        wait_until("the old relay held", || {
            stored(&t).held_relays.contains(&g1.url())
        });
        g1.set_open(false);
        ring.set_relay_url(&two.url()).unwrap();
        let before = calls.load(Ordering::SeqCst);
        ring.remove_member(&a.sign_key).unwrap();
        assert!(calls.load(Ordering::SeqCst) > before, "the view was built");
        assert_eq!(
            under.load(Ordering::SeqCst),
            0,
            "asked under the commit lock"
        );
        // The deferred emission still reached the observer, with the removal in it.
        let last = views.last.lock().unwrap().clone().unwrap();
        assert!(last.members.iter().all(|m| m.sign_key != a.sign_key));
        assert_eq!(last.farewells.len(), 1, "{:?}", last.farewells);
        ring.quit();
    }

    // ---- The daily quota (#42) ------------------------------------------------------------

    use xshell_protocol::ring::relay::contract::RawConn;
    use xshell_protocol::ring::relay::wire::ClientFrame;

    fn quota_relay(limit: u64) -> Arc<TestRelay> {
        Arc::new(TestRelay::start_with(TestRelayOptions {
            auth_timeout: Duration::from_millis(500),
            quota_frames_per_day: Some(limit),
            ..TestRelayOptions::default()
        }))
    }

    /// Probes every `every`, and every `limited` while the notice is shown.
    fn probing(
        dir: &std::path::Path,
        r: &TestRelay,
        every: u64,
        limited: u64,
    ) -> DesktopRingConfig {
        let mut c = cfg(dir, &r.url());
        c.quota_probe = Duration::from_millis(every);
        c.quota_probe_limited = Duration::from_millis(limited);
        c
    }

    /// A Ring opened with `c` on `r`, with a paired phone: the ring, its views, the phone's
    /// keys and chain.
    fn with_phone_cfg(
        c: DesktopRingConfig,
    ) -> (Arc<DesktopRing>, Arc<Views>, Arc<DeviceKeys>, RosterChain) {
        let (ring, views) = open(c);
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let offer = ring.pair_phone(Arc::new(Pairings::default())).unwrap();
        let phone = Arc::new(DeviceKeys::generate().unwrap());
        let chain = scan(&offer.payload, &phone).unwrap();
        (ring, views, phone, chain)
    }

    /// Uses up the Ring's quota for the day from the phone's own socket.
    fn use_up_quota(r: &TestRelay, chain: &RosterChain, phone: &DeviceKeys) {
        let mut m = RawConn::login(&r.target(), chain, phone);
        let head = chain.head().version();
        for id in 1..=1000 {
            m.send(&ClientFrame::RosterGet { id, since: head }.encode())
                .unwrap();
            match m.frame(WAIT) {
                Some(RelayFrame::RosterChain { .. }) => {}
                Some(RelayFrame::Error {
                    code: ErrorCode::Quota,
                    ..
                }) => return,
                other => panic!("expected roster.chain or quota, got {other:?}"),
            }
        }
        panic!("the quota was never reached");
    }

    /// Shows the notice until `until`, as a refusal would.
    fn set_quota_until(ring: &DesktopRing, until: u64) {
        {
            let mut l = ring.lock();
            l.quota.until = Some(until);
            l.quota.rev += 1;
        }
        ring.emit();
    }

    /// Makes the next probe due at once.
    fn probe_soon(ring: &DesktopRing) {
        ring.lock().quota.last_probe = None;
    }

    fn probe_results(ring: &DesktopRing) -> Vec<bool> {
        ring.probe_results.lock().unwrap().clone()
    }

    fn on_probe_result(ring: &DesktopRing, f: impl Fn(&Result<(), RingError>) + Send + 'static) {
        *ring.before_probe_apply.lock().unwrap() = Some(Box::new(f));
    }

    fn this_app(ring: &DesktopRing) -> SignKey {
        ring.view()
            .members
            .iter()
            .find(|m| m.this_app)
            .unwrap()
            .sign_key
    }

    fn is_quota(r: &Result<(), RingError>) -> bool {
        matches!(
            r,
            Err(RingError::Relay {
                code: ErrorCode::Quota,
                ..
            })
        )
    }

    #[test]
    fn next_utc_midnight_is_the_relays_day_end() {
        assert_eq!(next_utc_midnight(0), 86_400);
        assert_eq!(next_utc_midnight(86_399), 86_400);
        assert_eq!(next_utc_midnight(86_400), 172_800);
    }

    #[test]
    fn quota_notice_lapses_at_reset() {
        assert_eq!(quota_notice(None, 100), None);
        assert_eq!(quota_notice(Some(101), 100), Some(101));
        assert_eq!(quota_notice(Some(100), 100), None);
        assert_eq!(quota_notice(Some(99), 100), None);
    }

    #[test]
    fn probe_due_spacing() {
        let c = DesktopRingConfig::new("/x".into(), "desk".into());
        let at = Instant::now();
        let min = Duration::from_secs(60);
        assert!(probe_due(None, false, at, &c));
        assert!(probe_due(None, true, at, &c));
        assert!(!probe_due(Some(at), false, at + 9 * min, &c));
        assert!(probe_due(Some(at), false, at + 10 * min, &c));
        assert!(!probe_due(Some(at), true, at + 59 * min, &c));
        assert!(probe_due(Some(at), true, at + 60 * min, &c));
    }

    #[test]
    fn quota_refusal_reaches_the_ring_view() {
        let r = quota_relay(60);
        let t = tempfile::tempdir().unwrap();
        let (ring, views, phone, chain) =
            with_phone_cfg(probing(t.path(), &r, 100, 60 * 60 * 1000));
        assert_eq!(ring.view().quota_reset_at, None);
        use_up_quota(&r, &chain, &phone);
        let v = wait_view(&ring, "the quota notice", |v| v.quota_reset_at.is_some());
        assert_eq!(v.quota_reset_at, Some(next_utc_midnight(now())));
        assert!(v.relay_hosted, "the test's hosted URL is the relay's");
        wait_until("the observer told", || {
            views
                .last
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|v| v.quota_reset_at.is_some())
        });
        ring.quit();
    }

    #[test]
    fn own_relay_is_not_hosted() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let mut c = cfg(t.path(), &r.url());
        c.hosted_relay_url = "wss://relay.example.org".into();
        let (ring, _) = open(c);
        ring.enable(None, false).unwrap();
        let v = wait_view(&ring, "connected", connected);
        assert!(!v.limited);
        assert!(!v.relay_hosted);
        ring.quit();
    }

    #[test]
    fn accepted_probe_clears_the_notice() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, views, _, _) = with_phone_cfg(probing(t.path(), &r, 60 * 60 * 1000, 100));
        wait_until("the first probe", || !probe_results(&ring).is_empty());
        set_quota_until(&ring, now() + 3600);
        assert!(ring.view().quota_reset_at.is_some());
        wait_view(&ring, "the notice cleared", |v| v.quota_reset_at.is_none());
        wait_until("the observer told", || {
            views
                .last
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|v| v.quota_reset_at.is_none())
        });
        ring.quit();
    }

    /// Every view the observer was told, in order.
    #[derive(Default)]
    struct History(Mutex<Vec<RingView>>);

    impl RingObserver for History {
        fn changed(&self, v: &RingView) {
            self.0.lock().unwrap().push(v.clone());
        }
    }

    /// The notice is first seen ahead of its reset time; once that passes, the observer is
    /// told a view without it, with nothing else emitting (no phone, so no probes either).
    #[test]
    fn lapsed_notice_is_emitted_cleared() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let seen = Arc::new(History::default());
        let ring = DesktopRing::open(cfg(t.path(), &r.url()), seen.clone());
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        let until = now() + 2;
        set_quota_until(&ring, until);
        let shown = seen
            .0
            .lock()
            .unwrap()
            .iter()
            .rposition(|v| v.quota_reset_at == Some(until))
            .expect("the notice shown ahead of its reset time");
        let deadline = Instant::now() + WAIT;
        loop {
            let later = seen.0.lock().unwrap()[shown + 1..]
                .iter()
                .any(|v| v.connection == "connected" && v.quota_reset_at.is_none());
            if later {
                break;
            }
            assert!(Instant::now() < deadline, "no view without the notice");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(now() >= until);
        assert_eq!(ring.lock().quota.until, None);
        ring.quit();
    }

    #[test]
    fn other_window_shows_no_quota() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (a, _) = open(cfg(t.path(), &r.url()));
        a.enable(None, false).unwrap();
        wait_view(&a, "a connected", connected);
        set_quota_until(&a, now() + 3600);
        assert!(a.view().quota_reset_at.is_some());
        let (b, _) = open(cfg(t.path(), &r.url()));
        wait_view(&b, "b defers", |v| v.connection == "other-window");
        set_quota_until(&b, now() + 3600);
        assert_eq!(b.view().quota_reset_at, None);
        a.quit();
        b.quit();
    }

    #[test]
    fn ring_view_serializes_quota_fields() {
        let t = tempfile::tempdir().unwrap();
        let ring = quiet(&t);
        let json = serde_json::to_value(ring.view()).unwrap();
        assert_eq!(json["quotaResetAt"], Value::Null);
        assert_eq!(json["relayHosted"], true);
        set_quota_until(&ring, 4_000_000_000);
        let json = serde_json::to_value(ring.view()).unwrap();
        assert_eq!(json["quotaResetAt"], 4_000_000_000u64);
        ring.quit();
    }

    /// An accepted probe whose answer comes after a newer refusal leaves the notice standing.
    #[test]
    fn a_refusal_after_the_probe_was_sent_keeps_the_notice() {
        let r = Arc::new(relay());
        let t = tempfile::tempdir().unwrap();
        let hour = 60 * 60 * 1000;
        let (ring, _, _, chain) = with_phone_cfg(probing(t.path(), &r, hour, hour));
        wait_until("the first probe", || probe_results(&ring).len() == 1);
        set_quota_until(&ring, now() + 3600);
        let (rid, me) = (chain.ring_id().clone(), this_app(&ring));
        let (weak, relay) = (Arc::downgrade(&ring), r.clone());
        on_probe_result(&ring, move |res| {
            assert_eq!(res, &Ok(()));
            let ring = weak.upgrade().unwrap();
            let rev = ring.lock().quota.rev;
            let refusal = RelayFrame::Error {
                code: ErrorCode::Quota,
                id: None,
                to: None,
                detail: None,
            };
            assert!(relay.inject(&rid, &me, &refusal.encode()));
            wait_until("the refusal noted", || ring.lock().quota.rev != rev);
        });
        probe_soon(&ring);
        wait_until("the second probe", || probe_results(&ring).len() == 2);
        assert!(ring.view().quota_reset_at.is_some(), "the notice stays");
        assert_eq!(probe_results(&ring), vec![true, false], "fenced off");
        ring.quit();
    }

    /// A probe's refusal that is applied after the app quit shows no notice.
    #[test]
    fn a_stale_refusal_after_quit_shows_no_notice() {
        let r = quota_relay(60);
        let t = tempfile::tempdir().unwrap();
        let hour = 60 * 60 * 1000;
        let (ring, _, phone, chain) = with_phone_cfg(probing(t.path(), &r, hour, hour));
        wait_until("the first probe", || probe_results(&ring).len() == 1);
        use_up_quota(&r, &chain, &phone);
        let weak = Arc::downgrade(&ring);
        on_probe_result(&ring, move |res| {
            assert!(is_quota(res), "{res:?}");
            weak.upgrade().unwrap().quit();
        });
        probe_soon(&ring);
        wait_until("the second probe", || probe_results(&ring).len() == 2);
        assert_eq!(ring.view().quota_reset_at, None, "no notice");
        assert_eq!(probe_results(&ring), vec![true, false], "fenced off");
    }

    /// A probe's refusal that is applied after the notice it caused has lapsed (the quota
    /// recovered meanwhile) does not bring the notice back.
    #[test]
    fn a_stale_refusal_after_recovery_shows_no_notice() {
        let r = quota_relay(60);
        let t = tempfile::tempdir().unwrap();
        let hour = 60 * 60 * 1000;
        let (ring, _, phone, chain) = with_phone_cfg(probing(t.path(), &r, hour, hour));
        wait_until("the first probe", || probe_results(&ring).len() == 1);
        use_up_quota(&r, &chain, &phone);
        let weak = Arc::downgrade(&ring);
        on_probe_result(&ring, move |res| {
            assert!(is_quota(res), "{res:?}");
            let ring = weak.upgrade().unwrap();
            wait_until("the refusal noted", || ring.view().quota_reset_at.is_some());
            // The reset time passes; the reconcile tick clears the notice.
            ring.lock().quota.until = Some(now() - 1);
            wait_until("the lapse", || ring.lock().quota.until.is_none());
        });
        probe_soon(&ring);
        wait_until("the second probe", || probe_results(&ring).len() == 2);
        assert_eq!(ring.view().quota_reset_at, None, "no notice");
        assert_eq!(probe_results(&ring), vec![true, false], "fenced off");
        ring.quit();
    }

    /// Refusals of other frames count toward the connection's streak: probing as fast as it
    /// may, the Desktop stops before the Relay's close for 32 refusals in a row.
    #[test]
    fn mixed_traffic_does_not_probe_into_a_quota_close() {
        let r = quota_relay(60);
        let t = tempfile::tempdir().unwrap();
        let (ring, _, phone, chain) = with_phone_cfg(probing(t.path(), &r, 10, 10));
        wait_until("the first probe", || !probe_results(&ring).is_empty());
        let c = ring.lock().connector.clone().unwrap();
        let conn = c.connection();
        assert!(conn.is_some());
        use_up_quota(&r, &chain, &phone);
        let token = contract::fake_entitlement(chain.ring_id());
        for _ in 0..10 {
            let e = c.put_entitlement(&token).unwrap_err();
            assert!(is_quota(&Err(e)));
        }
        wait_until("the probes stop", || {
            ring.lock().quota.streak.1 >= PROBE_STREAK_LIMIT
        });
        // Long enough for the 32nd refusal at the probe rate (a tick every 50 ms).
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(c.connection(), conn, "the Relay never closed the socket");
        let (streak, conn_epoch) = {
            let l = ring.lock();
            (l.quota.streak, l.conn_epoch)
        };
        assert_eq!(streak.0, conn_epoch);
        assert!(
            (streak.1 as usize) < QUOTA_REFUSALS_BEFORE_CLOSE,
            "{streak:?}"
        );
        let v = ring.view();
        assert_eq!(v.connection, "connected");
        assert!(v.quota_reset_at.is_some());
        ring.quit();
    }

    /// The probes stop at the streak limit on this connection.
    fn at_streak_limit(ring: &DesktopRing) {
        wait_until("no probe in flight", || {
            ring.lock().quota.in_flight.is_none()
        });
        {
            let mut l = ring.lock();
            l.quota.streak = (l.conn_epoch, PROBE_STREAK_LIMIT);
            l.quota.until = Some(now() + 3600);
            l.quota.rev += 1;
        }
        wait_until("no probe in flight", || {
            ring.lock().quota.in_flight.is_none()
        });
        let n = probe_results(ring).len();
        std::thread::sleep(QUIET);
        assert_eq!(probe_results(ring).len(), n, "no probe at the limit");
    }

    const QUIET: Duration = Duration::from_millis(300);

    /// At the streak limit the probes stop; once the notice lapses (the Relay's day ended,
    /// so its next frame is accepted), they resume on the same connection.
    #[test]
    fn probing_resumes_after_the_quota_resets() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _, _, _) = with_phone_cfg(probing(t.path(), &r, 10, 10));
        wait_until("the first probe", || !probe_results(&ring).is_empty());
        at_streak_limit(&ring);
        let n = probe_results(&ring).len();
        ring.lock().quota.until = Some(now());
        wait_until("probing resumed", || probe_results(&ring).len() > n);
        wait_view(&ring, "no notice", |v| v.quota_reset_at.is_none());
        ring.quit();
    }

    /// An accepted counted request of the connection (an `entitlement.put`, say) ends the run
    /// of refusals, as it does on the Relay: the probes resume.
    #[test]
    fn an_accepted_request_ends_the_refusal_run() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _, _, chain) = with_phone_cfg(probing(t.path(), &r, 10, 10));
        wait_until("the first probe", || !probe_results(&ring).is_empty());
        at_streak_limit(&ring);
        let n = probe_results(&ring).len();
        let c = ring.lock().connector.clone().unwrap();
        c.put_entitlement(&contract::fake_entitlement(chain.ring_id()))
            .unwrap();
        wait_until("probing resumed", || probe_results(&ring).len() > n);
        wait_view(&ring, "no notice", |v| v.quota_reset_at.is_none());
        ring.quit();
    }

    /// A quota report of a Connector retired with its Ring (unreadable settings, started
    /// over) that arrives after the replacement's Connector is up changes nothing there.
    #[test]
    fn a_retired_connectors_quota_report_changes_nothing() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &r.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        // The connection's own commit (the Relay recorded as held) is behind us.
        wait_until("the relay held", || {
            stored(&t).held_relays.contains(&r.url())
        });
        let old = Events {
            ring: Arc::downgrade(&ring),
            epoch: ring.lock().epoch,
        };
        std::fs::write(t.path().join("ring").join("ring.json"), b"torn").unwrap();
        let (_, id) = identity();
        assert_eq!(ring.ensure_local_daemon(&id).unwrap(), None);
        assert_eq!(ring.view().problem.as_deref(), Some("recovered"));
        ring.enable(None, true).unwrap();
        wait_view(&ring, "the new Ring connected", connected);
        let conn_epoch = ring.lock().conn_epoch;
        old.quota(&r.url(), None);
        old.state(&LinkState::Stopped { error: None });
        let v = ring.view();
        assert_eq!(v.quota_reset_at, None, "no notice on the new Ring");
        assert_eq!(v.connection, "connected");
        let l = ring.lock();
        assert_eq!(l.quota.streak.1, 0);
        assert_eq!(l.conn_epoch, conn_epoch);
        drop(l);
        ring.quit();
    }

    /// An accepted probe whose answer is applied after the connection changed (the Relay
    /// dropped it and the Connector reconnected) clears nothing.
    #[test]
    fn a_probe_answer_applied_after_a_reconnect_is_fenced() {
        let r = Arc::new(relay());
        let t = tempfile::tempdir().unwrap();
        let hour = 60 * 60 * 1000;
        let (ring, _, _, chain) = with_phone_cfg(probing(t.path(), &r, hour, hour));
        wait_until("the first probe", || probe_results(&ring).len() == 1);
        set_quota_until(&ring, now() + 3600);
        let (rid, me) = (chain.ring_id().clone(), this_app(&ring));
        let (weak, relay) = (Arc::downgrade(&ring), r.clone());
        on_probe_result(&ring, move |res| {
            assert_eq!(res, &Ok(()));
            let ring = weak.upgrade().unwrap();
            let before = ring.lock().conn_epoch;
            assert!(relay.kick(&rid, &me, 1011));
            wait_until("reconnected", || {
                let l = ring.lock();
                l.conn_epoch != before && matches!(l.link, Some(LinkState::Connected { .. }))
            });
        });
        probe_soon(&ring);
        wait_until("the second probe", || probe_results(&ring).len() == 2);
        assert!(ring.view().quota_reset_at.is_some(), "the notice stays");
        assert_eq!(probe_results(&ring), vec![true, false], "fenced off");
        ring.quit();
    }

    // ---- Quota refusals at connect and quota closes (#43) ---------------------------------

    use xshell_protocol::ring::relay::wire::close;

    /// The Relay closes the Desktop's socket for quota: the notice shows.
    #[test]
    fn quota_close_shows_the_notice() {
        let r = relay();
        let t = tempfile::tempdir().unwrap();
        let hour = 60 * 60 * 1000;
        let (ring, _, _, chain) = with_phone_cfg(probing(t.path(), &r, hour, hour));
        wait_until("the first probe", || probe_results(&ring).len() == 1);
        assert_eq!(ring.view().quota_reset_at, None);
        assert!(r.kick(chain.ring_id(), &this_app(&ring), close::QUOTA));
        let v = wait_view(&ring, "the quota notice", |v| v.quota_reset_at.is_some());
        assert_eq!(v.quota_reset_at, Some(next_utc_midnight(now())));
        ring.quit();
    }

    /// The Relay is ahead of the Desktop's chain and the Ring is over quota: the connect is
    /// refused, the link waits, and the notice shows.
    #[test]
    fn connect_time_quota_refusal_shows_the_notice() {
        let r = quota_relay(60);
        let t = tempfile::tempdir().unwrap();
        let hour = 60 * 60 * 1000;
        let c = probing(t.path(), &r, hour, hour);
        let (ring, _, phone, _) = with_phone_cfg(c.clone());
        let (keys, mut chain) = {
            let l = ring.lock();
            let s = l.state.as_ref().unwrap();
            (s.keys.clone(), s.chain.clone())
        };
        ring.quit();
        // Another Desktop of this Ring would have published this: the Relay gets ahead.
        let next = chain
            .head()
            .next(&*keys, now(), |d| d.relay_url = r.url())
            .unwrap();
        let (other, _) = contract::connect(&r.target(), &chain, keys.clone());
        other.publish_roster(&next).unwrap();
        drop(other);
        chain.accept(std::slice::from_ref(&next)).unwrap();
        use_up_quota(&r, &chain, &phone);

        let (ring, _) = open(c);
        let v = wait_view(&ring, "waiting with the notice", |v| {
            v.connection == "waiting" && v.quota_reset_at.is_some()
        });
        assert_eq!(v.quota_reset_at, Some(next_utc_midnight(now())));
        ring.quit();
    }

    /// A4: a Relay that refuses the Ring for quota at connect holds it (only a Relay that
    /// holds a Ring counts its traffic): moving off it owes it a farewell.
    #[test]
    fn a_relay_that_refused_the_ring_for_quota_is_owed_a_farewell() {
        let (r, two) = (quota_relay(60), relay());
        let t = tempfile::tempdir().unwrap();
        let hour = 60 * 60 * 1000;
        let c = probing(t.path(), &r, hour, hour);
        let (ring, _, phone, _) = with_phone_cfg(c.clone());
        let (keys, mut chain) = {
            let l = ring.lock();
            let s = l.state.as_ref().unwrap();
            (s.keys.clone(), s.chain.clone())
        };
        ring.quit();
        let next = chain
            .head()
            .next(&*keys, now(), |d| d.relay_url = r.url())
            .unwrap();
        let (other, _) = contract::connect(&r.target(), &chain, keys.clone());
        other.publish_roster(&next).unwrap();
        drop(other);
        chain.accept(std::slice::from_ref(&next)).unwrap();
        use_up_quota(&r, &chain, &phone);
        // As if this Desktop had never connected there.
        Store::new(t.path().join("ring"))
            .transact(|cur, _| {
                let mut s = cur.unwrap();
                s.held_relays.clear();
                Ok((Some(s), ()))
            })
            .unwrap();

        let (ring, _) = open(c);
        wait_view(&ring, "refused for quota", |v| v.connection == "waiting");
        wait_until("held", || stored(&t).held_relays.contains(&r.url()));
        ring.set_relay_url(&two.url()).unwrap();
        assert_eq!(
            farewell_relays(&ring.view()),
            [r.url().as_str()],
            "{:?}",
            stored(&t).farewells
        );
        ring.quit();
    }

    /// The Ring's Relay changes while the old one is over quota: the old Relay's notice is
    /// cleared, and nothing of its quota shows again. (Its farewell reaches it all the same:
    /// what is staged at `auth` costs no quota.)
    #[test]
    fn a_relay_change_clears_the_old_relays_notice() {
        let (a, b) = (quota_relay(60), relay());
        let t = tempfile::tempdir().unwrap();
        let (ring, _, phone, chain) = with_phone_cfg(probing(t.path(), &a, 50, 50));
        use_up_quota(&a, &chain, &phone);
        wait_view(&ring, "the old Relay's notice", |v| {
            v.quota_reset_at.is_some()
        });
        ring.set_relay_url(&b.url()).unwrap();
        let v = ring.view();
        assert_eq!(v.relay_url.as_deref(), Some(b.url().as_str()));
        assert_eq!(v.quota_reset_at, None, "cleared with the change");
        wait_until("the old Relay told", || {
            a.head_version(chain.ring_id()) == Some(3) && stored(&t).farewells.is_empty()
        });
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            ring.view().quota_reset_at,
            None,
            "no notice for the new Relay"
        );
        ring.quit();
    }

    /// A quota report about the old Relay that crosses a Relay change (the refusal was on
    /// its way when the change was adopted) shows no notice; one about the new Relay does.
    #[test]
    fn an_old_relays_quota_report_after_a_relay_change_shows_no_notice() {
        let (a, b) = (relay(), relay());
        let t = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(t.path(), &a.url()));
        ring.enable(None, false).unwrap();
        wait_view(&ring, "connected", connected);
        // The Connector's report, held until after the change.
        let held = Events {
            ring: Arc::downgrade(&ring),
            epoch: ring.lock().epoch,
        };
        ring.set_relay_url(&b.url()).unwrap();
        held.quota(&a.url(), None);
        assert_eq!(ring.view().quota_reset_at, None, "the old Relay's report");
        assert_eq!(ring.lock().quota.streak.1, 0);
        held.quota(&b.url(), None);
        assert_eq!(
            ring.view().quota_reset_at,
            Some(next_utc_midnight(now())),
            "the new Relay's report"
        );
        ring.quit();
    }
    /// Legacy v1 control only; this is not the new consented Computer Create flow.
    #[test]
    fn legacy_enable_preserves_a_retained_pre_membership_identity() {
        let relay = relay();
        let dir = tempfile::tempdir().unwrap();
        let (ring, _) = open(cfg(dir.path(), &relay.url()));
        let retained = ring.store.retained_keys().unwrap();
        assert!(ring.chain().is_none());
        ring.enable(None, false).unwrap();
        let chain = ring.chain().unwrap();
        assert_eq!(chain.head().roster().signed_by, retained.sign_key());
        assert_eq!(
            chain.head().member(&retained.sign_key()).unwrap().noise_key,
            retained.noise_key()
        );
        ring.quit();
    }
}

#[cfg(test)]
mod v2_edits_tests {
    //! Membership edits on a format-v2 Ring (#87): whole-unit removal, a fixed Relay and a
    //! Host worker that never signs.
    use super::*;
    use xshell_protocol::ring::membership::{
        consent_message, local_consent_context, pairing_consent_context, ConsentProof, Enrollment,
        Membership, RingPolicy, RosterChange,
    };
    use xshell_protocol::ring::{b64, Signature, Signer};

    const RELAY: &str = "ws://127.0.0.1:9";

    struct Observer;
    impl RingObserver for Observer {
        fn changed(&self, _: &RingView) {}
    }

    struct V2 {
        ring: Arc<DesktopRing>,
        keys: Arc<DeviceKeys>,
        me: SignKey,
        my_daemon: Arc<DeviceKeys>,
        desk2: Arc<DeviceKeys>,
        daemon2: Arc<DeviceKeys>,
        phone: Arc<DeviceKeys>,
    }

    fn eid(n: u8) -> String {
        b64::encode(&[n; 32])
    }

    fn prove(r: &mut Roster, ks: &[&DeviceKeys]) {
        let mut m = r.membership().unwrap();
        let context = if matches!(m.change, RosterChange::Enroll { .. }) {
            pairing_consent_context(r, &[42; 32]).unwrap()
        } else {
            local_consent_context(r).unwrap()
        };
        let expires_at = r.issued_at + 600;
        let ps: Vec<ConsentProof> = ks
            .iter()
            .map(|k| ConsentProof {
                key: k.sign_key(),
                expires_at,
                context: context.clone(),
                signature: Signature::from_bytes(
                    k.sign(&consent_message(r, expires_at, &context).unwrap())
                        .unwrap(),
                ),
            })
            .collect();
        match &mut m.change {
            RosterChange::Genesis { proofs } | RosterChange::Enroll { proofs, .. } => *proofs = ps,
            _ => unreachable!(),
        }
        r.set_membership(&m);
    }

    fn enroll(
        prev: &SignedRoster,
        by: &DeviceKeys,
        id: String,
        kind: EnrollmentKind,
        added: &[(&DeviceKeys, Role, &str)],
    ) -> SignedRoster {
        let mut r = prev.roster().clone();
        r.version += 1;
        r.prev = Some(prev.hash());
        r.signed_by = by.sign_key();
        r.issued_at += 1;
        for (k, role, name) in added {
            r.members.push(Member::new(
                name,
                *role,
                k.sign_key(),
                k.noise_key(),
                r.issued_at,
            ));
        }
        let mut m = r.membership().unwrap();
        m.enrollments.push(Enrollment {
            id: id.clone(),
            kind,
            members: added.iter().map(|(k, _, _)| k.sign_key()).collect(),
            enrolled_at: r.issued_at,
        });
        m.change = RosterChange::Enroll {
            enrollment_id: id,
            proofs: vec![],
        };
        r.set_membership(&m);
        let ks: Vec<&DeviceKeys> = added.iter().map(|(k, _, _)| *k).collect();
        prove(&mut r, &ks);
        let next = r.sign(by).unwrap();
        xshell_protocol::ring::verify_successor(prev, &next).unwrap();
        next
    }

    /// This Computer (Desktop "desk" + its Daemon), a second Computer and a phone, in a v2
    /// Ring whose Relay is unreachable (the Connector exists but never connects).
    fn v2_ring(t: &tempfile::TempDir) -> V2 {
        v2_ring_with(t, None, Duration::from_millis(10))
    }

    fn v2_ring_with(
        t: &tempfile::TempDir,
        hook: Option<AdoptionTestHook>,
        backoff: Duration,
    ) -> V2 {
        let dir = t.path().join("ring");
        let store = Store::new(dir.clone());
        let keys = store.retained_keys().unwrap();
        let gen = |_| Arc::new(DeviceKeys::generate().unwrap());
        let (my_daemon, desk2, daemon2, phone) = (gen(0), gen(1), gen(2), gen(3));
        let mut g = Roster {
            v: 2,
            ring_id: RingId::derive(&keys.sign_key()),
            version: 1,
            prev: None,
            relay_url: RELAY.into(),
            signed_by: keys.sign_key(),
            issued_at: 1000,
            members: vec![
                Member::new(
                    "desk",
                    Role::Desktop,
                    keys.sign_key(),
                    keys.noise_key(),
                    1000,
                ),
                Member::new(
                    "desk",
                    Role::Daemon,
                    my_daemon.sign_key(),
                    my_daemon.noise_key(),
                    1000,
                ),
            ],
            extra: Default::default(),
        };
        g.set_membership(&Membership {
            enrollments: vec![Enrollment {
                id: eid(1),
                kind: EnrollmentKind::Computer,
                members: vec![keys.sign_key(), my_daemon.sign_key()],
                enrolled_at: 1000,
            }],
            retired: vec![],
            policy: RingPolicy {
                inactivity_seconds: Some(30 * 24 * 3600),
                activity_key: keys.sign_key(),
            },
            change: RosterChange::Genesis { proofs: vec![] },
        });
        prove(&mut g, &[&keys, &my_daemon]);
        let g = g.sign(&*keys).unwrap();
        let v2 = enroll(
            &g,
            &keys,
            eid(2),
            EnrollmentKind::Computer,
            &[
                (&desk2, Role::Desktop, "laptop"),
                (&daemon2, Role::Daemon, "laptop"),
            ],
        );
        let v3 = enroll(
            &v2,
            &keys,
            eid(3),
            EnrollmentKind::Mobile,
            &[(&phone, Role::Mobile, "phone")],
        );
        let chain = RosterChain::from_chain(vec![g, v2, v3]).unwrap();
        store
            .transact(|_, _| {
                Ok((
                    Some(RingState {
                        keys: keys.clone(),
                        chain,
                        local_member: Some(my_daemon.sign_key()),
                        farewells: vec![],
                        held_relays: BTreeSet::new(),
                        host_members: BTreeMap::new(),
                    }),
                    (),
                ))
            })
            .unwrap();
        let mut c = DesktopRingConfig::new(dir, "desk".into());
        c.default_relay_url = RELAY.into();
        c.hosted_relay_url = RELAY.into();
        c.backoff_unit = backoff;
        c.adoption_test_hook = hook;
        let ring = DesktopRing::open(c, Arc::new(Observer));
        assert_eq!(ring.chain().unwrap().head().version(), 3);
        V2 {
            ring,
            me: keys.sign_key(),
            keys,
            my_daemon,
            desk2,
            daemon2,
            phone,
        }
    }

    fn stored(t: &tempfile::TempDir) -> RingState {
        Store::new(t.path().join("ring")).load().unwrap().0.unwrap()
    }

    fn host(id: &str, k: &DeviceKeys) -> (HostRef, LocalIdentity) {
        (
            HostRef {
                id: id.into(),
                name: "laptop".into(),
                target: format!("{id}|"),
            },
            LocalIdentity {
                sign_key: k.sign_key(),
                noise_key: k.noise_key(),
                name: "laptop".into(),
            },
        )
    }

    /// The candidate a v2 removal publishes, and the state committed on the Relay's
    /// acknowledgment.
    fn removed(t: &tempfile::TempDir, x: &V2, key: &SignKey) -> (SignedRoster, RingState) {
        let s = stored(t);
        let (candidate, unit) = removal_candidate(&s.chain, &x.keys, key, now())
            .unwrap()
            .unwrap();
        let committed = commit_acknowledged(s.clone(), &s.chain, &candidate, &unit).unwrap();
        (candidate, committed)
    }

    fn assert_computer_removed(x: &V2, candidate: &SignedRoster, s: &RingState) {
        assert_eq!(candidate.version(), 4);
        let m = candidate.roster().membership().unwrap();
        assert!(
            matches!(m.change, RosterChange::Remove { ref enrollment_id } if *enrollment_id == eid(2))
        );
        assert!(m.retired.contains(&eid(2)));
        assert!(candidate.member(&x.desk2.sign_key()).is_none());
        assert!(candidate.member(&x.daemon2.sign_key()).is_none());
        assert_eq!(candidate.roster().members.len(), 3);
        assert_eq!(s.chain.head(), candidate);
    }

    #[test]
    fn v2_remove_member_removes_whole_computer() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        for key in [x.desk2.sign_key(), x.daemon2.sign_key()] {
            let (candidate, s) = removed(&t, &x, &key);
            assert_computer_removed(&x, &candidate, &s);
        }
        // A key the head does not list is a no-op.
        let stranger = DeviceKeys::generate().unwrap();
        assert!(
            removal_candidate(&stored(&t).chain, &x.keys, &stranger.sign_key(), now())
                .unwrap()
                .is_none()
        );
        x.ring.quit();
    }

    #[test]
    fn v2_remove_member_refuses_own_unit() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        assert_eq!(x.ring.remove_member(&x.me).unwrap_err(), REMOVE_SELF);
        assert_eq!(
            x.ring.remove_member(&x.my_daemon.sign_key()).unwrap_err(),
            REMOVE_SELF
        );
        assert_eq!(stored(&t).chain.head().version(), 3, "nothing signed");
        x.ring.quit();
    }

    #[test]
    fn v2_remove_phone_unit() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        let (c, s) = removed(&t, &x, &x.phone.sign_key());
        assert_eq!(c.version(), 4);
        assert!(c.member(&x.phone.sign_key()).is_none());
        let m = c.roster().membership().unwrap();
        assert!(m.retired.contains(&eid(3)));
        assert_eq!(m.enrollments.len(), 2);
        assert_eq!(s.chain.head(), &c);
        x.ring.quit();
    }

    /// Without a connected Relay nothing is signed into the store, and asking again tries
    /// again (no silent no-op).
    #[test]
    fn v2_remove_offline_is_refused_and_not_persisted() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        for _ in 0..2 {
            assert_eq!(
                x.ring.remove_member(&x.desk2.sign_key()).unwrap_err(),
                REMOVE_OFFLINE
            );
            assert_eq!(stored(&t).chain.head().version(), 3);
            assert_eq!(x.ring.chain().unwrap().head().version(), 3);
        }
        x.ring.quit();
    }

    /// Same-head terminal evidence held by the Connector fences this Desktop's signing
    /// authority: no Remove is built, published or committed.
    #[test]
    fn v2_terminal_expired_signer_cannot_remove() {
        use xshell_protocol::ring::activity::{policy_hash, ActivityEvidence, UnitActivity};
        use xshell_protocol::ring::relay::ChainPin;
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        let connector = x.ring.lock().connector.clone().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while connector
            .try_chain()
            .is_none_or(|c| c.head().version() != 3)
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let head = x.ring.chain().unwrap().head().clone();
        let m = head.roster().membership().unwrap();
        let evidence = ActivityEvidence {
            ring_id: head.ring_id().clone(),
            relay_url: RELAY.into(),
            policy_head_hash: head.hash(),
            policy_hash: policy_hash(&m.policy),
            observed_at: head.roster().issued_at + 31 * 24 * 3600,
            units: vec![UnitActivity {
                enrollment_id: eid(1),
                member_keys: vec![x.me, x.my_daemon.sign_key()],
                online: false,
                last_active: 1000,
                terminal_expired: true,
            }],
        }
        .sign(&*x.keys)
        .unwrap();
        connector
            .test_observe_activity(
                &ChainPin {
                    version: 3,
                    hash: head.hash(),
                },
                x.me,
                &evidence,
                || {},
            )
            .unwrap();
        assert!(connector.chain().authorized_member(&x.me).is_none());
        let e = x.ring.remove_member(&x.desk2.sign_key()).unwrap_err();
        assert!(e.contains("terminal"), "{e}");
        assert_eq!(stored(&t).chain.head().version(), 3, "nothing committed");
        assert!(
            connector.chain().authorized_member(&x.me).is_none(),
            "the fence stays"
        );
        x.ring.quit();
    }

    #[test]
    fn v2_set_relay_url_refused() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        assert_eq!(
            x.ring.set_relay_url("ws://127.0.0.1:10").unwrap_err(),
            RELAY_FIXED
        );
        assert!(RELAY_FIXED.starts_with("relay_fixed: "));
        // The same URL stays a no-op.
        assert_eq!(x.ring.set_relay_url(RELAY).unwrap().head().version(), 3);
        assert_eq!(stored(&t).chain.head().version(), 3);
        x.ring.quit();
    }

    #[test]
    fn v2_ensure_host_daemons_never_signs() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        let stranger = DeviceKeys::generate().unwrap();
        let (c, out) = x
            .ring
            .ensure_host_daemons(
                &[
                    host("h_aaaaaaaa", &x.daemon2),
                    host("h_bbbbbbbb", &stranger),
                ],
                &|_| true,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(c.head().version(), 3, "nothing signed");
        let of = |id: &str| out.iter().find(|(h, _)| h == id).unwrap().1;
        assert_eq!(of("h_aaaaaaaa"), HostOutcome::Joined);
        assert_eq!(of("h_bbbbbbbb"), HostOutcome::NeedsEnrollment);
        let s = stored(&t);
        assert_eq!(s.chain.head().version(), 3);
        // The enrolled key's mapping applies; the unenrolled one is not mapped.
        assert_eq!(s.host_members["h_aaaaaaaa"].sign_key, x.daemon2.sign_key());
        assert!(!s.host_members.contains_key("h_bbbbbbbb"));
        // A reinstall (same place, new key) neither removes the old Daemon nor maps the new.
        let reinstall = DeviceKeys::generate().unwrap();
        let (c, out) = x
            .ring
            .ensure_host_daemons(&[host("h_aaaaaaaa", &reinstall)], &|_| true, None)
            .unwrap()
            .unwrap();
        assert_eq!(c.head().version(), 3);
        assert_eq!(out[0].1, HostOutcome::NeedsEnrollment);
        assert!(c.head().member(&x.daemon2.sign_key()).is_some());
        assert_eq!(
            stored(&t).host_members["h_aaaaaaaa"].sign_key,
            x.daemon2.sign_key()
        );
        x.ring.quit();
    }

    /// A configured SSH Host does not block a v2 removal (no REMOVE_IN_USE); the committed
    /// state drops only its Ring mapping. (The Settings file is never touched; the workerd
    /// test checks that end to end.)
    #[test]
    fn v2_remove_keeps_ssh_host_entry() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        x.ring
            .ensure_host_daemons(&[host("h_aaaaaaaa", &x.daemon2)], &|_| true, None)
            .unwrap()
            .unwrap();
        x.ring.set_configured_hosts(Arc::new(|_: &str| true));
        assert!(stored(&t).host_members.contains_key("h_aaaaaaaa"));
        let (candidate, s) = removed(&t, &x, &x.daemon2.sign_key());
        assert_computer_removed(&x, &candidate, &s);
        assert!(!s.host_members.contains_key("h_aaaaaaaa"));
        assert_eq!(s.local_member, Some(x.my_daemon.sign_key()));
        x.ring.quit();
    }

    #[test]
    fn v2_view_groups_computer_unit() {
        let t = tempfile::tempdir().unwrap();
        let x = v2_ring(&t);
        let v = x.ring.view();
        assert_eq!(
            v.members.len(),
            3,
            "two Computers and a phone: {:?}",
            v.members
        );
        let own = v.members.iter().find(|m| m.sign_key == x.me).unwrap();
        assert_eq!(own.kind, Some(EnrollmentKind::Computer));
        assert_eq!(own.daemon_key, Some(x.my_daemon.sign_key()));
        assert!(own.this_app && own.this_computer && !own.removable);
        let other = v
            .members
            .iter()
            .find(|m| m.sign_key == x.desk2.sign_key())
            .unwrap();
        assert_eq!(other.name, "laptop");
        assert_eq!(other.role, Role::Desktop);
        assert_eq!(other.enrollment_id.as_deref(), Some(eid(2).as_str()));
        assert_eq!(other.daemon_key, Some(x.daemon2.sign_key()));
        assert!(!other.this_app && !other.this_computer && other.removable);
        assert!(v.members.iter().all(|m| m.sign_key != x.daemon2.sign_key()));
        let phone = v
            .members
            .iter()
            .find(|m| m.sign_key == x.phone.sign_key())
            .unwrap();
        assert_eq!(phone.kind, Some(EnrollmentKind::Mobile));
        assert_eq!(phone.daemon_key, None);
        assert!(phone.removable);
        let json = serde_json::to_value(other).unwrap();
        assert_eq!(json["kind"], "computer");
        assert_eq!(json["daemonKey"], x.daemon2.sign_key().to_string());
        assert_eq!(json["enrollmentId"], eid(2));
        x.ring.quit();
    }

    /// Terminal evidence installed after the candidate was built (at the publication seam)
    /// refuses the upload: the fenced publication sees it, nothing is committed.
    #[test]
    fn v2_terminal_evidence_at_publication_refuses_the_upload() {
        use xshell_protocol::ring::activity::{policy_hash, ActivityEvidence, UnitActivity};
        use xshell_protocol::ring::relay::ChainPin;
        let t = tempfile::tempdir().unwrap();
        type Seam = Box<dyn Fn() + Send + Sync>;
        let slot: Arc<Mutex<Option<Seam>>> = Arc::new(Mutex::new(None));
        let seam = slot.clone();
        let hook = AdoptionTestHook(Arc::new(move |p| {
            if p == AdoptionTestPoint::RemoveBeforePublish {
                if let Some(f) = seam.lock().unwrap().as_ref() {
                    f();
                }
            }
        }));
        // A long backoff: after the first failed dial the link stays as this test sets it.
        let x = v2_ring_with(&t, Some(hook), Duration::from_secs(60));
        let deadline = Instant::now() + Duration::from_secs(5);
        while x.ring.view().connection != "waiting" {
            assert!(Instant::now() < deadline, "{:?}", x.ring.view().connection);
            std::thread::sleep(Duration::from_millis(10));
        }
        let connector = x.ring.lock().connector.clone().unwrap();
        let head = x.ring.chain().unwrap().head().clone();
        let m = head.roster().membership().unwrap();
        let evidence = ActivityEvidence {
            ring_id: head.ring_id().clone(),
            relay_url: RELAY.into(),
            policy_head_hash: head.hash(),
            policy_hash: policy_hash(&m.policy),
            observed_at: head.roster().issued_at + 31 * 24 * 3600,
            units: vec![UnitActivity {
                enrollment_id: eid(1),
                member_keys: vec![x.me, x.my_daemon.sign_key()],
                online: false,
                last_active: 1000,
                terminal_expired: true,
            }],
        }
        .sign(&*x.keys)
        .unwrap();
        let (c, me, hash) = (connector.clone(), x.me, head.hash());
        *slot.lock().unwrap() = Some(Box::new(move || {
            c.test_with_stable_attempt(|| {
                c.test_observe_activity(
                    &ChainPin {
                        version: 3,
                        hash: hash.clone(),
                    },
                    me,
                    &evidence,
                    || {},
                )
            })
            .unwrap();
        }));
        // Pretend the link is up so the removal reaches its publication.
        x.ring.lock().link = Some(LinkState::Connected { limited: false });
        assert!(connector.chain().authorized_member(&x.me).is_some());
        let e = x.ring.remove_member(&x.desk2.sign_key()).unwrap_err();
        assert!(e.contains("terminal"), "{e}");
        assert_eq!(stored(&t).chain.head().version(), 3, "nothing committed");
        assert_eq!(x.ring.chain().unwrap().head().version(), 3);
        assert!(connector.chain().authorized_member(&x.me).is_none());
        x.ring.quit();
    }
}

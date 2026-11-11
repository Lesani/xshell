//! The Roster's signature chain. Version 1 is signed by the Ring's creator, whose key the
//! Ring id derives from; every later version is signed by a Desktop of the version before it
//! and names that version's hash in `prev`. A device keeps the head it trusts and accepts
//! only verified successors, so a Relay can withhold versions but cannot forge one or roll a
//! device back.

use super::roster::{Role, RosterError, SignedRoster};
use super::{Member, RingId, SignKey};

/// A chain is at most this many versions long.
pub const MAX_CHAIN_LEN: usize = 4096;

/// Checks that `g` can start a chain.
pub fn verify_genesis(g: &SignedRoster) -> Result<(), RosterError> {
    let r = g.roster();
    if r.version != 1 || r.prev.is_some() {
        return Err(RosterError::NotGenesis);
    }
    if r.ring_id != RingId::derive(&r.signed_by) {
        return Err(RosterError::RingMismatch);
    }
    match r.member(&r.signed_by) {
        None => Err(RosterError::SignerNotMember),
        Some(m) if m.role != Role::Desktop => Err(RosterError::SignerNotDesktop),
        Some(_) if r.v == 2 => super::membership::verify_genesis(r),
        Some(_) => Ok(()),
    }
}

/// Checks that `next` directly succeeds `prev`.
pub fn verify_successor(prev: &SignedRoster, next: &SignedRoster) -> Result<(), RosterError> {
    let (p, n) = (prev.roster(), next.roster());
    if n.ring_id != p.ring_id {
        return Err(RosterError::RingMismatch);
    }
    if n.version <= p.version {
        return Err(RosterError::Stale);
    }
    if n.version != p.version + 1 {
        return Err(RosterError::Gap);
    }
    if n.prev.as_deref() != Some(prev.hash().as_str()) {
        return Err(RosterError::PrevMismatch);
    }
    if p.v == 2 || n.v == 2 {
        super::membership::verify_transition(prev, next)?;
        return super::activity::verify_transition(std::slice::from_ref(prev), next);
    }
    match p.member(&n.signed_by) {
        None => Err(RosterError::SignerNotMember),
        Some(m) if m.role != Role::Desktop => Err(RosterError::SignerNotDesktop),
        Some(_) => Ok(()),
    }
}

fn verify_with_history(history: &[SignedRoster], next: &SignedRoster) -> Result<(), RosterError> {
    let prev = history.last().ok_or(RosterError::NotGenesis)?;
    if prev.roster().v == 1 && next.roster().v == 1 {
        return verify_successor(prev, next);
    }
    let (p, n) = (prev.roster(), next.roster());
    if n.ring_id != p.ring_id {
        return Err(RosterError::RingMismatch);
    }
    if n.version <= p.version {
        return Err(RosterError::Stale);
    }
    if n.version != p.version + 1 {
        return Err(RosterError::Gap);
    }
    if n.prev.as_deref() != Some(prev.hash().as_str()) {
        return Err(RosterError::PrevMismatch);
    }
    super::membership::verify_transition(prev, next)?;
    super::activity::verify_transition(history, next)
}

/// What [`RosterChain::accept`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// Versions appended.
    pub added: usize,
    /// Members of the old head that the new head no longer lists.
    pub removed: Vec<SignKey>,
}

/// A verified chain from version 1 to its head.
#[derive(Clone, PartialEq, Debug)]
pub struct RosterChain {
    chain: Vec<SignedRoster>,
    terminal: super::activity::TerminalEnrollments,
}

impl RosterChain {
    /// Verifies a whole chain from version 1.
    pub fn from_chain(chain: Vec<SignedRoster>) -> Result<Self, RosterError> {
        let first = chain.first().ok_or(RosterError::NotGenesis)?;
        if chain.len() > MAX_CHAIN_LEN {
            return Err(RosterError::TooLarge);
        }
        verify_genesis(first)?;
        let mut result = RosterChain {
            chain: vec![first.clone()],
            terminal: Default::default(),
        };
        result.extend(&chain[1..])?;
        Ok(result)
    }

    /// Parses and verifies a chain of tokens from version 1.
    pub fn from_tokens<S: AsRef<str>>(tokens: &[S]) -> Result<Self, RosterError> {
        if tokens.len() > MAX_CHAIN_LEN {
            return Err(RosterError::TooLarge);
        }
        let chain = tokens
            .iter()
            .map(|t| SignedRoster::parse(t.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_chain(chain)
    }

    /// Retains independently verified terminal evidence; later candidates cannot use that
    /// enrollment's stale head authority. This does not infer unseen Relay activity.
    pub fn observe_activity(&mut self, token: &str) -> Result<(), RosterError> {
        self.terminal.observe(&self.chain, token)
    }

    /// Effective authorization after independently verified terminal evidence, even while
    /// signed cleanup has not yet removed the immutable unit from the head.
    pub fn authorized_member(&self, key: &SignKey) -> Option<&Member> {
        let head = self.head();
        if head.roster().v == 2 {
            let membership = head.roster().membership().ok()?;
            let unit = membership.unit_for(key)?;
            if self.terminal.ids.contains(&unit.id) {
                return None;
            }
        }
        head.member(key)
    }

    pub fn terminal_enrollments(&self) -> &std::collections::HashSet<String> {
        &self.terminal.ids
    }

    #[cfg(feature = "relay-client")]
    pub(crate) fn merge_terminal_from(&mut self, other: &Self) -> bool {
        let overlap = self.head().version().min(other.head().version());
        if self.ring_id() != other.ring_id() || self.get(overlap) != other.get(overlap) {
            return false;
        }
        let before = self.terminal.ids.len();
        self.terminal.ids.extend(other.terminal.ids.iter().cloned());
        self.terminal.ids.len() != before
    }

    pub fn genesis(&self) -> &SignedRoster {
        &self.chain[0]
    }

    /// The latest version: the Ring's membership.
    pub fn head(&self) -> &SignedRoster {
        &self.chain[self.chain.len() - 1]
    }

    pub fn ring_id(&self) -> &RingId {
        self.genesis().ring_id()
    }

    pub fn versions(&self) -> &[SignedRoster] {
        &self.chain
    }

    /// The stored version `version`, if any.
    pub fn get(&self, version: u64) -> Option<&SignedRoster> {
        let i = usize::try_from(version.checked_sub(1)?).ok()?;
        self.chain.get(i)
    }

    /// Versions newer than `since`.
    pub fn since(&self, since: u64) -> &[SignedRoster] {
        let from = usize::try_from(since)
            .unwrap_or(usize::MAX)
            .min(self.chain.len());
        &self.chain[from..]
    }

    /// Appends verified successors. Versions this chain already holds byte for byte are
    /// skipped; a known version with other bytes is refused (`stale` below the head,
    /// `prev_mismatch` at the head, which is a fork), and so is a list with nothing new.
    /// On error the chain is unchanged.
    pub fn accept(&mut self, newer: &[SignedRoster]) -> Result<Accepted, RosterError> {
        let mut next = self.clone();
        let added = next.extend(newer)?;
        if added == 0 && !newer.is_empty() {
            return Err(RosterError::Stale);
        }
        let removed = removed_between(self.head(), next.head());
        *self = next;
        Ok(Accepted { added, removed })
    }

    /// Like `accept`, but a list that holds nothing new is fine. What a Relay applies to a
    /// device's `auth.chain` candidate.
    pub fn extended(&self, newer: &[SignedRoster]) -> Result<(RosterChain, Accepted), RosterError> {
        let mut next = self.clone();
        let added = next.extend(newer)?;
        let removed = removed_between(self.head(), next.head());
        Ok((next, Accepted { added, removed }))
    }

    fn extend(&mut self, newer: &[SignedRoster]) -> Result<usize, RosterError> {
        let mut added = 0;
        for r in newer {
            let head = self.head().version();
            if r.version() <= head {
                match self.get(r.version()) {
                    Some(known) if known.token() == r.token() => continue,
                    _ if r.version() == head => return Err(RosterError::PrevMismatch),
                    _ => return Err(RosterError::Stale),
                }
            }
            if self.chain.len() >= MAX_CHAIN_LEN {
                return Err(RosterError::TooLarge);
            }
            verify_with_history(&self.chain, r)?;
            super::membership::verify_terminal_authority(self.head(), r, &self.terminal.ids)?;
            if r.roster().v == 2 {
                if let super::membership::RosterChange::Expire { evidence, .. } =
                    r.roster().membership()?.change
                {
                    self.terminal.observe(&self.chain, &evidence)?;
                }
            }
            self.chain.push(r.clone());
            added += 1;
        }
        Ok(added)
    }
}

fn removed_between(old: &SignedRoster, new: &SignedRoster) -> Vec<SignKey> {
    old.roster()
        .members
        .iter()
        .filter(|m| new.member(&m.sign_key).is_none())
        .map(|m| m.sign_key)
        .collect()
}

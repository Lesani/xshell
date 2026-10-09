//! The Roster's signature chain. Version 1 is signed by the Ring's creator, whose key the
//! Ring id derives from; every later version is signed by a Desktop of the version before it
//! and names that version's hash in `prev`. A device keeps the head it trusts and accepts
//! only verified successors, so a Relay can withhold versions but cannot forge one or roll a
//! device back.

use super::roster::{Role, RosterError, SignedRoster};
use super::{RingId, SignKey};

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
    match p.member(&n.signed_by) {
        None => Err(RosterError::SignerNotMember),
        Some(m) if m.role != Role::Desktop => Err(RosterError::SignerNotDesktop),
        Some(_) => Ok(()),
    }
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
}

impl RosterChain {
    /// Verifies a whole chain from version 1.
    pub fn from_chain(chain: Vec<SignedRoster>) -> Result<Self, RosterError> {
        let first = chain.first().ok_or(RosterError::NotGenesis)?;
        if chain.len() > MAX_CHAIN_LEN {
            return Err(RosterError::TooLarge);
        }
        verify_genesis(first)?;
        for w in chain.windows(2) {
            verify_successor(&w[0], &w[1])?;
        }
        Ok(RosterChain { chain })
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
            verify_successor(self.head(), r)?;
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

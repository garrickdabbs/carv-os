//! Pure capability-space and derivation-tree logic.
//!
//! A capability can be copied or minted only with a subset of its source rights. Deleting a
//! capability hands its derived capabilities to its own parent, so revoking an ancestor still
//! invalidates every live descendant, and a CSpace never holds more derivation nodes than slots.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::ops::BitOr;

/// Identifies a kernel object referenced by a capability.
pub type ObjectId = u64;

/// Identifies the sender when a minted capability is used.
pub type Badge = u64;

/// The rights granted by a capability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rights(u8);

impl Rights {
    /// Permission to read an object.
    pub const READ: Self = Self(1 << 0);
    /// Permission to write an object.
    pub const WRITE: Self = Self(1 << 1);
    /// Permission to mint a capability with a badge.
    pub const GRANT: Self = Self(1 << 2);
    /// Permission to execute an object.
    pub const EXEC: Self = Self(1 << 3);
    /// Permission to derive a capability by copying.
    pub const DERIVE: Self = Self(1 << 4);
    /// Permission to revoke all descendants of a capability.
    pub const REVOKE: Self = Self(1 << 5);
    /// All rights currently defined by this crate.
    pub const ALL: Self = Self((1 << 6) - 1);

    /// Constructs rights from their bit representation, rejecting undefined bits.
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns this rights set's bit representation.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns whether this rights set contains every right in `other`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns whether every right in this set is also present in `other`.
    pub const fn is_subset_of(self, other: Self) -> bool {
        other.contains(self)
    }
}

impl BitOr for Rights {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// A capability's object, rights, and badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    object: ObjectId,
    rights: Rights,
    badge: Badge,
}

impl Capability {
    /// Returns the referenced object.
    pub const fn object(self) -> ObjectId {
        self.object
    }

    /// Returns the rights granted by this capability.
    pub const fn rights(self) -> Rights {
        self.rights
    }

    /// Returns the capability's badge.
    pub const fn badge(self) -> Badge {
        self.badge
    }
}

/// Errors returned by [`CSpace`] operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CSpaceError {
    /// The requested slot is outside the CSpace.
    SlotOutOfRange,
    /// The requested slot does not contain a live capability.
    EmptySlot,
    /// A capability cannot be inserted into an occupied slot.
    OccupiedSlot,
    /// The source lacks the right required for this operation.
    MissingAuthority,
    /// Requested rights include rights the source does not have.
    RightsNotSubset,
}

struct Node {
    capability: Capability,
    slot: usize,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// A capability space with stable slots and a derivation tree.
///
/// Slots are fixed in number when the space is created, and every derivation node belongs to
/// exactly one occupied slot, so the tree never holds more nodes than the space has slots. Deleting
/// a capability splices its node out of the tree and hands its children to its parent; revoking an
/// ancestor therefore still reaches descendants whose intermediate capability was deleted.
pub struct CSpace {
    slots: Vec<Option<usize>>,
    nodes: Vec<Node>,
    free: Vec<usize>,
}

impl CSpace {
    /// Creates a CSpace with `slot_count` empty slots.
    ///
    /// Node storage is reserved up front for `slot_count` nodes, the most the space can hold.
    pub fn new(slot_count: usize) -> Self {
        Self {
            slots: vec![None; slot_count],
            nodes: Vec::with_capacity(slot_count),
            free: Vec::with_capacity(slot_count),
        }
    }

    /// Returns the number of slots in this CSpace.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Returns the number of derivation-tree nodes allocated, never more than [`Self::capacity`].
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the live capability in `slot`.
    pub fn get(&self, slot: usize) -> Result<Capability, CSpaceError> {
        let node = self.slot_node(slot)?;
        Ok(self.nodes[node].capability)
    }

    /// Installs an initial capability into an empty slot.
    pub fn insert(
        &mut self,
        slot: usize,
        object: ObjectId,
        rights: Rights,
        badge: Badge,
    ) -> Result<(), CSpaceError> {
        self.check_empty_slot(slot)?;
        self.add_node(
            Capability {
                object,
                rights,
                badge,
            },
            slot,
            None,
        );
        Ok(())
    }

    /// Copies a capability into an empty slot with a subset of its rights.
    ///
    /// The source must have [`Rights::DERIVE`]. The copy preserves the object's badge.
    pub fn copy(
        &mut self,
        source: usize,
        destination: usize,
        rights: Rights,
    ) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(source)?;
        self.check_empty_slot(destination)?;
        let source_cap = self.nodes[source_node].capability;
        if !source_cap.rights.contains(Rights::DERIVE) {
            return Err(CSpaceError::MissingAuthority);
        }
        if !rights.is_subset_of(source_cap.rights) {
            return Err(CSpaceError::RightsNotSubset);
        }
        self.add_node(
            Capability {
                rights,
                ..source_cap
            },
            destination,
            Some(source_node),
        );
        Ok(())
    }

    /// Mints a capability into an empty slot with a subset of its rights and a new badge.
    ///
    /// The source must have [`Rights::GRANT`].
    pub fn mint(
        &mut self,
        source: usize,
        destination: usize,
        rights: Rights,
        badge: Badge,
    ) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(source)?;
        self.check_empty_slot(destination)?;
        let source_cap = self.nodes[source_node].capability;
        if !source_cap.rights.contains(Rights::GRANT) {
            return Err(CSpaceError::MissingAuthority);
        }
        if !rights.is_subset_of(source_cap.rights) {
            return Err(CSpaceError::RightsNotSubset);
        }
        self.add_node(
            Capability {
                rights,
                badge,
                ..source_cap
            },
            destination,
            Some(source_node),
        );
        Ok(())
    }

    /// Removes every live descendant of the capability in `slot`, leaving that capability intact.
    ///
    /// The source must have [`Rights::REVOKE`]. Each descendant's slot is cleared and its node
    /// freed; the cost is proportional to the number of descendants.
    pub fn revoke(&mut self, slot: usize) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(slot)?;
        if !self.nodes[source_node]
            .capability
            .rights
            .contains(Rights::REVOKE)
        {
            return Err(CSpaceError::MissingAuthority);
        }

        let mut pending = core::mem::take(&mut self.nodes[source_node].children);
        while let Some(node) = pending.pop() {
            pending.append(&mut self.nodes[node].children);
            self.slots[self.nodes[node].slot] = None;
            self.nodes[node].parent = None;
            self.free.push(node);
        }
        Ok(())
    }

    /// Removes and returns a capability without revoking its descendants.
    ///
    /// The capability's node is freed and its children are re-parented to its parent, so revoking
    /// any of its ancestors still reaches them.
    pub fn delete(&mut self, slot: usize) -> Result<Capability, CSpaceError> {
        let node = self.slot_node(slot)?;
        self.slots[slot] = None;
        let parent = self.nodes[node].parent.take();
        let children = core::mem::take(&mut self.nodes[node].children);
        for &child in &children {
            self.nodes[child].parent = parent;
        }
        if let Some(parent) = parent {
            let siblings = &mut self.nodes[parent].children;
            if let Some(position) = siblings.iter().position(|&sibling| sibling == node) {
                siblings.swap_remove(position);
            }
            siblings.extend_from_slice(&children);
        }
        self.free.push(node);
        Ok(self.nodes[node].capability)
    }

    fn slot_node(&self, slot: usize) -> Result<usize, CSpaceError> {
        let node = self.slots.get(slot).ok_or(CSpaceError::SlotOutOfRange)?;
        node.ok_or(CSpaceError::EmptySlot)
    }

    fn check_empty_slot(&self, slot: usize) -> Result<(), CSpaceError> {
        match self.slots.get(slot) {
            None => Err(CSpaceError::SlotOutOfRange),
            Some(Some(_)) => Err(CSpaceError::OccupiedSlot),
            Some(None) => Ok(()),
        }
    }

    fn add_node(&mut self, capability: Capability, slot: usize, parent: Option<usize>) {
        let fresh = Node {
            capability,
            slot,
            parent,
            children: Vec::new(),
        };
        let node = match self.free.pop() {
            Some(node) => {
                // Reuse the old child vector's allocation.
                let mut children = core::mem::take(&mut self.nodes[node].children);
                children.clear();
                self.nodes[node] = Node { children, ..fresh };
                node
            }
            None => {
                self.nodes.push(fresh);
                self.nodes.len() - 1
            }
        };
        if let Some(parent) = parent {
            self.nodes[parent].children.push(node);
        }
        self.slots[slot] = Some(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn copy_and_mint_attenuate_rights() {
        let mut cspace = CSpace::new(3);
        let rights = Rights::READ | Rights::WRITE | Rights::GRANT | Rights::DERIVE;
        cspace.insert(0, 42, rights, 7).unwrap();
        cspace.copy(0, 1, Rights::READ).unwrap();
        cspace.mint(0, 2, Rights::WRITE, 99).unwrap();

        assert_eq!(
            cspace.get(1).unwrap(),
            Capability {
                object: 42,
                rights: Rights::READ,
                badge: 7,
            }
        );
        assert_eq!(cspace.get(2).unwrap().badge(), 99);
        assert_eq!(cspace.get(2).unwrap().rights(), Rights::WRITE);
    }

    #[test]
    fn revoke_clears_descendants_but_preserves_source() {
        let mut cspace = CSpace::new(4);
        let rights = Rights::READ | Rights::GRANT | Rights::DERIVE | Rights::REVOKE;
        cspace.insert(0, 1, rights, 0).unwrap();
        cspace.copy(0, 1, rights).unwrap();
        cspace.mint(1, 2, Rights::READ | Rights::DERIVE, 8).unwrap();
        cspace.copy(2, 3, Rights::READ).unwrap();

        cspace.revoke(1).unwrap();

        assert!(cspace.get(1).is_ok());
        assert!(cspace.get(2).is_err());
        assert!(cspace.get(3).is_err());
        assert!(cspace.get(0).is_ok());
    }

    #[test]
    fn failed_derivation_does_not_change_slots() {
        let mut cspace = CSpace::new(3);
        cspace
            .insert(0, 1, Rights::READ | Rights::DERIVE, 0)
            .unwrap();
        cspace.insert(2, 1, Rights::READ, 0).unwrap();

        assert_eq!(
            cspace.copy(2, 1, Rights::READ),
            Err(CSpaceError::MissingAuthority)
        );
        assert_eq!(
            cspace.copy(0, 1, Rights::READ | Rights::WRITE),
            Err(CSpaceError::RightsNotSubset)
        );
        assert_eq!(
            cspace.copy(0, 3, Rights::READ),
            Err(CSpaceError::SlotOutOfRange)
        );
        assert_eq!(cspace.get(1), Err(CSpaceError::EmptySlot));
    }

    #[test]
    fn revoking_an_ancestor_reaches_descendants_after_parent_deletion() {
        let mut cspace = CSpace::new(3);
        cspace.insert(0, 1, Rights::ALL, 0).unwrap();
        cspace.copy(0, 1, Rights::ALL).unwrap();
        cspace.copy(1, 2, Rights::READ).unwrap();
        cspace.delete(1).unwrap();

        cspace.revoke(0).unwrap();

        assert_eq!(cspace.get(2), Err(CSpaceError::EmptySlot));
    }

    #[test]
    fn copy_delete_cycles_keep_node_count_bounded() {
        let mut cspace = CSpace::new(2);
        cspace.insert(0, 1, Rights::ALL, 0).unwrap();
        for _ in 0..10_000 {
            cspace.copy(0, 1, Rights::READ).unwrap();
            cspace.delete(1).unwrap();
        }
        assert!(cspace.node_count() <= cspace.capacity());
    }

    #[test]
    fn deleting_every_intermediate_link_keeps_revocation_and_bounds() {
        let mut cspace = CSpace::new(3);
        cspace.insert(0, 1, Rights::ALL, 0).unwrap();
        cspace.copy(0, 1, Rights::ALL).unwrap();
        let (mut current, mut spare) = (1, 2);
        for _ in 0..10_000 {
            cspace.copy(current, spare, Rights::ALL).unwrap();
            cspace.delete(current).unwrap();
            core::mem::swap(&mut current, &mut spare);
        }
        assert!(cspace.node_count() <= cspace.capacity());

        cspace.revoke(0).unwrap();

        assert_eq!(cspace.get(current), Err(CSpaceError::EmptySlot));
        assert!(cspace.get(0).is_ok());
    }

    #[test]
    fn revoked_slots_can_be_reused() {
        let mut cspace = CSpace::new(3);
        cspace.insert(0, 1, Rights::ALL, 0).unwrap();
        cspace.copy(0, 1, Rights::ALL).unwrap();
        cspace.copy(1, 2, Rights::READ).unwrap();
        cspace.revoke(0).unwrap();

        cspace.insert(1, 9, Rights::READ, 3).unwrap();
        cspace.copy(0, 2, Rights::READ).unwrap();

        assert_eq!(cspace.get(1).unwrap().object(), 9);
        assert_eq!(cspace.get(2).unwrap().object(), 1);
        assert_eq!(cspace.node_count(), 3);
        cspace.revoke(0).unwrap();
        assert_eq!(cspace.get(1).unwrap().object(), 9);
        assert_eq!(cspace.get(2), Err(CSpaceError::EmptySlot));
    }

    proptest! {
        #[test]
        fn revoke_reaches_descendants_through_deleted_links(
            operations in prop::collection::vec((any::<u8>(), any::<bool>()), 0..64),
            revoke_seed in any::<u8>(),
        ) {
            const SLOTS: usize = 8;
            let mut cspace = CSpace::new(SLOTS);
            cspace.insert(0, 7, Rights::ALL, 0).unwrap();
            // Model: the nearest live ancestor slot of each live slot (`None` for the root).
            let mut live = [false; SLOTS];
            let mut parent: [Option<usize>; SLOTS] = [None; SLOTS];
            live[0] = true;

            for (seed, delete) in operations {
                let deletable: Vec<usize> = (1..SLOTS).filter(|s| live[*s]).collect();
                if delete && !deletable.is_empty() {
                    let slot = deletable[seed as usize % deletable.len()];
                    cspace.delete(slot).unwrap();
                    live[slot] = false;
                    for other in 0..SLOTS {
                        if live[other] && parent[other] == Some(slot) {
                            parent[other] = parent[slot];
                        }
                    }
                } else if let Some(destination) = (1..SLOTS).find(|s| !live[*s]) {
                    let sources: Vec<usize> = (0..SLOTS).filter(|s| live[*s]).collect();
                    let source = sources[seed as usize % sources.len()];
                    cspace.copy(source, destination, Rights::ALL).unwrap();
                    live[destination] = true;
                    parent[destination] = Some(source);
                }
                prop_assert!(cspace.node_count() <= SLOTS);
            }

            let candidates: Vec<usize> = (0..SLOTS).filter(|s| live[*s]).collect();
            let revoked = candidates[revoke_seed as usize % candidates.len()];
            let is_descendant = |mut slot: usize| {
                while let Some(up) = parent[slot] {
                    if up == revoked {
                        return true;
                    }
                    slot = up;
                }
                false
            };

            cspace.revoke(revoked).unwrap();
            for (slot, &was_live) in live.iter().enumerate() {
                if !was_live || is_descendant(slot) {
                    prop_assert_eq!(cspace.get(slot), Err(CSpaceError::EmptySlot));
                } else {
                    prop_assert!(cspace.get(slot).is_ok());
                }
            }
        }

        #[test]
        fn revoke_removes_every_descendant_and_derivation_never_grows_rights(
            operations in prop::collection::vec((any::<u8>(), any::<u8>(), any::<bool>()), 0..32),
            revoke_selector in any::<u8>(),
        ) {
            let mut cspace = CSpace::new(35);
            cspace.insert(0, 7, Rights::ALL, 0).unwrap();
            cspace.copy(0, 1, Rights::ALL).unwrap();
            cspace.copy(1, 2, Rights::READ | Rights::DERIVE).unwrap();
            cspace.copy(2, 3, Rights::READ).unwrap();
            let mut live = vec![
                (0usize, Rights::ALL),
                (1usize, Rights::ALL),
                (2usize, Rights::READ | Rights::DERIVE),
                (3usize, Rights::READ),
            ];
            let mut parents = vec![None; 35];
            parents[1] = Some(0);
            parents[2] = Some(1);
            parents[3] = Some(2);

            for (offset, (source_seed, rights_seed, mint)) in operations.into_iter().enumerate() {
                let next_slot = offset + 4;
                let (source, source_rights) = live[source_seed as usize % live.len()];
                let rights = Rights::from_bits(rights_seed & Rights::ALL.bits()).unwrap();
                let result = if mint {
                    cspace.mint(source, next_slot, rights, u64::from(rights_seed))
                } else {
                    cspace.copy(source, next_slot, rights)
                };
                if result.is_ok() {
                    prop_assert!(rights.is_subset_of(source_rights));
                    prop_assert!(cspace.get(next_slot).unwrap().rights().is_subset_of(source_rights));
                    parents[next_slot] = Some(source);
                    live.push((next_slot, rights));
                }
            }

            let revoked = usize::from(revoke_selector % 2);

            let mut descendants = Vec::new();
            for (slot, _) in &live {
                let mut ancestor = parents[*slot];
                while let Some(parent) = ancestor {
                    if parent == revoked {
                        descendants.push(*slot);
                        break;
                    }
                    ancestor = parents[parent];
                }
            }

            cspace.revoke(revoked).unwrap();
            prop_assert!(cspace.get(revoked).is_ok());
            for descendant in descendants {
                prop_assert_eq!(cspace.get(descendant), Err(CSpaceError::EmptySlot));
            }
        }
    }
}

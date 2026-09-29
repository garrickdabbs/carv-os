//! Pure capability-space and derivation-tree logic.
//!
//! A capability can be copied or minted only with a subset of its source rights. Derived
//! capabilities retain their derivation links after a slot is deleted so revoking an ancestor still
//! invalidates every live descendant.

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
    children: Vec<usize>,
    alive: bool,
}

/// A capability space with stable slots and a derivation tree.
///
/// Slots are fixed in number when the space is created. Deleting a capability frees its slot but
/// does not sever its derivation history; revoking an ancestor therefore reaches descendants even
/// when an intermediate capability was deleted.
pub struct CSpace {
    slots: Vec<Option<usize>>,
    nodes: Vec<Node>,
}

impl CSpace {
    /// Creates a CSpace with `slot_count` empty slots.
    pub fn new(slot_count: usize) -> Self {
        Self {
            slots: vec![None; slot_count],
            nodes: Vec::new(),
        }
    }

    /// Returns the number of slots in this CSpace.
    pub fn capacity(&self) -> usize {
        self.slots.len()
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
        let node = self.add_node(
            Capability {
                object,
                rights,
                badge,
            },
            None,
        );
        self.slots[slot] = Some(node);
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
        let node = self.add_node(
            Capability {
                rights,
                ..source_cap
            },
            Some(source_node),
        );
        self.slots[destination] = Some(node);
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
        let node = self.add_node(
            Capability {
                rights,
                badge,
                ..source_cap
            },
            Some(source_node),
        );
        self.slots[destination] = Some(node);
        Ok(())
    }

    /// Removes every live descendant of the capability in `slot`, leaving that capability intact.
    ///
    /// The source must have [`Rights::REVOKE`]. Each descendant's slot is cleared.
    pub fn revoke(&mut self, slot: usize) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(slot)?;
        if !self.nodes[source_node]
            .capability
            .rights
            .contains(Rights::REVOKE)
        {
            return Err(CSpaceError::MissingAuthority);
        }

        let mut pending = self.nodes[source_node].children.clone();
        while let Some(node) = pending.pop() {
            pending.extend(self.nodes[node].children.iter().copied());
            self.nodes[node].alive = false;
            for slot in &mut self.slots {
                if *slot == Some(node) {
                    *slot = None;
                }
            }
        }
        Ok(())
    }

    /// Removes and returns a capability without revoking its descendants.
    pub fn delete(&mut self, slot: usize) -> Result<Capability, CSpaceError> {
        let node = self.slot_node(slot)?;
        self.slots[slot] = None;
        Ok(self.nodes[node].capability)
    }

    fn slot_node(&self, slot: usize) -> Result<usize, CSpaceError> {
        let node = self.slots.get(slot).ok_or(CSpaceError::SlotOutOfRange)?;
        let node = node.ok_or(CSpaceError::EmptySlot)?;
        if self.nodes[node].alive {
            Ok(node)
        } else {
            Err(CSpaceError::EmptySlot)
        }
    }

    fn check_empty_slot(&self, slot: usize) -> Result<(), CSpaceError> {
        match self.slots.get(slot) {
            None => Err(CSpaceError::SlotOutOfRange),
            Some(Some(_)) => Err(CSpaceError::OccupiedSlot),
            Some(None) => Ok(()),
        }
    }

    fn add_node(&mut self, capability: Capability, parent: Option<usize>) -> usize {
        let node = self.nodes.len();
        self.nodes.push(Node {
            capability,
            children: Vec::new(),
            alive: true,
        });
        if let Some(parent) = parent {
            self.nodes[parent].children.push(node);
        }
        node
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

    proptest! {
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

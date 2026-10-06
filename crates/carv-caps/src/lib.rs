//! Pure capability-space and shared derivation-tree logic.
//!
//! [`CapSpaces`] owns every CSpace in the system plus one shared derivation-tree arena. Copies,
//! mints, and cross-CSpace transfers all add child nodes to that shared tree, so revoking a
//! capability removes its descendants no matter which CSpace currently stores them. Deleting a
//! capability hands its derived capabilities to its own parent, so revoking an ancestor still
//! invalidates every live descendant, and live node count never exceeds the number of occupied
//! slots.

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

/// Index of a CSpace inside a [`CapSpaces`].
pub type SpaceId = usize;

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

/// Errors returned by [`CSpace`] and [`CapSpaces`] operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CSpaceError {
    /// The requested CSpace does not exist.
    NoSuchSpace,
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

struct Space {
    slots: Vec<Option<usize>>,
}

struct Node {
    capability: Capability,
    space: SpaceId,
    slot: usize,
    parent: Option<usize>,
    first_child: Option<usize>,
    next_sibling: Option<usize>,
    live: bool,
}

/// All CSpaces in a system, backed by one shared capability derivation tree.
pub struct CapSpaces {
    spaces: Vec<Option<Space>>,
    free_spaces: Vec<SpaceId>,
    nodes: Vec<Node>,
    free_nodes: Vec<usize>,
    pending: Vec<usize>,
    live_nodes: usize,
}

impl Default for CapSpaces {
    fn default() -> Self {
        Self::new()
    }
}

impl CapSpaces {
    /// Creates an empty set of capability spaces.
    pub const fn new() -> Self {
        Self {
            spaces: Vec::new(),
            free_spaces: Vec::new(),
            nodes: Vec::new(),
            free_nodes: Vec::new(),
            pending: Vec::new(),
            live_nodes: 0,
        }
    }

    /// Creates a new CSpace with `slot_count` slots and returns its reusable identifier.
    pub fn create_space(&mut self, slot_count: usize) -> SpaceId {
        let space = Space {
            slots: vec![None; slot_count],
        };
        if let Some(id) = self.free_spaces.pop() {
            self.spaces[id] = Some(space);
            id
        } else {
            self.spaces.push(Some(space));
            self.spaces.len() - 1
        }
    }

    /// Deletes every capability in `space` and frees its identifier for reuse.
    pub fn destroy_space(&mut self, space: SpaceId) -> Result<(), CSpaceError> {
        let nodes = self
            .space(space)?
            .slots
            .iter()
            .filter_map(|slot| *slot)
            .collect::<Vec<_>>();
        for node in nodes {
            if self.nodes[node].live {
                self.delete_node(node);
            }
        }
        self.spaces[space] = None;
        self.free_spaces.push(space);
        Ok(())
    }

    /// Returns the number of slots in `space`.
    pub fn capacity(&self, space: SpaceId) -> Result<usize, CSpaceError> {
        Ok(self.space(space)?.slots.len())
    }

    /// Returns the number of live derivation-tree nodes across all spaces.
    pub const fn node_count(&self) -> usize {
        self.live_nodes
    }

    /// Returns the live capability in `slot` of `space`.
    pub fn get(&self, space: SpaceId, slot: usize) -> Result<Capability, CSpaceError> {
        let node = self.slot_node(space, slot)?;
        Ok(self.nodes[node].capability)
    }

    /// Returns the first empty slot in `space`, or `None` if the space is absent or full.
    pub fn first_empty_slot(&self, space: SpaceId) -> Option<usize> {
        self.space(space)
            .ok()?
            .slots
            .iter()
            .position(Option::is_none)
    }

    /// Installs a root capability into an empty slot.
    pub fn insert(
        &mut self,
        space: SpaceId,
        slot: usize,
        object: ObjectId,
        rights: Rights,
        badge: Badge,
    ) -> Result<(), CSpaceError> {
        self.check_empty_slot(space, slot)?;
        self.add_node(
            space,
            slot,
            Capability {
                object,
                rights,
                badge,
            },
            None,
        );
        Ok(())
    }

    /// Copies a capability within one CSpace into an empty slot with a subset of its rights.
    pub fn copy(
        &mut self,
        space: SpaceId,
        source: usize,
        destination: usize,
        rights: Rights,
    ) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(space, source)?;
        self.check_empty_slot(space, destination)?;
        let source_cap = self.nodes[source_node].capability;
        if !source_cap.rights.contains(Rights::DERIVE) {
            return Err(CSpaceError::MissingAuthority);
        }
        if !rights.is_subset_of(source_cap.rights) {
            return Err(CSpaceError::RightsNotSubset);
        }
        self.add_node(
            space,
            destination,
            Capability {
                rights,
                ..source_cap
            },
            Some(source_node),
        );
        Ok(())
    }

    /// Mints a capability within one CSpace into an empty slot with a subset of its rights.
    pub fn mint(
        &mut self,
        space: SpaceId,
        source: usize,
        destination: usize,
        rights: Rights,
        badge: Badge,
    ) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(space, source)?;
        self.check_empty_slot(space, destination)?;
        let source_cap = self.nodes[source_node].capability;
        if !source_cap.rights.contains(Rights::GRANT) {
            return Err(CSpaceError::MissingAuthority);
        }
        if !rights.is_subset_of(source_cap.rights) {
            return Err(CSpaceError::RightsNotSubset);
        }
        self.add_node(
            space,
            destination,
            Capability {
                rights,
                badge,
                ..source_cap
            },
            Some(source_node),
        );
        Ok(())
    }

    /// Transfers a capability between CSpaces as a child of the source node.
    pub fn transfer(
        &mut self,
        from: (SpaceId, usize),
        to: (SpaceId, usize),
        rights: Rights,
    ) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(from.0, from.1)?;
        self.check_empty_slot(to.0, to.1)?;
        let source_cap = self.nodes[source_node].capability;
        if !source_cap.rights.contains(Rights::GRANT) {
            return Err(CSpaceError::MissingAuthority);
        }
        if !rights.is_subset_of(source_cap.rights) {
            return Err(CSpaceError::RightsNotSubset);
        }
        self.add_node(
            to.0,
            to.1,
            Capability {
                rights,
                ..source_cap
            },
            Some(source_node),
        );
        Ok(())
    }

    /// Removes every live descendant of a capability across every CSpace.
    pub fn revoke(&mut self, space: SpaceId, slot: usize) -> Result<(), CSpaceError> {
        let source_node = self.slot_node(space, slot)?;
        if !self.nodes[source_node]
            .capability
            .rights
            .contains(Rights::REVOKE)
        {
            return Err(CSpaceError::MissingAuthority);
        }

        self.pending.clear();
        let mut child = self.nodes[source_node].first_child.take();
        while let Some(node) = child {
            child = self.nodes[node].next_sibling;
            self.pending.push(node);
        }
        while let Some(node) = self.pending.pop() {
            let mut child = self.nodes[node].first_child.take();
            while let Some(descendant) = child {
                child = self.nodes[descendant].next_sibling;
                self.pending.push(descendant);
            }
            self.clear_node_slot(node);
            self.nodes[node].parent = None;
            self.nodes[node].next_sibling = None;
            self.free_node(node);
        }
        Ok(())
    }

    /// Removes and returns a capability without revoking its descendants.
    pub fn delete(&mut self, space: SpaceId, slot: usize) -> Result<Capability, CSpaceError> {
        let node = self.slot_node(space, slot)?;
        Ok(self.delete_node(node))
    }

    /// Deletes every capability in every space whose object is `object`.
    pub fn delete_object(&mut self, object: ObjectId) -> usize {
        let mut removed = 0;
        while let Some(node) = self
            .nodes
            .iter()
            .position(|node| node.live && node.capability.object == object)
        {
            self.delete_node(node);
            removed += 1;
        }
        removed
    }

    fn space(&self, space: SpaceId) -> Result<&Space, CSpaceError> {
        self.spaces
            .get(space)
            .and_then(Option::as_ref)
            .ok_or(CSpaceError::NoSuchSpace)
    }

    fn space_mut(&mut self, space: SpaceId) -> Result<&mut Space, CSpaceError> {
        self.spaces
            .get_mut(space)
            .and_then(Option::as_mut)
            .ok_or(CSpaceError::NoSuchSpace)
    }

    fn slot_node(&self, space: SpaceId, slot: usize) -> Result<usize, CSpaceError> {
        let slot = self
            .space(space)?
            .slots
            .get(slot)
            .ok_or(CSpaceError::SlotOutOfRange)?;
        slot.ok_or(CSpaceError::EmptySlot)
    }

    fn check_empty_slot(&self, space: SpaceId, slot: usize) -> Result<(), CSpaceError> {
        match self.space(space)?.slots.get(slot) {
            None => Err(CSpaceError::SlotOutOfRange),
            Some(Some(_)) => Err(CSpaceError::OccupiedSlot),
            Some(None) => Ok(()),
        }
    }

    fn add_node(
        &mut self,
        space: SpaceId,
        slot: usize,
        capability: Capability,
        parent: Option<usize>,
    ) {
        let fresh = Node {
            capability,
            space,
            slot,
            parent,
            first_child: None,
            next_sibling: None,
            live: true,
        };
        let node = match self.free_nodes.pop() {
            Some(node) => {
                self.nodes[node] = fresh;
                node
            }
            None => {
                self.nodes.push(fresh);
                self.nodes.len() - 1
            }
        };
        self.live_nodes += 1;
        if let Some(parent) = parent {
            self.nodes[node].next_sibling = self.nodes[parent].first_child;
            self.nodes[parent].first_child = Some(node);
        }
        self.space_mut(space).expect("space was validated").slots[slot] = Some(node);
    }

    fn delete_node(&mut self, node: usize) -> Capability {
        self.clear_node_slot(node);
        let capability = self.nodes[node].capability;
        let parent = self.nodes[node].parent.take();
        let children = self.nodes[node].first_child.take();
        let next_sibling = self.nodes[node].next_sibling;
        if let Some(parent) = parent {
            let mut previous = None;
            let mut sibling = self.nodes[parent].first_child;
            while let Some(index) = sibling {
                if index == node {
                    break;
                }
                previous = Some(index);
                sibling = self.nodes[index].next_sibling;
            }
            assert_eq!(sibling, Some(node));
            let replacement = if let Some(first_child) = children {
                let mut child = first_child;
                loop {
                    self.nodes[child].parent = Some(parent);
                    match self.nodes[child].next_sibling {
                        Some(next) => child = next,
                        None => {
                            self.nodes[child].next_sibling = next_sibling;
                            break;
                        }
                    }
                }
                Some(first_child)
            } else {
                next_sibling
            };
            if let Some(previous) = previous {
                self.nodes[previous].next_sibling = replacement;
            } else {
                self.nodes[parent].first_child = replacement;
            }
        } else if let Some(first_child) = children {
            let mut child = first_child;
            loop {
                self.nodes[child].parent = None;
                let next = self.nodes[child].next_sibling.take();
                let Some(next) = next else {
                    break;
                };
                child = next;
            }
        }
        self.nodes[node].next_sibling = None;
        self.free_node(node);
        capability
    }

    fn clear_node_slot(&mut self, node: usize) {
        let space = self.nodes[node].space;
        let slot = self.nodes[node].slot;
        if let Some(Some(space)) = self.spaces.get_mut(space) {
            space.slots[slot] = None;
        }
    }

    fn free_node(&mut self, node: usize) {
        self.nodes[node].live = false;
        self.free_nodes.push(node);
        self.live_nodes -= 1;
    }
}

/// A single capability space with stable slots and a derivation tree.
///
/// This is a compatibility wrapper over [`CapSpaces`] containing exactly one CSpace.
pub struct CSpace {
    spaces: CapSpaces,
    space: SpaceId,
}

impl CSpace {
    /// Creates a CSpace with `slot_count` empty slots.
    pub fn new(slot_count: usize) -> Self {
        let mut spaces = CapSpaces::new();
        let space = spaces.create_space(slot_count);
        Self { spaces, space }
    }

    /// Returns the number of slots in this CSpace.
    pub fn capacity(&self) -> usize {
        self.spaces
            .capacity(self.space)
            .expect("single CSpace is always present")
    }

    /// Returns the number of live derivation-tree nodes, never more than [`Self::capacity`].
    pub fn node_count(&self) -> usize {
        self.spaces.node_count()
    }

    /// Returns the live capability in `slot`.
    pub fn get(&self, slot: usize) -> Result<Capability, CSpaceError> {
        self.spaces.get(self.space, slot)
    }

    /// Returns the first empty slot, or `None` if the CSpace is full.
    pub fn first_empty_slot(&self) -> Option<usize> {
        self.spaces.first_empty_slot(self.space)
    }

    /// Installs an initial capability into an empty slot.
    pub fn insert(
        &mut self,
        slot: usize,
        object: ObjectId,
        rights: Rights,
        badge: Badge,
    ) -> Result<(), CSpaceError> {
        self.spaces.insert(self.space, slot, object, rights, badge)
    }

    /// Copies a capability into an empty slot with a subset of its rights.
    pub fn copy(
        &mut self,
        source: usize,
        destination: usize,
        rights: Rights,
    ) -> Result<(), CSpaceError> {
        self.spaces.copy(self.space, source, destination, rights)
    }

    /// Mints a capability into an empty slot with a subset of its rights and a new badge.
    pub fn mint(
        &mut self,
        source: usize,
        destination: usize,
        rights: Rights,
        badge: Badge,
    ) -> Result<(), CSpaceError> {
        self.spaces
            .mint(self.space, source, destination, rights, badge)
    }

    /// Removes every live descendant of the capability in `slot`, leaving that capability intact.
    pub fn revoke(&mut self, slot: usize) -> Result<(), CSpaceError> {
        self.spaces.revoke(self.space, slot)
    }

    /// Removes and returns a capability without revoking its descendants.
    pub fn delete(&mut self, slot: usize) -> Result<Capability, CSpaceError> {
        self.spaces.delete(self.space, slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const FULL_AUTHORITY: Rights = Rights::ALL;

    #[test]
    fn capspaces_methods_and_failures_are_reported() {
        let mut spaces = CapSpaces::new();
        let left = spaces.create_space(3);
        let right = spaces.create_space(2);

        assert_eq!(spaces.capacity(left), Ok(3));
        assert_eq!(spaces.capacity(99), Err(CSpaceError::NoSuchSpace));
        assert_eq!(spaces.first_empty_slot(left), Some(0));
        assert_eq!(spaces.first_empty_slot(99), None);
        assert_eq!(spaces.get(left, 0), Err(CSpaceError::EmptySlot));
        assert_eq!(spaces.get(99, 0), Err(CSpaceError::NoSuchSpace));
        assert_eq!(
            spaces.insert(left, 9, 1, Rights::READ, 0),
            Err(CSpaceError::SlotOutOfRange)
        );

        spaces.insert(left, 0, 42, FULL_AUTHORITY, 7).unwrap();
        assert_eq!(spaces.node_count(), 1);
        assert_eq!(spaces.first_empty_slot(left), Some(1));
        assert_eq!(
            spaces.insert(left, 0, 1, Rights::READ, 0),
            Err(CSpaceError::OccupiedSlot)
        );
        assert_eq!(spaces.copy(left, 0, 1, Rights::READ), Ok(()));
        assert_eq!(spaces.mint(left, 0, 2, Rights::WRITE, 99), Ok(()));
        assert_eq!(spaces.transfer((left, 0), (right, 0), Rights::EXEC), Ok(()));
        assert_eq!(
            spaces.transfer((left, 0), (left, 1), Rights::READ),
            Err(CSpaceError::OccupiedSlot)
        );
        assert_eq!(
            spaces.transfer((right, 0), (right, 1), Rights::EXEC),
            Err(CSpaceError::MissingAuthority)
        );
        assert_eq!(spaces.transfer((left, 0), (right, 1), Rights::ALL), Ok(()));
        assert_eq!(spaces.get(right, 1).unwrap().badge(), 7);
        assert_eq!(spaces.get(right, 1).unwrap().rights(), Rights::ALL);
    }

    #[test]
    fn transfer_revoke_crosses_spaces_and_same_space() {
        let mut spaces = CapSpaces::new();
        let a = spaces.create_space(4);
        let b = spaces.create_space(2);
        spaces.insert(a, 0, 5, FULL_AUTHORITY, 11).unwrap();
        spaces
            .transfer(
                (a, 0),
                (b, 0),
                Rights::READ | Rights::GRANT | Rights::REVOKE,
            )
            .unwrap();
        spaces.transfer((b, 0), (a, 1), Rights::READ).unwrap();
        spaces.transfer((a, 0), (a, 2), Rights::WRITE).unwrap();

        spaces.revoke(a, 0).unwrap();

        assert!(spaces.get(a, 0).is_ok());
        assert_eq!(spaces.get(b, 0), Err(CSpaceError::EmptySlot));
        assert_eq!(spaces.get(a, 1), Err(CSpaceError::EmptySlot));
        assert_eq!(spaces.get(a, 2), Err(CSpaceError::EmptySlot));
        assert_eq!(spaces.node_count(), 1);
    }

    #[test]
    fn destroy_space_deletes_caps_reparents_children_and_reuses_id() {
        let mut spaces = CapSpaces::new();
        let parent = spaces.create_space(2);
        let doomed = spaces.create_space(2);
        spaces.insert(parent, 0, 7, FULL_AUTHORITY, 0).unwrap();
        spaces
            .transfer((parent, 0), (doomed, 0), FULL_AUTHORITY)
            .unwrap();
        spaces
            .transfer((doomed, 0), (parent, 1), Rights::READ)
            .unwrap();

        spaces.destroy_space(doomed).unwrap();
        let reused = spaces.create_space(1);

        assert_eq!(reused, doomed);
        assert_eq!(spaces.capacity(doomed), Ok(1));
        assert!(spaces.get(parent, 1).is_ok());
        spaces.revoke(parent, 0).unwrap();
        assert_eq!(spaces.get(parent, 1), Err(CSpaceError::EmptySlot));
    }

    #[test]
    fn delete_object_removes_matching_caps_in_all_spaces() {
        let mut spaces = CapSpaces::new();
        let a = spaces.create_space(3);
        let b = spaces.create_space(2);
        spaces.insert(a, 0, 1, FULL_AUTHORITY, 0).unwrap();
        spaces.insert(a, 1, 2, FULL_AUTHORITY, 0).unwrap();
        spaces.transfer((a, 0), (b, 0), Rights::READ).unwrap();
        spaces.transfer((a, 1), (b, 1), Rights::READ).unwrap();

        assert_eq!(spaces.delete_object(1), 2);
        assert_eq!(spaces.get(a, 0), Err(CSpaceError::EmptySlot));
        assert_eq!(spaces.get(b, 0), Err(CSpaceError::EmptySlot));
        assert!(spaces.get(a, 1).is_ok());
        assert!(spaces.get(b, 1).is_ok());
        assert_eq!(spaces.delete_object(99), 0);
    }

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
    fn broad_revocation_uses_reserved_storage() {
        const SLOTS: usize = 256;
        let mut cspace = CSpace::new(SLOTS);
        cspace.insert(0, 1, Rights::ALL, 0).unwrap();
        for slot in 1..SLOTS {
            cspace.copy(0, slot, Rights::READ).unwrap();
        }

        cspace.revoke(0).unwrap();

        assert_eq!(cspace.node_count(), 1);
        for slot in 1..SLOTS {
            assert_eq!(cspace.get(slot), Err(CSpaceError::EmptySlot));
        }
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

    fn assert_rights_never_grow(spaces: &CapSpaces) {
        for node in &spaces.nodes {
            if node.live
                && let Some(parent) = node.parent
            {
                assert!(
                    node.capability
                        .rights()
                        .is_subset_of(spaces.nodes[parent].capability.rights())
                );
            }
        }
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
        fn multi_space_random_derivations_revoke_globally_and_attenuate(
            operations in prop::collection::vec((any::<u8>(), any::<u8>(), any::<u8>(), any::<bool>()), 0..96),
            revoke_seed in any::<u8>(),
        ) {
            const SPACES: usize = 3;
            const SLOTS: usize = 8;
            let mut spaces = CapSpaces::new();
            let ids = [
                spaces.create_space(SLOTS),
                spaces.create_space(SLOTS),
                spaces.create_space(SLOTS),
            ];
            spaces.insert(ids[0], 0, 7, Rights::ALL, 0).unwrap();

            for (a, b, c, delete) in operations {
                let rights = Rights::from_bits(c & Rights::ALL.bits()).unwrap();
                if delete {
                    let live: Vec<(SpaceId, usize)> = ids
                        .iter()
                        .flat_map(|space| (0..SLOTS).map(move |slot| (*space, slot)))
                        .filter(|(space, slot)| spaces.get(*space, *slot).is_ok() && !(*space == ids[0] && *slot == 0))
                        .collect();
                    if let Some(target) = live.get(a as usize % live.len().max(1)) {
                        let _ = spaces.delete(target.0, target.1);
                    }
                } else {
                    let live: Vec<(SpaceId, usize)> = ids
                        .iter()
                        .flat_map(|space| (0..SLOTS).map(move |slot| (*space, slot)))
                        .filter(|(space, slot)| spaces.get(*space, *slot).is_ok())
                        .collect();
                    let empties: Vec<(SpaceId, usize)> = ids
                        .iter()
                        .flat_map(|space| (0..SLOTS).map(move |slot| (*space, slot)))
                        .filter(|(space, slot)| spaces.get(*space, *slot) == Err(CSpaceError::EmptySlot))
                        .collect();
                    if !live.is_empty() && !empties.is_empty() {
                        let source = live[a as usize % live.len()];
                        let destination = empties[b as usize % empties.len()];
                        match c % 3 {
                            0 if source.0 == destination.0 => {
                                let _ = spaces.copy(source.0, source.1, destination.1, rights);
                            }
                            1 if source.0 == destination.0 => {
                                let _ = spaces.mint(source.0, source.1, destination.1, rights, u64::from(c));
                            }
                            _ => {
                                let _ = spaces.transfer(source, destination, rights);
                            }
                        }
                    }
                }
                assert_rights_never_grow(&spaces);
                prop_assert!(spaces.node_count() <= SPACES * SLOTS);
            }

            let revokers: Vec<usize> = spaces
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| node.live && node.capability.rights().contains(Rights::REVOKE))
                .map(|(node, _)| node)
                .collect();
            let revoked = revokers[revoke_seed as usize % revokers.len()];
            let descendants: Vec<usize> = spaces
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| node.live)
                .filter_map(|(node, _)| {
                    let mut parent = spaces.nodes[node].parent;
                    while let Some(up) = parent {
                        if up == revoked {
                            return Some(node);
                        }
                        parent = spaces.nodes[up].parent;
                    }
                    None
                })
                .collect();
            let revoke_space = spaces.nodes[revoked].space;
            let revoke_slot = spaces.nodes[revoked].slot;

            spaces.revoke(revoke_space, revoke_slot).unwrap();

            for descendant in descendants {
                prop_assert!(!spaces.nodes[descendant].live);
            }
            assert_rights_never_grow(&spaces);
        }
    }
}

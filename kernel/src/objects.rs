//! Kernel object storage and the object-local part of `invoke`.
//!
//! This is deliberately independent of the syscall ABI.  The syscall dispatcher can translate
//! ABI method numbers into [`InvokeMethod`] once that ABI is available, while this module owns
//! object identity, lifetime, and budget accounting.

use alloc::collections::BTreeMap;

use crate::mm::frame;

/// Stable identity for an object while it is present in a registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectId(u64);

impl ObjectId {
    /// Returns the numeric identity, useful when adapting to an ABI handle.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The kernel object types supported by the initial registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectType {
    /// Execution context.
    Thread,
    /// Page-table root.
    AddressSpace,
    /// Physical memory range.
    Frame,
    /// Synchronous IPC rendezvous.
    Endpoint,
    /// Asynchronous signal word.
    Notification,
    /// Resource accounting object.
    Budget,
    /// One-shot IPC reply.
    Reply,
}

/// Operations that can be dispatched by a future syscall `invoke` implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvokeMethod {
    /// Return the object's type and budget charge.
    Describe,
    /// Destroy the object and credit its charge.
    Destroy,
    /// Read budget counters; valid only for a [`ObjectType::Budget`].
    ReadBudget,
}

/// Result of dispatching an [`InvokeMethod`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvokeResult {
    /// Description of a live object.
    Description {
        /// Object type.
        object_type: ObjectType,
        /// Budget charged for this object.
        charged_bytes: usize,
    },
    /// The object was destroyed.
    Destroyed,
    /// Current and maximum memory counters for a budget.
    Budget {
        /// Bytes currently charged.
        used_bytes: usize,
        /// Maximum bytes this budget may charge.
        limit_bytes: usize,
    },
}

/// Errors returned by registry operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectError {
    /// The requested object does not exist.
    NotFound,
    /// The object's budget cannot pay the requested charge.
    BudgetExceeded,
    /// The requested method is not implemented by this object type.
    InvalidMethod,
    /// The root budget cannot be destroyed.
    RootBudget,
}

const THREAD_BYTES: usize = 256;
const ADDRESS_SPACE_BYTES: usize = 128;
const FRAME_BYTES: usize = 4096;
const ENDPOINT_BYTES: usize = 128;
const NOTIFICATION_BYTES: usize = 64;
const BUDGET_BYTES: usize = 128;
const REPLY_BYTES: usize = 64;

/// Resource budget tracked by the object registry.
#[derive(Debug)]
pub struct BudgetObject {
    used_bytes: usize,
    limit_bytes: usize,
}

impl BudgetObject {
    /// Creates an empty budget with `limit_bytes` available.
    pub const fn new(limit_bytes: usize) -> Self {
        Self {
            used_bytes: 0,
            limit_bytes,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), ObjectError> {
        let next = self
            .used_bytes
            .checked_add(bytes)
            .ok_or(ObjectError::BudgetExceeded)?;
        if next > self.limit_bytes {
            return Err(ObjectError::BudgetExceeded);
        }
        self.used_bytes = next;
        Ok(())
    }

    fn credit(&mut self, bytes: usize) {
        self.used_bytes = self.used_bytes.saturating_sub(bytes);
    }
}

struct FrameObject {
    frame: frame::Frame,
}

impl Drop for FrameObject {
    fn drop(&mut self) {
        frame::free(self.frame);
    }
}

enum Object {
    Thread,
    AddressSpace,
    Frame(FrameObject),
    Endpoint,
    Notification,
    Budget(BudgetObject),
    Reply,
}

impl Object {
    fn object_type(&self) -> ObjectType {
        match self {
            Self::Thread => ObjectType::Thread,
            Self::AddressSpace => ObjectType::AddressSpace,
            Self::Frame(_) => ObjectType::Frame,
            Self::Endpoint => ObjectType::Endpoint,
            Self::Notification => ObjectType::Notification,
            Self::Budget(_) => ObjectType::Budget,
            Self::Reply => ObjectType::Reply,
        }
    }

    fn budget(&self) -> Option<&BudgetObject> {
        match self {
            Self::Budget(budget) => Some(budget),
            _ => None,
        }
    }
}

struct Entry {
    object: Object,
    budget: ObjectId,
    charged_bytes: usize,
}

/// Registry of live kernel objects.
///
/// The registry itself has no global authority and is intended to be held by the future kernel
/// object manager.  Every non-root object has an owning budget; removing it credits that budget.
pub struct ObjectRegistry {
    next_id: u64,
    root_budget: ObjectId,
    entries: BTreeMap<ObjectId, Entry>,
}

impl ObjectRegistry {
    /// Creates a registry with one uncharged root budget.
    pub fn new(root_limit_bytes: usize) -> Self {
        let root_budget = ObjectId(1);
        let mut entries = BTreeMap::new();
        entries.insert(
            root_budget,
            Entry {
                object: Object::Budget(BudgetObject::new(root_limit_bytes)),
                budget: root_budget,
                charged_bytes: 0,
            },
        );
        Self {
            next_id: 2,
            root_budget,
            entries,
        }
    }

    /// Returns the registry's root budget, which may create child budgets and pay for objects.
    pub const fn root_budget(&self) -> ObjectId {
        self.root_budget
    }

    /// Creates a child budget charged to `parent`.
    pub fn create_budget(
        &mut self,
        parent: ObjectId,
        limit_bytes: usize,
    ) -> Result<ObjectId, ObjectError> {
        self.charge(parent, BUDGET_BYTES)?;
        let id = self.insert(
            Object::Budget(BudgetObject::new(limit_bytes)),
            parent,
            BUDGET_BYTES,
        );
        Ok(id)
    }

    /// Creates one kernel object and charges its owning budget.
    pub fn create(
        &mut self,
        object_type: ObjectType,
        budget: ObjectId,
    ) -> Result<ObjectId, ObjectError> {
        let bytes = object_bytes(object_type);
        self.charge(budget, bytes)?;
        let object = match object_type {
            ObjectType::Thread => Object::Thread,
            ObjectType::AddressSpace => Object::AddressSpace,
            ObjectType::Frame => Object::Frame(FrameObject {
                frame: frame::allocate().ok_or_else(|| {
                    self.credit(budget, bytes);
                    ObjectError::BudgetExceeded
                })?,
            }),
            ObjectType::Endpoint => Object::Endpoint,
            ObjectType::Notification => Object::Notification,
            ObjectType::Budget => Object::Budget(BudgetObject::new(0)),
            ObjectType::Reply => Object::Reply,
        };
        Ok(self.insert(object, budget, bytes))
    }

    /// Dispatches an invoke-facing operation against a live object.
    pub fn invoke(
        &mut self,
        object: ObjectId,
        method: InvokeMethod,
    ) -> Result<InvokeResult, ObjectError> {
        let entry = self.entries.get(&object).ok_or(ObjectError::NotFound)?;
        match method {
            InvokeMethod::Describe => Ok(InvokeResult::Description {
                object_type: entry.object.object_type(),
                charged_bytes: entry.charged_bytes,
            }),
            InvokeMethod::ReadBudget => entry
                .object
                .budget()
                .map(|budget| InvokeResult::Budget {
                    used_bytes: budget.used_bytes,
                    limit_bytes: budget.limit_bytes,
                })
                .ok_or(ObjectError::InvalidMethod),
            InvokeMethod::Destroy => {
                if object == self.root_budget {
                    return Err(ObjectError::RootBudget);
                }
                self.destroy(object)?;
                Ok(InvokeResult::Destroyed)
            }
        }
    }

    /// Destroys an object and credits the budget that paid for it.
    pub fn destroy(&mut self, object: ObjectId) -> Result<(), ObjectError> {
        if self.entries.values().any(|entry| entry.budget == object) {
            return Err(ObjectError::InvalidMethod);
        }
        let entry = self.entries.remove(&object).ok_or(ObjectError::NotFound)?;
        self.credit(entry.budget, entry.charged_bytes);
        drop(entry);
        Ok(())
    }

    fn insert(&mut self, object: Object, budget: ObjectId, charged_bytes: usize) -> ObjectId {
        let id = ObjectId(self.next_id);
        self.next_id += 1;
        self.entries.insert(
            id,
            Entry {
                object,
                budget,
                charged_bytes,
            },
        );
        id
    }

    fn charge(&mut self, budget: ObjectId, bytes: usize) -> Result<(), ObjectError> {
        let entry = self.entries.get_mut(&budget).ok_or(ObjectError::NotFound)?;
        match &mut entry.object {
            Object::Budget(budget) => budget.charge(bytes),
            _ => Err(ObjectError::InvalidMethod),
        }
    }

    fn credit(&mut self, budget: ObjectId, bytes: usize) {
        if let Some(Entry {
            object: Object::Budget(budget),
            ..
        }) = self.entries.get_mut(&budget)
        {
            budget.credit(bytes);
        }
    }
}

fn object_bytes(object_type: ObjectType) -> usize {
    match object_type {
        ObjectType::Thread => THREAD_BYTES,
        ObjectType::AddressSpace => ADDRESS_SPACE_BYTES,
        ObjectType::Frame => FRAME_BYTES,
        ObjectType::Endpoint => ENDPOINT_BYTES,
        ObjectType::Notification => NOTIFICATION_BYTES,
        ObjectType::Budget => BUDGET_BYTES,
        ObjectType::Reply => REPLY_BYTES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn registry_invokes_and_credits_every_object_type() {
        let mut registry = ObjectRegistry::new(64 * 1024);
        let budget = registry
            .create_budget(registry.root_budget(), 64 * 1024)
            .expect("child budget");
        assert!(matches!(
            registry.invoke(budget, InvokeMethod::Describe),
            Ok(InvokeResult::Description {
                object_type: ObjectType::Budget,
                charged_bytes: BUDGET_BYTES
            })
        ));
        let types = [
            ObjectType::Thread,
            ObjectType::AddressSpace,
            ObjectType::Frame,
            ObjectType::Endpoint,
            ObjectType::Notification,
            ObjectType::Reply,
        ];
        let mut objects = alloc::vec::Vec::new();
        for object_type in types {
            let object = registry.create(object_type, budget).expect("object");
            assert_eq!(
                registry.invoke(object, InvokeMethod::Describe),
                Ok(InvokeResult::Description {
                    object_type,
                    charged_bytes: object_bytes(object_type),
                })
            );
            objects.push(object);
        }
        let before = registry
            .invoke(budget, InvokeMethod::ReadBudget)
            .expect("budget stats");
        for object in objects {
            assert_eq!(
                registry.invoke(object, InvokeMethod::Destroy),
                Ok(InvokeResult::Destroyed)
            );
        }
        assert_eq!(
            registry.invoke(budget, InvokeMethod::Destroy),
            Ok(InvokeResult::Destroyed)
        );
        let after = registry
            .invoke(registry.root_budget(), InvokeMethod::ReadBudget)
            .expect("root budget stats");
        assert_eq!(
            after,
            InvokeResult::Budget {
                used_bytes: 0,
                limit_bytes: 64 * 1024
            }
        );
        assert_ne!(before, after);
    }

    #[test_case]
    fn registry_rejects_unfunded_objects_and_dangling_budget_destroy() {
        let mut registry = ObjectRegistry::new(BUDGET_BYTES);
        let budget = registry
            .create_budget(registry.root_budget(), 0)
            .expect("budget fits exactly");
        assert_eq!(
            registry.create(ObjectType::Thread, budget),
            Err(ObjectError::BudgetExceeded)
        );
        let object = registry.create(ObjectType::Reply, registry.root_budget());
        assert!(
            object.is_err(),
            "root budget was exhausted by the child budget"
        );
        assert_eq!(
            registry.destroy(budget),
            Ok(()),
            "an empty child budget can be reclaimed"
        );
    }

    #[test_case]
    fn registry_does_not_destroy_budget_with_live_objects() {
        let mut registry = ObjectRegistry::new(64 * 1024);
        let budget = registry
            .create_budget(registry.root_budget(), 64 * 1024)
            .expect("child budget");
        let object = registry.create(ObjectType::Reply, budget).expect("reply");
        assert_eq!(registry.destroy(budget), Err(ObjectError::InvalidMethod));
        registry.destroy(object).expect("reply destroy");
        registry.destroy(budget).expect("budget destroy");
    }
}

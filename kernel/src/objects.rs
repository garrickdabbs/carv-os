//! Kernel objects, their storage, and budget accounting (P2.4).
//!
//! Every kernel object lives in the [`ObjectRegistry`] and is reached by user code only through a
//! capability in its CSpace (`carv_caps::CapSpaces`); the capability's object id is the registry
//! key. Each object is paid for by a Budget object: creating it charges the budget's memory
//! account (`carv_budget::Budget::charge_memory`) and destroying it credits the charge back.
//! Child budgets are carved out of their parent with `carv_budget::Budget::carve_out`, so a child
//! can never hold more CPU time or memory than its parent had uncommitted, and destroying a child
//! returns its reservation (`release_child`).
//!
//! The registry, the CSpaces and the scheduler together form the [`Kernel`] state, kept behind
//! one lock that is only taken with interrupts disabled ([`with`]).

use alloc::collections::BTreeMap;

use carv_abi::ObjectType;
use carv_budget::Budget;
use carv_caps::CapSpaces;

use crate::ipc::{Endpoint, IrqLine, Notification};
use crate::mm::address_space::AddressSpace;
use crate::mm::frame;
use crate::scheduler::Scheduler;
use crate::sync::{SpinLock, without_interrupts};

/// Stable identity of a registry object; also the object id stored in capabilities.
pub type ObjectId = u64;

/// Errors returned by registry operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectError {
    /// The requested object does not exist.
    NotFound,
    /// The paying budget cannot cover the charge.
    BudgetExceeded,
    /// The object has the wrong type for the operation, or the operation is not allowed on it.
    InvalidMethod,
    /// The root budget cannot be destroyed.
    RootBudget,
    /// The object is still in use (a budget paying for live objects, an address space a thread
    /// runs in).
    InUse,
    /// Frames ran out.
    OutOfMemory,
}

impl From<ObjectError> for carv_abi::Error {
    fn from(e: ObjectError) -> Self {
        match e {
            ObjectError::NotFound => Self::InvalidCapability,
            ObjectError::BudgetExceeded => Self::BudgetExhausted,
            ObjectError::InvalidMethod | ObjectError::RootBudget | ObjectError::InUse => {
                Self::InvalidOperation
            }
            ObjectError::OutOfMemory => Self::OutOfMemory,
        }
    }
}

/// Bytes charged for a thread: its 32 KiB kernel stack plus the control block and FPU state.
pub const THREAD_BYTES: u64 = (crate::mm::kstack::KSTACK_PAGES as u64) * 4096 + 1024;
/// Bytes charged for an empty address space: its level-4 table.
pub const ADDRESS_SPACE_BYTES: u64 = 4 * 4096;
/// Bytes reserved per user mapping for its data frame and up to three page-table frames.
pub const PAGE_BYTES: u64 = 4 * 4096;
const FRAME_BYTES: u64 = 4096;
const ENDPOINT_BYTES: u64 = 128;
const NOTIFICATION_BYTES: u64 = 64;
const BUDGET_BYTES: u64 = 128;
const REPLY_BYTES: u64 = 64;
const IRQ_BYTES: u64 = 64;

/// CPU period every budget uses (10 ms, ten timer ticks).
pub const CPU_PERIOD_NS: u64 = 10_000_000;

/// Bytes charged to the paying budget when an object of `ty` is created.
pub const fn object_bytes(ty: ObjectType) -> u64 {
    match ty {
        ObjectType::Thread => THREAD_BYTES,
        ObjectType::AddressSpace => ADDRESS_SPACE_BYTES,
        ObjectType::Frame => FRAME_BYTES,
        ObjectType::Endpoint => ENDPOINT_BYTES,
        ObjectType::Notification => NOTIFICATION_BYTES,
        ObjectType::Budget => BUDGET_BYTES,
        ObjectType::Reply => REPLY_BYTES,
        ObjectType::Irq | ObjectType::IrqControl => IRQ_BYTES,
    }
}

/// A frame object: one zeroed physical frame, freed when the object is destroyed.
#[derive(Debug)]
pub struct FrameObject {
    frame: frame::Frame,
}

impl FrameObject {
    /// The frame's physical address.
    pub fn start(&self) -> u64 {
        self.frame.start().as_u64()
    }
}

impl Drop for FrameObject {
    fn drop(&mut self) {
        frame::free(self.frame);
    }
}

/// The state behind one object id.
pub enum Object {
    /// A thread; its control block lives in the scheduler under the same id.
    Thread,
    /// A user address space.
    AddressSpace(AddressSpace),
    /// One physical frame.
    Frame(FrameObject),
    /// A synchronous IPC endpoint.
    Endpoint(Endpoint),
    /// An asynchronous signal word.
    Notification(Notification),
    /// A CPU and memory account.
    Budget(Budget),
    /// A one-shot reply object (replies are implicit in the thread today; the object is
    /// accounted so user code can hold one).
    Reply,
    /// One hardware interrupt line.
    Irq(IrqLine),
    /// The authority to create Irq objects.
    IrqControl,
}

impl Object {
    /// The ABI type of this object.
    pub fn object_type(&self) -> ObjectType {
        match self {
            Self::Thread => ObjectType::Thread,
            Self::AddressSpace(_) => ObjectType::AddressSpace,
            Self::Frame(_) => ObjectType::Frame,
            Self::Endpoint(_) => ObjectType::Endpoint,
            Self::Notification(_) => ObjectType::Notification,
            Self::Budget(_) => ObjectType::Budget,
            Self::Reply => ObjectType::Reply,
            Self::Irq(_) => ObjectType::Irq,
            Self::IrqControl => ObjectType::IrqControl,
        }
    }
}

struct Entry {
    object: Object,
    /// The budget that pays for this object (a budget's parent pays for it).
    payer: ObjectId,
    charged: u64,
}

/// Registry of live kernel objects.
pub struct ObjectRegistry {
    next_id: ObjectId,
    root_budget: ObjectId,
    entries: BTreeMap<ObjectId, Entry>,
}

impl ObjectRegistry {
    /// Creates a registry with one root budget owning `cpu_ns` of every [`CPU_PERIOD_NS`] and
    /// `memory_bytes` of memory. The root budget pays for nothing itself.
    pub fn new(cpu_ns: u64, memory_bytes: u64) -> Self {
        let root_budget = 1;
        let mut entries = BTreeMap::new();
        entries.insert(
            root_budget,
            Entry {
                object: Object::Budget(
                    Budget::new(cpu_ns, CPU_PERIOD_NS, memory_bytes, 0)
                        .expect("root budget fits its period"),
                ),
                payer: root_budget,
                charged: 0,
            },
        );
        Self {
            next_id: 2,
            root_budget,
            entries,
        }
    }

    /// The root budget: the system's untyped authority.
    pub const fn root_budget(&self) -> ObjectId {
        self.root_budget
    }

    /// The object behind `id`.
    pub fn get(&self, id: ObjectId) -> Option<&Object> {
        self.entries.get(&id).map(|e| &e.object)
    }

    /// The object behind `id`, mutably.
    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.entries.get_mut(&id).map(|e| &mut e.object)
    }

    /// Bytes charged for `id` and the budget that paid them.
    pub fn charge_of(&self, id: ObjectId) -> Option<(ObjectId, u64)> {
        self.entries.get(&id).map(|e| (e.payer, e.charged))
    }

    /// The budget behind `id`.
    pub fn budget_mut(&mut self, id: ObjectId) -> Option<&mut Budget> {
        match self.get_mut(id) {
            Some(Object::Budget(b)) => Some(b),
            _ => None,
        }
    }

    /// Ids of every live object, for diagnostics and the IRQ table.
    pub fn ids(&self) -> impl Iterator<Item = (ObjectId, &Object)> {
        self.entries.iter().map(|(id, e)| (*id, &e.object))
    }

    fn charge(&mut self, budget: ObjectId, bytes: u64) -> Result<(), ObjectError> {
        self.budget_mut(budget)
            .ok_or(ObjectError::NotFound)?
            .charge_memory(bytes)
            .map_err(|_| ObjectError::BudgetExceeded)
    }

    fn credit(&mut self, budget: ObjectId, bytes: u64) {
        if let Some(b) = self.budget_mut(budget) {
            let _ = b.release_memory(bytes);
        }
    }

    /// Charges `bytes` more to the budget paying for `id` (e.g. a page mapped into an address
    /// space); the charge is credited back when `id` is destroyed.
    pub fn charge_more(&mut self, id: ObjectId, bytes: u64) -> Result<(), ObjectError> {
        let payer = self.entries.get(&id).ok_or(ObjectError::NotFound)?.payer;
        self.charge(payer, bytes)?;
        self.entries.get_mut(&id).expect("checked above").charged += bytes;
        Ok(())
    }

    /// Returns `bytes` of an earlier [`Self::charge_more`] (a mapping that failed after all).
    pub fn uncharge(&mut self, id: ObjectId, bytes: u64) {
        if let Some(e) = self.entries.get_mut(&id) {
            e.charged = e.charged.saturating_sub(bytes);
            let payer = e.payer;
            self.credit(payer, bytes);
        }
    }

    /// Stores `object` paid for by `payer`, charging `bytes`. On failure the object is dropped.
    pub fn insert(
        &mut self,
        payer: ObjectId,
        object: Object,
        bytes: u64,
    ) -> Result<ObjectId, ObjectError> {
        self.charge(payer, bytes)?;
        let id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            id,
            Entry {
                object,
                payer,
                charged: bytes,
            },
        );
        Ok(id)
    }

    /// Charges `payer` for an object of type `ty` and builds its initial state (threads and
    /// IRQ objects are built by their owners and stored with [`Self::insert`]).
    pub fn create(&mut self, ty: ObjectType, payer: ObjectId) -> Result<ObjectId, ObjectError> {
        let bytes = object_bytes(ty);
        // Charge first so an unfunded request allocates nothing.
        self.charge(payer, bytes)?;
        let object = match ty {
            ObjectType::AddressSpace => AddressSpace::new().map(Object::AddressSpace),
            ObjectType::Frame => frame::allocate().map(|f| {
                // SAFETY: the frame was just allocated and is reached through the HHDM, which maps
                // all usable RAM writable; nothing else references it.
                unsafe {
                    core::ptr::write_bytes(
                        (crate::mm::paging::hhdm_offset() + f.start().as_u64()) as *mut u8,
                        0,
                        4096,
                    );
                }
                Object::Frame(FrameObject { frame: f })
            }),
            ObjectType::Endpoint => Some(Object::Endpoint(Endpoint::new())),
            ObjectType::Notification => Some(Object::Notification(Notification::new())),
            ObjectType::Reply => Some(Object::Reply),
            ObjectType::Thread | ObjectType::Budget | ObjectType::Irq | ObjectType::IrqControl => {
                self.credit(payer, bytes);
                return Err(ObjectError::InvalidMethod);
            }
        };
        let Some(object) = object else {
            self.credit(payer, bytes);
            return Err(ObjectError::OutOfMemory);
        };
        self.credit(payer, bytes);
        self.insert(payer, object, bytes)
    }

    /// Carves a child budget out of `parent`: `cpu_ns` of each period and `memory_bytes`, both
    /// reserved from what the parent has uncommitted at `now_ns`. The parent also pays for the
    /// child object itself.
    pub fn create_budget(
        &mut self,
        parent: ObjectId,
        cpu_ns: u64,
        memory_bytes: u64,
        now_ns: u64,
    ) -> Result<ObjectId, ObjectError> {
        self.charge(parent, BUDGET_BYTES)?;
        let parent_budget = self.budget_mut(parent).expect("charged above");
        let child = match parent_budget.carve_out(cpu_ns, memory_bytes, now_ns) {
            Ok(child) => child,
            Err(_) => {
                self.credit(parent, BUDGET_BYTES);
                return Err(ObjectError::BudgetExceeded);
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            id,
            Entry {
                object: Object::Budget(child),
                payer: parent,
                charged: BUDGET_BYTES,
            },
        );
        Ok(id)
    }

    /// Whether some live object is paid for by `budget`.
    pub fn pays_for_anything(&self, budget: ObjectId) -> bool {
        self.entries
            .iter()
            .any(|(id, e)| *id != budget && e.payer == budget)
    }

    /// Removes `id`, credits its payer (and returns a child budget's reservation), and hands the
    /// object back so the caller can tear down any external state. Budgets that still pay for
    /// objects are refused.
    pub fn remove(&mut self, id: ObjectId) -> Result<Object, ObjectError> {
        if id == self.root_budget {
            return Err(ObjectError::RootBudget);
        }
        let entry = self.entries.get(&id).ok_or(ObjectError::NotFound)?;
        if matches!(entry.object, Object::Budget(_)) && self.pays_for_anything(id) {
            return Err(ObjectError::InUse);
        }
        let entry = self.entries.remove(&id).expect("checked above");
        if let Object::Budget(child) = &entry.object
            && let Some(parent) = self.budget_mut(entry.payer)
        {
            let _ = parent.release_child(child.limits());
        }
        self.credit(entry.payer, entry.charged);
        Ok(entry.object)
    }
}

/// The kernel's mutable state: objects, capability spaces and threads.
pub struct Kernel {
    /// Every live kernel object.
    pub objects: ObjectRegistry,
    /// Every CSpace, sharing one derivation tree.
    pub caps: CapSpaces,
    /// Threads and the run queue.
    pub sched: Scheduler,
}

static KERNEL: SpinLock<Option<Kernel>> = SpinLock::new(None);

/// Builds the kernel state: a root budget owning the whole CPU and available `memory_bytes`, no
/// CSpaces, and a scheduler whose current thread is the caller (the boot thread). Frames consumed
/// by the scheduler's idle stack are deducted from the root budget. Call once, with interrupts
/// disabled, after the heap is up.
pub fn init(memory_bytes: u64) {
    let free_before = frame::stats().0;
    let sched = Scheduler::new();
    let scheduler_frames = free_before.saturating_sub(frame::stats().0) as u64;
    let scheduler_bytes = scheduler_frames * carv_frames::FRAME_SIZE as u64;
    without_interrupts(|| {
        *KERNEL.lock() = Some(Kernel {
            objects: ObjectRegistry::new(
                CPU_PERIOD_NS,
                memory_bytes.saturating_sub(scheduler_bytes),
            ),
            caps: CapSpaces::new(),
            sched,
        });
    });
}

/// Whether [`init`] has run.
pub fn ready() -> bool {
    without_interrupts(|| KERNEL.lock().is_some())
}

/// Runs `f` on the kernel state with interrupts disabled and the kernel lock held. Never switch
/// threads inside `f`: the lock would stay held.
pub fn with<R>(f: impl FnOnce(&mut Kernel) -> R) -> R {
    without_interrupts(|| {
        let mut guard = KERNEL.lock();
        f(guard.as_mut().expect("kernel state not initialised"))
    })
}

/// Like [`with`], but returns `None` instead of panicking before [`init`].
pub fn try_with<R>(f: impl FnOnce(&mut Kernel) -> R) -> Option<R> {
    without_interrupts(|| KERNEL.lock().as_mut().map(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn registry_charges_and_credits_every_object_type() {
        let mut registry = ObjectRegistry::new(CPU_PERIOD_NS, 1 << 20);
        let root = registry.root_budget();
        let budget = registry
            .create_budget(root, 1_000_000, 512 * 1024, 0)
            .expect("child budget");
        let types = [
            ObjectType::AddressSpace,
            ObjectType::Frame,
            ObjectType::Endpoint,
            ObjectType::Notification,
            ObjectType::Reply,
        ];
        let mut objects = alloc::vec::Vec::new();
        for ty in types {
            let id = registry.create(ty, budget).expect("object");
            assert_eq!(registry.get(id).map(Object::object_type), Some(ty));
            assert_eq!(registry.charge_of(id), Some((budget, object_bytes(ty))));
            objects.push(id);
        }
        let used: u64 = types.iter().map(|t| object_bytes(*t)).sum();
        assert_eq!(
            registry.budget_mut(budget).unwrap().memory_used_bytes(),
            used
        );
        assert_eq!(registry.remove(budget).err(), Some(ObjectError::InUse));
        for id in objects {
            registry.remove(id).expect("destroy");
        }
        assert_eq!(registry.budget_mut(budget).unwrap().memory_used_bytes(), 0);
        registry.remove(budget).expect("empty budget");
        assert_eq!(registry.budget_mut(root).unwrap().memory_used_bytes(), 0);
        // The reservation came back: the whole root can be carved out again.
        registry
            .create_budget(root, CPU_PERIOD_NS, (1 << 20) - 128, CPU_PERIOD_NS)
            .expect("reservation released");
    }

    #[test_case]
    fn child_budgets_never_exceed_their_parent() {
        let mut registry = ObjectRegistry::new(CPU_PERIOD_NS, 64 * 1024);
        let root = registry.root_budget();
        assert_eq!(
            registry.create_budget(root, CPU_PERIOD_NS + 1, 0, 0),
            Err(ObjectError::BudgetExceeded)
        );
        assert_eq!(
            registry.create_budget(root, 0, 64 * 1024, 0),
            Err(ObjectError::BudgetExceeded),
            "the child object itself is charged first"
        );
        let a = registry
            .create_budget(root, 7_000_000, 16 * 1024, 0)
            .unwrap();
        assert_eq!(
            registry.create_budget(root, 4_000_000, 0, 0),
            Err(ObjectError::BudgetExceeded),
            "only 3 ms of the period is left"
        );
        let b = registry.create_budget(a, 2_000_000, 8 * 1024, 0).unwrap();
        assert_eq!(
            registry.create_budget(a, 0, 16 * 1024, 0),
            Err(ObjectError::BudgetExceeded)
        );
        assert_eq!(registry.remove(root).err(), Some(ObjectError::RootBudget));
        registry.remove(b).unwrap();
        registry.remove(a).unwrap();
    }

    #[test_case]
    fn unfunded_objects_are_refused_without_side_effects() {
        let mut registry = ObjectRegistry::new(CPU_PERIOD_NS, 128);
        let root = registry.root_budget();
        let budget = registry.create_budget(root, 0, 0, 0).expect("fits exactly");
        let (free_before, _) = frame::stats();
        assert_eq!(
            registry.create(ObjectType::Frame, budget),
            Err(ObjectError::BudgetExceeded)
        );
        assert_eq!(frame::stats().0, free_before, "no frame was allocated");
        assert_eq!(
            registry.create(ObjectType::Reply, root),
            Err(ObjectError::BudgetExceeded)
        );
        registry
            .remove(budget)
            .expect("empty child budget can be reclaimed");
    }
}

//! Threads, budget-aware round-robin scheduling and context switching (P2.5, P2.6).
//!
//! Every thread has a control block ([`Tcb`]) holding its saved kernel stack pointer, its kernel
//! stack, the address space and CSpace it runs with and the Budget object it runs on. The ready
//! queue is FIFO; a thread is picked only while its budget has CPU time left in the current period
//! (`carv_budget::Budget::refill` / `cpu_remaining_ns`), so CPU shares follow the budgets: two
//! spinning threads with 3 ms and 7 ms of every 10 ms period split the CPU 30/70. Each 1 ms timer
//! tick charges the running thread's budget; an overrun drains the rest of the period.
//!
//! Kernel code is never preempted: it runs with interrupts disabled (syscalls mask IF, interrupt
//! gates clear it), and the timer only switches threads when it interrupted ring 3. Threads leave
//! the CPU by blocking ([`Scheduler::block_current`] then [`reschedule`]) or being preempted. When
//! nothing is runnable the idle thread halts with interrupts enabled.
//!
//! Switching never happens with the kernel lock held: [`Scheduler::plan`] decides under the lock
//! and returns raw stack pointers, and [`reschedule`] switches after dropping it. Control blocks
//! are boxed so those pointers stay valid; a thread that exits is parked as a zombie and freed by
//! the next plan that runs on another thread's stack.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use carv_abi::Message;
use carv_caps::SpaceId;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::PhysFrame;

use crate::arch::x86_64::{apic, syscall};
use crate::mm::kstack::KernelStack;
use crate::mm::paging;
use crate::objects::{self, ObjectId, ObjectRegistry};

/// Identity of a thread: the object id of its Thread object (the boot and idle threads, which
/// are not objects, use [`BOOT_THREAD`] and [`IDLE_THREAD`]).
pub type ThreadId = u64;
/// The thread `kmain` runs on.
pub const BOOT_THREAD: ThreadId = 0;
/// The thread that halts when nothing else can run.
pub const IDLE_THREAD: ThreadId = u64::MAX;

/// CPU time one timer tick is worth.
pub const TICK_NS: u64 = 1_000_000_000 / apic::TIMER_HZ;

/// What a blocked thread waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// A receiver on this endpoint (send or call).
    Send(ObjectId),
    /// A sender on this endpoint (recv or reply_recv).
    Recv(ObjectId),
    /// The reply from this server thread (the second half of call).
    Reply(ThreadId),
    /// A signal on this notification.
    Notification(ObjectId),
    /// The tick count to reach.
    Sleep(u64),
}

/// Lifecycle of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Created but not started.
    Inactive,
    /// In the ready queue.
    Ready,
    /// On the CPU.
    Running,
    /// Waiting for an event.
    Blocked(Wait),
    /// Exited; freed once another thread runs.
    Dead,
}

/// Why a thread stopped, kept for diagnostics and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Its Thread object was destroyed (possibly by itself).
    Destroyed,
    /// It raised a CPU exception in ring 3.
    Faulted,
}

/// IPC state parked in a thread while it waits.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ipc {
    /// The message a blocked sender is sending.
    pub message: Message,
    /// Badge of the endpoint capability the sender used.
    pub badge: u64,
    /// Whether the sender called (and waits for a reply).
    pub call: bool,
    /// User address of the buffer a receiver or caller gets its message in.
    pub buffer: u64,
}

/// A thread control block.
pub struct Tcb {
    /// Thread id.
    pub id: ThreadId,
    /// Lifecycle state.
    pub state: State,
    saved_rsp: u64,
    kstack: Option<KernelStack>,
    /// Page-table root the thread runs with (`None`: the kernel's).
    pub root: Option<PhysFrame>,
    /// Address-space object the thread runs in.
    pub space: Option<ObjectId>,
    /// CSpace the thread's syscalls name capabilities in.
    pub cspace: Option<SpaceId>,
    /// Budget object charged for the thread's CPU time (`None`: kernel thread, unaccounted).
    pub budget: Option<ObjectId>,
    /// Ring-3 context the thread starts with.
    pub entry: syscall::UserContext,
    /// Status and two result words a blocked syscall completes with.
    pub result: [u64; 3],
    /// IPC state while blocked in IPC.
    pub ipc: Ipc,
    /// Caller waiting for this thread's reply.
    pub reply_to: Option<ThreadId>,
    /// Timer ticks this thread has been on the CPU for.
    pub run_ticks: u64,
}

impl Tcb {
    fn new(id: ThreadId, state: State, kstack: Option<KernelStack>) -> Box<Self> {
        Box::new(Self {
            id,
            state,
            saved_rsp: 0,
            kstack,
            root: None,
            space: None,
            cspace: None,
            budget: None,
            entry: syscall::UserContext::new(0, 0),
            result: [0; 3],
            ipc: Ipc::default(),
            reply_to: None,
            run_ticks: 0,
        })
    }
}

/// The run queue and every thread.
pub struct Scheduler {
    threads: BTreeMap<ThreadId, Box<Tcb>>,
    ready: VecDeque<ThreadId>,
    current: ThreadId,
    zombies: Vec<Box<Tcb>>,
    exits: VecDeque<(ThreadId, Exit)>,
    switches: u64,
}

/// A switch decided by [`Scheduler::plan`], carried out by [`reschedule`] after the lock is gone.
struct Switch {
    old_rsp: *mut u64,
    new_rsp: u64,
    kstack_top: Option<u64>,
    root: PhysFrame,
}

const EXIT_LOG: usize = 32;

impl Scheduler {
    /// A scheduler whose running thread is the caller ([`BOOT_THREAD`]) plus an idle thread.
    ///
    /// # Panics
    /// If the idle thread's kernel stack cannot be mapped.
    pub fn new() -> Self {
        let mut threads = BTreeMap::new();
        threads.insert(BOOT_THREAD, Tcb::new(BOOT_THREAD, State::Running, None));
        let stack = KernelStack::new().expect("mapping the idle thread's kernel stack");
        let mut idle = Tcb::new(IDLE_THREAD, State::Ready, None);
        // SAFETY: the stack was just mapped for this thread alone and is 32 KiB long.
        idle.saved_rsp = unsafe { syscall::prepare_stack(stack.top(), idle_main, 0) };
        idle.kstack = Some(stack);
        threads.insert(IDLE_THREAD, idle);
        Self {
            threads,
            ready: VecDeque::new(),
            current: BOOT_THREAD,
            zombies: Vec::new(),
            exits: VecDeque::new(),
            switches: 0,
        }
    }

    /// The running thread.
    pub const fn current(&self) -> ThreadId {
        self.current
    }

    /// The control block of `id`.
    pub fn thread(&self, id: ThreadId) -> Option<&Tcb> {
        self.threads.get(&id).map(|t| &**t)
    }

    /// The control block of `id`, mutably.
    pub fn thread_mut(&mut self, id: ThreadId) -> Option<&mut Tcb> {
        self.threads.get_mut(&id).map(|t| &mut **t)
    }

    /// The running thread's control block.
    pub fn current_tcb(&mut self) -> &mut Tcb {
        let id = self.current;
        self.thread_mut(id).expect("the current thread exists")
    }

    /// Every live thread.
    pub fn threads(&self) -> impl Iterator<Item = &Tcb> {
        self.threads.values().map(|t| &**t)
    }

    /// Context switches so far.
    pub const fn switches(&self) -> u64 {
        self.switches
    }

    /// Why `id` exited, if it did recently.
    pub fn exit_of(&self, id: ThreadId) -> Option<Exit> {
        self.exits.iter().find(|(t, _)| *t == id).map(|(_, e)| *e)
    }

    /// Creates an inactive user thread `id` with its own kernel stack, charged to and running on
    /// `budget`, naming capabilities in `cspace`. `None` if no kernel stack could be mapped.
    pub fn create_user_thread(
        &mut self,
        id: ThreadId,
        budget: ObjectId,
        cspace: SpaceId,
    ) -> Option<()> {
        let stack = KernelStack::new()?;
        let mut tcb = Tcb::new(id, State::Inactive, None);
        // SAFETY: the stack was just mapped for this thread alone and is 32 KiB long.
        tcb.saved_rsp = unsafe { syscall::prepare_stack(stack.top(), user_thread_main, id) };
        tcb.kstack = Some(stack);
        tcb.budget = Some(budget);
        tcb.cspace = Some(cspace);
        self.threads.insert(id, tcb);
        Some(())
    }

    /// Makes an inactive thread runnable in `space` (page-table root `root`) at `entry`.
    pub fn start(
        &mut self,
        id: ThreadId,
        space: ObjectId,
        root: PhysFrame,
        entry: syscall::UserContext,
    ) -> bool {
        let Some(t) = self.thread_mut(id) else {
            return false;
        };
        if t.state != State::Inactive {
            return false;
        }
        t.space = Some(space);
        t.root = Some(root);
        t.entry = entry;
        t.state = State::Ready;
        self.ready.push_back(id);
        true
    }

    /// Marks the running thread blocked on `wait`; call [`reschedule`] once the lock is dropped.
    pub fn block_current(&mut self, wait: Wait) {
        self.current_tcb().state = State::Blocked(wait);
    }

    /// Wakes `id` if it is blocked, completing its syscall with `result` (status, rdx, rsi).
    pub fn wake(&mut self, id: ThreadId, result: [u64; 3]) -> bool {
        match self.thread_mut(id) {
            Some(t) if matches!(t.state, State::Blocked(_)) => {
                t.state = State::Ready;
                t.result = result;
                self.ready.push_back(id);
                true
            }
            _ => false,
        }
    }

    /// Whether `id` is blocked on `wait`.
    pub fn is_blocked_on(&self, id: ThreadId, wait: Wait) -> bool {
        self.thread(id)
            .is_some_and(|t| t.state == State::Blocked(wait))
    }

    /// Stops `id` for good: removes it from the run queue and frees it (or parks it as a zombie
    /// if it is the running thread). The caller has already cleaned up IPC queues, CSpace and
    /// object state. Returns the dead control block's wait, if it was blocked.
    pub fn kill(&mut self, id: ThreadId, why: Exit) -> Option<Wait> {
        let tcb = self.threads.get_mut(&id)?;
        let wait = match tcb.state {
            State::Blocked(w) => Some(w),
            _ => None,
        };
        tcb.state = State::Dead;
        self.ready.retain(|t| *t != id);
        if self.exits.len() == EXIT_LOG {
            self.exits.pop_front();
        }
        self.exits.push_back((id, why));
        if id != self.current {
            self.threads.remove(&id);
        }
        wait
    }

    /// Accounts one timer tick at `now_ticks` to the running thread and wakes due sleepers.
    pub fn tick(&mut self, objects: &mut ObjectRegistry, now_ticks: u64) {
        let now_ns = now_ticks * TICK_NS;
        let current = self.current_tcb();
        current.run_ticks += 1;
        if let Some(budget) = current.budget.and_then(|b| objects.budget_mut(b))
            && !budget.charge_cpu(TICK_NS, now_ns)
        {
            let rest = budget.cpu_remaining_ns();
            budget.charge_cpu(rest, now_ns);
        }
        let due: Vec<ThreadId> = self
            .threads
            .values()
            .filter(|t| matches!(t.state, State::Blocked(Wait::Sleep(at)) if at <= now_ticks))
            .map(|t| t.id)
            .collect();
        for id in due {
            self.wake(id, [0; 3]);
        }
    }

    fn eligible(&self, id: ThreadId, objects: &mut ObjectRegistry, now_ns: u64) -> bool {
        let Some(t) = self.thread(id) else {
            return false;
        };
        match t.budget {
            None => true,
            Some(b) => objects.budget_mut(b).is_some_and(|budget| {
                budget.refill(now_ns);
                budget.cpu_remaining_ns() > 0
            }),
        }
    }

    /// Picks the next thread and returns the switch to make, if it is not the running thread.
    fn plan(&mut self, objects: &mut ObjectRegistry, now_ticks: u64) -> Option<Switch> {
        let now_ns = now_ticks * TICK_NS;
        // Zombies parked by earlier switches are off their stacks now: this code runs on the
        // current thread's stack, and a zombie is never current.
        self.zombies.clear();

        let old = self.current;
        if old != IDLE_THREAD && self.thread(old).is_some_and(|t| t.state == State::Running) {
            self.current_tcb().state = State::Ready;
            self.ready.push_back(old);
        }
        let position = (0..self.ready.len()).find(|i| self.eligible(self.ready[*i], objects, now_ns));
        let next = match position {
            Some(i) => self.ready.remove(i).expect("index in range"),
            None => IDLE_THREAD,
        };
        let next_tcb = self.threads.get_mut(&next).expect("ready threads exist");
        next_tcb.state = State::Running;
        if next == old {
            return None;
        }
        let new_rsp = next_tcb.saved_rsp;
        let kstack_top = next_tcb.kstack.as_ref().map(KernelStack::top);
        let root = next_tcb.root.unwrap_or_else(paging::kernel_root);
        self.current = next;
        self.switches += 1;
        let old_tcb = if self.threads.get(&old).is_some_and(|t| t.state == State::Dead) {
            let tcb = self.threads.remove(&old).expect("checked above");
            self.zombies.push(tcb);
            self.zombies.last_mut().expect("just pushed")
        } else {
            self.threads.get_mut(&old).expect("the old thread exists")
        };
        Some(Switch {
            old_rsp: &raw mut old_tcb.saved_rsp,
            new_rsp,
            kstack_top,
            root,
        })
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Current timer tick count, the scheduler's clock.
pub fn now_ticks() -> u64 {
    apic::ticks()
}

/// Runs the scheduler: switches to the next eligible thread, or returns at once if the running
/// thread stays. Call with interrupts disabled and without the kernel lock. Returns when this
/// thread is scheduled again.
pub fn reschedule() {
    let Some(Some(switch)) = objects::try_with(|k| k.sched.plan(&mut k.objects, now_ticks()))
    else {
        return;
    };
    if let Some(top) = switch.kstack_top {
        syscall::set_kernel_stack(top);
    }
    let (active, flags) = Cr3::read();
    if active != switch.root {
        // SAFETY: `switch.root` is either the kernel's root or a user root that shares the
        // kernel half (AddressSpace::new copies it), so the code and stack in use stay mapped.
        unsafe { Cr3::write(switch.root, flags) };
    }
    // SAFETY: interrupts are off; `old_rsp` points into a boxed control block that stays alive
    // (live thread or parked zombie) until this context is resumed or reaped by another thread;
    // `new_rsp` was saved by `switch_context` or built by `prepare_stack`.
    unsafe { syscall::switch_context(switch.old_rsp, switch.new_rsp) };
}

/// Blocks the running thread for `ms` timer ticks. Interrupts must be disabled.
pub fn sleep_ms(ms: u64) {
    let until = now_ticks() + ms;
    objects::with(|k| k.sched.block_current(Wait::Sleep(until)));
    reschedule();
}

/// Called by the timer interrupt (after EOI): charges the running thread and, if the timer
/// interrupted ring 3, preempts it.
pub fn timer_tick(from_user: bool) {
    let ticked = objects::try_with(|k| k.sched.tick(&mut k.objects, now_ticks()));
    if ticked.is_some() && from_user {
        reschedule();
    }
}

/// The idle thread: halt until an interrupt, then see whether anything became runnable.
extern "C" fn idle_main(_: u64) -> ! {
    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
        reschedule();
    }
}

/// First code a user thread runs: drop to ring 3 at its entry context.
extern "C" fn user_thread_main(id: u64) -> ! {
    let entry = objects::with(|k| k.sched.thread(id).map(|t| t.entry))
        .expect("a starting thread has a control block");
    // SAFETY: we run on this thread's kernel stack (installed by `reschedule`) with interrupts
    // off, and `reschedule` loaded the thread's address space.
    unsafe { syscall::enter_user(entry) }
}

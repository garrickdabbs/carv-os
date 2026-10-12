//! Syscall wrappers: one function per syscall and per `invoke` method (ABI v1 + ADR-0003).
//!
//! `syscall` takes the number in `rax` and arguments in `rdi, rsi, rdx, r10, r8, r9`; it returns
//! the status in `rax` and up to two results in `rdx` and `rsi`. All other registers survive.

use core::arch::asm;

use carv_abi::{Message, ObjectType, Rights, Syscall, method};

use crate::error::{Result, check};

/// Raw syscall results: the status and the two result registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Raw {
    /// `rax`: 0 or an error code.
    pub status: u64,
    /// `rdx`: first result.
    pub r0: u64,
    /// `rsi`: second result.
    pub r1: u64,
}

/// Issues `number` with `args` = `rdi, rsi, rdx, r10, r8, r9`.
pub fn syscall(number: Syscall, args: [u64; 6]) -> Raw {
    let [a0, a1, a2, a3, a4, a5] = args;
    let mut status = number as u64;
    let mut r0 = a2;
    let mut r1 = a1;
    // SAFETY: this is the CarvOS userspace syscall ABI: rax holds the syscall number; arguments
    // are in rdi, rsi, rdx, r10, r8 and r9; `syscall` clobbers rcx and r11 and returns the status
    // and results in rax, rdx and rsi. The assembly declares exactly those registers. Pointer
    // arguments are only passed by the typed wrappers below, which borrow the pointee for the
    // whole call.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") status,
            in("rdi") a0,
            inlateout("rsi") r1,
            inlateout("rdx") r0,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    Raw { status, r0, r1 }
}

/// Sender identity and label of a received message; the payload is in the caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// The badge of the sender's endpoint capability.
    pub badge: u64,
    /// The message label.
    pub label: u64,
}

fn received(raw: Raw) -> Result<Received> {
    check(raw.status)?;
    Ok(Received {
        badge: raw.r0,
        label: raw.r1,
    })
}

/// `debug_putc`: writes one byte to the kernel's serial console. Errors are ignored.
pub fn debug_putc(byte: u8) {
    let _ = syscall(Syscall::DebugPutc, [u64::from(byte), 0, 0, 0, 0, 0]);
}

/// `send`: sends `message` to the endpoint in slot `endpoint` (needs `WRITE`); blocks until a
/// receiver takes it.
pub fn send(endpoint: u64, message: &Message) -> Result<()> {
    let ptr = core::ptr::from_ref(message) as u64;
    check(syscall(Syscall::Send, [endpoint, ptr, 0, 0, 0, 0]).status)
}

/// `recv`: blocks for a message on the endpoint in slot `endpoint` (needs `READ`) and stores it
/// in `message`.
pub fn recv(endpoint: u64, message: &mut Message) -> Result<Received> {
    let ptr = core::ptr::from_mut(message) as u64;
    received(syscall(Syscall::Recv, [endpoint, ptr, 0, 0, 0, 0]))
}

/// `call`: sends `message` and blocks for the reply, which replaces `message`. Returns the reply's
/// label.
pub fn call(endpoint: u64, message: &mut Message) -> Result<u64> {
    let ptr = core::ptr::from_mut(message) as u64;
    let raw = syscall(Syscall::Call, [endpoint, ptr, 0, 0, 0, 0]);
    check(raw.status)?;
    Ok(raw.r1)
}

/// `reply_recv`: replies with `message` to the thread whose `call` was last received, then blocks
/// for the next message on `endpoint`, which replaces `message`.
pub fn reply_recv(endpoint: u64, message: &mut Message) -> Result<Received> {
    let ptr = core::ptr::from_mut(message) as u64;
    received(syscall(Syscall::ReplyRecv, [endpoint, ptr, 0, 0, 0, 0]))
}

/// `signal`: counts one signal on the notification in slot `notification` (needs `WRITE`).
pub fn signal(notification: u64) -> Result<()> {
    check(syscall(Syscall::Signal, [notification, 0, 0, 0, 0, 0]).status)
}

/// `wait`: blocks until the notification in slot `notification` (needs `READ`) has been signalled,
/// returns the signal count and resets it.
pub fn wait(notification: u64) -> Result<u64> {
    let raw = syscall(Syscall::Wait, [notification, 0, 0, 0, 0, 0]);
    check(raw.status)?;
    Ok(raw.r0)
}

/// `yield`: gives up the rest of this thread's time slice.
pub fn yield_now() {
    let _ = syscall(Syscall::Yield, [0; 6]);
}

/// `invoke`: calls `method` on the object behind `cap` with up to four arguments and returns the
/// two result registers.
pub fn invoke(cap: u64, method: u64, args: [u64; 4]) -> Result<(u64, u64)> {
    let [a0, a1, a2, a3] = args;
    let raw = syscall(Syscall::Invoke, [cap, method, a0, a1, a2, a3]);
    check(raw.status)?;
    Ok((raw.r0, raw.r1))
}

/// What `DESCRIBE` reports about a capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Description {
    /// The object's type, or `None` for a type this runtime does not know.
    pub object_type: Option<ObjectType>,
    /// The capability's rights.
    pub rights: Rights,
}

/// `DESCRIBE`: the type and rights of the capability in slot `cap`.
pub fn describe(cap: u64) -> Result<Description> {
    let (ty, rights) = invoke(cap, method::DESCRIBE, [0; 4])?;
    Ok(Description {
        object_type: ObjectType::from_raw(ty),
        rights: u8::try_from(rights)
            .ok()
            .and_then(Rights::from_bits)
            .unwrap_or_default(),
    })
}

/// `DESTROY`: destroys the object behind `cap` and credits its budget. Destroying one's own
/// thread ends it and does not return.
pub fn destroy(cap: u64) -> Result<()> {
    invoke(cap, method::DESTROY, [0; 4]).map(|_| ())
}

/// Memory accounting of a budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetUsage {
    /// Bytes charged so far.
    pub memory_used: u64,
    /// Bytes the budget may charge in all.
    pub memory_limit: u64,
}

/// `BUDGET_READ`: memory use of the budget in slot `budget`.
pub fn budget_read(budget: u64) -> Result<BudgetUsage> {
    let (used, limit) = invoke(budget, method::BUDGET_READ, [0; 4])?;
    Ok(BudgetUsage {
        memory_used: used,
        memory_limit: limit,
    })
}

/// `BUDGET_CREATE`: creates an object of type `ty` charged to `budget`, with a full-rights
/// capability in the empty slot `dest`. For a child budget `cpu_ns` and `memory_bytes` are carved
/// out of `budget`; they are ignored for other types. Returns the slot.
pub fn budget_create(
    budget: u64,
    ty: ObjectType,
    dest: u64,
    cpu_ns: u64,
    memory_bytes: u64,
) -> Result<u64> {
    invoke(
        budget,
        method::BUDGET_CREATE,
        [ty as u64, dest, cpu_ns, memory_bytes],
    )
    .map(|(slot, _)| slot)
}

/// `ADDRESS_SPACE_MAP`: maps a fresh zeroed page at the page-aligned address `va` in the address
/// space in slot `space`, with [`carv_abi::map`] `flags`, charged to the address space's budget.
pub fn map_page(space: u64, va: u64, flags: u64) -> Result<()> {
    invoke(space, method::ADDRESS_SPACE_MAP, [va, flags, 0, 0]).map(|_| ())
}

/// `THREAD_START`: starts the not-yet-started thread in slot `thread` at `rip` with stack pointer
/// `rsp` and `arg` in `rdi`, in the address space in slot `space` (`None` = the caller's own).
/// The thread shares the caller's CSpace.
pub fn thread_start(thread: u64, rip: u64, rsp: u64, arg: u64, space: Option<u64>) -> Result<()> {
    invoke(
        thread,
        method::THREAD_START,
        [rip, rsp, arg, space.unwrap_or(u64::MAX)],
    )
    .map(|_| ())
}

/// `IRQ_CONTROL_GET`: creates an `Irq` capability for global system interrupt `gsi` in the empty
/// slot `dest`. Returns the slot.
pub fn irq_control_get(control: u64, gsi: u64, dest: u64) -> Result<u64> {
    invoke(control, method::IRQ_CONTROL_GET, [gsi, dest, 0, 0]).map(|(slot, _)| slot)
}

/// `IRQ_BIND`: delivers the interrupt to the notification in slot `notification` and unmasks it.
pub fn irq_bind(irq: u64, notification: u64) -> Result<()> {
    invoke(irq, method::IRQ_BIND, [notification, 0, 0, 0]).map(|_| ())
}

/// `IRQ_UNBIND`: masks the interrupt and drops its notification.
pub fn irq_unbind(irq: u64) -> Result<()> {
    invoke(irq, method::IRQ_UNBIND, [0; 4]).map(|_| ())
}
